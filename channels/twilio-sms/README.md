# channel-twilio-sms

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for Twilio SMS.

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

Behavior:

- outbound delivery uses the Twilio Messages API
- outbound media uses Twilio MMS `MediaUrl` fields when attachment URLs are supplied
- health checks validate account credentials against the Twilio Accounts API
- ingress verifies Twilio webhook signatures and parses inbound SMS webhooks
- inbound status callbacks are acknowledged but not turned into Dispatch events

Not implemented:

- attachment upload from inline data or staged storage keys
- status-frame delivery
- delivery-status callback processing beyond acknowledgement
- per-number provisioning or Twilio control-plane management

## Build

```bash
cargo build --release
```

## Configuration

Required env vars:

- `TWILIO_ACCOUNT_SID`
- `TWILIO_AUTH_TOKEN`

Optional env vars:

- `TWILIO_WEBHOOK_AUTH_TOKEN` - not sent by Twilio itself, but useful if your host wants to require an additional shared token at the ingress edge

Useful config fields:

- `from_number` - source number for outbound SMS
- `messaging_service_sid` - optional Twilio Messaging Service SID instead of a direct source number
- `webhook_public_url` - public base URL used for ingress declaration
- `webhook_path` - ingress route path, default `/twilio/sms`

## Setup

To obtain the required Twilio credentials:

1. Create or open a Twilio project in the Twilio Console.
2. Copy the Account SID and export it as `TWILIO_ACCOUNT_SID`.
3. Copy the Auth Token and export it as `TWILIO_AUTH_TOKEN`.
4. Provision a Twilio phone number or Messaging Service for outbound SMS.
5. Set `from_number` or `messaging_service_sid` in the plugin config.

Minimal config:

```toml
from_number = "+15551234567"
webhook_public_url = "https://example.com"
webhook_path = "/twilio/sms"
```

## Dispatch usage

```bash
dispatch channel call channel-twilio-sms \
  --request-json '{"kind":"health","config":{"from_number":"+15551234567"}}'

dispatch channel listen channel-twilio-sms \
  --listen 127.0.0.1:8787 \
  --config-file ./twilio-sms.toml

dispatch channel call channel-twilio-sms \
  --request-json '{"kind":"push","config":{"from_number":"+15551234567"},"message":{"to_number":"+15557654321","content":"Dispatch SMS test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Dispatch operators normally use the host CLI rather than writing raw envelopes.

## Notes on ingress

Twilio does not have a webhook registration API shaped like Telegram's `setWebhook`. This plugin therefore treats `start_ingress` as a configuration handshake:

1. validate Twilio credentials
2. validate the declared public route
3. report the endpoint and Twilio signature-verification requirement to the host

That keeps the plugin honest about what it can support.

## License

MIT
