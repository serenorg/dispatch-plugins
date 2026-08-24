# channel-whatsapp

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for WhatsApp, using the native Rust [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust) client.

**No Docker. No Meta Business Account. No Cloud API app. No external daemon.** The plugin is a single Rust binary that links a WhatsApp Web session via QR code and stores that session locally in SQLite.

This repository is the source of truth for the first-party WhatsApp channel plugin. The main `dispatch-plugins` repository keeps only a pointer README plus catalog metadata so WhatsApp remains discoverable without carrying its dependency graph inside the standard plugin workspace.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `poll_ingress` (one-shot receive)
- `start_ingress` (background receive worker, emits `channel.event`)
- `stop_ingress`
- `deliver` (text plus one optional `data_base64` attachment)
- `push` (same send path as `deliver`)
- `status` (mapped to WhatsApp typing indicators)
- `shutdown`

Not yet implemented:

- inbound attachment byte download (metadata only)
- multiple outbound attachments in one Dispatch message
- group send and group receive as first-class behavior
- reactions, edits, read receipts as distinct event types
- phone-number-to-JID resolution

## Install

Build from source. The `whatsapp-rust` dependency stack requires a working `protoc` at build time on every platform. Install it via `brew install protobuf` on macOS, your distribution's package manager on Linux, or `choco install protoc` on Windows. The `choco` example assumes Chocolatey is already installed; otherwise, install Chocolatey first or place `protoc.exe` on `PATH` manually. The resulting `channel-whatsapp` binary has no runtime dependencies beyond TLS root certificates.

```bash
cargo build --release
```

The binary is written to `target/release/channel-whatsapp`.

## One-time setup: link as a WhatsApp Web device

Before the plugin can send or receive, it must be linked to a WhatsApp account. Run the built-in `--link` subcommand and scan the displayed QR code from the WhatsApp app on your phone (WhatsApp -> Settings -> Linked Devices -> Link a Device):

```bash
channel-whatsapp --link --device-name "Dispatch"
```

Flags:

```
-n, --device-name <NAME>        Device name shown in WhatsApp's linked
                                devices list. Defaults to `Dispatch`.
    --account <NAME>            Logical account name; selects the
                                per-account subdirectory under the
                                default store root. Defaults to
                                `default`. Use this to link multiple
                                WhatsApp accounts on the same host.
    --sqlite-store-path <PATH>  Absolute path to the SQLite store.
                                Overrides --account.
-h, --help                      Print help and exit.
```

On success the plugin prints a JSON summary containing the linked JIDs, device id, push name, and store path. Subsequent runs with no arguments reuse the stored session.

## Configuration

| Field | Purpose |
| --- | --- |
| `sqlite_store_path` | Absolute path to the SQLite store. Overrides `account`. |
| `account` | Logical account name selecting the per-account subdirectory when `sqlite_store_path` is unset. Defaults to `default`. |
| `default_recipient` | Fallback JID for operator-driven `push`, `deliver`, and `status` frames with no routing metadata. |
| `poll_timeout_secs` | Receive timeout in seconds for one `poll_ingress` cycle. Defaults to 10. |
| `dm_policy` | Direct-message ingress policy: `deny`, `allowlist`, or `open`. Omitted means `deny`. Persistent ingress requires `allowlist` with senders or explicit `open`. |
| `allowed_dm_sender_ids` | Exact direct-message JIDs permitted when `dm_policy` is `allowlist`. Empty lists deny all; wildcards and group JIDs are rejected. |
| `outbound_recipient_ids` | Optional direct-message destinations. Empty falls back to `allowed_dm_sender_ids`, so outbound delivery cannot widen an allowlist. |
| `reply_delivery` | `runtime_owned` (default) or `tool_owned`, selecting the path that publishes the visible reply. |

Default store layout:

```
$XDG_CONFIG_HOME/dispatch/channels/whatsapp/<account>/store.db
# or
$HOME/.config/dispatch/channels/whatsapp/<account>/store.db
```

## Recipient format

Outbound delivery and status frames currently require a full WhatsApp JID:

- `15551234567@s.whatsapp.net` for a direct-message chat
- `<lid>@lid` for LID-addressed direct-message chats

Bare phone numbers are rejected. Group JIDs (`@g.us`) are not supported.

## Authorization policy

WhatsApp persistent ingress is direct-message-only. The plugin rejects group messages before emitting a channel event and evaluates the same normalized policy for configuration, inbound messages, `deliver`, `push`, and typing-status frames. An `allowlist` requires one or more exact direct WhatsApp JIDs; `open` is an explicit unbounded direct-message surface; `deny` has no ingress or outbound surface. The plugin records its authenticated linked-account JID and direct-message activation evidence on each accepted event so the host can re-authorize it.

