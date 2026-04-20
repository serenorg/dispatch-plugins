# channel-gmail

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for Gmail.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `poll_ingress`
- `start_ingress`
- `stop_ingress`
- `deliver`
- `push`
- `status`
- `shutdown`

Behavior:

- inbound email supports both one-shot `poll_ingress` fetches and background
  `start_ingress` sessions over Gmail IMAP
- outbound mail is sent through Gmail SMTP
- replies route back to the sender or `Reply-To` address from the inbound mail
- inbound subjects are surfaced as `thread_id` so Dispatch replies can preserve
  the thread subject without host changes
- inbound attachment metadata is surfaced on the normalized event
- inbound message metadata includes `body_source = text | html | subject` so
  downstream consumers can tell whether the message body came from a real
  text/plain part, HTML fallback, or subject-only fallback
- outbound attachments require `data_base64`
- outbound deliveries accept per-message `cc` / `bcc` metadata (comma-separated
  address lists), with optional `default_cc` / `default_bcc` fallbacks on the
  channel config
- inbound messages with only an HTML body are converted to plaintext with a
  lightweight HTML stripper
- mailing-list and auto-responder traffic is suppressed at ingress via the
  `Auto-Submitted`, `Precedence`, and `List-Unsubscribe` headers so agents do
  not reply to bounces, digests, or bulk mail

Not implemented:

- Gmail API watch / PubSub receive
- Gmail search-query semantics
- Gmail labels as first-class Dispatch concepts
- HTML-first outbound rendering
- status frames

## Availability

Inbound:

- one-shot polling and background ingress are available anywhere the plugin can
  reach Gmail IMAP
- Dispatch keeps the plugin subprocess alive while polling is active, and the
  plugin emits `channel.event` notifications as new mail arrives

Outbound:

- replies and proactive pushes use Gmail SMTP
- `default_recipient` and `default_subject` provide routing defaults for
  operator-triggered sends

## Configuration

This plugin is intentionally Gmail-specific, but it still uses the standard
IMAP/SMTP transport rather than the Gmail REST API.

Required:

- `imap_username` - usually your Gmail address
- `smtp_from_email` if you want outbound delivery without relying on the
  username fallback
- `GMAIL_APP_PASSWORD` or the env var named by `imap_password_env` /
  `smtp_password_env`

Defaults:

- `imap_host = "imap.gmail.com"`
- `imap_port = 993`
- `smtp_host = "smtp.gmail.com"`
- `smtp_port = 465`
- `imap_password_env = "GMAIL_APP_PASSWORD"`
- `smtp_password_env = "GMAIL_APP_PASSWORD"`
- `imap_mailbox = "INBOX"`

Useful config fields:

- `smtp_username` - falls back to `imap_username`
- `smtp_from_name` - optional display name for the From header
- `default_recipient` - fallback destination for manual pushes
- `default_cc` - comma-separated fallback Cc list for manual pushes
- `default_bcc` - comma-separated fallback Bcc list for manual pushes
- `default_subject` - fallback subject for manual pushes
- `poll_interval_secs` - IMAP polling interval, clamped to at least 5 seconds

Minimal config:

```toml
imap_username = "you@gmail.com"
smtp_from_email = "you@gmail.com"
poll_interval_secs = 60
```

Environment example:

```bash
export GMAIL_APP_PASSWORD="replace-me"
```

## Setup

1. Enable 2-Step Verification on the Google account.
2. Create an app password for Mail in the Google account security settings.
3. Export that app password as `GMAIL_APP_PASSWORD`.
4. Configure `imap_username` with the Gmail address to watch.
5. Optionally set `smtp_from_email` and `smtp_from_name` for clearer outbound
    identity.
6. Start polling through Dispatch.

Operational notes:

- `poll_ingress` performs one IMAP fetch cycle and returns updated cursor state
- `start_ingress` records the current IMAP UID cursor and does not replay the
  full historical mailbox by default
- restored Dispatch polling state resumes from the prior IMAP UID
- if Gmail rotates the mailbox UID validity, the plugin resets its cursor to
  the current mailbox head instead of replaying stale history

## Dispatch usage

```bash
dispatch channel call channel-gmail \
  --request-json '{"kind":"health","config":{"imap_username":"you@gmail.com","smtp_from_email":"you@gmail.com"}}'

dispatch channel poll channel-gmail \
  --config-file ./gmail.toml --once

dispatch channel call channel-gmail \
  --request-json '{"kind":"push","config":{"imap_username":"you@gmail.com","smtp_from_email":"you@gmail.com","default_recipient":"user@example.com","default_subject":"Dispatch Gmail test"},"message":{"content":"Dispatch Gmail test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. The plugin translates
Gmail's IMAP/SMTP transport into the shared Dispatch channel protocol.

## Notes

- This plugin is for Gmail accounts that should behave like an email channel in
  Dispatch.
- If you need Gmail-native APIs such as watch / PubSub, query syntax, or label
  operations, that should be a separate provider-native plugin rather than
  overloading this one.

## License

MIT
