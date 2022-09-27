///! Root of the PulseAudio sound driver implementation.

mod intercept;
mod cfg;
mod context;
mod event;
mod owned;

extern crate libpulse_binding as libpulse;

use std::collections::HashSet;
use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};

use log::{debug, error, info, warn};
use regex::Regex;
use tokio::sync::broadcast::error::RecvError as BroadcastRecvError;
use tokio::sync::watch::Receiver as WatchReceiver;

use libpulse::error::{Code, PAErr};
use libpulse::proplist::Proplist;
use libpulse::proplist::properties::{
    APPLICATION_NAME,
    APPLICATION_PROCESS_BINARY
};

use crate::cfg::InterceptMode;
use crate::util;
use crate::util::task::{TaskSetBuilder, ValueJoinHandle};
use self::intercept::{Interceptor, CapturingInterceptor, PeekingInterceptor};
use self::context::{AsyncIntrospector, PulseContextWrapper, SampleConsumer};
use self::event::{
    AudioEntity,
    ChangeEvent,
    EventListener,
    EventListenerError,
    OwnedEventListener,
    ToFilterMapped
};
use self::event::factory::EventListenerFactory;
use self::owned::{OwnedSinkInfo, OwnedSinkInputInfo};
use super::{Driver, DriverStartError};

pub use self::cfg::PulseDriverConfig;

pub const DRIVER_NAME: &'static str = "pulse";

// TYPE DEFINITIONS ************************************************************

/// Error type for differing kinds of failure modes with the PulseAudio API.
#[derive(Debug, Copy, Clone)]
pub enum PulseFailure {
    /// Wrapper around an underlying PulseAudio API error. Also used to indicate
    /// error conditions within the driver code itself.
    Error(Code),

    /// Indicates that a PulseAudio connection or entity unexpectedly
    /// terminated. Provides the terminated entity's exit code.
    // TODO: This is used in several places that don't provide an exit code, and
    // a fake value is used instead. This should instead wrap an Option<i32> to
    // better represent the underlying termination.
    Terminated(i32),
}

/// Helper for matching specific properties in a [`Proplist`] against a regular
/// expression.
#[derive(Debug, Clone)]
pub struct ProplistMatcher {
    props: HashSet<String>,
    pattern: Regex,
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

// TRAIT IMPLS *****************************************************************
impl From<Code> for PulseFailure {
    fn from(code: Code) -> Self {
        PulseFailure::Error(code)
    }
}

impl From<PAErr> for PulseFailure {
    fn from(pa_err: PAErr) -> Self {
        PulseFailure::from(Code::try_from(pa_err).unwrap_or(Code::Unknown))
    }
}

impl Display for PulseFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            PulseFailure::Error(code) => write!(f, "PulseAudio error: {}", code),
            PulseFailure::Terminated(code) => write!(f, "PulseAudio connection terminated with status {}", code),
        }
    }
}

impl Error for PulseFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let PulseFailure::Error(code) = self {
            Some(code)
        } else {
            None
        }
    }
}

// PUBLIC INTERFACE FUNCTIONS **************************************************

/// Starts the PulseAudio sound driver with the given configuration, to shut
/// down when a signal is received on `shutdown_rx`.
pub async fn start_driver(
    config: &PulseDriverConfig,
    shutdown_rx: WatchReceiver<bool>
) -> Result<Driver, DriverStartError> {
    let (ctx, ctx_task) = connect(config.server.as_deref()).await
        .map_err(|e| DriverStartError::ConnectionFailure(
            format!("Failed to connect to PulseAudio ({})", e)
        ))?.into_tuple();

    let init_result = initialize(
        ctx,
        shutdown_rx,
        config
    ).await;

    match init_result {
        Ok((stream, mut tasks)) => {
            tasks.insert(ctx_task);
            Ok(Driver::new(stream, tasks.build()))
        },
        Err(e) => {
            //If the driver failed to initialize, we need to await the context
            //handler task to ensure it has a chance to tear down gracefully
            if ctx_task.await.is_err() {
                debug!("Context handler tak aborted or panicked upon driver initialization error");
            }

            Err(DriverStartError::InitializationFailure(
                format!("PulseAudio driver failed to initialize ({})", e)
            ))
        },
    }
}

// HELPER FUNCTIONS ************************************************************

/// Resolves the metadata for the audio sink targeted by the given
/// configuration. If a specified sink cannot be found, this returns
/// `Err(PulseFailure::Error(Code::NoEntity))`.
async fn resolve_configured_sink(
    introspect: &AsyncIntrospector,
    config: &PulseDriverConfig
) -> Result<OwnedSinkInfo, PulseFailure> {
    if let Some(sink_index) = config.sink_index {
        introspect.get_sink_by_index(sink_index).await
    } else if let Some(sink_name) = &config.sink_name {
        introspect.get_sink_by_name(sink_name).await
    } else {
        introspect.get_default_sink().await
    }
}

