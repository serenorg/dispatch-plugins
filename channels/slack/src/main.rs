use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use dispatch_channel_runtime::{
    IngressWorker, no_after_cycle, restart_ingress_worker as restart_runtime_ingress_worker,
    stop_ingress_worker, write_stdout_line,
};
use hmac::{Hmac, Mac};
use jiff::Timestamp;
use serde::Deserialize;
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    io::{self, BufRead},
    sync::{Arc, Mutex},
};

mod protocol;
mod slack_api;

use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ChannelPolicy, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressCallbackReply, IngressMode, IngressPayload,
    IngressState, OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse,
    StatusAcceptance, StatusFrame, StatusKind, capabilities, parse_jsonrpc_request, plugin_error,
    response_to_jsonrpc,
};
use slack_api::{SlackClient, SlackUpload, send_incoming_webhook};
use slack_api::{SlackSocketEnvelope, SlackSocketModeClient};

const META_REASON: &str = "reason";
const META_PLATFORM: &str = "platform";
const META_BOT_TOKEN_ENV: &str = "bot_token_env";
const META_APP_TOKEN_ENV: &str = "app_token_env";
const META_DEFAULT_CHANNEL_ID: &str = "default_channel_id";
const META_DEFAULT_THREAD_TS: &str = "default_thread_ts";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_EVENTS_ENDPOINT: &str = "events_endpoint";
const META_SIGNING_SECRET_ENV: &str = "signing_secret_env";
const META_INCOMING_WEBHOOK: &str = "incoming_webhook";
const META_ALLOWED_TEAM_COUNT: &str = "allowed_team_count";
const META_TEAM_ID: &str = "team_id";
const META_TEAM_NAME: &str = "team_name";
const META_MODE: &str = "mode";
const META_HOST_ACTION: &str = "host_action";
const META_SIGNING_SECRET: &str = "signing_secret";
const META_BOT_USER_ID: &str = "bot_user_id";
const META_POLL_TIMEOUT_SECS: &str = "poll_timeout_secs";
const META_DELIVERY_MODE: &str = "delivery_mode";
const META_CHANNEL_ID: &str = "channel_id";
const META_THREAD_TS: &str = "thread_ts";
const META_DESTINATION_URL: &str = "destination_url";
const META_ATTACHMENT_COUNT: &str = "attachment_count";
const META_EVENT_TYPE: &str = "event_type";
const META_EVENT_SUBTYPE: &str = "event_subtype";
const META_TRANSPORT: &str = "transport";
const META_ENDPOINT_ID: &str = "endpoint_id";
const META_PATH: &str = "path";
const META_API_APP_ID: &str = "api_app_id";
const META_EVENT_CONTEXT: &str = "event_context";
const META_CHANNEL_TYPE: &str = "channel_type";
const META_STATUS_KIND: &str = "status_kind";
const META_REASON_CODE: &str = "reason_code";

const PLATFORM_SLACK: &str = "slack";
const MODE_INCOMING_WEBHOOK: &str = "incoming_webhook";
const DELIVERY_MODE_CHAT_POST_MESSAGE: &str = "chat.postMessage";
const TRANSPORT_EVENTS_WEBHOOK: &str = "events_webhook";
const TRANSPORT_SOCKET_MODE: &str = "socket_mode";
const MAX_SIGNATURE_AGE_SECS: i64 = 300;

const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const ROUTE_DESTINATION_URL: &str = "destination_url";
const SLACK_STATUS_TEXT: &str = "slack_status_text";

fn main() -> Result<()> {
    let stdin = io::stdin().lock();
    let stdout_lock = Arc::new(Mutex::new(()));
    let mut ingress_worker: Option<IngressWorker> = None;

    for line in stdin.lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let (request_id, envelope) = parse_jsonrpc_request(&line)
            .map_err(|error| anyhow!("failed to parse channel request: {error}"))?;
        let should_exit = matches!(envelope.request, PluginRequest::Shutdown);

        let response = match handle_request(&envelope, &stdout_lock, &mut ingress_worker) {
            Ok(response) => response,
            Err(error) => plugin_error("internal_error", error.to_string()),
        };

        let json = response_to_jsonrpc(&request_id, &response).map_err(|error| anyhow!(error))?;
        write_stdout_line(&stdout_lock, &json)?;
        if should_exit {
            break;
        }
    }

    let _ = stop_ingress_worker(&mut ingress_worker);
    Ok(())
}

fn handle_request(
    envelope: &PluginRequestEnvelope,
    stdout_lock: &Arc<Mutex<()>>,
    ingress_worker: &mut Option<IngressWorker>,
) -> Result<PluginResponse> {
    if envelope.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return Ok(plugin_error(
            "unsupported_protocol_version",
            format!(
                "expected protocol_version {}, got {}",
                CHANNEL_PLUGIN_PROTOCOL_VERSION, envelope.protocol_version
            ),
        ));
    }

    match &envelope.request {
        PluginRequest::Capabilities => Ok(PluginResponse::Capabilities {
            capabilities: capabilities(),
        }),
        PluginRequest::Configure { config } => Ok(PluginResponse::Configured {
            configuration: Box::new(configure(config)?),
        }),
        PluginRequest::Health { config } => Ok(PluginResponse::Health {
            health: health(config)?,
        }),
        PluginRequest::PollIngress { config, state } => handle_poll_ingress(config, state.as_ref()),
        PluginRequest::StartIngress { config, state } => {
            let started = start_ingress(config)?;
            let started = match (&started.mode, state.clone()) {
                (IngressMode::Polling, Some(state)) if state.mode == IngressMode::Polling => state,
                _ => started,
            };
            if matches!(started.mode, IngressMode::Polling) {
                restart_ingress_worker(
                    ingress_worker,
                    config.clone(),
                    started.clone(),
                    Arc::clone(stdout_lock),
                );
            } else {
                let _ = stop_ingress_worker(ingress_worker);
            }
            Ok(PluginResponse::IngressStarted { state: started })
        }
        PluginRequest::StopIngress { config, state } => Ok(PluginResponse::IngressStopped {
            state: stop_ingress(
                config,
                stop_ingress_worker(ingress_worker).or(state.clone()),
            )?,
        }),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::IngressEvent {
            config,
            state,
            payload,
        } => handle_ingress_event(config, state.as_ref(), payload),
        PluginRequest::Status { config, update } => Ok(PluginResponse::StatusAccepted {
            status: send_status(config, update)?,
        }),
        PluginRequest::Shutdown => {
            let _ = stop_ingress_worker(ingress_worker);
            Ok(PluginResponse::Ok)
        }
    }
}

