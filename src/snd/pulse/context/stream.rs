///! PulseAudio stream handling.

extern crate libpulse_binding as libpulse;

use std::panic;
use std::sync::Arc;

use async_ringbuf::{AsyncConsumer, AsyncProducer, AsyncRb};
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use ringbuf::HeapRb;
use tokio::sync::oneshot::{
    self,
    Receiver as OneshotReceiver,
    Sender as OneshotSender
};
use tokio::task::JoinHandle;

use libpulse::channelmap::Map as ChannelMap;
use libpulse::context::Context;
use libpulse::error::Code;
use libpulse::sample::{Format as SampleFormat, Spec as SampleSpec};
use libpulse::stream::{
    FlagSet,
    PeekResult,
    State,
    Stream
};

use crate::util::{Lease, Lessor};
use super::super::owned::{OwnedSinkInfo, OwnedSinkInputInfo};
use super::{AsyncIntrospector, PulseContextWrapper};
use super::async_polyfill::{InitializingFuture, StreamReadNotifier};

// MODULE-INTERNAL CONSTANTS ***************************************************

/// The audio sample rate in Hz. Per the driver specification, this must be
/// 48 kHz.
const SAMPLE_RATE: u32 = 48000;

/// The size of the sample buffer in bytes.
const SAMPLE_QUEUE_SIZE: usize = SAMPLE_RATE as usize * 2;

/// PulseAudio [`SampleSpec`] definition matching the requirements of the driver
/// specification.
const SAMPLE_SPEC: SampleSpec = SampleSpec {
    format: SampleFormat::F32le,
    channels: 2,
    rate: SAMPLE_RATE,
};

const MAX_CONSECUTIVE_ERRORS: usize = 3;

// TYPE DEFINITIONS ************************************************************
pub trait StreamConfig {
    fn configure(&self, stream: Stream) -> Result<Stream, Code>;
}

#[async_trait]
pub trait IntoStreamConfig {
    type Target: StreamConfig + Send + 'static;

    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code>;
}

pub struct StreamManager {
    ctx: PulseContextWrapper,
    sample_tx: Lessor<Producer<u8>>,
    task: Option<(OneshotSender<()>, JoinHandle<()>)>,
}

pub struct ResolvedSinkInput(OwnedSinkInputInfo, OwnedSinkInfo);

enum StreamWriteResult {
    DataWritten,
    PulseError(Code),
    HandleClosed,
    NoOp,
}

type Consumer<T> = AsyncConsumer<T, Arc<AsyncRb<T, HeapRb<T>>>>;

type Producer<T> = AsyncProducer<T, Arc<AsyncRb<T, HeapRb<T>>>>;

/// Type alias for the concrete audio consumer type.
pub type SampleConsumer = Consumer<u8>;

// TYPE IMPLS ******************************************************************
impl StreamManager {
    pub fn new(ctx: PulseContextWrapper) -> (Self, Consumer<u8>) {
        let (tx, rx) = AsyncRb::<u8, HeapRb<u8>>::new(SAMPLE_QUEUE_SIZE).split();

        (Self {
            ctx,
            sample_tx: Lessor::new(tx),
            task: None,
        }, rx)
    }

    pub fn is_running(&self) -> bool {
        self.task.is_some()
    }

