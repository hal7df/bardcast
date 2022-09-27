///! Discord audio client.

pub mod cfg;
mod client;
pub mod error;

use std::cmp;
use std::sync::Arc;

use clap::{crate_name, crate_version};
use futures::future::TryFutureExt;
use futures::stream::{self, Stream, TryStreamExt};
use log::{info, warn};
use serenity::CacheAndHttp;
use serenity::http::{CacheHttp, GuildPagination, Http};
use serenity::model::guild::{GuildInfo, PartialGuild};
use serenity::model::id::{ChannelId, GuildId};
use serenity::model::user::User;
use serenity::prelude::SerenityError;
use serenity::utils::MessageBuilder;
use songbird::{Call, Songbird};
use songbird::input::{Container, Input};
use songbird::input::codec::Codec;
use songbird::input::reader::Reader;
use songbird::tracks::TrackHandle;
use stream_flatten_iters::TryStreamExt as _;
use tokio::sync::Mutex;
use tokio::sync::watch::Receiver as WatchReceiver;
use tokio::task::JoinHandle;

use crate::snd::AudioStream;
use crate::util::fmt as fmt_util;
use self::cfg::DiscordConfig;
use self::client::{ChannelResolutionReceiver, Channels};
use self::error::DiscordError;

const GUILD_ID_HEADER: &'static str = "Server ID";
const GUILD_NAME_HEADER: &'static str = "Server Name";
const OWNER_HEADER: &'static str = "Owner";
const DEFAULT_FETCH_SIZE: u64 = 100;

// TYPE DEFINITIONS ************************************************************

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

    /// A handle to the active audio connection to Discord.
    call: Arc<Mutex<Call>>,

    /// A handle for making API calls to Discord.
    cache_http: Arc<CacheAndHttp>,
}

// TYPE IMPLS ******************************************************************
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

        let client_task = tokio::spawn(async move {
            client.start().await
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
            call,
            cache_http
        })
    }

    /// Entry point for the main Discord client logic. Panics if the client
    /// exits abnormally.
    async fn run(
        mut self,
        stream: Box<dyn AudioStream>,
        shutdown_rx: WatchReceiver<bool>
    ) {
        let track = self.startup(stream).await;
        let shutdown_res = self.until_shutdown(shutdown_rx).await;
        self.cleanup(track).await;

        //If the shutdown condition is an error, then panic the thread to cause
        //the rest of the application to shut down
        if let Err(e) = shutdown_res {
            panic!("Discord client crashed: {}", e);
        }
    }

    /// Sends a startup message to the metadata channel (if configured) and
    /// begins playing audio into the connected voice chat.
    async fn startup(&self, stream: Box<dyn AudioStream>) -> TrackHandle {
        //Try to post an initialization message to the configured metadata
        //channel
        if let Some(metadata_channel) = self.channels.metadata {
            match self.channels.voice.to_channel(&self.cache_http).await {
                Ok(voice_channel) => {
                    let msg = MessageBuilder::new()
                        .push(crate_name!())
                        .push(" version ")
                        .push(crate_version!())
                        .push(" connected to voice channel ")
                        .mention(&voice_channel)
                        .build();

                    if let Err(e) = metadata_channel.say(
                        self.cache_http.http(),
                        msg
                    ).await {
                        warn!(
                            "Failed to post initialization message to Discord: {}",
                            e
                        );
                    }
                },
                Err(e) => {
                    warn!(
                        "Could not post initialization message to Discord due \
                         to failure to look up voice channel metadata: {}",
                        e
                    );
                }
            }
        }

        self.call.lock().await.play_only_source(Input::new(
            true,
            Reader::Extension(stream.into_media_source()),
            Codec::Pcm,
            Container::Raw,
            None
        ))
    }

    /// Main monitor loop for the Discord client once it has been fully
    /// initialized. Returns `Err(DiscordError)` if the client exits abnormally.
    async fn until_shutdown(
        &mut self,
        mut shutdown_rx: WatchReceiver<bool>
    ) -> Result<(), DiscordError> {
        loop {
            tokio::select! {
                //The client task shouldn't exit on its own, but handle it in
                //case it does
                client_crash = &mut self.client_task, if !self.client_task.is_finished() => {
                    match client_crash {
                        Err(e) => return Err(e.into()),
                        Ok(Err(e)) => return Err(e.into()),
                        Ok(Ok(_)) => warn!(
                            "Discord client background handler task unexpectedly quit without error"
                        ),
                    }
                },
                shutdown_changed = shutdown_rx.changed() => if shutdown_changed.is_err() || !*shutdown_rx.borrow() {
                    return Ok(());
                }
            }
        }
    }

    /// Terminates any playing audio and posts a disconnect message to the
    /// configured metadata channel (if any).
    async fn cleanup(&self, track: TrackHandle) {
        //Stop playing audio and disconnect from the voice chat
        if let Err(e) = track.stop() {
            warn!("Failed to gracefully stop audio playback: {}", e);
        }

        if let Err(e) = self.call.lock().await.leave().await {
            warn!("Failed to leave voice channel: {}", e);
        }

        //Post a message that the application is disconnecting
        if let Some(metadata_channel) = self.channels.metadata {
            if let Err(e) = metadata_channel.say(
                self.cache_http.http(),
                format!("{} disconnected", crate_name!())
            ).await {
                warn!("failed to post disconnect message to Discord: {}", e);
            }
        }
    }
}

// TRAIT IMPLS *****************************************************************
impl From<GuildEnumerationState> for Option<GuildPagination> {
    fn from(state: GuildEnumerationState) -> Self {
        if let GuildEnumerationState::LastGuild(id) = state {
            Some(GuildPagination::After(id))
        } else {
            None
        }
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
pub async fn send_audio<'a>(
    stream: Box<dyn AudioStream>,
    cfg: &'a DiscordConfig,
    shutdown_rx: WatchReceiver<bool>
) -> Result<JoinHandle<()>, DiscordError> {
    Ok(tokio::spawn(ConnectedClient::new(cfg).await?.run(stream, shutdown_rx)))
}

/// Prints a list of Discord servers available to the given bot token, or
/// returns an error if one is encountered while generating the list of servers.
pub async fn list_guilds(token: impl AsRef<str>) -> Result<(), SerenityError> {
    match client::get(token, None, None).await {
        Ok(client) => {
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
        },
        Err(e) => Err(e),
    }
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
