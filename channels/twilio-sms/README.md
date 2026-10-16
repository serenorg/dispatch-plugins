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

Each `ingress_event` confirms a `channel.event` batch with the host, including an empty batch when a callback is ignored or rejected, and the plugin answers the request, including any HTTP callback reply, only after the exact durable host receipt. A callback that repeats the accepted batch resends that accepted original instead of journaling a new one. A request that fails before it produces a batch returns a plugin error and confirms nothing. An unconfirmed journaled batch remains pending and is replayed before the next batch. Whether Twilio retries a failed callback follows the provider's delivery policy; consumers must deduplicate by stable event ID.

For authenticated inbound webhooks, Twilio Advanced Opt-Out's `OptOutType=STOP`, `START`, or `HELP` becomes message metadata `opt_out_type=stop`, `start`, or `help`. Missing or unknown values produce no metadata field. Message text alone never determines this metadata. Consumers can use it to avoid replying to keywords that Twilio has already answered.

When Twilio refuses a REST request with HTTP 4xx and a positive JSON integer `code` that fits in an unsigned 32-bit integer, the plugin returns that code in the existing typed error payload as `twilio_<decimal>`, such as `twilio_21610`. The error message is sanitized and contains no provider response text, message body, recipient, SID, or URL. HTTP 5xx and transport failures remain generic plugin errors without a Twilio refusal code; consumers must not classify errors by matching message text.

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
- either `TWILIO_AUTH_TOKEN` or the pair `TWILIO_API_KEY_SID` and `TWILIO_API_KEY_SECRET`. If both are set, API-key credentials take precedence for outbound REST calls.

Optional env vars:

- `TWILIO_WEBHOOK_AUTH_TOKEN` - override the secret used to verify `X-Twilio-Signature`; if omitted, the plugin uses `TWILIO_AUTH_TOKEN`

Useful config fields:

- `from_number` - source number for outbound SMS
- `messaging_service_sid` - optional Twilio Messaging Service SID instead of a direct source number
- `webhook_public_url` - public base URL used for ingress declaration
- `webhook_path` - ingress route path, default `/twilio/sms`

## Setup

To obtain the required Twilio credentials:

1. Create or open a Twilio project in the Twilio Console.
2. Copy the Account SID and export it as `TWILIO_ACCOUNT_SID`.
3. For REST API authentication, either:
  - copy the Auth Token and export it as `TWILIO_AUTH_TOKEN`, or
  - create an API key and export `TWILIO_API_KEY_SID` plus `TWILIO_API_KEY_SECRET`
4. Provision a Twilio phone number or Messaging Service for outbound SMS.
5. Set `from_number` or `messaging_service_sid` in the plugin config.
6. If you want inbound webhook verification while using API-key auth, also provide the account Auth Token via `TWILIO_WEBHOOK_AUTH_TOKEN` or `TWILIO_AUTH_TOKEN`.

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

When using `TWILIO_API_KEY_SID` and `TWILIO_API_KEY_SECRET`, the plugin validates REST access against the Messages API instead of the account identity endpoint. Twilio webhook signature verification still uses the account Auth Token, so inbound SMS handling needs `TWILIO_WEBHOOK_AUTH_TOKEN` or `TWILIO_AUTH_TOKEN` even when outbound requests use API-key auth.

## License

MIT
