///! Audio consumer that streams audio to a .wav file.

use std::borrow::Borrow;
use std::io::{
    BufWriter,
    Error as IoError,
    ErrorKind,
    Read,
};
use std::iter;
use std::mem;
use std::time::{Duration, Instant};

use byteorder::{LittleEndian, ReadBytesExt};
use clap::crate_name;
use log::{error, info, warn};
use tokio::fs::File as AsyncFile;
use tokio::sync::watch::Receiver;
use tokio::task::{self, JoinHandle};
use wave_stream::samples_by_channel::SamplesByChannel;
use wave_stream::wave_header::{Channels, SampleFormat, WavHeader};
use wave_stream::wave_writer::RandomAccessWavWriter;

use crate::snd::StreamNotifier;
use crate::util;

const SAMPLE_RATE: u32 = 48000;
const MAX_RECORDING_WARN_THRESHOLD: Duration = Duration::from_secs(86400); // 1 day

struct WavWriteState<I> {
    writer: RandomAccessWavWriter<f32>,
    input: I,
    last_write: Instant,
    n_sample: usize,
}

// PUBLIC INTERFACE FUNCTIONS **************************************************
/// Starts recording the given stream to a new file named according to
/// `output_file`. If `output_file` is `-`, this will write to standard output.
pub async fn record<I, S>(
    output_file: &str,
    input: I,
    shutdown_rx: Receiver<bool>
) -> Result<JoinHandle<()>, IoError>
where
    I: Read + Borrow<S> + Send + 'static,
    S: StreamNotifier + 'static
{
    let output = BufWriter::new(
        AsyncFile::create(output_file).await?.into_std().await
    );
    let writer = task::spawn_blocking(move || {
        wave_stream::write_wav(output, WavHeader {
            sample_format: SampleFormat::Float,
            channels: Channels::new().front_left().front_right(),
            sample_rate: SAMPLE_RATE,
        })?.get_random_access_f32_writer()
    }).await.map_err(|_| IoError::from(ErrorKind::Other))??;

    if max_recording_len() < MAX_RECORDING_WARN_THRESHOLD {
        warn!(
            "Due to platform limitations, {} can only record {:.2} minutes of \
             audio. The application will shut down if this length is exceeded.",
            crate_name!(),
            max_recording_len().as_secs_f64() / 60f64
        );
    }

    Ok(task::spawn_local(record_monitor(writer, input, shutdown_rx)))
}

// INTERNAL HELPER FUNCTIONS ***************************************************
/// Determines the maximum recording length supported on this platform.
const fn max_recording_len() -> Duration {
    Duration::from_secs((usize::MAX / SAMPLE_RATE as usize) as u64)
}

/// Delays the next iteration of the async stream monitor loop until one of two
/// conditions are met:
/// 
///   1. The stream has data available.
///   2. A shutdown signal is sent.
async fn should_continue_output(
    shutdown_rx: &mut Receiver<bool>,
    stream: &impl StreamNotifier
) -> bool {
    tokio::select! {
        biased;
        shutdown = shutdown_rx.changed() => {
            if shutdown.is_ok() {
                *Receiver::borrow(shutdown_rx)
            } else {
                warn!("Shutdown channel closed unexpectedly, wav writer closing...");
                false
            }
        },
        _ = stream.await_samples() => true,
    }
}

/// Convenience function for fetching the next stereo sample from the given
/// stream.
fn get_next_sample(stream: &mut impl Read) -> Result<(f32, f32), IoError> {
    Ok((stream.read_f32::<LittleEndian>()?, stream.read_f32::<LittleEndian>()?))
}

/// Writes a single sample to the WAV file, attempting to increment the sample
/// position in the process.
fn write_sample(
    writer: &mut RandomAccessWavWriter<f32>,
    sample: (f32, f32),
    n_sample: usize
) -> Result<usize, IoError> {
    writer.write_samples(
        n_sample,
        SamplesByChannel::new().front_left(sample.0).front_right(sample.1)
    )?;

    let res = n_sample.checked_add(1)
        .ok_or(IoError::from(ErrorKind::UnexpectedEof));

    if res.is_err() {
        error!("Recording length limit reached");
        writer.flush()?;
    }

    res
}

