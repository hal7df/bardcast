///! Abstractions around the core libpulse API and PulseAudio server connection.

mod async_polyfill;
pub mod collect;
mod introspect;

extern crate libpulse_binding as libpulse;

use std::mem;
use std::sync::{Arc, Mutex};

use async_ringbuf::{AsyncConsumer, AsyncProducer, AsyncRb};
use clap::crate_name;
use log::{debug, error, warn, trace};
use ringbuf::HeapRb;
use tokio::sync::mpsc::{self, Sender as MpscSender, Receiver};
use tokio::sync::oneshot::{self, Sender as OneshotSender};
use tokio::task::{self, JoinHandle};

use libpulse::channelmap::Map as ChannelMap;
use libpulse::context::{Context, FlagSet, State as ContextState};
use libpulse::error::Code;
use libpulse::mainloop::standard::{IterateResult, Mainloop};
use libpulse::operation::{Operation, State as OperationState};
use libpulse::sample::{Format as SampleFormat, Spec as SampleSpec};
use libpulse::stream::{
    FlagSet as StreamFlagSet,
    PeekResult,
    State as StreamState,
    Stream
};

use self::async_polyfill::{
    InitializingFuture,
    PulseFutureInner,
    StreamReadNotifier
};
use super::PulseFailure;
use crate::util;
use crate::util::task::ValueJoinHandle;

// RE-EXPORTS ******************************************************************
pub use self::introspect::AsyncIntrospector;
pub use async_polyfill::{
    PulseFuture,
    PulseResultRef
};

// MODULE-INTERNAL CONSTANTS ***************************************************

/// The maximum number of context operations that can be queued at once.
const OP_QUEUE_SIZE: usize = 32;

/// The audio sample rate in Hz. Per the driver specification, this must be
/// 48 kHz.
const SAMPLE_RATE: u32 = 48000;

/// The size of the sample buffer in bytes.
const SAMPLE_QUEUE_SIZE: usize = SAMPLE_RATE as usize;

/// PulseAudio [`SampleSpec`] definition matching the requirements of the driver
/// specification.
const SAMPLE_SPEC: SampleSpec = SampleSpec {
    format: SampleFormat::S16le,
    channels: 2,
    rate: SAMPLE_RATE,
};

// TYPE DEFINITIONS ************************************************************

/// Type alias for the concrete audio consumer type.
pub type SampleConsumer = AsyncConsumer<u8, Arc<AsyncRb<u8, HeapRb<u8>>>>;

/// Type alias for submitted context operations.
type ContextThunk = Box<dyn FnOnce(&mut Context) + Send + 'static>;

/// Wrapper interface around a libpulse [`Context`] object.
///
/// To work around libpulse's restrictions on concurrent API calls, this creates
/// a thread-locked task in the background that receives and executes arbitary
/// functions against the context serially, allowing other tasks to interact
/// with PulseAudio from multiple threads.
#[derive(Clone)]
pub struct PulseContextWrapper {
    op_queue: MpscSender<ContextThunk>,
}

// TYPE IMPLS ******************************************************************
impl PulseContextWrapper {
    /// Creates a new wrapper instance, returning the new instance wrapped in a
    /// [`ValueJoinHandle`] alongside the handle for the background task for
    /// tracking.
    pub async fn new(
        server: Option<&str>
    ) -> Result<ValueJoinHandle<Self>, PulseFailure> {
        let (op_queue_tx, op_queue_rx) = mpsc::channel::<ContextThunk>(
            OP_QUEUE_SIZE
        );

        Ok(ValueJoinHandle::new(
            Self { op_queue: op_queue_tx },
            start_context_handler(server, op_queue_rx).await?
        ))
    }

