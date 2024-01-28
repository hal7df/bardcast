///! PulseAudio stream handling.

extern crate libpulse_binding as libpulse;

use std::fmt::{Display, Error as FormatError, Formatter};
use std::panic;
use std::sync::Arc;

use async_ringbuf::{AsyncConsumer, AsyncProducer, AsyncRb};
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use ringbuf::HeapRb;
use tokio::sync::oneshot::{
    self,
    Sender as OneshotSender
};
use tokio::task::{self, JoinHandle};

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
use super::super::owned::{OwnedSinkInfo, OwnedSinkInputInfo, OwnedSourceInfo};
use super::{AsyncIntrospector, PulseContextWrapper};
use super::async_polyfill::{self, InitializingFuture, StreamReadNotifier};
use super::collect::CollectResult;

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

/// The maximum number of consecutive non-fatal stream read errors that are
/// allowed before the stream task automatically terminates.
const MAX_CONSECUTIVE_ERRORS: usize = 3;

/// Capacity of the stream queue.
const COMMAND_QUEUE_SIZE: usize = 3;

// TYPE DEFINITIONS ************************************************************
/// Trait for configuring a stream to read from an implementation-specified
/// source. Responsible for calling `Stream::record()`.
///
/// This is typically a PulseAudio source object or sink input.
pub trait StreamConfig {
    /// Configures the given unconfigured stream object for the audio source
    /// specified by the implementation. This must generally call
    /// `Stream::record()`.
    fn configure(&self, stream: Stream) -> Result<Stream, Code>;
}

/// Helper trait for resolving partial stream source information into a
/// fully-resolved [`StreamConfig`] object.
///
/// A null automatic implementation is provided for all types that implement
/// `StreamConfig`.
#[async_trait]
pub trait IntoStreamConfig {
    /// The `StreamConfig` type that this resolves to.
    type Target: StreamConfig + Send + 'static;

    /// Resolves this object for use for configuring an audio stream.
    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code>;
}

/// Represents the possible outcomes of processing a call to `Stream::peek()`.
enum StreamWriteResult {
    /// Audio data was written to the internal stream handle.
    DataWritten,

    /// A PulseAudio error occurred when attempting to read stream data.
    PulseError(Code),

    /// The receiving end attached to the internal stream handle closed,
    /// preventing any further writes.
    HandleClosed,

    /// No action was performed, nor was any actionable condition encountered.
    NoOp,
}

/// Commands that can be sent to the stream handler task.
enum StreamCommand {
    /// Instructs the stream handler task to cork or uncork the stream,
    /// according to the `bool` parameter. The result of the operation is
    /// transmitted via tha provided channel.
    SetCork(bool, OneshotSender<Result<(), Code>>),

    /// Instructs the stream handler task to shut down.
    Shutdown,
}

/// Type alias for generic, asynchronous ring buffer consumers.
type Consumer<T> = AsyncConsumer<T, Arc<AsyncRb<T, HeapRb<T>>>>;

/// Type alias for generic, asynchronous ring buffer producers.
type Producer<T> = AsyncProducer<T, Arc<AsyncRb<T, HeapRb<T>>>>;

/// Type alias for the concrete audio consumer type.
pub type SampleConsumer = Consumer<u8>;

/// Manages the producing end of the application-internal stream, allowing one
/// stream handle to be reused for different `Stream` instances.
pub struct StreamManager {
    ctx: PulseContextWrapper,
    sample_tx: Lessor<Producer<u8>>,
    task: Option<(Producer<StreamCommand>, JoinHandle<()>)>,
}

/// Represents a unique identifier for a PulseAudio sink.
pub enum SinkId<'a> {
    Index(u32),
    Name(&'a str),
}

/// Allows a sink input to be used as a `StreamConfig` (sink inputs by
/// themselves do not provide the name of the sink monitor, so this caches the
/// name of the associated sink monitor after lookup).
pub struct ResolvedSinkInput(OwnedSinkInputInfo, OwnedSinkInfo);

// TYPE IMPLS ******************************************************************
impl StreamManager {
    /// Creates a new `StreamManager` against the given PulseAudio context. The
    /// consuming end of the application-internal stream is returned in addition
    /// to the manager instance.
    pub fn new(ctx: &PulseContextWrapper) -> (Self, Consumer<u8>) {
        let (tx, rx) = AsyncRb::<u8, HeapRb<u8>>::new(SAMPLE_QUEUE_SIZE).split();

        (Self {
            ctx: ctx.clone(),
            sample_tx: Lessor::new(tx),
            task: None,
        }, rx)
    }