fn restart_ingress_worker(
    worker: &mut Option<IngressWorker>,
    config: ChannelConfig,
    state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
) {
    restart_runtime_ingress_worker(
        worker,
        config,
        state,
        stdout_lock,
        PLATFORM_SLACK,
        slack_poll_ingress,
        no_after_cycle::<ChannelConfig>,
    );
}

fn slack_poll_ingress(
    config: &ChannelConfig,
    state: Option<IngressState>,
) -> Result<PluginResponse> {
    handle_poll_ingress(config, state.as_ref())
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let mut metadata = BTreeMap::new();

    if has_optional_env(bot_token_env(config)) {
        metadata.insert(
            META_BOT_TOKEN_ENV.to_string(),
            bot_token_env(config).to_string(),
        );
    }
    if has_optional_env(app_token_env(config)) {
        metadata.insert(
            META_APP_TOKEN_ENV.to_string(),
            app_token_env(config).to_string(),
        );
        metadata.insert(
            META_POLL_TIMEOUT_SECS.to_string(),
            poll_timeout_secs(config).to_string(),
        );
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        metadata.insert(
            META_DEFAULT_CHANNEL_ID.to_string(),
            default_channel_id.clone(),
        );
    }
    if let Some(default_thread_ts) = &config.default_thread_ts {
        metadata.insert(
            META_DEFAULT_THREAD_TS.to_string(),
            default_thread_ts.clone(),
        );
    }
    if let Some(endpoint) = resolved_endpoint(config) {
        metadata.insert(
            META_INGRESS_MODE.to_string(),
            ingress_mode_name(IngressMode::EventsWebhook),
        );
        metadata.insert(META_EVENTS_ENDPOINT.to_string(), endpoint);
        metadata.insert(
            META_SIGNING_SECRET_ENV.to_string(),
            signing_secret_env(config).to_string(),
        );
    }
    if let Some(webhook_url) = resolved_incoming_webhook_url(config) {
        validate_url(&webhook_url, "incoming webhook url")?;
        metadata.insert(META_INCOMING_WEBHOOK.to_string(), "configured".to_string());
    }
    if !config.allowed_team_ids.is_empty() {
        metadata.insert(
            META_ALLOWED_TEAM_COUNT.to_string(),
            config.allowed_team_ids.len().to_string(),
        );
    }

    Ok(ConfiguredChannel {
        metadata,
        policy: Some(channel_policy(config)),
        runtime: None,
    })
}

fn channel_policy(config: &ChannelConfig) -> ChannelPolicy {
    ChannelPolicy {
        owner_id: config.owner_id.clone(),
        allowed_sender_ids: config.allowed_sender_ids.clone(),
        allowed_conversation_ids: config.allowed_team_ids.clone(),
        dm_policy: config.dm_policy.clone(),
        require_signature_validation: Some(true),
        allow_group_messages: None,
        max_attachment_bytes: None,
        metadata: BTreeMap::new(),
    }
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());

    if has_optional_env(bot_token_env(config)) {
        let client = SlackClient::from_env(bot_token_env(config))?;
        let identity = client.identity()?;
        if let Some(team_id) = &identity.team_id {
            metadata.insert(META_TEAM_ID.to_string(), team_id.clone());
        }
        if let Some(team_name) = &identity.team_name {
            metadata.insert(META_TEAM_NAME.to_string(), team_name.clone());
        }
        return Ok(HealthReport {
            ok: true,
            status: "ok".to_string(),
            account_id: Some(identity.user_id),
            display_name: Some(
                identity
                    .user
                    .or(identity.team_name)
                    .unwrap_or_else(|| "slack-bot".to_string()),
            ),
            metadata,
        });
    }

    if has_optional_env(app_token_env(config)) {
        let client = SlackSocketModeClient::from_env(app_token_env(config))?;
        client.open_connection_url()?;
        metadata.insert(META_MODE.to_string(), TRANSPORT_SOCKET_MODE.to_string());
        return Ok(HealthReport {
            ok: true,
            status: "configured".to_string(),
            account_id: None,
            display_name: Some("slack-socket-mode".to_string()),
            metadata,
        });
    }

    if let Some(webhook_url) = resolved_incoming_webhook_url(config) {
        validate_url(&webhook_url, "incoming webhook url")?;
        metadata.insert(META_MODE.to_string(), MODE_INCOMING_WEBHOOK.to_string());
        return Ok(HealthReport {
            ok: true,
            status: "configured".to_string(),
            account_id: None,
            display_name: Some("slack-incoming-webhook".to_string()),
            metadata,
        });
    }

    Err(anyhow!(
        "slack health requires either a bot token or an incoming webhook URL"
    ))
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    if has_optional_env(app_token_env(config)) && resolved_endpoint(config).is_none() {
        return polling_state(config, "configured", None);
    }

    let endpoint = resolved_endpoint(config)
        .ok_or_else(|| anyhow!("slack ingress requires webhook_public_url"))?;
    validate_url(&endpoint, "slack events endpoint")?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());
    metadata.insert(
        META_HOST_ACTION.to_string(),
        "route Slack Events API POSTs to the reported endpoint and verify Slack signatures with the configured signing secret".to_string(),
    );
    metadata.insert(
        META_SIGNING_SECRET_ENV.to_string(),
        signing_secret_env(config).to_string(),
    );
    if has_optional_env(signing_secret_env(config)) {
        metadata.insert(META_SIGNING_SECRET.to_string(), "configured".to_string());
    }
    if has_optional_env(bot_token_env(config)) {
        let client = SlackClient::from_env(bot_token_env(config))?;
        let identity = client.identity()?;
        if let Some(team_id) = identity.team_id {
            metadata.insert(META_TEAM_ID.to_string(), team_id);
        }
        metadata.insert(META_BOT_USER_ID.to_string(), identity.user_id);
    }

    Ok(IngressState {
        mode: IngressMode::EventsWebhook,
        status: "configured".to_string(),
        endpoint: Some(endpoint),
        metadata,
    })
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    if has_optional_env(app_token_env(config)) && resolved_endpoint(config).is_none() {
        let mut stopped = match state {
            Some(state) => state,
            None => polling_state(config, "running", None)?,
        };
        stopped.status = "stopped".to_string();
        stopped
            .metadata
            .insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());
        return Ok(stopped);
    }

    if let Some(endpoint) = resolved_endpoint(config) {
        validate_url(&endpoint, "slack events endpoint")?;
    }

    let mut stopped = state.unwrap_or(IngressState {
        mode: IngressMode::EventsWebhook,
        status: "configured".to_string(),
        endpoint: resolved_endpoint(config),
        metadata: BTreeMap::new(),
    });
    stopped.status = "stopped".to_string();
    stopped
        .metadata
        .insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());
    Ok(stopped)
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let upload = slack_upload(message)?;
    if has_optional_env(bot_token_env(config)) {
        let channel_id = resolve_channel_id(config, message)?;
        let thread_ts = resolve_thread_ts(config, message);
        let client = SlackClient::from_env(bot_token_env(config))?;
        let posted =
            client.send_message(&channel_id, &message.content, thread_ts, upload.as_ref())?;

        let mut metadata = BTreeMap::new();
        metadata.insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());
        metadata.insert(
            META_DELIVERY_MODE.to_string(),
            DELIVERY_MODE_CHAT_POST_MESSAGE.to_string(),
        );
        metadata.insert(META_CHANNEL_ID.to_string(), posted.channel_id.clone());
        if let Some(thread_ts) = &posted.thread_ts {
            metadata.insert(META_THREAD_TS.to_string(), thread_ts.clone());
        }
        if upload.is_some() {
            metadata.insert(META_ATTACHMENT_COUNT.to_string(), "1".to_string());
        }

        return Ok(DeliveryReceipt {
            message_id: posted.message_id,
            conversation_id: posted.channel_id,
            metadata,
        });
    }

    let webhook_url = resolve_destination_url(config, message)
        .ok_or_else(|| anyhow!("slack delivery requires a bot token or an incoming webhook URL"))?;
    if upload.is_some() {
        bail!(
            "slack outbound attachments require bot-token delivery; incoming webhook delivery does not support attachments"
        );
    }
    let posted = send_incoming_webhook(&webhook_url, &message.content)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());
    metadata.insert(
        META_DELIVERY_MODE.to_string(),
        MODE_INCOMING_WEBHOOK.to_string(),
    );
    metadata.insert(META_DESTINATION_URL.to_string(), "configured".to_string());

    Ok(DeliveryReceipt {
        message_id: posted.message_id,
        conversation_id: posted.channel_id,
        metadata,
    })
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let content = render_status_message(update);
    if content.trim().is_empty() {
        return Ok(rejected_status(
            "missing_message",
            "slack status frames require a message or slack_status_text override",
        ));
    }

    let message = OutboundMessage {
        content,
        attachments: Vec::new(),
        channel_id: update
            .conversation_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_CONVERSATION_ID).cloned()),
        thread_ts: update
            .thread_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_THREAD_ID).cloned()),
        destination_url: update.metadata.get(ROUTE_DESTINATION_URL).cloned(),
        metadata: BTreeMap::new(),
    };
    let delivery = match deliver(config, &message) {
        Ok(delivery) => delivery,
        Err(error) => {
            return Ok(rejected_status("delivery_failed", error.to_string()));
        }
    };

    let mut metadata = delivery.metadata.clone();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());
    metadata.insert(META_STATUS_KIND.to_string(), status_kind_name(&update.kind));
    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

