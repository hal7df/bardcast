///! Types responsible for establishing the event subscription with the server,
///! and creating [`super::listen::EventListener`] instances.

extern crate libpulse_binding as libpulse;

use std::mem;
use std::sync::Arc;

use async_ringbuf::{AsyncConsumer, AsyncRb};
use log::{debug, warn};
use ringbuf::HeapRb;
use tokio::sync::broadcast::{
    self,
    Receiver as BroadcastReceiver,
    Sender as BroadcastSender
};
use tokio::sync::watch::Receiver as WatchReceiver;
use tokio::task::JoinHandle;

use libpulse::context::subscribe::{
    Facility,
    InterestMaskSet,
    Operation as FacilityOperation
};
use libpulse::error::Code;

use crate::util::task::AbortingJoinHandle;
use super::*;
use super::error::{MissingEventField, ResolveError};
use super::listen;
use super::super::PulseFailure;
use super::super::context::{AsyncIntrospector, PulseContextWrapper};
use super::super::owned::{OwnedSinkInputInfo, OwnedSinkInfo};

/// The maximum length of the event notification queue, for both the internal
/// and external queues.
const NOTIFY_QUEUE_LENGTH: usize = 16;

// TYPE DEFINITIONS ************************************************************
/// Lightweight wrapper around the data in the raw libpulse notifications.
#[derive(Clone, Debug)]
struct RawChangeEvent(Facility, FacilityOperation, u32);

/// State data for [`EventListenerFactory`] while the factory has not yet
/// stood up the event notification mechanism.
struct UninitializedFactoryState {
    ctx: PulseContextWrapper,
    shutdown_rx: WatchReceiver<bool>,
}

/// State data for [`EventListenerFactory`] after it has established the
/// server event subscription.
struct InitializedFactoryState {
    rx: BroadcastReceiver<Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>>,
    lookup_task: AbortingJoinHandle<()>,
    ctx: PulseContextWrapper,
    entity_types: InterestMaskSet,
}

/// Enum representing the current state of the factory, wrapping relevant state
/// data.
enum FactoryState {
    /// The factory has not yet established an event subscription with the
    /// server, and is currently being configured.
    Uninitialized(UninitializedFactoryState),

    /// The factory is actively establishing an event subscription with the
    /// server.
    Initializing,

    /// The factory has an established event subscription with the server.
    Initialized(InitializedFactoryState),
}

/// Factory for creating listeners for audio graph entity change events,
/// providing a more ergonomic interface to
/// [`libpulse::context::Context::subscribe`].
///
/// Generally, there should only be one `EventListenerFactory` instance per
/// [`PulseContextWrapper`] instance, as the factory assumes that it has sole
/// control over the context's notification mechanism; undesirable race
/// conditions may occur if this is not true.
pub struct EventListenerFactory(FactoryState);

/// Type alias for the internal event queue consumer.
type RawEventConsumer = AsyncConsumer<RawChangeEvent, Arc<AsyncRb<RawChangeEvent, HeapRb<RawChangeEvent>>>>;

// TYPE IMPLS ******************************************************************
impl RawChangeEvent {
    /// Attempts to create a `RawChangeEvent` from the raw data returned by a
    /// libpulse event notification.
    fn try_new(
        facility: Option<Facility>,
        change: Option<FacilityOperation>,
        index: u32
    ) -> Result<Self, MissingEventField> {
        Ok(Self(
            facility.ok_or(MissingEventField::Facility)?,
            change.ok_or(MissingEventField::ChangeType)?,
            index
        ))
    }

    /// Creates a [`ResolveError`] for the entity represented by this event.
    fn unsupported(&self) -> ResolveError {
        ResolveError::UnsupportedFacility(self.0, self.2)
    }
}

impl UninitializedFactoryState {
    /// Establishes an event subscription with the server, spawning a task to
    /// handle metadata lookup, returning the initialized state data.
    async fn init(self) -> InitializedFactoryState {
        let (tx, rx) = broadcast::channel::<Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>>(
            NOTIFY_QUEUE_LENGTH
        );

        let raw_rx = set_subscribe_callback(&self.ctx).await;
        let lookup_task = AbortingJoinHandle::from(tokio::spawn(lookup_broadcast_loop(
            self.shutdown_rx,
            raw_rx,
            tx,
            AsyncIntrospector::from(&self.ctx)
        )));

        InitializedFactoryState {
            rx,
            lookup_task,
            ctx: self.ctx,
            entity_types: InterestMaskSet::NULL,
        }
    }
}

