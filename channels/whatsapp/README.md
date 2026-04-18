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

## Protocol

Requests are sent as JSONL on stdin:

```json
{"protocol_version":1,"request":{"kind":"capabilities"}}
```

Delivery example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "deliver",
    "config": {
      "phone_number_id": "1234567890"
    },
    "message": {
      "to_number": "15557654321",
      "content": "Dispatch says hello from WhatsApp."
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
      "phone_number_id": "1234567890"
    },
    "message": {
      "to_number": "15557654321",
      "content": "Dispatch scheduled update from WhatsApp."
    }
  }
}
```

## Notes on ingress

WhatsApp Cloud API uses a webhook callback configured in the Meta developer
console rather than a bot-side registration API. This plugin therefore
treats `start_ingress` as a configuration handshake:

1. validate access token and route shape
2. report the expected callback URL
3. report verify-token and signature-verification requirements to the host

## License

MIT
