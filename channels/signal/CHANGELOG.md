# Changelog

All notable changes to the Dispatch Signal channel plugin are documented in this file.

## [0.2.0] - 2026-08-24

### Added

- The receive worker emits an empty `channel.event` notification when its stream opens and every 20 seconds thereafter. These notifications contain no message content and let the host distinguish a quiet Signal channel from a dead receive worker.
- The README now documents the best-effort inbound delivery guarantee and the missing durability, acknowledgement, and deduplication mechanisms.

### Changed

- The plugin now uses Dispatch channel protocol `v0.5.0`, reports `polling` and `websocket` ingress modes, and populates the typed direct-message authorization and activation fields.

### Fixed

- Persistent ingress writes `channel.event` notifications without waiting for another JSON-RPC request, so inbound messages reach a host that only listens after starting ingress.
- The receive worker reconnects with bounded exponential backoff and per-attempt jitter instead of ending on the first failure.
- After five consecutive failures without a sustained receive stream, the plugin exits nonzero so the host can restart the channel. Only an open stream that stays up for one minute clears the failure history.
- A receive stream that closes on its own is treated as a lost connection. Only a stop request ends the worker healthily.
- A worker that does not stop within three seconds exits the plugin, which prevents a blocked provider call from stopping process replacement.
- The `poll_timeout_secs` documentation now correctly states that zero uses the default timeout.

### Security

- Signal direct-message authorization now fails closed. Persistent ingress requires an explicit sender allowlist or `dm_policy: open`; outbound delivery and typing indicators use the same effective recipient scope.
- Store encryption configuration now rejects empty passphrase environment-variable names and empty passphrase values.

## [0.1.0] - 2026-04-24

Initial release.

### Added

- Native Rust Signal channel plugin for Dispatch, built on the [`presage`](https://github.com/whisperfish/presage) client. No `signal-cli`, no Docker, and no external daemon -- a single binary owns its own Signal session on a local SQLite store and talks directly to Signal's servers over WebSocket.
- Built-in `--link` subcommand for pairing the plugin as a secondary Signal device via QR code, with support for multiple logical accounts on the same host (`--account`) and an optional SQLCipher passphrase (`--passphrase-env`) for store encryption at rest. The generated provisioning URL automatically advertises the `backup5` capability required by current Signal linking expectations.
- `capabilities`, `configure`, `health`, and `shutdown` channel operations, including surfaced Signal session state metadata (ACI, PNI, phone number, device id, store path).
- `poll_ingress` one-shot drain that opens a single WebSocket to Signal, collects all queued messages until `QueueEmpty`, and closes -- suited to CLI-driven `dispatch channel poll --once` and short-lived hosts.
- `start_ingress` / `stop_ingress` persistent background receive worker that runs on a dedicated OS thread with its own tokio current-thread runtime, bridging inbound Signal messages back to the host as `channel.event` notifications between JSON-RPC requests.
- `deliver` / `push` outbound message paths supporting text and inline (`data_base64`) attachments to ACI- or PNI-addressed direct conversations.
- `status` frames mapped to Signal typing indicators (`processing`/`delivering`/`operation_started` -> typing started, `completed`/`cancelled`/`operation_finished` -> typing stopped, other kinds accepted without upstream traffic).
- Inbound event envelope surfaces sender ACI as both `conversation.id` and `actor.id`, with per-attachment metadata (name, MIME type, size) when a message carries attachments.

### Notes

- Because this plugin links against `presage`, the combined binary is distributed under the AGPL-3.0 license.
- SQLCipher support is built via `libsqlite3-sys` with vendored OpenSSL, so builds do not rely on host OpenSSL headers or libraries, and `passphrase_env` is available across supported targets.
- Inbound attachment byte download, group messaging, username-based recipients, and reactions/edits/read receipts as distinct events are not yet implemented and are tracked for a future release.
- `channel-signal` imports `dispatch-channel-protocol` directly from the `dispatch` repository. That wire protocol should not yet be treated as a stable long-term Dispatch core compatibility contract.
