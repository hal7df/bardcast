///! Utility types to help bridge libpulse/C-style asynchronous calls with
///! Rust-style asynchronous calls.

extern crate libpulse_binding as libpulse;

use std::future::Future;
use std::marker::{PhantomData, Unpin};
use std::mem;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use log::{debug, warn};
use tokio::sync::oneshot::{self, Receiver as OneshotReceiver};

use libpulse::context::{Context as PulseContext, State as PulseContextState};
use libpulse::operation::{
    Operation as PulseOperation,
    State as PulseOperationState
};
use libpulse::stream::{Stream as PulseStream, State as PulseStreamState};

use crate::util;

//TYPE DEFINITIONS *************************************************************

/// Generic representation of state for an entity that initializes
/// asynchronously.
pub enum InitializingState<S> {
    /// The entity is currently initializing.
    Initializing,

    /// The entity has initialized and is ready for use.
    Initialized,

    /// The entity failed to properly initialize, with the wrapped value
    /// providing a specific state or error value.
    InitializationFailed(S),
}

/// Trait implemented for libpulse entities that initialize asynchronously.
pub trait InitializingEntity<S> {
    /// Queries the object's current state of initialization.
    fn get_initialization_state(&self) -> InitializingState<S>;

    /// When `waker` is `Some`, sets the provided [`Waker`] to wake when the
    /// object's state next changes. When `None`, unsets any existing wake
    /// notifications for this object.
    fn wake_on_state_change(&mut self, waker: Option<Waker>);
}

/// [`Future`] implementation for libpulse entities that initialize
/// asynchronously. The underlying object is unavailable until its
/// [`InitializingEntity`] implementation returns either `Initialized` or
/// `InitializationFailed`, the result of which is returned accordingly as a
/// [`Result`].
pub struct InitializingFuture<S, T: InitializingEntity<S>>(
    Option<T>,
    PhantomData<Rc<S>>, //satisfies the S generic, impls !Send + !Sync
);

/// Helper for asynchronously determining when a recording audio stream has new
/// data to be read.
///
/// Stream handler loops are likely to iterate much faster than PulseAudio can
/// provide data, leading to wasted CPU cycles. This allows handlers to be
/// awoken only when data becomes available.
// TODO: Find a way to get rid of this (perhaps in favor of tokio::sync::Notify,
// if the reason it causes repeated audio gaps can be identified and addressed).
pub struct StreamReadNotifier(Arc<Mutex<Option<Waker>>>);

/// [`Future`] implementation used in tandem with [`StreamReadNotifier`]. This
/// type should not be instantiated directly.
pub struct StreamReadFuture<'a> {
    waker: &'a Arc<Mutex<Option<Waker>>>,
    has_polled: bool,
}

//IMPL: InitializingFuture *****************************************************
impl<S: Unpin, T: InitializingEntity<S> + Unpin> From<T> for InitializingFuture<S, T> {
    fn from(entity: T) -> Self {
        Self(Some(entity), PhantomData)
    }
}

impl<S: Unpin, T: InitializingEntity<S> + Unpin> Future for InitializingFuture<S, T> {
    type Output = Result<T, S>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(entity) = &mut self.0 {
            match entity.get_initialization_state() {
                InitializingState::Initializing => {
                    entity.wake_on_state_change(Some(ctx.waker().clone()));
                    Poll::Pending
                },
                InitializingState::Initialized => {
                    entity.wake_on_state_change(None);
                    let entity = mem::take(&mut self.0).unwrap();
                    Poll::Ready(Ok(entity))
                },
                InitializingState::InitializationFailed(state) => {
                    entity.wake_on_state_change(None);
                    Poll::Ready(Err(state))
                }
            }
        } else {
            panic!("Attempted to poll an exhausted InitializingFuture");
        }
    }
}

impl<S, T: InitializingEntity<S>> Drop for InitializingFuture<S, T> {
    fn drop(&mut self) {
        if let Some(entity) = &mut self.0 {
            //In case the future is cancelled, clear out the Waker to prevent
            //the stream read callback from potentially accidentally waking a
            //nonexistent task
            entity.wake_on_state_change(None)
        }
    }
}

impl InitializingEntity<PulseContextState> for PulseContext {
    fn get_initialization_state(&self) -> InitializingState<PulseContextState> {
        match self.get_state() {
            PulseContextState::Failed | PulseContextState::Terminated =>
                InitializingState::InitializationFailed(self.get_state()),
            PulseContextState::Ready => InitializingState::Initialized,
            _ => InitializingState::Initializing,
        }
    }

    fn wake_on_state_change(&mut self, waker: Option<Waker>) {
        self.set_state_callback(waker.map(|waker| -> Box<dyn FnMut() + 'static> {
            Box::new(move || waker.wake_by_ref())
        }));
    }
}

impl InitializingEntity<PulseStreamState> for PulseStream {
    fn get_initialization_state(&self) -> InitializingState<PulseStreamState> {
        match self.get_state() {
            PulseStreamState::Unconnected | PulseStreamState::Creating =>
                InitializingState::Initializing,
            PulseStreamState::Failed | PulseStreamState::Terminated =>
                InitializingState::InitializationFailed(self.get_state()),
            PulseStreamState::Ready => InitializingState::Initialized,
        }
    }

