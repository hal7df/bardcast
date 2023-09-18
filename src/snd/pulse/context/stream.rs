///! PulseAudio stream handling.

extern crate libpulse_binding as libpulse;

use std::cmp;
use std::iter;
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_ringbuf::{AsyncConsumer, AsyncProducer, AsyncRb};
use log::{debug, error, trace, warn};
use ringbuf::HeapRb;
use tokio::sync::oneshot;
use tokio::task;

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

use crate::util::task::ValueJoinHandle;
use super::super::owned::OwnedSinkInputInfo;
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

/// The size of a stereo IEEE 754 32-bit floating point audio sample.
const F32LE_STEREO_SAMPLE_SIZE: usize = mem::size_of::<f32>() * 2;

/// The maximum number of stereo IEEE 754 32-bit floating point audio samples
/// that can be counted by a usize.
const MAX_F32LE_STEREO_SAMPLES: usize = usize::MAX / F32LE_STEREO_SAMPLE_SIZE;

/// The number of microseconds per second (1000000)
const MICROS_PER_SECOND: u128 = Duration::from_secs(1).as_micros();

const MAX_CONSECUTIVE_ERRORS: u8 = 3;

// TYPE DEFINITIONS ************************************************************
struct StreamHandler {
    ctx: PulseContextWrapper,
    sample_tx: Producer<u8>,
    stream: Option<(Stream, StreamReadNotifier)>,
    last_write: Instant,
}

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
impl StreamHandler {
    async fn new(ctx: PulseContextWrapper, sample_tx: Producer<u8>) -> Self {
        let mut handler = Self {
            ctx,
            sample_tx,
            stream: None,
            last_write: Instant::now()
        };

        //Inject a single sample of silence into the stream, to synchronize the
        //downstream reader with the handler loop, once it starts
        handler.write_silence(1).await
            .expect("Sample consumer terminated unexpectedly");

        handler
    }

    fn from_stream(
        ctx: PulseContextWrapper,
        sample_tx: Producer<u8>,
        mut stream: Stream
    ) -> Self {
        let notify = StreamReadNotifier::new(&mut stream);

        Self {
            ctx,
            sample_tx,
            stream: Some((stream, notify)),
            last_write: Instant::now()
        }
    }

    async fn start(mut self) {
        let mut consecutive_err_count = 0u8;

        while !self.sample_tx.is_closed() {
            self.await_next_write().await;
            
            if let StreamWriteResult::PulseError(_) = self.write_next_chunk().await {
                consecutive_err_count += 1;

                if consecutive_err_count > MAX_CONSECUTIVE_ERRORS {
                    let msg = format!(
                        "PulseAudio stream handler encountered {} consecutive errors",
                        consecutive_err_count
                    );
                    error!("{}, terminating", msg);
                    panic!("{}", msg);
                }
            } else {
                consecutive_err_count = 0u8;
            }
        }
    }

    async fn write_next_chunk(&mut self) -> StreamWriteResult {
        if let Some((stream, _)) = self.stream.as_mut() {
            match stream.peek() {
                Ok(PeekResult::Data(samples)) => {
                    self.last_write = Instant::now();
                    let mut result = StreamWriteResult::DataWritten;

                    if self.sample_tx.push_slice(samples).await.is_err() {
                        debug!(
                            "Sample receiver dropped while transmitting samples"
                        );
                        result = StreamWriteResult::HandleClosed;
                    }
                    if let Err(e) = stream.discard() {
                        warn!("Failure to discard stream sample: {}", e);
                    }

                    result
                },
                Ok(PeekResult::Hole(hole_size)) => {
                    warn!(
                        "PulseAudio stream returned hold of size {} bytes",
                        hole_size
                    );

                    if let Err(e) = stream.discard() {
                        warn!("Failure to discard stream sample: {}", e);
                    }

                    StreamWriteResult::NoOp
                },
                Ok(PeekResult::Empty) => {
                    trace!("PulseAudio stream had no data");
                    StreamWriteResult::NoOp
                },
                Err(e) => {
                    error!("Failed to read audio from PulseAudio: {}", e);
                    StreamWriteResult::PulseError(
                        Code::try_from(e).unwrap_or(Code::Unknown)
                    )
                },
            }
        } else {
            self.write_silence_chunk().await
        }
    }

    async fn write_silence_chunk(&mut self) -> StreamWriteResult {
        let usec_since_last_write = self.update_last_write().as_micros();
        let mut samples_since_last_write = 
            (usec_since_last_write * SAMPLE_RATE as u128) / MICROS_PER_SECOND;

        let mut data_written = false;

        //It's unlikely that this loop will execute more than once per call
        //to write_next_chunk() -- the consumer would have to lag by roughly
        //9 minutes on 32-bit architectures for that to happen -- but we
        //would have to handle the overflow condition anyways, and it's
        //relatively simple to handle gracefully with a loop.
        while samples_since_last_write > 0 {
            let samples_to_write = cmp::min(
                usize::try_from(samples_since_last_write)
                    .unwrap_or(MAX_F32LE_STEREO_SAMPLES),
                MAX_F32LE_STEREO_SAMPLES
            );
            data_written = true;

            if self.write_silence(samples_to_write).await.is_err() {
                return StreamWriteResult::HandleClosed;
            }

            samples_since_last_write =
                samples_since_last_write.saturating_sub(
                    samples_to_write as u128
                );
        }

        if data_written {
            StreamWriteResult::DataWritten
        } else {
            StreamWriteResult::NoOp
        }
    }

