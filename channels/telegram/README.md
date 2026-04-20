# channel-telegram

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Telegram.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `poll_ingress`
- `start_ingress`
- `stop_ingress`
- `ingress_event`
- `deliver`
- `push`
- `status`
- `shutdown`

Behavior:

- outbound delivery uses Telegram Bot API `sendMessage`
- outbound attachments use `sendPhoto` for image URLs, file ids, and uploads,
  and `sendDocument` for other URLs, file ids, and uploads
- health checks validate credentials with `getMe`
- webhook ingress uses Telegram `setWebhook` / `deleteWebhook`
- polling ingress supports both one-shot `poll_ingress` fetches and background
  `start_ingress` sessions via Bot API `getUpdates`
- inbound webhook and polling updates are normalized into Dispatch events
- status frames map to Telegram chat actions

Not implemented:

- multi-attachment delivery
- inline keyboards and other rich Telegram reply surfaces

## Build

```bash
cargo build --release
```

## Availability

Inbound:

- webhook mode is available when `webhook_public_url` is configured and
  Telegram can reach the resulting HTTPS endpoint
- polling mode is available anywhere the bot can call the Bot API; no public
  ingress URL is required

Outbound:

- replies and proactive pushes can target an explicit `chat_id`
- `default_chat_id` and `default_message_thread_id` provide routing defaults
  for operators who want a fixed destination

## Configuration

Required:

- `TELEGRAM_BOT_TOKEN` - bot token for health, ingress registration, and
  outbound delivery

Optional environment:

- `TELEGRAM_WEBHOOK_SECRET` - secret token sent by Telegram to the configured
  webhook endpoint

Useful config fields:

- `bot_token_env` - override the bot-token env var name, defaults to
  `TELEGRAM_BOT_TOKEN`
- `webhook_secret_env` - override the webhook-secret env var name, defaults to
  `TELEGRAM_WEBHOOK_SECRET`
- `webhook_public_url` - public base URL used when `ingress_mode` is `webhook`
- `webhook_path` - webhook path override, defaults to `/telegram/updates`
- `ingress_mode` - `webhook` or `polling`; defaults to `webhook` when
  `webhook_public_url` is set, otherwise `polling`
- `poll_timeout_secs` - long-poll timeout for `getUpdates`, clamped to 1-25s
- `default_chat_id` - fallback destination for outbound delivery
- `default_message_thread_id` - fallback Telegram topic/thread id
- `drop_pending_updates` - request Telegram to drop queued updates when
  switching ingress mode
- `allowed_chat_ids`, `allowed_sender_ids`, `owner_id`, `dm_policy`, and
  `allow_group_messages` - optional policy controls surfaced through
  `configure`

## Setup

Telegram bot setup:

1. Open Telegram and start a chat with `@BotFather`.
2. Run `/newbot` and follow the prompts to create the bot.
3. Copy the returned token and export it as `TELEGRAM_BOT_TOKEN`.
4. Decide whether the bot should receive messages through a public webhook or
    through Bot API polling.
5. If you want Telegram to send the webhook secret-token header, generate a
    secret value and export it as `TELEGRAM_WEBHOOK_SECRET`.

Webhook mode:

1. Choose a public HTTPS base URL that Telegram can reach.
2. Set `ingress_mode = "webhook"` and `webhook_public_url` in the plugin
    config.
3. Optionally set `webhook_path` if you do not want the default
    `/telegram/updates`.
4. If you are testing locally, use a tunnel such as ngrok or Cloudflare Tunnel;
    Telegram will not call `http://127.0.0.1`.
5. Start the ingress session through Dispatch. The plugin will call
    `setWebhook` and return the registered endpoint.

Polling mode:

1. Set `ingress_mode = "polling"` in the plugin config.
2. Set `poll_timeout_secs` if you want a non-default long-poll timeout.
3. Start the ingress session through Dispatch. The plugin will call
    `deleteWebhook`, then keep a background receive loop running while the host
    poller is active.

Getting destination ids:

- private chats use the numeric chat id from an inbound event
- groups and channels usually use negative ids such as `-1001234567890`
- forum topics use `default_message_thread_id` for the topic id

Minimal polling config:

```toml
ingress_mode = "polling"
default_chat_id = "-1001234567890"
poll_timeout_secs = 25
```

Minimal webhook config:

```toml
ingress_mode = "webhook"
webhook_public_url = "https://example.com"
webhook_path = "/telegram/updates"
default_chat_id = "-1001234567890"
```

Environment example:

```bash
export TELEGRAM_BOT_TOKEN="123456:ABC..."
export TELEGRAM_WEBHOOK_SECRET="replace-me"
```

## Dispatch usage

Dispatch operators normally use the host CLI rather than writing JSON-RPC
envelopes by hand:

```bash
dispatch channel call channel-telegram \
  --request-json '{"kind":"health","config":{"bot_token_env":"TELEGRAM_BOT_TOKEN"}}'

dispatch channel listen channel-telegram \
  --listen 127.0.0.1:8787 \
  --config-file ./telegram-webhook.toml

dispatch channel poll channel-telegram \
  --config-file ./telegram-polling.toml --once

dispatch channel call channel-telegram \
  --request-json '{"kind":"push","config":{"default_chat_id":"-1001234567890"},"message":{"content":"Dispatch Telegram test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. For the full wire
format, see Dispatch's `docs/extensions.md`.

## Notes on ingress

- `start_ingress` registers the Bot API webhook when configured for webhook
  mode
- `poll_ingress` performs one `getUpdates` cycle when configured for polling
  mode
- `start_ingress` deletes the webhook and starts a background receive loop when
  configured for polling mode
- webhook payloads are parsed through `ingress_event`
- polling events are delivered back to Dispatch as `channel.event`
  notifications

Common failure modes:

- `getMe` failures usually mean the bot token is invalid
- `setWebhook` failures usually mean the public URL is not reachable by
  Telegram or is not HTTPS
- missing inbound events in polling mode usually mean the bot still had a
  webhook registered before polling was started

## Manifest

The Dispatch channel manifest is stored in `channel-plugin.json`. The host can
install it with `dispatch channel install`.

## License

MIT
