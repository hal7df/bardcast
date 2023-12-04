///! Root module for all application sound drivers.
///!
///! A sound driver consists of the following components:
///!
///!   1. A configuration type to be used in the [`crate::cfg`] module when
///!      building the application's configuration. This type should contain any
///!      configuration options needed to establish a driver session, in
///!      addition to common driver options provided on the command line (e.g.
///!      stream volume, application intercept options).
///!   2. An entry point function that returns a [`Driver`] instance, consisting
///!      of an [`AudioStream`] implementation that provides an IEEE 754 32-bit
///!      floating point stereo audio stream at 48 kHz to be consumed by the
///!      audio consumer, and a collection of [`tokio::task::JoinHandle`]s for
///!      any ongoing tasks the driver spawns to manage the audio stream.
///!   3. A unique name identifying the driver. This should be named according
///!      to the specific audio server or API that it supports, rather than
///!      the name of the operating system it is found on; for example, instead
///!      of a "linux" sound driver, there might be "pulse", "pipewire", or
///!      "jack" sound drivers.
///!
///! Every sound driver should be contolled by conditional compiliation;
///! generally a combination of the operating systems that the driver works
///! with, as well as a feature flag named according to (3) above, to allow
///! users to compile the application without support for the driver if they so
///! choose.

mod types;

#[cfg(all(target_family = "unix", feature = "pulse"))]
pub mod pulse;

use std::mem;

use log::{debug, info};
use tokio::sync::watch::Receiver;

use crate::cfg::ApplicationConfig;

pub use self::types::{
    AsyncAudioStream,
    AudioStream,
    StreamNotifier,
    SyncAudioStream,
    Driver,
    DriverStartError
};

/// List of all sound drivers compiled in the application. When not specified,
/// the first driver with an available backend is selected.
const DRIVERS: &'static [&'static str] = &[
    #[cfg(all(target_family = "unix", feature = "pulse"))]
    pulse::DRIVER_NAME
];

/// The size of a stereo IEEE 754 32-bit floating point audio sample.
const F32LE_STEREO_SAMPLE_SIZE: usize = mem::size_of::<f32>() * 2;

// PUBLIC INTERFACE FUNCTIONS **************************************************
/// Selects the appropriate sound driver based on the provided configuration and
/// starts it, if possible. If a sound driver is not explicitly specified by the
/// user, this will select the first availble sound driver and start it,
/// according to the order of the [`DRIVERS`] constant.
pub async fn select_and_start_driver(
    config: &ApplicationConfig,
    shutdown_rx: Receiver<bool>
) -> Result<Driver, DriverStartError> {
    info!("Configured sound drivers: {}", DRIVERS.join(", "));

    if let Some(driver_name) = &config.driver_name {
        start_driver(driver_name.as_str(), config, &shutdown_rx).await
    } else {
        autodetect_driver(config, &shutdown_rx).await
    }
}

/// Prints a list of all sound drivers availble in the application to standard
/// output.
pub fn list_drivers() {
    for driver in DRIVERS {
        println!("{}", driver);
    }
}

// HELPER FUNCTIONS ************************************************************
/// Attempts to start the driver with the given name.
async fn start_driver(
    driver_name: &str,
    config: &ApplicationConfig,
    shutdown_rx: &Receiver<bool>
) -> Result<Driver, DriverStartError> {
    let start_result: Result<Driver, DriverStartError> = match driver_name {
        #[cfg(all(target_family = "unix", feature = "pulse"))]
        pulse::DRIVER_NAME => pulse::start_driver(
            &config.pulse,
            shutdown_rx.clone()
        ).await.map_err(|e| e.into()),
        _ => Err(DriverStartError::UnknownDriver(driver_name.to_string())),
    };

    if start_result.is_ok() {
        info!("Started application using driver '{}'", driver_name);
    }

    start_result
}

/// Iterates through all known sound drivers and returns the [`Driver`] handle
/// for the first driver that successfully connects to its backend.
///
/// If a driver successfully connects to its backend but fails to properly
/// initialize, this will stop searching for drivers and bubble the error up
/// the call stack.
async fn autodetect_driver(
    config: &ApplicationConfig,
    shutdown_rx: &Receiver<bool>
) -> Result<Driver, DriverStartError> {
    for driver_name in DRIVERS.iter() {
        match start_driver(driver_name, config, shutdown_rx).await {
            Ok(driver) => return Ok(driver),
            Err(DriverStartError::ConnectionError(msg)) => debug!("Driver '{}' failed to connect: {}", driver_name, msg),
            Err(e) => return Err(e),
        }
    }

    Err(DriverStartError::NoAvailableBackends)
}
