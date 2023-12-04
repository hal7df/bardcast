///! Discord audio client.

pub mod cfg;
mod client;
pub mod error;

use std::cmp;
use std::error::Error;
use std::io::{Error as IoError, ErrorKind, Read, Seek, SeekFrom};
use std::mem;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{self, Either, FutureExt, TryFutureExt};
use futures::stream::{self, Stream, TryStreamExt};
use log::{error, info, warn};
use serenity::CacheAndHttp;
use serenity::http::{CacheHttp, GuildPagination, Http};
use serenity::model::guild::{GuildInfo, PartialGuild};
use serenity::model::id::{ChannelId, GuildId};
use serenity::model::user::User;
use serenity::prelude::SerenityError;
use songbird::{Call, Songbird};
use songbird::error::TrackError;
use songbird::input::{Container, Input};
use songbird::input::codec::Codec;
use songbird::input::reader::{MediaSource, Reader};
use songbird::tracks::TrackHandle;
use stream_flatten_iters::TryStreamExt as _;
use tokio::sync::Mutex;
use tokio::sync::oneshot::{self, Sender as OneshotSender};
use tokio::sync::watch::Receiver as WatchReceiver;
use tokio::task::JoinHandle;

use crate::snd::{AudioStream, StreamNotifier};
use crate::util::Lessor;
use crate::util::fmt as fmt_util;
use crate::util::task::{TaskContainer, TaskSetBuilder};
use self::cfg::DiscordConfig;
use self::client::{ChannelResolutionReceiver, Channels};
use self::error::DiscordError;
use super::AudioConsumer;

const GUILD_ID_HEADER: &'static str = "Server ID";
const GUILD_NAME_HEADER: &'static str = "Server Name";
const OWNER_HEADER: &'static str = "Owner";
const DEFAULT_FETCH_SIZE: u64 = 100;

// TYPE DEFINITIONS ************************************************************
/// Discord [`AudioCosumer`] implementation.
pub struct DiscordConsumer<'a>(&'a DiscordConfig, Option<Duration>);

/// Internal representation of state for a paginated query of Discord servers.
#[derive(Debug, PartialEq, Eq)]
enum GuildEnumerationState {
    /// Guild enumeration has not yet been started.
    NotStarted,

    /// The last iteration of guild enumeration ended with the given guild ID.
    LastGuild(GuildId),

    /// All guilds have been queried from the API.
    Exhausted
}

/// Handler for an active audio connection to Discord.
struct ConnectedClient {
    /// All channels used by the application, resolved to their IDs.
    channels: Channels<ChannelId>,

    /// A handle to the event handler task.
    client_task: JoinHandle<Result<(), SerenityError>>,

    /// A channel write handle to communicate exit conditions to the
    /// `client_task`.
    client_tx: Option<OneshotSender<()>>,

    /// A handle to the active audio connection to Discord.
    call: Arc<Mutex<Call>>,

    /// A handle for making API calls to Discord.
    cache_http: Arc<CacheAndHttp>,
}

/// Adapter type for implementing MediaSource on existing Read types.
struct MediaSourceAdapter<R>(R);

// TYPE IMPLS ******************************************************************
impl<'a> DiscordConsumer<'a> {
    fn new(cfg: &'a DiscordConfig, read_timeout: Option<Duration>) -> Self {
        Self(cfg, read_timeout)
    }
}

impl ConnectedClient {
    /// Creates a client from the given config, resolving the configured
    /// channels and connecting to the desired voice chat.
    async fn new(cfg: &DiscordConfig) -> Result<Self, DiscordError> {
        //Step 0: Get configuration information
        let token = cfg.token.as_ref().ok_or(
            DiscordError::MissingRequiredProperty(String::from("token"))
        )?;
        let channels = Channels::try_from(cfg)?;
        let songbird = Songbird::serenity();

        //Step 1: Create the client
        let mut client = client::get(
            token,
            Some(channels),
            Some(songbird.clone())
        ).await?;
        let cache_http = client.cache_and_http.clone();

        //Step 2: Start the client and wait for the resolved channels
        let channel_rx = client.data
            .write().await
            .remove::<ChannelResolutionReceiver>()
            .ok_or(DiscordError::InitializationError)?;

        let (client_tx, client_rx) = oneshot::channel();
        let client_task = tokio::spawn(async move {
            let shutdown_res = if let Either::Right((Err(e), _)) = future::select(
                client_rx,
                client.start().boxed()
            ).await {
                Err(e)
            } else {
                Ok(())
            };

            //Regardless of the result, we're shutting down, so terminate the
            //connection with Discord
            client.shard_manager.lock().await.shutdown_all().await;

            shutdown_res
        });

        let channels = match channel_rx.await {
            Ok(Ok(channels)) => channels,
            Err(_) => {
                //Channel closed without sending a response
                client_task.abort();
                return Err(DiscordError::InitializationError)
            },
            Ok(Err(lookup_err)) => {
                client_task.abort();
                return Err(lookup_err)
            },
        };

        //Step 3: Connect to the voice channel
        let (call, result) = songbird.join(channels.guild_id, channels.voice).await;

        if let Err(e) = result {
            client_task.abort();
            return Err(e.into());
        }

        Ok(Self {
            channels,
            client_task,
            client_tx: Some(client_tx),
            call,
            cache_http
        })
    }

