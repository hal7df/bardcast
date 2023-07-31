///! I/O utilities for data streams.

use std::borrow::{Borrow, BorrowMut};
use std::convert::From;
use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom, Write};

use async_ringbuf::consumer::AsyncConsumer;
use async_ringbuf::ring_buffer::AsyncRbRead;
use futures::executor;
use futures::io::AsyncReadExt;
use ringbuf::ring_buffer::RbRef;
use songbird::input::reader::MediaSource;

// TYPE DEFINITIONS ************************************************************
pub struct SeekNotSupportedWrite<W: Write>(W);

/// Wrapper around an [`AsyncConsumer`] that allows it to be used as an audio
/// stream in the application.
pub struct AsyncConsumerReadWrapper<R: RbRef>(AsyncConsumer<u8, R>)
where <R as RbRef>::Rb: AsyncRbRead<u8>;

// TRAIT IMPLS *****************************************************************
impl<W: Write> From<W> for SeekNotSupportedWrite<W> {
    fn from(write: W) -> Self {
        Self(write)
    }
}

impl<W: Write> Write for SeekNotSupportedWrite<W> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush()
    }
}

impl<W: Write> Seek for SeekNotSupportedWrite<W> {
    fn seek(&mut self, _pos: SeekFrom) -> Result<u64> {
        Err(Error::from(ErrorKind::Unsupported))
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
