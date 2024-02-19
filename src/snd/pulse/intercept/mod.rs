///! Utilities for intercepting application audio streams for recording.
///!
///! The core abstraction in this modules is the [`Interceptor`] trait, which
///! allows bardcast to intercept applications playing audio via their sink
///! input ID. When used in conjunction with the event notification
///! functionality of the [`super::event`] module, this allows applications to
///! be efficiently intercepted as soon as they open an audio stream.

mod types;
mod util;

use std::borrow::Cow;

use libpulse_binding::error::Code;
use log::{debug, error, info, warn};
use tokio::sync::broadcast::error::RecvError;

use crate::cfg::InterceptMode;
use crate::util::task::ValueJoinHandle;
use self::types::{
    CapturingInterceptor,
    DuplexingInterceptor,
    InterceptError,
    Interceptor,
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

// PUBLIC HELPER FUNCTIONS *****************************************************
/// Primary entry point for all interceptors.
///
/// This automatically determines the appropriate interceptor implementation
/// from `mode` and `limit`.
///
/// The value of `sink` is ignored by the `Capture` intercept mode, and the
/// `Monitor` intercept mode when `limit` is `Some(1)`, as no existing sink is
/// needed for these intercept modes to function properly.
///
/// The provided event listener should be configured to perform any necessary
/// filtering of events prior to being passed to this function. Events **MUST
/// NOT** be filtered by type, as the interceptor logic needs to be notified of
/// all entity lifecycle events.
///
/// The interceptor will ignore `Changed` and `Removed` events that do not match
/// any current intercepts.
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
/// Generic implementation of `start()`, for use with a configured
/// [`Interceptor`] instance.
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
                    if let Err(e) = intercept.update_intercept(&input).await {
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
