///! Owned versions of metadata types returned by
///! `libpulse::context::introspect::Introspect`.

extern crate libpulse_binding as libpulse;

use std::borrow::Cow;
use std::fmt::{Display, Error as FormatError, Formatter};

use libpulse::channelmap::Map as ChannelMap;
use libpulse::context::introspect::{SinkInfo, SinkInputInfo, SourceInfo};
use libpulse::def::{SinkFlagSet, SinkState, SourceFlagSet, SourceState};
use libpulse::format::Info as FormatInfo;
use libpulse::proplist::Proplist;
use libpulse::sample::Spec;
use libpulse::time::MicroSeconds;
use libpulse::volume::{ChannelVolumes, Volume};

const UNKNOWN_ENTITY_LABEL: String = String::from("[unknown]");

// TYPE DEFINITIONS ************************************************************

/// Trait to help convert the native PulseAudio info types into their owned
/// counterparts.
pub trait IntoOwnedInfo {
    /// The owned version of the native PulseAudio type.
    type Owned;

    /// Converts the native type into its owned equivalent.
    fn into_owned(&self) -> Self::Owned;
}

/// Owned version of [`SinkInfo`].
#[derive(Debug, Clone)]
pub struct OwnedSinkInfo {
    pub name: Option<String>,
    pub index: u32,
    pub description: Option<String>,
    pub sample_spec: Spec,
    pub channel_map: ChannelMap,
    pub owner_module: Option<u32>,
    pub volume: ChannelVolumes,
    pub mute: bool,
    pub monitor_source: u32,
    pub monitor_source_name: Option<String>,
    pub latency: MicroSeconds,
    pub driver: Option<String>,
    pub flags: SinkFlagSet,
    pub proplist: Proplist,
    pub configured_latency: MicroSeconds,
    pub base_volume: Volume,
    pub state: SinkState,
    pub n_volume_steps: u32,
    pub card: Option<u32>,
    pub formats: Vec<FormatInfo>,
}

/// Owned version of [`SinkInputInfo`].
#[derive(Debug, Clone)]
pub struct OwnedSinkInputInfo {
    pub index: u32,
    pub name: Option<String>,
    pub owner_module: Option<u32>,
    pub sink: u32,
    pub sample_spec: Spec,
    pub channel_map: ChannelMap,
    pub volume: ChannelVolumes,
    pub buffer_usec: MicroSeconds,
    pub sink_usec: MicroSeconds,
    pub resample_method: Option<String>,
    pub driver: Option<String>,
    pub mute: bool,
    pub proplist: Proplist,
    pub corked: bool,
    pub has_volume: bool,
    pub volume_writable: bool,
    pub format: FormatInfo,
}

// Owned version of [`SourceInfo`].
#[derive(Debug, Clone)]
pub struct OwnedSourceInfo {
    pub name: Option<String>,
    pub index: u32,
    pub description: Option<String>,
    pub sample_spec: Spec,
    pub channel_map: ChannelMap,
    pub owner_module: Option<u32>,
    pub volume: ChannelVolumes,
    pub mute: bool,
    pub monitor_of_sink: Option<u32>,
    pub monitor_of_sink_name: Option<String>,
    pub latency: MicroSeconds,
    pub driver: Option<String>,
    pub flags: SourceFlagSet,
    pub proplist: Proplist,
    pub configured_latency: MicroSeconds,
    pub base_volume: Volume,
    pub state: SourceState,
    pub n_volume_steps: u32,
    pub card: Option<u32>,
    pub formats: Vec<FormatInfo>,
}

// TRAIT IMPLS *****************************************************************
impl From<&SinkInfo<'_>> for OwnedSinkInfo {
    fn from(info: &SinkInfo<'_>) -> Self {
        Self {
            name: map_opt_str(info.name.as_ref()),
            index: info.index,
            description: map_opt_str(info.description.as_ref()),
            sample_spec: info.sample_spec.clone(),
            channel_map: info.channel_map.clone(),
            owner_module: info.owner_module.clone(),
            volume: info.volume.clone(),
            mute: info.mute,
            monitor_source: info.monitor_source,
            monitor_source_name: map_opt_str(info.monitor_source_name.as_ref()),
            latency: info.latency.clone(),
            driver: map_opt_str(info.driver.as_ref()),
            flags: info.flags.clone(),
            proplist: info.proplist.clone(),
            configured_latency: info.configured_latency.clone(),
            base_volume: info.base_volume.clone(),
            state: info.state.clone(),
            n_volume_steps: info.n_volume_steps,
            card: info.card.clone(),
            formats: info.formats.clone(),
        }
    }
}

