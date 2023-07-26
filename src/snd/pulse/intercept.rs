///! Utilities for intercepting application audio streams for recording.
///!
///! The core abstraction in this modules is the [`Interceptor`] trait, which
///! allows bardcast to intercept applications playing audio via their sink
///! input ID. When used in conjunction with the event notification
///! functionality of the [`super::event`] module, this allows applications to
///! be efficiently intercepted as soon as they open an audio stream.

extern crate libpulse_binding as libpulse;

use std::process;

use async_trait::async_trait;
use clap::crate_name;
use futures::future;
use log::{debug, error, warn};

use libpulse::error::Code;

use super::context::AsyncIntrospector;
use super::owned::OwnedSinkInfo;

const TEARDOWN_FAILURE_WARNING: &'static str = "Application audio may not work correctly. Please check your system audio configuration.";
const SINK_INPUT_MOVE_FAILURE: &'static str = "Some captured inputs failed to move to their original or default sink.";

// TYPE DEFINITIONS ************************************************************

/// Core abstraction for intercepting application audio.
///
/// The main function in this interface is [`intercept`], which performs the
/// work of intercepting applications and routing them such that bardcast can
/// read their stream. This interface also provides a few functions to query the
/// current state of intercepted streams.
#[async_trait]
pub trait Interceptor: Send + Sync {
    /// Intercepts an application with the given sink input ID.
    async fn intercept(&mut self, sink_input_id: u32) -> Result<(), Code>;

    /// Returns the system name of the audio source that can be used to read
    /// intercepted application audio.
    fn source_name(&self) -> Result<String, Code>;

    /// Returns the number applications that have been intercepted by bardcast.
    async fn intercepted_stream_count(&self) -> Result<usize, Code>;

    /// Gracefully closes any system resources that were created by the
    /// `Interceptor`, returning any intercepted streams to another audio device
    /// if needed.
    async fn close(mut self);

    /// Version of [`Interceptor::close`] for boxed trait objects.
    async fn boxed_close(mut self: Box<Self>);
}

/// Implementation of [`Interceptor`] that captures application audio in a
/// dedicated audio sink, preventing it from reaching any hardware audio
/// devices.
pub struct CapturingInterceptor {
    introspect: AsyncIntrospector,
    rec: OwnedSinkInfo,
}

/// Implementation of [`Interceptor`] that duplicates application audio streams
/// before reading them to allow the audio to reach a hardware sink in addition
/// to being read by bardcast.
pub struct PeekingInterceptor {
    introspect: AsyncIntrospector,
    demux: OwnedSinkInfo,
    rec: OwnedSinkInfo,
    orig_idx: u32,
}

/// TYPE IMPLS *****************************************************************
impl CapturingInterceptor {
    /// Creates a new `CapturingInterceptor`.
    pub async fn new(
        introspect: &AsyncIntrospector,
    ) -> Result<Self, Code> {
        let rec_sink = create_rec_sink(introspect).await?;
        debug!("Created rec sink at index {}", rec_sink.index);

        Ok(Self {
            introspect: introspect.clone(),
            rec: rec_sink,
        })
    }
}

impl PeekingInterceptor {
    /// Creates a new `PeekingInterceptor`, using the given sink as the second
    /// output.
    pub async fn from_sink(
        introspect: &AsyncIntrospector,
        sink: &OwnedSinkInfo,
    ) -> Result<Self, Code> {
        let rec_sink = create_rec_sink(
            introspect,
        ).await?;
        debug!("Created rec sink at index {}", rec_sink.index);

        let demux_sink = create_demux_sink(
            introspect,
            &[&rec_sink, &sink],
        ).await;

        let demux_sink = if let Some(mod_idx) = &rec_sink.owner_module {
            tear_down_module_on_failure(introspect, *mod_idx, demux_sink).await?
        } else {
            demux_sink?
        };
        debug!("Created demux sink at index {}", demux_sink.index);

        Ok(Self {
            introspect: introspect.clone(),
            demux: demux_sink,
            rec: rec_sink,
            orig_idx: sink.index,
        })
    }
}

