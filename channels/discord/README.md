# channel-discord

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for Discord.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `start_ingress`
- `stop_ingress`
- `ingress_event`
- `deliver`
- `push`
- `status`

Behavior:

- outbound delivery sends bot messages to a Discord channel or thread
- outbound attachments support one inline `data_base64` file upload per message
- health checks validate the bot token against `GET /users/@me`
- ingress verifies Discord interaction signatures and normalizes interaction payloads
- `start_ingress` reports the expected public interaction endpoint and verification-key requirement when `webhook_public_url` is configured
- `start_ingress` uses a long-running Discord websocket when no webhook URL is configured
- websocket ingress resumes Discord sessions after transient reconnects when Discord marks the session resumable
- status frames render visible status messages into a Discord channel or thread

Not implemented:

- command registration and slash-command lifecycle management
- URL- or storage-key-backed attachment delivery
- message edits

## Build

```bash
cargo build --release
```

## Configuration

Required:

- `DISCORD_BOT_TOKEN` - bot token used for health checks and outbound delivery

Ingress verification when using `start_ingress`:

- `DISCORD_INTERACTION_PUBLIC_KEY` - optional at install time, required when the configured ingress mode relies on Discord interaction webhooks

## Setup

To obtain the required Discord credentials:

1. Create an application in the Discord Developer Portal.
2. Add a bot user to the application.
3. Copy the bot token and export it as `DISCORD_BOT_TOKEN`.
4. If you are using interaction webhooks, copy the application's public key and export it as `DISCORD_INTERACTION_PUBLIC_KEY`.
5. If you are using websocket ingress and need to read arbitrary guild message text, enable the bot's Message Content privileged intent in the Discord Developer Portal and set `message_content_intent = true` in the channel config. Without that opt-in, Discord may deliver guild message events with empty content unless the message is a DM, mention, or reply.
6. Install the bot into the target server with the permissions needed to post messages in the destination channel or thread.

Minimal interaction-webhook config:

```toml
default_channel_id = "123456789012345678"
webhook_public_url = "https://example.com"
webhook_path = "/discord/interactions"
```

Minimal websocket config:

```toml
default_channel_id = "123456789012345678"
allowed_guild_ids = ["234567890123456789"]
message_content_intent = false
```

Set `message_content_intent = true` only after enabling the matching privileged intent for the bot application. Keeping it false lets the websocket connect without privileged intent approval, but guild messages may have empty `content` unless they mention the bot or otherwise qualify under Discord's message-content rules.

## Manifest

The Dispatch channel manifest is stored in `channel-plugin.json`. The host can install it with `dispatch channel install`.

## Dispatch usage

```bash
dispatch channel call channel-discord \
  --request-json '{"kind":"health","config":{"default_channel_id":"123456789012345678"}}'

dispatch channel listen channel-discord \
  --listen 127.0.0.1:8787 \
  --config-file ./discord.toml

dispatch channel poll channel-discord \
  --config-file ./discord-websocket.toml

dispatch channel call channel-discord \
  --request-json '{"kind":"push","config":{"default_channel_id":"123456789012345678"},"message":{"content":"Dispatch Discord test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Dispatch operators normally use the host CLI rather than writing raw envelopes.

## Notes on ingress

When `webhook_public_url` is configured, this plugin treats `start_ingress` as a configuration handshake for an interaction-webhook deployment:

1. validate the bot token
2. validate the configured interaction verification key
3. return the expected public route that the host should expose

When no webhook URL is configured, `start_ingress` starts a long-running Discord Gateway websocket session owned by the plugin. The websocket session emits `channel.event` notifications for matching message events and preserves Discord resume metadata across reconnects when Discord marks the session resumable. For local smoke tests, run `dispatch channel poll` or a `dispatch up` project with a channel binding in `mode = "poll"`; the plugin reports websocket ingress through the same receive loop.

## License

MIT
