///! Utilities to help manage asynchronous tasks.

use std::convert::{AsMut, AsRef};
use std::mem;
use std::vec::Vec;

use async_trait::async_trait;
use futures::future::{self, SelectAll};
use log::warn;
use tokio::sync::oneshot::{
    self,
    Receiver as OneshotReceiver,
    Sender as OneshotSender
};
use tokio::task::{JoinError, JoinHandle, JoinSet};

// TYPE DEFINITIONS ************************************************************

/// Lightweight wrapper around a [`JoinHandle`] that aborts the task when it is
/// dropped. Generally useful for cancelling tasks that are spawned in the
/// middle of operations that can fail.
///
/// This does not allow direct access to the underlying [`JoinHandle`]; instead
/// the wrapper should be consumed with [`AbortingJoinHandle::take`].
#[derive(Default)]
pub struct AbortingJoinHandle<T>(Option<JoinHandle<T>>);

/// Wrapper around a [`JoinHandle`] that has the ability to send a graceful
/// shutdown signal to the underlying task, allowing it to clean up before
/// terminating.
pub enum ControlledJoinHandle<T> {
    ActiveTask(JoinHandle<T>, OneshotSender<()>),
    Returned(Result<T, JoinError>),
}

/// Wrapper utility that allows for initialization funcions to return both a
/// value and a [`JoinHandle`] for a task that was spawned during initialization.
pub struct ValueJoinHandle<T>(T, JoinHandle<()>);

/// Builder for a [`TaskSet`] that consumes externally spawned [`JoinHandle`]s.
///
/// If dropped without converting to a [`TaskSet`], the builder will abort all
/// tasks added to the list.
pub struct TaskSetBuilder(Vec<JoinHandle<()>>);

/// Similar in concept to a [`JoinSet`], but allows for the collection of
/// [`JoinHandle`]s without needing to spawn them in the context of the set.
///
/// All active tasks in the set are aborted when the set is dropped.
///
/// [`TaskSet`]s should not be created directly, see [`TaskSetBuilder`] instead.
pub struct TaskSet(Option<SelectAll<JoinHandle<()>>>);

/// Trait defining common functionality for collections of task handles.
#[async_trait]
pub trait TaskContainer<T> {
    /// Waits until the next task in the container completes, and returns the
    /// result. If there are no tasks in the container, returns `None`.
    async fn join_next(&mut self) -> Option<Result<T, JoinError>>;

    /// Tests whether there are currently any tasks in the container.
    fn is_empty(&self) -> bool;
}

// TYPE IMPLS ******************************************************************
impl<T> AbortingJoinHandle<T> {
    /// Consumes the [`AbortingJoinHandle`], returning the underlying task
    /// handle without aborting it.
    pub fn take(&mut self) -> Option<JoinHandle<T>> {
        mem::take(&mut self.0)
    }
}

impl<T> ControlledJoinHandle<T> {
    /// Aborts the underlying task without sending a stop signal. Identical to
    /// [`JoinHandle::abort`].
    pub fn abort(&self) {
        if let Self::ActiveTask(task, _) = self {
            task.abort();
        }
    }

    /// Checks if the task associated with this `ControlledJoinHandle` has
    /// finished.
    ///
    /// As with [`JoinHandle::is_finished`], this method can return `false` even
    /// if [`ControlledJoinHandle::abort`] has been called on the task, as this
    /// does not return true until the task has actually stopped, which does not
    /// happen immediately when a task is aborted.
    pub fn is_finished(&self) -> bool {
        if let Self::ActiveTask(task, _) = self {
            task.is_finished()
        } else {
            true
        }
    }

    /// Waits for the underlying task to return a result, without aborting or
    /// signaling to the task to quit.
    pub async fn await_completion(&mut self) {
        if let Self::ActiveTask(task, _) = self {
            let result = task.await;
            *self = Self::Returned(result);
        }
    }

    /// Signals the underlying task to quit, and waits for it to return a
    /// result. If the underlying task has closed its control handle, the task
    /// will be aborted.
    pub async fn join(self) -> Result<T, JoinError> {
        match self {
            Self::ActiveTask(task, control_tx) => {
                if control_tx.send(()).is_err() && !task.is_finished() {
                    warn!("Controlled task closed its control handle but is \
                           still running. It will be forcibly aborted.");
                    task.abort();
                }

                task.await
            },
            Self::Returned(result) => result,
        }
    }
}

impl<T> ValueJoinHandle<T> {
    /// Creates a new [`ValueJoinHandle`] from the given value and task handle.
    pub fn new(val: T, task: JoinHandle<()>) -> Self {
        Self(val, task)
    }

    /// Consumes the [`ValueJoinHandle`], returning the underlying value and
    /// task handle.
    pub fn into_tuple(self) -> (T, JoinHandle<()>) {
        (self.0, self.1)
    }
}

impl TaskSetBuilder {
    /// Creates an empty [`TaskSetBuilder`].
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Adds the given [`JoinHandle`] to the list of tasks to add to the
    /// [`TaskSet`].
    pub fn insert(&mut self, handle: JoinHandle<()>) {
        self.0.push(handle);
    }

    /// Destructures the given [`ValueJoinHandle`] before inserting just the
    /// task handle into the set, returing the associated value.
    pub fn detaching_insert<T>(&mut self, handle: ValueJoinHandle<T>) -> T {
        self.insert(handle.1);
        handle.0
    }

    /// Consumes this [`TaskSetBuilder`] and creates a corresponding
    /// [`TaskSet`] over the collected tasks.
    pub fn build(mut self) -> TaskSet {
        if !self.0.is_empty() {
            TaskSet(Some(future::select_all(mem::take(&mut self.0))))
        } else {
            TaskSet(None)
        }
    }
}

/// TRAIT IMPLS ****************************************************************
impl<F, T> From<F> for ControlledJoinHandle<T>
where F: FnOnce(OneshotReceiver<()>) -> JoinHandle<T> {
    fn from(spawn_fn: F) -> Self {
        let (tx, rx) = oneshot::channel::<()>();

        Self::ActiveTask(spawn_fn(rx), tx)
    }
}

#[async_trait]
impl TaskContainer<()> for TaskSet {
    async fn join_next(&mut self) -> Option<Result<(), JoinError>> {
        if let Some(next_task) = &mut self.0 {
            let (result, _, tasks) = next_task.await;

            if !tasks.is_empty() {
                self.0 = Some(future::select_all(tasks))
            } else {
                self.0 = None
            }

            Some(result)
        } else {
            None
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_none()
    }
}

#[async_trait]
impl<T: Send + 'static> TaskContainer<T> for JoinSet<T> {
    async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        JoinSet::join_next(self).await
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl<T> AsRef<T> for ValueJoinHandle<T> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T> AsMut<T> for ValueJoinHandle<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T> From<JoinHandle<T>> for AbortingJoinHandle<T> {
    fn from(join_handle: JoinHandle<T>) -> Self {
        Self(Some(join_handle))
    }
}

impl<T> Drop for AbortingJoinHandle<T> {
    fn drop(&mut self) {
        if let Some(join_handle) = &self.0 {
            join_handle.abort();
        }
    }
}

impl Drop for TaskSetBuilder {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

impl Drop for TaskSet {
    fn drop(&mut self) {
        if let Some(tasks) = self.0.take() {
            for handle in tasks.into_inner() {
                handle.abort();
            }
        }
    }
}
