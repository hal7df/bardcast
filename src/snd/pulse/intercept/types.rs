///! Concrete implementations of the [`Interceptor`] and [`LimitingInterceptor`]
///! traits.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use libpulse_binding::error::Code;
use log::{debug, info, warn};

use super::super::context::{AsyncIntrospector, PulseContextWrapper};
use super::super::context::stream::{SampleConsumer, StreamManager};
use super::super::owned::{OwnedSinkInfo, OwnedSinkInputInfo};
use super::util;
use super::{InterceptError, Interceptor, LimitingInterceptor};

/// Caches the original sink ID and cork state of an intercepted audio stream.
struct InputState {
    orig_sink: u32,
    corked: bool,
}

/// Implementation of [`Interceptor`] that makes use of 
/// `Stream::set_monitor_stream()` to directly monitor a single application
/// without the need for creating virtual sinks. This inherently only allows
/// a single application to be intercepted at a time.
pub struct SingleInputMonitor {
    stream_manager: StreamManager,
    intercepted: Option<OwnedSinkInputInfo>,
    volume: Option<f64>,
}

/// Implementation of [`Interceptor`] that captures application audio in a
/// dedicated audio sink, preventing it from reaching any hardware audio
/// devices.
pub struct CapturingInterceptor {
    rec: OwnedSinkInfo,
    stream_manager: StreamManager,
    intercepts: HashMap<u32, InputState>,
    introspect: AsyncIntrospector,
    volume: Option<f64>,
    limit: Option<usize>,
}

/// Implementation of [`Interceptor`] that duplicates application audio streams
/// before reading them to allow the audio to reach a hardware sink in addition
/// to being read by bardcast.
pub struct DuplexingInterceptor {
    demux: OwnedSinkInfo,
    rec: OwnedSinkInfo,
    orig: OwnedSinkInfo,
    stream_manager: StreamManager,
    intercepts: HashMap<u32, InputState>,
    introspect: AsyncIntrospector,
    volume: Option<f64>,
    limit: Option<usize>,
}

/// Wrapper implementation of [`Interceptor`] that queues intercepted inputs
/// that would otherwise cause the inner [`LimitingInterceptor`] implementation
/// to exceed its limit. Queued inputs are dequeued when inputs that were
/// previously intercepted by the wrapped interceptor are removed.
pub struct QueuedInterceptor<I> {
    inner: I,
    queue: VecDeque<OwnedSinkInputInfo>,
}

/// TYPE IMPLS *****************************************************************
impl SingleInputMonitor {
    /// Creates a new `SingleInputMonitor` instance against the given PulseAudio
    /// context that initializes new streams at the given volume.
    pub async fn new(
        ctx: &PulseContextWrapper,
        volume: Option<f64>
    ) -> (Self, SampleConsumer) {
        let (stream_manager, stream) = StreamManager::new(ctx);

        (Self {
            stream_manager,
            intercepted: None,
            volume,
        }, stream)
    }
}

impl CapturingInterceptor {
    /// Creates a new `CapturingInterceptor` instance against the given
    /// PulseAudio context. An optional limit to the number of simultaneously
    /// intercepted streams can be provided.
    ///
    /// The recording stream will be initialized to the provided normalized
    /// volume, if any.
    pub async fn new(
        ctx: &PulseContextWrapper,
        volume: Option<f64>,
        limit: Option<usize>
    ) -> Result<(Self, SampleConsumer), Code> {
        let introspect = AsyncIntrospector::from(ctx);
        let rec = util::create_rec_sink(&introspect).await?;
        debug!("Created rec sink at index {}", rec.index);

        let (stream_manager, stream) = StreamManager::new(ctx);

        Ok((Self {
            rec,
            stream_manager,
            intercepts: HashMap::new(),
            introspect,
            volume,
            limit
        }, stream))
    }
}

