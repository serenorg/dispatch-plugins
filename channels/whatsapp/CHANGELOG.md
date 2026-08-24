# Changelog

All notable changes to the Dispatch WhatsApp channel plugin are documented in this file.

## [0.2.0] - 2026-08-24

### Added

- The receive worker emits an empty `channel.event` notification after WhatsApp is ready and every 20 seconds while the connection remains ready. These notifications contain no message content.
- The README now documents the best-effort inbound delivery guarantee and the missing durability, acknowledgement, and deduplication mechanisms.

### Changed

- The plugin now uses Dispatch channel protocol `v0.5.0`, reports `polling` and `websocket` ingress modes, and populates typed direct-message authorization and activation fields.

### Fixed

- Persistent ingress writes `channel.event` notifications without waiting for another JSON-RPC request, so inbound messages reach a host that only listens after starting ingress.
- The receive worker reconnects with bounded exponential backoff and per-attempt jitter instead of ending on the first failure.
- The worker waits for WhatsApp authentication and critical sync before its first heartbeat, stops heartbeats during disconnects, and allows the library 60 seconds to reconnect.
- After five consecutive failures without a sustained ready connection, the plugin exits nonzero so the host can restart the channel. Only a ready connection that stays up for one minute clears the failure history.
- A receive session that ends on its own is treated as a lost connection. Only a stop request ends the worker healthily.
- A `stop_ingress` request received before the first connection is now handled promptly instead of waiting for the full connection grace period.
- Slow receive workers are joined instead of detached, so a replaced worker cannot continue consuming messages after restart.

### Security

- WhatsApp direct-message authorization now fails closed for inbound events, outbound delivery, and typing-status frames. Persistent ingress requires an explicit sender allowlist or `dm_policy: open`, and group messages remain unsupported.

## [0.1.0] - 2026-04-24

Initial release.

### Added

- Native Rust WhatsApp channel plugin for Dispatch, built on the [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust) client. No Docker, no Meta Cloud API app, and no external daemon -- a single binary links a WhatsApp Web session via QR code and stores that session locally in SQLite.
- Built-in `--link` subcommand for pairing the plugin as a WhatsApp Web device, with support for multiple logical accounts on the same host (`--account`) and explicit SQLite store path selection (`--sqlite-store-path`).
- `capabilities`, `configure`, `health`, and `shutdown` channel operations, including surfaced linked-account metadata from the stored session.
- `poll_ingress` one-shot receive flow for CLI-driven `dispatch channel poll --once` and short-lived hosts.
- `start_ingress` / `stop_ingress` background receive worker that emits inbound WhatsApp messages back to the host as `channel.event` notifications between JSON-RPC requests.
- `deliver` / `push` outbound message paths supporting text and one optional inline (`data_base64`) attachment to direct-message JIDs.
- `status` frames mapped to WhatsApp typing indicators.
- Inbound event envelope surfaces per-attachment metadata (kind, MIME type, size, file name, and media extras) when a message carries attachments.

### Notes

- Inbound attachment byte download, multiple outbound attachments in one message, group messaging, reactions/edits/read receipts, and phone-number-to-JID resolution are not yet implemented and remain follow-up work for a future release.
- `channel-whatsapp` imports `dispatch-channel-protocol` directly from the `dispatch` repository. That wire protocol should not yet be treated as a stable long-term Dispatch core compatibility contract.
