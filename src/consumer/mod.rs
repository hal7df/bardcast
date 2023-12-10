///! Root module for sound consumers.

pub mod discord;
#[cfg(feature = "wav")]
pub mod wav;

use std::error::Error;

use async_trait::async_trait;
use tokio::sync::watch::Receiver;

use crate::snd::AudioStream;
use crate::util::task::TaskSet;

#[async_trait]
pub trait AudioConsumer {
    async fn start<S: AudioStream + Send + Sync + 'static>(
        self,
        stream: S,
        shutdown_rx: Receiver<bool>
    ) -> Result<TaskSet<()>, Box<dyn Error>>;
}