impl DuplexingInterceptor {
    /// Creates a new `DuplexingInterceptor` instance against the given
    /// PulseAudio context, routing audio to the given sink in addition to the
    /// internally-created record sink. An optional limit to the number of
    /// simultaneously intercepted streams can be provided.
    pub async fn from_sink(
        ctx: &PulseContextWrapper,
        sink: &OwnedSinkInfo,
        volume: Option<f64>,
        limit: Option<usize>
    ) -> Result<(Self, SampleConsumer), Code> {
        let introspect = AsyncIntrospector::from(ctx);
        let rec = util::create_rec_sink(&introspect).await?;
        debug!("Created rec sink at index {}", rec.index);

        let demux = util::tear_down_module_on_failure(
            &introspect,
            rec.owner_module,
            util::create_demux_sink(
                &introspect,
                &[&rec, &sink],
            ).await
        ).await?;
        debug!("Created demux sink at index {}", demux.index);

        let (stream_manager, stream) = StreamManager::new(ctx);

        Ok((Self {
            demux,
            rec,
            orig: sink.clone(),
            stream_manager,
            intercepts: HashMap::new(),
            introspect,
            volume,
            limit
        }, stream))
    }
}

// TRAIT IMPLS *****************************************************************
impl<I: LimitingInterceptor> From<I> for QueuedInterceptor<I> {
    fn from(interceptor: I) -> Self {
        Self {
            inner: interceptor,
            queue: VecDeque::new(),
        }
    }
}