fn slack_upload(message: &OutboundMessage) -> Result<Option<SlackUpload>> {
    match message.attachments.as_slice() {
        [] => Ok(None),
        [attachment] => {
            if attachment.url.is_some() {
                bail!(
                    "slack outbound attachments require data_base64; url attachments are not supported"
                );
            }
            if attachment.storage_key.is_some() {
                bail!(
                    "slack outbound attachments require data_base64; storage_key attachments are not supported"
                );
            }
            let Some(data_base64) = attachment.data_base64.as_deref() else {
                bail!("slack outbound attachments require data_base64");
            };
            let data = BASE64_STANDARD.decode(data_base64).with_context(|| {
                format!(
                    "invalid base64 attachment payload for `{}`",
                    attachment.name
                )
            })?;
            Ok(Some(SlackUpload {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                data,
            }))
        }
        _ => bail!("slack delivery supports at most one attachment"),
    }
}

fn handle_ingress_event(
    config: &ChannelConfig,
    state: Option<&IngressState>,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if !payload.method.eq_ignore_ascii_case("POST") {
        return Ok(ingress_rejection(
            405,
            "slack ingress expects POST requests",
        ));
    }

    if let Some(reply) = validate_ingress_signature(config, payload)? {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(reply),
            state: None,
            poll_after_ms: None,
        });
    }

    let envelope: SlackEventEnvelope = match serde_json::from_str(&payload.body) {
        Ok(envelope) => envelope,
        Err(_) => return Ok(ingress_rejection(400, "invalid Slack event payload")),
    };

    if !team_is_allowed(config, envelope.team_id.as_deref()) {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: None,
            poll_after_ms: None,
        });
    }

    match envelope.envelope_type.as_str() {
        "url_verification" => {
            let Some(challenge) = envelope.challenge else {
                return Ok(ingress_rejection(
                    400,
                    "slack url_verification payload missing challenge",
                ));
            };
            Ok(PluginResponse::IngressEventsReceived {
                events: Vec::new(),
                callback_reply: Some(IngressCallbackReply {
                    status: 200,
                    content_type: Some("text/plain; charset=utf-8".to_string()),
                    body: challenge,
                }),
                state: None,
                poll_after_ms: None,
            })
        }
        "event_callback" => {
            let Some(event) = envelope.event.as_ref() else {
                return Ok(ingress_rejection(
                    400,
                    "slack event_callback payload missing event body",
                ));
            };

            let Some(inbound_event) = build_inbound_event(
                config,
                state,
                payload,
                &envelope,
                event,
                TRANSPORT_EVENTS_WEBHOOK,
            )?
            else {
                return Ok(PluginResponse::IngressEventsReceived {
                    events: Vec::new(),
                    callback_reply: None,
                    state: None,
                    poll_after_ms: None,
                });
            };

            Ok(PluginResponse::IngressEventsReceived {
                events: vec![inbound_event],
                callback_reply: None,
                state: None,
                poll_after_ms: None,
            })
        }
        _ => Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: None,
            poll_after_ms: None,
        }),
    }
}

