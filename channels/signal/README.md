# channel-signal

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Signal using the upstream `signal-cli-rest-api` HTTP API.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `start_ingress`
- `stop_ingress`
- `poll_ingress`
- `deliver`
- `push`
- `status`

Behavior:

- outbound delivery uses `POST /v2/send`
- outbound delivery supports base64-backed attachments
- health checks validate connectivity with `GET /v1/about`
- status frames map to Signal typing indicators with
  `PUT /v1/typing-indicator/{number}`
- polling ingress uses `GET /v1/receive/{number}` and normalizes inbound
  message envelopes into Dispatch events

Not implemented:

- webhook-style `ingress_event` handling
- message reactions or read receipts

## Configuration

Either set:

- `base_url` in the plugin config

Or provide:

- `SIGNAL_RPC_URL`

Optional config fields:

- `account` - sending account or linked Signal identity when the backend
  requires it
- `default_recipient` - fallback recipient for `deliver`, `push`, and `status`
- `poll_timeout_secs` - polling receive timeout in seconds, minimum 1

`config.account` is also required for polling ingress, because the Signal
backend `receive` call is scoped to a specific account.

Recipients may be plain phone numbers, `group:<id>`, `username:<value>`, or
prefixed forms like `signal:+15551234567`.

## Setup

This plugin expects a running
[`bbernhard/signal-cli-rest-api`](https://github.com/bbernhard/signal-cli-rest-api)
backend.

Typical setup:

1. Provision or link a Signal account with `signal-cli`.
2. Start `signal-cli-rest-api` in one of its supported modes:
  - `native` or `normal` for HTTP polling via `GET /v1/receive/{number}`
  - `json-rpc` for websocket receive on `/v1/receive/{number}`
3. Point the plugin at that service with `base_url` or `SIGNAL_RPC_URL`.
4. Set `account` in the plugin config when the backend requires an explicit
  account selector for polling and sending.

Notes:

- `poll_ingress` adapts to the upstream backend mode automatically:
  - `native` and `normal` use the documented HTTP receive endpoint
  - `json-rpc` uses the documented websocket receive endpoint
- `deliver` and `status` target the upstream REST API routes documented by
  `signal-cli-rest-api`, not the older `/api/v1/rpc` wrapper contract.

## Build

```bash
cargo build --release
```

## License

MIT
