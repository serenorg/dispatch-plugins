# Changelog

All notable changes to the Dispatch Signal channel plugin are documented in this file.

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
