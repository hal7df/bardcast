///! Error types used with the Discord client.

use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};

use serenity::prelude::SerenityError;
use songbird::error::JoinError as CallJoinError;
use tokio::task::JoinError;

//TYPE DEFINITIONS *************************************************************

/// Primary error type used with the Discord client. Wraps a number of
/// underlying error types that can occur during normal operation of the client,
/// in addition to a few additional exceptional cases that occur outside of
/// library code.
#[derive(Debug)]
pub enum DiscordError {
    /// Underlying error from the serenity Discord client library.
    Serenity(SerenityError),

    /// Underlying error from the songbird Discord audio client library.
    Songbird(CallJoinError),

    /// A client task crashed.
    TaskCrash(JoinError),

    /// An error occurred while initializing the Discord client.
    InitializationError,

    /// The client failed to look up information from Discord that is otherwise
    /// required for normal function of the application, with detail provided as
    /// a string.
    DataLookupError(String),

    /// The user failed to provide a required configuration property to the
    /// client, with the name of the property provided as a string.
    MissingRequiredProperty(String),
}

//TRAIT IMPLS ******************************************************************
impl Display for DiscordError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            DiscordError::Serenity(e) => write!(f, "{}", e),
            DiscordError::Songbird(e) => write!(f, "{}", e),
            DiscordError::TaskCrash(e) => write!(
                f,
                "Discord client task unexpectedly quit: {}",
                e
            ),
            DiscordError::InitializationError => write!(
                f,
                "Initialization error"
            ),
            DiscordError::DataLookupError(msg) => write!(
                f,
                "Could not find entity in Discord: {}",
                msg
            ),
            DiscordError::MissingRequiredProperty(prop_name) => write!(
                f,
                "Could not start Discord client: missing required property '{}'",
                prop_name
            ),
        }
    }
}

impl Error for DiscordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            DiscordError::Serenity(e) => Some(e),
            DiscordError::Songbird(e) => Some(e),
            DiscordError::TaskCrash(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SerenityError> for DiscordError {
    fn from(e: SerenityError) -> Self {
        DiscordError::Serenity(e)
    }
}

impl From<CallJoinError> for DiscordError {
    fn from(e: CallJoinError) -> Self {
        DiscordError::Songbird(e)
    }
}

impl From<JoinError> for DiscordError {
    fn from(e: JoinError) -> Self {
        DiscordError::TaskCrash(e)
    }
}

