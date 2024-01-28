///! Common types used within and exposed by the snd module.

use std::borrow::{Borrow, BorrowMut};
use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};
use std::io::{Error as IoError, ErrorKind, Read};
use std::time::Duration;

use async_ringbuf::consumer::AsyncConsumer;
use async_ringbuf::ring_buffer::AsyncRbRead;
use async_trait::async_trait;
use futures::future::{self, Either};
use futures::io::{AsyncRead, AsyncReadExt};
use ringbuf::ring_buffer::RbRef;
use tokio::runtime::{Builder as TokioRuntimeBuilder, Runtime};

/// The default timeout for [`SyncStreamAdapter`], if none is otherwise
/// specified.
const DEFAULT_ASYNC_READ_TIMEOUT: Duration = Duration::from_millis(250u64);

// TYPE DEFINITIONS ************************************************************
/// Extension trait to AsyncRead that enables an audio stream reader to
/// non-destructively detect when data is written to the stream. This stream
/// should contain IEEE 754 32-bit float stereo audio samples (where the left
/// audio sample comes before the right) at a sample rate of 48kHz, encoded as
/// bytes.
#[async_trait]
pub trait StreamNotifier {
    /// Waits for at least one stereo audio sample to be written to the stream,
    /// signaling for playback to resume if it has stopped due to a lack of
    /// data.
    async fn await_samples(&self);
}

/// Base trait for types that can be converted into readable audio streams.
pub trait AudioStream {
    /// The concrete implementation returned by `into_async_stream`.
    type AsyncImpl: AsyncRead + StreamNotifier + Unpin + Send + Sync + 'static;

    /// The concrete implementation returned by `into_sync_stream`.
    type SyncImpl: Read + StreamNotifier + Send + Sync + 'static;

    /// Converts this `AudioStream` into a usable Read + StreamNotifier
    /// implementation.
    ///
    /// Audio streams that are inherently asynchronous may accomplish this by
    /// using a timeout on the read operation.
    fn into_sync_stream(
        self,
        read_timeout: Option<Duration>
    ) -> Self::SyncImpl;

    /// Converts this `AudioStream` into a usable AsyncRead + StreamNotifier
    /// implementation.
    fn into_async_stream(self) -> Self::AsyncImpl;
}

/// Adapter to use a type implementing [`AsyncRead`] in a context where a
/// blocking [`Read`] is required, with a configurable read timeout.
pub struct SyncStreamAdapter<A> {
    reader: A,
    runtime: Option<Runtime>,
    async_timeout: Duration,
}

/// High-level representation of errors that can occur during the process of
/// starting/initializing a sound driver.
#[derive(Debug)]
pub enum StartError {
    /// The sound driver failed to connect to its backend. The underlying error
    /// object is wrapped as a trait object.
    ConnectionError(Box<dyn Error>),

    /// The consumer failed to initialize after a successful initialization of
    /// the driver. The underlying error object is wrapped as a trait object.
    ConsumerInitializationError(Box<dyn Error>),

    /// The sound driver encountered an internal error while initializing. The
    /// underlying error object is wrapped as a trait object.
    DriverInitializationError(Box<dyn Error>),

    /// The application failed to automatically find a suitable sound driver.
    NoAvailableBackends,

    /// The user-requeted sound driver does not exist, with the requested sound
    /// driver provided as a string.
    UnknownDriver(String),
}

/// Internal specialization of errors thrown specifically by sound drivers
/// during initialization.
#[derive(Debug)]
pub enum DriverInitError<E> {
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
impl<A: AsyncRead + Unpin> SyncStreamAdapter<A> {
    /// Creates a new `AsyncReadAdapter` from the given [`AsyncRead`]
    /// implementor, and read timeout. If no timeout is given, a timeout of a
    /// quarter second (250ms) is used.
    pub fn new(reader: A, async_timeout: Option<Duration>) -> Self {
        Self {
            reader,
            runtime: None,
            async_timeout: async_timeout.unwrap_or(DEFAULT_ASYNC_READ_TIMEOUT),
        }
    }
}

// TRAIT IMPLS *****************************************************************
#[async_trait]
impl<R: RbRef + Sync + 'static> StreamNotifier for AsyncConsumer<u8, R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    async fn await_samples(&self) {
        self.wait(super::F32LE_STEREO_SAMPLE_SIZE).await;
    }
}

impl<R: RbRef + Send + Sync + 'static> AudioStream for AsyncConsumer<u8, R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    type AsyncImpl = Self;
    type SyncImpl = SyncStreamAdapter<Self>;

    fn into_sync_stream(
        self,
        read_timeout: Option<Duration>
    ) -> Self::SyncImpl {
        SyncStreamAdapter::new(self, read_timeout)
    }

    fn into_async_stream(self) -> Self::AsyncImpl {
        self
    }
}

#[async_trait]
impl<A: StreamNotifier + Sync + Send> StreamNotifier for SyncStreamAdapter<A> {
    async fn await_samples(&self) {
        self.reader.await_samples().await;
    }
}

impl<A> Borrow<A> for SyncStreamAdapter<A> {
    fn borrow(&self) -> &A {
        &self.reader
    }
}

impl<A> BorrowMut<A> for SyncStreamAdapter<A> {
    fn borrow_mut(&mut self) -> &mut A {
        &mut self.reader
    }
}

impl<A: AsyncRead + Unpin> Read for SyncStreamAdapter<A> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if self.runtime.is_none() {
            self.runtime = Some(TokioRuntimeBuilder::new_current_thread()
                .enable_time()
                .build()?
            )
        }

        self.runtime.as_ref().expect(
            "SyncStreamAdapter should have an initialized Runtime"
        ).block_on(async {
            let timeout = tokio::time::sleep(self.async_timeout);
            tokio::pin!(timeout);

            let timed_read_res = future::select(
                self.reader.read(buf),
                timeout
            ).await;

            match timed_read_res {
                Either::Left((res, _)) => res,
                Either::Right(_) => Err(IoError::from(ErrorKind::TimedOut)),
            }
        })
    }
}

impl Display for StartError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            StartError::ConnectionError(e) => write!(
                f,
                "Sound driver failed to connect to backend: {}",
                e
            ),
            StartError::ConsumerInitializationError(e) => write!(
                f,
                "Audio consumer failed to initialize: {}",
                e
            ),
            StartError::DriverInitializationError(e) => write!(
                f,
                "Sound driver failed to initialize: {}",
                e
            ),
            StartError::NoAvailableBackends => write!(
                f,
                "No audio backends available"
            ),
            StartError::UnknownDriver(name) => write!(
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

impl Error for StartError {}

impl<E: Error + 'static> Error for DriverInitError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            DriverInitError::ConnectionError(e) => e,
            DriverInitError::InitializationError(e) => e,
        })
    }
}

impl<E: Error + 'static> From<DriverInitError<E>> for StartError {
    fn from(init_error: DriverInitError<E>) -> Self {
        match init_error {
            DriverInitError::ConnectionError(e) =>
                StartError::ConnectionError(Box::new(e)),
            DriverInitError::InitializationError(e) =>
                StartError::DriverInitializationError(Box::new(e)),
        }
    }
}
