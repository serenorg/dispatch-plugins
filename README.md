# dispatch-plugins

Dispatch extension projects collected under a single parent directory.

## Layout

- `catalog/extensions.json` - local machine-readable extension catalog index
- `channels/schema` - shared manifest and catalog schema for channel plugins
- `channels/discord` - Discord channel plugin
- `channels/signal` - Signal channel plugin
- `channels/slack` - Slack channel plugin
- `channels/telegram` - Telegram channel plugin
- `channels/twilio-sms` - Twilio SMS channel plugin
- `channels/webhook` - generic webhook channel plugin
- `channels/whatsapp` - WhatsApp Cloud API channel plugin
- `catalog` - local catalog search and inspection helper

## Notes

- Channel plugins use `dispatch-channel-protocol` directly for the channel
  wire contract.
- `channel-schema` remains a repo-local crate for shared manifest and catalog
  types used by plugin tests and the catalog helper.
- `catalog/extensions.json` acts as a local catalog seed for install/search
  tooling and normalizes the plugin manifests into one
  searchable index.
- The channel plugins share a common channel protocol and supporting manifest
  models.
- The protocol surface may still change as Dispatch hardens the channel
  runtime, so it should not yet be treated as a stable Dispatch core
  compatibility contract.
