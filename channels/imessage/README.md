# channel-imessage

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin for iMessage through [BlueBubbles](https://bluebubbles.app/install).

## Setup

1. Run BlueBubbles on a Mac signed into Messages with the Apple ID that will receive messages. Enable the BlueBubbles web API and set its password.
2. Bind `BLUEBUBBLES_URL` to the server's reachable HTTPS base URL and `BLUEBUBBLES_PASSWORD` to its API password. HTTP is permitted only for a literal loopback address such as `http://127.0.0.1:1234` or `http://[::1]:1234` on the same host. Public and private-network addresses require HTTPS. The URL must have no credentials, query string, or fragment. The host supplies these values through the plugin's environment.
3. Register a BlueBubbles webhook for `new-message` at the host's assigned endpoint. Include `?password=<password>` in that URL using the same BlueBubbles password. Percent-encode the password as a query value, including `+`, `&`, `=`, and spaces. For a stand-alone Dispatch host, the URL is `https://your-host.example/imessage?password=<password>`.
4. Send a message to that Apple ID from another iMessage account. Replies reuse the inbound chat GUID.

The plugin checks the webhook password in constant time. Missing or incorrect passwords, including a missing configured password, return HTTP 401. Only a host-provided `trust_verified: true` bypasses this check. There is no localhost exemption or header-secret authentication. Keep webhook URLs private and redact their query strings in proxy/access logs.

## Configuration

| Field | Default | Purpose |
| --- | --- | --- |
| `bluebubbles_url_env` | `BLUEBUBBLES_URL` | Name of the env var containing the API base URL |
| `bluebubbles_password_env` | `BLUEBUBBLES_PASSWORD` | Name of the env var containing the password |
| `webhook_public_url` | Unset | Optional public base URL; `start_ingress` reports it with `/imessage` appended |

Use `{}` for the default config. Set environment variable names in config, never secret values. `webhook_public_url` follows the same HTTPS and literal-loopback rule as the API URL and must not contain the password or a query string; append the password only when registering the URL in BlueBubbles. A DNS name such as `localhost` is not treated as a verified loopback address. Loopback HTTP API requests bypass inherited proxies so they stay on the same host.

The manifest declares `allowed_paths: ["/imessage"]` for the stand-alone host. Hosts that forward callbacks from another assigned route must preserve the query string. `start_ingress` validates credentials via ping and reports configuration; it does not register a webhook remotely. `stop_ingress` reports stopped without changing the BlueBubbles server.

## Behavior

- JSON-RPC 2.0 over JSONL on stdio, using channel protocol version 2 from `dispatch-channel-protocol`.
- Handles `capabilities`, `configure`, `health`, `start_ingress`, `stop_ingress`, `ingress_event`, `deliver`, `push`, and `shutdown`.
- Ingress accepts `new-message` with `isFromMe: false` and a direct-message GUID of the form `iMessage;-;<handle>` or `any;-;<handle>`. Every reported coordinate in `chats[].guid` and `chatGuid` must name the same chat, and its handle must match `handle.address`. Email comparison ignores ASCII case; phone values must match exactly. An `any` GUID requires verification of the exact chat through the authenticated provider API, including when `handle.service` is absent. A supplied service that contradicts iMessage is rejected for either GUID form. Self messages, group chats, other services, updates, missing or inconsistent routing fields, and sender mismatches are acknowledged with HTTP 200 and no event.
- Actor is `handle.address` unchanged; conversation is the original provider chat GUID; event/message IDs are the provider's `guid`. Text and attachment GUID, MIME type, name, and size are forwarded. Attachments are never downloaded.
- Each `ingress_event` confirms a `channel.event` batch with the host, including an empty batch for an ignored or rejected callback, and returns the callback reply only after the host durably accepts it. If delivery fails, the request fails with `event_delivery_failed`; a batch that was already journaled stays pending and is replayed before the next one. Whether BlueBubbles retries a failed callback is provider behavior. Consumers deduplicate replays by `event_id`.
- Delivery requires `metadata["conversation_id"]` containing the existing `iMessage;-;<handle>` or `any;-;<handle>` chat GUID. Missing, bare-handle, group, explicit other-service, or invalid destinations fail with `imessage_dm_required` before credential lookup or network access. The plugin never constructs or rewrites a GUID or creates a chat.
- Ingress and delivery verify `any` chats through the authenticated provider API. A valid empty result rejects the route; HTTP, decoding, or inconsistent-response failures return a plugin error. The plugin uses the query result for routing and does not forward or log it. Caller metadata cannot assert the transport.
- Text sends use `POST /api/v1/message/text?password=...` with `{chatGuid, tempGuid, message, method: "apple-script"}` and a UUID v4 `tempGuid`. Sends do not use the private API, effects, or quoted-message threading. Receipts use `data.guid`.
- Health uses `GET /api/v1/ping?password=...` and requires the provider's HTTP 200 JSON response with `status: 200` and `data: "pong"`. Responses are limited to 1 MiB. Requests have a 20-second timeout and do not follow redirects; outbound verification and sending share one 20-second deadline. HTTP and decoding failures use fixed error codes that contain no URLs, passwords, handles, or message content.
- Polling, message read-back, permalinks, status frames, and outbound attachments are unsupported. Status returns `accepted: false`; attachments fail with `imessage_outbound_attachments_unsupported` before credential lookup or network access.

Capabilities use `chat_or_thread` for one conversation per provider chat GUID with no `thread_id`. `attachment_support: true` describes inbound metadata; the plugin rejects all outbound attachments. The empty `attachment_sources` list must not be interpreted as declaring that the host will reject attachments: Dispatch 0.7 defaults an empty list to inline data. `reply_verification_support` is false because quoted-reply verification is absent. `webhook_secret_support` is false because BlueBubbles supplies no host-managed header secret; query authentication remains mandatory inside the plugin unless the host has verified trust. Config is account-scoped to one BlueBubbles server. Wildcard network domains allow an operator-chosen host.

Chat verification uses `POST /api/v1/message/query?password=...` with the original `chatGuid`, fixed parameterized predicates for `chat.service_name = iMessage` and `chat.style = 45` (direct message), and a limit of one record. The response must contain the exact chat GUID and style. BlueBubbles Server 1.9.9 supports this contract in its [message router](https://github.com/BlueBubblesApp/bluebubbles-server/blob/v1.9.9/packages/server/src/server/api/http/api/v1/routers/messageRouter.ts) and [repository query](https://github.com/BlueBubblesApp/bluebubbles-server/blob/v1.9.9/packages/server/src/server/databases/imessage/index.ts).

## Build and test

From the workspace root:

```bash
cargo build -p channel-imessage --release --locked
cargo test -p channel-imessage --locked
```

Before enabling the connection, verify webhook authentication, incoming messages, and replies against the installed BlueBubbles server, Mac Messages, and host routing. API and unit-fixture checks do not establish phone delivery or host customer routing.

On macOS 26 (Tahoe), `any` is a transport-neutral GUID prefix used by iMessage, SMS, and RCS. BlueBubbles' AppleScript address fallback can fail when it uses `any` as a service type; see [the provider issue](https://github.com/BlueBubblesApp/bluebubbles-server/issues/777). The plugin preserves the exact GUID and does not work around that error by selecting a different transport.

## License

MIT