    pub async fn monitor(&mut self) {
        let panic_obj = if let Some((_, task_handle)) = self.task.as_mut() {
            let task_res = task_handle.await;

            if let Err(task_err) = task_res {
                if let Ok(panic_obj) = task_err.try_into_panic() {
                    Some(panic_obj)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // If this function is not cancelled before this point is reached, the
        // task is guaranteed to be stopped.
        if self.task.is_some() {
            self.task = None;
        }

        if let Some(panic_obj) = panic_obj {
            panic::resume_unwind(panic_obj);
        }

        self.sample_tx.await_release().await;
    }

    pub async fn start<'a>(
        &'a mut self,
        source: impl IntoStreamConfig + 'a,
        volume: Option<f64>
    ) -> Result<(), Code> {
        if let Some(sample_tx) = self.sample_tx.lease() {
            if self.is_running() {
                return Err(Code::Exist);
            }

            let stream = get_connected_stream(&self.ctx, source).await?;

            if let Some(volume) = volume {
                AsyncIntrospector::from(&self.ctx).set_source_output_volume(
                    stream.get_index().unwrap(),
                    volume
                ).await?;
            }

            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let task_handle = self.ctx.spawn(do_stream(
                stream,
                sample_tx,
                shutdown_rx
            )).await;

            self.task = Some((shutdown_tx, task_handle));
            Ok(())
        } else {
            Err(Code::Exist)
        }
    }

    pub async fn stop(&mut self) {
        if let Some((shutdown_tx, task_handle)) = self.task.take() {
            if shutdown_tx.send(()).is_err() && !task_handle.is_finished() {
                warn!("PulseAudio stream handler ignored shutdown signal. \
                       Forcibly aborting task");
                task_handle.abort();
            }

            if let Err(task_err) = task_handle.await {
                if let Ok(panic_obj) = task_err.try_into_panic() {
                    panic::resume_unwind(panic_obj);
                }
            }

            self.sample_tx.await_release().await;
        }
    }
}

// TRAIT IMPLS *****************************************************************
#[async_trait]
impl<C: StreamConfig + Send + 'static> IntoStreamConfig for C {
    type Target = C;

    async fn resolve(
        self,
        _: &AsyncIntrospector
    ) -> Result<Self::Target, Code> {
        Ok(self)
    }
}

#[async_trait]
impl<I> IntoStreamConfig for Option<I>
where I: IntoStreamConfig<Target = OwnedSinkInfo> + Send {
    type Target = OwnedSinkInfo;

    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code> {
        if let Some(into_config) = self {
            into_config.resolve(introspect).await
        } else {
            introspect.get_default_sink().await
        }
    }
}

#[async_trait]
impl IntoStreamConfig for &str {
    type Target = OwnedSinkInfo;

    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code> {
        introspect.get_sink_by_name(self).await
    }
}

#[async_trait]
impl IntoStreamConfig for u32 {
    type Target = OwnedSinkInfo;

    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code> {
        introspect.get_sink_by_index(self).await
    }
}

#[async_trait]
impl IntoStreamConfig for OwnedSinkInputInfo {
    type Target = ResolvedSinkInput;

    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code> {
        Ok(ResolvedSinkInput(
            self,
            introspect.get_sink_by_index(self.sink).await?
        ))
    }
}

impl StreamConfig for OwnedSinkInfo {
    fn configure(&self, mut stream: Stream) -> Result<Stream, Code> {
        if let Some(source_name) = self.monitor_source_name.as_ref() {
           stream.connect_record(
               Some(&source_name),
               None,
               FlagSet::START_UNMUTED
            ).map_err(|pa_err| Code::try_from(pa_err).unwrap_or(Code::Unknown))?;

           Ok(stream)
        } else {
            Err(Code::NoEntity)
        }
    }
}

impl StreamConfig for ResolvedSinkInput {
    fn configure(&self, mut stream: Stream) -> Result<Stream, Code> {
        stream.set_monitor_stream(self.0.index)
            .map_err(|pa_err| Code::try_from(pa_err).unwrap_or(Code::Unknown))?;

        self.1.configure(stream)
    }
}

impl Drop for StreamManager {
    fn drop(&mut self) {
        debug!("PulseAudio stream handler shutting down");

        if let Some((shutdown_tx, _)) = self.task.take() {
            warn!("PulseAudio stream manager shutting down while stream handler active");
            if shutdown_tx.send(()).is_err() {
                info!(
                    "PulseAudio stream handler task unexpectedly shut down before manager"
                );
            }
        }

        debug!("PulseAudio stream handler shut down");
    }
}

// HELPER FUNCTIONS ************************************************************
/// Determines if the stream handler should continue reading samples, or exit.
///
/// This also waits for data to appear in the underlying stream, to prevent
/// needless iterating over a stream with no data.
async fn should_continue_stream_read(
    sample_tx: &AsyncProducer<u8, Arc<AsyncRb<u8, HeapRb<u8>>>>,
    notify: &StreamReadNotifier,
    shutdown_rx: &mut OneshotReceiver<()>
) -> bool {
    if sample_tx.is_closed() {
        false
    } else {
        tokio::select! {
            _ = notify.await_data() => true,
            _ = shutdown_rx => false,
        }
    }
}

async fn stream_read(
    stream: &mut Stream,
    sample_tx: &mut Producer<u8>
) -> StreamWriteResult {
    match stream.peek() {
        Ok(PeekResult::Data(samples)) => {
            let mut res = StreamWriteResult::DataWritten;

            if sample_tx.push_iter(samples.iter().copied()).await.is_err() {
                debug!("Sample receiver dropped while transmitting samples");
                res = StreamWriteResult::HandleClosed;
            }

            if let Err(e) = stream.discard() {
                warn!("Failure to discard stream sample: {}", e);

                if !matches!(res, StreamWriteResult::HandleClosed) {
                    res = StreamWriteResult::PulseError(
                        Code::try_from(e).unwrap_or(Code::Unknown)
                    )
                }
            }

            res
        },
        Ok(PeekResult::Hole(hole_size)) => {
            warn!(
                "PulseAudio stream returned hole of size {} bytes",
                hole_size
            );
            if let Err(e) = stream.discard() {
                warn!("Failure to discard stream sample: {}", e);
                StreamWriteResult::PulseError(
                    Code::try_from(e).unwrap_or(Code::Unknown)
                )
            } else {
                StreamWriteResult::NoOp
            }
        },
        Ok(PeekResult::Empty) => {
            trace!("PulseAudio stream had no data");
            StreamWriteResult::NoOp
        },
        Err(e) => StreamWriteResult::PulseError(
            Code::try_from(e).unwrap_or(Code::Unknown)
        ),
    }
}

async fn do_stream(
    mut stream: Stream,
    mut sample_tx: Lease<Producer<u8>>,
    mut shutdown_rx: OneshotReceiver<()>
) {
    let notify = StreamReadNotifier::new(&mut stream);
    let mut err_count = 0usize;

    while should_continue_stream_read(
        &sample_tx,
        &notify,
        &mut shutdown_rx
    ).await {
        match stream_read(&mut stream, &mut sample_tx).await {
            StreamWriteResult::HandleClosed => break,
            StreamWriteResult::PulseError(e) => {
                error!("Error reading from stream: {}", e);
                err_count += 1;

                if err_count >= MAX_CONSECUTIVE_ERRORS {
                    break;
                }
            },
            StreamWriteResult::DataWritten | StreamWriteResult::NoOp => {
                err_count = 0
            },
        }
    }

    debug!("Stream handler shutting down");
    stream_disconnect(&mut stream, notify);
    debug!("Stream handler shut down");

    if err_count >= MAX_CONSECUTIVE_ERRORS {
        panic!(
            "Stream handler encountered {} consecutive errors, exiting abnormally",
            err_count
        );
    }
}

fn stream_disconnect(
    stream: &mut Stream,
    notify: StreamReadNotifier
) {
    notify.close(stream);

    if let Err(e) = stream.disconnect() {
        warn!("Record stream failed to disconnect: {}", e);
    }
}

/// Creates and returns a new recording stream against the source with the
/// given name.
async fn get_connected_stream(
    ctx_wrap: &PulseContextWrapper,
    source: impl IntoStreamConfig
) -> Result<Stream, Code> {
    let source = source.resolve(&AsyncIntrospector::from(ctx_wrap)).await?;

    let stream_fut = ctx_wrap.with_ctx(move |ctx| create_and_connect_stream(
        ctx,
        source
    )).await?;

    stream_fut.await_on(ctx_wrap).await
        .map_err(|state| if state == State::Terminated {
            Code::Killed
        } else {
            Code::BadState
        })
}

/// Creates a recording stream against the named audio source, returning it
/// as an [`InitializingFuture`] that resolves once the stream enters the ready
/// state.
fn create_and_connect_stream(
    ctx: &mut Context,
    config: impl StreamConfig
) -> Result<InitializingFuture<State, Stream>, Code> {
    //Technically init_stereo should create a new channel map with the format
    //specifier "front-left,front-right", but due to oddities in the Rust
    //binding for PulseAudio, we have to manually specify the format first. We
    //call init_stereo anyways to make sure we are always using the server's
    //understanding of stereo audio.
    let mut channel_map = ChannelMap::new_from_string("front-left,front-right")
        .map_err(|_| Code::Invalid)?;
    channel_map.init_stereo();

    let mut stream = Stream::new(
        ctx,
        "capture stream",
        &SAMPLE_SPEC,
        Some(&channel_map)
    ).expect("Failed to open record stream");

    Ok(InitializingFuture::from(config.configure(stream)?))
}
