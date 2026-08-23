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

- inbound events are emitted only for guilds, channels, threads, and DMs the binding explicitly allows, and only when the event addresses the bot
- outbound delivery sends bot messages to a Discord channel or thread inside the configured outbound scope
- outbound attachments support one inline `data_base64` file upload per message
- health checks validate the bot token against `GET /users/@me` and report the redacted effective policy
- ingress verifies Discord interaction signatures and normalizes interaction payloads
- `start_ingress` reports the expected public interaction endpoint and verification-key requirement when `webhook_public_url` is configured
- `start_ingress` uses a long-running Discord websocket when no webhook URL is configured, and resolves the bot account identity before opening the gateway
- websocket ingress resumes Discord sessions after transient reconnects when Discord marks the session resumable, and carries the proven bot identity across the resume
- status frames render visible status messages into a Discord channel or thread inside the configured outbound scope

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

### Channel policy

Discord permissions decide what the bot account can observe and post. They do not decide which messages may wake this binding. That is stated here, and it is fail-closed: every allowlist below is empty by default, and an empty allowlist denies.

| Field | Meaning | Absent means |
| --- | --- | --- |
| `allowed_guild_ids` | Guilds the binding may act within | deny every guild |
| `allowed_channel_ids` | Channels the binding may receive from. A literal `"*"` entry is the only wildcard, and it widens channel scope only inside `allowed_guild_ids` | deny every channel |
| `thread_policy` | `deny`, `inherit_parent`, or `allowlist` | `deny` |
| `allowed_thread_ids` | Threads allowed under `thread_policy = "allowlist"` | deny every thread |
| `activation` | `mention_or_reply`, `slash_command`, or `all_messages` | `mention_or_reply` |
| `dm_policy` | `deny`, `allowlist`, or `open` | `deny` |
| `allowed_dm_sender_ids` | Senders allowed under `dm_policy = "allowlist"` | deny every DM sender |
| `allowed_sender_ids` | Senders allowed in allowed guild channels | any sender already inside an allowed channel |
| `outbound_channel_ids` | Destinations the binding may publish to | falls back to `allowed_channel_ids`, never wider |
| `reply_delivery` | `runtime_owned` or `tool_owned` | `runtime_owned` |
| `owner_id` | Single sender the binding answers to, on every surface | no owner restriction |

Fail-closed rules:

- Persistent websocket guild ingress requires a non-empty `allowed_guild_ids` and a non-empty `allowed_channel_ids`. `configure` and `start_ingress` fail rather than treat an empty allowlist as "all".
- An unrecognized `activation`, `thread_policy`, `dm_policy`, or `reply_delivery` value fails validation. It never falls through to a permissive default.
- `dm_policy = "allowlist"` requires at least one `allowed_dm_sender_ids` entry, and `dm_policy = "deny"` or `dm_policy = "open"` requires `allowed_dm_sender_ids` to be empty.
- `allowed_dm_sender_ids` rejects a literal `"*"` entry. A sender allowlist enumerates senders and has no wildcard form; `dm_policy = "open"` is the written-down mode for accepting any sender.
- `dm_policy = "open"` accepts a direct message from any sender. Like `activation = "all_messages"`, it is unbounded ingress, so it has to be written down: it is never inferred from an absent `dm_policy`. When `owner_id` is set it still applies, so `open` plus `owner_id` means owner-only direct messages.
- `activation = "all_messages"` requires explicitly listed channels and rejects the `"*"` wildcard, because it is meant for a dedicated channel.
- A thread is a separate conversation from the channel that holds it. Under `inherit_parent` the thread's parent is resolved from Discord and must itself be allowed; if parentage cannot be proven, the event is dropped.
- `default_channel_id` is an outbound fallback only. It grants no ingress, and it is checked against the outbound allowlist like any other destination.

### Activation

In an allowed guild channel, `mention_or_reply` accepts:

- a message whose Discord `mentions` list contains this bot's account ID; or
- a reply whose referenced message Discord resolved to a bot-authored message.

It does not accept literal text such as `@Piper`, a mention of another member, a role or `@everyone` mention, or a bare `message_reference` whose target author Discord did not resolve. A zero-content message is dropped unless a verified mention or reply, or an attachment on an otherwise authorized event, makes it actionable.

