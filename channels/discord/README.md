# channel-discord

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Discord.

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
- ingress verifies Discord interaction signatures and normalizes interaction
  payloads
- `start_ingress` reports the expected public interaction endpoint and
  verification-key requirement
- status frames render visible status messages into a Discord channel or thread

Not implemented:

- command registration and slash-command lifecycle management
- Discord gateway connections and intent management
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

- `DISCORD_INTERACTION_PUBLIC_KEY` - optional at install time, required when
  the configured ingress mode relies on Discord interaction webhooks

## Setup

To obtain the required Discord credentials:

1. Create an application in the Discord Developer Portal.
2. Add a bot user to the application.
3. Copy the bot token and export it as `DISCORD_BOT_TOKEN`.
4. If you are using interaction webhooks, copy the application's public key
    and export it as `DISCORD_INTERACTION_PUBLIC_KEY`.
5. Install the bot into the target server with the permissions needed to post
    messages in the destination channel or thread.

Minimal config:

```toml
default_channel_id = "123456789012345678"
webhook_public_url = "https://example.com"
webhook_path = "/discord/interactions"
```

## Manifest

The Dispatch channel manifest is stored in `channel-plugin.json`. The host can
install it with `dispatch channel install`.

## Dispatch usage

```bash
dispatch channel call channel-discord \
  --request-json '{"kind":"health","config":{"default_channel_id":"123456789012345678"}}'

dispatch channel listen channel-discord \
  --listen 127.0.0.1:8787 \
  --config-file ./discord.toml

dispatch channel call channel-discord \
  --request-json '{"kind":"push","config":{"default_channel_id":"123456789012345678"},"message":{"content":"Dispatch Discord test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Dispatch operators
normally use the host CLI rather than writing raw envelopes.

## Notes on ingress

Discord does not provide a simple webhook registration flow equivalent to
Telegram. This plugin therefore treats `start_ingress` as a configuration
handshake for an interaction-webhook deployment:

1. validate the bot token
2. validate the configured interaction verification key
3. return the expected public route that the host should expose

That keeps the plugin honest about what it can do.

## License

MIT
