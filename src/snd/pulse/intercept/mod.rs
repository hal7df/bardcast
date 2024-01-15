///! Utilities for intercepting application audio streams for recording.
///!
///! The core abstraction in this modules is the [`Interceptor`] trait, which
///! allows bardcast to intercept applications playing audio via their sink
///! input ID. When used in conjunction with the event notification
///! functionality of the [`super::event`] module, this allows applications to
///! be efficiently intercepted as soon as they open an audio stream.

mod concrete;
mod util;

use std::borrow::Cow;
use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};

use async_trait::async_trait;
use libpulse_binding::error::Code;
use log::{debug, error, info, warn};
use tokio::sync::broadcast::error::RecvError;

use crate::cfg::InterceptMode;
use crate::util::task::ValueJoinHandle;
use self::concrete::{
    CapturingInterceptor,
    DuplexingInterceptor,
    SingleInputMonitor,
    QueuedInterceptor
};
use super::context::{PulseContextWrapper, AsyncIntrospector};
use super::context::stream::{IntoStreamConfig, SampleConsumer};
use super::event::{
    AudioEntity,
    ChangeEvent,
    EventListener,
    EventListenerError
};
use super::owned::{OwnedSinkInfo, OwnedSinkInputInfo};

// CONSTANT DEFINITIONS ********************************************************
const TEARDOWN_FAILURE_WARNING: &'static str =
    "Application audio may not work correctly. Please check your system audio \
     configuration.";
const SINK_INPUT_MOVE_FAILURE: &'static str =
    "Some captured inputs failed to move to their original or default sink.";

// TYPE DEFINITIONS ************************************************************
/// Represents possible error modes for [`Interceptor::record()`].
#[derive(Debug)]
enum InterceptError<'a> {
    /// PulseAudio returned an error while attempting to intercept the
    /// application.
    PulseError(Code),

    /// The interceptor cannot concurrently intercept any more applications.
    AtCapacity(usize, Cow<'a, OwnedSinkInputInfo>),
}

/// Core abstraction for intercepting application audio.
///
/// The main function in this interface is [`intercept`], which performs the
/// work of intercepting applications and routing them such that bardcast can
/// read their stream. This interface also provides a few functions to query the
/// current state of intercepted streams.
#[async_trait]
trait Interceptor: Send + Sync {
    /// Starts recording audio from the given application.
    async fn record<'a>(
        &mut self,
        source: Cow<'a, OwnedSinkInputInfo>
    ) -> Result<(), InterceptError<'a>>;

    /// Updates the stream metadata for the given captured stream. This may
    /// cork or uncork the stream depending on the cork state of all captured
    /// applications.
    async fn update_capture(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code>;

    /// Stops recording audio from the given application. If this interceptor
    /// is not currently recording audio from the given application, this
    /// returns `Ok(false)`.
    async fn stop(
        &mut self,
        source_idx: u32
    ) -> Result<bool, Code>;

    /// Determines if [`Interceptor::monitor()`] must be called to ensure
    /// underlying tasks are properly monitored and cleaned up during normal
    /// operation.
    fn needs_monitor(&self) -> bool;

    /// Monitors and cleans up underlying tasks, if any.
    async fn monitor(&mut self);

    /// Returns the number of applications that have been intercepted by this
    /// interceptor.
    fn len(&self) -> usize;

    /// Determines whether the consuming end of the stream has closed.
    fn stream_closed(&self) -> bool;

    /// Gracefully closes any system resources that were created by the
    /// `Interceptor`, returning any intercepted streams to another audio device
    /// if needed.
    async fn close(mut self);
}

/// Specialization of [`Interceptor`] that allows for limits on the number of
/// concurrently captured applications.
trait LimitingInterceptor: Interceptor {
    /// Returns the number of concurrent applications that can be intercepted
    /// by this `LimitedInterceptor` instance.
    ///
    /// Returns `None` if there is no limit applied to this interceptor.
    fn capacity(&self) -> Option<usize>;

    /// Returns the number of additional applications that can be concurrently
    /// intercepted by this `LimitedInterceptor` instance.
    ///
    /// Returns `None` if there is no limit applied to this interceptor.
    fn remaining(&self) -> Option<usize> {
        self.capacity().map(|capacity| capacity - self.len())
    }

    /// Determines whether this interceptor has intercepted the maximum number
    /// of concurrent applications.
    ///
    /// This will always be `false` for interceptors with no limit.
    fn is_full(&self) -> bool {
        self.remaining().is_some_and(|remaining| remaining == 0)
    }
}

// TRAIT IMPLS *****************************************************************
impl From<Code> for InterceptError<'_> {
    fn from(code: Code) -> Self {
        Self::PulseError(code)
    }
}

impl<'a> TryFrom<InterceptError<'a>> for Code {
    type Error = InterceptError<'a>;

    fn try_from(err: InterceptError<'a>) -> Result<Self, Self::Error> {
        if let InterceptError::PulseError(code) = err {
            Ok(code)
        } else {
            Err(err)
        }
    }
}

impl Display for InterceptError<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            Self::PulseError(code) => code.fmt(f),
            Self::AtCapacity(limit, source) => write!(
                f,
                "Cannot intercept application {}, interceptor at limit ({})",
                source.as_ref(),
                limit
            ),
        }
    }
}