// TRAIT IMPLS *****************************************************************
#[async_trait]
impl Interceptor for CapturingInterceptor {
    async fn intercept(&mut self, sink_input_id: u32) -> Result<(), Code> {
       self.introspect.move_sink_input_by_index(
           sink_input_id,
           self.rec.index
        ).await
    }

    fn source_name(&self) -> Result<String, Code> {
        self.rec.monitor_source_name.clone().ok_or(Code::NoData)
    }

    async fn intercepted_stream_count(&self) -> Result<usize, Code> {
       self.introspect.sink_inputs_for_sink(
           self.rec.index
       ).await.map(|inputs| inputs.len())
    }

    async fn close(mut self) {
        if let Ok(inputs) = self.introspect.sink_inputs_for_sink(self.rec.index).await {
            let default_sink = self.introspect.get_default_sink().await.map(|sink| sink.index);

            if let Ok(default_sink) = default_sink {
                if !move_sink_inputs_to_sink(
                    &self.introspect,
                    inputs.iter().map(|input| input.index),
                    default_sink
                ).await {
                    warn!("{} {}", SINK_INPUT_MOVE_FAILURE, TEARDOWN_FAILURE_WARNING);
                }
            } else {
                warn!("Failed to determine default sink. {}", TEARDOWN_FAILURE_WARNING);
            }
        } else {
            warn!(
                "Failed to determine inputs for rec sink at index {}. {} {}",
                self.rec.index,
                SINK_INPUT_MOVE_FAILURE,
                TEARDOWN_FAILURE_WARNING
            );
        }

        tear_down_virtual_sink(&self.introspect, &self.rec).await;
    }

    async fn boxed_close(mut self: Box<Self>) {
        self.close().await;
    }
}

#[async_trait]
impl Interceptor for PeekingInterceptor {
    async fn intercept(&mut self, sink_input_id: u32) -> Result<(), Code> {
        self.introspect.move_sink_input_by_index(
            sink_input_id,
            self.demux.index
        ).await
    }

    fn source_name(&self) -> Result<String, Code> {
        self.rec.monitor_source_name.clone().ok_or(Code::NoData)
    }

    async fn intercepted_stream_count(&self) -> Result<usize, Code> {
        self.introspect.sink_inputs_for_sink(
            self.demux.index
        ).await.map(|inputs| inputs.len())
    }

    async fn close(mut self) {
        if let Ok(inputs) = self.introspect.sink_inputs_for_sink(self.demux.index).await {
            if !move_sink_inputs_to_sink(
                &self.introspect,
                inputs.iter().map(|input| input.index),
                self.orig_idx
            ).await {
                warn!("{} {}", SINK_INPUT_MOVE_FAILURE, TEARDOWN_FAILURE_WARNING);
            }
        } else {
            warn!(
                "Failed to determine inputs for demux sink at index {}. {} {}",
                self.demux.index,
                SINK_INPUT_MOVE_FAILURE,
                TEARDOWN_FAILURE_WARNING
            );
        }

        //Tear down demux and rec sinks
        tear_down_virtual_sink(&self.introspect, &self.demux).await;
        tear_down_virtual_sink(&self.introspect, &self.rec).await;
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
    mod_idx: u32,
    op_result: Result<T, Code>
) -> Result<T, Code> {
    if op_result.is_err() {
        debug!("Tearing down virtual sink module with index {} due to initialization failure", mod_idx);
        tear_down_virtual_sink_module(introspect, mod_idx).await?;
    }

    op_result
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
        mod_idx,
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
        mod_idx,
        get_created_virtual_sink(introspect, sink_name, mod_idx).await
    ).await
}

/// Tears down the virtual sink described by the given metadata.
async fn tear_down_virtual_sink(
    introspect: &AsyncIntrospector,
    sink: &OwnedSinkInfo
) {
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

