# channel-webhook

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for generic webhook-based integrations.

This is the simplest concrete channel plugin shape from the extension docs:

- webhook ingress instead of a platform-specific SDK
- JSON POST outbound delivery instead of a channel-specific send API
- no courier behavior mixed into the plugin

It is intended as a small, generic baseline for the channel family.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `start_ingress`
- `stop_ingress`
- `deliver`
- `push`
- `ingress_event`
- `status`

Behavior:

- outbound delivery POSTs JSON to a configured webhook URL
- outbound attachments are forwarded in the JSON payload using the shared
  `attachments` array
- status frames POST JSON to the configured webhook URL
- optional bearer-token auth for outbound delivery
- ingress parses raw webhook requests into normalized inbound events
- ingress declares a public endpoint and optional shared-secret header
- health validates config and optionally probes a configured health URL

Not implemented:

- retries and backoff

## Build

```bash
cargo build --release
```

## Configuration

Optional env vars:

- `WEBHOOK_INGRESS_SECRET` - shared secret used by the host to verify inbound
  webhook traffic
- `WEBHOOK_OUTBOUND_BEARER_TOKEN` - bearer token attached to outbound webhook
  deliveries

Primary config fields:

- `webhook_public_url` - base public URL for ingress
- `webhook_path` - ingress route path, default `/webhook/inbound`
- `outbound_url` - required for `deliver` unless provided per-message
- `outbound_url` - also used for `status` unless `metadata.destination_url` overrides it
- `healthcheck_url` - optional URL probed by `health`

## Protocol

Requests are sent as JSONL on stdin:

```json
{"protocol_version":1,"request":{"kind":"capabilities"}}
```

Ingress example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "start_ingress",
    "config": {
      "webhook_public_url": "https://example.com",
      "webhook_path": "/dispatch/webhook"
    }
  }
}
```

Ingress parsing example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "ingress_event",
    "config": {},
    "payload": {
      "endpoint_id": "dispatch_webhook",
      "method": "POST",
      "path": "/dispatch/webhook",
      "headers": {
        "Content-Type": "application/json"
      },
      "query": {},
      "body": "{\"message\":\"hello\"}",
      "trust_verified": true
    }
  }
}
```

Status example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "status",
    "config": {
      "outbound_url": "https://hooks.example.com/dispatch"
    },
    "update": {
      "kind": "processing",
      "message": "Dispatch is working on the request.",
      "conversation_id": "customer-123",
      "thread_id": "thread-9",
      "metadata": {}
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
      "outbound_url": "https://hooks.example.com/dispatch"
    },
    "message": {
      "content": "Dispatch says hello from the webhook channel.",
      "conversation_id": "customer-123",
      "metadata": {
        "run_id": "run_abc"
      }
    }
  }
}
```

## Notes

This plugin is deliberately generic. That makes it useful in two ways:

1. it is an immediate integration for any system that can receive POSTs
2. it is a clean baseline for understanding the shared Dispatch channel
  protocol surface

## License

MIT
