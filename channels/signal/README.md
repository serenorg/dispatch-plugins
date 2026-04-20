# channel-signal

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for Signal, using the native Rust [presage](https://github.com/whisperfish/presage) client.

**No Docker. No signal-cli. No external daemon.** The plugin is a single Rust binary that owns its own Signal session on disk and talks directly to Signal's servers over WebSocket. It ships with a built-in `--link` subcommand that pairs the plugin as a secondary device via QR code scanned from the Signal app on your phone.

> This plugin links against `presage`, which is licensed under AGPL-3.0. The `channel-signal` binary is therefore distributed under AGPL-3.0.

This repository is the source of truth for the first-party Signal channel plugin. The main `dispatch-plugins` repository keeps only a pointer README plus catalog metadata so Signal remains discoverable without carrying its dependency graph inside the standard plugin workspace.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `poll_ingress` (one-shot drain)
- `start_ingress` (background receive loop, emits `channel.event`)
- `stop_ingress`
- `deliver` (text + `data_base64` attachments)
- `push` (same send path as `deliver`)
- `status` (mapped to Signal typing indicators)
- `shutdown`

Not yet implemented (v0.1.1 follow-ups):

- inbound attachment byte download (metadata is surfaced on inbound events, but the encrypted bytes remain on Signal's CDN until a dedicated fetch step lands)
- group messaging (send + receive)
- username-based recipients
- reactions, edits, read receipts as distinct events

## Install

Build from source. This is not a Rust-only build: the `presage` + `libsignal` dependency stack requires a working `protoc` at build time on every platform, and the bundled SQLCipher/OpenSSL path also requires a native C toolchain. Install `protoc` via `brew install protobuf` on macOS, your distribution's package manager on Linux, or `choco install protoc` on Windows. The `choco` examples assume Chocolatey is already installed; otherwise, install Chocolatey first or place `protoc.exe` and `perl.exe` on `PATH` manually. On Windows, the vendored OpenSSL build also requires `perl` on `PATH` (for example `choco install strawberryperl`) and the MSVC C toolchain from Visual Studio Build Tools. The resulting `channel-signal` binary has no runtime dependencies beyond TLS root certificates.

```bash
cargo build --release
```

The binary is written to `target/release/channel-signal`.

## One-time setup: link as a secondary Signal device

Before the plugin can send or receive, it must be linked to your Signal account. Run the built-in `--link` subcommand and scan the displayed QR code from the Signal app on your phone (Signal -> Settings -> Linked Devices -> Link a Device):

```bash
channel-signal --link --device-name "Dispatch"
```

Flags:

```
-n, --device-name <NAME>        Device name shown in Signal's Linked
                                Devices list. Defaults to `Dispatch`.
    --account <NAME>            Logical account name; selects the
                                per-account subdirectory under the
                                default store root. Defaults to
                                `default`. Use this to link multiple
                                Signal accounts on the same host.
    --sqlite-store-path <PATH>  Absolute path to the SQLite store.
                                Overrides --account.
    --passphrase-env <NAME>     Name of an env var holding a
                                passphrase used to encrypt the store
                                at rest.
-s, --servers <production|staging>
                                Signal server pool. Defaults to
                                production.
```

On success the plugin prints a JSON summary containing the ACI, phone number, device id, and store path. Subsequent plugin invocations (with no arguments) reuse the stored session.

## Configuration

All configuration fields are optional. Defaults match what `--link` wrote to disk.

| Field | Purpose |
| --- | --- |
| `sqlite_store_path` | Absolute path to the SQLite store. Overrides `account`. |
| `account` | Logical account name selecting the per-account subdirectory when `sqlite_store_path` is unset. Defaults to `default`. |
| `passphrase_env` | Env var name holding a passphrase used to encrypt the store at rest (SQLCipher `PRAGMA key`). Leave unset for an unencrypted store. |
| `default_recipient` | Fallback recipient for operator-driven `push`, `deliver`, and `status` frames with no routing metadata. |
| `poll_timeout_secs` | Receive timeout (seconds) for one `poll_ingress` cycle. Defaults to 10. |

Default store layout:

```
$XDG_CONFIG_HOME/dispatch/channels/signal/<account>/store.db
# or
$HOME/.config/dispatch/channels/signal/<account>/store.db
```

Recipient format for outbound delivery and status frames:

- bare UUID                               -> treated as ACI
- `ACI:<uuid>`                            -> ACI
- `PNI:<uuid>`                            -> PNI

Phone numbers (`+15551234567`) and Signal usernames are not yet accepted on outbound; wait for inbound events first to capture the ACI, which is surfaced as both `conversation.id` and `actor.id`.

## Dispatch usage

```bash
dispatch channel install \
  https://raw.githubusercontent.com/serenorg/dispatch-channel-signal/v0.1.0/channel-plugin.json

dispatch channel call channel-signal \
  --request-json '{"kind":"health","config":{}}'

dispatch channel poll channel-signal \
  --config-file ./signal.toml --once

dispatch channel call channel-signal \
  --request-json '{
    "kind": "push",
    "config": {},
    "message": {
      "content": "Hello from Dispatch",
      "metadata": {
        "conversation_id": "00000000-0000-0000-0000-000000000042"
      }
    }
  }'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Dispatch operators normally go through the host CLI rather than writing raw envelopes.

## Ingress model

- `poll_ingress` opens a single WebSocket to Signal, drains all queued messages until Signal reports `QueueEmpty`, then closes. Useful for CLI-driven `dispatch channel poll --once` and for short-lived environments.
- `start_ingress` / `stop_ingress` run a persistent background receive worker on a dedicated OS thread with its own tokio current-thread runtime. Inbound messages are delivered back to the host as `channel.event` notifications between JSON-RPC requests.

The plugin always owns the upstream receive loop. There is no webhook or polling-of-third-party-service transport.

Both ingress paths emit the same `InboundEventEnvelope` shape:

- `conversation.id` = sender ACI (as a UUID string)
- `conversation.kind` = `signal_direct`
- `actor.id`          = sender ACI
- `message.content`   = the text body (or a placeholder when the message only carries attachments)
- `message.attachments[]` = metadata per attachment (name, MIME type, size). Attachment bytes are not downloaded in v0.1.0.

## Outbound attachments

Attach inline with `message.attachments`, providing `name`, `mime_type`, and `data_base64`. URL- and storage-key-backed attachments are not yet supported; dispatch stages media locally and the caller should inline it as base64.

```json
{
  "kind": "deliver",
  "config": {},
  "message": {
    "content": "here's the report",
    "metadata": {"conversation_id": "00000000-0000-0000-0000-000000000042"},
    "attachments": [
      {
        "name": "report.pdf",
        "mime_type": "application/pdf",
        "data_base64": "..."
      }
    ]
  }
}
```

## Status frames

Dispatch emits `status` frames to mark turn-taking. The plugin maps the subset that makes sense for Signal into typing indicators:

| StatusKind | Signal action |
| --- | --- |
| `processing`, `delivering`, `operation_started` | typing started |
| `completed`, `cancelled`, `operation_finished` | typing stopped |
| `info`, `approval_needed`, `auth_required`, others | accepted, no upstream traffic |

The status frame's `conversation_id` is required and is treated the same way as an outbound recipient.

## Notes on security

- The SQLite store holds your Signal identity keys, sessions, and message history. Treat it like a private key file.
- `chmod 700 ~/.config/dispatch/channels/signal/<account>/` is recommended.
- For defense-in-depth, set `passphrase_env` and export the passphrase through whatever secret manager your deployment uses. The passphrase is never written to disk.
- To revoke this device's access, open Signal on your phone -> Settings -> Linked Devices and remove the Dispatch entry.

## License

AGPL-3.0
