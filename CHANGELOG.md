# Changelog

All notable changes to Dispatch Plugins are documented in this file.

## [0.1.0] - 2026-04-22

Initial release.

### Added

- Channel plugins for Discord, Slack, Telegram, Twilio SMS (with both API-key and account auth-token auth modes), generic webhook, generic email, Gmail, and Outlook
- First-class support for persistent ingress sessions and one-shot poll ingress across the shared channel runtime
- Shared email channel core and shared channel ingress runtime crates for cross-plugin behavior
- Email delivery features including cc/bcc support, HTML fallback handling, and auto-submitted suppression
- Shared manifest and catalog types in `channel-schema`, with the channel wire protocol sourced directly from `dispatch-channel-protocol` in the `dispatch` repository
- `dispatch-extension-catalog` helper for browsing and inspecting the local extension catalog
- GitHub Actions CI and release workflows for tagged builds
- Release binaries and checksums for supported target triples, with catalog entries that describe installable GitHub release assets

### Notes

- Channel plugins import `dispatch-channel-protocol` directly from the `dispatch` repository, but it should not yet be treated as a stable long-term Dispatch core compatibility contract.
