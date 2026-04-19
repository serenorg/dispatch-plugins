# channel-outlook

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for Outlook
and Microsoft 365 mailboxes exposed over IMAP and SMTP.

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

- inbound email is fetched from Outlook / Microsoft 365 over IMAP through a
  background polling worker
- outbound mail is sent through Microsoft SMTP
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

- Microsoft Graph mail APIs
- Graph change subscriptions / webhook receive
- mailbox-folder management beyond the configured IMAP mailbox
- HTML-first outbound rendering
- status frames

## Availability

Inbound:

- background ingress is available anywhere the plugin can reach Outlook /
  Microsoft 365 IMAP
- Dispatch keeps the plugin subprocess alive while polling is active, and the
  plugin emits `channel.event` notifications as new mail arrives

Outbound:

- replies and proactive pushes use Outlook / Microsoft 365 SMTP
- `default_recipient` and `default_subject` provide routing defaults for
  operator-triggered sends

## Configuration

This plugin is Microsoft-specific, but it still uses the standard IMAP/SMTP
transport rather than Microsoft Graph.

Required:

- `imap_username` - usually the Outlook / Microsoft 365 mailbox address
- `smtp_from_email` if you want outbound delivery without relying on the
  username fallback
- `OUTLOOK_EMAIL_PASSWORD` or the env var named by `imap_password_env` /
  `smtp_password_env`

Defaults:

- `imap_host = "outlook.office365.com"`
- `imap_port = 993`
- `smtp_host = "smtp.office365.com"`
- `smtp_port = 587`
- `imap_password_env = "OUTLOOK_EMAIL_PASSWORD"`
- `smtp_password_env = "OUTLOOK_EMAIL_PASSWORD"`
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
imap_username = "you@outlook.com"
smtp_from_email = "you@outlook.com"
poll_interval_secs = 60
```

Environment example:

```bash
export OUTLOOK_EMAIL_PASSWORD="replace-me"
```

## Setup

1. Enable IMAP for the mailbox if the tenant or consumer account requires it.
2. Create an app password if the account uses MFA and the tenant still permits
    IMAP/SMTP app passwords; otherwise use the mailbox password only if the
    account policy allows legacy IMAP/SMTP auth.
3. Export the credential as `OUTLOOK_EMAIL_PASSWORD`.
4. Configure `imap_username` with the mailbox address to watch.
5. Optionally set `smtp_from_email` and `smtp_from_name` for clearer outbound
    identity.
6. Start polling through Dispatch.

Operational notes:

- `start_ingress` records the current IMAP UID cursor and does not replay the
  full historical mailbox by default
- restored Dispatch polling state resumes from the prior IMAP UID
- if the mailbox UID validity changes, the plugin resets its cursor to the
  current mailbox head instead of replaying stale history

## Dispatch usage

```bash
dispatch channel call channel-outlook \
  --request-json '{"kind":"health","config":{"imap_username":"you@outlook.com","smtp_from_email":"you@outlook.com"}}'

dispatch channel poll channel-outlook \
  --config-file ./outlook.toml --once

dispatch channel call channel-outlook \
  --request-json '{"kind":"push","config":{"imap_username":"you@outlook.com","smtp_from_email":"you@outlook.com","default_recipient":"user@example.com","default_subject":"Dispatch Microsoft email test"},"message":{"content":"Dispatch Microsoft email test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. The plugin translates
Outlook / Microsoft 365 IMAP/SMTP transport into the shared Dispatch channel
protocol.

## Notes

- This plugin is for Outlook.com, Hotmail, and Microsoft 365 mailboxes that
  should behave like an email channel in Dispatch.
- If you need Microsoft Graph-specific mail features, that should be a
  separate provider-native plugin rather than overloading this one.

## License

MIT
