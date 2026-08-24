# Changelog

All notable changes to Dispatch Plugins are documented in this file.

## [0.3.0] - 2026-08-24

### Added

- Accepted Slack inbound messages now receive a fail-open `:eyes:` reaction when bot-token delivery is configured; Slack apps need the `reactions:write` scope to enable it.

### Changed

- Persistent ingress workers now reconnect after bounded transient receive failures and exit the plugin process after terminal or repeated failures so the host supervisor can restart an unhealthy channel.
- All channel plugins now target Dispatch channel protocol `v0.5.0` and populate its workspace, parent-conversation, activation, thread, direct-message sender, outbound destination, and reply-delivery fields when supported.
- Slack Socket Mode now distinguishes healthy receive timeouts from disconnects, fails fast on authentication and connection-response protocol errors, honors `Retry-After` guidance, and avoids exposing connection tickets in errors.
- Discord Gateway workers now emit empty liveness notifications after session readiness and heartbeat acknowledgements, and repeated disconnects become terminal instead of leaving a silent worker.
- **Breaking:** poll callbacks passed to `dispatch-channel-runtime` now receive an `IngressPollContext` third argument, which exposes shutdown-aware waiting and in-cycle event delivery. Plugins built against the crate must add the parameter; ignoring it with `_context` preserves the previous behavior.
- **Breaking:** Slack Socket Mode now requires supervised `start_ingress`; one-shot `poll_ingress` is rejected because its JSON-RPC response does not provide a post-handoff acknowledgement point.

### Fixed

- Slack Socket Mode now acknowledges an `events_api` envelope only after its notification has been serialized and flushed to plugin stdout. A failed stdout write leaves the envelope unacknowledged for Slack redelivery; durable host acceptance still requires a host-level acknowledgement contract.
- A Slack event redelivered to a still-running worker, because its acknowledgement never reached Slack, is recognized by `event_id` and written to plugin stdout once. Redelivery after a plugin restart still needs deduplication in the host.
- Slack rate-limit waits and Socket Mode reads now observe shutdown promptly instead of blocking for the full provider or polling timeout.
- Slack rate-limit recovery publishes liveness before and after the provider-directed wait so a healthy recovery does not appear stale to the host.
- Slack stdout delivery failures now terminate with a `notification_delivery` classification instead of being retried as transport failures.
- Preserve a completed poll result when shutdown races with the poll, while terminal poll results caused by shutdown no longer force a failure exit.
- Reset reconnect failure history only after a sustained successful receive window rather than after one short-lived success.
- Discord reconnect history is cleared after a sustained gateway session, so routine gateway-directed reconnects spread over a long-lived deployment no longer accumulate into a terminal process exit.
- Discord sessions torn down by shutdown are no longer counted as supervision failures and can no longer exit the process while the host awaits a shutdown response.
- Reconnect jitter mixes clock, process, and attempt inputs to reduce correlated retries.
- `Retry-After` values with surrounding whitespace or a fractional component are honored instead of silently falling back to a one-second retry.
- Twilio GET webhook signatures now use query parameters only through the signed request URL instead of appending them again as POST form parameters.

### Security

- Discord ingress now validates workspace, conversation, thread, direct-message sender, and activation scope before emitting events, and outbound delivery is restricted to configured destinations.
- Slack outbound delivery now fails closed unless the destination is explicitly allowed by channel policy or unrestricted access is configured.
- Slack Events API, Telegram webhook, Twilio webhook, and generic webhook ingress now fail closed when their configured verification secret or token is missing or empty.
- Generic webhook secrets use constant-time comparison, outbound destinations require absolute HTTP or HTTPS URLs, redirects are disabled, and configured credentials are not forwarded to dynamic destinations.

## [0.2.0] - 2026-05-05

### Added

- Discord Gateway websocket ingress mode for the Discord channel, including session resume, heartbeat handling, optional Message Content intent support, and automatic reconnects

### Changed

- Catalog entries now carry stable `id` values such as `seren.channel.discord`, and the catalog now declares a top-level `catalog_id`
- Catalog `install_hint` values now use `dispatch extension install <name>` instead of `dispatch channel install <path>`
- Pinned `dispatch-channel-protocol` to the `v0.4.0` tag from the `dispatch` repository

### Fixed

- Catalog loading now rejects invalid catalog identifiers, duplicate entry IDs, duplicate entry names, and entry IDs that do not belong to the declared catalog

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