fn handle_poll_ingress(
    config: &ChannelConfig,
    state: Option<&IngressState>,
) -> Result<PluginResponse> {
    if !has_optional_env(app_token_env(config)) {
        return Ok(plugin_error(
            "polling_not_supported",
            "slack poll_ingress requires Slack Socket Mode with SLACK_APP_TOKEN",
        ));
    }

    let client = SlackSocketModeClient::from_env(app_token_env(config))?;
    let socket_envelope = client.receive_event(poll_timeout_secs(config))?;
    let next_state = Some(polling_state(config, "running", state)?);

    let Some(socket_envelope) = socket_envelope else {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: next_state,
            poll_after_ms: Some(1000),
        });
    };

    let Some(inbound_event) = build_socket_mode_event(config, state, &socket_envelope)? else {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: next_state,
            poll_after_ms: Some(1000),
        });
    };

    Ok(PluginResponse::IngressEventsReceived {
        events: vec![inbound_event],
        callback_reply: None,
        state: next_state,
        poll_after_ms: Some(1000),
    })
}

fn build_socket_mode_event(
    config: &ChannelConfig,
    ingress_state: Option<&IngressState>,
    socket_envelope: &SlackSocketEnvelope,
) -> Result<Option<InboundEventEnvelope>> {
    let Some(payload) = socket_envelope.payload.as_ref() else {
        return Ok(None);
    };

    let envelope: SlackEventEnvelope = serde_json::from_value(payload.clone())
        .context("invalid Slack Socket Mode event payload")?;
    let synthetic_payload = IngressPayload {
        endpoint_id: None,
        method: "SOCKET".to_string(),
        path: String::new(),
        headers: BTreeMap::new(),
        query: BTreeMap::new(),
        raw_query: None,
        body: String::new(),
        trust_verified: true,
        received_at: None,
    };

    match envelope.envelope_type.as_str() {
        "event_callback" => {
            let Some(event) = envelope.event.as_ref() else {
                return Ok(None);
            };
            build_inbound_event(
                config,
                ingress_state,
                &synthetic_payload,
                &envelope,
                event,
                TRANSPORT_SOCKET_MODE,
            )
        }
        _ => Ok(None),
    }
}

fn build_inbound_event(
    config: &ChannelConfig,
    ingress_state: Option<&IngressState>,
    payload: &IngressPayload,
    envelope: &SlackEventEnvelope,
    event: &SlackEventPayload,
    transport: &str,
) -> Result<Option<InboundEventEnvelope>> {
    if !supports_inbound_event(event) {
        return Ok(None);
    }
    if is_self_authored_event(ingress_state, event) {
        return Ok(None);
    }

    let Some(channel_id) = event.channel.as_ref() else {
        return Ok(None);
    };
    let Some(actor) = inbound_actor(event) else {
        return Ok(None);
    };
    if !sender_is_allowed(config, &actor.id, event.channel_type.as_deref()) {
        return Ok(None);
    }

    let message_id = event
        .client_msg_id
        .clone()
        .or_else(|| event.ts.clone())
        .or_else(|| event.event_ts.clone())
        .unwrap_or_else(|| {
            envelope
                .event_id
                .clone()
                .unwrap_or_else(|| "slack-event".to_string())
        });
    let received_at = received_at(payload.received_at.as_deref(), envelope.event_time)?;
    let thread_id = event.thread_ts.clone();
    let parent_message_id = match (event.thread_ts.as_ref(), event.ts.as_ref()) {
        (Some(thread_ts), Some(ts)) if thread_ts != ts => Some(thread_ts.clone()),
        _ => None,
    };

    let mut message_metadata = BTreeMap::new();
    if let Some(channel_type) = &event.channel_type {
        message_metadata.insert(META_CHANNEL_TYPE.to_string(), channel_type.clone());
    }
    let attachments = extract_attachments(event, &mut message_metadata);

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(META_TRANSPORT.to_string(), transport.to_string());
    event_metadata.insert(META_EVENT_TYPE.to_string(), event.event_type.clone());
    if let Some(subtype) = &event.subtype {
        event_metadata.insert(META_EVENT_SUBTYPE.to_string(), subtype.clone());
    }
    if let Some(endpoint_id) = &payload.endpoint_id {
        event_metadata.insert(META_ENDPOINT_ID.to_string(), endpoint_id.clone());
    }
    if !payload.path.is_empty() {
        event_metadata.insert(META_PATH.to_string(), payload.path.clone());
    }
    if let Some(team_id) = &envelope.team_id {
        event_metadata.insert(META_TEAM_ID.to_string(), team_id.clone());
    }
    if let Some(api_app_id) = &envelope.api_app_id {
        event_metadata.insert(META_API_APP_ID.to_string(), api_app_id.clone());
    }
    if let Some(event_context) = &envelope.event_context {
        event_metadata.insert(META_EVENT_CONTEXT.to_string(), event_context.clone());
    }

    let account_id = envelope.team_id.clone();
    if !team_is_allowed(config, account_id.as_deref()) {
        return Ok(None);
    }

    Ok(Some(InboundEventEnvelope {
        event_id: envelope.event_id.clone().unwrap_or_else(|| {
            let ts = event
                .event_ts
                .as_deref()
                .or(event.ts.as_deref())
                .unwrap_or("unknown");
            format!("slack:{channel_id}:{ts}")
        }),
        platform: PLATFORM_SLACK.to_string(),
        event_type: event.event_type.clone(),
        received_at,
        conversation: InboundConversationRef {
            id: channel_id.clone(),
            kind: conversation_kind(event.channel_type.as_deref()),
            thread_id,
            parent_message_id: parent_message_id.clone(),
        },
        actor,
        message: InboundMessage {
            id: message_id,
            content: event.text.clone().unwrap_or_default(),
            content_type: "text/plain".to_string(),
            reply_to_message_id: parent_message_id.clone(),
            attachments,
            metadata: message_metadata,
        },
        account_id,
        metadata: event_metadata,
    }))
}

fn supports_inbound_event(event: &SlackEventPayload) -> bool {
    if event.hidden {
        return false;
    }

    match event.event_type.as_str() {
        "app_mention" => event.subtype.is_none(),
        "message" => matches!(event.subtype.as_deref(), None | Some("file_share")),
        _ => false,
    }
}

