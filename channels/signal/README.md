# channel-signal

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Signal using the upstream `signal-cli-rest-api` API.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `poll_ingress`
- `start_ingress`
- `stop_ingress`
- `deliver`
- `push`
- `status`
- `shutdown`

Behavior:

- outbound delivery uses `POST /v2/send`
- outbound delivery supports base64-backed attachments
- health checks validate connectivity with `GET /v1/about`
- status frames map to Signal typing indicators with
  `PUT /v1/typing-indicator/{number}`
- ingress supports both one-shot `poll_ingress` fetches and background
  `start_ingress` sessions
- the plugin automatically adapts to the backend mode:
  - `native` and `normal` use HTTP receive
  - `json-rpc` uses the websocket receive endpoint

Not implemented:

- webhook-style `ingress_event` handling
- message reactions or read receipts

## Configuration

Either set:

- `base_url` in the plugin config

Or provide:

- `SIGNAL_RPC_URL`

Optional config fields:

- `base_url_env` - override the base-url env var name, defaults to
  `SIGNAL_RPC_URL`
- `account` - sending account or linked Signal identity; required for ingress
  because receive calls are account-scoped
- `default_recipient` - fallback recipient for `deliver`, `push`, and `status`
- `poll_timeout_secs` - receive timeout in seconds, minimum 1

Recipients may be plain phone numbers, `group:<id>`, `username:<value>`, or
prefixed forms such as `signal:+15551234567`.

## Availability

Inbound:

- available as either one-shot polling or a background ingress session
- there is no webhook mode; the plugin always owns the upstream receive loop
- upstream transport is HTTP in `native` / `normal` mode and websocket in
  `json-rpc` mode

Outbound:

- available whenever the configured Signal backend can send messages for the
  chosen account

## Setup

This plugin expects a running
[`bbernhard/signal-cli-rest-api`](https://github.com/bbernhard/signal-cli-rest-api)
backend.

Typical setup:

1. Provision or link a Signal account with `signal-cli`.
2. Start `signal-cli-rest-api` in one of its supported modes:
    - `native` or `normal` for HTTP receive
    - `json-rpc` for websocket-backed receive
3. Point the plugin at that service with `base_url` or `SIGNAL_RPC_URL`.
4. Set `account` in the plugin config.
5. Optionally set `default_recipient` for proactive outbound delivery.

One practical Docker setup:

```bash
mkdir -p "$HOME/.local/share/signal-cli"
docker run -d --name signal-api --restart=always \
  -p 8080:8080 \
  -v "$HOME/.local/share/signal-cli:/home/.local/share/signal-cli" \
  -e MODE=json-rpc \
  bbernhard/signal-cli-rest-api
```

Linking a device:

1. Open the QR code endpoint exposed by `signal-cli-rest-api`, for example
    `http://127.0.0.1:8080/v1/qrcodelink?device_name=dispatch`.
2. In the Signal mobile app, open `Settings -> Linked devices`.
3. Scan the QR code.
4. Confirm that `GET /v1/about` returns the expected mode and version.

Minimal config:

```toml
base_url = "http://127.0.0.1:8080"
account = "+15551234567"
poll_timeout_secs = 30
default_recipient = "+15557654321"
```

Environment example:

```bash
export SIGNAL_RPC_URL="http://127.0.0.1:8080"
```

## Dispatch usage

Dispatch operators normally drive Signal through the host CLI:

```bash
dispatch channel call channel-signal \
  --request-json '{"kind":"health","config":{"base_url":"http://127.0.0.1:8080","account":"+15551234567"}}'

dispatch channel poll channel-signal \
  --config-file ./signal.toml --once

dispatch channel call channel-signal \
  --request-json '{"kind":"push","config":{"base_url":"http://127.0.0.1:8080","account":"+15551234567","default_recipient":"+15557654321"},"message":{"content":"Dispatch Signal test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. The upstream Signal
transport may still be HTTP or websocket depending on backend mode; Dispatch
does not speak websocket to the plugin directly.

## Notes

- `poll_ingress` performs one receive cycle against the configured backend
- `start_ingress` validates the account and backend, then starts the background
  receive worker
- inbound messages are emitted back to Dispatch as `channel.event`
  notifications
- `ingress_event` is intentionally unsupported because Signal is not modeled as
  a host-managed webhook integration

Common failure modes:

- `signal polling ingress requires config.account` means the plugin does not
  know which Signal identity to poll
- connection errors against `/v1/about` usually mean the REST API container is
  not running or `base_url` is wrong
- websocket receive issues in `json-rpc` mode usually mean the backend is not
  actually running in `json-rpc` mode

## Build

```bash
cargo build --release
```

## License

MIT
