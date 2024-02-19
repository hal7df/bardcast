///! Root of the PulseAudio sound driver implementation.

mod intercept;
mod cfg;
mod context;
mod event;
pub mod error;
mod owned;

extern crate libpulse_binding as libpulse;

use std::collections::HashSet;
use std::fmt::{Display, Error as FormatError, Formatter};

use log::{debug, info, warn};
use regex::Regex;
use tokio::sync::watch::Receiver as WatchReceiver;

use libpulse::error::Code;
use libpulse::proplist::Proplist;
use libpulse::proplist::properties::{
    APPLICATION_NAME,
    APPLICATION_PROCESS_BINARY
};

use crate::cfg::InterceptMode;
use crate::util::task::{TaskSet, TaskSetBuilder, ValueJoinHandle};
use self::context::PulseContextWrapper;
use self::context::stream::{SampleConsumer, SinkId, StreamManager};
use self::event::{
    AudioEntity,
    ChangeEvent,
    ToFilterMapped
};
use self::event::factory::EventListenerFactory;
use self::error::{ComponentError, PulseDriverError};
use self::owned::OwnedSinkInputInfo;
use super::types::DriverInitError;

pub use self::cfg::PulseDriverConfig;

pub const DRIVER_NAME: &'static str = "pulse";

// TYPE DEFINITIONS ************************************************************

/// Helper for matching specific properties in a [`Proplist`] against a regular
/// expression.
#[derive(Debug, Clone)]
pub struct ProplistMatcher {
    props: HashSet<String>,
    pattern: Regex,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DriverComponent {
    Context,
    EventHandler,
    Stream,
    Interceptor,
}

// TYPE IMPLS ******************************************************************
impl ProplistMatcher {
    /// Creates a new instance against the given properties and regex pattern.
    pub fn new(props: HashSet<String>, pattern: Regex) -> Self {
        info!(
            "Matching applications against pattern '{}' on properties: {}",
            pattern,
            props.clone().drain().collect::<Vec<String>>().join(",")
        );

        Self {
            props,
            pattern,
        }
    }

    /// Tests whether the configured properties in the given proplist match
    /// the regular expression.
    pub fn matches(&self, proplist: &Proplist) -> bool {
        for key in proplist.iter() {
            if !self.props.contains(&key) {
                continue;
            }
            if let Some(val) = proplist.get_str(&key) {
                if self.pattern.is_match(&val) {
                    return true;
                }
            }
        }

        false
    }
}

impl DriverComponent {
    fn to_error(&self, code: Code) -> ComponentError {
        ComponentError::new(*self, code)
    }
}

// TRAIT IMPLS *****************************************************************
impl Display for DriverComponent {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            DriverComponent::Context => write!(f, "context"),
            DriverComponent::EventHandler => write!(f, "event handler"),
            DriverComponent::Stream => write!(f, "record stream"),
            DriverComponent::Interceptor => write!(f, "stream interceptor"),
        }
    }
}

// PUBLIC INTERFACE FUNCTIONS **************************************************

/// Starts the PulseAudio sound driver with the given configuration, to shut
/// down when a signal is received on `shutdown_rx`.
pub async fn start_driver(
    config: &PulseDriverConfig,
    shutdown_rx: WatchReceiver<bool>
) -> Result<(SampleConsumer, TaskSet<()>), DriverInitError<PulseDriverError>> {
    let (ctx, ctx_task) = connect(
        config.server.as_deref(),
        config.max_mainloop_interval_usec
    ).await.map_err(|e| DriverInitError::ConnectionError(
        PulseDriverError::from(DriverComponent::Context.to_error(e))
    ))?.into_tuple();

    let init_result = initialize(
        ctx,
        shutdown_rx,
        config
    ).await;

    match init_result {
        Ok((stream, mut tasks)) => {
            tasks.insert(ctx_task);
            Ok((stream, tasks.build()))
        },
        Err(e) => {
            //If the driver failed to initialize, we need to await the context
            //handler task to ensure it has a chance to tear down gracefully
            if ctx_task.await.is_err() {
                debug!("Context handler tak aborted or panicked upon driver initialization error");
            }

            Err(DriverInitError::InitializationError(e))
        },
    }
}