fn is_self_authored_event(ingress_state: Option<&IngressState>, event: &SlackEventPayload) -> bool {
    let bot_user_id = ingress_state
        .and_then(|state| state.metadata.get(META_BOT_USER_ID))
        .map(String::as_str);
    is_self_authored_event_for_bot_user(event, bot_user_id)
}

fn is_self_authored_event_for_bot_user(
    event: &SlackEventPayload,
    bot_user_id: Option<&str>,
) -> bool {
    if event.bot_id.is_some() {
        return true;
    }

    if matches!(event.subtype.as_deref(), Some("bot_message")) {
        return true;
    }

    match (event.user.as_deref(), bot_user_id) {
        (Some(event_user), Some(bot_user_id)) => event_user == bot_user_id,
        _ => false,
    }
}

fn extract_attachments(
    event: &SlackEventPayload,
    message_metadata: &mut BTreeMap<String, String>,
) -> Vec<InboundAttachment> {
    let mut attachments = Vec::new();
    for file in &event.files {
        let Some(url) = file.url_private.clone().or_else(|| file.permalink.clone()) else {
            continue;
        };

        let mut extras = BTreeMap::new();
        if let Some(permalink) = &file.permalink {
            extras.insert("permalink".to_string(), permalink.clone());
        }
        if let Some(filetype) = &file.filetype {
            extras.insert("filetype".to_string(), filetype.clone());
        }
        if let Some(pretty_type) = &file.pretty_type {
            extras.insert("pretty_type".to_string(), pretty_type.clone());
        }
        if let Some(mode) = &file.mode {
            extras.insert("mode".to_string(), mode.clone());
        }

        attachments.push(InboundAttachment {
            id: file.id.clone(),
            kind: attachment_kind(file.mimetype.as_deref()),
            url: Some(url),
            mime_type: file.mimetype.clone(),
            size_bytes: file.size,
            name: file.name.clone(),
            storage_key: None,
            extracted_text: None,
            extras,
        });
    }

    if !attachments.is_empty() {
        message_metadata.insert(
            "attachment_count".to_string(),
            attachments.len().to_string(),
        );
    }

    attachments
}

fn attachment_kind(mime_type: Option<&str>) -> String {
    match mime_type.and_then(|mime| mime.split('/').next()) {
        Some("image") => "image".to_string(),
        Some("audio") => "audio".to_string(),
        Some("video") => "video".to_string(),
        _ => "file".to_string(),
    }
}

fn inbound_actor(event: &SlackEventPayload) -> Option<InboundActor> {
    if let Some(user_id) = &event.user {
        return Some(InboundActor {
            id: user_id.clone(),
            display_name: event.username.clone(),
            username: None,
            is_bot: false,
            metadata: BTreeMap::new(),
        });
    }

    event.bot_id.as_ref().map(|bot_id| InboundActor {
        id: bot_id.clone(),
        display_name: event.username.clone(),
        username: None,
        is_bot: true,
        metadata: BTreeMap::from([("actor_kind".to_string(), "bot".to_string())]),
    })
}

fn validate_ingress_signature(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let Ok(secret) = std::env::var(signing_secret_env(config)) else {
        return Ok(None);
    };

    validate_slack_signature(&secret, payload, current_unix_timestamp()?)
}

fn validate_slack_signature(
    secret: &str,
    payload: &IngressPayload,
    now_epoch_secs: i64,
) -> Result<Option<IngressCallbackReply>> {
    let Some(timestamp_header) = header_value(&payload.headers, "X-Slack-Request-Timestamp") else {
        return Ok(Some(callback_reply(
            403,
            "slack request timestamp header missing",
        )));
    };
    let timestamp = timestamp_header
        .parse::<i64>()
        .map_err(|_| anyhow!("invalid X-Slack-Request-Timestamp header"))?;
    if (now_epoch_secs - timestamp).abs() > MAX_SIGNATURE_AGE_SECS {
        return Ok(Some(callback_reply(
            403,
            "slack request timestamp is too old",
        )));
    }

    let Some(signature_header) = header_value(&payload.headers, "X-Slack-Signature") else {
        return Ok(Some(callback_reply(
            403,
            "slack request signature header missing",
        )));
    };
    let Some(signature_hex) = signature_header.strip_prefix("v0=") else {
        return Ok(Some(callback_reply(
            403,
            "slack request signature must use the v0 format",
        )));
    };

    let signature =
        hex::decode(signature_hex).map_err(|_| anyhow!("invalid X-Slack-Signature header"))?;
    let signing_input = format!("v0:{timestamp_header}:{}", payload.body);

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .context("failed to initialize Slack signature verifier")?;
    mac.update(signing_input.as_bytes());

    if mac.verify_slice(&signature).is_err() {
        return Ok(Some(callback_reply(
            403,
            "slack request signature mismatch",
        )));
    }

    Ok(None)
}

fn current_unix_timestamp() -> Result<i64> {
    Ok(Timestamp::now().as_second())
}

fn received_at(host_received_at: Option<&str>, event_time: Option<i64>) -> Result<String> {
    if let Some(host_received_at) = host_received_at {
        return Ok(host_received_at.to_string());
    }

    let timestamp = event_time.unwrap_or(current_unix_timestamp()?);
    Ok(Timestamp::from_second(timestamp)
        .context("slack event timestamp is out of range")?
        .to_string())
}

fn team_is_allowed(config: &ChannelConfig, team_id: Option<&str>) -> bool {
    config.allowed_team_ids.is_empty()
        || team_id
            .map(|team_id| {
                config
                    .allowed_team_ids
                    .iter()
                    .any(|allowed| allowed == team_id)
            })
            .unwrap_or(false)
}

fn sender_is_allowed(config: &ChannelConfig, sender_id: &str, channel_type: Option<&str>) -> bool {
    if let Some(owner_id) = config.owner_id.as_deref()
        && owner_id != sender_id
    {
        return false;
    }

    if !matches!(channel_type, Some("im")) {
        return true;
    }

    match config.dm_policy.as_deref() {
        Some("deny") => false,
        Some("pairing") => id_matches_allowlist(&config.allowed_sender_ids, sender_id),
        Some("open") | None => true,
        Some(_) => true,
    }
}

fn id_matches_allowlist(allowlist: &[String], value: &str) -> bool {
    allowlist
        .iter()
        .any(|allowed| allowed == "*" || allowed == value)
}

