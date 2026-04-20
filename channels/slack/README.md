# channel-slack

A [Dispatch](https://github.com/serenorg/dispatch) channel plugin
for Slack.

## Scope

Implemented:

- `capabilities`
- `configure`
- `health`
- `poll_ingress`
- `start_ingress`
- `stop_ingress`
- `ingress_event`
- `deliver`
- `push`
- `status`
- `shutdown`

Behavior:

- outbound delivery can use `chat.postMessage` with a bot token
- outbound delivery can fall back to a Slack incoming webhook URL
- outbound attachments support one inline `data_base64` upload per message when
  using bot-token delivery
- health checks validate the bot token with `auth.test` when configured
- Events API ingress uses host-managed webhook callbacks
- Socket Mode ingress supports both one-shot `poll_ingress` fetches and
  background `start_ingress` sessions
- Socket Mode opens Slack's websocket via `apps.connections.open` and emits
  normalized inbound events back to Dispatch
- challenge and acknowledgement replies are returned through `callback_reply`
- status frames render visible status messages into Slack conversations

Not implemented:

- slash command registration and lifecycle
- Block Kit generation
- URL- or storage-key-backed attachment delivery
- message edits

## Availability

Inbound:

- Events API webhook mode is available when `webhook_public_url` is configured
  and `SLACK_SIGNING_SECRET` is available
- Socket Mode is available when `SLACK_APP_TOKEN` is configured and
  `webhook_public_url` is not set

Outbound:

- bot-token delivery uses `SLACK_BOT_TOKEN` and `chat.postMessage`
- incoming-webhook delivery uses `SLACK_INCOMING_WEBHOOK_URL`
- `default_channel_id` and `default_thread_ts` provide routing defaults

## Configuration

Optional environment:

- `SLACK_BOT_TOKEN` - used for `auth.test` and `chat.postMessage`
- `SLACK_APP_TOKEN` - app-level token used for Socket Mode via
  `apps.connections.open`
- `SLACK_SIGNING_SECRET` - used to verify inbound Slack event signatures
- `SLACK_INCOMING_WEBHOOK_URL` - used for low-friction outbound delivery

Useful config fields:

- `bot_token_env`, `app_token_env`, `signing_secret_env`,
  `incoming_webhook_url_env` - override the default env var names
- `incoming_webhook_url` - inline incoming webhook URL when env vars are not
  desirable
- `webhook_public_url` - public base URL used for Events API ingress
- `webhook_path` - ingress route path, default `/slack/events`
- `default_channel_id` - default target channel for bot-token delivery
- `default_thread_ts` - optional default thread timestamp
- `poll_timeout_secs` - Socket Mode receive timeout in seconds, minimum 1
- `allowed_team_ids`, `allowed_sender_ids`, `owner_id`, and `dm_policy` -
  optional policy controls surfaced through `configure`

`SLACK_APP_TOKEN` is only for Socket Mode ingress. It does not replace the bot
token for `health`, `deliver`, `push`, or `status`.

## Setup

Slack supports two ingress paths and two outbound auth paths in this plugin.

Bot-token mode:

1. Create a Slack app in the Slack API dashboard.
2. Enable a bot user for the app.
3. Install the app into the target workspace.
4. Copy the bot token and export it as `SLACK_BOT_TOKEN`.
5. If you are using Events API ingress, copy the signing secret and export it
    as `SLACK_SIGNING_SECRET`.

Recommended bot scopes:

- `chat:write`
- `files:write`

Recommended event subscriptions:

- `app_mention`
- `message.channels`
- `message.groups`
- `message.im`
- `message.mpim`

Socket Mode setup:

1. Open the app at `https://api.slack.com/apps`.
2. In `Socket Mode`, enable Socket Mode for the app.
3. In `Basic Information`, create an app-level token.
4. Give that token the `connections:write` scope.
5. Export the generated `xapp-...` token as `SLACK_APP_TOKEN`.
6. Reinstall the app if Slack asks for it after scope or event changes.

Events API setup:

1. In `Event Subscriptions`, enable events.
2. Set the request URL to the public endpoint reported by Dispatch, for
    example `https://example.com/slack/events`.
3. Subscribe to the bot events you want to receive.
4. Export the signing secret as `SLACK_SIGNING_SECRET`.

Incoming-webhook mode:

1. Enable incoming webhooks on the Slack app.
2. Install the app into the target workspace.
3. Create an incoming webhook for the target channel.
4. Export the webhook URL as `SLACK_INCOMING_WEBHOOK_URL`.

Minimal Socket Mode config:

```toml
default_channel_id = "C1234567890"
poll_timeout_secs = 60
```

Minimal Events API config:

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

## Dispatch usage

Dispatch operators normally use the host CLI:

```bash
dispatch channel call channel-slack \
  --request-json '{"kind":"health","config":{"bot_token_env":"SLACK_BOT_TOKEN","default_channel_id":"C1234567890"}}'

dispatch channel listen channel-slack \
  --listen 127.0.0.1:8787 \
  --config-file ./slack-events.toml

dispatch channel poll channel-slack \
  --config-file ./slack-socket-mode.toml --once

dispatch channel call channel-slack \
  --request-json '{"kind":"push","config":{"bot_token_env":"SLACK_BOT_TOKEN","default_channel_id":"C1234567890"},"message":{"content":"Dispatch Slack test"}}'
```

The plugin transport is JSON-RPC 2.0 over JSONL on stdio. Events API and
Socket Mode are upstream Slack transport choices that the plugin translates
into the shared Dispatch channel protocol.

## Notes on ingress

- if `SLACK_APP_TOKEN` is configured and `webhook_public_url` is not set,
  `poll_ingress` performs one Socket Mode receive cycle and
  `start_ingress` chooses Socket Mode background-session behavior
- otherwise `start_ingress` chooses Events API webhook mode and reports the
  public route that the host should expose
- Socket Mode keeps a background worker alive, acknowledges each `envelope_id`,
  and emits normalized inbound notifications to Dispatch

Common failure modes:

- `polling_not_supported` means `SLACK_APP_TOKEN` was not available to the
  plugin process
- `failed to connect Slack socket mode websocket` usually means the app-level
  token is invalid or the binary was built without websocket TLS support
- repeated self-replies usually mean the app is receiving its own bot messages;
  the plugin filters bot-authored events, so verify the running binary is up to
  date if this appears again

## License

MIT