    /// Entry point for the main Discord client logic. Panics if the client
    /// exits abnormally.
    async fn run<S: Read + StreamNotifier + Send + Sync + 'static>(
        mut self,
        stream: S,
        mut shutdown_rx: WatchReceiver<bool>
    ) {
        let mut stream = Lessor::new(stream);
        let mut playback: Option<TrackHandle> = None;
        let mut runtime_err: Result<(), DiscordError> = Ok(());

        self.channels.hello(&self.cache_http).await;

        //Main consumer event loop
        while runtime_err.is_ok() {
            tokio::select! {
                //The client task shouldn't exit on its own, but handle it in
                //case it does
                client_crash = &mut self.client_task, if !self.client_task.is_finished() => {
                    match client_crash {
                        Err(e) => runtime_err = Err(e.into()),
                        Ok(Err(e)) => runtime_err = Err(e.into()),
                        Ok(Ok(_)) => warn!(
                            "Discord client background handler task \
                             unexpectedly quit without error"
                        ),
                    }
                },
                shutdown_changed = shutdown_rx.changed() => {
                    if shutdown_changed.is_err() || !*shutdown_rx.borrow() {
                        break;
                    }
                },
                new_playback = monitor_playback(
                    &mut stream,
                    &self.call,
                    playback.as_ref()
                ) => {
                    playback = new_playback;
                }
            }
        }

        //Tear down the connection
        self.cleanup(playback);

        //If the shutdown condition is an error, then panic the thread to cause
        //the rest of the application to shut down
        if let Err(e) = runtime_err {
            panic!("Discord client crashed: {}", e);
        }
    }

    /// Terminates any playing audio and posts a disconnect message to the
    /// configured metadata channel (if any).
    async fn cleanup(mut self, playback: Option<TrackHandle>) {
        //Stop playing audio and disconnect from the voice chat
        if let Some(playback) = playback {
            stop_playback(&playback);
        }

        if let Err(e) = self.call.lock().await.leave().await {
            warn!("Failed to leave voice channel: {}", e);
        }

        //Post a message that the application is disconnecting
        self.channels.goodbye(self.cache_http.http());

        //Shut down the client handler thread, if it is still running
        if let Some(client_tx) = mem::take(&mut self.client_tx) {
            if !self.client_task.is_finished() {
                if client_tx.send(()).is_err() {
                    warn!("Client task still running after dropping shutdown \
                          channel, it will be forcibly aborted");
                    self.client_task.abort();
                } else {
                    match (&mut self.client_task).await {
                        Err(e) => if e.is_panic() {
                            warn!("Client task panicked during shutdown");
                        },
                        Ok(Err(e)) => {
                            warn!("Client task terminated with error: {}", e);
                        },
                        _ => {},
                    }
                }
            }
        }
    }
}

// TRAIT IMPLS *****************************************************************
#[async_trait]
impl<'a> AudioConsumer for DiscordConsumer<'a> {
    async fn start<S: AudioStream + Send + Sync>(
        self,
        stream: S,
        shutdown_rx: WatchReceiver<bool>
    ) -> Result<Box<dyn TaskContainer<()>>, Box<dyn Error>> {
        let mut tasks = TaskSetBuilder::new();
        let client = ConnectedClient::new(self.0).await?;

        tasks.insert(tokio::spawn(client.run(
            stream.into_sync_stream(self.1),
            shutdown_rx
        )));
        Ok(Box::new(tasks.build()))
    }
}

impl From<GuildEnumerationState> for Option<GuildPagination> {
    fn from(state: GuildEnumerationState) -> Self {
        if let GuildEnumerationState::LastGuild(id) = state {
            Some(GuildPagination::After(id))
        } else {
            None
        }
    }
}

impl<R> From<R> for MediaSourceAdapter<R>
where
    R: DerefMut + Send + Sync,
    <R as Deref>::Target: Read
{
    fn from(stream: R) -> Self {
        MediaSourceAdapter(stream)
    }
}

impl<R> Seek for MediaSourceAdapter<R> {
    fn seek(&mut self, _: SeekFrom) -> Result<u64, IoError> {
        Err(IoError::from(ErrorKind::Unsupported))
    }
}

impl<R> Read for MediaSourceAdapter<R>
where
    R: DerefMut + Send + Sync,
    <R as Deref>::Target: Read
{
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        (*self.0).read(buf)
    }
}

