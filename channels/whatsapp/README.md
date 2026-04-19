# channel-whatsapp

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for the WhatsApp Cloud API.

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

- outbound delivery uses the WhatsApp Cloud API `messages` endpoint
- outbound media supports one URL-backed or pre-uploaded media item per
  message, using `whatsapp:media:<id>` storage keys for existing media ids
- inline `data_base64` media is uploaded to WhatsApp first and then sent by the
  returned media id
- health checks validate the access token against Graph API `me`
- ingress handles Meta webhook verification and validates `X-Hub-Signature-256`
- inbound webhook payloads are normalized into Dispatch events
- status frames render visible status messages into WhatsApp conversations

Not implemented:

- webhook registration in the Meta developer console
- delivery status callback processing

## Build

```bash
cargo build --release
```

## Configuration

Required env vars:

- `WHATSAPP_ACCESS_TOKEN`

Optional env vars:

- `WHATSAPP_VERIFY_TOKEN` - verify token used when configuring the webhook in
  Meta
- `WHATSAPP_APP_SECRET` - useful for host-side `X-Hub-Signature-256`
  verification

Useful config fields:

- `phone_number_id` - required for outbound delivery
- `api_version` - defaults to `v18.0`
- `webhook_public_url` - public base URL for ingress declaration
- `webhook_path` - ingress route path, default `/whatsapp/webhook`

## Setup

To obtain the required WhatsApp Cloud API credentials:

1. Create a Meta app with WhatsApp enabled in the Meta developer console.
2. Generate or copy a WhatsApp Cloud API access token.
3. Export it as `WHATSAPP_ACCESS_TOKEN`.
4. Copy the WhatsApp phone number id for the sending number and set it as
    `phone_number_id` in the plugin config.
5. If you are configuring webhook verification, choose a verify token and
    export it as `WHATSAPP_VERIFY_TOKEN`.
6. If you want to validate `X-Hub-Signature-256`, copy the app secret and
    export it as `WHATSAPP_APP_SECRET`.

Minimal config:

```toml
phone_number_id = "1234567890"
webhook_public_url = "https://example.com"
webhook_path = "/whatsapp/webhook"
```

## Dispatch usage

```bash
dispatch channel call channel-whatsapp \
  --request-json '{"kind":"health","config":{"phone_number_id":"1234567890"}}'

dispatch channel listen channel-whatsapp \
  --listen 127.0.0.1:8787 \
  --config-file ./whatsapp.toml

dispatch channel call channel-whatsapp \
  --request-json '{"kind":"push","config":{"phone_number_id":"1234567890"},"message":{"to_number":"15557654321","content":"Dispatch WhatsApp test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Dispatch operators
normally use the host CLI rather than writing raw envelopes.

## Notes on ingress

WhatsApp Cloud API uses a webhook callback configured in the Meta developer
console rather than a bot-side registration API. This plugin therefore
treats `start_ingress` as a configuration handshake:

1. validate access token and route shape
2. report the expected callback URL
3. report verify-token and signature-verification requirements to the host

## License

MIT
