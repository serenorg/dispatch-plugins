# channel-imessage

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for iMessage
through [BlueBubbles](https://bluebubbles.app/install).

## Setup

1. Run BlueBubbles on the owner's MacBook Pro, signed into Messages with
    Camilla's Apple ID. Enable the BlueBubbles web API and set its password.
2. Bind `BLUEBUBBLES_URL` to the server's reachable HTTP(S) base URL and
    `BLUEBUBBLES_PASSWORD` to its API password. The URL must have no credentials,
    query string, or fragment. The gateway injects these bound secrets into the
    plugin's environment.
3. Register a BlueBubbles webhook for `new-message`. **The webhook URL must end
    with `?password=<password>`**, using the same BlueBubbles password. Percent-encode
    the password as a query value (including `+`, `&`, `=`, and spaces).
    With Seren Core, use the assigned route:
    `https://api.serendb.com/webhooks/<route_id>?password=<password>`.
    With the stand-alone Dispatch host, use
    `https://your-host.example/imessage?password=<password>`.
4. Text Camilla's Apple ID from a customer's verified phone number or Apple ID
    email. Core normalizes the sender and routes the message to that customer's
    hire. Replies reuse the inbound chat GUID.

The webhook password is checked by this plugin in constant time. Missing or
incorrect passwords, including a missing configured password, return HTTP 401.
Only a host-provided `trust_verified: true` bypasses this check. There is no
localhost exemption or header-secret authentication. Keep webhook URLs private
and redact their query strings in proxy/access logs.

## Configuration

| Field | Default | Purpose |
| --- | --- | --- |
| `bluebubbles_url_env` | `BLUEBUBBLES_URL` | Name of the env var containing the API base URL |
| `bluebubbles_password_env` | `BLUEBUBBLES_PASSWORD` | Name of the env var containing the password |
| `webhook_public_url` | Unset | Optional public base URL for stand-alone Dispatch; `start_ingress` reports it with `/imessage` appended |

Use `{}` for the default config. Set env variable names in config, never secret
values. `webhook_public_url` must not contain the password or a query string;
append the password only when registering the URL in BlueBubbles.

`allowed_paths: ["/imessage"]` is for the stand-alone host. Core forwards its
`/webhooks/<route_id>` path and query unchanged, and the plugin accepts that path.
`start_ingress` validates credentials via ping and reports configuration; it does
not register a webhook remotely. `stop_ingress` reports stopped without changing
the BlueBubbles server.

## Behavior

- JSON-RPC 2.0 over JSONL on stdio, using `dispatch-channel-protocol` v0.7.0.
- Handles `capabilities`, `configure`, `health`, `start_ingress`, `stop_ingress`,
  `ingress_event`, `deliver`, `push`, and `shutdown`.
- Ingress accepts `new-message` with `isFromMe: false` and a DM GUID (`;-;`).
  Self messages, group chats (`;+;`), updates, missing routing fields, and
  conflicting chat coordinates are acknowledged with HTTP 200 and no event.
- Actor is `handle.address` unchanged; conversation is `chats[0].guid` or
  `chatGuid`; event/message IDs are the provider's `guid`. Text and attachment
  GUID, MIME type, name, and size are forwarded. Attachments are never downloaded.
- Delivery requires `metadata["conversation_id"]` containing the existing DM
  chat GUID. Missing, bare-handle, or group destinations fail with
  `imessage_dm_required`. The plugin never constructs a GUID or creates a chat.
- Text sends use `POST /api/v1/message/text?password=...` and exactly
  `{chatGuid, tempGuid, message}` with a UUID v4 `tempGuid`. There is no `method`
  field, private API, effect, or quoted-message threading. Receipts use `data.guid`.
- Health uses `GET /api/v1/ping?password=...`. Requests have a 20-second timeout
  and do not follow redirects. HTTP and decoding failures use fixed error codes
  that contain no URLs, passwords, handles, or message content.
- Polling, message read-back, permalinks, status frames, and outbound attachments
  are unsupported. Status returns `accepted: false`; attachments fail with
  `imessage_outbound_attachments_unsupported`.

Capabilities use `chat_or_thread` for one conversation per provider chat GUID
(no `thread_id`), `attachment_support: true` for inbound metadata only, and no
outbound `attachment_sources`. `reply_verification_support` is false because
quoted-reply verification is absent. `webhook_secret_support` is false because
BlueBubbles supplies no host-managed header secret; query authentication remains
mandatory inside the plugin. Config is account-scoped to one BlueBubbles server.
Wildcard network domains allow an operator-chosen host.

## Build and test

From the workspace root:

```bash
cargo build -p channel-imessage --release --locked
cargo test -p channel-imessage --locked
```

The manifest stays at workspace version 0.6.0 until the maintainer releases
0.7.0 and adds the catalog entry. A real BlueBubbles server and production shared
line routing still need production check 11 after deployment.

## License

MIT
