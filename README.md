# dispatch-plugins

Dispatch channel extensions built and released together from one workspace.

## Layout

- `catalog/extensions.json` - machine-readable extension catalog index
- `channels/schema` - shared manifest and catalog schema for channel plugins
- `channels/discord` - Discord interaction-webhook and websocket plugin
- `channels/email` - IMAP + SMTP email plugin
- `channels/gmail` - Gmail-focused IMAP + SMTP email plugin
- `channels/outlook` - Outlook / Microsoft 365 IMAP + SMTP email plugin
- `channels/signal` - native Signal plugin based on presage
- `channels/slack` - Slack Events API / Socket Mode plugin
- `channels/telegram` - Telegram Bot API plugin
- `channels/twilio-sms` - Twilio SMS plugin
- `channels/webhook` - generic webhook plugin
- `channels/whatsapp` - native WhatsApp Web plugin based on whatsapp-rust
- `catalog` - local catalog search and inspection helper

## Channel availability

| Plugin | Ingress availability | Upstream receive transport | Outbound delivery |
| --- | --- | --- | --- |
| `channel-discord` | Interaction webhook or websocket session | Host-managed HTTPS callback or Discord Gateway websocket | Discord REST bot messages |
| `channel-email` | One-shot poll or background ingress session | IMAP polling worker inside the plugin | SMTP delivery |
| `channel-gmail` | One-shot poll or background ingress session | Gmail IMAP polling worker inside the plugin | Gmail SMTP delivery |
| `channel-outlook` | One-shot poll or background ingress session | Outlook / Microsoft 365 IMAP polling worker inside the plugin | Outlook / Microsoft 365 SMTP delivery |
| `channel-signal` | One-shot poll or background ingress session | Native Signal websocket through presage | Native Signal messages and attachments |
| `channel-slack` | Events API webhook or background Socket Mode session | Host-managed HTTPS callback or Slack Socket Mode websocket | `chat.postMessage` or incoming webhook |
| `channel-telegram` | Bot API webhook, one-shot poll, or background poll session | Telegram webhook callback or Bot API `getUpdates` | `sendMessage`, `sendPhoto`, `sendDocument` |
| `channel-twilio-sms` | Twilio webhook only | Host-managed HTTPS callback | Twilio Messages API |
| `channel-webhook` | Generic webhook only | Host-managed HTTPS callback | JSON `POST` to configured endpoint |
| `channel-whatsapp` | One-shot poll or background ingress session | Native WhatsApp Web websocket through whatsapp-rust | Native WhatsApp messages and attachments |

## Dispatch integration

Channel plugins use `dispatch-channel-protocol` directly for the wire contract. The framing is still one JSON message per line on stdio, but the message shape inside that framing is JSON-RPC 2.0.

In practice:

- `dispatch channel call` exercises one-shot operations such as `configure`, `health`, `deliver`, `push`, and `status`
- `dispatch channel listen` runs webhook-style ingress bindings
- `dispatch channel poll --once` issues a one-shot `poll_ingress` request
- `dispatch channel poll` without `--once` runs background-ingress bindings for polling or sessioned upstream transports
- `dispatch up` manages project-level channel bindings from `dispatch.toml`

While an ingress session is active, Dispatch keeps the plugin subprocess alive and receives inbound activity as `channel.event` notifications. There is no central gateway process; each plugin owns its upstream receive transport directly and may translate that into repeated HTTP polling, an upstream websocket, or plain webhook callbacks.

## Build layout

All in-repo plugins build from the repo-root workspace:

- `cargo check --workspace --locked`
- `cargo test --workspace --locked`

Signal and WhatsApp require `protoc` during compilation. Signal also requires a native C toolchain and Perl for its bundled SQLCipher and OpenSSL build.

## Notes

- `channel-schema` remains a repo-local crate for shared manifest and catalog types used by plugin tests and the catalog helper.
- Email-oriented plugins are still channels in Dispatch because they surface inbound user-originated messages and outbound replies through the same normalized conversation/event model as chat platforms.
- Email service variants stay service-specific at the plugin layer: `channel-email`, `channel-gmail`, and `channel-outlook`. Broader provider umbrellas such as Google or Microsoft can exist later as separate connector or tool surfaces without overloading the channel naming.
- `catalog/extensions.json` acts as the local catalog seed for install and search tools. It includes all channel plugins that this repository builds and releases.
- The protocol surface may still change as Dispatch hardens the channel runtime, so it should not yet be treated as a stable Dispatch core compatibility contract.

## License

The workspace uses the MIT License except for `channel-signal`. The `channel-signal` binary links to `presage` and uses the AGPL-3.0 license. See `channels/signal/LICENSE`.
