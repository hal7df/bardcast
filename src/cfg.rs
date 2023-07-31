///! Common application configuration types, and helper functions for parsing
///! configuration options from strings and/or [`configparser::ini::Ini`].

use std::collections::HashSet;
use std::mem;
use std::path::Path;

use clap::{crate_name, Parser, ValueEnum};
use configparser::ini::Ini;
use log::{LevelFilter, warn};
use regex::{Error as RegexError, Regex};

use crate::consumer::discord::cfg::DiscordConfig;

#[cfg(all(target_family = "unix", feature = "pulse"))]
use crate::snd::pulse::PulseDriverConfig;

/// Connects a system audio source to a Discord voice chat
#[derive(Parser, Debug)]
pub struct Args {
    /// An identifier for an application stream property that bardcast should
    /// match against when determining which streams to capture. This option
    /// can be specified multiple times to match against multiple properties;
    /// bardcast will capture any application that has at least one matching
    /// property.
    ///
    /// This is intended for use with -E/--stream-regex.
    #[clap(short = 'p', long)]
    pub stream_property: Vec<String>,

    /// Load application configuration from the given configuration file.
    /// Commandline arguments will take precedence over configuration.
    #[clap(short, long)]
    pub configfile: Option<String>,

    /// Name of the sound driver to use.
    ///
    /// bardcast will try to autodetect this using sound servers available on
    /// the system. This option, if present, will override autodection.
    #[clap(short, long = "driver")]
    pub driver_name: Option<String>,

    /// Match application streams using the given regex
    #[clap(short = 'E', long)]
    pub stream_regex: Option<String>,

    /// An identifier for the audio output that bardcast should interact with.
    /// The interpretation of this option depends on the selected capture mode.
    /// Peek mode will use this output for all recorded audio, while monitor
    /// mode will monitor all audio sent to this sink.
    ///
    /// The format of this option depends on the underlying audio server, but
    /// will typically be the audio output's system index or name. If not
    /// specified, the system's default audio output will be used.
    #[clap(short = 'i', long)]
    pub sink: Option<String>,

    /// Identifier for a sound backend that is compatible with the selected
    /// driver.
    ///
    /// The value expected here may vary based on the sound driver being used,
    /// although this is typically a UNIX domain socket or named pipe. Some
    /// drivers may not support this option.
    #[clap(long)]
    pub snd_backend: Option<String>,

    /// The Discord API token to use to authenticate with Discord.
    ///
    /// Note that passing raw secrets on the command line is generally insecure.
    /// Tokens should preferably be provided via the config file passed to
    /// -c/--configfile. If you must specify it on the commandline, use an
    /// environment variable to store the token value to prevent it from
    /// appearing in your shell history.
    #[clap(short, long)]
    pub token: Option<String>,

    /// The name of the Discord voice channel to which bardcast should connect.
    #[clap(long)]
    pub voice_channel: Option<String>,

    /// The name of the Discord text channel that bardcast should use to send
    /// metadata and lifecycle event notifications. If not specified (either on
    /// the command line or in the config file passed to -c/--configfile),
    /// bardcast will not post any informational messages.
    #[clap(long)]
    pub metadata_channel: Option<String>,

    /// Path to a file to which to write recorded audio. Only works with audio
    /// consumers that write to file (e.g. "wav").
    #[cfg(feature = "wav")]
    #[clap(short, long)]
    pub output_file: Option<String>,

    /// The unique ID of the Discord server to connect to.
    ///
    /// This server must be accessible by the provided token. In order to
    /// determine the IDs of servers accessible by a given token, run bardcast
    /// with a valid token and the --list-servers option.
    #[clap(short, long)]
    pub server_id: Option<u64>,

    /// Volume of the audio sent to Discord, represented as a decimal from 0 to
    /// 1.
    ///
    /// Some platforms may allow altering this at runtime using the system
    /// mixer. Specifically, this controls the volume level of the record
    /// stream. Any virtual audio devices created by bardcast should remain at
    /// full volume.
    #[clap(short, long)]
    pub volume: Option<f64>,

    /// Number of worker threads to spawn, not including the main thread.
    /// Defaults to 2.
    #[clap(short = 'j', long)]
    pub threads: Option<usize>,

    /// Application verbosity level. One of "off", "error", "warn", "info",
    /// "debug", or "trace". If not specified, this will default to "info".
    #[clap(long)]
    pub log_level: Option<LevelFilter>,

    /// Audio intercept mode, controlling where audio read by bardcast is sent.
    /// Some drivers may not support all options.
    #[clap(short = 'm', long, value_enum)]
    pub intercept_mode: Option<InterceptMode>,

    /// Selects the audio consumer to use when running the application. The
    /// available consumers depend on what features were enabled when bardcast
    /// was built.
    ///
    /// Defaults to the "discord" consumer.
    #[clap(long, value_enum, default_value = "discord")]
    pub consumer: SelectedConsumer,

    /// Print the supported sound drivers and exit.
    #[clap(long, action)]
    pub list_drivers: bool,

