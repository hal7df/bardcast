///! Abstractions around the core libpulse API and PulseAudio server connection.

mod async_polyfill;
pub mod collect;
mod introspect;
pub mod stream;

extern crate libpulse_binding as libpulse;

use std::cmp;
use std::future::Future;
use std::mem;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use clap::crate_name;
use itertools::Itertools;
use log::{debug, error, warn, trace};
use tokio::sync::mpsc::{
    self,
    Sender as MpscSender,
    Receiver as MpscReceiver
};
use tokio::sync::oneshot::{
    self,
    Receiver as OneshotReceiver
};
use tokio::task::{self, JoinHandle};
use tokio::time;

use libpulse::context::{Context, FlagSet, State as ContextState};
use libpulse::error::Code;
use libpulse::mainloop::standard::{IterateResult, Mainloop};
use libpulse::operation::{Operation, State as OperationState};
use libpulse::sample::{Format as SampleFormat, Spec as SampleSpec};

use self::async_polyfill::InitializingFuture;
use crate::util;
use crate::util::task::{ControlledJoinHandle, ValueJoinHandle};

// RE-EXPORTS ******************************************************************
pub use self::introspect::AsyncIntrospector;

// MODULE-INTERNAL CONSTANTS ***************************************************

/// The maximum number of context operations that can be queued at once.
const OP_QUEUE_SIZE: usize = 32;

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

/// The default maximum interval between mainloop iterations.
const DEFAULT_MAINLOOP_MAX_INTERVAL: Duration = Duration::from_millis(20);

/// The number of iterations over which to compute the delay for the next
/// mainloop iteration.
const MAINLOOP_ITER_TRACKING_LEN: usize = 5;

// TYPE DEFINITIONS ************************************************************

/// Type alias for submitted context operations.
type ContextThunk = Box<dyn FnOnce(&mut Context) + Send + 'static>;

/// Wrapper interface around a libpulse [`Context`] object.
///
/// To work around libpulse's restrictions on concurrent API calls, this creates
/// a thread-locked task in the background that receives and executes arbitary
/// functions against the context serially, allowing other tasks to interact
/// with PulseAudio from multiple threads.
#[derive(Clone)]
pub struct PulseContextWrapper(MpscSender<ContextThunk>);

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