    /// Submits the given context operation to the background task.
    ///
    /// This function does not provide a return channel. If a result is needed
    /// from the task, either use [`PulseContextWrapper::do_ctx_op`] and its
    /// cohort, or manually implement the return channel in the submitted
    /// operation.
    pub async fn with_ctx<F: FnOnce(&mut Context) + Send + 'static>(&self, op: F) {
        if self.op_queue.send(Box::new(op)).await.is_err() {
            panic!("PulseAudio context handler task unexpectedly terminated");
        }
    }

    /// Opens a recording stream against the audio source with the given name,
    /// at the requested volume, if any. Returns a consuming handle for the
    /// stream and the underlying task performing the raw stream handling in a
    /// [`ValueJoinHandle`].
    pub async fn open_rec_stream(
        &self,
        source_name: impl ToString,
        volume: Option<f64>
    ) -> Result<ValueJoinHandle<SampleConsumer>, PulseFailure> {
        let mut stream = self.get_connected_stream(source_name.to_string()).await?;
        let (mut sample_tx, sample_rx) = AsyncRb::<u8, HeapRb<u8>>::new(SAMPLE_QUEUE_SIZE).split();

        if let Some(volume) = volume {
            AsyncIntrospector::from(self).set_source_output_volume(
                stream.get_index().unwrap(),
                volume
            ).await?
        }

        //Create a handle to the context queue, to force the mainloop to stay
        //open while we're still processing samples
        let self_handle = self.clone();

        Ok(ValueJoinHandle::new(sample_rx, task::spawn_local(async move {
            let notify = StreamReadNotifier::new(&mut stream);

            while should_continue_stream_read(&sample_tx, &notify).await {
                match stream.peek() {
                    Ok(PeekResult::Data(samples)) => {
                        if sample_tx.push_iter(samples.iter().map(|x| *x)).await.is_err() {
                            //If this happens, we can break the loop immediately,
                            //since we know the receiver dropped.
                            debug!("Sample receiver dropped while transmitting samples");
                            break;
                        }
                        if let Err(e) = stream.discard() {
                            warn!(
                                "Failure to discard stream sample: {:?}",
                                Code::try_from(e).unwrap_or(Code::Unknown)
                            );
                        }
                    },
                    Ok(PeekResult::Hole(hole_size)) => {
                        warn!("PulseAudio stream returned hole of size {} bytes", hole_size);
                        if let Err(e) = stream.discard() {
                            warn!(
                                "Failure to discard stream sample: {:?}",
                                Code::try_from(e).unwrap_or(Code::Unknown)
                            );
                        }
                    },
                    Ok(PeekResult::Empty) => {
                        trace!("PulseAudio stream had no data");
                    },
                    Err(e) => {
                        if let Some(msg) = e.to_string() {
                            error!("Failed to read audio from PulseAudio: {}", msg);
                        } else {
                            error!("Failed to read audio from PulseAudio (code {})", e.0);
                        }
                    }
                }
            }

            debug!("Stream handler shutting down");

            //Tear down the record stream
            if let Err(e) = stream.disconnect() {
                if let Some(msg) = e.to_string() {
                    warn!("Record stream failed to disconnect: {}", msg);
                } else {
                    warn!("Record stream failed to disconnect (code {})", e.0);
                }
            }
            mem::drop(self_handle);

            debug!("Stream handler shut down");
        })))
    }

    /// Creates and returns a new recording stream against the source with the
    /// given name.
    async fn get_connected_stream(
        &self,
        source_name: String
    ) -> Result<Stream, PulseFailure> {
        let (tx, rx) = oneshot::channel();

        self.with_ctx(move |ctx| {
            if tx.send(create_and_connect_stream(ctx, &source_name)).is_err() {
                panic!("Stream receiver unexpectedly terminated");
            }
        }).await;

        // Note: stream_fut makes PulseAudio API calls directly, so we need
        // to await it in a local task.
        let stream_fut = rx.await.map_err(|_| PulseFailure::from(Code::BadState))??;
        task::spawn_local(stream_fut).await
            .map_err(|_| PulseFailure::from(Code::Unknown))?
            .map_err(|state| match state {
                StreamState::Terminated => PulseFailure::Terminated(0),
                _ => PulseFailure::from(Code::BadState),
            })
    }

    /// Core implementation of the [`PulseWrapper::do_ctx_op`] family of
    /// functions.
    async fn do_ctx_op_impl<T, S, F>(
        &self,
        fut_inner: Arc<Mutex<PulseFutureInner<T>>>,
        op: F
    ) -> Result<T, OperationState>
    where
        T: Default + Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(&mut Context, PulseResultRef<T>) -> Operation<S> + Send + 'static {
        let fut = PulseFuture::from(&fut_inner);

        self.with_ctx(move |ctx| {
            let op = Arc::new(Mutex::new(Some(PulseFutureInner::apply(&fut_inner, ctx, op))));
            let mut op_ref = Arc::clone(&op);

            op.lock().unwrap().as_mut().unwrap().set_state_callback(Some(Box::new(move || {
                let mut fut_inner = fut_inner.lock().unwrap();

                fut_inner.set_state(if let Some(op) = &*op_ref.lock().unwrap() {
                    op.get_state()
                } else {
                    warn!("Operation object dropped during state callback execution");
                    OperationState::Cancelled
                });
                fut_inner.wake();

                //If the operation completed, drop the self reference
                if fut_inner.terminated() {
                    mem::drop(mem::take(&mut op_ref));
                }
            })));
        }).await;

        fut.await
    }

    /// Executes the given function `op`, and waits for the returned
    /// [`Operation`] to terminate before returning a user-defined value that
    /// implements [`Default`].
    pub async fn do_ctx_op_default<T, S, F>(&self, op: F) -> Result<T, OperationState>
    where
        T: Default + Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(&mut Context, PulseResultRef<T>) -> Operation<S> + Send + 'static {
        self.do_ctx_op_impl(PulseFutureInner::new(), op).await
    }

    /// Executes the given function `op`, and waits for the returned
    /// [`Operation`] to terminate before returning a user-defined value, or
    /// `None` if the operation completed without setting a value.
    pub async fn do_ctx_op<T, S, F>(&self, op: F) -> Result<Option<T>, OperationState>
    where
        T: Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(&mut Context, PulseResultRef<Option<T>>) -> Operation<S> + Send + 'static {
        self.do_ctx_op_default(op).await
    }

    /// Executes the given function `op`, and waits for the returned
    /// [`Operation`] to terminate. The result of the operation (which is
    /// expected to return a list of values) is collected into a `Vec` and
    /// returned. If no values are returned by the inner API call, an empty
    /// `Vec` is returned.
    pub async fn do_ctx_op_list<T, S, F>(&self, op: F) -> Result<Vec<T>, OperationState>
    where 
        T: Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(&mut Context, PulseResultRef<Vec<T>>) -> Operation<S> + Send + 'static {
        self.do_ctx_op_default(op).await
    }
}

