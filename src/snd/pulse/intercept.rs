///! Utilities for intercepting application audio streams for recording.
///!
///! The core abstraction in this modules is the [`Interceptor`] trait, which
///! allows bardcast to intercept applications playing audio via their sink
///! input ID. When used in conjunction with the event notification
///! functionality of the [`super::event`] module, this allows applications to
///! be efficiently intercepted as soon as they open an audio stream.

extern crate libpulse_binding as libpulse;

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};
use std::process;

use async_trait::async_trait;
use clap::crate_name;
use futures::future::{self, TryFutureExt};
use log::{debug, error, info, warn};

use libpulse::error::Code;

use super::context::{AsyncIntrospector, PulseContextWrapper};
use super::context::stream::{SampleConsumer, StreamManager};
use super::event::EventListener;
use super::owned::{OwnedSinkInfo, OwnedSinkInputInfo};

const TEARDOWN_FAILURE_WARNING: &'static str =
    "Application audio may not work correctly. Please check your system audio \
     configuration.";
const SINK_INPUT_MOVE_FAILURE: &'static str =
    "Some captured inputs failed to move to their original or default sink.";

// TYPE DEFINITIONS ************************************************************

/// Represents possible error modes for [`Interceptor::record()`].
#[derive(Debug)]
pub enum InterceptError {
    /// PulseAudio returned an error while attempting to intercept the
    /// application.
    PulseError(Code),

    /// The interceptor cannot concurrently intercept any more applications.
    AtCapacity(usize),
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
    async fn record(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), InterceptError>;

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
    /// Returns `Ok(None)` if there is no limit applied to this interceptor.
    fn remaining(&self) -> Option<usize> {
        self.capacity().map(|capacity| capacity - self.len())
    }

    /// Determines whether this interceptor has intercepted the maximum number
    /// of concurrent applications.
    ///
    /// This will always be `Ok(false)` for interceptors with no limit.
    fn is_full(&self) -> bool {
        self.remaining().is_some_and(|remaining| remaining == 0)
    }
}

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
        let rec = create_rec_sink(&introspect).await?;
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
        let rec = create_rec_sink(&introspect).await?;
        debug!("Created rec sink at index {}", rec.index);

        let demux = tear_down_module_on_failure(
            &introspect,
            rec.owner_module,
            create_demux_sink(
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
impl From<Code> for InterceptError {
    fn from(code: Code) -> Self {
        Self::PulseError(code)
    }
}

impl TryFrom<InterceptError> for Code {
    type Error = InterceptError;

    fn try_from(err: InterceptError) -> Result<Self, Self::Error> {
        if let InterceptError::PulseError(code) = err {
            Ok(code)
        } else {
            Err(err)
        }
    }
}

impl Display for InterceptError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            Self::PulseError(code) => code.fmt(f),
            Self::AtCapacity(limit) => write!(
                f,
                "Interceptor at limit ({})",
                limit
            ),
        }
    }
}

