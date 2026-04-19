# channel-webhook

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for generic webhook-based integrations.

This is the simplest concrete channel plugin shape from the extension docs:

- webhook ingress instead of a platform-specific SDK
- JSON `POST` outbound delivery instead of a channel-specific send API
- no courier behavior mixed into the plugin

It is intended as a small, generic baseline for the channel family.

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
- `shutdown`

Behavior:

- outbound delivery `POST`s JSON to a configured webhook URL
- outbound attachments are forwarded in the JSON payload using the shared
  `attachments` array
- status frames `POST` JSON to the configured webhook URL
- optional bearer-token auth for outbound delivery
- ingress parses raw webhook requests into normalized inbound events
- ingress declares a public endpoint and optional shared-secret header
- health validates config and can probe a configured health URL

Not implemented:

- retries and backoff

## Build

```bash
cargo build --release
```

## Availability

Inbound:

- webhook ingress only
- Dispatch terminates the HTTP listener and forwards the raw request to the
  plugin through `ingress_event`

Outbound:

- `deliver`, `push`, and `status` can target `outbound_url`
- individual messages may override the destination with `destination_url`

## Configuration

Optional environment:

- `WEBHOOK_INGRESS_SECRET` - shared secret used by the host or plugin to
  verify inbound webhook traffic
- `WEBHOOK_OUTBOUND_BEARER_TOKEN` - bearer token attached to outbound webhook
  deliveries

Primary config fields:

- `webhook_public_url` - base public URL for ingress
- `webhook_path` - ingress route path, default `/webhook/inbound`
- `ingress_secret_env` - override the default secret env var name
- `ingress_secret_header` - override the default inbound secret header name,
  which is `X-Dispatch-Webhook-Secret`
- `outbound_url` - required for `deliver` unless provided per message
- `outbound_bearer_token_env` - override the outbound bearer-token env var name
- `healthcheck_url` - optional URL probed by `health`
- `static_headers` - static headers attached to outbound delivery and status
  requests

Minimal config:

```toml
webhook_public_url = "https://example.com"
webhook_path = "/webhook/inbound"
outbound_url = "https://hooks.example.com/dispatch"
```

## Setup

1. Choose the public base URL that Dispatch should advertise for ingress.
2. Set `webhook_public_url` and optionally `webhook_path`.
3. If you want a shared-secret header, export `WEBHOOK_INGRESS_SECRET`.
4. If outbound delivery requires auth, export
    `WEBHOOK_OUTBOUND_BEARER_TOKEN`.
5. Start the listener through Dispatch and route inbound `POST`s to the
    reported endpoint.

If you are testing locally, `dispatch channel listen` plus `curl` is enough;
there is no third-party control plane to configure.

## Dispatch usage

```bash
dispatch channel call channel-webhook \
  --request-json '{"kind":"health","config":{"healthcheck_url":"https://hooks.example.com/health"}}'

dispatch channel listen channel-webhook \
  --listen 127.0.0.1:8794 \
  --config-file ./webhook.toml

dispatch channel call channel-webhook \
  --request-json '{"kind":"push","config":{"outbound_url":"https://hooks.example.com/dispatch"},"message":{"content":"Dispatch webhook test","conversation_id":"customer-123"}}'
```

A local ingress smoke test looks like this:

```bash
curl -X POST http://127.0.0.1:8794/webhook/inbound \
  -H 'Content-Type: application/json' \
  -H 'X-Dispatch-Webhook-Secret: replace-me' \
  --data '{"event_type":"message.received","conversation_id":"customer-123","actor":{"id":"user-1"},"message":{"content":"hello"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. The raw HTTP request
body, headers, query params, and trust-verification result are carried through
`ingress_event`.

## Inbound payload shape

The plugin accepts arbitrary text bodies, but JSON bodies are the most useful.
These keys are recognized when present:

- `event_type` or `type`
- `event_id`
- `conversation_id` or nested `conversation.id`
- `thread_id`
- `actor_id` or nested `actor.id`
- nested `actor.display_name`, `actor.username`, `actor.is_bot`
- `message_id` or nested `message.id`
- nested `message.content`, `message.content_type`, `message.attachments`

Anything not recognized remains available in the original body text or payload
metadata that the host passes to the plugin.

## Notes

This plugin is deliberately generic. That makes it useful in two ways:

1. it is an immediate integration for any system that can receive `POST`s
2. it is a clean baseline for understanding the shared Dispatch channel
    protocol surface

## License

MIT
