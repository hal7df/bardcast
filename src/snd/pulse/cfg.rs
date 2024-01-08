///! Configuration for the PulseAudio sound driver.

use std::collections::HashSet;

use configparser::ini::Ini;
use regex::Regex;

use crate::cfg::{self, Action, Args, InterceptMode, Config};
use crate::snd::pulse;

// TYPE DEFINITIONS ************************************************************

/// Main configuration type for the PulseAudio sound driver.
#[derive(Debug)]
pub struct PulseDriverConfig {
    /// A regular expression to match against applications to intercept.
    pub stream_regex: Option<Regex>,

    /// The set of properties to match [`stream_regex`] against. If none are
    /// present, `application.name` and `application.process.binary` are used.
    pub stream_properties: HashSet<String>,

    /// The path to the PulseAudio server socket. If not set, the application
    /// will determine the PulseAudio server to connect to automatically.
    pub server: Option<String>,

    /// The name of the existing sink that bardcast should interact with,
    /// according to the [`intercept_mode`].  For `Monitor` mode, this is the
    /// sink that is monitored.
    ///
    /// This is ignored if [`sink_index`] is set.
    pub sink_name: Option<String>,

    /// The maximum duration between mainloop iterations.
    ///
    /// bardcast tries to detect the appropriate maximum delay before the next
    /// iteration of the mainloop automatically, but caps its maximum value to
    /// prevent audio stuttering in the worst case. The default value
    /// (20 ms/20000 us) should work in most cases, but the control is exposed
    /// here in case your hardware requires a shorter max interval.
    pub max_mainloop_interval_usec: Option<u64>,

    /// The initial volume for the recording stream. The system audio mixer can
    /// be used to adjust audio levels after application startup.
    pub volume: Option<f64>,

    /// The number of applications that can be captured concurrently.
    ///
    /// This is ignored if [`stream_regex`] is not set.
    pub intercept_limit: Option<usize>,

    /// The index of the existing sink that bardcast should interact with,
    /// according to the [`intercept_mode`]. For `Monitor` mode, this is the
    /// sink that is monitored.
    ///
    /// This option takes precedence over [`sink_name`] if both are set.
    pub sink_index: Option<u32>,

    /// The approach the driver should use to intercept individual application
    /// audio.
    pub intercept_mode: Option<InterceptMode>,
}

// TRAIT IMPLS *****************************************************************
impl TryFrom<&Args> for PulseDriverConfig {
    type Error = String;

    fn try_from(args: &Args) -> Result<Self, Self::Error> {
        let (sink_name, sink_index) = if let Some(sink) = &args.sink {
            if let Ok(index) = u32::from_str_radix(sink, 10) {
                (None, Some(index))
            } else {
                (Some(sink.clone()), None)
            }
        } else {
            (None, None)
        };

        Ok(Self {
            stream_regex: cfg::validate_regex(&args.stream_regex)?,
            stream_properties: args.stream_property.clone().drain(..).collect(),
            server: args.snd_backend.clone(),
            sink_name,
            max_mainloop_interval_usec: None,
            volume: validate_volume(args.volume.clone())?,
            intercept_limit: args.intercept_limit,
            sink_index,
            intercept_mode: args.intercept_mode.clone(),
        })
    }
}

impl<'a> Config<'a> for PulseDriverConfig {
    fn from_ini(config: &Ini) -> Result<Self, String> {
        Ok(Self {
            stream_regex: cfg::validate_regex(&config.get(pulse::DRIVER_NAME, "stream-regex"))?,
            stream_properties: cfg::parse_raw_property_list(&config.get(pulse::DRIVER_NAME, "stream-properties")),
            server: config.get(pulse::DRIVER_NAME, "server"),
            sink_name: config.get(pulse::DRIVER_NAME, "sink-name"),
            max_mainloop_interval_usec: config.getuint(pulse::DRIVER_NAME, "max-mainloop-interval-usec")?,
            volume: validate_volume(config.getfloat(pulse::DRIVER_NAME, "volume")?)?,
            intercept_limit: process_intercept_limit(config.getuint(pulse::DRIVER_NAME, "intercept-limit")?),
            sink_index: validate_sink_index(config.getuint(pulse::DRIVER_NAME, "sink-index"))?,
            intercept_mode: cfg::validate_intercept_mode(&config.get(pulse::DRIVER_NAME, "intercept-mode"))?,
        })
    }

    fn merge(&mut self, other: Self) {
        cfg::merge_opt(&mut self.intercept_mode, other.intercept_mode);
        cfg::merge_opt(&mut self.stream_regex, other.stream_regex);
        cfg::merge_opt(&mut self.server, other.server);
        cfg::merge_opt(&mut self.volume, other.volume);
        cfg::merge_opt(&mut self.intercept_limit, other.intercept_limit);
        cfg::merge_opt(
            &mut self.max_mainloop_interval_usec,
            other.max_mainloop_interval_usec
        );

        // Reset the fallback sink index if the override specifies a sink name.,
        // The sink index takes precedence over the sink name if present, but
        // if the override specifies any sink settings we want that to take
        // precedence.
        if other.sink_name.is_some() && self.sink_index.is_some() {
            self.sink_index = None;
        }

        cfg::merge_opt(&mut self.sink_name, other.sink_name);
        cfg::merge_opt(&mut self.sink_index, other.sink_index);

        if !other.stream_properties.is_empty() {
            self.stream_properties = other.stream_properties;
        }
    }

    fn validate_semantics(&self, _: Action) -> Result<(), String> {
        if let Some(intercept_mode) = self.intercept_mode {
            if intercept_mode != InterceptMode::Monitor && self.stream_regex.is_none() {
                return Err(String::from(
                    "The selected intercept mode requires -E/--stream-regex"
                ));
            }
        }

        if self.intercept_limit.is_some() && self.stream_regex.is_none() {
            eprintln!("-n/--intercept-limit has no effect without -E/--stream-regex, ignoring");
        }

        Ok(())
    }
}

// HELPER FUNCTIONS ************************************************************
/// Validates that the given volume is valid (i.e. between 0 and 1).
fn validate_volume(raw_volume: Option<f64>) -> Result<Option<f64>, String> {
    if let Some(raw_volume) = raw_volume {
        if raw_volume < 0. || raw_volume > 1. {
            Err(String::from("Volume must be between 0 and 1."))
        } else {
            Ok(Some(raw_volume))
        }
    } else {
        Ok(raw_volume)
    }
}

fn process_intercept_limit(intercept_limit_u64: Option<u64>) -> Option<usize> {
    intercept_limit_u64.and_then(|n_u64| {
        if let Ok(n_usize) = usize::try_from(n_u64) {
            Some(n_usize)
        } else {
            eprintln!(
                "Intercept limit of {} exceeds maximum of {}. Limit will be \
                 removed.",
                n_u64,
                usize::MAX
            );
            None
        }
    })
}

/// Narrows the byte width of the value containing a sink index, and returns the
/// result.
fn validate_sink_index(raw_index: Result<Option<u64>, String>) -> Result<Option<u32>, String> {
    raw_index
        .map_err(|raw| format!("Provided sink-index '{}' is not valid", raw))
        .and_then(|index| {
            index.map(|i| {
                u32::try_from(i)
                    .map_err(|_| format!(
                        "Provided sink-index '{}' is too large",
                        i
                    ))
            }).transpose()
        })
}