/// Starts a local task that iterates the libpulse [`Mainloop`] object,
/// returning the task's handle and a sender handle that can be used to shut
/// down the mainloop.
fn start_mainloop(mut mainloop: Mainloop) -> ValueJoinHandle<OneshotSender<()>> {
    let (tx, mut rx) = oneshot::channel();

    ValueJoinHandle::new(tx, task::spawn_local(async move {
        debug!("Mainloop started");

        while util::check_oneshot_rx(&mut rx) {
            match mainloop.iterate(false) {
                IterateResult::Success(n_evts) => trace!("Mainloop iteration success, dispatched {} events", n_evts),
                IterateResult::Quit(_) => {
                    warn!("Mainloop quit unexpectedly");
                    break;
                },
                IterateResult::Err(e) => {
                    if let Some(msg) = e.to_string() {
                        error!("Pulse mainloop encountered error: {}", msg);
                    } else {
                        error!("Pulse mainloop encountered unknown error (code {})", e.0);
                    }
                },
            }

            task::yield_now().await;
        }

        debug!("Mainloop exiting");
    }))
}

/// Uses the provided `mainloop_tx` handle to signal a shutdown event to the
/// task spawned by [`start_mainloop`] and waits for it to terminate.
async fn stop_mainloop(
    mainloop_tx: OneshotSender<()>,
    mainloop_handle: JoinHandle<()>
) {
    if mainloop_tx.send(()).is_err() {
        warn!("Failed to terminate mainloop, it may already have exited");
    }

    if !mainloop_handle.is_finished() {
        if let Err(e) = mainloop_handle.await {
            if e.is_panic() {
                warn!("Mainloop task panicked upon shutdown");
            }
        }
    }
}