/// Adds a number of zero samples (in stereo) to the wav file, to match the
/// amount of time elapsed since the last write.
fn fill_zero_samples(
    writer: &mut RandomAccessWavWriter<f32>,
    last_write: &Instant,
    mut n_sample: usize
) -> Result<usize, IoError> {
    let n_zero_samples = usize::try_from(
        (last_write.elapsed().as_micros() * SAMPLE_RATE as u128) /
        Duration::from_secs(1).as_micros()
    ).map_err(|_| IoError::from(ErrorKind::UnexpectedEof))?;

    for zero_sample in iter::repeat((0f32, 0f32)).take(n_zero_samples) {
        n_sample = write_sample(writer, zero_sample, n_sample)?;
    }

    Ok(n_sample)
}

/// Writes audio data to the audio file configured in `state` using the input
/// stream and related data also present in `state`.
///
/// This will start by adding zero samples to fill the time elapsed since the
/// last contentful write. This then repeatedly polls for audio samples until
/// a read call times out or a shutdown signal is sent, whereupon the state is
/// repackaged and returned to the caller.
fn stream_to_wav<I: Read>(
    mut state: WavWriteState<I>,
    mut shutdown_rx: Receiver<bool>,
) -> Result<WavWriteState<I>, IoError> {
    let mut n_sample = fill_zero_samples(
        &mut state.writer,
        &state.last_write,
        state.n_sample
    )?;
    let mut last_write = state.last_write;

    info!("Starting output wav stream");
    while util::check_shutdown_rx(&mut shutdown_rx) {
        match get_next_sample(&mut state.input) {
            Ok(sample) => {
                n_sample = write_sample(
                    &mut state.writer,
                    sample,
                    n_sample
                )?;
                last_write = Instant::now();
            },
            Err(e) => {
                if e.kind() == ErrorKind::TimedOut {
                    info!("Stream read timed out, wav writer waiting for data");
                    break;
                } else {
                    warn!(
                        "Failed to read sample, discarding from output (error: {})",
                        e
                    );
                }
            }
        }
    }

    Ok(WavWriteState {
        writer: state.writer,
        input: state.input,
        last_write,
        n_sample,
    })
}

/// Monitors the recording process, idling and restarting the write task as
/// necessary.
async fn record_monitor<I, S>(
    writer: RandomAccessWavWriter<f32>,
    input: I,
    mut shutdown_rx: Receiver<bool>
)
where
    I: Read + Borrow<S> + Send + 'static,
    S: StreamNotifier
{
    let mut state = Some(WavWriteState {
        writer,
        input,
        last_write: Instant::now(),
        n_sample: 0usize,
    });

    info!("Starting output wav stream");

    // .unwrap() is safe on state/moved_state prior to being moved into the
    // blocking task, since we explicitly check that it is Some at the top of
    // this loop
    while state.is_some() && should_continue_output(
        &mut shutdown_rx,
        state.as_ref().unwrap().input.borrow()
    ).await {
        let moved_state = mem::take(&mut state);
        let shutdown_rx_clone = shutdown_rx.clone();

        let res = task::spawn_blocking(
            move || stream_to_wav(moved_state.unwrap(), shutdown_rx_clone)
        ).await;

        // Panic here to invoke an application shutdown on either error
        // condition, both are fatal
        match res {
            Ok(Ok(new_state)) => state = Some(new_state),
            Ok(Err(e)) => panic!("Wav writer encountered error: {}", e),
            Err(_) => panic!("Wav writer crashed unexpectedly"),
        }
    }

    if let Some(mut state) = state {
        if let Err(e) = state.writer.flush() {
            warn!("Failed to flush wav file to permanent storage, \
                   some audio data may be missing. {}", e);
        }
    }

    info!("Output wav stream shutting down");
}