impl InitializedFactoryState {
    /// Checks if the given entity type(s) are included in the context's current
    /// event subscription, and adds them if not.
    async fn listen_for_entity_types(
        &mut self,
        entity_types: InterestMaskSet
    ) -> Result<(), PulseFailure> {
        if !self.entity_types.contains(entity_types) {
            self.entity_types |= entity_types;
            subscribe(&self.ctx, self.entity_types).await
        } else {
            Ok(())
        }
    }
}

impl FactoryState {
    /// Initializes the factory's event subscription if it has not yet been
    /// established, doing nothing otherwise. Returns initialized state data.
    async fn init(self) -> Self {
        if let FactoryState::Uninitialized(state) = self {
            FactoryState::Initialized(state.init().await)
        } else {
            self
        }
    }
}

impl EventListenerFactory {
    /// Initializes the factory's event subscription if it has not yet been
    /// established, doing nothing otherwise.
    async fn init(&mut self) {
        if matches!(self.0, FactoryState::Uninitialized(_)) {
            self.0 = mem::replace(
                &mut self.0,
                FactoryState::Initializing
            ).init().await;
        }
    }

    /// Creates a new factory against the given context. THe internal entity
    /// lookup task will shut down upon signal from `shutdown_rx`.
    pub fn new(
        ctx: &PulseContextWrapper,
        shutdown_rx: WatchReceiver<bool>
    ) -> EventListenerFactory {
        Self(FactoryState::Uninitialized(UninitializedFactoryState {
            ctx: ctx.clone(),
            shutdown_rx,
        }))
    }

    /// Creates an `EventListener` instance that listens for events relating to
    /// the given entity type.
    pub async fn build<I: EntityInfo>(&mut self) -> Result<OwnedEventListener<I>, PulseFailure> {
        self.init().await;

        if let FactoryState::Initialized(state) = &mut self.0 {
            state.listen_for_entity_types(I::to_entity_type().to_interest_mask()).await?;

            listen::new_listener(
                AsyncIntrospector::from(&state.ctx),
                state.rx.resubscribe()
            ).await
        } else {
            panic!("EventListenerFactory lifecycle error");
        }
    }

    /// Consumes this factory, returning a handle to the internal lookup task
    /// if the factory was initialized.
    pub fn consume_task(self) -> Option<JoinHandle<()>> {
        if let FactoryState::Initialized(mut state) = self.0 {
            state.lookup_task.take()
        } else {
            None
        }
    }
}

// TRAIT IMPLS *****************************************************************
impl TryFrom<&RawChangeEvent> for ChangeEntity {
    type Error = PulseFailure;

    fn try_from(event: &RawChangeEvent) -> Result<Self, Self::Error> {
        match event.0 {
            Facility::Sink => Ok(Self::Sink(AudioEntity::Index(event.2))),
            Facility::SinkInput => Ok(Self::SinkInput(AudioEntity::Index(event.2))),
            _ => Err(PulseFailure::from(Code::NotSupported))
        }
    }
}

// HELPER FUNCTIONS ************************************************************

/// Sets the subscribe callback on the context to forward events into the
/// returned channel as [`RawChangeEvent`]s.
async fn set_subscribe_callback(ctx_wrap: &PulseContextWrapper) -> RawEventConsumer {
    let (mut tx, rx) = AsyncRb::<RawChangeEvent, HeapRb<RawChangeEvent>>::new(
        NOTIFY_QUEUE_LENGTH
    ).split();

    ctx_wrap.with_ctx(move |ctx| {
        ctx.set_subscribe_callback(Some(Box::new(move |facility, change, index| {
            let change_event = RawChangeEvent::try_new(facility, change, index);

            if let Ok(change_event) = change_event {
                if let Err(change_event) = tx.as_mut_base().push(change_event) {
                    warn!(
                        "Server {:?} event for {:?} with index {} was ignored as events are no longer being consumed",
                        change_event.1,
                        change_event.0,
                        change_event.2
                    );
                }
            } else {
                warn!("Context event for entity with index {} missing required metadata", index);
            }
        })));
    }).await;

    rx
}

