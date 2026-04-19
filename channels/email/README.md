# channel-email

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for email.

Provider-specific presets also exist in this repo:

- `channel-gmail` - Gmail-flavoured IMAP/SMTP defaults and setup guidance
- `channel-outlook` - Outlook / Microsoft 365 IMAP/SMTP defaults and
  setup guidance

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `start_ingress`
- `stop_ingress`
- `deliver`
- `push`
- `status`
- `shutdown`

Behavior:

- inbound email is fetched from IMAP through a background polling worker
- outbound email is sent through SMTP
- replies route back to the sender or `Reply-To` address from the inbound mail
- inbound subjects are surfaced as `thread_id` so Dispatch replies can preserve
  the thread subject without host changes
- inbound attachment metadata is surfaced on the normalized event
- outbound attachments require `data_base64`

Not implemented:

- POP3 ingress
- IMAP IDLE / server-push receive
- HTML-first outbound rendering
- status frames

## Why IMAP, not POP

POP is not a good fit for a channel-style integration. Dispatch needs mailbox
state, threading metadata, and repeatable incremental reads. IMAP provides
that; POP does not.

## Availability

Inbound:

- background ingress is available anywhere the plugin can reach the IMAP server
- Dispatch keeps the plugin subprocess alive while polling is active, and the
  plugin emits `channel.event` notifications as new mail arrives

Outbound:

- replies and proactive pushes use SMTP
- `default_recipient` and `default_subject` provide routing defaults for
  operator-triggered sends

## Configuration

Required for IMAP ingress:

- `imap_host`
- `imap_username`
- `EMAIL_IMAP_PASSWORD` or the env var named by `imap_password_env`

Required for SMTP delivery:

- `smtp_host`
- `smtp_from_email`
- `EMAIL_SMTP_PASSWORD` or the env var named by `smtp_password_env`
- `smtp_username` if it differs from `imap_username`

Useful config fields:

- `imap_port` - defaults to `993`
- `imap_mailbox` - defaults to `INBOX`
- `imap_password_env` - defaults to `EMAIL_IMAP_PASSWORD`
- `smtp_port` - defaults to `587`
- `smtp_username` - falls back to `imap_username`
- `smtp_password_env` - defaults to `EMAIL_SMTP_PASSWORD`, then falls back to
  `imap_password_env`
- `smtp_from_email` - outbound sender address; falls back to `smtp_username`
- `smtp_from_name` - optional display name for the From header
- `default_recipient` - fallback destination for manual pushes
- `default_subject` - fallback subject for manual pushes
- `poll_interval_secs` - IMAP polling interval, clamped to at least 5 seconds

Provider presets:

- Gmail defaults: `imap.gmail.com:993`, `smtp.gmail.com:465`
- Microsoft / Outlook defaults: `outlook.office365.com:993`,
  `smtp.office365.com:587`

Minimal config:

```toml
imap_host = "imap.example.com"
imap_username = "dispatch@example.com"
imap_mailbox = "INBOX"

smtp_host = "smtp.example.com"
smtp_username = "dispatch@example.com"
smtp_from_email = "dispatch@example.com"

poll_interval_secs = 60
```

Environment example:

```bash
export EMAIL_IMAP_PASSWORD="replace-me"
export EMAIL_SMTP_PASSWORD="replace-me"
```

## Setup

1. Create or choose the mailbox Dispatch should watch.
2. Enable IMAP access for that mailbox with an app password or provider-issued
    SMTP/IMAP credential.
3. Export the IMAP password into `EMAIL_IMAP_PASSWORD` or another env var named
    by `imap_password_env`.
4. Export the SMTP password into `EMAIL_SMTP_PASSWORD` or another env var named
    by `smtp_password_env`.
5. Set the IMAP and SMTP hostnames in the plugin config.
6. Start polling through Dispatch.

Examples:

- Gmail usually uses an app password with `imap.gmail.com` and
  `smtp.gmail.com`
- Outlook.com / Microsoft 365 usually uses `outlook.office365.com` and
  `smtp.office365.com`

Operational notes:

- `start_ingress` records the current IMAP UID cursor and does not replay the
  full historical mailbox by default
- restored Dispatch polling state resumes from the prior IMAP UID
- if the mailbox UID validity changes, the plugin resets its cursor to the
  current mailbox head instead of replaying stale history

## Dispatch usage

```bash
dispatch channel call channel-email \
  --request-json '{"kind":"health","config":{"imap_host":"imap.example.com","imap_username":"dispatch@example.com","smtp_host":"smtp.example.com","smtp_username":"dispatch@example.com","smtp_from_email":"dispatch@example.com"}}'

dispatch channel poll channel-email \
  --config-file ./email.toml --once

dispatch channel call channel-email \
  --request-json '{"kind":"push","config":{"smtp_host":"smtp.example.com","smtp_username":"dispatch@example.com","smtp_from_email":"dispatch@example.com","default_recipient":"user@example.com","default_subject":"Dispatch test"},"message":{"content":"Dispatch email test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. For the full wire
format, see Dispatch's `docs/extensions.md`.

## Notes on routing

- inbound `conversation_id` is the preferred reply destination: `Reply-To`
  when present, otherwise `From`
- inbound `thread_id` is the message subject
- inbound `message.id` is the RFC5322 `Message-ID` when present
- Dispatch-generated replies preserve `conversation_id`, `thread_id`, and
  `reply_to_message_id`, which lets the plugin rebuild a threaded SMTP reply

## License

MIT