    async fn write_silence(&mut self, n_samples: usize) -> Result<(), usize> {
        if let Err(remaining_bytes) = self.sample_tx.push_iter(
            iter::repeat(0u8).take(n_samples * F32LE_STEREO_SAMPLE_SIZE)
        ).await {
            Err(remaining_bytes.count() / F32LE_STEREO_SAMPLE_SIZE)
        } else {
            Ok(())
        }
    }

    async fn await_next_write(&self) {
        if let Some((_, notify)) = self.stream.as_ref() {
            notify.await_data().await;
        } else {
            self.sample_tx.wait_free(self.sample_tx.capacity()).await;
        }
    }

    fn update_last_write(&mut self) -> Duration {
        let duration_since_last_write = self.last_write.elapsed();
        self.last_write = Instant::now();

        duration_since_last_write
    }

    fn close_input(&mut self) {
        if let Some((mut stream, notify)) = self.stream.take() {
            notify.close(&mut stream);

            if let Err(e) = stream.disconnect() {
                warn!("Record stream failed to disconnect: {}", e);
            }
        }
    }
}

// TRAIT IMPLS *****************************************************************
impl Drop for StreamHandler {
    fn drop(&mut self) {
        debug!("PulseAudio stream handler shutting down");

        self.close_input();

        debug!("PulseAudio stream handler shut down");
    }
}

// HELPER FUNCTIONS ************************************************************

/// Opens a recording stream against the audio source with the given name,
/// at the requested volume, if any. Returns a consuming handle for the
/// stream and the underlying task performing the raw stream handling in a
/// [`ValueJoinHandle`].
pub async fn open_simple(
    ctx: &PulseContextWrapper,
    source_name: impl ToString,
    volume: Option<f64>
) -> Result<ValueJoinHandle<SampleConsumer>, Code> {
    let stream = get_connected_stream(ctx, source_name.to_string()).await?;
    let (
        sample_tx,
        sample_rx
    ) = AsyncRb::<u8, HeapRb<u8>>::new(SAMPLE_QUEUE_SIZE).split();

    if let Some(volume) = volume {
        AsyncIntrospector::from(ctx).set_source_output_volume(
            stream.get_index().unwrap(),
            volume
        ).await?
    }

    Ok(ValueJoinHandle::new(
        sample_rx,
        task::spawn_local(do_stream_simple(
            ctx.clone(),
            stream,
            sample_tx,
        )
    )))
}

async fn do_stream_simple(
    ctx: PulseContextWrapper,
    mut stream: Stream,
    mut sample_tx: Producer<u8>
) {
    let notify = StreamReadNotifier::new(&mut stream);

    while should_continue_stream_read(&sample_tx, &notify).await &&
        stream_read(&mut stream, &mut sample_tx).await {}

    debug!("Stream handler shutting down");
    stream_disconnect(&mut stream, notify);

    //We only have a reference to the context to drop it here, in order to
    //prevent the PulseAudio connection from shutting down prematurely
    mem::drop(ctx);

    debug!("Stream handler shut down");
}

async fn stream_read(
    stream: &mut Stream,
    sample_tx: &mut Producer<u8>
) -> bool {
    match stream.peek() {
        Ok(PeekResult::Data(samples)) => {
            if sample_tx.push_iter(samples.iter().copied()).await.is_err() {
                //If this happens, we can break the loop immediately, since we
                //know the receiver dropped.
                debug!("Sample receiver dropped while transmitting samples");
                return false;
            } else if let Err(e) = stream.discard() {
                warn!("Failure to discard stream sample: {}", e);
            }
        },
        Ok(PeekResult::Hole(hole_size)) => {
            warn!(
                "PulseAudio stream returned hole of size {} bytes",
                hole_size
            );
            if let Err(e) = stream.discard() {
                warn!("Failure to discard stream sample: {}", e);
            }
        },
        Ok(PeekResult::Empty) => trace!("PulseAudio stream had no data"),
        Err(e) => error!("Failed to read audio from PulseAudio: {}", e),
    }

    true
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
    source_name: String
) -> Result<Stream, Code> {
    let (tx, rx) = oneshot::channel();

    ctx_wrap.with_ctx(move |ctx| {
        if tx.send(create_and_connect_stream(ctx, &source_name)).is_err() {
            panic!("Stream receiver unexpectedly terminated");
        }
    }).await;

    // Note: stream_fut makes PulseAudio API calls directly, so we need
    // to await it in a local task.
    let stream_fut = rx.await.map_err(|_| Code::NoData)??;
    task::spawn_local(stream_fut).await
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
    source_name: impl AsRef<str>
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

    stream.connect_record(
        Some(source_name.as_ref()),
        None,
        FlagSet::START_UNMUTED
    ).map_err(|pa_err| Code::try_from(pa_err).unwrap_or(Code::Unknown))?;

    Ok(InitializingFuture::from(stream))
}

/// Determines if the stream handler should continue reading samples, or exit.
///
/// This also waits for data to appear in the underlying stream, to prevent
/// needless iterating over a stream with no data.
async fn should_continue_stream_read(
    sample_tx: &AsyncProducer<u8, Arc<AsyncRb<u8, HeapRb<u8>>>>,
    notify: &StreamReadNotifier
) -> bool {
    if sample_tx.is_closed() {
        false
    } else {
        notify.await_data().await;
        true
    }
}