impl<R> MediaSource for MediaSourceAdapter<R>
where
    R: DerefMut + Send + Sync,
    <R as Deref>::Target: Read
{
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

impl Drop for ConnectedClient {
    fn drop(&mut self) {
        if !self.client_task.is_finished() {
            self.client_task.abort();
        }
    }
}

// PUBLIC INTERFACE FUNCTIONS **************************************************
/// Connects to Discord using the provided config and begins playing audio into
/// the configured voice chat until shutdown.
pub async fn send_audio<S>(
    stream: S,
    cfg: &DiscordConfig,
    shutdown_rx: WatchReceiver<bool>
) -> Result<JoinHandle<()>, DiscordError>
where S: Read + StreamNotifier + Send + Sync + 'static {
    Ok(tokio::spawn(ConnectedClient::new(cfg).await?.run(stream, shutdown_rx)))
}

/// Prints a list of Discord servers available to the given bot token, or
/// returns an error if one is encountered while generating the list of servers.
pub async fn list_guilds(token: impl AsRef<str>) -> Result<(), SerenityError> {
    let client = client::get(token, None, None).await?;

    // Fetch a list of basic metadata for all available servers
    let http = &client.cache_and_http.http;
    let guild_data = enumerate_guilds(http)
        .and_then(|guild| http.get_guild(guild.id.0))
        .and_then(|guild| http
            .get_user(guild.owner_id.0)
            .map_ok(move |owner| (guild, owner))
        )
        .try_collect::<Vec<(PartialGuild, User)>>().await?;

    if guild_data.is_empty() {
        info!("No servers were found with the given token. \
               Ensure your bot is invited to at least one server.");
        return Ok(());
    }

    //Determine the display widths for the first two columns
    let guild_id_width = cmp::max(
        GUILD_ID_HEADER.len(),
        fmt_util::max_display_length(
            guild_data.iter().map(|guild_and_owner| guild_and_owner.0.id.0)
        )
    );
    let guild_name_width = cmp::max(
        GUILD_NAME_HEADER.len(),
        fmt_util::max_display_length(
            guild_data.iter().map(|guild_and_owner| &guild_and_owner.0.name)
        )
    );

    //Print out the metadata
    println!(
        "{: <guild_id_width$}\t{: <guild_name_width$}\t{}",
        GUILD_ID_HEADER,
        GUILD_NAME_HEADER,
        OWNER_HEADER
    );
    for (guild, owner) in guild_data {
        println!(
            "{: <guild_id_width$}\t{: <guild_name_width$}\t{}#{:0>4}",
            guild.id.0,
            guild.name,
            owner.name,
            owner.discriminator
        );
    }

    Ok(())
}

// HELPER FUNCTIONS ************************************************************
/// Asynchronously generates a list of all guilds available via the given API
/// object.
fn enumerate_guilds<'a>(
    api: &'a Http
) -> impl Stream<Item = Result<GuildInfo, SerenityError>> + 'a {
    stream::try_unfold(
        GuildEnumerationState::NotStarted,
        |state| async {
            if state == GuildEnumerationState::Exhausted {
                return Ok(None);
            }

            let guilds = api.get_guilds(
                Option::<GuildPagination>::from(state).as_ref(),
                Some(DEFAULT_FETCH_SIZE)
            ).await;

            match guilds {
                Ok(guilds) => {
                    let next_state = if let Some(last) = guilds.last() {
                        GuildEnumerationState::LastGuild(last.id)
                    } else {
                        GuildEnumerationState::Exhausted
                    };

                    Ok(Some((guilds, next_state)))
                },
                Err(e) => Err(e),
            }
        }
    ).try_flatten_iters()
}

fn stop_playback(playback: &TrackHandle) {
    if let Err(e) = playback.stop() {
        warn!("Failed to gracefully stop audio playback: {}", e);
    }
}

async fn monitor_playback<S>(
    stream: &mut Lessor<S>,
    call: &Arc<Mutex<Call>>,
    playback: Option<&TrackHandle>
) -> Option<TrackHandle>
where S: Read + StreamNotifier + Send + Sync + 'static {
    /*
     * It is safe to hold a leased stream across an await point, since a
     * cancelled future from this function will result in the lease being
     * dropped, and the value being returned to the lessor.
     *
     * (This does require an extra call to this function to reclaim the value)
     */
    if let Some(stream_lease) = stream.lease() {
        //Wait for data to be written the stream
        stream_lease.await_samples().await;

        //Start playback
        Some(call.lock().await.play_only_source(Input::new(
            true,
            Reader::Extension(Box::new(MediaSourceAdapter::from(stream_lease))),
            Codec::FloatPcm,
            Container::Raw,
            None
        )))
    } else {
        //Wait for playback to terminate naturally, and reclaim the stream
        stream.await_release().await;

        //Make sure the track has stopped
        if let Some(playback) = playback {
            match playback.get_info().await {
                Ok(state) => if !state.playing.is_done() {
                    stop_playback(playback);
                },
                Err(TrackError::Finished) => {},
                Err(e) => error!(
                    "Encountered unexpected error when checking playback status: {}",
                    e
                ),
            }
        }

        None
    }
}
