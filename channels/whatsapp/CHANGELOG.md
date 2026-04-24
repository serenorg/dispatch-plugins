# Changelog

All notable changes to the Dispatch WhatsApp channel plugin are documented in this file.

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