    /// Checks if the PulseAudio `Stream` is being actively read.
    pub fn is_running(&self) -> bool {
        self.task.is_some()
    }

    /// Checks if the consuming end of the application-internal stream is
    /// closed.
    ///
    /// Due to technical limitations, this assumes that the stream is open
    /// while [`is_running()`] returns `true`; the stream handler task is
    /// expected to automatically shut down when the consumer is dropped.
    pub fn is_closed(&self) -> bool {
        self.sample_tx.as_ref().is_some_and(|tx| tx.is_closed())
    }

    /// Corks or uncorks the active stream. If there is no active stream, this
    /// returns `Err(Code::NoEntity)`.
    ///
    /// If `cork` is `true`, the stream will be corked; if `cork` is `false`,
    /// the stream will be uncorked.
    pub async fn set_cork(&mut self, cork: bool) -> Result<(), Code> {
        if let Some((cmd_tx, _)) = self.task.as_mut() {
            let (tx, rx) = oneshot::channel::<Result<(), Code>>();
            
            cmd_tx.push(StreamCommand::SetCork(
                cork,
                tx
            )).await.map_err(|_| Code::Killed)?;
            rx.await.map_err(|_| Code::Killed)?
        } else {
            Err(Code::NoEntity)
        }
    }

    /// Monitors the state/health of the stream handler task. The owner of the
    /// `StreamManager` should call this function while [`is_running()`]
    /// returns true.
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

    /// Starts recording audio into the application-internal stream from the
    /// given `source` at the given normalized `volume`.
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

            let (cmd_tx, cmd_rx) = 
                AsyncRb::<StreamCommand, HeapRb<StreamCommand>>::new(
                    COMMAND_QUEUE_SIZE
                ).split();
            let task_handle = self.ctx.spawn(do_stream(
                stream,
                sample_tx,
                cmd_rx
            )).await;