fn conversation_kind(channel_type: Option<&str>) -> String {
    match channel_type {
        Some("im") => "direct_message".to_string(),
        Some("mpim") => "group_dm".to_string(),
        Some("group") => "private_channel".to_string(),
        Some("app_home") => "app_home".to_string(),
        Some(kind) => kind.to_string(),
        None => "channel".to_string(),
    }
}

fn render_status_message(update: &StatusFrame) -> String {
    if let Some(status_text) = update.metadata.get(SLACK_STATUS_TEXT) {
        return status_text.clone();
    }

    let prefix = match update.kind {
        StatusKind::Processing => "Processing",
        StatusKind::Completed => "Completed",
        StatusKind::Cancelled => "Cancelled",
        StatusKind::OperationStarted => "Started",
        StatusKind::OperationFinished => "Finished",
        StatusKind::ApprovalNeeded => "Approval needed",
        StatusKind::Info => "Info",
        StatusKind::Delivering => "Delivering",
        StatusKind::AuthRequired => "Authentication required",
        _ => "Status",
    };
    if update.message.trim().is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {}", update.message)
    }
}

fn ingress_rejection(status: u16, message: &str) -> PluginResponse {
    PluginResponse::IngressEventsReceived {
        events: Vec::new(),
        callback_reply: Some(callback_reply(status, message)),
        state: None,
        poll_after_ms: None,
    }
}

fn callback_reply(status: u16, message: &str) -> IngressCallbackReply {
    IngressCallbackReply {
        status,
        content_type: Some("text/plain; charset=utf-8".to_string()),
        body: message.to_string(),
    }
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn rejected_status(reason_code: &str, reason: impl Into<String>) -> StatusAcceptance {
    StatusAcceptance {
        accepted: false,
        metadata: BTreeMap::from([
            (META_REASON_CODE.to_string(), reason_code.to_string()),
            (META_REASON.to_string(), reason.into()),
        ]),
    }
}

fn status_kind_name(kind: &StatusKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn resolved_endpoint(config: &ChannelConfig) -> Option<String> {
    let base = config.webhook_public_url.as_deref()?.trim_end_matches('/');
    let path = config
        .webhook_path
        .as_deref()
        .unwrap_or("/slack/events")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn resolved_incoming_webhook_url(config: &ChannelConfig) -> Option<String> {
    if let Some(webhook_url) = &config.incoming_webhook_url {
        return Some(webhook_url.clone());
    }
    std::env::var(incoming_webhook_url_env(config)).ok()
}

fn resolve_channel_id(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
    if let Some(channel_id) = &message.channel_id {
        return Ok(channel_id.clone());
    }
    if let Some(channel_id) = message.metadata.get(ROUTE_CONVERSATION_ID) {
        return Ok(channel_id.clone());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        return Ok(default_channel_id.clone());
    }
    Err(anyhow!(
        "slack bot-token delivery requires message.channel_id or config.default_channel_id"
    ))
}

fn resolve_thread_ts<'a>(
    config: &'a ChannelConfig,
    message: &'a OutboundMessage,
) -> Option<&'a str> {
    message
        .thread_ts
        .as_deref()
        .or_else(|| message.metadata.get(ROUTE_THREAD_ID).map(String::as_str))
        .or(config.default_thread_ts.as_deref())
}

fn resolve_destination_url(config: &ChannelConfig, message: &OutboundMessage) -> Option<String> {
    message
        .destination_url
        .clone()
        .or_else(|| message.metadata.get(ROUTE_DESTINATION_URL).cloned())
        .or_else(|| resolved_incoming_webhook_url(config))
}

fn bot_token_env(config: &ChannelConfig) -> &str {
    config.bot_token_env.as_deref().unwrap_or("SLACK_BOT_TOKEN")
}

fn app_token_env(config: &ChannelConfig) -> &str {
    config.app_token_env.as_deref().unwrap_or("SLACK_APP_TOKEN")
}

fn signing_secret_env(config: &ChannelConfig) -> &str {
    config
        .signing_secret_env
        .as_deref()
        .unwrap_or("SLACK_SIGNING_SECRET")
}

fn incoming_webhook_url_env(config: &ChannelConfig) -> &str {
    config
        .incoming_webhook_url_env
        .as_deref()
        .unwrap_or("SLACK_INCOMING_WEBHOOK_URL")
}

fn has_optional_env(name: &str) -> bool {
    std::env::var(name).is_ok()
}

fn poll_timeout_secs(config: &ChannelConfig) -> u16 {
    config.poll_timeout_secs.unwrap_or(30).max(1)
}

fn polling_state(
    config: &ChannelConfig,
    status: &str,
    prior_state: Option<&IngressState>,
) -> Result<IngressState> {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SLACK.to_string());
    metadata.insert(META_MODE.to_string(), TRANSPORT_SOCKET_MODE.to_string());
    metadata.insert(
        META_POLL_TIMEOUT_SECS.to_string(),
        poll_timeout_secs(config).to_string(),
    );
    if has_optional_env(app_token_env(config)) {
        metadata.insert(
            META_APP_TOKEN_ENV.to_string(),
            app_token_env(config).to_string(),
        );
    }
    if has_optional_env(bot_token_env(config)) {
        if let Some(existing_bot_user_id) =
            prior_state.and_then(|state| state.metadata.get(META_BOT_USER_ID))
        {
            metadata.insert(META_BOT_USER_ID.to_string(), existing_bot_user_id.clone());
        }
        if let Some(existing_team_id) =
            prior_state.and_then(|state| state.metadata.get(META_TEAM_ID))
        {
            metadata.insert(META_TEAM_ID.to_string(), existing_team_id.clone());
        }
        if !metadata.contains_key(META_BOT_USER_ID) {
            let client = SlackClient::from_env(bot_token_env(config))?;
            let identity = client.identity()?;
            if let Some(team_id) = identity.team_id {
                metadata.insert(META_TEAM_ID.to_string(), team_id);
            }
            metadata.insert(META_BOT_USER_ID.to_string(), identity.user_id);
        }
    }

    Ok(IngressState {
        mode: IngressMode::Polling,
        status: status.to_string(),
        endpoint: None,
        metadata,
    })
}

fn ingress_mode_name(mode: IngressMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "events_webhook".to_string())
}

