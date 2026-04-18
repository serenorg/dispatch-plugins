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
4. Confirm that `GET /v1/about` returns the expected mode and version from the
  backend.

Choosing a backend mode:

- `normal` is the safest baseline and works everywhere
- `native` reduces startup overhead for each HTTP receive/send call
- `json-rpc` keeps a daemon process alive and gives the best receive latency;
  this plugin uses websocket-backed receive in that mode

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

Notes:

- `poll_ingress` adapts to the upstream backend mode automatically:
  - `native` and `normal` use the documented HTTP receive endpoint
  - `json-rpc` uses the documented websocket receive endpoint
- the plugin still exposes the same Dispatch `poll_ingress` request in both
  cases; only the upstream Signal transport changes
- `deliver` and `status` target the upstream REST API routes documented by
  `signal-cli-rest-api`, not the older `/api/v1/rpc` wrapper contract.

Common failure modes:

- `signal polling ingress requires config.account` means the plugin does not
  know which Signal identity to poll.
- connection errors against `/v1/about` usually mean the REST API container is
  not running or `base_url` is wrong.
- websocket receive issues in `json-rpc` mode usually mean the backend is not
  actually running in `json-rpc` mode or the plugin binary was built without
  websocket TLS support.

## Build

```bash
cargo build --release
```

## License

MIT
