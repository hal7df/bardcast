///! Abstractions around the core libpulse API and PulseAudio server connection.

mod async_polyfill;
pub mod collect;
mod introspect;

extern crate libpulse_binding as libpulse;

use std::cmp;
use std::future::Future;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_ringbuf::{AsyncConsumer, AsyncProducer, AsyncRb};
use clap::crate_name;
use itertools::Itertools;
use log::{debug, error, warn, trace};
use ringbuf::HeapRb;
use tokio::sync::mpsc::{
    self,
    Sender as MpscSender,
    Receiver as MpscReceiver
};
use tokio::sync::oneshot::{
    self,
    Receiver as OneshotReceiver,
    Sender as OneshotSender
};
use tokio::task::{self, JoinError, JoinHandle};
use tokio::time;

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

/// The default maximum interval between mainloop iterations.
const DEFAULT_MAINLOOP_MAX_INTERVAL: Duration = Duration::from_millis(20);

/// The number of iterations over which to compute the delay for the next
/// mainloop iteration.
const MAINLOOP_ITER_TRACKING_LEN: usize = 5;

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

/// Utility type for managing the mainloop.
///
/// Simply using [`tokio::task::yield_now()`] to inject an await into the
/// mainloop causes the mainloop to poll far more often than necessary.
/// `MainloopHandler` uses the number of dispatched events on each iteration to
/// guess when the next iteration _should_ be, and sleeps the mainloop
/// accordingly.
struct MainloopHandler {
    mainloop: Mainloop,
    iter_lengths: [(Instant, u32); MAINLOOP_ITER_TRACKING_LEN],
    last_iter: Instant,
    max_interval: Duration,
}

/// A handle to cancel an actively running [`MainloopHandler`].
struct MainloopHandle(OneshotSender<()>, JoinHandle<()>);

