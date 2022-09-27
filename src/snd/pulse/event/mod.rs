///! Abstractions for handling entity status changes received from the
///! PulseAudio server.
///!
///! PulseAudio's capacity to notify applications when and how the audio graph
///! changes is rather simplistic, only providing the entity's name, index, and
///! whether it was added, changed, or removed. This module, its submodules, and
///! its types provide a more thorough event notification system that provides
///! additional entity metadata (if possible) on top of the existing information
///! provided by libpulse, while providing a more ergonomic interface by
///! leveraging Rust's type system.

extern crate libpulse_binding as libpulse;

mod error;
pub mod factory;
mod listen;

use std::fmt::{Display, Error as FormatError, Formatter};

use async_trait::async_trait;
use log::{error, warn};

use libpulse::context::subscribe::{
    Facility,
    Operation as FacilityOperation
};

use super::PulseFailure;
use super::context::AsyncIntrospector;
use super::owned::{OwnedSinkInfo, OwnedSinkInputInfo};

//RE-EXPORTS *******************************************************************
pub use self::error::{
    EventListenerError,
    LookupError
};
pub use self::listen::{
    EventListener,
    FilterMapEventListener,
    OwnedEventListener,
    ToFilterMapped
};

//TYPES ************************************************************************

/// Interface for entity info/metadata types for audio graph entities that
/// PulseAudio can notify about.
#[async_trait]
pub trait EntityInfo: Clone + Send {
    /// Looks up the entity metadata of this type that has the specified index.
    async fn lookup(introspect: &AsyncIntrospector, idx: u32) -> Result<Self, PulseFailure>;

    /// Looks up all entity metadata of this type currently present in the audio
    /// graph.
    async fn lookup_all(introspect: &AsyncIntrospector) -> Result<Vec<Self>, PulseFailure>;

    /// Unwraps an [`AudioEntity`] from the given [`ChangeEntity`] if it wraps
    /// an entity of this type.
    fn from_change_entity(change_entity: ChangeEntity) -> Option<AudioEntity<Self>>;

    /// The server index of the entity referred to by this info instance.
    fn index(&self) -> u32;

    /// Gets the [`Facility`] representation of this type.
    fn to_entity_type() -> Facility;
}

/// Representation of an audio entity as either a full set of metadata or just
/// an index.
#[derive(Clone, Debug)]
pub enum AudioEntity<I: EntityInfo> {
    /// The entity's full metadata.
    Info(I),

    /// An index referring to the entity's server index.
    Index(u32),
}

/// Wrapper type for all possible audio graph entity types that the `event`
/// module can handle.
#[derive(Clone, Debug)]
pub enum ChangeEntity {
    Sink(AudioEntity<OwnedSinkInfo>),
    SinkInput(AudioEntity<OwnedSinkInputInfo>),
}

/// Root event type sent to event listeners, indicating the type of change that
/// occurred to the given audio graph entity.
#[derive(Clone, Debug)]
pub enum ChangeEvent<E> {
    New(E),
    Changed(E),
    Removed(E),
}

// TYPE IMPLS ******************************************************************
impl<I: EntityInfo> AudioEntity<I> {
    /// Gets the server index for the wrapped entity.
    pub fn index(&self) -> u32 {
        match self {
            Self::Info(info) => info.index(),
            Self::Index(idx) => *idx,
        }
    }
}

impl ChangeEntity {
    /// Gets the server index for the wrapped entity.
    pub fn index(&self) -> u32 {
        match self {
            Self::Sink(sink) => sink.index(),
            Self::SinkInput(sink_input) => sink_input.index(),
        }
    }

    /// Gets the type of the wrapped audio graph entity as a [`Facility`].
    pub fn facility(&self) -> Facility {
        match self {
            Self::Sink(_) => Facility::Sink,
            Self::SinkInput(_) => Facility::SinkInput,
        }
    }
}

impl<E> ChangeEvent<E> {
    /// Returns a human-readable name for the type of change that occurred.
    fn name(&self) -> &str {
        match self {
            Self::New(_) => "New",
            Self::Changed(_) => "Changed",
            Self::Removed(_) => "Removed",
        }
    }

    /// Returns a reference to the underlying entity affected by this event.
    pub fn entity(&self) -> &E {
        match self {
            Self::New(entity) => &entity,
            Self::Changed(entity) => &entity,
            Self::Removed(entity) => &entity,
        }
    }

