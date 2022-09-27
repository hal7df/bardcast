///! Listener types for consuming audio graph change events from the PulseAudio
///! server.

use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;

use super::*;
use super::super::context::AsyncIntrospector;

// TYPE DEFINITIONS ************************************************************
/// Core trait for all event listeners.
#[async_trait]
pub trait EventListener<I: EntityInfo, T>: Send {
    /// Fetches the next event returned by the server.
    async fn next(&mut self) -> Result<T, EventListenerError<AudioEntity<I>>>;

    /// Fetches the next event returned by the server, ignoring any dropped
    /// messages due to the consumer lagging.
    async fn next_ignore_lag(&mut self) -> Result<T, EventListenerError<AudioEntity<I>>>;
}

/// Trait implemented by event listeners that support filtering and mapping
/// events.
pub trait ToFilterMapped<I: EntityInfo> {
    /// The type of the event listener after filtering.
    type Filtered<'a, T>: EventListener<I, T> + 'a where I: 'a, T: 'a;

    /// Applies a filter operation and a map operation to received events in
    /// one step.
    fn filter_map<'a, T, F>(
        self,
        filter_map: F
    ) -> Self::Filtered<'a, T>
    where F: FnMut(ChangeEvent<AudioEntity<I>>) -> Option<T> + Send + 'a;
}

/// Basic event listener implementation that has a single owner.
pub struct OwnedEventListener<I: EntityInfo> {
    existing: Vec<I>,
    rx: Receiver<Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>>,
}

/// Basic implementation of an owned event listener that has had a filter and/or
/// map operation applied.
pub struct FilterMapEventListener<'a, I: EntityInfo, T>(
    Box<dyn EventListener<I, ChangeEvent<AudioEntity<I>>> + 'a>,
    Box<dyn FnMut(ChangeEvent<AudioEntity<I>>) -> Option<T> + Send + 'a>
);

// TYPE IMPLS ******************************************************************
impl<I: EntityInfo> OwnedEventListener<I> {
    /// Creates a new event listener concerned with audio entities of the given
    /// type, populating the initial set of notifications with the current set
    /// of entities present in the audio graph.
    // Using this function to create new EventListener instances forces the
    // caller to create a new rx handle *before* we look up the currently
    // existing entities. This ensures that entities that may be created while
    // we are creating the EventListener are caught by at least one of the
    // entity lookup or the subscription.
    async fn new(
        introspect: AsyncIntrospector,
        rx: Receiver<Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>>
    ) -> Result<Self, PulseFailure> {
        Ok(Self {
            existing: I::lookup_all(&introspect).await?,
            rx,
        })
    }
}

// TRAIT IMPLS *****************************************************************
#[async_trait]
impl<I: EntityInfo> EventListener<I, ChangeEvent<AudioEntity<I>>> for OwnedEventListener<I> {
    async fn next(&mut self) -> Result<ChangeEvent<AudioEntity<I>>, EventListenerError<AudioEntity<I>>> {
        if let Some(info) = self.existing.pop() {
            return Ok(ChangeEvent::New(AudioEntity::Info(info)));
        } else {
            loop {
                match self.rx.recv().await? {
                    Ok(event) => if let Some(event) = event.try_unwrap() {
                        return Ok(event);
                    },
                    Err(e) => if let Some(e) = e.try_map_entity() {
                        return Err(EventListenerError::LookupError(e));
                    },
                }
            }
        }
    }

    async fn next_ignore_lag(&mut self) -> Result<ChangeEvent<AudioEntity<I>>, EventListenerError<AudioEntity<I>>> {
        loop {
            let event = self.next().await;

            if let Err(EventListenerError::ChannelError(RecvError::Lagged(lag))) = &event {
                warn!("Event listener missed {} events", lag);
            } else {
                return event;
            }
        }
    }
}

#[async_trait]
impl<'a, I: EntityInfo, T> EventListener<I, T> for FilterMapEventListener<'a, I, T> {
    async fn next(&mut self) -> Result<T, EventListenerError<AudioEntity<I>>> {
        loop {
            let event = self.0.next().await;
            match do_filter_map(event, &mut self.1) {
                Some(event) => return event,
                None => (), //loop until we find an event matching the filter
            }
        }
    }

    async fn next_ignore_lag(&mut self) -> Result<T, EventListenerError<AudioEntity<I>>> {
        loop {
            let event = self.0.next_ignore_lag().await;
            match do_filter_map(event, &mut self.1) {
                Some(event) => return event,
                None => (), //loop until we find an event matching the filter
            }
        }
    }
}

impl<I: EntityInfo + 'static> ToFilterMapped<I> for OwnedEventListener<I> {
    type Filtered<'a, T> = FilterMapEventListener<'a, I, T> where T: 'a;

    fn filter_map<'a, T, F>(
        self,
        filter_map: F
    ) -> FilterMapEventListener<'a, I, T>
    where F: FnMut(ChangeEvent<AudioEntity<I>>) -> Option<T> + Send + 'a {
        FilterMapEventListener(Box::new(self), Box::new(filter_map))
    }
}

// HELPER FUNCTIONS ************************************************************
/// Alias for [`EventListener::new`] that is accessible outside of the listen
/// module.
pub async fn new_listener<I: EntityInfo>(
    introspect: AsyncIntrospector,
    rx: Receiver<Result<ChangeEvent<ChangeEntity>, LookupError<ChangeEntity>>>
) -> Result<OwnedEventListener<I>, PulseFailure> {
    OwnedEventListener::new(introspect, rx).await
}

/// Core logic for applying the filter + map operation.
fn do_filter_map<I: EntityInfo, T, F>(
    event: Result<ChangeEvent<AudioEntity<I>>, EventListenerError<AudioEntity<I>>>,
    filter_map: &mut F
) -> Option<Result<T, EventListenerError<AudioEntity<I>>>>
where F: FnMut(ChangeEvent<AudioEntity<I>>) -> Option<T> {
    match event {
        Ok(event) => if let Some(val) = filter_map(event) {
            Some(Ok(val))
        } else {
            None
        },
        Err(EventListenerError::LookupError(e)) => if let Some(_) = filter_map(e.raw_event().clone()) {
            Some(Err(EventListenerError::LookupError(e)))
        } else {
            None
        },
        Err(e) => Some(Err(e)),
    }
}
