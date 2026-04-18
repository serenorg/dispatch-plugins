# channel-signal

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Signal using a `signal-cli` REST or JSON-RPC endpoint.

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

- outbound delivery uses `POST /api/v1/rpc` with the `send` method
- outbound delivery supports base64-backed attachments
- health checks validate connectivity with `GET /api/v1/check`
- status frames map to Signal typing indicators with `sendTyping`
- polling ingress calls the Signal `receive` method and normalizes inbound
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
- `poll_timeout_secs` - polling receive timeout in seconds, clamped to 1-30

`config.account` is also required for polling ingress, because the Signal
backend `receive` call is scoped to a specific account.

Recipients may be plain phone numbers, `group:<id>`, `username:<value>`, or
prefixed forms like `signal:+15551234567`.

## Setup

This plugin expects a running `signal-cli` REST or JSON-RPC backend.

Typical setup:

1. Provision or link a Signal account with `signal-cli`.
2. Start a `signal-cli` REST or JSON-RPC service that exposes the account.
3. Point the plugin at that service with `base_url` or `SIGNAL_RPC_URL`.
4. Set `account` in the plugin config when the backend requires an explicit
    account selector for polling and sending.

## Build

```bash
cargo build --release
```

## License

MIT