impl From<&SinkInputInfo<'_>> for OwnedSinkInputInfo {
    fn from(info: &SinkInputInfo<'_>) -> OwnedSinkInputInfo {
        OwnedSinkInputInfo {
            index: info.index,
            name: map_opt_str(info.name.as_ref()),
            owner_module: info.owner_module.clone(),
            sink: info.sink,
            sample_spec: info.sample_spec.clone(),
            channel_map: info.channel_map.clone(),
            volume: info.volume.clone(),
            buffer_usec: info.buffer_usec.clone(),
            sink_usec: info.sink_usec.clone(),
            resample_method: map_opt_str(info.resample_method.as_ref()),
            driver: map_opt_str(info.driver.as_ref()),
            mute: info.mute,
            proplist: info.proplist.clone(),
            corked: info.corked,
            has_volume: info.has_volume,
            volume_writable: info.volume_writable,
            format: info.format.clone(),
        }
    }
}

impl From<&SourceInfo<'_>> for OwnedSourceInfo {
    fn from(info: &SourceInfo<'_>) -> OwnedSourceInfo {
        OwnedSourceInfo {
            name: map_opt_str(info.name.as_ref()),
            index: info.index,
            description: map_opt_str(info.description.as_ref()),
            sample_spec: info.sample_spec.clone(),
            channel_map: info.channel_map.clone(),
            owner_module: info.owner_module.clone(),
            volume: info.volume.clone(),
            mute: info.mute,
            monitor_of_sink: info.monitor_of_sink.clone(),
            monitor_of_sink_name: map_opt_str(info.monitor_of_sink_name.as_ref()),
            latency: info.latency.clone(),
            driver: map_opt_str(info.driver.as_ref()),
            flags: info.flags.clone(),
            proplist: info.proplist.clone(),
            configured_latency: info.configured_latency.clone(),
            base_volume: info.base_volume.clone(),
            state: info.state.clone(),
            n_volume_steps: info.n_volume_steps,
            card: info.card.clone(),
            formats: info.formats.clone(),
        }
    }
}

impl Display for OwnedSinkInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        fmt_name_or_index(f, self.name.as_ref(), self.index)
    }
}

impl Display for OwnedSinkInputInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        fmt_name_or_index(f, self.name.as_ref(), self.index)
    }
}

impl Display for OwnedSourceInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        fmt_name_or_index(f, self.name.as_ref(), self.index)
    }
}

impl IntoOwnedInfo for SinkInfo<'_> {
    type Owned = OwnedSinkInfo;

    fn into_owned(&self) -> Self::Owned {
        OwnedSinkInfo::from(self)
    }
}

impl IntoOwnedInfo for SinkInputInfo<'_> {
    type Owned = OwnedSinkInputInfo;

    fn into_owned(&self) -> Self::Owned {
        OwnedSinkInputInfo::from(self)
    }
}

impl IntoOwnedInfo for SourceInfo<'_> {
    type Owned = OwnedSourceInfo;

    fn into_owned(&self) -> Self::Owned {
        OwnedSourceInfo::from(self)
    }
}

// HELPER FUNCTIONS ************************************************************

/// Maps an optional copy-on-write `str` into an optional owned `String`.
fn map_opt_str(orig: Option<&Cow<'_, str>>) -> Option<String> {
    orig.map(|orig_str| orig_str.to_string())
}

fn fmt_name_or_index(
    f: &mut Formatter<'_>,
    name: Option<&String>,
    index: u32
) -> Result<(), FormatError> {
    write!(f, "{} (index {})", name.unwrap_or(&UNKNOWN_ENTITY_LABEL), index)
}
