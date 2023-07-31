///! Discord client configuration.

use configparser::ini::Ini;

use crate::cfg::{self, Action, Args, Config, SelectedConsumer};

const SECTION_NAME: &'static str = "discord";

/// TYPE DEFINITIONS ***********************************************************

/// Configuration type for the Discord client.
#[derive(Debug)]
pub struct DiscordConfig {
    /// The bot token to authenticate with.
    pub token: Option<String>,

    /// The name of the voice channel to connect to.
    pub voice_channel: Option<String>,

    /// The name of the text channel to post messages to (if any).
    pub metadata_channel: Option<String>,

    /// The unique ID of the server to connect to. This ID can be found either
    /// by using Discord's developer mode, or by running bardcast with the
    /// --list-servers option.
    pub server_id: Option<u64>,
}

/// TRAIT IMPLS ****************************************************************

impl TryFrom<&Args> for DiscordConfig {
    type Error = String;

    fn try_from(args: &Args) -> Result<Self, Self::Error> {
        Ok(Self {
            token: args.token.clone(),
            voice_channel: args.voice_channel.clone(),
            metadata_channel: args.metadata_channel.clone(),
            server_id: args.server_id.clone(),
        })
    }
}

impl<'a> Config<'a> for DiscordConfig {
    fn from_ini(config: &Ini) -> Result<Self, String> {
        Ok(Self {
            token: config.get(SECTION_NAME, "token"),
            voice_channel: config.get(SECTION_NAME, "voice-channel"),
            metadata_channel: config.get(SECTION_NAME, "metadata-channel"),
            server_id: config.getuint(SECTION_NAME, "server-id")?,
        })
    }

    fn merge(&mut self, other: Self) {
        cfg::merge_opt(&mut self.token, other.token);
        cfg::merge_opt(&mut self.voice_channel, other.voice_channel);
        cfg::merge_opt(&mut self.metadata_channel, other.metadata_channel);
        cfg::merge_opt(&mut self.server_id, other.server_id);
    }

    fn validate_semantics(&self, action: Action) -> Result<(), String> {
        match action {
            Action::Run(SelectedConsumer::Discord) => {
                if self.token.is_none() {
                    Err(String::from("A Discord bot token is required."))
                } else if self.server_id.is_none() {
                    Err(String::from("A Discord server ID is required."))
                } else if self.voice_channel.is_none() {
                    Err(String::from("A voice channel name is required."))
                } else {
                    Ok(())
                }
            },
            Action::ListServers => {
                if self.token.is_none() {
                    Err(String::from("A Discord bot token is required."))
                } else {
                    Ok(())
                }
            },
            _ => Ok(())
        }
    }
}