// HELPER FUNCTIONS ************************************************************

/// Starts a task to intercept application audio, as specified by the config.
/// Connects to the PulseAudio server at the given socket. If not specified,
/// connects to an automatically-chosen PulseAudio server using the underlying
/// libpulse server discovery logic.
async fn connect(
    server: Option<&str>,
    max_mainloop_interval_usec: Option<u64>
) -> Result<ValueJoinHandle<PulseContextWrapper, ()>, Code> {
    let ctx = PulseContextWrapper::new(
        server,
        max_mainloop_interval_usec
    ).await?;
    info!("Connected to PulseAudio server");

    Ok(ctx)
}

/// Initializes the sound driver after a successful connection to a PulseAudio
/// server, pursuant to the provided configuration.
async fn initialize(
    ctx: PulseContextWrapper,
    shutdown_rx: WatchReceiver<bool>,
    config: &PulseDriverConfig
) -> Result<(SampleConsumer, TaskSetBuilder<()>), PulseDriverError> {
    let mut driver_tasks = TaskSetBuilder::new();

    // Set up the event listener builder (but do not build it just yet)
    let mut event_builder = EventListenerFactory::new(
        &ctx,
        shutdown_rx
    );

    let intercept_mode = config.intercept_mode.unwrap_or(InterceptMode::Capture);
    let target_sink = SinkId::merged(
        config.sink_index,
        config.sink_name.as_ref().map(|s| s.as_str())
    );

    // Determine the sink to read audio samples from
    let stream_handle = if let Some(stream_regex) = &config.stream_regex {
        let props = if config.stream_properties.is_empty() {
            vec![
                APPLICATION_NAME,
                APPLICATION_PROCESS_BINARY,
            ].iter().map(|x| String::from(*x)).collect()
        } else {
            config.stream_properties.clone()
        };

        let matcher = ProplistMatcher::new(props, stream_regex.clone());
        let event_rx = event_builder.build::<OwnedSinkInputInfo>()
            .await
            .map_err(|e| DriverComponent::EventHandler.to_error(e))?
            .filter_map(move |event| {
                match &event {
                    ChangeEvent::New(AudioEntity::Info(info)) |
                    ChangeEvent::Changed(AudioEntity::Info(info)) |
                    ChangeEvent::Removed(AudioEntity::Info(info)) => {
                        if matcher.matches(&info.proplist) {
                            Some(event)
                        } else {
                            let app_name = info.proplist
                                .get_str(APPLICATION_NAME)
                                .unwrap_or(String::from("unknown"));
                            debug!(
                                "Ignoring sink input for non-matching \
                                 application '{}' (index {})",
                                app_name,
                                info.index
                            );
                            None
                        }
                    },
                    _ => Some(event)
                }
            });

        intercept::start(
            &ctx,
            target_sink,
            event_rx,
            intercept_mode,
            config.volume,
            config.intercept_limit,
        ).await.map_err(|e| DriverComponent::Interceptor.to_error(e))?
    } else {
        if intercept_mode == InterceptMode::Monitor {
            warn!(
                "Monitor intercept mode is being used without a stream filter. \
                 This may cause feedback if the output stream is being played \
                 back on the monitored audio device ('{}')",
                if let Some(target_sink) = &target_sink {
                    target_sink.to_string()
                } else {
                    String::from("default")
                }
            );

            let (mut stream_manager, stream) = StreamManager::new(&ctx);
            stream_manager.start(target_sink, config.volume)
                .await
                .map_err(|e| DriverComponent::Stream.to_error(e))?;

            ValueJoinHandle::new(
                stream,
                tokio::spawn(async move {
                    stream_manager.monitor().await
                })
            )
        } else {
            return Err(PulseDriverError::BadConfig(String::from(
                "The `capture' intercept mode requires -E/--stream-regex"
            )));
        }
    };

    let stream = driver_tasks.detaching_insert(stream_handle);

    if let Some(event_task) = event_builder.consume_task() {
        driver_tasks.insert(event_task);
    }

    Ok((stream, driver_tasks))
}
