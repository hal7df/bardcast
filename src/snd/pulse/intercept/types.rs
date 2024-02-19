///! Concrete implementations of the [`Interceptor`] and [`LimitingInterceptor`]
///! traits.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};

use async_trait::async_trait;
use libpulse_binding::error::Code;
use log::{debug, info, warn};

use super::super::context::{AsyncIntrospector, PulseContextWrapper};
use super::super::context::stream::{SampleConsumer, StreamManager};
use super::super::owned::{OwnedSinkInfo, OwnedSinkInputInfo};
use super::util;

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

    /// Updates the stream metadata for the given intercepted stream. This may
    /// cork or uncork the stream depending on the cork state of all intercepted
    /// applications.
    async fn update_intercept(
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

    /// Determines whether the consuming end of the stream has closed.
    fn stream_closed(&self) -> bool;

    /// Gracefully closes any system resources that were created by the
    /// `Interceptor`, returning any intercepted streams to another audio device
    /// if needed.
    async fn close(mut self);
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

        debug!("SingleInputMonitor initialized");

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

        debug!("CapturingInterceptor initialized");

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
        warn!(
            "There may be significant latency (1-5s) when intercepting \
             multiple applications with monitor mode. For lower latency, \
             consider using capture mode, or setting an intercept limit of 1 \
             using -n/--intercept-limit."
        );

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

        debug!("DuplexingInterceptor initialized");

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

impl<I> QueuedInterceptor<I> {
    /// Searches the queue for the first uncorked sink input, removes it, and
    /// returns it. If there are no uncorked sink inputs, or the queue is empty,
    /// returnes `None`.
    fn get_top_uncorked_input(&mut self) -> Option<OwnedSinkInputInfo> {
        let next_idx = self.queue.iter().enumerate()
            .find_map(|(i, input)| if !input.corked { Some(i) } else { None });

        if let Some(next_idx) = next_idx {
            self.queue.remove(next_idx)
        } else {
            None
        }
    }
}

impl<I: LimitingInterceptor> QueuedInterceptor<I> {
    /// Finds the first uncorked sink input in the queue and passes it to the
    /// underlying interceptor. If the interceptor is at capacity, the sink
    /// input will be requeued at the end.
    async fn attempt_queued_intercept(&mut self) -> Result<(), Code> {
        if let Some(next_source) = self.get_top_uncorked_input() {
            info!(
                "Attempting to intercept previously queued application `{}'",
                next_source
            );

            match self.record(Cow::Owned(next_source)).await {
                Ok(()) => Ok(()),
                Err(InterceptError::PulseError(e)) => Err(e),
                Err(InterceptError::AtCapacity(..)) => unreachable!(
                    "QueuedInterceptor::record() should never return \
                     InterceptError::AtCapacity"
                ),
            }
        } else {
            Ok(())
        }
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

    fn try_from(err: InterceptError<'a>) -> Result<Self, Self::Error> {
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

/// Due to lifetime constraints, `InterceptError` cannot correctly implement
/// [`Error::source()`]. To get a [`Code`] from an `InterceptError`, use
/// [`Code::try_from`].
impl Error for InterceptError<'_> {}

impl<I: LimitingInterceptor> From<I> for QueuedInterceptor<I> {
    fn from(interceptor: I) -> Self {
        debug!(
            "QueuedInterceptor initialized (limit: {:?})",
            interceptor.capacity()
        );
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

        self.stream_manager.set_cork(
            self.intercepts.values().all(|intercept| intercept.corked)
        ).await?;

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
            self.intercepts.values().all(|intercept| intercept.corked)
        ).await
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(intercept) = self.intercepts.get(&source_idx) {
            let target_sink = util::get_sink_or_default(
                &self.introspect,
                intercept.orig_sink
            ).await?;

            match checked_move(
                &self.introspect,
                source_idx,
                target_sink.index
            ).await {
                Ok(_) => {},
                Err(Code::NoEntity) => info!(
                    "Could not restore sink configuration for application with
                     index {} as it no longer exists",
                    source_idx
                ),
                Err(e) => return Err(e),
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

        self.stream_manager.set_cork(
            self.intercepts.values().all(|intercept| intercept.corked)
        ).await?;

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
            self.intercepts.values().all(|intercept| intercept.corked)
        ).await
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(intercept) = self.intercepts.get(&source_idx) {
            let target_sink = util::get_sink_or_default(
                &self.introspect,
                intercept.orig_sink
            ).await?;

            match checked_move(
                &self.introspect,
                source_idx,
                target_sink.index
            ).await {
                Ok(_) => {},
                Err(Code::NoEntity) => info!(
                    "Could not restore sink configuration for application with \
                     index {} as it no longer exists",
                    source_idx
                ),
                Err(e) => return Err(e)
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
        // Send corked streams straight to the queue if they're corked. The user
        // probably doesn't want corked streams using up interceptor capacity.
        if source.corked {
            info!(
                "Not intercepting matching application `{}', as it is not \
                 currently playing audio. It will be queued and intercepted \
                 later when it starts playing audio, and the interceptor has \
                 capacity.",
                source
            );
            self.queue.push_back(source.into_owned());
            return Ok(());
        }

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

        self.inner.update_intercept(source).await?;

        // If there is capacity and the change is due to an application
        // uncorking, then allow it to be intercepted
        if !self.inner.is_full() {
            self.attempt_queued_intercept().await
        } else {
            Ok(())
        }
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
            self.attempt_queued_intercept().await.map(|_| true)
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
async fn checked_move(
    introspect: &AsyncIntrospector,
    input_idx: u32,
    target_sink_idx: u32
) -> Result<(), Code> {
    // We don't care about Ok() value of the result here, this just checks
    // whether the sink input exists
    introspect.get_sink_input(input_idx).await?;

    introspect.move_sink_input_by_index(input_idx, target_sink_idx).await
}

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
