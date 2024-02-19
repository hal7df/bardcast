///! Root module for sound consumers.

pub mod discord;
#[cfg(feature = "wav")]
pub mod wav;

use std::error::Error;

use async_trait::async_trait;
use tokio::sync::watch::Receiver;

use crate::snd::AudioStream;
use crate::util::task::TaskSet;

/// Base trait implementing an audio sink.
///
/// Implementations should provide any configuration necessary to establish a
/// stable audio channel.
#[async_trait]
pub trait AudioConsumer {
    /// Starts consuming audio from `stream`, consuming this `AudioConsumer`.
    ///
    /// The consumer **MUST** stop all operations when `false` is transmitted
    /// on `shutdown_rx`.
    async fn start<S: AudioStream + Send + Sync + 'static>(
        self,
        stream: S,
        shutdown_rx: Receiver<bool>
    ) -> Result<TaskSet<()>, Box<dyn Error>>;
}
