///! Lower-level Serenity client types and logic.

use std::sync::Arc;

use async_trait::async_trait;
use clap::{crate_name, crate_version};
use dashmap::DashMap;
use log::{debug, info, warn};
use serenity::CacheAndHttp;
use serenity::client::{Client, Context, EventHandler};
use serenity::http::CacheHttp;
use serenity::http::client::Http;
use serenity::model::channel::{ChannelType, GuildChannel};
use serenity::model::gateway::{GatewayIntents, Ready};
use serenity::model::id::{ChannelId, GuildId};
use serenity::utils::MessageBuilder;
use serenity::prelude::{SerenityError, TypeMapKey};
use songbird::Songbird;
use songbird::serenity::SerenityInit;
use tokio::sync::oneshot::{
    self,
    Receiver,
    Sender
};

use super::cfg::DiscordConfig;
use super::error::DiscordError;

// TYPE DEFINITIONS ************************************************************

/// Wrapper type containing information on which channels the application should
/// connect to.
#[derive(Debug)]
pub struct Channels<T> {
    /// The ID of the guild to which all channels mentioned in this struct
    /// belong.
    pub guild_id: GuildId,

    /// The voice channel.
    pub voice: T,

    /// A text channel suitable for automatic status updates from the
    /// application.
    pub metadata: Option<T>
}

/// Type alias for the result of the channel resolution process.
pub type ChannelResolutionResult = Result<Channels<ChannelId>, DiscordError>;

/// Marker struct for storing and retrieving a oneshot [`Sender`] in/from
/// serenity's TypeMap.
pub struct ChannelResolutionSender;

/// Marker struct for storing and retrieving a oneshot [`Receiver`] in/from
/// serenity's TypeMap.
pub struct ChannelResolutionReceiver;

/// Implementation of serenity's [`EventHandler`] for bardcast.
///
/// This struct is largely unused; most of bardcast's user-visible interactions
/// stem from events internal to the application itself, rather than in response
/// to events from Discord. However, the implementation is used for one thing:
/// resolving channel names to channel IDs, prior to the application connecting.
struct Handler;

// TYPE IMPLS ******************************************************************
impl Channels<ChannelId> {
    /// Posts a "hello"/on-connect message to the configured metadata channel,
    /// if any.
    pub async fn hello(&self, cache_http: &CacheAndHttp) {
        if let Some(metadata_channel) = self.metadata {
            match self.voice.to_channel(cache_http).await {
                Ok(voice_channel) => {
                    let msg = MessageBuilder::new()
                        .push(format!(
                            "{} version {} connected to voice channel ",
                            crate_name!(),
                            crate_version!()
                        ))
                        .mention(&voice_channel)
                        .build();

                    if let Err(e) = metadata_channel.say(
                        cache_http.http(),
                        msg
                    ).await {
                        warn!(
                            "Failed to post initialization message to Discord: {}",
                            e
                        );
                    }
                },
                Err(e) => warn!(
                    "Failed to post initialization message to Discord due to \
                     failure to look up voice channel metadata: {}",
                    e
                ),
            }
        }
    }


    /// Posts a "goodbye"/on-disconnect message to the configured metadata
    /// channel.
    pub async fn goodbye(&self, http: &Http) {
        if let Some(metadata_channel) = self.metadata {
            if let Err(e) = metadata_channel.say(
                http,
                format!("{} disconnected", crate_name!())
            ).await {
                warn!("Failed to post disconnect message to Discord: {}", e);
            }
        }
    }
}

// TRAIT IMPLS *****************************************************************
impl TryFrom<&DiscordConfig> for Channels<String> {
    type Error = DiscordError;

