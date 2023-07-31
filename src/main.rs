mod cfg;
mod consumer;
mod snd;
mod util;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{crate_name, crate_version, crate_authors, CommandFactory, Parser};
use futures::future::{FusedFuture, FutureExt};
use log::{LevelFilter, debug, info, warn, error};
use simple_logger::SimpleLogger;
use time::UtcOffset;
use tokio::runtime::Builder as TokioRuntimeBuilder;
use tokio::signal;
use tokio::sync::watch::{self, Sender};
use tokio::task::{JoinError, LocalSet};

use self::cfg::{Action, Args, ApplicationConfig, SelectedConsumer};
use self::consumer::discord;
use self::util::fmt as fmt_util;
use self::util::task::{TaskContainer, TaskSetBuilder};

#[cfg(feature = "wav")]
use self::consumer::wav;

const DEFAULT_WORKER_THREADS: usize = 2;

fn main() -> ExitCode {
    let args = Args::parse();
    let action = Action::from(&args);

    match action {
        Action::PrintVersion => {
            print_startup_message();
            ExitCode::SUCCESS
        },
        Action::ListDrivers => {
            snd::list_drivers();
            ExitCode::SUCCESS
        },
        Action::ListServers | Action::Run(_) => {
            let config = match build_config(args, action) {
                Ok(config) => config,
                Err(s) => {
                    eprintln!("Error in configuration: {}", s);

                    let mut app = Args::command();
                    let _ = app.print_long_help();
                    return ExitCode::FAILURE;
                }
            };

            let result = if let Action::Run(consumer) = action {
                start(config, consumer)
            } else {
                do_server_list(config)
            };

            if let Err(msg) = result {
                eprintln!("Runtime error: {}", msg);
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

/// Consumes a [`cfg::Args`] object and converts it into an
/// [`cfg::ApplicationConfig`] instance, merging configuration from a specified
/// config file, if present.
fn build_config(args: Args, action: Action) -> Result<ApplicationConfig, String> {
    let config: Option<ApplicationConfig> = if let Some(configfile) = &args.configfile {
        Some(ApplicationConfig::from_file(configfile)?)
    } else {
        None
    };

    let config_from_args = ApplicationConfig::try_from(args)?;

    if let Some(mut config_from_file) = config {
        config_from_file.merge(config_from_args);
        config_from_file.validate_semantics(action)?;

        Ok(config_from_file)
    } else {
        config_from_args.validate_semantics(action)?;

        Ok(config_from_args)
    }
}

/// Prints a message listing the application's name, authors, and version.
fn print_startup_message() {
    let title = format!("{} v{}", crate_name!(), crate_version!());
    let authors = format!("(c) {}", crate_authors!(", "));
    let title_length = fmt_util::display_length!(&title, &authors);
    let title_decoration = "=".repeat(title_length);

    println!("{}", title_decoration);
    println!("{}", title);
    println!("{}", authors);
    println!("{}", title_decoration);
}

/// Configures the logging framework.
///
/// If the selected logging level is at least as terse as Info, this will also
/// apply dependency-specific logging config to make some chatty dependencies
/// quieter during normal use. Using a debug level of Debug or Trace will enable
/// all logging for these dependencies.
fn init_logger(config: &ApplicationConfig, default_log_level: LevelFilter) {
    let log_level = if let Some(log_level) = config.log_level {
        log_level
    } else {
        default_log_level
    };

    let mut logger = SimpleLogger::new()
        .with_level(log_level)
        .env()
        .with_local_timestamps()
        .with_colors(io::stderr().is_terminal());

    //these third-party modules are very noisy at info level, so make them
    //quieter if not debugging
    if log_level <= LevelFilter::Info {
        logger = logger
            .with_module_level("tracing", LevelFilter::Warn)
            .with_module_level("serenity::http", LevelFilter::Warn)
            .with_module_level(
                "serenity::client::bridge::gateway::shard_runner",
                LevelFilter::Warn
            );
    }

    if let Ok(offset) = UtcOffset::current_local_offset() {
        logger = logger.with_utc_offset(offset);
    } else {
        panic!("Failed to determine UTC offset");
    }

    logger.init().expect("Logging subsystem failed to initialize");
}

/// Helper function that prints a listing of available servers that the
/// application can connect to.
fn do_server_list(config: ApplicationConfig) -> Result<(), String> {
    if let Some(token) = &config.discord.token {
        init_logger(&config, LevelFilter::Warn);

        debug!("Initializing async runtime...");
        let runtime = TokioRuntimeBuilder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(discord::list_guilds(token)).map_err(|e| format!(
            "Failed to look up available Discord servers: {}",
            e
        ))
    } else {
        Err(String::from("A Discord bot token is required to use this application."))
    }
}

/// Initializes the asynchronous runtime and all components needed to record
/// system audio, before handing over to [`application_monitor`] to monitor
/// running tasks and initiate a graceful shutdown when relevant.
fn start(
    config: ApplicationConfig,
    consumer: SelectedConsumer
) -> Result<(), String> {
    print_startup_message();
    init_logger(&config, LevelFilter::Info);

    debug!("Initializing async runtime...");

    let runtime = TokioRuntimeBuilder::new_multi_thread()
        .worker_threads(config.threads.unwrap_or(DEFAULT_WORKER_THREADS))
        .enable_io()
        .enable_time()
        .build()
        .unwrap();

    runtime.block_on(async {
        debug!("Async runtime initialized");

        //Step 0: Set up the application shutdown notification channel
        debug!("Setting up application shutdown notification channel");
        let (shutdown_tx, shutdown_rx) = watch::channel(true);

        //Step 1: Start up the audio driver
        debug!("Starting audio driver");
        let local = LocalSet::new();
        let driver = local.run_until(snd::select_and_start_driver(&config, shutdown_rx.clone())).await;

        match driver {
            Ok(driver) => {
                let (stream, driver_tasks) = driver.into_tuple();
                let stream_tasks: Result<Box<dyn TaskContainer<()>>, String> = match consumer {
                    SelectedConsumer::Discord => {
                        let mut tasks = TaskSetBuilder::new();
                        let send_audio_result = discord::send_audio(
                            stream,
                            &config.discord,
                            shutdown_rx
                        ).await;

                        match send_audio_result {
                            Ok(task) => {
                                tasks.insert(task);
                                Ok(Box::new(tasks.build()))
                            },
                            Err(e) => Err(e.to_string()),
                        }
                    },
                    #[cfg(feature = "wav")]
                    SelectedConsumer::Wav => {
                        if let Some(output_file) = &config.output_file {
                            let mut tasks = TaskSetBuilder::new();
                            let record_result = wav::record(
                                output_file,
                                stream.into_media_source(),
                                shutdown_rx
                            );

                            match record_result {
                                Ok(task) => {
                                    tasks.insert(task);
                                    Ok(Box::new(tasks.build()))
                                },
                                Err(e) => Err(e.to_string()),
                            }
                        } else {
                            Err(String::from("Wav consumer requires -o/--output-file"))
                        }
                    },
                };

                application_monitor(
                    driver_tasks,
                    stream_tasks?,
                    shutdown_tx,
                    local
                ).await
            },
            Err(e) => Err(e.to_string()),
        }
    })
}

/// Monitors the given task sets and initiates a graceful shutdown when
/// conditions are met (either on SIGINT or when a monitored task panics). Also
/// advances tasks in the given [`LocalSet`], if any.
async fn application_monitor(
    mut driver_tasks: Box<dyn TaskContainer<()>>,
    mut stream_tasks: Box<dyn TaskContainer<()>>,
    shutdown_tx: Sender<bool>,
    local: LocalSet
) -> Result<(), String> {
    //Step 2: Listen for interrupts
    debug!("Setting up signal handler");
    let mut interrupt = Box::pin(signal::ctrl_c());
    let mut local = local.fuse();

    //Step 3: Application lifecycle handling
    debug!("Waiting for application shutdown events");
    let mut interrupt_read = false;
    let mut shutdown_result: Result<(), String> = Ok(());

    while !(driver_tasks.is_empty() && stream_tasks.is_empty()) {
        tokio::select! {
            biased;
            interrupted = &mut interrupt, if !interrupt_read => {
                //If we received an interrupt signal, we don't want to kill
                //the loop just yet. Instead, initiate a graceful shutdown
                //and kill the loop after we await all of the driver_tasks
                shutdown_tx.send(false).expect("Failed to initiate graceful shutdown");
                interrupt_read = true;

                if let Err(e) = interrupted {
                    warn!("Failed to set up application interrupt listener, tearing down...");
                    shutdown_result = Err(e.kind().to_string());
                } else {
                    info!("Received interrupt, tearing down...");
                }
            },
            driver_task = driver_tasks.join_next(), if !driver_tasks.is_empty() => if let Err(e) = handle_task_complete(driver_task) {
                shutdown_result = Err(e);
                shutdown_tx.send(false).expect("Failed to initiate graceful shutdown");
            },
            stream_task = stream_tasks.join_next(), if !stream_tasks.is_empty() => if let Err(e) = handle_task_complete(stream_task) {
                shutdown_result = Err(e);
                shutdown_tx.send(false).expect("Failed to initiate graceful shutdown");
            },
            _ = &mut local, if !local.is_terminated() => {}
        }
    }

    info!("Terminating");
    shutdown_result
}

/// Helper function to log the result of finished application tasks, and
/// indicates whether the task panicked and needs to initiate a shutdown.
fn handle_task_complete(task: Option<Result<(), JoinError>>) -> Result<(), String> {
    if let Some(task_res) = task {
        if let Err(task_err) = task_res {
            if task_err.is_panic() {
                error!("Task unexpectedly panicked, tearing down application...");
                return Err(String::from("Fatal error in application task"))
            } else {
                debug!("Handler task was cancelled");
            }
        } else {
            debug!("Handler task completed");
        }
    } else {
        debug!("No more tasks in container");
    }

    Ok(())
}
