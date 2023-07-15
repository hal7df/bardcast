///! Utility types to help bridge libpulse/C-style asynchronous calls with
///! Rust-style asynchronous calls.

extern crate libpulse_binding as libpulse;

use std::future::Future;
use std::marker::{PhantomData, Unpin};
use std::mem;
use std::ops::{Deref, DerefMut, FnOnce};
use std::pin::Pin;
use std::sync::{Arc, LockResult, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use libpulse::context::{Context as PulseContext, State as PulseContextState};
use libpulse::stream::{Stream as PulseStream, State as PulseStreamState};
use libpulse::operation::{Operation, State};

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
    PhantomData<S>
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

/// Inner state for [`PulseFuture`].
pub struct PulseFutureInner<T> {
    state: State,
    waker: Option<Waker>,
    result: T,
}

/// A wrapper for a [`PulseFuture`]'s internal state that exposes just the
/// future's pending result.
pub struct PulseResultRef<T>(Arc<Mutex<PulseFutureInner<T>>>);

/// Lock guard for [`PulseResultRef`].
pub struct PulseResultGuard<'a, T>(MutexGuard<'a, PulseFutureInner<T>>);

/// Generic future implementation for asynchronous operations executing in the
/// libpulse mainloop abstraction.
pub struct PulseFuture<T>(Arc<Mutex<PulseFutureInner<T>>>);

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

//IMPL: PulseFuture ************************************************************
impl<T> PulseFutureInner<T> {
    /// Creates a new shared state object seeded with the given initial value.
    pub fn from(initial: T) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            state: State::Running,
            waker: None,
            result: initial,
        }))
    }

    /// Calls the given operation with a reference to the pending future result
    /// that does not expose other state values.
    pub fn apply<S: ?Sized, F>(
        this: &Arc<Mutex<Self>>,
        ctx: &mut PulseContext,
        op: F
    ) -> Operation<S>
    where F: FnOnce(&mut PulseContext, PulseResultRef<T>) -> Operation<S> {
        op(ctx, PulseResultRef::from(this))
    }

    /// Determines if the operation associated with this future has terminated.
    pub fn terminated(&self) -> bool {
        self.state != State::Running
    }

    /// Updates the cached operation execution state.
    pub fn set_state(&mut self, state: State) {
        self.state = state;
    }

    /// Wakes the associated future, if it has caused a task to go to sleep.
    pub fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }
}

impl<T: Default> PulseFutureInner<T> {
    /// Creates a new shared state object seeded with the underlying type's
    /// `Default` value.
    pub fn new() -> Arc<Mutex<Self>> {
        Self::from(T::default())
    }
}

impl<T> From<&Arc<Mutex<PulseFutureInner<T>>>> for PulseResultRef<T> {
    fn from(inner: &Arc<Mutex<PulseFutureInner<T>>>) -> Self {
        Self(Arc::clone(inner))
    }
}

impl<T> PulseResultRef<T> {
    /// Locks the underlying state object, and returns a guard object that
    /// unlocks the state object when it is dropped.
    pub fn lock<'a>(&'a self) -> LockResult<PulseResultGuard<'a, T>> {
        self.0.lock()
            .map(|guard| PulseResultGuard::from(guard))
            .map_err(|poison| PoisonError::new(PulseResultGuard::from(poison.into_inner())))
    }
}

impl<'a, T> From<MutexGuard<'a, PulseFutureInner<T>>> for PulseResultGuard<'a, T> {
    fn from(inner: MutexGuard<'a, PulseFutureInner<T>>) -> Self {
        Self(inner)
    }
}

impl<T> Deref for PulseResultGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0.result
    }
}

impl<T> DerefMut for PulseResultGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0.result
    }
}

impl<T> From<&Arc<Mutex<PulseFutureInner<T>>>> for PulseFuture<T> {
    fn from(op: &Arc<Mutex<PulseFutureInner<T>>>) -> Self {
        Self(Arc::clone(op))
    }
}

impl<T: Default> Future for PulseFuture<T> {
    type Output = Result<T, State>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.0.lock().unwrap();

        match inner.state {
            State::Running => {
                inner.waker = Some(ctx.waker().clone());
                Poll::Pending
            },
            State::Cancelled => Poll::Ready(Err(inner.state)),
            State::Done => Poll::Ready(Ok(mem::take(&mut inner.result))),
        }
    }
}

impl<T> Drop for PulseFuture<T> {
    fn drop(&mut self) {
        //In case the future is cancelled, clear out the Waker to prevent the
        //stream read callback from potentially accidentally waking a
        //nonexistent task
        self.0.lock().unwrap().waker = None
    }
}