impl Error for InterceptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::PulseError(code) = self {
            Some(&code)
        } else {
            None
        }
    }
}

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
    async fn record(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), InterceptError> {
        if self.captured.is_some() {
            return Err(InterceptError::AtCapacity(1usize));
        }

        // Make sure there isn't a running stream already before starting it
        debug!(
            "Starting new PulseAudio stream targeting application `{}'",
            source
        );
        self.stream_manager.stop().await;
        self.stream_manager.start(source.clone(), self.volume).await?;

        self.captured = Some(source.clone());

        info!("Started monitor of application `{}'", source);

        Ok(())
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
    async fn record(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), InterceptError> {
        if self.is_full() {
            // limit is guaranteed to be Some if is_full() returns true
            return Err(InterceptError::AtCapacity(self.limit.unwrap()));
        }

        if !self.stream_manager.is_running() {
            debug!("Starting PulseAudio stream");
            self.stream_manager.start(
                self.rec.clone(),
                self.volume
            ).await?;
        }

        self.introspect.move_sink_input_by_index(
            source.index,
            self.rec.index
        ).await?;

        self.captures.insert(source.index, InputState {
            orig_sink: source.sink,
            corked: source.corked,
        });

        info!("Intercepted application `{}'", source);

        Ok(())
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(capture) = self.captures.get(&source_idx) {
            let target_sink = get_sink_or_default(
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
        tear_down_virtual_sink(&self.introspect, &self.rec, None).await;
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
    async fn record(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), InterceptError> {
        if self.is_full() {
            // limit is guaranteed to be Some if is_full() returns true
            return Err(InterceptError::AtCapacity(self.limit.unwrap()));
        }

        if !self.stream_manager.is_running() {
            debug!("Starting PulseAudio stream");
            self.stream_manager.start(
                self.rec.clone(),
                self.volume
            ).await?;
        }

        self.introspect.move_sink_input_by_index(
            source.index,
            self.demux.index
        ).await?;

        self.captures.insert(source.index, InputState {
            orig_sink: source.sink,
            corked: source.corked,
        });

        info!("Intercepted application `{}'", source);

        Ok(())
    }

    async fn stop(&mut self, source_idx: u32) -> Result<bool, Code> {
        if let Some(capture) = self.captures.get(&source_idx) {
            let target_sink = get_sink_or_default(
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
        tear_down_virtual_sink(
            &self.introspect,
            &self.demux,
            Some(&self.orig)
        ).await;
        tear_down_virtual_sink(
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
    async fn record(
        &mut self,
        source: &OwnedSinkInputInfo
    ) -> Result<(), InterceptError> {
        match self.inner.record(source).await {
            Ok(()) => Ok(()),
            Err(InterceptError::AtCapacity(n)) => {
                info!(
                    "Hit application intercept limit ({}). Queueing \
                     application `{}' for later intercept.",
                     n,
                     source
                );

                self.queue.push_back(source.clone());
                Ok(())
            },
            Err(InterceptError::PulseError(e)) => Err(
                InterceptError::PulseError(e)
            ),
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
            if let Some(next_source) = self.queue.pop_front() {
                info!(
                    "Attempting to intercept previously queued application `{}'",
                    next_source
                );

                match self.record(&next_source).await {
                    Ok(()) => Ok(true),
                    Err(InterceptError::PulseError(e)) => Err(e),
                    Err(InterceptError::AtCapacity(_)) => unreachable!(
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

// HELPER FUNCTIONS ************************************************************

/// Gets a unique name for a sink based on an abstract notion of `type`, by
/// combining the application name ("bardcast"), its PID, the sink type, and
/// a unique integer at the end.
async fn get_unique_sink_name(
    introspect: &AsyncIntrospector,
    sink_type: &str
) -> Result<String, Code> {
    let sink_name_template = format!(
        "{}-{}_{}",
        crate_name!(),
        process::id(),
        sink_type
    );
    let matching_sink_count = introspect.count_sinks_with_name_prefix(
        sink_name_template.as_str()
    ).await?;

    Ok(format!("{}-{}", sink_name_template, matching_sink_count))
}

/// Gets the metadata for a virtual sink based on its name and owning module
/// index.
async fn get_created_virtual_sink<S: ToString>(
    introspect: &AsyncIntrospector,
    sink_name: S,
    mod_idx: u32
) -> Result<OwnedSinkInfo, Code> {
    let sink_name = sink_name.to_string();

    //The PulseAudio server can sometimes change the requested name of the sink
    //by appending characters to it, so we do a substring search on the sink
    //name instead of a string comparison.
    introspect.get_sinks().await?.drain(..)
        .filter(|sink| sink.name.as_ref().is_some_and(
            |name| name.contains(&sink_name)
        ) && sink.owner_module.as_ref().is_some_and(
            |owner_module| *owner_module == mod_idx
        ))
        .next().ok_or(Code::NoEntity)
}

async fn get_sink_or_default(
    introspect: &AsyncIntrospector,
    sink_idx: u32
) -> Result<OwnedSinkInfo, Code> {
    introspect.get_sink_by_index(sink_idx).or_else(|err| async move {
        if err == Code::NoEntity {
            introspect.get_default_sink().await
        } else {
            future::err::<OwnedSinkInfo, Code>(err).await
        }
    }).await
}

/// Unloads a virtual sink by its module index.
async fn tear_down_virtual_sink_module(
    introspect: &AsyncIntrospector,
    mod_idx: u32
) -> Result<(), Code> {
    debug!("Tearing down virtual sink owned by module {}", mod_idx);
    introspect.unload_module(mod_idx).await?;
    debug!("Virtual sink torn down");

    Ok(())
}

/// Unloads a virtual sink by its module index if the given `op_result` is
/// `Err`, returning the given result on success, or bubbling up a new error on
/// failure.
async fn tear_down_module_on_failure<T>(
    introspect: &AsyncIntrospector,
    mod_idx: Option<u32>,
    op_result: Result<T, Code>
) -> Result<T, Code> {
    if let Some(mod_idx) = mod_idx {
        if op_result.is_err() {
            debug!(
                "Tearing down virtual sink module with index {} due to \
                 initialization failure",
                mod_idx
            );
            tear_down_virtual_sink_module(introspect, mod_idx).await?;
        }
    }

    op_result
}

/// Creates a virtual audio sink dedicated for recording audio. No audio sent to
/// this sink is sent to any other audio devices on the system.
async fn create_rec_sink(
    introspect: &AsyncIntrospector
) -> Result<OwnedSinkInfo, Code> {
    let sink_name = get_unique_sink_name(introspect, "rec").await?;
    let args = format!("sink_name={}", sink_name);

    debug!("Creating audio capture sink with name '{}'", sink_name);
    let mod_idx = introspect.load_module("module-null-sink", &args).await?;
    debug!("Audio capture module loaded (index {})", mod_idx);

    tear_down_module_on_failure(
        introspect,
        Some(mod_idx),
        get_created_virtual_sink(introspect, sink_name, mod_idx).await
    ).await
}

/// Creates a sink that duplicates audio routed to it to two other sinks, useful
/// for setting up an audio capture routing that does not impede that audio
/// ultimately reaching a hardware audio device.
async fn create_demux_sink(
    introspect: &AsyncIntrospector,
    sinks: &[&OwnedSinkInfo]
) -> Result<OwnedSinkInfo, Code> {
    let sink_name = get_unique_sink_name(introspect, "demux").await?;
    let child_sink_names = sinks.iter()
        .inspect(|sink| if sink.name.is_none() {
            warn!(
                "Downstream sink at index {} has no name, cannot attach to demux sink",
                sink.index
            );
        })
        .filter_map(|sink| sink.name.as_ref().map(|n| n.as_str()))
        .collect::<Vec<&str>>().join(",");
    let args = format!("sink_name={}, slaves={}", sink_name, child_sink_names);

    debug!(
        "Creating audio demux sink with name '{}' and attached sinks: {}",
        sink_name,
        child_sink_names
    );
    let mod_idx = introspect.load_module("module-combine-sink", &args).await?;
    debug!("Audio demux module loaded (index {})", mod_idx);

    tear_down_module_on_failure(
        introspect,
        Some(mod_idx),
        get_created_virtual_sink(introspect, sink_name, mod_idx).await
    ).await
}

async fn interceptor_stop_all(
    interceptor: &mut impl Interceptor,
    inputs: impl IntoIterator<Item = u32>
) {
    for input in inputs {
        if let Err(e) = interceptor.stop(input).await {
            warn!(
                "{} (error {}) {}",
                SINK_INPUT_MOVE_FAILURE,
                e,
                TEARDOWN_FAILURE_WARNING
            );
        }
    }
}

/// Bulk moves all of the sink inputs in `sink_inputs` to the given sink.
async fn move_sink_inputs_to_sink<I>(
    introspect: &AsyncIntrospector,
    sink_inputs: I,
    target_sink_idx: u32
) -> bool
where I: Iterator<Item = u32> {
    future::join_all(sink_inputs.map(|input| {
        debug!(
            "Moving captured input with index {} to sink with index {}",
            input,
            target_sink_idx
        );
        introspect.move_sink_input_by_index(input, target_sink_idx)
    })).await.iter().map(|res| res.is_ok()).fold(true, |acc, x| acc && x)
}

/// Tears down the virtual sink described by the given metadata.
///
/// Any sink inputs still attached to the sink will be moved to the sink
/// described by `move_target`, or the default sink, if no such sink is
/// provided.
async fn tear_down_virtual_sink(
    introspect: &AsyncIntrospector,
    sink: &OwnedSinkInfo,
    move_target: Option<&OwnedSinkInfo>
) {
    // Step 1: Move any lingering sink inputs to another sink
    match introspect.sink_inputs_for_sink(sink.index).await {
        Ok(inputs) => {
            if !inputs.is_empty() {
                warn!(
                    "{0} detected unknown applications attached to sink `{1}'. \
                     This sink is managed automatically by {0}; please avoid \
                     manually attaching applications to this sink using system \
                     audio utilities.",
                    crate_name!(),
                    sink
                );

                let move_target = if let Some(sink) = move_target {
                    Ok(sink.clone())
                } else {
                    introspect.get_default_sink().await
                };

                match move_target {
                    Ok(move_target) => {
                        if !move_sink_inputs_to_sink(
                            introspect,
                            inputs.iter().map(|input| input.index),
                            move_target.index
                        ).await {
                            warn!(
                                "{} {}",
                                SINK_INPUT_MOVE_FAILURE,
                                TEARDOWN_FAILURE_WARNING
                            );
                        }
                    },
                    Err(e) => warn!(
                        "Failed to determine target sink for applications \
                         currently attached to sink `{}', which will be\
                         destroyed (error: {}). {}",
                        sink,
                        e,
                        TEARDOWN_FAILURE_WARNING
                    )

                }
            }
        },
        Err(e) => warn!(
            "Failed to determine applications still attached to sink `{}' \
             error {}). {}",
            sink,
            e,
            TEARDOWN_FAILURE_WARNING
        ),
    }

    // Step 2: Tear down the sink
    if let Some(owner_module) = sink.owner_module {
        if let Err(e) = tear_down_virtual_sink_module(
            introspect,
            owner_module
        ).await {
            error!(
                "Failed to tear down virtual sink at index {}: {}",
                sink.index,
                e
            );
        }
    } else {
        warn!(
            "Failed to tear down virtual sink at index {} as it does not have an owner module",
            sink.index
        );
    }
}