// TYPE IMPLS ******************************************************************
impl PulseContextWrapper {
    /// Creates a new wrapper instance, returning the new instance wrapped in a
    /// [`ValueJoinHandle`] alongside the handle for the background task for
    /// tracking.
    pub async fn new(
        server: Option<&str>,
        max_mainloop_interval_usec: Option<u64>
    ) -> Result<ValueJoinHandle<Self, ()>, Code> {
        let (op_queue_tx, op_queue_rx) = mpsc::channel::<ContextThunk>(
            OP_QUEUE_SIZE
        );

        Ok(ValueJoinHandle::new(
            Self(op_queue_tx),
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
    /// from the task, refer to [`PulseContextWrapper::with_ctx()`] for simple
    /// cases. When a context task that produces an asynchronous [`Operation`],
    /// use [`PulseContextWrapper::do_ctx_op`] and its cohort instead.
    pub async fn submit<F>(&self, op: F)
    where F: FnOnce(&mut Context) + Send + 'static {
        if self.0.send(Box::new(op)).await.is_err() {
            panic!("PulseAudio context handler task unexpectedly terminated");
        }
    }

    /// Runs and awaits the submitted context operation, returning its result.
    ///
    /// If the operation works with context operations that return an
    /// asynchronous [`Operation`] handle, use
    /// [`PulseContextWrapper::do_ctx_op`] and its cohort instead.
    pub async fn with_ctx<F, T>(&self, op: F) -> T
    where F: FnOnce(&mut Context) -> T + Send + 'static {
        let (tx, rx) = oneshot::channel::<T>();

        self.submit(move |ctx| {
            if tx.send(op(ctx)).is_err() {
                panic!("Context task result receiver unexpectedly dropped");
            }
        });

        rx.await.expect("Context task unexpectedly dropped without returning")
    }

    /// Spawns a task to execute the given future on the context thread.
    pub async fn spawn<T>(
        &self,
        fut: impl Future<Output = T> + Send + 'static
    ) -> JoinHandle<T> {
        self.with_ctx(move |_| task::spawn_local(fut)).await
    }

    /// Core implementation of the [`PulseWrapper::do_ctx_op`] family of
    /// functions.
    async fn do_ctx_op_impl<T, S, F>(
        &self,
        initial_value: T,
        op: F
    ) -> Result<T, OperationState>
    where
        T: Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(&mut Context, Weak<Mutex<T>>) -> Operation<S> + Send + 'static {
        let (tx, rx) = oneshot::channel::<Result<T, OperationState>>();

        self.submit(move |ctx| {
            let value = Arc::new(Mutex::new(initial_value));
            let op = Arc::new(Mutex::new(Some(op(ctx, Arc::downgrade(&value)))));

            let mut op_ref = Arc::clone(&op);
            let mut tx = Some(tx);
            let mut result = Some(value);

            op.lock().unwrap().as_mut().unwrap().set_state_callback(Some(Box::new(move || {
                let state = op_ref.lock().unwrap().as_ref().map(
                    Operation::get_state
                );

                match state {
                    Some(OperationState::Done) => {
                        // There should never be any other strong references to
                        // the value when this runs, so just try to unwrap it 
                        if let Some(result) = mem::take(&mut result).map(Arc::into_inner).flatten() {
                            let result = result.into_inner().unwrap();

                            if util::opt_oneshot_try_send(&mut tx, Ok(result)).is_err() {
                                warn!("Failed to report operation result, receiver dropped");
                            }
                        } else {
                            warn!("Could not acquire context result");
                            
                            if util::opt_oneshot_try_send(
                                &mut tx,
                                Err(OperationState::Done)
                            ).is_err() {
                                warn!("Failed to report operation result, receiver dropped");
                            }
                        }
                    },
                    Some(OperationState::Cancelled) | None => {
                        if state.is_none() {
                            warn!("Operation handle dropped during callback execution");
                        }

                        if util::opt_oneshot_try_send(
                            &mut tx,
                            Err(OperationState::Cancelled)
                        ).is_err() {
                            warn!("Failed to report operation result, receiver dropped");
                        }
                    }
                    Some(state) => debug!(
                        "Context handler ignoring state change to {:?}",
                        state
                    ),
                }

                if tx.is_none() {
                    mem::drop(mem::take(&mut op_ref));
                }
            })));
        }).await;

        //this returns Result<Result<T, OperationState>, RecvError>. If the
        //outer Result returns an Err, then the sender was dropped before it
        //sent any value, so we treat this as a cancelled state.
        rx.await.map_err(|_| OperationState::Cancelled)?
    }

    /// Executes the given function `op`, and waits for the returned
    /// [`Operation`] to terminate before returning a user-defined value that
    /// implements [`Default`].
    pub async fn do_ctx_op_default<T, S, F>(&self, op: F) -> Result<T, OperationState>
    where
        T: Default + Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(&mut Context, Weak<Mutex<T>>) -> Operation<S> + Send + 'static {
        self.do_ctx_op_impl(T::default(), op).await
    }

    /// Executes the given function `op`, and waits for the returned
    /// [`Operation`] to terminate before returning a user-defined value, or
    /// `None` if the operation completed without setting a value.
    pub async fn do_ctx_op<T, S, F>(&self, op: F) -> Result<Option<T>, OperationState>
    where
        T: Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(&mut Context, Weak<Mutex<Option<T>>>) -> Operation<S> + Send + 'static {
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
        F: FnOnce(&mut Context, Weak<Mutex<Vec<T>>>) -> Operation<S> + Send + 'static {
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
    fn start(mut self) -> ControlledJoinHandle<()> {
        ControlledJoinHandle::from(|rx| task::spawn_local(async move {
            self.mainloop(rx).await;
        }))
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
) -> Result<(ControlledJoinHandle<()>, InitializingFuture<ContextState, Context>), Code> {
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

async fn stop_mainloop(mainloop_handle: ControlledJoinHandle<()>) {
    if let Err(e) = mainloop_handle.join().await {
        if e.is_panic() {
            warn!("Mainloop task panicked on shutdown");
        }
    }
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
                        _ = mainloop_handle.await_completion() => panic!("Mainloop terminated unexpectedly"),
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
                stop_mainloop(mainloop_handle);
                debug!("Context handler shut down");
            }))
        },
        Err(state) => {
            error!("PulseAudio context failed to connect");
            stop_mainloop(mainloop_handle);
            Err(if state == ContextState::Terminated {
                Code::ConnectionTerminated
            } else {
                Code::Unknown
            })
        }
    }
}