    /// Print a list of Discord servers to which the configured token has
    /// access and exit.
    #[clap(long, action)]
    pub list_servers: bool,

    /// Print application version information and exit.
    #[clap(long, action)]
    pub version: bool,
}

/// Represents the audio consumer to be used for a run of the application.
#[derive(Clone, Copy, ValueEnum, Debug, PartialEq)]
pub enum SelectedConsumer {
    Discord,
    #[cfg(feature = "wav")]
    Wav,
}

/// Represents the high-level action that the user has requested. Most of these
/// options correspond to commandline options, except Run(Discord) is assumed
/// if no overriding option is otherwise passed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Run(SelectedConsumer),
    ListServers,
    ListDrivers,
    PrintVersion,
}

/// Represents the approach the application uses to intercept system audio.
#[derive(Clone, Copy, ValueEnum, Debug, PartialEq)]
pub enum InterceptMode {
    /// Capture system audio, preventing it from reaching a hardware audio
    /// output.
    Capture,

    /// Capture system audio without preventing it from reaching a hardware
    /// audio output.
    Peek,

    /// Rather than capture individual application audio streams, simply monitor
    /// all sound played via a specific audio output. WARNING: When using this
    /// mode, be sure the Discord voice chat is not also sending audio to the
    /// same output, or you will get feedback.
    Monitor,
}

/// Common trait shared by config structures that can be constructed from
/// either commandline arguments, or a configuration file.
pub trait Config<'a>: TryFrom<&'a Args> + std::fmt::Debug {
    /// Constructs a configuration instance from the given configuration file.
    fn from_ini(ini: &Ini) -> Result<Self, String>;

    /// Merges a separate instance of this configuration type into this
    /// instance, overwriting current values in `self` if corresponding values
    /// are found to be set in `other`.
    fn merge(&mut self, other: Self);

    /// Performs semantic validation of the configuration, i.e.
    /// misconfigurations that are necessarily correct according to the nature
    /// of the data structure but are semantically incorrect according to the
    /// nature of the underlying code. Typically, such configurations arise from
    /// a combination of configuration parameters, rather than just one.
    ///
    /// The action that the application is to run is provided as an argument, as
    /// that may alter the semantics.
    ///
    /// This will only be called after all config merges are complete.
    fn validate_semantics(&self, action: Action) -> Result<(), String>;
}

/// Top-level application configuration structure.
///
/// Unlike the domain-specific configuration types, this does _not_ implement
/// [`Config`] as this type is responsible for creating the [`Ini`] instance
/// that is passed to the domain-specific types' implementations of [`Config`].
#[derive(Debug)]
pub struct ApplicationConfig {
    /// The configuration for the PulseAudio sound driver.
    #[cfg(all(target_family = "unix", feature = "pulse"))]
    pub pulse: PulseDriverConfig,

    /// The configuration for the Discord client.
    pub discord: DiscordConfig,

    /// The maximum log level to log at.
    pub log_level: Option<LevelFilter>,

    /// The name of the sound driver to use.
    pub driver_name: Option<String>,

    /// A path to the output file to which to write, if using a consumer that
    /// writes to file.
    #[cfg(feature = "wav")]
    pub output_file: Option<String>,

    /// The number of threads to use in the application runtime.
    pub threads: Option<usize>,
}

impl From<&Args> for Action {
    fn from(args: &Args) -> Self {
        if args.version {
            return Action::PrintVersion;
        }

        if args.list_drivers {
            return Action::ListDrivers;
        }

        if args.list_servers {
            return Action::ListServers;
        }

        Action::Run(args.consumer)
    }
}

impl TryFrom<Args> for ApplicationConfig {
    type Error = String;

    fn try_from(args: Args) -> Result<Self, Self::Error> {
        Ok(Self {
            #[cfg(all(target_family = "unix", feature = "pulse"))]
            pulse: PulseDriverConfig::try_from(&args)?,
            discord: DiscordConfig::try_from(&args)?,
            log_level: args.log_level,
            driver_name: args.driver_name,
            #[cfg(feature = "wav")]
            output_file: args.output_file,
            threads: args.threads,
        })
    }
}

impl SelectedConsumer {
    fn needs_output_file(&self) -> bool {
        match self {
            SelectedConsumer::Discord => false,
            #[cfg(feature = "wav")]
            SelectedConsumer::Wav => true,
        }
    }
}

impl ApplicationConfig {
    /// Creates an [`ApplicationConfig`] instance from the given INI file.
    pub fn from_file<T: AsRef<Path>>(filepath: T) -> Result<Self, String> {
        let mut config = Ini::new();

        config.set_default_section(crate_name!());
        config.load(filepath)?;

        Ok(Self {
            #[cfg(all(target_family = "unix", feature = "pulse"))]
            pulse: PulseDriverConfig::from_ini(&config)?,
            discord: DiscordConfig::from_ini(&config)?,
            log_level: validate_log_level(&config.get(crate_name!(), "log-level"))?,
            driver_name: config.get(crate_name!(), "driver"),
            #[cfg(feature = "wav")]
            output_file: config.get("wav", "output-file"),
            threads: validate_thread_count(&config.getuint(crate_name!(), "threads")?)?,
        })
    }

