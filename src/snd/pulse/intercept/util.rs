use std::process;

use clap::crate_name;
use libpulse_binding::error::Code;
use log::{debug, error, warn};
use futures::future::{self, TryFutureExt};

use super::super::context::AsyncIntrospector;
use super::super::owned::OwnedSinkInfo;

// PUBLIC HELPER FUNCTIONS *****************************************************
/// Creates a virtual audio sink dedicated for recording audio. No audio sent to
/// this sink is sent to any other audio devices on the system.
pub async fn create_rec_sink(
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
pub async fn create_demux_sink(
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

/// Resolves the sink with the given index, or, if no sink exists at that index,
/// returns the system default sink.
pub async fn get_sink_or_default(
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

/// Unloads a virtual sink by its module index if the given `op_result` is
/// `Err`, returning the given result on success, or bubbling up a new error on
/// failure.
pub async fn tear_down_module_on_failure<T>(
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

/// Tears down the virtual sink described by the given metadata.
///
/// Any sink inputs still attached to the sink will be moved to the sink
/// described by `move_target`, or the default sink, if no such sink is
/// provided.
pub async fn tear_down_virtual_sink(
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
                                super::SINK_INPUT_MOVE_FAILURE,
                                super::TEARDOWN_FAILURE_WARNING
                            );
                        }
                    },
                    Err(e) => warn!(
                        "Failed to determine target sink for applications \
                         currently attached to sink `{}', which will be\
                         destroyed (error: {}). {}",
                        sink,
                        e,
                        super::TEARDOWN_FAILURE_WARNING
                    )

                }
            }
        },
        Err(e) => warn!(
            "Failed to determine applications still attached to sink `{}' \
             error {}). {}",
            sink,
            e,
            super::TEARDOWN_FAILURE_WARNING
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

// PRIVATE HELPER FUNCTIONS ****************************************************
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
