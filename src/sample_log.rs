#![cfg(debug_assertions)]

///! A basic audio stream consumer that computes audio sample statistics for
///! every quarter second of audio. Statistics are measured as a function of
///! scalar amplitude; the sign information is not preserved.
///!
///! This module is only available in debug builds.

use std::marker::Unpin;

use byteorder::{ByteOrder, LittleEndian};
use futures::io::{AsyncRead, AsyncReadExt};
use log::{debug, error, info};
use tokio::sync::watch::Receiver;

use crate::util;

const SAMPLE_BUF_SIZE: usize = 24000;

/// Computes the minimum, average, and maximum values for the given slice of
/// `i16`s, returning a tuple containing the statistics in that order.
fn compute_stats(samples: &[i16]) -> (i16, i64, i16) {
    let raw_stats = samples.iter().fold(
        (i16::MAX, 0i64, i16::MIN),
        |stats, sample| (
            if sample < &stats.0 { *sample } else { stats.0 },
            stats.1 + i64::from(*sample),
            if sample > &stats.2 { *sample } else { stats.2 }
        )
    );

    (
        raw_stats.0,
        (raw_stats.1 / i64::try_from(samples.len()).unwrap()),
        raw_stats.2
    )
}

/// Core task loop for the sample consumer.
pub async fn log_sample_stats<R: AsyncRead + Unpin>(
    mut stream: R,
    mut shutdown_rx: Receiver<bool>
) {
    let mut buf = [0u8; SAMPLE_BUF_SIZE];

    debug!("Collecting sample statistics");
    while util::check_shutdown_rx(&mut shutdown_rx) {
        if let Err(e) = stream.read_exact(&mut buf).await {
            error!("Stream failed with error: {:?}", e.kind());
        } else {
            let (left, right): (Vec<i16>, Vec<i16>) = buf.chunks(4).map(|sample| (
                LittleEndian::read_i16(&sample[..2]).saturating_abs(),
                LittleEndian::read_i16(&sample[2..]).saturating_abs()
            )).unzip();

            let left_stats = compute_stats(&left);
            let right_stats = compute_stats(&right);

            info!(
                "L(min: {} avg: {} max: {}), R(min: {}, avg: {}, max:{})",
                left_stats.0,
                left_stats.1,
                left_stats.2,
                right_stats.0,
                right_stats.1,
                right_stats.2
            );
        }
    }

    debug!("Sample statistics shut down");
}
