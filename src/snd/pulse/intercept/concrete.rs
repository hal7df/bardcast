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

struct InputState {
    orig_sink: u32,
    corked: bool,
}

pub struct SingleInputMonitor {
    stream_manager: StreamManager,
    captured: Option<OwnedSinkInputInfo>,
    volume: Option<f64>,
}

/// Implementation of [`Interceptor`] that captures application audio in a
/// dedicated audio sink, preventing it from reaching any hardware audio
/// devices.
pub struct CapturingInterceptor {
    rec: OwnedSinkInfo,
    stream_manager: StreamManager,
    captures: HashMap<u32, InputState>,
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
    captures: HashMap<u32, InputState>,
    introspect: AsyncIntrospector,
    volume: Option<f64>,
    limit: Option<usize>,
}

pub struct QueuedInterceptor<I> {
    inner: I,
    queue: VecDeque<OwnedSinkInputInfo>,
}

/// TYPE IMPLS *****************************************************************
impl SingleInputMonitor {
    pub async fn new(
        ctx: &PulseContextWrapper,
        volume: Option<f64>
    ) -> (Self, SampleConsumer) {
        let (stream_manager, stream) = StreamManager::new(ctx);

        (Self {
            stream_manager,
            captured: None,
            volume,
        }, stream)
    }
}

impl CapturingInterceptor {
    /// Creates a new `CapturingInterceptor`.
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
            captures: HashMap::new(),
            introspect,
            volume,
            limit
        }, stream))
    }
}

impl DuplexingInterceptor {
    /// Creates a new `DuplexingInterceptor`, using the given sink as the second
    /// output.
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
            captures: HashMap::new(),
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
        if self.captured.is_some() {
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

        self.captured = Some(source);

        info!("Started monitor of application `{}'", source);

        Ok(())
    }

    async fn update_capture(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        if let Some(captured) = self.captured {
            if captured.index == source.index && captured.corked != source.corked {
                captured.corked = source.corked;
                self.stream_manager.set_cork(captured.corked).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(captured) = self.captured {
            if source_idx != captured.index {
                return Ok(false);
            }

            debug!("Stopping PulseAudio stream");
            self.stream_manager.stop().await;
            info!("Stopped monitor of application `{}'", captured);
            self.captured = None;

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
        if self.captured.is_some() { 1usize } else { 0usize }
    }

    async fn close(mut self) {
        self.stream_manager.stop().await;
    }

    async fn boxed_close(mut self: Box<Self>) {
        self.close().await;
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

        self.captures.insert(source.index, InputState {
            orig_sink: source.as_ref().sink,
            corked: source.as_ref().corked,
        });

        info!("Intercepted application `{}'", source);

        Ok(())
    }

    async fn update_capture(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        if let Some(capture) = self.captures.get_mut(&source.index) {
            if capture.corked == source.corked {
                return Ok(());
            }

            capture.corked = source.corked;
        } else {
            return Ok(());
        }

        self.stream_manager.set_cork(
            self.captures.values().any(|capture| capture.corked)
        ).await
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(capture) = self.captures.get(&source_idx) {
            let target_sink = util::get_sink_or_default(
                &self.introspect,
                capture.orig_sink
            ).await?;

            self.introspect.move_sink_input_by_index(
                source_idx,
                target_sink.index
            ).await?;

            self.captures.remove(&source_idx);

            info!("Stopped intercept of application with index {}", source_idx);

            if self.captures.is_empty() {
                debug!("Stopping PulseAudio stream");
                self.stream_manager.stop();
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
        self.captures.len()
    }

    async fn close(mut self) {
        interceptor_stop_all(
            &mut self,
            self.captures.keys().copied().collect::<Vec<u32>>()
        ).await;
        util::tear_down_virtual_sink(&self.introspect, &self.rec, None).await;
    }

    async fn boxed_close(mut self: Box<Self>) {
        self.close().await;
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

        self.captures.insert(source.index, InputState {
            orig_sink: source.as_ref().sink,
            corked: source.as_ref().corked,
        });

        info!("Intercepted application `{}'", source);

        Ok(())
    }

    async fn update_capture(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        if let Some(capture) = self.captures.get_mut(&source.index) {
            if capture.corked == source.corked {
                return Ok(());
            }

            capture.corked = source.corked;
        } else {
            return Ok(());
        }

        self.stream_manager.set_cork(
            self.captures.values().any(|capture| capture.corked)
        ).await
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(capture) = self.captures.get(&source_idx) {
            let target_sink = util::get_sink_or_default(
                &self.introspect,
                capture.orig_sink
            ).await?;

            self.introspect.move_sink_input_by_index(
                source_idx,
                target_sink.index
            ).await?;

            self.captures.remove(&source_idx);

            info!("Stopped intercept of application with index {}", source_idx);

            if self.captures.is_empty() {
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
        self.captures.len()
    }

    async fn close(mut self) {
        interceptor_stop_all(
            &mut self,
            self.captures.keys().copied().collect::<Vec<u32>>()
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

    async fn boxed_close(mut self: Box<Self>) {
        self.close().await;
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

    async fn update_capture(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), Code> {
        self.queue.iter_mut()
            .filter(|queued| queued.index == source.index)
            .for_each(|queued| *queued = source.clone());

        self.inner.update_capture(source).await
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

    async fn close(mut self) {
        self.inner.close().await
    }

    async fn boxed_close(mut self: Box<Self>) {
        self.close().await;
    }
}

// UTILITY FUNCTIONS ***********************************************************
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