The intended operator flow is to receive an inbound event first, then reuse `conversation.id` as the outbound `message.metadata.conversation_id`.

## Dispatch usage

```bash
dispatch channel call channel-whatsapp \
  --request-json '{"kind":"health","config":{}}'

dispatch channel poll channel-whatsapp \
  --config-file ./whatsapp.toml --once

dispatch channel call channel-whatsapp \
  --request-json '{
    "kind": "push",
    "config": {},
    "message": {
      "content": "Hello from Dispatch",
      "metadata": {
        "conversation_id": "15551234567@s.whatsapp.net"
      }
    }
  }'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Operators normally go through the Dispatch CLI rather than writing raw envelopes.

## Ingress model

- `poll_ingress` opens a WhatsApp session, drains queued inbound messages until the idle window or timeout is reached, then exits. This is the one-shot path used by `dispatch channel poll --once`.
- `start_ingress` / `stop_ingress` run a background receive worker on a dedicated OS thread with its own current-thread tokio runtime. The worker sends inbound messages to the host as `channel.event` notifications without waiting for another JSON-RPC request.

## Delivery guarantee

Inbound delivery is best effort. Messages can be lost or processed more than once. Do not rely on this plugin for messages that must never be lost or duplicated.

`whatsapp-rust` acknowledges a message to WhatsApp after it dispatches its event handlers, and it dispatches them as detached tasks. The plugin handler only places the message on an in-memory channel and returns, so the acknowledgement is not tied to the message reaching the host. The receive worker then writes a `channel.event` notification to stdout. A crash or a failed stdout write after the provider acknowledgement and before that write drops the message: WhatsApp considers it delivered, and the plugin has no local outbox and no replay path.

Redelivery is equally unprotected in the other direction: the host allocates a new execution identifier for every notification and does not claim the provider event id, so a message that did arrive twice would produce two runs and two replies.

Reliable delivery without duplicate runs needs all three of the following, none of which exist yet:

- a host-side claim on `(organization, deployment, binding, platform account, provider event id)` taken before a run is created, so redelivery is collapsed into one run and one reply;
- an explicit host-acceptance acknowledgement in the channel protocol, so the plugin learns when a notification is durably held rather than merely written;
- a plugin-side outbox that retains an event until that acknowledgement arrives.

Inbound media is surfaced as attachment metadata:

- `message.attachments[].kind` = `image`, `video`, `audio`, or `document`
- `mime_type`, `size_bytes`, and `name` are populated when WhatsApp provides them
- dimensions, caption, duration, and audio `ptt` state are carried in `attachments[].extras`
- attachment bytes are not downloaded

If an inbound message carries only media and no text or caption, the plugin emits a fallback body like `(1 attachment(s))` so the event is still visible to the agent.

## Outbound attachments

`deliver` and `push` support one optional inline attachment per message. Only `data_base64` attachments are supported.

- `url` attachments are rejected
- `storage_key` attachments are rejected
- image, video, audio, and document payloads are inferred from `mime_type`
- document attachments use the attachment `name` as the WhatsApp file name
- audio attachments cannot carry `message.content`; send the text as a separate message or send the file as a document attachment instead

Example:

```json
{
  "kind": "deliver",
  "config": {},
  "message": {
    "content": "here is the file",
    "metadata": {
      "conversation_id": "15551234567@s.whatsapp.net"
    },
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

Dispatch emits `status` frames to mark turn-taking. The plugin maps the subset that makes sense for WhatsApp chatstate updates:

| StatusKind | WhatsApp action |
| --- | --- |
| `processing`, `delivering`, `operation_started` | `composing` |
| `completed`, `cancelled`, `operation_finished` | `paused` |
| `info`, `approval_needed`, `auth_required`, others | accepted, no upstream traffic |

The status frame's `conversation_id` is required and is treated the same way as an outbound recipient JID.

## Known limitations

- Multiple outbound attachments in one Dispatch message are not supported.
- Group JIDs are intentionally out of scope for the first release.
- Inbound attachment bytes are not downloaded yet; only metadata is surfaced.
- Reactions, edits, and read receipts are ignored for now.

## Notes on security

- The SQLite store contains the linked WhatsApp session and should be treated like a credential.
- Restrict the store directory permissions, for example:

```bash
chmod 700 ~/.config/dispatch/channels/whatsapp/<account>/
```

- Removing the linked device from WhatsApp on the phone immediately revokes this plugin's access.

## License

MIT