/// Due to lifetime constraints, `InterceptError` cannot correctly implement
/// [`Error::source()`]. To get a [`Code`] from an `InterceptError`, use
/// [`Code::try_from`].
impl Error for InterceptError<'_> {}

// PUBLIC HELPER FUNCTIONS *****************************************************
pub async fn start<L>(
    ctx: &PulseContextWrapper,
    sink: impl IntoStreamConfig<Target = OwnedSinkInfo>,
    listen: L,
    mode: InterceptMode,
    volume: Option<f64>,
    limit: Option<usize>,
) -> Result<ValueJoinHandle<SampleConsumer, ()>, Code>
where
    L: EventListener<
        OwnedSinkInputInfo,
        ChangeEvent<AudioEntity<OwnedSinkInputInfo>>
    > + 'static
{
    match mode {
        InterceptMode::Capture => {
            let (intercept, stream) = CapturingInterceptor::new(
                ctx,
                volume,
                limit
            ).await?;

            Ok(ValueJoinHandle::new(
                stream,
                if limit.is_some() {
                    tokio::spawn(run(
                        QueuedInterceptor::from(intercept),
                        listen,
                    ))
                } else {
                    tokio::spawn(run(intercept, listen))
                }
            ))
        },
        InterceptMode::Monitor => {
            if let Some(1) = limit {
                let (intercept, stream) = SingleInputMonitor::new(
                    ctx,
                    volume
                ).await;

                Ok(ValueJoinHandle::new(
                    stream,
                    tokio::spawn(run(
                        QueuedInterceptor::from(intercept),
                        listen
                    ))
                ))
            } else {
                let (intercept, stream) = DuplexingInterceptor::from_sink(
                    ctx,
                    &sink.resolve(&AsyncIntrospector::from(ctx)).await?,
                    volume,
                    limit
                ).await?;

                Ok(ValueJoinHandle::new(
                    stream,
                    if limit.is_some() {
                        tokio::spawn(run(
                            QueuedInterceptor::from(intercept),
                            listen
                        ))
                    } else {
                        tokio::spawn(run(intercept, listen))
                    }
                ))
            }
        }
    }
}

// HELPER FUNCTIONS ************************************************************
async fn run<L>(
    mut intercept: impl Interceptor,
    mut listen: L
)
where
    L: EventListener<
        OwnedSinkInputInfo,
        ChangeEvent<AudioEntity<OwnedSinkInputInfo>>
    >
{
    debug!("Application stream intercept task started");

    while !intercept.stream_closed() {
        tokio::select! {
            change_event = listen.next_ignore_lag() => match change_event {
                Ok(ChangeEvent::New(AudioEntity::Info(input))) => {
                    let input_idx = input.index;

                    match intercept.record(Cow::Owned(input)).await {
                        Ok(()) => {},
                        Err(InterceptError::PulseError(e)) => error!(
                            "Failed to intercept application with index {} \
                             (error: {})",
                            input_idx,
                            e
                        ),
                        Err(InterceptError::AtCapacity(n, _)) => error!(
                            "Failed to intercept application with index {} as \
                             the interceptor was unexpectedly at capacity \
                             ({}). This should not happen; please submit a bug \
                             report with your run configuration.",
                            input_idx,
                            n
                        ),
                    }
                },
                Ok(ChangeEvent::Changed(AudioEntity::Info(input))) => {
                    if let Err(e) = intercept.update_capture(&input).await {
                        warn!(
                            "Failed to read status update for application with \
                             index {} (error: {}). This may prevent audio from \
                             being recorded.",
                            input.index,
                            e
                        );
                    }
                },
                Ok(ChangeEvent::Removed(entity)) => {
                    let idx = match entity {
                        AudioEntity::Info(info) => info.index,
                        AudioEntity::Index(index) => index,
                    };

                    match intercept.stop(idx).await {
                        Ok(true) => info!(
                            "Stopped intercept on application with index {}",
                            idx
                        ),
                        Ok(false) => debug!(
                            "Did not stop intercept on application with index \
                             {} as it was not intercepted",
                            idx
                        ),
                        Err(e) => error!(
                            "Failed to stop intercept on application with \
                             index {} (error: {}). {}",
                            idx,
                            e,
                            TEARDOWN_FAILURE_WARNING
                        ),
                    }
                },
                Ok(ChangeEvent::New(AudioEntity::Index(input_idx))) |
                Ok(ChangeEvent::Changed(AudioEntity::Index(input_idx))) => info!(
                    "Ignoring new/change event for unresolved sink input  with \
                     index {}",
                    input_idx
                ),
                Err(EventListenerError::LookupError(e)) => warn!(
                    "Interceptor could not update intercepts as the entity \
                     lookup for a change event failed (error: {}).",
                    e
                ),
                Err(EventListenerError::ChannelError(RecvError::Closed)) => {
                    // If the event listener shuts down, that is an indication
                    // this task must shut down too
                    break;
                },
                Err(EventListenerError::ChannelError(RecvError::Lagged(_))) => debug!(
                    "Intercept unexpectedly received unhandled channel lag \
                     notification"
                ),
            },
            _ = intercept.monitor(), if intercept.needs_monitor() => {},
        }
    }

    intercept.close().await;
    debug!("Application stream intercept shut down");
}