/// Sets the context's current subscription to cover the given types.
///
/// This can be called multiple times for the same context; subsequent calls
/// will replace the previous subscription with the new subscription.
async fn subscribe(
    ctx_wrap: &PulseContextWrapper,
    entity_types: InterestMaskSet
) -> Result<(), PulseFailure> {
    ctx_wrap.do_ctx_op_default(move |ctx, result| {
        ctx.subscribe(entity_types, move |success| *result.lock().unwrap() = success)
    }).await.map_err(
        |_| PulseFailure::Error(Code::Killed)
    ).and_then(|subscribe_result| if subscribe_result {
            debug!("Set server entity subscription.");
            Ok(())
    } else {
        error!("Failed to set server entity subscription");
        Err(PulseFailure::Error(Code::BadState))
    })
}

/// Attempts to fetch the entity info for the given raw event, returning it as
/// a [`ChangeEntity`] instance upon success.
async fn resolve_entity(
    event: &RawChangeEvent,
    introspect: &AsyncIntrospector,
) -> Result<ChangeEntity, PulseFailure> {
    match event.0 {
        Facility::Sink => Ok(ChangeEntity::Sink(AudioEntity::Info(
            OwnedSinkInfo::lookup(introspect, event.2).await?
        ))),
        Facility::SinkInput => Ok(ChangeEntity::SinkInput(AudioEntity::Info(
            OwnedSinkInputInfo::lookup(introspect, event.2).await?
        ))),
        _ => Err(PulseFailure::from(Code::NotSupported)),
    }
}

/// Converts a [`RawChangeEvent`] into a [`ChangeEvent`] that is more easily
/// consumed by event listeners.
async fn resolve_event(
    event: &RawChangeEvent,
    introspect: &AsyncIntrospector
) -> Result<ChangeEvent<ChangeEntity>, ResolveError> {
    if event.1 != FacilityOperation::Removed {
        resolve_entity(event, introspect).await.map_err(|err| {
            if let Ok(raw_entity) = ChangeEntity::try_from(event) {
                ResolveError::LookupError(LookupError::new(
                    err,
                    ChangeEvent::from_change_entity(event.1, raw_entity)
                ))
            } else {
                warn!("Entity lookup failed on unsupported entity type '{:?}': {}", event.0, err);
                event.unsupported()
            }
        })
    } else {
        ChangeEntity::try_from(event).map_err(|_| event.unsupported())
    }.map(|entity| ChangeEvent::from_change_entity(event.1, entity))
}

/// Helper function for broadcasting a server change event to all event
/// listeners.
fn do_change_broadcast(
    event: Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>,
    tx: &BroadcastSender<Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>>
) {
    if tx.send(event).is_err() {
        warn!("Failed to handle change event, no active listeners");
    }
}

/// Implementation of the internal event resolution task.
///
/// The task will shut down when a signal is received on `shutdown_rx`.
async fn lookup_broadcast_loop(
    mut shutdown_rx: WatchReceiver<bool>,
    mut raw_rx: RawEventConsumer,
    tx: BroadcastSender<Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>>,
    introspect: AsyncIntrospector
) {
    loop {
        tokio::select! {
            biased;
            shutdown = shutdown_rx.changed() => if shutdown.is_err() || !*shutdown_rx.borrow_and_update() {
                break;
            },
            event = raw_rx.pop() => if let Some(event) = event {
                match resolve_event(&event, &introspect).await {
                    Ok(event) => do_change_broadcast(Ok(event), &tx),
                    Err(ResolveError::LookupError(e)) => do_change_broadcast(Err(e), &tx),
                    Err(ResolveError::UnsupportedFacility(entity_type, idx)) => debug!(
                        "Skipping event for unsupported entity type {:?} (index {})",
                        entity_type,
                        idx
                    ),
                }
            } else {
                warn!("Server event listener unexpectedly dropped, terminating lookup task");
                break;
            }
        }
    }

    debug!("Server event listener shut down");
}