            self.task = Some((cmd_tx, task_handle));
            Ok(())
        } else {
            Err(Code::Exist)
        }
    }

    /// Stops any current audio recording. If [`is_running()`] is false, this
    /// does nothing.
    pub async fn stop(&mut self) {
        if let Some((mut cmd_tx, task_handle)) = self.task.take() {
            if cmd_tx.push(StreamCommand::Shutdown).await.is_err() && !task_handle.is_finished() {
                warn!(
                    "PulseAudio stream handler ignored shutdown signal. \
                     Forcibly aborting task"
                );
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

impl<'a> SinkId<'a> {
    /// Disambiguates between a sink index and a sink name, returning an
    /// optional `SinkId` object accordingly.
    pub fn merged(index: Option<u32>, name: Option<&'a str>) -> Option<Self> {
        if let Some(index) = index {
            Some(Self::Index(index))
        } else if let Some(name) = name {
            Some(Self::Name(name))
        } else {
            None
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

    /// Calls the internal [`resolve()`] implementation if `Self` is `Some`,
    /// otherwise, looks up and returns the system default sink.
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
impl<'a> IntoStreamConfig for SinkId<'a> {
    type Target = OwnedSinkInfo;

    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code> {
        match self {
            SinkId::Index(index) => IntoStreamConfig::resolve(
                index,
                introspect
            ).await,
            SinkId::Name(name) => IntoStreamConfig::resolve(
                name,
                introspect
            ).await
        }
    }
}

#[async_trait]
impl IntoStreamConfig for OwnedSinkInputInfo {
    type Target = ResolvedSinkInput;

    async fn resolve(
        self,
        introspect: &AsyncIntrospector
    ) -> Result<Self::Target, Code> {
        let sink_idx = self.sink;

        Ok(ResolvedSinkInput(
            self,
            introspect.get_sink_by_index(sink_idx).await?
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

impl StreamConfig for OwnedSourceInfo {
    fn configure(&self, mut stream: Stream) -> Result<Stream, Code> {
        if let Some(name) = self.name.as_ref() {
            stream.connect_record(
                Some(&name),
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

impl Display for SinkId<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            Self::Index(idx) => write!(f, "sink index {}", idx),
            Self::Name(name) => name.fmt(f),
        }
    }
}

impl Drop for StreamManager {
    fn drop(&mut self) {
        debug!("PulseAudio stream handler shutting down");

        if let Some((mut cmd_tx, task_handle)) = self.task.take() {
            warn!("PulseAudio stream manager shutting down while stream handler active");

            if !cmd_tx.is_closed() {
                if cmd_tx.as_mut_base().push(StreamCommand::Shutdown).is_err() {
                    warn!(
                        "Command queue for PulseAudio stream handler is full. \
                         Forcibly aborting task."
                    );
                    task_handle.abort();
                }
            } else {
                info!(
                    "PulseAudio stream handler task unexpectedly shut down \
                     before manager"
                );
            }
        }

        debug!("PulseAudio stream handler shut down");
    }
}

// HELPER FUNCTIONS ************************************************************
/// Core implementation for recording audio from a `Stream`.
///
/// Data will be read into `sample_tx`.
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

/// Main event loop for the stream handler task.
///
/// This monitors both the `cmd_rx` channel for commands, and the stream object
/// for new data.
async fn do_stream(
    mut stream: Stream,
    mut sample_tx: Lease<Producer<u8>>,
    mut cmd_rx: Consumer<StreamCommand>
) {
    let notify = StreamReadNotifier::new(&mut stream);
    let mut err_count = 0usize;

    while !sample_tx.is_closed() {
        tokio::select! {
            biased;
            cmd = cmd_rx.pop() => match cmd {
                Some(StreamCommand::SetCork(cork, tx)) => {
                    let cork_state = stream.is_corked()
                        .map_err(|e| Code::try_from(e).unwrap_or(Code::Unknown));

                    let ret_result = tx.send(match cork_state {
                        Ok(cork_state) => if cork != cork_state {
                            let op_result = if cork {
                                async_polyfill::operation_to_future(
                                    false,
                                    |result| {
                                        stream.cork(Some(Box::new(
                                            move |success| result.store(success)
                                        )))
                                    }
                                ).await
                            } else {
                                async_polyfill::operation_to_future(
                                    false,
                                    |result| {
                                        stream.uncork(Some(Box::new(
                                            move |success| result.store(success)
                                        )))
                                    }
                                ).await
                            };

                            match op_result {
                                Ok(Ok(res)) => if res {
                                    Ok(())
                                } else {
                                    Err(Code::Unknown)
                                },
                                Ok(Err(_)) | Err(_) => Err(Code::Killed),
                            }
                        } else {
                            Ok(())
                        },
                        Err(e) => Err(e),
                    });

                    if ret_result.is_err() {
                        warn!(
                            "Could not transmit cork operation result back to \
                             stream manager"
                        );
                    }
                },
                Some(StreamCommand::Shutdown) | None => {
                    info!("Received command to shut down PulseAudio stream");
                    break;
                }
            },
            _ = notify.await_data() => match stream_read(
                &mut stream,
                &mut sample_tx
            ).await {
                StreamWriteResult::HandleClosed => {
                    info!(
                        "PulseAudio stream receiver closed, shutting down \
                         stream handler"
                    );
                    break;
                },
                StreamWriteResult::PulseError(e) => {
                    error!("Error reading from stream: {}", e);
                    err_count += 1;

                    if err_count >= MAX_CONSECUTIVE_ERRORS {
                        error!(
                            "Stream handler encountered {} consecutive errors, \
                             shutting down",
                            e
                        );

                        break;
                    }
                },
                StreamWriteResult::DataWritten | StreamWriteResult::NoOp => {
                    err_count = 0;
                }
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

/// Helper function to disconnect a `Stream`. Logs a warning if the stream fails
/// to disconnect.
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

    let stream_fut = ctx_wrap.with_ctx(move |ctx| {
        create_and_connect_stream(
            ctx,
            source
        ).map(|fut| task::spawn_local(fut))
    }).await?;

    stream_fut.await
        .map_err(|_| Code::Killed)?
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

    let stream = Stream::new(
        ctx,
        "capture stream",
        &SAMPLE_SPEC,
        Some(&channel_map)
    ).expect("Failed to open record stream");

    Ok(InitializingFuture::from(config.configure(stream)?))
}