    /// Maps the entity affected by this event according to the given map
    /// function, while preserving the `ChangeEvent` enum variant.
    pub fn map<T, F: FnOnce(E) -> T>(self, op: F) -> ChangeEvent<T> {
        match self {
            Self::New(entity) => ChangeEvent::New(op(entity)),
            Self::Changed(entity) => ChangeEvent::Changed(op(entity)),
            Self::Removed(entity) => ChangeEvent::Removed(op(entity)),
        }
    }
}

impl<E> ChangeEvent<Option<E>> {
    /// Converts a `ChangeEvent<Option<E>>` to an `Option<ChangeEvent<E>>`.
    pub fn transpose(self) -> Option<ChangeEvent<E>> {
        match self {
            Self::New(None) | Self::Changed(None) | Self::Removed(None) => None,
            Self::New(Some(entity)) => Some(ChangeEvent::New(entity)),
            Self::Changed(Some(entity)) => Some(ChangeEvent::Changed(entity)),
            Self::Removed(Some(entity)) => Some(ChangeEvent::Removed(entity)),
        }
    }
}

impl ChangeEvent<ChangeEntity> {
    /// Wraps a `ChangeEntity` in a change event with the given change type.
    fn from_change_entity(change: FacilityOperation, entity: ChangeEntity) -> Self {
        match change {
            FacilityOperation::New => Self::New(entity),
            FacilityOperation::Changed => Self::Changed(entity),
            FacilityOperation::Removed => Self::Removed(entity),
        }
    }

    /// Attempts to unwrap the inner [`ChangeEntity`] into a `ChangeEvent`
    /// directly wrapping the specified audio graph entity type, returning
    /// `None` if the wrapped entity is not of the requested type.
    fn try_unwrap<I: EntityInfo>(self) -> Option<ChangeEvent<AudioEntity<I>>> {
        self.map(|entity| I::from_change_entity(entity)).transpose()
    }
}

// TRAIT IMPLS *****************************************************************
impl From<OwnedSinkInfo> for ChangeEntity {
    fn from(info: OwnedSinkInfo) -> Self {
        Self::Sink(AudioEntity::Info(info))
    }
}

impl From<OwnedSinkInputInfo> for ChangeEntity {
    fn from(info: OwnedSinkInputInfo) -> Self {
        Self::SinkInput(AudioEntity::Info(info))
    }
}

#[async_trait]
impl EntityInfo for OwnedSinkInfo {
    async fn lookup(introspect: &AsyncIntrospector, idx: u32) -> Result<Self, PulseFailure> {
        introspect.get_sink_by_index(idx).await
    }

    async fn lookup_all(introspect: &AsyncIntrospector) -> Result<Vec<Self>, PulseFailure> {
        introspect.get_sinks().await
    }

    fn from_change_entity(change_entity: ChangeEntity) -> Option<AudioEntity<Self>> {
        if let ChangeEntity::Sink(entity) = change_entity {
            Some(entity)
        } else {
            None
        }
    }

    fn index(&self) -> u32 {
        self.index
    }

    fn to_entity_type() -> Facility {
        Facility::Sink
    }
}

#[async_trait]
impl EntityInfo for OwnedSinkInputInfo {
    async fn lookup(introspect: &AsyncIntrospector, idx: u32) -> Result<Self, PulseFailure> {
        introspect.get_sink_input(idx).await
    }

    async fn lookup_all(introspect: &AsyncIntrospector) -> Result<Vec<Self>, PulseFailure> {
        introspect.get_sink_inputs().await
    }

    fn from_change_entity(change_entity: ChangeEntity) -> Option<AudioEntity<Self>> {
        if let ChangeEntity::SinkInput(entity) = change_entity {
            Some(entity)
        } else {
            None
        }
    }

    fn index(&self) -> u32 {
        self.index
    }

    fn to_entity_type() -> Facility {
        Facility::SinkInput
    }
}

impl Display for ChangeEntity {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        write!(
            f,
            "{:?} (index {})",
            self.facility(),
            self.index()
        )
    }
}

impl<I: EntityInfo> Display for AudioEntity<I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        write!(
            f,
            "{:?} (index {})",
            I::to_entity_type(),
            self.index()
        )
    }
}

impl<E: Display> Display for ChangeEvent<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        write!(
            f,
            "{} {}",
            self.name(),
            self.entity()
        )
    }
}
