# dispatch-plugins

Dispatch channel extensions collected under a single parent directory.

## Layout

- `catalog/extensions.json` - machine-readable extension catalog index
- `channels/schema` - shared manifest and catalog schema for channel plugins
- `channels/discord` - Discord interaction-webhook plugin
- `channels/email` - IMAP + SMTP email plugin
- `channels/gmail` - Gmail-focused IMAP + SMTP email plugin
- `channels/outlook` - Outlook / Microsoft 365 IMAP + SMTP email plugin
- `channels/signal` - Signal plugin backed by `signal-cli-rest-api`
- `channels/slack` - Slack Events API / Socket Mode plugin
- `channels/telegram` - Telegram Bot API plugin
- `channels/twilio-sms` - Twilio SMS plugin
- `channels/webhook` - generic webhook plugin
- `channels/whatsapp` - WhatsApp Cloud API plugin
- `catalog` - local catalog search and inspection helper

## Channel availability

| Plugin | Ingress availability | Upstream receive transport | Outbound delivery |
| --- | --- | --- | --- |
| `channel-discord` | Interaction webhook only | Host-managed HTTPS callback | Discord REST bot messages |
| `channel-email` | Background ingress session only | IMAP polling worker inside the plugin | SMTP delivery |
| `channel-gmail` | Background ingress session only | Gmail IMAP polling worker inside the plugin | Gmail SMTP delivery |
| `channel-outlook` | Background ingress session only | Outlook / Microsoft 365 IMAP polling worker inside the plugin | Outlook / Microsoft 365 SMTP delivery |
| `channel-signal` | Background ingress session only | `signal-cli-rest-api` HTTP receive in `native` / `normal`, websocket receive in `json-rpc` | Signal REST API |
| `channel-slack` | Events API webhook or Socket Mode | Host-managed HTTPS callback or Slack Socket Mode websocket | `chat.postMessage` or incoming webhook |
| `channel-telegram` | Bot API webhook or polling | Telegram webhook callback or Bot API `getUpdates` | `sendMessage`, `sendPhoto`, `sendDocument` |
| `channel-twilio-sms` | Twilio webhook only | Host-managed HTTPS callback | Twilio Messages API |
| `channel-webhook` | Generic webhook only | Host-managed HTTPS callback | JSON `POST` to configured endpoint |
| `channel-whatsapp` | Meta webhook only | Host-managed HTTPS callback | WhatsApp Cloud API |

## Dispatch integration

Channel plugins use `dispatch-channel-protocol` directly for the wire contract.
The framing is still one JSON message per line on stdio, but the message shape
inside that framing is JSON-RPC 2.0.

In practice:

- `dispatch channel call` exercises one-shot operations such as `configure`,
  `health`, `deliver`, `push`, and `status`
- `dispatch channel listen` runs webhook-style ingress bindings
- `dispatch channel poll` runs background-ingress bindings for polling or
  sessioned upstream transports
- `dispatch up` manages project-level channel bindings from `dispatch.toml`

While an ingress session is active, Dispatch keeps the plugin subprocess alive
and receives inbound activity as `channel.event` notifications. The upstream
transport is plugin-specific: a plugin may translate that into repeated HTTP
polling, an upstream websocket, or plain webhook callbacks.

## Notes

- `channel-schema` remains a repo-local crate for shared manifest and catalog
  types used by plugin tests and the catalog helper.
- Email-oriented plugins are still channels in Dispatch because they surface
  inbound user-originated messages and outbound replies through the same
  normalized conversation/event model as chat platforms.
- Email service variants stay service-specific at the plugin layer:
  `channel-email`, `channel-gmail`, and `channel-outlook`. Broader provider
  umbrellas such as Google or Microsoft can exist later as separate connector
  or tool surfaces without overloading the channel naming.
- `catalog/extensions.json` acts as the local catalog seed for install/search
  tooling and normalizes the plugin manifests into one searchable index.
- The protocol surface may still change as Dispatch hardens the channel
  runtime, so it should not yet be treated as a stable Dispatch core
  compatibility contract.