#[async_trait]
impl Interceptor for SingleInputMonitor {
    async fn record<'a>(
        &mut self,
        source: Cow<'a, OwnedSinkInputInfo>
    ) -> Result<(), InterceptError<'a>> {
        if self.intercepted.is_some() {
            return Err(InterceptError::AtCapacity(1usize, source));
        }

        let source = source.into_owned();

        // Make sure there isn't a running stream already before starting it
        debug!(
            "Starting new PulseAudio stream targeting application `{}'",
            source
        );
        self.stream_manager.stop().await;
        self.stream_manager.start(source.clone(), self.volume).await?;
        
        info!("Started monitor of application `{}'", source);

        self.intercepted = Some(source);


        Ok(())
    }

    async fn update_intercept(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        if let Some(intercepted) = self.intercepted.as_mut() {
            if intercepted.index == source.index && intercepted.corked != source.corked {
                intercepted.corked = source.corked;
                self.stream_manager.set_cork(intercepted.corked).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(intercepted) = self.intercepted.as_ref() {
            if source_idx != intercepted.index {
                return Ok(false);
            }

            debug!("Stopping PulseAudio stream");
            self.stream_manager.stop().await;
            info!("Stopped monitor of application `{}'", intercepted);
            self.intercepted = None;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn needs_monitor(&self) -> bool {
        self.stream_manager.is_running()
    }

    async fn monitor(&mut self) {
        self.stream_manager.monitor().await;
    }

    fn len(&self) -> usize {
        if self.intercepted.is_some() { 1usize } else { 0usize }
    }

    fn stream_closed(&self) -> bool {
        self.stream_manager.is_closed()
    }

    async fn close(mut self) {
        self.stream_manager.stop().await;
    }
}

impl LimitingInterceptor for SingleInputMonitor {
    fn capacity(&self) -> Option<usize> {
        Some(1usize)
    }
}

#[async_trait]
impl Interceptor for CapturingInterceptor {
    async fn record<'a>(
        &mut self,
        source: Cow<'a, OwnedSinkInputInfo>
    ) -> Result<(), InterceptError<'a>> {
        if self.is_full() {
            // limit is guaranteed to be Some if is_full() returns true
            return Err(InterceptError::AtCapacity(
                self.limit.unwrap(),
                source
            ));
        }

        if !self.stream_manager.is_running() {
            debug!("Starting PulseAudio stream");
            self.stream_manager.start(
                self.rec.clone(),
                self.volume
            ).await?;
        }

        self.introspect.move_sink_input_by_index(
            source.as_ref().index,
            self.rec.index
        ).await?;

        self.intercepts.insert(source.index, InputState {
            orig_sink: source.as_ref().sink,
            corked: source.as_ref().corked,
        });

        info!("Intercepted application `{}'", source);

        Ok(())
    }

    async fn update_intercept(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        if let Some(intercept) = self.intercepts.get_mut(&source.index) {
            if intercept.corked == source.corked {
                return Ok(());
            }

            intercept.corked = source.corked;
        } else {
            return Ok(());
        }

        self.stream_manager.set_cork(
            self.intercepts.values().any(|intercept| intercept.corked)
        ).await
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(intercept) = self.intercepts.get(&source_idx) {
            let target_sink = util::get_sink_or_default(
                &self.introspect,
                intercept.orig_sink
            ).await?;

            let move_res = self.introspect.move_sink_input_by_index(
                source_idx,
                target_sink.index
            ).await;

            if let Err(e) = move_res {
                if e == Code::NoEntity {
                    info!(
                        "Could not restore sink configuration for application \
                         with index {} as it no longer exists",
                         source_idx
                    );
                } else {
                    return Err(e)
                }
            }

            self.intercepts.remove(&source_idx);

            info!("Stopped intercept of application with index {}", source_idx);

            if self.intercepts.is_empty() {
                debug!("Stopping PulseAudio stream");
                self.stream_manager.stop().await;
            }

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn needs_monitor(&self) -> bool {
        self.stream_manager.is_running()
    }

    async fn monitor(&mut self) {
        self.stream_manager.monitor().await;
    }

    fn len(&self) -> usize {
        self.intercepts.len()
    }

    fn stream_closed(&self) -> bool {
        self.stream_manager.is_closed()
    }

    async fn close(mut self) {
        let intercept_idxs = self.intercepts.keys().copied().collect::<Vec<u32>>();

        interceptor_stop_all(
            &mut self,
            intercept_idxs
        ).await;

        util::tear_down_virtual_sink(&self.introspect, &self.rec, None).await;
    }
}

impl LimitingInterceptor for CapturingInterceptor {
    fn capacity(&self) -> Option<usize> {
        self.limit
    }
}

#[async_trait]
impl Interceptor for DuplexingInterceptor {
    async fn record<'a>(
        &mut self,
        source: Cow<'a, OwnedSinkInputInfo>
    ) -> Result<(), InterceptError<'a>> {
        if self.is_full() {
            // limit is guaranteed to be Some if is_full() returns true
            return Err(InterceptError::AtCapacity(
                self.limit.unwrap(),
                source
            ));
        }

        if !self.stream_manager.is_running() {
            debug!("Starting PulseAudio stream");
            self.stream_manager.start(
                self.rec.clone(),
                self.volume
            ).await?;
        }

        self.introspect.move_sink_input_by_index(
            source.as_ref().index,
            self.demux.index
        ).await?;

        self.intercepts.insert(source.index, InputState {
            orig_sink: source.as_ref().sink,
            corked: source.as_ref().corked,
        });

        info!("Intercepted application `{}'", source);

        Ok(())
    }

    async fn update_intercept(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        if let Some(intercept) = self.intercepts.get_mut(&source.index) {
            if intercept.corked == source.corked {
                return Ok(());
            }

            intercept.corked = source.corked;
        } else {
            return Ok(());
        }

        self.stream_manager.set_cork(
            self.intercepts.values().any(|intercept| intercept.corked)
        ).await
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(intercept) = self.intercepts.get(&source_idx) {
            let target_sink = util::get_sink_or_default(
                &self.introspect,
                intercept.orig_sink
            ).await?;

            let move_res = self.introspect.move_sink_input_by_index(
                source_idx,
                target_sink.index
            ).await;

            if let Err(e) = move_res {
                if e == Code::NoEntity {
                    info!(
                        "Could not restore sink configuration for application \
                         with index {} as it no longer exists",
                         source_idx
                    );
                } else {
                    return Err(e)
                }
            }

            self.intercepts.remove(&source_idx);

            info!("Stopped intercept of application with index {}", source_idx);

            if self.intercepts.is_empty() {
                debug!("Stopping PulseAudio stream");
                self.stream_manager.stop().await;
            }

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn needs_monitor(&self) -> bool {
        self.stream_manager.is_running()
    }

    async fn monitor(&mut self) {
        self.stream_manager.monitor().await;
    }

    fn len(&self) -> usize {
        self.intercepts.len()
    }

    fn stream_closed(&self) -> bool {
        self.stream_manager.is_closed()
    }

    async fn close(mut self) {
        let intercept_idxs = self.intercepts.keys().copied().collect::<Vec<u32>>();

        interceptor_stop_all(
            &mut self,
            intercept_idxs
        ).await;

        //Tear down demux and rec sinks
        util::tear_down_virtual_sink(
            &self.introspect,
            &self.demux,
            Some(&self.orig)
        ).await;
        util::tear_down_virtual_sink(
            &self.introspect,
            &self.rec,
            Some(&self.orig)
        ).await;
    }
}

impl LimitingInterceptor for DuplexingInterceptor {
    fn capacity(&self) -> Option<usize> {
        self.limit
    }
}

#[async_trait]
impl<I: LimitingInterceptor> Interceptor for QueuedInterceptor<I> {
    async fn record<'a>(
        &mut self,
        source: Cow<'a, OwnedSinkInputInfo>
    ) -> Result<(), InterceptError<'a>> {
        match self.inner.record(source).await {
            Ok(()) => Ok(()),
            Err(InterceptError::AtCapacity(n, source)) => {
                info!(
                    "Hit application intercept limit ({}). Queueing \
                     application `{}' for later intercept.",
                     n,
                     source
                );
                self.queue.push_back(source.into_owned());
                Ok(())
            },
            Err(InterceptError::PulseError(e)) => Err(
                InterceptError::PulseError(e)
            ),
        }
    }

    async fn update_intercept(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        self.queue.iter_mut()
            .filter(|queued| queued.index == source.index)
            .for_each(|queued| *queued = source.clone());

        self.inner.update_intercept(source).await
    }

    async fn stop(
        &mut self,
        source_idx: u32
    ) -> Result<bool, Code> {
        self.queue.retain(|source| {
            if source.index == source_idx {
                info!(
                    "Removing application `{}' from intercept queue",
                    source
                );
                false
            } else {
                true
            }
        });

        if self.inner.stop(source_idx).await? {
            if let Some(next_source) = self.queue.pop_front() {
                info!(
                    "Attempting to intercept previously queued application `{}'",
                    next_source
                );

                match self.record(Cow::Owned(next_source)).await {
                    Ok(()) => Ok(true),
                    Err(InterceptError::PulseError(e)) => Err(e),
                    Err(InterceptError::AtCapacity(..)) => unreachable!(
                        "QueuedInterceptor::record() should never return \
                         InterceptError::AtCapacity"
                    ),
                }
            } else {
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }

    fn needs_monitor(&self) -> bool {
        self.inner.needs_monitor()
    }

    async fn monitor(&mut self) {
        self.inner.monitor().await;
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn stream_closed(&self) -> bool {
        self.inner.stream_closed()
    }

    async fn close(mut self) {
        self.inner.close().await
    }
}

// UTILITY FUNCTIONS ***********************************************************
/// Helper function to stop all intercepts on the given [`Interceptor`]
/// implementation.
async fn interceptor_stop_all(
    interceptor: &mut impl Interceptor,
    inputs: impl IntoIterator<Item = u32>
) {
    for input in inputs {
        if let Err(e) = interceptor.stop(input).await {
            warn!(
                "{} (error {}) {}",
                super::SINK_INPUT_MOVE_FAILURE,
                e,
                super::TEARDOWN_FAILURE_WARNING
            );
        }
    }
}