    /// Merges options from `other` into `self`, overriding options in `self`
    /// with values set in `other` if present.
    pub fn merge(&mut self, other: Self) {
        #[cfg(all(target_family = "unix", feature = "pulse"))]
        self.pulse.merge(other.pulse);
        self.discord.merge(other.discord);

        merge_opt(&mut self.log_level, other.log_level);
        merge_opt(&mut self.driver_name, other.driver_name);
        #[cfg(feature = "wav")]
        merge_opt(&mut self.output_file, other.output_file);
        merge_opt(&mut self.threads, other.threads);
    }

    /// Performs semantic validation of the configuration, i.e.
    /// misconfigurations that are necessarily correct according to the nature
    /// of the data structure but are semantically incorrect according to the
    /// nature of the underlying code. Typically, such configurations arise from
    /// a combination of configuration parameters, rather than just one.
    ///
    /// The action that the application is to run is provided as an argument, as
    /// that may alter the semantics.
    ///
    /// This will only be called after all config merges are complete.
    pub fn validate_semantics(&self, action: Action) -> Result<(), String> {
        if let Action::Run(consumer) = action {
            if self.output_file.is_some() && !consumer.needs_output_file() {
                warn!("-o/--output-file does not make sense with the selected audio consumer, ignoring");
            } else if self.output_file.is_none() && consumer.needs_output_file() {
                return Err(String::from("Selected audio consumer requires an output file to be specified"));
            }

            self.pulse.validate_semantics(action)?;
            self.discord.validate_semantics(action)
        } else if action == Action::ListServers {
            self.discord.validate_semantics(action)
        } else {
            Ok(())
        }
    }
}

/// Determines the correct [`LevelFilter`] value represented by the given
/// string, if possible.
fn level_filter_from_string(raw: &str) -> Option<LevelFilter> {
    match raw.to_ascii_uppercase().as_str() {
        "OFF" => Some(LevelFilter::Off),
        "ERROR" => Some(LevelFilter::Error),
        "WARN" => Some(LevelFilter::Warn),
        "INFO" => Some(LevelFilter::Info),
        "DEBUG" => Some(LevelFilter::Debug),
        "TRACE" => Some(LevelFilter::Trace),
        _ => None,
    }
}

/// Determines the correct [`CaptureMode`] value from the given string, if
/// possible.
fn capture_mode_from_string(raw: &str) -> Option<InterceptMode> {
    match raw.to_ascii_uppercase().as_str() {
        "CAPTURE" => Some(InterceptMode::Capture),
        "PEEK" => Some(InterceptMode::Peek),
        "MONITOR" => Some(InterceptMode::Monitor),
        _ => None,
    }
}

/// Overwrites the value present in `into` if `from` is `Some`.
pub fn merge_opt<T>(into: &mut Option<T>, from: Option<T>) {
    if from.is_some() {
        mem::drop(mem::replace(into, from));
    }
}

/// Parses the [`CaptureMode`] from the given string (if present) and performs
/// validation.
pub fn validate_intercept_mode(raw_capture_mode: &Option<String>) -> Result<Option<InterceptMode>, String> {
    raw_capture_mode.as_ref().map(|m| {
        capture_mode_from_string(m).ok_or(String::from("Unknown capture mode"))
    }).transpose()
}

/// Parses a regular expression from the given string and performs validation.
pub fn validate_regex(raw_regex: &Option<String>) -> Result<Option<Regex>, String> {
    raw_regex.as_ref().map(|r| Regex::new(r))
        .transpose()
        .map_err(|e| match e {
            RegexError::Syntax(syn_error) => format!("regex syntax error: {}", syn_error),
            RegexError::CompiledTooBig(limit) => format!("compiled regex exceeded size limit of {}", limit),
            _ => String::from("unknown regex error"),
        })
}

/// Parses a comma-separated list of string values into a set of string values.
pub fn parse_raw_property_list(raw: &Option<String>) -> HashSet<String> {
    raw.as_ref().map(|r| r.split(",").map(|x| String::from(x)).collect())
        .unwrap_or(HashSet::new())
}

/// Parses the [`LevelFilter`] from the given string (if present) and performs
/// validation.
fn validate_log_level(raw_level: &Option<String>) -> Result<Option<LevelFilter>, String> {
    raw_level.as_ref().map(|l| {
        level_filter_from_string(l).ok_or(String::from("Unknown log level"))
    }).transpose()
}

/// Validates that a `u64` can be contained in a `usize`, and returns a
/// validation error if not. Used to determine the number of threads to be used
/// in the application runtime.
fn validate_thread_count(raw_thread_count: &Option<u64>) -> Result<Option<usize>, String> {
    raw_thread_count.as_ref().map(|t| usize::try_from(*t))
        .transpose()
        .map_err(|_| String::from("Too many threads specified"))
}