    fn try_from(cfg: &DiscordConfig) -> Result<Self, Self::Error> {
        if let Some(server_id) = cfg.server_id {
            if let Some(voice_channel) = &cfg.voice_channel {
                Ok(Self {
                    guild_id: GuildId(server_id),
                    voice: voice_channel.clone(),
                    metadata: cfg.metadata_channel.clone(),
                })
            } else {
                Err(DiscordError::MissingRequiredProperty(String::from("voice-channel")))
            }
        } else {
            Err(DiscordError::MissingRequiredProperty(String::from("server-id")))
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, _metadata: Ready) {
        info!("Connected to Discord");
    }

    async fn cache_ready(&self, ctx: Context, _guilds: Vec<GuildId>) {
        debug!("Discord client cache populated");

        let mut data = ctx.data.write().await;

        // Resolve the channel names to IDs, if needed
        if let Some(unresolved_channels) = data.remove::<Channels<String>>() {
            let tx = data.remove::<ChannelResolutionSender>()
                .expect("Discord channel resolution sender should be present");

            if let Some(all_channels) = ctx.cache.guild_channels(
                unresolved_channels.guild_id
            ) {
                let voice = find_matching_channel(
                    &all_channels,
                    &unresolved_channels.voice,
                    ChannelType::Voice
                );
                let metadata = unresolved_channels.metadata.as_ref()
                    .map(|name| find_matching_channel(
                        &all_channels,
                        &name,
                        ChannelType::Text
                    ))
                    .flatten();

                if let Some(voice) = voice {
                    tx.send(Ok(Channels {
                        guild_id: unresolved_channels.guild_id,
                        voice,
                        metadata,
                    })).expect("Resolved channels failed to send");
                } else {
                    tx.send(Err(DiscordError::DataLookupError(format!(
                        "No such voice channel with name '{}'",
                        unresolved_channels.voice
                    )))).expect("Channel resolution failure failed to send");
                }
            } else {
                tx.send(Err(DiscordError::DataLookupError(String::from(
                    "Available channels unexpectedly missing from cache"
                )))).expect("Channel resolution failure failed to send");
            }
        }
    }
}

impl<T: Send + Sync + 'static> TypeMapKey for Channels<T> {
    type Value = Channels<T>;
}

impl TypeMapKey for ChannelResolutionSender {
    type Value = Sender<ChannelResolutionResult>;
}

impl TypeMapKey for ChannelResolutionReceiver {
    type Value = Receiver<ChannelResolutionResult>;
}

// PUBLIC INTERFACE FUNCTIONS **************************************************

/// Creates a serenity [`Client`].
///
/// If `channels` is provided, the client will resolve the channel names to
/// [`ChannelId`] instances; the result can be found by fetching the
/// [`ChannelResolutionReceiver`] from the client's type map after creation.
///
/// If the client is to be used for interacting with a voice chat, an optional
/// [`Songbird`] instance can be passed in.
pub async fn get(
    token: impl AsRef<str>,
    channels: Option<Channels<String>>,
    songbird: Option<Arc<Songbird>>
) -> Result<Client, SerenityError> {
    let mut client_builder = Client::builder(
        token,
        GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES
    ).event_handler(Handler);

    if let Some(channels) = channels {
        let (tx, rx) = oneshot::channel::<ChannelResolutionResult>();
        client_builder = client_builder
            .type_map_insert::<Channels<String>>(channels)
            .type_map_insert::<ChannelResolutionSender>(tx)
            .type_map_insert::<ChannelResolutionReceiver>(rx);
    }

    if let Some(songbird) = songbird {
        client_builder = client_builder.register_songbird_with(songbird);
    }

    client_builder.await
}

// HELPER FUNCTIONS ************************************************************

/// Helper function for finding a channel with a matching name and type from
/// a connection of channels for a single guild.
fn find_matching_channel(
    channels: &DashMap<ChannelId, GuildChannel>,
    name: impl AsRef<str>,
    kind: ChannelType
) -> Option<ChannelId> {
    channels.iter().find_map(move |entry| {
        let channel = entry.value();

        if channel.name.as_str() == name.as_ref() && channel.kind == kind {
            Some(entry.key().clone())
        } else {
            None
        }
    })
}
