///! Rust-style async wrapper around the libpulse [`Introspector`] interface.

extern crate libpulse_binding as libpulse;

use std::string::ToString;
use std::sync::{Mutex, Weak};

use libpulse::context::introspect::Introspector;
use libpulse::error::Code;
use libpulse::operation::Operation;
use libpulse::volume::Volume;

use crate::snd::pulse::owned::{
    OwnedSinkInfo,
    OwnedSinkInputInfo,
    OwnedSourceInfo
};
use super::PulseContextWrapper;
use super::collect::{
    self,
    CollectResult,
    CollectOptionalResult,
    CollectListResult
};

// TYPE DEFINITIONS ************************************************************

/// Rust-style async wrapper around the libpulse [`Introspector`] interface.
#[derive(Clone)]
pub struct AsyncIntrospector {
    ctx_wrap: PulseContextWrapper,
}

// TRAIT IMPLS *****************************************************************
impl From<&PulseContextWrapper> for AsyncIntrospector {
    fn from(ctx_wrap: &PulseContextWrapper) -> Self {
        Self {
            ctx_wrap: ctx_wrap.clone(),
        }
    }
}

impl AsyncIntrospector {
    /// Wrapper for [`PulseContextWrapper::do_ctx_op_default`] that creates an
    /// [`Introspector`] instance from the context and passes it to the inner
    /// function.
    async fn do_default<T, S, F>(&self, op: F) -> Result<T, Code>
    where
        T: Default + Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(Introspector, Weak<Mutex<T>>) -> Operation<S> + Send + 'static {
        self.ctx_wrap
            .do_ctx_op_default(|ctx, result| op(ctx.introspect(), result))
            .await
            .map_err(|_| Code::Killed)
    }

    /// Wrapper for [`PulseContextWrapper::do_ctx_op`] that creates an
    /// [`Introspector`] instance from the context and passes it to the inner
    /// function.
    async fn do_op<T, S, F>(&self, op: F) -> Result<Option<T>, Code>
    where
        T: Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(Introspector, Weak<Mutex<Option<T>>>) -> Operation<S> + Send + 'static {
        self.ctx_wrap
            .do_ctx_op(|ctx, result| op(ctx.introspect(), result))
            .await
            .map_err(|_| Code::Killed)
    }

    /// Wrapper for [`PulseContextWrapper::do_ctx_op_list`] that creates an
    /// [`Introspector`] instance from the context and passes it to the inner
    /// function.
    async fn do_list<T, S, F>(&self, op: F) -> Result<Vec<T>, Code>
    where
        T: Send + 'static,
        S: ?Sized + 'static,
        F: FnOnce(Introspector, Weak<Mutex<Vec<T>>>) -> Operation<S> + Send + 'static {
        self.ctx_wrap
            .do_ctx_op_list(|ctx, result| op(ctx.introspect(), result))
            .await
            .map_err(|_| Code::Killed)
    }

    /// Fetches entity info for all sinks available on the PulseAudio server.
    pub async fn get_sinks(&self) -> Result<Vec<OwnedSinkInfo>, Code> {
        self.do_list(|introspect, result| {
            introspect.get_sink_info_list(move |sink_info| {
                result.push_info_from_list(sink_info)
            })
        }).await
    }

    /// Fetches entity info for the sink with the specified index. If no such
    /// sink exists, this returns `Err(PulseFailure::Error(Code:NoEntity))`.
    pub async fn get_sink_by_index(
        &self,
        sink_idx: u32
    ) -> Result<OwnedSinkInfo, Code> {
        self.do_op(move |introspect, result| {
            introspect.get_sink_info_by_index(
                sink_idx,
                move |sink_info| result.first_info_in_list(sink_info)
            )
        }).await?.ok_or(Code::NoEntity)
    }

    /// Fetches entity info for the sink with the specified name. If no such
    /// sink exists, this returns `Err(PulseFailure::Error(Code:NoEntity))`.
    pub async fn get_sink_by_name<S: ToString>(
        &self,
        name: S
    ) -> Result<OwnedSinkInfo, Code> {
        let name = name.to_string();

        self.do_op(move |introspect, result| {
            introspect.get_sink_info_by_name(
                &name,
                move |sink_info| result.first_info_in_list(sink_info)
            )
        }).await?.ok_or(Code::NoEntity)
    }

    /// Fetches entity info for the sink currently configured by the user as
    /// default. If this cannot be determined, this returns
    /// `Err(PulseFailure::Error(Code::NoEntity))`.
    pub async fn get_default_sink(&self) -> Result<OwnedSinkInfo, Code> {
        let name = self.do_op(|introspect, result| introspect.get_server_info(move |server_info| {
            if let Some(default_sink_name) = &server_info.default_sink_name {
                result.last(default_sink_name.to_string())
            }
        })).await?.ok_or(Code::NoEntity)?;

        self.get_sink_by_name(name).await
    }

    /// Fetches entity info for the source with the specified index. If no such
    /// source exists, this returns `Err(PulseFailure::Error(Code::NoEntity))`.
    pub async fn get_source_by_index(
        &self,
        source_idx: u32
    ) -> Result<OwnedSourceInfo, Code> {
        self.do_op(move |introspect, result| {
            introspect.get_source_info_by_index(
                source_idx,
                move |source_info| result.first_info_in_list(source_info)
            )
        }).await?.ok_or(Code::NoEntity)
    }

    /// Returns the name of the monitor source for the specified sink, fetching
    /// it from the server if not present in the entity info object.
    pub async fn resolve_sink_monitor_name(
        &self,
        sink: OwnedSinkInfo
    ) -> Result<String, Code> {
        if let Some(monitor_source_name) = sink.monitor_source_name {
            Ok(monitor_source_name)
        } else {
            self.get_source_by_index(sink.monitor_source).await
                .and_then(|info| info.name.ok_or(Code::NoEntity))
        }
    }

    /// Returns the number of sinks that have names starting with the given
    /// string.
    pub async fn count_sinks_with_name_prefix<S: ToString>(
        &self,
        prefix: S
    ) -> Result<usize, Code> {
        let prefix = prefix.to_string();

        self.do_default(|introspect, result| introspect.get_sink_info_list(move |sink_info| {
            if let Some(_) = collect::filter_list(
                sink_info,
                |info| info.name.as_ref().is_some_and(|name| name.starts_with(&prefix))
            ) {
                result.with(|counter| *counter += 1);
            }
        })).await
    }

    /// Fetches entity info for the application playback stream ("sink input")
    /// with the specified index. If no such stream exists, this returns
    /// `Err(PulseFailure::Error(Code::NoEntity))`.
    pub async fn get_sink_input(
        &self,
        idx: u32
    ) -> Result<OwnedSinkInputInfo, Code> {
        self.do_op(move |introspect, result| introspect.get_sink_input_info(
                idx,
                move |sink_input_info| result.first_info_in_list(sink_input_info)
        )).await?.ok_or(Code::NoEntity)
    }

    /// Fetches entity info for all application playback streams ("sink inputs")
    /// currently known to the server.
    pub async fn get_sink_inputs(&self) -> Result<Vec<OwnedSinkInputInfo>, Code> {
        self.do_list(|introspect, result| {
            introspect.get_sink_input_info_list(move |sink_input_info| {
                result.push_info_from_list(sink_input_info);
            })
        }).await
    }

    /// Fetches entity info for all application playback streams ("sink inputs")
    /// currently playing audio to the sink with the given index. An empty `Vec`
    /// will be returned if no such sink exists.
    pub async fn sink_inputs_for_sink(
        &self,
        sink_idx: u32
    ) -> Result<Vec<OwnedSinkInputInfo>, Code> {
        self.do_list(move |introspect, result| introspect.get_sink_input_info_list(move |sink_input_info| {
            if let Some(sink_input_info) = collect::filter_list(
                sink_input_info,
                |info| info.sink == sink_idx
            ) {
                result.push_info(sink_input_info)
            }
        })).await
    }

    /// Moves the application playback stream ("sink input") with the given
    /// `input_index` to the sink specified by `sink_index`.
    pub async fn move_sink_input_by_index(
        &self,
        input_index: u32,
        sink_index: u32
    ) -> Result<(), Code> {
        self.do_default(move |mut introspect, result| introspect.move_sink_input_by_index(
            input_index,
            sink_index,
            Some(Box::new(move |success| result.store(success)))
        )).await.map_or_else(
            |e| Err(e),
            |success| if success {
                Ok(())
            } else {
                Err(Code::Unknown)
            }
        )
    }

    /// Sets the volume for the application recording stream ("source output")
    /// with the given `output_index`. `volume` is a normalized decimal value
    /// from 0 to 1 (inclusive).
    pub async fn set_source_output_volume(
        &self,
        output_index: u32,
        volume: f64
    ) -> Result<(), Code> {
        let output_index_copy = output_index.clone();
        let mut channel_volumes = self.do_default(move |introspect, result| {
            introspect.get_sink_input_info(output_index_copy, move |output| {
                if let Some(output) = collect::with_list(output) {
                    result.store(output.volume);
                }
            })
        }).await?;

        let volume = Volume((f64::from(channel_volumes.max().0) * volume) as u32);
        channel_volumes.set(channel_volumes.len(), volume);

        self.do_default(move |mut introspect, result| introspect.set_source_output_volume(
            output_index,
            &channel_volumes,
            Some(Box::new(move |success| result.store(success)))
        )).await.map_or_else(
            |e| Err(e),
            |success| if success {
                Ok(())
            } else {
                Err(Code::Unknown)
            }
        )
    }

    /// Loads the named PulseAudio module on the server with the given argument
    /// string, returing the newly loaded module's index.
    pub async fn load_module<S1: ToString, S2: ToString>(
        &self,
        name: S1,
        args: S2
    ) -> Result<u32, Code> {
        let name = name.to_string();
        let args = args.to_string();

        self.do_op(move |mut introspect, result| introspect.load_module(
            &name,
            &args,
            move |index| result.last(index)
        )).await?.ok_or(Code::NoEntity)
    }

    /// Unloads the module with the specified index from the server.
    pub async fn unload_module(&self, idx: u32) -> Result<(), Code> {
        self.do_default(move |mut introspect, result| introspect.unload_module(
            idx,
            move |success| result.store(success)
        )).await.map_or_else(
            |e| Err(e),
            |success| if success {
                Ok(())
            } else {
                Err(Code::Unknown)
            }
        )
    }
}
