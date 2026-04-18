# channel-telegram

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Telegram.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `start_ingress`
- `stop_ingress`
- `ingress_event`
- `poll_ingress`
- `deliver`
- `push`
- `status`

Behavior:

- outbound delivery uses Telegram Bot API `sendMessage`
- outbound attachments use `sendPhoto` for image URLs/file ids/uploads and
  `sendDocument` for other URLs/file ids/uploads
- health checks validate credentials with `getMe`
- webhook ingress uses Telegram `setWebhook` / `deleteWebhook`
- polling ingress uses Telegram `getUpdates`
- inbound webhook and polling updates are normalized into Dispatch events
- status frames map to Telegram chat actions

Not implemented:

- multi-attachment delivery
- inline keyboards and other rich Telegram reply surfaces

## Build

```bash
cargo build --release
```

## Configuration

Required:

- `TELEGRAM_BOT_TOKEN` - bot token for health, ingress, and delivery

Optional:

- `TELEGRAM_WEBHOOK_SECRET` - secret token sent by Telegram to the configured
  webhook endpoint
- `webhook_public_url` - public base URL used when `ingress_mode` is `webhook`
- `webhook_path` - webhook path override, defaults to `/telegram/updates`
- `ingress_mode` - `webhook` or `polling`; defaults to `webhook` when
  `webhook_public_url` is set, otherwise `polling`
- `poll_timeout_secs` - long-poll timeout for `getUpdates`, clamped to 1-25s
- `default_chat_id` - fallback destination for outbound delivery
- `default_message_thread_id` - fallback Telegram topic/thread id
- `drop_pending_updates` - request Telegram to drop queued updates when
  switching ingress mode

## Setup

To obtain a Telegram bot token:

1. Open Telegram and start a chat with `@BotFather`.
2. Run `/newbot` and follow the prompts to create the bot.
3. Copy the bot token that BotFather returns.
4. Export it as `TELEGRAM_BOT_TOKEN`.
5. If you want Telegram to send the webhook secret-token header, generate a
    secret value and export it as `TELEGRAM_WEBHOOK_SECRET`.

## Manifest

The Dispatch channel manifest is stored in `channel-plugin.json`. The host can
install it with `dispatch channel install`.

## Protocol

Requests are sent as JSONL on stdin:

```json
{"protocol_version":1,"request":{"kind":"capabilities"}}
```

Ingress registration example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "start_ingress",
    "config": {
      "webhook_public_url": "https://example.com",
      "webhook_path": "/telegram/updates"
    }
  }
}
```

Delivery example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "deliver",
    "config": {
      "default_chat_id": "-1001234567890"
    },
    "message": {
      "content": "Dispatch says hello from Telegram."
    }
  }
}
```

Push example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "push",
    "config": {
      "default_chat_id": "-1001234567890"
    },
    "message": {
      "content": "Dispatch proactive update for Telegram."
    }
  }
}
```

## Notes on ingress

Telegram supports both webhook and polling ingress:

1. `start_ingress` calls `setWebhook` when configured for webhooks
2. `start_ingress` calls `deleteWebhook` and returns polling state when
  configured for polling
3. `ingress_event` parses webhook payloads forwarded by the host
4. `poll_ingress` fetches updates from `getUpdates` and advances the cursor
5. `deliver` and `push` use `sendMessage`

That makes it a strong reference channel for the Dispatch runtime.

## License

MIT