/// Starts a task to intercept application audio, as specified by the config.
async fn start_stream_intercept(
    ctx: &PulseContextWrapper,
    event_rx: OwnedEventListener<OwnedSinkInputInfo>,
    matcher: ProplistMatcher,
    config: &PulseDriverConfig,
) -> Result<ValueJoinHandle<String>, PulseFailure> {
    let introspect = AsyncIntrospector::from(ctx);
    let capture_mode = config.intercept_mode.unwrap_or(InterceptMode::Peek);

    debug!("Establishing event listener for application stream intercept");
    let mut event_rx = event_rx.filter_map(move |event| {
        match event {
            ChangeEvent::New(AudioEntity::Info(input)) => if matcher.matches(&input.proplist) {
                Some(input)
            } else {
                let app_name = input.proplist.get_str(APPLICATION_NAME).unwrap_or(String::from("unknown"));
                debug!(
                    "Ignoring sink input for non-matching application '{}' (index {})",
                    app_name,
                    input.index
                );
                None
            },
            ChangeEvent::New(AudioEntity::Index(idx)) => {
                warn!(
                    "Metadata not provided for sink input at index {}, not capturing input",
                    idx
                );
                None
            },
            _ => None,
        }
    });

    // Set up the stream interceptor
    info!("Intercepting matched applications using mode: {:?}", capture_mode);
    let intercept: Box<dyn Interceptor> = match capture_mode {
        InterceptMode::Peek => Box::new(PeekingInterceptor::from_sink(
            &introspect,
            &resolve_configured_sink(&introspect, config).await?,
        ).await?),
        InterceptMode::Capture => Box::new(CapturingInterceptor::new(
            &introspect,
        ).await?),
        _ => panic!("Attempted to start stream intercept with unsupported capture mode"),
    };

    let source_name = intercept.source_name();
    let (source_name, mut intercept) = intercept::boxed_close_interceptor_if_err(
        intercept,
        source_name
    ).await?;

    // Start intercepting new matching audio streams
    debug!("Starting application stream intercept task");
    Ok(ValueJoinHandle::new(source_name, tokio::spawn(async move {
        loop {
            match event_rx.next_ignore_lag().await {
                Ok(sink_input) => {
                    let app_name = sink_input.proplist.get_str(APPLICATION_NAME).unwrap_or(String::from("unknown"));
                    info!(
                        "Intercepting matching application '{}' (index {})",
                        app_name,
                        sink_input.index
                    );

                    if let Err(err) = intercept.intercept(sink_input.index).await {
                        error!(
                            "Failed to intercept matching application '{}' (index {}): {}",
                            app_name,
                            sink_input.index,
                            err
                        );
                    }
                },
                Err(EventListenerError::LookupError(err)) => {
                    warn!(
                        "Failed to look up metadata for new sink input at index {}: {}",
                        err.raw_event().entity().index(),
                        err.source().unwrap()
                    );
                },
                Err(EventListenerError::ChannelError(BroadcastRecvError::Closed)) => break,
                Err(_) => (),
            }
        }

        intercept.boxed_close().await;
        debug!("Application stream intercept shut down");
    })))
}

/// Connects to the PulseAudio server at the given socket. If not specified,
/// connects to an automatically-chosen PulseAudio server using the underlying
/// libpulse server discovery logic.
async fn connect(server: Option<&str>) -> Result<ValueJoinHandle<PulseContextWrapper>, PulseFailure> {
    let ctx = PulseContextWrapper::new(server).await?;
    info!("Connected to PulseAudio server");

    Ok(ctx)
}

/// Initializes the sound driver after a successful connection to a PulseAudio
/// server, pursuant to the provided configuration.
async fn initialize(
    ctx: PulseContextWrapper,
    shutdown_rx: WatchReceiver<bool>,
    config: &PulseDriverConfig
) -> Result<(SampleConsumer, TaskSetBuilder), PulseFailure> {
    let mut driver_tasks = TaskSetBuilder::new();

    // Set up the event listener builder (but do not build it just yet)
    let mut event_builder = EventListenerFactory::new(
        &ctx,
        shutdown_rx
    );

    let monitor_mode = util::opt_is_some_and(&config.intercept_mode, |mode| *mode == InterceptMode::Monitor);

    // Determine the sink to read audio samples from
    let rec_name = if monitor_mode {
        let introspect = AsyncIntrospector::from(&ctx);

        introspect.resolve_sink_monitor_name(resolve_configured_sink(
            &introspect,
            config
        ).await?).await?
    } else {
        // Application stream intercept should only be used if a filter is present
        if let Some(stream_regex) = &config.stream_regex {
            let props = if config.stream_properties.is_empty() {
                vec![
                    APPLICATION_NAME,
                    APPLICATION_PROCESS_BINARY,
                ].iter().map(|x| String::from(*x)).collect()
            } else {
                config.stream_properties.clone()
            };

            let matcher = ProplistMatcher::new(props, stream_regex.clone());
            driver_tasks.detaching_insert(start_stream_intercept(
                &ctx,
                event_builder.build().await?,
                matcher,
                config
            ).await?)
        } else {
            return Err(PulseFailure::from(Code::BadState));
        }
    };

    let stream = driver_tasks.detaching_insert(ctx.open_rec_stream(
        rec_name,
        config.volume
    ).await?);

    if let Some(event_task) = event_builder.consume_task() {
        driver_tasks.insert(event_task);
    }

    Ok((stream, driver_tasks))
}