Slash commands, components, and modals are routed to the application by Discord, so they activate under any `activation` value. They are still confined to the allowed channels and threads, and still answer to the sender and DM rules.

Direct messages activate on their own, and are governed by `dm_policy` and `allowed_dm_sender_ids` independently of any guild setting. A direct message is addressed to the bot account by construction, so the sender is the only question `dm_policy` answers: `deny` refuses every direct message, `allowlist` accepts only the listed senders, and `open` accepts any sender.

### Provenance

Every emitted event carries the evidence a host revalidates on its own:

- `conversation.id` (channel, or thread ID inside a thread), `conversation.kind` (`channel`, `thread`, or `dm`)
- `conversation.workspace_id` (guild; absent for a DM) and `conversation.parent_conversation_id`
- `activation.reason` (`direct_mention`, `reply_to_agent`, `slash_command`, `direct_message`, or `all_messages`), `activation.agent_account_id`, and `activation.referenced_message_author_id`

### Examples

Single shared channel, mention or reply only, DMs denied:

```toml
allowed_guild_ids = ["234567890123456789"]
allowed_channel_ids = ["345678901234567890"]
activation = "mention_or_reply"
thread_policy = "deny"
dm_policy = "deny"
reply_delivery = "runtime_owned"
default_channel_id = "345678901234567890"
message_content_intent = false
```

Shared channel plus its child threads:

```toml
allowed_guild_ids = ["234567890123456789"]
allowed_channel_ids = ["345678901234567890"]
thread_policy = "inherit_parent"
activation = "mention_or_reply"
```

Dedicated channel where every message is a request:

```toml
allowed_guild_ids = ["234567890123456789"]
allowed_channel_ids = ["456789012345678901"]
activation = "all_messages"
dm_policy = "deny"
```

Allowlisted direct messages, no guild ingress, interaction webhook transport:

```toml
webhook_public_url = "https://example.com"
webhook_path = "/discord/interactions"
dm_policy = "allowlist"
allowed_dm_sender_ids = ["567890123456789012"]
```

Public support bot that answers a direct message from anyone, with no guild ingress:

```toml
webhook_public_url = "https://example.com"
webhook_path = "/discord/interactions"
dm_policy = "open"
```

Set `message_content_intent = true` only after enabling the matching privileged intent for the bot application. Keeping it false lets the websocket connect without privileged intent approval, but guild messages may have empty `content` unless they mention the bot or otherwise qualify under Discord's message-content rules. The intent changes the text Discord sends, not the authorization decision: an equivalent message is authorized the same way with the intent on or off.

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
  --request-json '{"kind":"push","config":{"default_channel_id":"123456789012345678","outbound_channel_ids":["123456789012345678"]},"message":{"content":"Dispatch Discord test"}}'
```

A `push`, `deliver`, or `status` request to a destination outside the outbound allowlist is rejected before any Discord request, with the reason code `unauthorized_destination`.

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Dispatch operators normally use the host CLI rather than writing raw envelopes.

## Notes on ingress

When `webhook_public_url` is configured, this plugin treats `start_ingress` as a configuration handshake for an interaction-webhook deployment:

1. validate the bot token
2. validate the configured interaction verification key
3. return the expected public route that the host should expose

When no webhook URL is configured, `start_ingress` resolves the bot account through `GET /users/@me`, then starts a long-running Discord Gateway websocket session owned by the plugin. The gateway `READY` frame is cross-checked against that account, and against `application_id` when one is configured; a mismatch ends the session rather than running under an unverified identity.

The websocket session emits `channel.event` notifications only for message events that satisfy the binding policy, and preserves Discord resume metadata and the proven bot identity across reconnects when Discord marks the session resumable. For local smoke tests, run `dispatch channel poll` or a `dispatch up` project with a channel binding in `mode = "poll"`; the plugin reports websocket ingress through the same receive loop.

Dropped events are counted by reason code without recording message content: `bot_event`, `unsupported_message_type`, `unauthorized_workspace`, `unauthorized_channel`, `unauthorized_thread`, `unresolved_conversation`, `dm_denied`, `sender_denied`, `not_addressed`, `empty_message`, `application_mismatch`, and `invalid_policy`. Set `DISCORD_GATEWAY_DEBUG=1` to log them.

## License

MIT
