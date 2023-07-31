///! Audio consumer that streams audio to a .wav file.

use std::io::{
    self,
    BufWriter,
    Error as IoError,
    ErrorKind,
    Read,
    Seek,
    Write
};
use std::fs::File;
use std::time::Duration;

use byteorder::{LittleEndian, ReadBytesExt};
use clap::crate_name;
use log::{error, info, warn};
use tokio::sync::watch::Receiver;
use tokio::task::{self, JoinHandle};
use wave_stream;
use wave_stream::samples_by_channel::SamplesByChannel;
use wave_stream::wave_header::{Channels, SampleFormat, WavHeader};

use crate::util;
use crate::util::io::SeekNotSupportedWrite;

const SAMPLE_RATE: u32 = 48000;
const MAX_RECORDING_WARN_THRESHOLD: Duration = Duration::from_secs(86400); // 1 day

// PUBLIC INTERFACE FUNCTIONS **************************************************
/// Starts recording the given stream to a new file named according to
/// `output_file`. If `output_file` is `-`, this will write to standard output.
pub fn record<S: Read + Send + 'static>(
    output_file: &str,
    stream: S,
    shutdown_rx: Receiver<bool>
) -> Result<JoinHandle<()>, IoError> {
    Ok(if output_file == "-" {
        start_stream_to_wav(
            BufWriter::new(SeekNotSupportedWrite::from(io::stdout())),
            stream,
            shutdown_rx
        )
    } else {
        start_stream_to_wav(
            BufWriter::new(File::create(output_file)?),
            stream,
            shutdown_rx
        )
    })
}

// INTERNAL HELPER FUNCTIONS ***************************************************
/// Generic receiver method for [`record`].
fn start_stream_to_wav<O: Write + Seek + Send + 'static, S: Read + Send + 'static>(
    output: O,
    stream: S,
    shutdown_rx: Receiver<bool>
) -> JoinHandle<()> {
    task::spawn_blocking(move || {
        if let Err(e) = stream_to_wav(output, stream, shutdown_rx) {
            panic!("Wav output stream failed with error: {}", e);
        }
    })
}

/// Determines the maximum recording length supported on this platform.
const fn max_recording_len() -> Duration {
    Duration::from_secs((usize::MAX / SAMPLE_RATE as usize) as u64)
}

/// Convenience function for fetching the next stereo sample from the given
/// stream.
fn get_next_sample(stream: &mut impl Read) -> Result<(f32, f32), IoError> {
    Ok((stream.read_f32::<LittleEndian>()?, stream.read_f32::<LittleEndian>()?))
}

/// Writes the given stream to the provided output handle.
fn stream_to_wav<O: Write + Seek + 'static>(
    output: O,
    mut stream: impl Read,
    mut shutdown_rx: Receiver<bool>
) -> Result<(), IoError> {
    let mut writer = wave_stream::write_wav(output, WavHeader {
        sample_format: SampleFormat::Float,
        channels: Channels::new().front_left().front_right(),
        sample_rate: SAMPLE_RATE,
    })?.get_random_access_f32_writer()?;

    if max_recording_len() > MAX_RECORDING_WARN_THRESHOLD {
        warn!(
            "Due to platform limitations, {} can only record {:.2} minutes of audio.\
             The application will shut down if this length is exceeded.",
            crate_name!(),
            max_recording_len().as_secs_f64() / 60f64
        );
    }

    let mut n_sample = 0usize;

    info!("Starting output wav stream");
    while util::check_shutdown_rx(&mut shutdown_rx) {
        match get_next_sample(&mut stream) {
            Ok((left, right)) => {
                writer.write_samples(
                    n_sample,
                    SamplesByChannel::new().front_left(left).front_right(right)
                )?;

                n_sample = if let Some(n_sample) = n_sample.checked_add(1) {
                    n_sample
                } else {
                    error!("Recording length limit reached");
                    writer.flush()?;
                    return Err(IoError::from(ErrorKind::UnexpectedEof));
                }
            },
            Err(e) => {
                warn!(
                    "Failed to read sample, discarding from output (error: {})",
                    e
                );
            }
        }
    }

    writer.flush()?;
    info!("Output wav stream shutting down");
    Ok(())
}
