![bardcast](icon/bardcast-icon-text-192px.png)

bardcast is a commandline application that can read audio playing on your system
and stream it to a Discord voice chat.

bardcast currently only supports Unix-like systems running PulseAudio (and
possibly PipeWire via its PulseAudio compatibility layer), although support for
other OSes and sound servers may be added in the future.

# Building
bardcast needs the following libraries available on your system:

  * `libopus` (via [Songbird](https://github.com/serenity-rs/songbird#dependencies))
  * Linux only
    * `libpulse` (if using the default `pulse` feature for PulseAudio support)
      * Debian/Ubuntu: `libpulse-dev`
      * Fedora: `pulseaudio-libs-devel`
      * openSUSE: `libpulse-devel`
      * Arch: `libpulse`

Once these dependencies are installed, simply run `cargo build -r` to create a
release build of bardcast.

Each audio driver has its own feature flag; the most common servers are enabled
by default, but can be disabled by passing `--no-default-features` to `cargo`
and manually specifying the drivers you want.

# Setup
Unlike most Discord bots, there is no quick link to add bardcast to your
server -- because bardcast runs on your machine (rather than on a server), you
must generate your own bot user and token to use it.

To create your own bot user, visit the
[Discord developer portal](https://discord.com/developers/appliations) and
create a new application. Disable the "Public Bot" option in the Bot settings
(since you will have sole control over the bot), generate a token for the bot
user (using the "Reset Token" button on the Bot page), and store it in a safe
place. This token can be used with bardcast's `-t` command line option, or the
`token` option in the `discord` section of a configuration file.

When generating an OAuth2 invite URL (Application settings > OAuth2 > URL
Generator), ensure the following options are checked:

  * Scopes
    * `bot`
  * Bot permissions
    * General permissions
      * Read messages/view channels
    * Text permissions
      * Send messages
    * Voice permissions
      * Connect
      * Speak
      * Use Voice Activity

Visit the generated URL in a browser that is logged into your Discord account to
invite the bot to any server where you have rights to invite bots.

# Usage
bardcast needs, at a minimum, a bot token, a server ID, and a voice channel
name in order to connect and start playing audio. The server ID can be found by
enabling developer mode in Discord and using the "copy server ID" option in the
context menu for the server, or by using the `--list-servers` option:

```
bardcast -t $BOT_TOKEN_HERE --list-servers
```
_Note: Be careful not to leak your bot token into your shell history when using
`-t`/`--token` -- use an environment variable or a commandline password manager
like [`pass`](https://www.passwordstore.org)._

Once you have the server, specify the ID and name of the voice chat to connect
to:

```
bardcast -t $BOT_TOKEN_HERE -s $SERVER_ID --voice-channel $VOICE_CHANNEL
```

bardcast also supports different _intercept modes_, which determine how system
audio is captured and can be specified with `-m`. The default depends on the
driver; some modes that focus on capturing individual applications may require
that you specify a regular expression to match applications against with `-E`:

```
bardcast -t $BOT_TOKEN_HERE -s $SERVER_ID --voice-channel $VOICE_CHANNEL -m peek -E vlc
```

Most of bardcast's command line arguments can also be specified through an
INI-format configuration file with `-c`:

```
bardcast -c $CONFIG_FILE
```

Fore more commandline options, see `-h`/`--help`.

## Configuration file options
All configuration options will be overridden by their corresponding command line
argument, if both are present.

### Section: bardcast
General top-level application configuration.

| Option | Type | Description |
| ------ | ---- | ----------- |
| `log-level` | `off`, `error`, `warn`, `info`, `debug`, `trace` | Specifies the level of verbosity produced by the application. Defaults to `info`. |
| `driver` | `pulse` | The name of the sound driver to use. Some drivers may be unavailable, depending on your OS. |
| `threads` | usize | The number of worker threads to spin up for the application. This does not include the application main thread or threads spun up internally by libraries. Defaults to 2. |

### Section: discord
Configuration options for the discord client.

| Option | Type | Description |
| ------ | ---- | ----------- |
| `token` | String | The Discord API token to use. |
| `server-id` | u64 | The numeric ID of the Discord server to connect to. |
| `voice-channel` | String | The name of the voice channel to connect to. |
| `metadata-channel` | String | The name of the text channel to post event messages to. |

### Section: pulse
Configuration options for the PulseAudio sound driver. This driver is only
available on Unix systems when built with the `pulse` feature flag.

| Option | Type | Description |
| ------ | ---- | ----------- |
| `intercept-mode` | `capture`, `peek`, `monitor` | The audio intercept mode that the driver should use. Defaults to `peek`. |
| `stream-regex` | Regular expression | A regular expression that matches against applications that you want to capture. |
| `stream-properties` | Comma-separated list | A list of PulseAudio property names to match `stream-regex` against. |
| `server` | String | The path to the PulseAudio server. |
| `sink-name` | String | The name of the audio sink to use with `peek` or `monitor` intercept modes. |
| `sink-index` | u32 | The index of the audio sink to use with `peek` or `monitor` intercept modes. Overrides `sink-name` if both are set (unless `sink-name` is provided as a command line argument). |
| `volume` | 0.0 to 1.0 | Normalized volume level for the recording stream. |