// TYPE IMPLS ******************************************************************
impl PulseContextWrapper {
    /// Creates a new wrapper instance, returning the new instance wrapped in a
    /// [`ValueJoinHandle`] alongside the handle for the background task for
    /// tracking.
    pub async fn new(
        server: Option<&str>,
        max_mainloop_interval_usec: Option<u64>
    ) -> Result<ValueJoinHandle<Self>, Code> {
        let (op_queue_tx, op_queue_rx) = mpsc::channel::<ContextThunk>(
            OP_QUEUE_SIZE
        );

        Ok(ValueJoinHandle::new(
            Self { op_queue: op_queue_tx },
            start_context_handler(
                server,
                max_mainloop_interval_usec,
                op_queue_rx
            ).await?
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
    ) -> Result<ValueJoinHandle<SampleConsumer>, Code> {
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
                            warn!("Failure to discard stream sample: {}", e);
                        }
                    },
                    Ok(PeekResult::Hole(hole_size)) => {
                        warn!("PulseAudio stream returned hole of size {} bytes", hole_size);
                        if let Err(e) = stream.discard() {
                            warn!( "Failure to discard stream sample: {}", e);
                        }
                    },
                    Ok(PeekResult::Empty) => {
                        trace!("PulseAudio stream had no data");
                    },
                    Err(e) => error!(
                        "Failed to read audio from PulseAudio: {}",
                        e
                    ),
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
    ) -> Result<Stream, Code> {
        let (tx, rx) = oneshot::channel();

        self.with_ctx(move |ctx| {
            if tx.send(create_and_connect_stream(ctx, &source_name)).is_err() {
                panic!("Stream receiver unexpectedly terminated");
            }
        }).await;

        // Note: stream_fut makes PulseAudio API calls directly, so we need
        // to await it in a local task.
        let stream_fut = rx.await.map_err(|_| Code::NoData)??;
        task::spawn_local(stream_fut).await
            .map_err(|_| Code::Killed)?
            .map_err(|state| if state == StreamState::Terminated {
                Code::Killed
            } else {
                Code::BadState
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

impl MainloopHandler {
    /// Creates a new handler over the given mainloop, where each iteration
    /// lasts no longer than `max_interval`.
    fn new(mainloop: Mainloop, max_interval: Duration) -> Self {
        let now = Instant::now();

        Self {
            mainloop,
            iter_lengths: [(now, 0); MAINLOOP_ITER_TRACKING_LEN],
            last_iter: now,
            max_interval,
        }
    }

    /// Saves the number of processed events, if that number is nonzero. Also
    /// updates the last recorded iteration time as `iter_time`.
    fn save_processed_events(
        &mut self,
        iter_time: Instant,
        processed_events: u32
    ) {
        let mut new_iteration = (iter_time, processed_events);

        for iteration in self.iter_lengths.iter_mut() {
            if new_iteration.1 == 0 {
                break;
            }

            new_iteration = mem::replace(iteration, new_iteration);
        }

        self.last_iter = iter_time;
    }

    /// Computes the next time the mainloop should be woken to iterate. Returns
    /// `None` if it could not be comptued.
    fn compute_next_iteration_instant(&self) -> Option<Instant> {
        let mut min_duration: Option<Duration> = None;

        for (event1, event2) in self.iter_lengths.iter().tuple_windows() {
            if event1.1 == 0 || event2.1 == 0 {
                break;
            }

            let eff_duration = event1.0.duration_since(event2.0) / event1.1;

            if min_duration.is_none() || eff_duration < min_duration.unwrap() {
                min_duration = Some(eff_duration);
            }
        }

        min_duration.map(|duration| {
            let final_duration = cmp::min(duration, self.max_interval);

            trace!("Next iteration interval: {}us", final_duration.as_micros());

            self.last_iter.checked_add(final_duration)
                .expect("Next iteration instant cannot be computed")
        })
    }

    /// Waits for the timeout to the next iteration to expire, or for the
    /// mainloop to receive a shutdown signal. Returns false if a shutdown
    /// signal was received.
    async fn await_next_iteration(
        &self,
        shutdown_rx: &mut OneshotReceiver<()>
    ) -> bool {
        if let Some(instant) = self.compute_next_iteration_instant() {
            self.await_next_iteration_impl(shutdown_rx, time::sleep_until(instant.into())).await
        } else {
            self.await_next_iteration_impl(shutdown_rx, task::yield_now()).await
        }
    }

    /// Waits for the future to complete, or for the mainloop to receive a
    /// shutdown singla. Returns false if a shutdown signal was received.
    async fn await_next_iteration_impl<F: Future<Output = ()>>(
        &self,
        shutdown_rx: &mut OneshotReceiver<()>,
        next_iter_fut: F
    ) -> bool {
        tokio::select! {
            _ = next_iter_fut => true,
            _ = shutdown_rx => false,
        }
    }

    /// Performs a single iteration of the mainloop.
    fn iterate(&mut self) -> Result<(), IterateResult> {
        let iter_time = Instant::now();

        match self.mainloop.iterate(false) {
            IterateResult::Success(n_evts) => {
                trace!("Mainloop iteration success, dispatched {} events", n_evts);
                self.save_processed_events(iter_time, n_evts);
                Ok(())
            },
            e => Err(e),
        }
    }

    /// Entry point for executing the mainloop.
    async fn mainloop(&mut self, mut shutdown_rx: OneshotReceiver<()>) {
        debug!("Mainloop started");

        while self.await_next_iteration(&mut shutdown_rx).await {
            match self.iterate() {
                Err(IterateResult::Quit(_)) => {
                    warn!("Mainloop quit unexpectedly");
                    break;
                },
                Err(IterateResult::Err(e)) => {
                    error!("Pulse mainloop encountered error: {}", e);
                },
                _ => (),
            }
        }

        debug!("Mainloop exiting");
    }

    /// Launches a dedicated task to execute the mainloop.
    fn start(mut self) -> MainloopHandle {
        let (tx, rx) = oneshot::channel();

        MainloopHandle(tx, task::spawn_local(async move {
            self.mainloop(rx).await;
        }))
    }
}

impl MainloopHandle {
    /// Awaits a potential termination of the mainloop task without consuming
    /// the handle, to allow cancellations and reuse.
    async fn await_termination(&mut self) -> Result<(), JoinError> {
        (&mut self.1).await
    }

    /// Sends a shutdown signal to the mainloop task and waits for it to
    /// terminate.
    async fn stop(self) {
        if self.0.send(()).is_err() {
            warn!("Failed to terminated mainloop, it may already have exited");
        }

        if !self.1.is_finished() {
            if let Err(e) = self.1.await {
                if e.is_panic() {
                    warn!("Mainloop task panicked upon shutdown");
                }
            }
        }
    }
}

// HELPER FUNCTIONS ************************************************************

/// Initializes a PulseAudio [`Context`] against the server at the given path
/// (if present, otherwise autodetects a running server) and starts a task to
/// iterate the corresponding [`Mainloop`]. The [`Context`] is returned as an
/// [`InitializingFuture`] that resolves once the context successfully connects
/// to the server.
fn pulse_init(
    server: Option<&str>,
    max_mainloop_interval_usec: Option<u64>
) -> Result<(MainloopHandle, InitializingFuture<ContextState, Context>), Code> {
    let mainloop = Mainloop::new().ok_or(Code::BadState)?;
    let mut ctx = Context::new(
        &mainloop,
        crate_name!(),
    ).ok_or(Code::BadState)?;

    //Initiate a connection with a running PulseAudio daemon
    debug!("Connecting to PulseAudio server");

    //We don't want to bubble failures up the stack yet, since the mainloop
    //is still locked
    ctx.connect(
        server,
        FlagSet::NOAUTOSPAWN,
        None
    ).map_err(|err| Code::try_from(err).unwrap_or(Code::Unknown))?;

    //Set up a Future that resolves with the state of the context so
    //dependents can wait until it's ready
    let ctx_fut = InitializingFuture::from(ctx);

    //Start the mainloop thread and return
    Ok((
        MainloopHandler::new(
            mainloop,
            max_mainloop_interval_usec.map_or(
                DEFAULT_MAINLOOP_MAX_INTERVAL,
                |max_interval| Duration::from_micros(max_interval)
            )
        ).start(),
        ctx_fut
    ))
}

/// Initializes a PulseAudio [`Context`] and spawns a local task to receive and
/// execute context operations from the given `op_queue`.
async fn start_context_handler(
    server: Option<&str>,
    max_mainloop_interval_usec: Option<u64>,
    mut op_queue: MpscReceiver<ContextThunk>
) -> Result<JoinHandle<()>, Code> {
    let (mut mainloop_handle, ctx_fut) = pulse_init(
        server,
        max_mainloop_interval_usec
    )?;

    match ctx_fut.await {
        Ok(mut ctx) => {
            debug!("PulseAudio context ready");

            Ok(task::spawn_local(async move {
                loop {
                    trace!("Context handler awaiting next job");
                    tokio::select! {
                        biased;
                        _ = mainloop_handle.await_termination() => panic!("Mainloop terminated unexpectedly"),
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
                mainloop_handle.stop().await;
                debug!("Context handler shut down");
            }))
        },
        Err(state) => {
            error!("PulseAudio context failed to connect");

            mainloop_handle.stop().await;
            Err(if state == ContextState::Terminated {
                Code::ConnectionTerminated
            } else {
                Code::Unknown
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
) -> Result<InitializingFuture<StreamState, Stream>, Code> {
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
        StreamFlagSet::START_UNMUTED
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