    fn wake_on_state_change(&mut self, waker: Option<Waker>) {
        self.set_state_callback(waker.map(|waker| -> Box<dyn FnMut() + 'static> {
            Box::new(move || waker.wake_by_ref())
        }));
    }
}

//IMPL: StreamReadNotifier *****************************************************
impl StreamReadNotifier {
    /// Creates a new notifier for the given audio recording stream.
    ///
    /// While this will create a notifier for playback streams, it will never
    /// notify for new data.
    pub fn new(stream: &mut PulseStream) -> Self {
        let waker: Arc<Mutex<Option<Waker>>> = Arc::default();
        let waker_ref = Arc::clone(&waker);

        stream.set_read_callback(Some(Box::new(move |_| {
            if let Some(waker) = mem::take(&mut *waker_ref.lock().unwrap()) {
                waker.wake();
            }
        })));

        Self(waker)
    }

    /// Waits for new data to become available in the associated recording
    /// stream.
    pub fn await_data<'a>(&'a self) -> StreamReadFuture<'a> {
        StreamReadFuture::new(&self.0)
    }

    /// Closes the notifier, and unsets the read callback on the stream. This
    /// should be done instead of letting the notifier go out of scope
    /// implicitly, as leaving the read callback set can lead to intermittent
    /// segfaults.
    ///
    /// This is done as a dedicated function to avoid holding a mutable
    /// reference to the stream during the notifier's lifetime when it is not
    /// needed, which would also prevent reading data from the stream.
    pub fn close(self, stream: &mut PulseStream) {
        stream.set_read_callback(None);
    }
}

//IMPL: StreamReadFuture *******************************************************
impl<'a> StreamReadFuture<'a> {
    fn new(waker: &'a Arc<Mutex<Option<Waker>>>) -> Self {
        Self {
            waker,
            has_polled: false,
        }
    }
}

impl<'a> Future for StreamReadFuture<'a> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.has_polled {
            //Future has been polled once before and has now reawoken, so we can
            //complete the future.
            Poll::Ready(())
        } else {
            //Future has not been polled yet, so set the appropriate state and
            //sleep until reawoken.
            *self.waker.lock().unwrap() = Some(ctx.waker().clone());
            self.has_polled = true;
            Poll::Pending
        }
    }
}

impl<'a> Drop for StreamReadFuture<'a> {
    fn drop(&mut self) {
        //In case the future is cancelled, clear out the Waker to prevent the
        //stream read callback from potentially accidentally waking a
        //nonexistent task
        *self.waker.lock().unwrap() = None;
    }
}

// PUBLIC UTILITY FUNCTIONS ****************************************************
/// Helper function for converting PulseAudio's C-style asynchronous operations
/// that use the `Operation` type into Rust-compatible `Future`s.
///
/// A default/initial value for the result of the future is required, which will
/// be returned if the `Operation` changes to the `Done` state without returning
/// any data.
pub fn operation_to_future<T, S, F>(
    initial_value: T,
    op: F
) -> OneshotReceiver<Result<T, PulseOperationState>>
where
    T: 'static,
    S: ?Sized + 'static,
    F: FnOnce(Weak<Mutex<T>>) -> PulseOperation<S>
{
    let (tx, rx) = oneshot::channel::<Result<T, PulseOperationState>>();
    let value = Arc::new(Mutex::new(initial_value));
    let op = Arc::new(Mutex::new(Some(op(Arc::downgrade(&value)))));

    let mut op_ref = Arc::clone(&op);
    let mut tx = Some(tx);
    let mut result = Some(value);

    op.lock().unwrap().as_mut().unwrap().set_state_callback(Some(Box::new(move || {
        let state = op_ref.lock().unwrap().as_ref().map(PulseOperation::get_state);

        match state {
            Some(PulseOperationState::Done) => {
                // There should never be any other strong references to the
                // value when this runs, so just try to unwrap it
                if let Some(result) = mem::take(&mut result).map(Arc::into_inner).flatten() {
                    let result = result.into_inner().unwrap();

                    if util::opt_oneshot_try_send(&mut tx, Ok(result)).is_err() {
                        warn!("Failed to report operation result, receiver dropped");
                    }
                } else {
                    warn!("Could not acquire context result");

                    if util::opt_oneshot_try_send(
                        &mut tx,
                        Err(PulseOperationState::Done)
                    ).is_err() {
                        warn!("Failed to report operation result, receiver dropped");
                    }
                }
            },
            Some(PulseOperationState::Cancelled) | None => {
                if state.is_none() {
                    warn!("Operation handle dropped during callback execution");
                }

                if util::opt_oneshot_try_send(
                    &mut tx,
                    Err(PulseOperationState::Cancelled)
                ).is_err() {
                    warn!("Failed to report operation result, receiver dropped");
                }
            },
            Some(state) => debug!(
                "Pulse operation handler ignoring state change to {:?}",
                state
            ),
        }

        if tx.is_none() {
            mem::drop(mem::take(&mut op_ref));
        }
    })));

    rx
}
