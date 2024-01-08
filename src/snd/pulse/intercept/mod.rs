///! Utilities for intercepting application audio streams for recording.
///!
///! The core abstraction in this modules is the [`Interceptor`] trait, which
///! allows bardcast to intercept applications playing audio via their sink
///! input ID. When used in conjunction with the event notification
///! functionality of the [`super::event`] module, this allows applications to
///! be efficiently intercepted as soon as they open an audio stream.

mod concrete;
mod util;

use std::borrow::Cow;
use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};

use async_trait::async_trait;
use libpulse_binding::error::Code;

use super::owned::OwnedSinkInputInfo;

// CONSTANT DEFINITIONS ********************************************************
const TEARDOWN_FAILURE_WARNING: &'static str =
    "Application audio may not work correctly. Please check your system audio \
     configuration.";
const SINK_INPUT_MOVE_FAILURE: &'static str =
    "Some captured inputs failed to move to their original or default sink.";

// TYPE DEFINITIONS ************************************************************
/// Represents possible error modes for [`Interceptor::record()`].
#[derive(Debug)]
pub enum InterceptError<'a> {
    /// PulseAudio returned an error while attempting to intercept the
    /// application.
    PulseError(Code),

    /// The interceptor cannot concurrently intercept any more applications.
    AtCapacity(usize, Cow<'a, OwnedSinkInputInfo>),
}

/// Core abstraction for intercepting application audio.
///
/// The main function in this interface is [`intercept`], which performs the
/// work of intercepting applications and routing them such that bardcast can
/// read their stream. This interface also provides a few functions to query the
/// current state of intercepted streams.
#[async_trait]
pub trait Interceptor: Send + Sync {
    /// Starts recording audio from the given application.
    async fn record<'a>(
        &mut self,
        source: Cow<'a, OwnedSinkInputInfo>
    ) -> Result<(), InterceptError<'a>>;

    /// Updates the stream metadata for the given captured stream. This may
    /// cork or uncork the stream depending on the cork state of all captured
    /// applications.
    async fn update_capture(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code>;

    /// Stops recording audio from the given application. If this interceptor
    /// is not currently recording audio from the given application, this
    /// returns `Ok(false)`.
    async fn stop(
        &mut self,
        source_idx: u32
    ) -> Result<bool, Code>;

    /// Determines if [`Interceptor::monitor()`] must be called to ensure
    /// underlying tasks are properly monitored and cleaned up during normal
    /// operation.
    fn needs_monitor(&self) -> bool;

    /// Monitors and cleans up underlying tasks, if any.
    async fn monitor(&mut self);

    /// Returns the number of applications that have been intercepted by this
    /// interceptor.
    fn len(&self) -> usize;

    /// Gracefully closes any system resources that were created by the
    /// `Interceptor`, returning any intercepted streams to another audio device
    /// if needed.
    async fn close(mut self);

    /// Version of [`Interceptor::close`] for boxed trait objects.
    async fn boxed_close(mut self: Box<Self>);
}

/// Specialization of [`Interceptor`] that allows for limits on the number of
/// concurrently captured applications.
pub trait LimitingInterceptor: Interceptor {
    /// Returns the number of concurrent applications that can be intercepted
    /// by this `LimitedInterceptor` instance.
    ///
    /// Returns `None` if there is no limit applied to this interceptor.
    fn capacity(&self) -> Option<usize>;

    /// Returns the number of additional applications that can be concurrently
    /// intercepted by this `LimitedInterceptor` instance.
    ///
    /// Returns `None` if there is no limit applied to this interceptor.
    fn remaining(&self) -> Option<usize> {
        self.capacity().map(|capacity| capacity - self.len())
    }

    /// Determines whether this interceptor has intercepted the maximum number
    /// of concurrent applications.
    ///
    /// This will always be `false` for interceptors with no limit.
    fn is_full(&self) -> bool {
        self.remaining().is_some_and(|remaining| remaining == 0)
    }
}

// TRAIT IMPLS *****************************************************************
impl From<Code> for InterceptError<'_> {
    fn from(code: Code) -> Self {
        Self::PulseError(code)
    }
}

impl<'a> TryFrom<InterceptError<'a>> for Code {
    type Error = InterceptError<'a>;

    fn try_from(err: InterceptError) -> Result<Self, Self::Error> {
        if let InterceptError::PulseError(code) = err {
            Ok(code)
        } else {
            Err(err)
        }
    }
}

impl Display for InterceptError<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            Self::PulseError(code) => code.fmt(f),
            Self::AtCapacity(limit, source) => write!(
                f,
                "Cannot intercept application {}, interceptor at limit ({})",
                source.as_ref(),
                limit
            ),
        }
    }
}

impl Error for InterceptError<'_> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::PulseError(code) = self {
            Some(&code)
        } else {
            None
        }
    }
}

// PUBLIC HELPER FUNCTIONS *****************************************************
/// Helper function to close an interceptor if `res` is an error, returning the
/// `Ok` value of `res` and the boxed interceptor if successful, otherwise
/// bubbling up the error.
///
/// Due to the lack of an async `Drop` trait, closing interceptors must be done
/// manually, which can make code that deals with `Result` types less ergonomic.
/// This function alleviates this problem by allowing for the continued use of
/// the `?` operator without causing resource leaks in the PulseAudio server.
pub async fn boxed_close_interceptor_if_err<I: Interceptor + ?Sized, T, E>(
    intercept: Box<I>,
    res: Result<T, E>
) -> Result<(T, Box<I>), E> {
    match res {
        Ok(val) => Ok((val, intercept)),
        Err(err) => {
            intercept.boxed_close().await;
            Err(err)
        }
    }
}
