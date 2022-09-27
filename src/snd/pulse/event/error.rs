///! Error types used internally and externally by the PulseAudio event handler.

extern crate libpulse_binding as libpulse;

use std::convert::From;
use std::error::Error;
use std::fmt::{Debug, Display, Error as FormatError, Formatter};

use libpulse::context::subscribe::Facility;

use tokio::sync::broadcast::error::RecvError;

use super::super::PulseFailure;
use super::{AudioEntity, ChangeEvent, ChangeEntity, EntityInfo};

// TYPE DEFINITIONS ************************************************************

/// Enum representing theoretical (albeit unlikely) error cases where libpulse
/// fails to provide mandatory event information.
#[derive(Clone, Copy, Debug)]
pub enum MissingEventField {
    /// The event had a null audio graph entity type.
    Facility,

    /// The event had a null change type.
    ChangeType,
}

/// Error type returned when the event handler fails to look up the full
/// metadata for an audio graph entity when it might otherwise be expected to.
/// The raw event (without metadata) can be fetched from the error using
/// [`Self::raw_event`].
#[derive(Clone, Debug)]
pub struct LookupError<E> {
    error: PulseFailure,
    raw_event: ChangeEvent<E>,
}

/// Error type returned when the event handler failed to resolve an event into
/// entity metadata.
#[derive(Clone, Debug)]
pub enum ResolveError {
    /// The event handler code doesn't support resolving audio graph entities
    /// of the given type. The index of the entity in question is also provided.
    UnsupportedFacility(Facility, u32),

    /// The event handler failed to look up the entity metadata due to a
    /// runtime error.
    LookupError(LookupError<ChangeEntity>),
}

/// Types of errors that can be returned by an event listener.
#[derive(Clone, Debug)]
pub enum EventListenerError<E> {
    /// The event handler failed to look up the entity metadata due to a
    /// runtime error.
    LookupError(LookupError<E>),

    /// A communication error occurred with the event handler task.
    ChannelError(RecvError),
}

//TYPE IMPLS *******************************************************************
impl<E> LookupError<E> {
    /// Creates a new error instance from the given root cause and raw event.
    pub fn new(error: PulseFailure, raw_event: ChangeEvent<E>) -> Self {
        Self {
            error,
            raw_event,
        }
    }

    /// Gets the raw, unresolved event that triggered the error.
    pub fn raw_event(&self) -> &ChangeEvent<E> {
        &self.raw_event
    }
}

impl LookupError<ChangeEntity> {
    /// Attempts to unwrap the raw [`AudioEntity`] from the inner
    /// [`ChangeEvent`] if it matches the specified type, returning `None`
    /// otherwise.
    pub fn try_map_entity<I: EntityInfo>(self) -> Option<LookupError<AudioEntity<I>>> {
        self.raw_event.try_unwrap().map(|raw_event| LookupError {
            error: self.error,
            raw_event,
        })
    }
}

//TRAIT IMPLS ******************************************************************
impl<E: Display> Display for LookupError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        write!(f, "Lookup failure for event {}: {}", self.raw_event, self.error)
    }
}

impl<E: Display> Display for EventListenerError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            EventListenerError::LookupError(e) => e.fmt(f),
            EventListenerError::ChannelError(e) => write!(f, "Event broadcast channel error: {}", e),
        }
    }
}

impl<E: Display + Debug> Error for LookupError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

impl<E: Display + Debug + 'static> Error for EventListenerError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            EventListenerError::LookupError(e) => e,
            EventListenerError::ChannelError(e) => e,
        })
    }
}

//CONVERSIONS ******************************************************************
impl<E> From<LookupError<E>> for PulseFailure {
    fn from(err: LookupError<E>) -> Self {
        err.error
    }
}

impl<E> From<LookupError<E>> for EventListenerError<E> {
    fn from(err: LookupError<E>) -> Self {
        Self::LookupError(err)
    }
}

impl<E> From<RecvError> for EventListenerError<E> {
    fn from(err: RecvError) -> Self {
        Self::ChannelError(err)
    }
}