fn validate_url(url: &str, field: &str) -> Result<()> {
    let parsed = ureq::http::Uri::try_from(url)
        .map_err(|error| anyhow!("{field} is not a valid URI: {error}"))?;
    let scheme = parsed
        .scheme_str()
        .ok_or_else(|| anyhow!("{field} must include a URL scheme"))?;
    if scheme != "http" && scheme != "https" {
        return Err(anyhow!("{field} must use http or https"));
    }
    if parsed.host().is_none() {
        return Err(anyhow!("{field} must include a host"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct SlackEventEnvelope {
    #[serde(rename = "type")]
    envelope_type: String,
    #[serde(default)]
    challenge: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    api_app_id: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    event_time: Option<i64>,
    #[serde(default)]
    event_context: Option<String>,
    #[serde(default)]
    event: Option<SlackEventPayload>,
}

#[derive(Debug, Deserialize)]
struct SlackEventPayload {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    channel_type: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    event_ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    client_msg_id: Option<String>,
    #[serde(default)]
    files: Vec<SlackFile>,
}

#[derive(Debug, Deserialize)]
struct SlackFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mimetype: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    url_private: Option<String>,
    #[serde(default)]
    permalink: Option<String>,
    #[serde(default)]
    filetype: Option<String>,
    #[serde(default)]
    pretty_type: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::OutboundAttachment;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    fn base_payload(body: &str) -> IngressPayload {
        IngressPayload {
            endpoint_id: Some("slack-events".to_string()),
            method: "POST".to_string(),
            path: "/slack/events".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: body.to_string(),
            trust_verified: true,
            received_at: Some("2026-04-11T18:00:00Z".to_string()),
        }
    }

    #[test]
    fn url_verification_returns_challenge_reply() {
        let payload = base_payload(
            r#"{"type":"url_verification","challenge":"challenge-token","team_id":"T123"}"#,
        );

        let response = handle_ingress_event(&ChannelConfig::default(), None, &payload)
            .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(events.is_empty());
                let reply = callback_reply.expect("challenge reply");
                assert_eq!(reply.status, 200);
                assert_eq!(reply.body, "challenge-token");
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn event_callback_maps_message_event() {
        let payload = base_payload(
            r#"{
                "type":"event_callback",
                "team_id":"T123",
                "api_app_id":"A123",
                "event_id":"Ev123",
                "event_time":1712860000,
                "event_context":"4-message-T123-C123",
                "event":{
                    "type":"app_mention",
                    "channel":"C123",
                    "channel_type":"channel",
                    "user":"U123",
                    "text":"hello from slack",
                    "ts":"1712860000.100200",
                    "event_ts":"1712860000.100200",
                    "thread_ts":"1712860000.000001"
                }
            }"#,
        );

        let response = handle_ingress_event(&ChannelConfig::default(), None, &payload)
            .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(callback_reply.is_none());
                assert_eq!(events.len(), 1);
                let event = &events[0];
                assert_eq!(event.event_id, "Ev123");
                assert_eq!(event.platform, "slack");
                assert_eq!(event.event_type, "app_mention");
                assert_eq!(event.account_id.as_deref(), Some("T123"));
                assert_eq!(event.conversation.id, "C123");
                assert_eq!(
                    event.conversation.thread_id.as_deref(),
                    Some("1712860000.000001")
                );
                assert_eq!(event.actor.id, "U123");
                assert_eq!(event.message.content, "hello from slack");
                assert_eq!(
                    event.metadata.get(META_TRANSPORT).map(String::as_str),
                    Some(TRANSPORT_EVENTS_WEBHOOK)
                );
                assert_eq!(
                    event.metadata.get(META_ENDPOINT_ID).map(String::as_str),
                    Some("slack-events")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn hidden_or_subtyped_message_is_ignored() {
        let payload = base_payload(
            r#"{
                "type":"event_callback",
                "team_id":"T123",
                "event_id":"Ev124",
                "event":{
                    "type":"message",
                    "subtype":"message_changed",
                    "channel":"C123",
                    "user":"U123",
                    "text":"edited"
                }
            }"#,
        );

        let response = handle_ingress_event(&ChannelConfig::default(), None, &payload)
            .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => assert!(events.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn file_share_message_maps_attachments() {
        let payload = base_payload(
            r#"{
                "type":"event_callback",
                "team_id":"T123",
                "event_id":"Ev125",
                "event":{
                    "type":"message",
                    "subtype":"file_share",
                    "channel":"C123",
                    "channel_type":"channel",
                    "user":"U123",
                    "text":"sharing a file",
                    "ts":"1712860001.100200",
                    "event_ts":"1712860001.100200",
                    "thread_ts":"1712860000.000001",
                    "files":[
                        {
                            "id":"F123",
                            "name":"report.pdf",
                            "mimetype":"application/pdf",
                            "size":5120,
                            "url_private":"https://files.slack.com/files-pri/T123-F123/report.pdf",
                            "permalink":"https://example.slack.com/files/U123/F123/report.pdf",
                            "filetype":"pdf",
                            "pretty_type":"PDF",
                            "mode":"hosted"
                        }
                    ]
                }
            }"#,
        );

        let response = handle_ingress_event(&ChannelConfig::default(), None, &payload)
            .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => {
                assert_eq!(events.len(), 1);
                let event = &events[0];
                assert_eq!(
                    event.message.reply_to_message_id.as_deref(),
                    Some("1712860000.000001")
                );
                assert_eq!(event.message.attachments.len(), 1);
                let attachment = &event.message.attachments[0];
                assert_eq!(attachment.id.as_deref(), Some("F123"));
                assert_eq!(attachment.kind, "file");
                assert_eq!(attachment.mime_type.as_deref(), Some("application/pdf"));
                assert_eq!(attachment.name.as_deref(), Some("report.pdf"));
                assert_eq!(attachment.size_bytes, Some(5120));
                assert_eq!(
                    attachment.extras.get("permalink").map(String::as_str),
                    Some("https://example.slack.com/files/U123/F123/report.pdf")
                );
                assert_eq!(
                    event.metadata.get(META_EVENT_SUBTYPE).map(String::as_str),
                    Some("file_share")
                );
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get("attachment_count")
                        .map(String::as_str),
                    Some("1")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn self_authored_bot_user_message_is_detected() {
        let payload = base_payload(
            r#"{
                "type":"event_callback",
                "team_id":"T123",
                "event_id":"Ev127",
                "event":{
                    "type":"message",
                    "channel":"C123",
                    "channel_type":"channel",
                    "user":"UBOT123",
                    "text":"echo: reply",
                    "ts":"1712860002.100200",
                    "event_ts":"1712860002.100200"
                }
            }"#,
        );

        let event_envelope: SlackEventEnvelope =
            serde_json::from_str(&payload.body).expect("parse event envelope");
        let event = event_envelope.event.as_ref().expect("event body");
        let ingress_state = IngressState {
            mode: IngressMode::EventsWebhook,
            status: "configured".to_string(),
            endpoint: Some("https://example.com/slack/events".to_string()),
            metadata: BTreeMap::from([(META_BOT_USER_ID.to_string(), "UBOT123".to_string())]),
        };
        assert!(is_self_authored_event(Some(&ingress_state), event));
    }

    #[test]
    fn bot_message_subtype_is_ignored() {
        let payload = base_payload(
            r#"{
                "type":"event_callback",
                "team_id":"T123",
                "event_id":"Ev128",
                "event":{
                    "type":"message",
                    "subtype":"bot_message",
                    "channel":"C123",
                    "channel_type":"channel",
                    "bot_id":"B123",
                    "text":"echo: reply",
                    "ts":"1712860003.100200",
                    "event_ts":"1712860003.100200"
                }
            }"#,
        );

        let response = handle_ingress_event(&ChannelConfig::default(), None, &payload)
            .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => assert!(events.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let mut payload = base_payload(r#"{"type":"event_callback","team_id":"T123","event":{}}"#);
        payload.trust_verified = false;
        payload.headers.insert(
            "X-Slack-Request-Timestamp".to_string(),
            "1712860000".to_string(),
        );
        payload.headers.insert(
            "X-Slack-Signature".to_string(),
            "v0=0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        );

        let reply =
            validate_slack_signature("secret", &payload, 1712860001).expect("validate signature");

        let reply = reply.expect("rejection reply");
        assert_eq!(reply.status, 403);
        assert_eq!(reply.body, "slack request signature mismatch");
    }

    #[test]
    fn direct_message_is_dropped_when_pairing_policy_has_no_allowlist_match() {
        let payload = base_payload(
            r#"{
                "type":"event_callback",
                "team_id":"T123",
                "event_id":"Ev126",
                "event":{
                    "type":"message",
                    "channel":"D123",
                    "channel_type":"im",
                    "user":"U999",
                    "text":"hello from slack",
                    "ts":"1712860000.100200",
                    "event_ts":"1712860000.100200"
                }
            }"#,
        );

        let response = handle_ingress_event(
            &ChannelConfig {
                dm_policy: Some("pairing".to_string()),
                ..ChannelConfig::default()
            },
            None,
            &payload,
        )
        .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => assert!(events.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn status_kind_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&StatusKind::OperationStarted).expect("serialize"),
            "\"operation_started\""
        );
    }

    #[test]
    fn render_status_message_uses_override() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "ignored".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::from([(SLACK_STATUS_TEXT.to_string(), "custom".to_string())]),
        };

        assert_eq!(render_status_message(&update), "custom");
    }

    #[test]
    fn resolve_channel_id_uses_standard_metadata() {
        let message = OutboundMessage {
            content: "reply".to_string(),
            attachments: Vec::new(),
            channel_id: None,
            thread_ts: None,
            destination_url: None,
            metadata: BTreeMap::from([(ROUTE_CONVERSATION_ID.to_string(), "C123".to_string())]),
        };

        assert_eq!(
            resolve_channel_id(&ChannelConfig::default(), &message).expect("channel id"),
            "C123"
        );
    }

    #[test]
    fn send_status_rejects_missing_destination() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "hello".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        let acceptance = send_status(&ChannelConfig::default(), &update).expect("send status");

        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some("delivery_failed")
        );
    }

    #[test]
    fn slack_upload_decodes_inline_attachment_data() {
        let message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: None,
                storage_key: None,
            }],
            channel_id: Some("C123".to_string()),
            thread_ts: None,
            destination_url: None,
            metadata: BTreeMap::new(),
        };

        let upload = slack_upload(&message)
            .expect("inline attachment")
            .expect("upload present");
        assert_eq!(upload.name, "report.txt");
        assert_eq!(upload.mime_type, "text/plain");
        assert_eq!(upload.data, b"hello");
    }

    #[test]
    fn slack_upload_rejects_url_and_storage_key_sources() {
        let url_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: Some("https://example.com/report.txt".to_string()),
                storage_key: None,
            }],
            channel_id: Some("C123".to_string()),
            thread_ts: None,
            destination_url: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            slack_upload(&url_message)
                .expect_err("url attachments rejected")
                .to_string()
                .contains("url attachments are not supported")
        );

        let staged_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: None,
                storage_key: Some("cache://report.txt".to_string()),
            }],
            channel_id: Some("C123".to_string()),
            thread_ts: None,
            destination_url: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            slack_upload(&staged_message)
                .expect_err("storage_key attachments rejected")
                .to_string()
                .contains("storage_key attachments are not supported")
        );
    }

    #[test]
    fn incoming_webhook_delivery_redacts_destination_url_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(false)
            .expect("listener blocking mode");
        let address = listener.local_addr().expect("listener addr");
        let webhook_url = format!("http://{address}/services/test");

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .expect("set read timeout");

            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            || error.kind() == io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(error) => panic!("read request: {error}"),
                }
            }

            let response =
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nok";
            stream.write_all(response).expect("write response");
        });

        let delivery = deliver(
            &ChannelConfig {
                incoming_webhook_url: Some(webhook_url.clone()),
                ..ChannelConfig::default()
            },
            &OutboundMessage {
                content: "hello from incoming webhook".to_string(),
                attachments: Vec::new(),
                channel_id: None,
                thread_ts: None,
                destination_url: None,
                metadata: BTreeMap::new(),
            },
        )
        .expect("deliver");

        assert_eq!(
            delivery
                .metadata
                .get(META_DELIVERY_MODE)
                .map(String::as_str),
            Some(MODE_INCOMING_WEBHOOK)
        );
        assert_eq!(
            delivery
                .metadata
                .get(META_DESTINATION_URL)
                .map(String::as_str),
            Some("configured")
        );
        assert_ne!(
            delivery
                .metadata
                .get(META_DESTINATION_URL)
                .map(String::as_str),
            Some(webhook_url.as_str())
        );

        server.join().expect("server thread");
    }
}