/// Initializes a PulseAudio [`Context`] against the server at the given path
/// (if present, otherwise autodetects a running server) and starts a task to
/// iterate the corresponding [`Mainloop`]. The [`Context`] is returned as an
/// [`InitializingFuture`] that resolves once the context successfully connects
/// to the server.
fn pulse_init(
    server: Option<&str>
) -> Result<(ValueJoinHandle<OneshotSender<()>>, InitializingFuture<ContextState, Context>), PulseFailure> {
    let mainloop = Mainloop::new().ok_or(PulseFailure::Error(Code::BadState))?;
    let mut ctx = Context::new(
        &mainloop,
        crate_name!(),
    ).ok_or(PulseFailure::Error(Code::BadState))?;

    //Initiate a connection with a running PulseAudio daemon
    debug!("Connecting to PulseAudio server");

    //We don't want to bubble failures up the stack yet, since the mainloop
    //is still locked
    ctx.connect(
        server,
        FlagSet::NOAUTOSPAWN,
        None
    ).map_err(|err| {
        PulseFailure::Error(Code::try_from(err).unwrap_or(Code::Unknown))
    })?;

    //Set up a Future that resolves with the state of the context so
    //dependents can wait until it's ready
    let ctx_fut = InitializingFuture::from(ctx);

    //Start the mainloop thread and return
    Ok((start_mainloop(mainloop), ctx_fut))
}

/// Initializes a PulseAudio [`Context`] and spawns a local task to receive and
/// execute context operations from the given `op_queue`.
async fn start_context_handler(
    server: Option<&str>,
    mut op_queue: Receiver<ContextThunk>
) -> Result<JoinHandle<()>, PulseFailure> {
    let (mainloop_handle, ctx_fut) = pulse_init(server)?;
    let (mainloop_tx, mut mainloop_handle) = mainloop_handle.into_tuple();

    match ctx_fut.await {
        Ok(mut ctx) => {
            debug!("PulseAudio context ready");

            Ok(task::spawn_local(async move {
                loop {
                    trace!("Context handler awaiting next job");
                    tokio::select! {
                        biased;
                        _ = &mut mainloop_handle => panic!("Mainloop terminated unexpectedly"),
                        op = op_queue.recv() => if let Some(op) = op {
                            op(&mut ctx);
                        } else {
                            debug!("Task queue closed, terminating context handler");
                            break;
                        }
                    }
                }

                debug!("Context handler shutting down");
                ctx.disconnect();
                stop_mainloop(mainloop_tx, mainloop_handle).await;
                debug!("Context handler shut down");
            }))
        },
        Err(state) => {
            error!("PulseAudio context failed to connect");

            stop_mainloop(mainloop_tx, mainloop_handle).await;
            Err(match state {
                ContextState::Failed => PulseFailure::from(Code::Unknown),
                ContextState::Terminated => PulseFailure::Terminated(1),
                _ => PulseFailure::from(Code::Unknown),
            })
        }
    }
}

/// Creates a recording stream against the named audio source, returning it
/// as an [`InitializingFuture`] that resolves once the stream enters the ready
/// state.
fn create_and_connect_stream(
    ctx: &mut Context,
    source_name: impl AsRef<str>
) -> Result<InitializingFuture<StreamState, Stream>, PulseFailure> {
    //Technically init_stereo should create a new channel map with the format
    //specifier "front-left,front-right", but due to oddities in the Rust
    //binding for PulseAudio, we have to manually specify the format first. We
    //call init_stereo anyways to make sure we are always using the server's
    //understanding of stereo audio.
    let mut channel_map = ChannelMap::new_from_string("front-left,front-right")
        .map_err(|_| PulseFailure::from(Code::Invalid))?;
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
        StreamFlagSet::START_UNMUTED
    )?;

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
