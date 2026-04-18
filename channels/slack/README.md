# channel-slack

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Slack.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `start_ingress`
- `stop_ingress`
- `poll_ingress`
- `ingress_event`
- `deliver`
- `push`
- `status`

Behavior:

- outbound delivery can use `chat.postMessage` with a bot token
- outbound delivery can also fall back to a Slack incoming webhook URL
- outbound attachments support one inline `data_base64` upload per message when
  using bot-token delivery
- health checks validate the bot token with `auth.test` when configured
- ingress verifies Slack request signatures and parses Events API payloads
- polling ingress can also receive Events API payloads over Slack Socket Mode
- challenge and acknowledgement replies are returned through `callback_reply`
- status frames render visible status messages into Slack conversations

Not implemented:

- slash command registration and lifecycle
- block kit generation
- URL- or storage-key-backed attachment delivery
- message edits

## Build

```bash
cargo build --release
```

## Configuration

Optional env vars:

- `SLACK_BOT_TOKEN` - used for `auth.test` and `chat.postMessage`
- `SLACK_APP_TOKEN` - used for Slack Socket Mode polling via `apps.connections.open`
- `SLACK_SIGNING_SECRET` - used by the host to verify incoming Slack event
  signatures
- `SLACK_INCOMING_WEBHOOK_URL` - used for low-friction outbound delivery

Useful config fields:

- `default_channel_id` - default target channel for bot-token delivery
- `default_thread_ts` - optional default thread timestamp
- `webhook_public_url` - public base URL used for ingress declaration
- `webhook_path` - ingress route path, default `/slack/events`
- `poll_timeout_secs` - socket-mode wait timeout in seconds, minimum 1

`SLACK_APP_TOKEN` is only for Socket Mode ingress. It does not replace the bot
token for `health`, `deliver`, `push`, or `status`.

## Setup

Slack supports two ingress paths and two outbound auth paths in this plugin.

Ingress paths:

1. Events API webhook mode:
  - configure `webhook_public_url`
  - export `SLACK_SIGNING_SECRET`
2. Socket Mode polling:
  - enable Socket Mode in the Slack app settings
  - create an app-level token with the `connections:write` scope
  - export it as `SLACK_APP_TOKEN`
  - keep the bot token configured as well if you want outbound delivery and
    identity metadata

Bot-token mode:

1. Create a Slack app in the Slack API dashboard.
2. Enable a bot user for the app.
3. Install the app into the target workspace.
4. Copy the bot token and export it as `SLACK_BOT_TOKEN`.
5. If you are using Events API ingress, copy the signing secret and export it
  as `SLACK_SIGNING_SECRET`.

Recommended bot scopes for this plugin:

- `chat:write` for outbound delivery
- `files:write` for inline `data_base64` attachment uploads

Recommended event subscriptions:

- `app_mention` for mention-driven bots
- `message.channels` if you want channel message events
- `message.groups` if you want private-channel message events
- `message.im` if you want direct-message events
- `message.mpim` if you want multi-person direct-message events

Socket Mode setup in the Slack dashboard:

1. Open the app at `https://api.slack.com/apps`.
2. In `Socket Mode`, enable Socket Mode for the app.
3. In `Basic Information`, create an app-level token.
4. Give the app-level token the `connections:write` scope.
5. Copy the generated `xapp-...` token and export it as `SLACK_APP_TOKEN`.
6. Reinstall the app to the workspace if Slack asks for it after scope or
  event changes.

Events API webhook setup in the Slack dashboard:

1. In `Event Subscriptions`, enable events.
2. Set the request URL to the public endpoint reported by Dispatch, for
  example `https://example.com/slack/events`.
3. Subscribe to the bot events you want to receive.
4. Copy the signing secret from `Basic Information` and export it as
  `SLACK_SIGNING_SECRET`.

Incoming-webhook mode:

1. Create a Slack app and enable incoming webhooks.
2. Install the app into the target workspace.
3. Create an incoming webhook for the target channel.
4. Export the webhook URL as `SLACK_INCOMING_WEBHOOK_URL`.

Minimal Socket Mode config:

```toml
poll_timeout_secs = 60
default_channel_id = "C1234567890"
```

Minimal webhook config:

```toml
webhook_public_url = "https://example.com"
webhook_path = "/slack/events"
default_channel_id = "C1234567890"
```

Environment example:

```bash
export SLACK_BOT_TOKEN="xoxb-..."
export SLACK_APP_TOKEN="xapp-..."
export SLACK_SIGNING_SECRET="..."
```

Common failure modes:

- `polling_not_supported` means `SLACK_APP_TOKEN` was not available to the
  plugin process.
- `failed to connect Slack socket mode websocket` usually means the plugin was
  built without websocket TLS support or the app-level token is invalid.
- repeated self-replies usually mean the app is receiving its own bot messages;
  the plugin filters bot-authored events, so verify the running binary is up to
  date if this appears again.

## Protocol

Requests are sent as JSONL on stdin:

```json
{"protocol_version":1,"request":{"kind":"capabilities"}}
```

Delivery example using bot-token mode:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "deliver",
    "config": {
      "default_channel_id": "C1234567890"
    },
    "message": {
      "content": "Dispatch says hello from Slack."
    }
  }
}
```

Push example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "push",
    "config": {
      "default_channel_id": "C1234567890"
    },
    "message": {
      "content": "Dispatch scheduled update for Slack."
    }
  }
}
```

## Notes on ingress

For Events API webhooks, Slack does not provide a simple webhook registration
API equivalent to Telegram's `setWebhook`. This plugin therefore treats
`start_ingress` as a configuration handshake:

1. validate available auth material
2. validate the declared public route
3. report the endpoint and signing-secret requirement to the host

For Socket Mode, `poll_ingress` opens a websocket connection through
`apps.connections.open`, waits for the next event envelope, acknowledges it by
`envelope_id`, and returns the normalized Dispatch event to the host.

## License

MIT
