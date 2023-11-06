///! I/O utilities for data streams.

use std::borrow::{Borrow, BorrowMut};
use std::convert::From;
use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom};
use std::time::Duration;

use async_ringbuf::consumer::AsyncConsumer;
use async_ringbuf::ring_buffer::AsyncRbRead;
use futures::executor;
use futures::future::{self, Either};
use futures::io::{AsyncRead, AsyncReadExt};
use ringbuf::ring_buffer::RbRef;
use songbird::input::reader::MediaSource;

pub const DEFAULT_ASYNC_READ_TIMEOUT: Duration = Duration::from_millis(250u64);

// TYPE DEFINITIONS ************************************************************
/// Adapter to use a type implementing [`AsyncRead`] in a context where a
/// blocking [`Read`] is required, with a configurable read timeout.
pub struct AsyncReadAdapter<A: AsyncRead + Unpin> {
    reader: A,
    async_timeout: Duration,
}

/// Wrapper around an [`AsyncConsumer`] that allows it to be used as an audio
/// stream in the application.
pub struct AsyncConsumerReadWrapper<R: RbRef>(AsyncConsumer<u8, R>)
where <R as RbRef>::Rb: AsyncRbRead<u8>;

// TYPE IMPLS ******************************************************************
impl<A: AsyncRead + Unpin> AsyncReadAdapter<A> {
    /// Creates a new `AsyncReadAdapter` from the given [`AsyncRead`]
    /// implementor, and read timeout. If no timeout is given, a timeout of a
    /// quarter second (250ms) is used.
    pub fn new(reader: A, async_timeout: Option<Duration>) -> Self {
        Self {
            reader,
            async_timeout: async_timeout.unwrap_or(DEFAULT_ASYNC_READ_TIMEOUT),
        }
    }
}

// TRAIT IMPLS *****************************************************************
impl<A: AsyncRead + Unpin> Borrow<A> for AsyncReadAdapter<A> {
    fn borrow(&self) -> &A {
        &self.reader
    }
}

impl<A: AsyncRead + Unpin> BorrowMut<A> for AsyncReadAdapter<A> {
    fn borrow_mut(&mut self) -> &mut A {
        &mut self.reader
    }
}

impl<A: AsyncRead + Unpin> Read for AsyncReadAdapter<A> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        executor::block_on(async {
            let timeout = tokio::time::sleep(self.async_timeout);
            tokio::pin!(timeout);

            let timed_read_res = future::select(
                self.reader.read(buf),
                timeout
            ).await;

            match timed_read_res {
                Either::Left((res, _)) => res,
                Either::Right(_) => Err(Error::from(ErrorKind::TimedOut)),
            }
        })
    }
}

impl<R: RbRef> From<AsyncConsumer<u8, R>> for AsyncConsumerReadWrapper<R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    fn from(rb_consumer: AsyncConsumer<u8, R>) -> Self {
        Self(rb_consumer)
    }
}

impl<R: RbRef> Borrow<AsyncConsumer<u8, R>> for AsyncConsumerReadWrapper<R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    fn borrow(&self) -> &AsyncConsumer<u8, R> {
        &self.0
    }
}

impl<R: RbRef> BorrowMut<AsyncConsumer<u8, R>> for AsyncConsumerReadWrapper<R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    fn borrow_mut(&mut self) -> &mut AsyncConsumer<u8, R> {
        &mut self.0
    }
}

impl<R: RbRef> Read for AsyncConsumerReadWrapper<R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        //Although the underlying ringbuf implements Read, it returns WouldBlock
        //if the buffer is empty. Because this trait is only ever used in a
        //blocking context, this causes the stream to crash prematurely if the
        //consumer happens to call read() before the producer has had a chance
        //to fill the ringbuffer. So, instead we call AsyncRead in a lightweight
        //executor, and only return once we know we have data.
        executor::block_on(self.0.read(buf))
    }
}

// AsyncConsumerReadWrapper doesn't actually support seeking, but the
// MediaSource trait depends on it (MediaSources can specify that they are not
// seekable within the MediaSource trait)
impl<R: RbRef> Seek for AsyncConsumerReadWrapper<R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    fn seek(&mut self, _: SeekFrom) -> Result<u64> {
        Err(Error::from(ErrorKind::Unsupported))
    }
}

impl<R: RbRef + Send + Sync> MediaSource for AsyncConsumerReadWrapper<R>
where <R as RbRef>::Rb: AsyncRbRead<u8> {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }
}
