# channel-whatsapp

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for WhatsApp, using the native Rust [whatsapp-rust](https://github.com/jlucaso1/whatsapp-rust) client.

**No Docker. No Meta Business Account. No Cloud API app. No external daemon.** The plugin is a single Rust binary that links a WhatsApp Web session via QR code and stores that session locally in SQLite.

This directory is the source of truth for the first-party WhatsApp channel plugin. The `dispatch-plugins` workspace builds and releases it with the other first-party channels.

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
cargo build --release -p channel-whatsapp
```

The binary is written to `target/release/channel-whatsapp`.

## One-time setup: link as a WhatsApp Web device

Before the plugin can send or receive, it must be linked to a WhatsApp account. Run the built-in `--link` subcommand and scan the displayed QR code from the WhatsApp app on your phone (WhatsApp -> Settings -> Linked Devices -> Link a Device):

```bash
channel-whatsapp --link --device-name "Dispatch"
```

Flags:

```
    --json                     Stream pairing events as JSON lines on stdout.
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

For browser/gateway linking, run `channel-whatsapp --link --json`. Stdout contains one JSON object per line: `{"pairing":"<raw PairingQrCode.code>"}` for each QR code (codes rotate; use the newest), then the existing success summary with `"status":"linked"` and `"linked":true`, preserving `phone_jid`, `lid_jid`, `device_id`, `push_name`, `device_name`, and `sqlite_store_path`. When QR codes run out, the command prints `{"error":"pairing_expired"}` and exits nonzero, with or without `--json`. QR art and scan instructions stay on stderr. Each stdout line is flushed immediately.

## Configuration

| Field | Purpose |
| --- | --- |
| `self_chat_only` | `false` (default) preserves ordinary DM behavior. `true` accepts only the linked account's Message yourself chat from its other devices and restricts replies to that account. |
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

With `self_chat_only: true`, use a linked companion session and set `dm_policy` to `open` or a nonempty `allowlist`. Omitted and `deny` policies fail validation. Sender IDs, outbound recipients, and `default_recipient` must belong to the linked account (PN or LID). The host receives an account-only allowlist containing the available account JIDs without device suffixes, even with `dm_policy: "open"`. Messages from the phone, desktop, and web count; the plugin excludes groups, other chats, and its own companion device. Replies use the event's conversation JID without a device suffix.

For account-only linking, set `self_chat_only: true` and `dm_policy: "open"` with empty sender and outbound lists. The plugin derives the allowlist from the linked session.

Linking and self-chat sessions skip history synchronization. WhatsApp still sends account traffic to linked devices; the plugin filters other conversations before emitting host events and does not enable inbound message persistence. The store retains the device and cryptographic routing state needed to stay connected. Enabling self-chat mode does not erase data already present in an existing session store.

The intended operator flow is to receive an inbound event first, then reuse `conversation.id` as the outbound `message.metadata.conversation_id`.

## Dispatch usage

```bash
dispatch extension install channel-whatsapp

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
- `start_ingress` / `stop_ingress` run a background receive worker on a dedicated OS thread with its own current-thread tokio runtime. The worker submits inbound messages to the host as ID-bearing `channel.event` batches without waiting for another JSON-RPC request.

## Delivery guarantee

Inbound delivery is at-least-once while WhatsApp still retains a message. The plugin registers a `whatsapp-rust` inbound durability hook, so the library buffers each decrypted batch and withholds its provider acknowledgement until the hook returns. The hook maps the batch into `channel.event` batches of at most 32 events, journals each one in the host-supplied ingress storage, and returns only after the host has durably accepted every one. If any batch is not accepted, the hook fails the whole library batch, no message in it is acknowledged, and the worker reconnects because WhatsApp redelivers unacknowledged messages only on a new connection. A `poll_ingress` request refuses a library batch that would exceed the host's per-request acceptance page and leaves it for the next poll.

The library does not route every message through the hook. Recovered placeholder messages and newsletter messages are acknowledged on the library's own path before the plugin sees them, so the plugin delivers them durably only on a best-effort basis. The library also prunes its buffered copies after seven days and, when it cannot write its buffer, can degrade a message to at-most-once; see the `whatsapp-rust` durability hook documentation.

Each event's `event_id` is a domain-separated SHA-256 of the provider identity `(chat, sender, message id)`; `message.id` keeps the provider message ID. The library re-parses some metadata, such as the sender's push name, from a redelivered stanza, so events omit the push name (`actor.display_name` is absent) and carry only fields that a redelivery reproduces exactly. A redelivered message therefore maps to the same event. Consumers deduplicate by `event_id`.

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
