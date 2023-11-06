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

#[cfg(all(target_family = "unix", feature = "pulse"))]
pub mod pulse;

use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};
use std::mem;
use std::pin::Pin;

use async_ringbuf::consumer::AsyncConsumer;
use async_ringbuf::ring_buffer::AsyncRbRead;
use async_trait::async_trait;
use futures::io::AsyncRead;
use log::{debug, info};
use ringbuf::ring_buffer::RbRef;
use tokio::sync::watch::Receiver;

use crate::cfg::ApplicationConfig;
use crate::util::task::TaskContainer;

/// List of all sound drivers compiled in the application. When not specified,
/// the first driver with an available backend is selected.
const DRIVERS: &'static [&'static str] = &[
    #[cfg(all(target_family = "unix", feature = "pulse"))]
    pulse::DRIVER_NAME
];

/// The size of a stereo IEEE 754 32-bit floating point audio sample.
const F32LE_STEREO_SAMPLE_SIZE: usize = mem::size_of::<f32>() * 2;

// TYPE DEFINITIONS ************************************************************

/// Extension trait to AsyncRead that enables an audio stream reader to
/// non-destructively detect when data is written to the stream. This stream
/// should contain IEEE 754 32-bit float stereo audio samples (where the left
/// audio sample comes before the right) at a sample rate of 48kHz, encoded as
/// bytes.
#[async_trait]
pub trait AudioStream: AsyncRead + Send + Sync {
    /// Waits for at least one stereo audio sample to be written to the stream,
    /// signaling for playback to resume if it has stopped due to a lack of
    /// data.
    async fn await_samples(&self);
}

/// Wrapper struct for the two functional components of an audio driver: its
/// stream and any tasks it needs to stay alive.
pub struct Driver {
    stream: Pin<Box<dyn AudioStream>>,
    driver_tasks: Box<dyn TaskContainer<()>>,
}

/// High-level representation of errors that can occur during the process of
/// starting/initializing a sound driver.
#[derive(Debug)]
pub enum DriverStartError {
    /// The sound driver failed to connect to its backend. The underlying error
    /// object is wrapped as a trait object.
    ConnectionError(Box<dyn Error>),

    /// The sound driver encountered an internal error while initializing. The
    /// underlying error object is wrapped as a trait object.
    InitializationError(Box<dyn Error>),

    /// The application failed to automatically find a suitable sound driver.
    NoAvailableBackends,

    /// The user-requeted sound driver does not exist, with the requested sound
    /// driver provided as a string.
    UnknownDriver(String),
}

/// Internal specialization of errors thrown specifically by sound drivers
/// during initialization.
#[derive(Debug)]
enum DriverInitError<E> {
    /// The sound driver failed to connect to its backend, with a
    /// driver-provided error type providing details.
    ///
    /// When a driver returns this error during driver autoselection, the
    /// application will attempt to startthe next configured sound driver, if
    /// any.
    ConnectionError(E),

    /// The sound driver encountered an internal error while initializing, but
    /// was otherwise able to communicate with its backend. A driver-provided
    /// error type carries the details of the failure.
    ///
    /// When a driver returns this error during driver autoselection, the
    /// application will shut down without attempting to start any other sound
    /// drivers.
    InitializationError(E),
}

// TYPE IMPLS ******************************************************************
impl Driver {
    /// Creates a new instance from the given stream and set of tasks.
    fn new<S, T>(stream: S, tasks: T) -> Self
    where S: AudioStream + 'static,
          T: TaskContainer<()> + 'static {
        Self {
            stream: Box::pin(stream),
            driver_tasks: Box::new(tasks),
        }
    }

    /// Consumes this driver into its constituent parts.
    pub fn into_tuple(self) -> (Pin<Box<dyn AudioStream>>, Box<dyn TaskContainer<()>>) {
        (self.stream, self.driver_tasks)
    }
}

// TRAIT IMPLS *****************************************************************
#[async_trait]
impl<R: RbRef + Send + Sync + 'static> AudioStream for AsyncConsumer<u8, R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    async fn await_samples(&self) {
        self.wait(F32LE_STEREO_SAMPLE_SIZE).await;
    }
}

impl Display for DriverStartError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            DriverStartError::ConnectionError(e) => write!(
                f,
                "Sound driver failed to connect to backend: {}",
                e
            ),
            DriverStartError::InitializationError(e) => write!(
                f,
                "Sound driver failed to initialize: {}",
                e
            ),
            DriverStartError::NoAvailableBackends => write!(
                f,
                "No audio backends available"
            ),
            DriverStartError::UnknownDriver(name) => write!(
                f,
                "No such sound driver '{}'",
                name
            ),
        }
    }
}

impl<E: Display> Display for DriverInitError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            DriverInitError::ConnectionError(e) => write!(
                f,
                "Sound driver failed to connect to backend: {}",
                e
            ),
            DriverInitError::InitializationError(e) => write!(
                f,
                "Sound driver failed to initialize: {}",
                e
            ),
        }
    }
}

impl Error for DriverStartError {}

impl<E: Error + 'static> Error for DriverInitError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            DriverInitError::ConnectionError(e) => e,
            DriverInitError::InitializationError(e) => e,
        })
    }
}

impl<E: Error + 'static> From<DriverInitError<E>> for DriverStartError {
    fn from(init_error: DriverInitError<E>) -> Self {
        match init_error {
            DriverInitError::ConnectionError(e) =>
                DriverStartError::ConnectionError(Box::new(e)),
            DriverInitError::InitializationError(e) =>
                DriverStartError::InitializationError(Box::new(e)),
        }
    }
}

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
