use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use dispatch_channel_runtime::{
    IngressPollContext, IngressWorker, RuntimeError, no_after_cycle,
    restart_ingress_worker as restart_runtime_ingress_worker, stop_ingress_worker,
    write_stdout_line,
};
use hmac::{Hmac, KeyInit, Mac};
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
    DeliveryReceipt, FetchedMessage, FetchedMessageAuthor, HealthReport, InboundActivation,
    InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope, InboundMessage,
    IngressCallbackReply, IngressMode, IngressPayload, IngressState, MessagePermalink, MessageRef,
    OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse, SlackActivationPolicy,
    SlackDirectMessagePolicy, StatusAcceptance, StatusFrame, StatusKind, capabilities,
    parse_jsonrpc_request, plugin_error, response_to_jsonrpc,
};
use slack_api::{SlackClient, SlackUpload, send_incoming_webhook};
use slack_api::{
    SlackEnvelopeDelivery, SlackEnvelopeDisposition, SlackSocketEnvelope, SlackSocketModeClient,
    SlackSocketModeError, SlackSocketReceiveOutcome,
};

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
const META_MESSAGE_TS: &str = "message_ts";
const META_CLIENT_MSG_ID: &str = "client_msg_id";
const META_STATUS_KIND: &str = "status_kind";
const META_REASON_CODE: &str = "reason_code";
const REJECT_CHANNEL_TARGET_DENIED: &str = "channel_target_denied";

const PLATFORM_SLACK: &str = "slack";
const MODE_INCOMING_WEBHOOK: &str = "incoming_webhook";
const DELIVERY_MODE_CHAT_POST_MESSAGE: &str = "chat.postMessage";
const TRANSPORT_EVENTS_WEBHOOK: &str = "events_webhook";
const TRANSPORT_SOCKET_MODE: &str = "socket_mode";
const THINKING_REACTION: &str = "eyes";
const MAX_SIGNATURE_AGE_SECS: i64 = 300;
const MAX_SLACK_CHANNEL_ID_BYTES: usize = 512;

const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const SLACK_STATUS_TEXT: &str = "slack_status_text";

#[derive(Debug)]
struct ChannelTargetDenied(String);

impl std::fmt::Display for ChannelTargetDenied {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ChannelTargetDenied {}

fn channel_target_denied(reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ChannelTargetDenied(reason.into()))
}

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
            Err(error) => {
                let code = error
                    .downcast_ref::<ChannelTargetDenied>()
                    .map(|_| REJECT_CHANNEL_TARGET_DENIED)
                    .unwrap_or("internal_error");
                plugin_error(code, error.to_string())
            }
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
        PluginRequest::PollIngress { .. } => Ok(plugin_error(
            "supervised_ingress_required",
            "Slack Socket Mode requires start_ingress supervision for delivery-before-acknowledgement",
        )),
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
        PluginRequest::GetMessage { config, reference } => get_message(config, reference),
        PluginRequest::GetPermalink { config, reference } => get_permalink(config, reference),
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
    context: &IngressPollContext<'_>,
) -> Result<PluginResponse> {
    handle_poll_ingress(config, state.as_ref(), context)
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    validate_outbound_config(config)?;
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
    if !config.allowed_channel_ids.is_empty() {
        metadata.insert(
            "allowed_channel_count".to_string(),
            config.allowed_channel_ids.len().to_string(),
        );
    }
    if config.unrestricted_channel_access {
        metadata.insert(
            "unrestricted_channel_access".to_string(),
            "true".to_string(),
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
    let mut metadata = BTreeMap::new();
    if config.unrestricted_channel_access {
        metadata.insert(
            "unrestricted_channel_access".to_string(),
            "true".to_string(),
        );
    }
    ChannelPolicy {
        owner_id: config.owner_id.clone(),
        allowed_sender_ids: config.allowed_sender_ids.clone(),
        allowed_conversation_ids: config.allowed_channel_ids.clone(),
        allowed_workspace_ids: config.allowed_team_ids.clone(),
        allowed_outbound_conversation_ids: config.allowed_channel_ids.clone(),
        activation: Some(config.activation.as_str().to_string()),
        thread_policy: None,
        allowed_thread_ids: Vec::new(),
        dm_policy: Some(config.dm_policy.as_str().to_string()),
        allowed_dm_sender_ids: config.allowed_sender_ids.clone(),
        reply_delivery: Some("tool_owned".to_string()),
        require_signature_validation: Some(true),
        allow_group_messages: None,
        max_attachment_bytes: None,
        metadata,
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
    validate_requested_channel_consistency(message)?;
    if has_optional_env(bot_token_env(config)) {
        let channel_id = resolve_channel_id(config, message)?;
        authorize_channel_id(config, &channel_id)?;
        let upload = slack_upload(message)?;
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
            reference: MessageRef {
                conversation_id: posted.channel_id.clone(),
                message_id: posted.message_id.clone(),
                thread_id: posted.thread_ts.clone(),
            },
            metadata,
        });
    }

    let webhook_url = resolved_incoming_webhook_url(config).ok_or_else(|| {
        anyhow!("slack delivery requires a bot token or a configured incoming webhook URL")
    })?;
    authorize_webhook_delivery(config, message)?;
    let upload = slack_upload(message)?;
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
        reference: MessageRef {
            conversation_id: posted.channel_id,
            message_id: posted.message_id,
            thread_id: None,
        },
        metadata,
    })
}

/// Fetch a message for an authorized receipt-bound reference.
fn get_message(config: &ChannelConfig, reference: &MessageRef) -> Result<PluginResponse> {
    let channel_id = authorize_read_back(config, reference)?;
    let client = SlackClient::from_env(bot_token_env(config))?;
    let Some(fetched) = client.fetch_message(
        &channel_id,
        &reference.message_id,
        reference.thread_id.as_deref(),
    )?
    else {
        return Ok(PluginResponse::MessageNotFound {
            reference: reference.clone(),
        });
    };
    let permalink = client.message_permalink(&channel_id, &reference.message_id)?;
    let author = fetched.author_id.clone().map(|id| FetchedMessageAuthor {
        id,
        display_name: None,
        username: fetched.author_username.clone(),
        is_bot: fetched.author_is_bot,
    });
    Ok(PluginResponse::MessageFetched {
        message: FetchedMessage {
            reference: MessageRef {
                conversation_id: channel_id,
                message_id: fetched.message_id,
                thread_id: fetched.thread_id,
            },
            content: fetched.content,
            content_type: Some("text/plain".to_string()),
            author,
            permalink,
            metadata: BTreeMap::new(),
        },
    })
}

/// Resolve a permalink for an authorized receipt-bound reference.
fn get_permalink(config: &ChannelConfig, reference: &MessageRef) -> Result<PluginResponse> {
    let channel_id = authorize_read_back(config, reference)?;
    let client = SlackClient::from_env(bot_token_env(config))?;
    match client.message_permalink(&channel_id, &reference.message_id)? {
        Some(url) => Ok(PluginResponse::PermalinkResolved {
            permalink: MessagePermalink {
                reference: MessageRef {
                    conversation_id: channel_id,
                    message_id: reference.message_id.clone(),
                    thread_id: reference.thread_id.clone(),
                },
                url,
            },
        }),
        None => Ok(PluginResponse::MessageNotFound {
            reference: reference.clone(),
        }),
    }
}

/// Authorize the referenced conversation and require bot-token transport.
fn authorize_read_back(config: &ChannelConfig, reference: &MessageRef) -> Result<String> {
    if reference.message_id.trim().is_empty() {
        return Err(channel_target_denied(
            "Slack read-back requires a message id",
        ));
    }
    // Authorize scope before checking transport credentials.
    let channel_id = reference.conversation_id.clone();
    authorize_channel_id(config, &channel_id)?;
    if !has_optional_env(bot_token_env(config)) {
        return Err(anyhow!(
            "slack read-back requires a bot token; incoming-webhook delivery cannot be read back"
        ));
    }
    Ok(channel_id)
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let content = render_status_message(update);
    if content.trim().is_empty() {
        return Ok(rejected_status(
            "missing_message",
            "slack status frames require a message or slack_status_text override",
        ));
    }

    let mut message_metadata = BTreeMap::new();
    if let Some(conversation_id) = update.metadata.get(ROUTE_CONVERSATION_ID) {
        message_metadata.insert(ROUTE_CONVERSATION_ID.to_string(), conversation_id.clone());
    }
    if let Some(thread_id) = update.metadata.get(ROUTE_THREAD_ID) {
        message_metadata.insert(ROUTE_THREAD_ID.to_string(), thread_id.clone());
    }
    let message = OutboundMessage {
        content,
        attachments: Vec::new(),
        channel_id: update.conversation_id.clone(),
        thread_ts: update.thread_id.clone(),
        destination_url: None,
        metadata: message_metadata,
    };
    let delivery = match deliver(config, &message) {
        Ok(delivery) => delivery,
        Err(error) => {
            let reason_code = error
                .downcast_ref::<ChannelTargetDenied>()
                .map(|_| REJECT_CHANNEL_TARGET_DENIED)
                .unwrap_or("delivery_failed");
            return Ok(rejected_status(reason_code, error.to_string()));
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
            if !team_is_allowed(config, envelope.team_id.as_deref()) {
                return Ok(PluginResponse::IngressEventsReceived {
                    events: Vec::new(),
                    callback_reply: None,
                    state: None,
                    poll_after_ms: None,
                });
            }
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
    context: &IngressPollContext<'_>,
) -> Result<PluginResponse> {
    if !has_optional_env(app_token_env(config)) {
        return Ok(plugin_error(
            "polling_not_supported",
            "slack poll_ingress requires Slack Socket Mode with SLACK_APP_TOKEN",
        ));
    }

    let client = SlackSocketModeClient::from_env(app_token_env(config))?;
    let next_state = Some(polling_state(config, "running", state)?);

    // Supervised polling writes each event before acknowledging its envelope.
    let deliver = {
        let next_state = next_state.clone();
        move |envelope: &SlackSocketEnvelope| -> Result<SlackEnvelopeDisposition> {
            let Some(inbound_event) = build_socket_mode_event(config, state, envelope)? else {
                return Ok(SlackEnvelopeDisposition::Ignored);
            };
            if socket_mode_event_was_delivered(&inbound_event.event_id) {
                return Ok(SlackEnvelopeDisposition::Ignored);
            }
            let event_id = inbound_event.event_id.clone();
            context
                .deliver(vec![inbound_event], next_state.clone(), Some(1000))
                .map_err(slack_api::notification_delivery_error)?;
            record_socket_mode_event_delivered(&event_id);
            Ok(SlackEnvelopeDisposition::Delivered)
        }
    };
    let is_stopped = || context.is_stopped();

    let socket_envelope = match client.receive_event(
        poll_timeout_secs(config),
        Some(&deliver as SlackEnvelopeDelivery<'_>),
        Some(&is_stopped),
    ) {
        Ok(SlackSocketReceiveOutcome::Delivered) => {
            // Refresh state after in-cycle delivery and acknowledgement.
            return Ok(PluginResponse::IngressEventsReceived {
                events: Vec::new(),
                callback_reply: None,
                state: next_state,
                poll_after_ms: Some(1000),
            });
        }
        Ok(SlackSocketReceiveOutcome::Event(envelope)) => Some(envelope),
        Ok(SlackSocketReceiveOutcome::Timeout) => None,
        Ok(SlackSocketReceiveOutcome::Stopped) => {
            bail!("Slack Socket Mode polling stopped")
        }
        Ok(SlackSocketReceiveOutcome::Disconnected) => {
            if publish_socket_reconnecting(context, &next_state).is_err() {
                return Ok(socket_notification_delivery_error());
            }
            bail!("Slack Socket Mode disconnected before the poll deadline")
        }
        Err(SlackSocketModeError::Authentication { code }) => {
            return Ok(plugin_error(
                "slack_authentication_failed",
                format!("Slack rejected the configured Socket Mode credentials: {code}"),
            ));
        }
        Err(SlackSocketModeError::Configuration { code }) => {
            return Ok(plugin_error(
                "slack_socket_protocol_error",
                format!("Slack rejected the configured Socket Mode connection: {code}"),
            ));
        }
        Err(SlackSocketModeError::ConnectionResponse { code }) => {
            if publish_socket_reconnecting(context, &next_state).is_err() {
                return Ok(socket_notification_delivery_error());
            }
            bail!("Slack Socket Mode connection response failed: {code}")
        }
        Err(SlackSocketModeError::RateLimited { retry_after }) => {
            if publish_socket_reconnecting(context, &next_state).is_err() {
                return Ok(socket_notification_delivery_error());
            }
            context.sleep_until_stopped(retry_after);
            bail!("Slack rate limited the Socket Mode connection request")
        }
        Err(SlackSocketModeError::NotificationDelivery { .. }) => {
            return Ok(plugin_error(
                "notification_delivery_failed",
                "Slack could not flush the inbound event notification",
            ));
        }
        Err(SlackSocketModeError::Transport(error)) => {
            if publish_socket_reconnecting(context, &next_state).is_err() {
                return Ok(socket_notification_delivery_error());
            }
            return Err(error);
        }
    };

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

fn publish_socket_reconnecting(
    context: &IngressPollContext<'_>,
    running_state: &Option<IngressState>,
) -> std::result::Result<(), RuntimeError> {
    let mut reconnecting_state = running_state.clone();
    if let Some(state) = reconnecting_state.as_mut() {
        state.status = "reconnecting".to_string();
    }
    context.deliver(Vec::new(), reconnecting_state, Some(1000))
}

fn socket_notification_delivery_error() -> PluginResponse {
    plugin_error(
        "notification_delivery_failed",
        "Slack could not flush the recovery notification",
    )
}

type DeliveredEventHistory = (
    std::collections::HashSet<String>,
    std::collections::VecDeque<String>,
);

fn delivered_event_history() -> &'static Mutex<DeliveredEventHistory> {
    use std::collections::{HashSet, VecDeque};
    use std::sync::OnceLock;

    static DELIVERED: OnceLock<Mutex<DeliveredEventHistory>> = OnceLock::new();
    DELIVERED.get_or_init(|| Mutex::new((HashSet::new(), VecDeque::new())))
}

fn socket_mode_event_was_delivered(event_id: &str) -> bool {
    delivered_event_history()
        .lock()
        .map(|history| history.0.contains(event_id))
        .unwrap_or(false)
}

/// Record successful stdout delivery for Slack redelivery suppression.
fn record_socket_mode_event_delivered(event_id: &str) {
    const DELIVERED_EVENT_HISTORY_LIMIT: usize = 512;

    let Ok(mut delivered) = delivered_event_history().lock() else {
        // Preserve at-least-once delivery when local deduplication is unavailable.
        return;
    };
    let (seen, order) = &mut *delivered;
    if seen.insert(event_id.to_string()) {
        order.push_back(event_id.to_string());
    }
    if order.len() > DELIVERED_EVENT_HISTORY_LIMIT
        && let Some(evicted) = order.pop_front()
    {
        seen.remove(&evicted);
    }
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
    let Some(bot_user_id) = ingress_state
        .and_then(|state| state.metadata.get(META_BOT_USER_ID))
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(actor) = inbound_actor(event) else {
        return Ok(None);
    };
    if !sender_is_allowed(config, &actor.id, event.channel_type.as_deref()) {
        return Ok(None);
    }
    if !matches!(event.channel_type.as_deref(), Some("im"))
        && authorize_channel_id(config, channel_id).is_err()
    {
        return Ok(None);
    }

    let Some(message_ts) = event.ts.as_deref().and_then(unpadded_provider_ts) else {
        return Ok(None);
    };
    let message_id = message_ts.to_string();
    let received_at = received_at(payload.received_at.as_deref(), envelope.event_time)?;
    let thread_ts = match event.thread_ts.as_deref() {
        None => None,
        Some(thread_ts) => {
            let Some(thread_ts) = unpadded_provider_ts(thread_ts) else {
                return Ok(None);
            };
            Some(thread_ts)
        }
    };
    let thread_id = Some(thread_ts.unwrap_or(message_ts).to_string());
    let parent_message_id = thread_ts
        .filter(|thread_ts| *thread_ts != message_ts)
        .map(ToOwned::to_owned);
    let Some(activation) = inbound_activation(config, event, bot_user_id) else {
        return Ok(None);
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
    event_metadata.insert(META_MESSAGE_TS.to_string(), message_id.clone());
    if let Some(client_msg_id) = event
        .client_msg_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    {
        event_metadata.insert(META_CLIENT_MSG_ID.to_string(), client_msg_id.to_string());
    }

    if !team_is_allowed(config, envelope.team_id.as_deref()) {
        return Ok(None);
    }

    let inbound_event = InboundEventEnvelope {
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
            workspace_id: envelope.team_id.clone(),
            parent_conversation_id: None,
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
        account_id: Some(bot_user_id.clone()),
        activation: Some(activation),
        metadata: event_metadata,
    };

    acknowledge_inbound_event(config, channel_id, Some(message_ts));

    Ok(Some(inbound_event))
}

fn unpadded_provider_ts(value: &str) -> Option<&str> {
    (!value.is_empty() && value.trim() == value).then_some(value)
}

fn acknowledge_inbound_event(config: &ChannelConfig, channel_id: &str, message_ts: Option<&str>) {
    if !has_optional_env(bot_token_env(config)) {
        return;
    }

    acknowledge_inbound_event_with(config, channel_id, message_ts, |channel_id, message_ts| {
        let client = SlackClient::from_env(bot_token_env(config))?;
        client.add_reaction(channel_id, message_ts, THINKING_REACTION)
    });
}

fn acknowledge_inbound_event_with(
    config: &ChannelConfig,
    channel_id: &str,
    message_ts: Option<&str>,
    add_reaction: impl FnOnce(&str, &str) -> Result<()>,
) {
    if authorize_channel_id(config, channel_id).is_err() {
        return;
    }
    let result = message_ts
        .ok_or_else(|| anyhow!("Slack event is missing its message timestamp"))
        .and_then(|message_ts| add_reaction(channel_id, message_ts));
    if result.is_err() {
        eprintln!("slack inbound acknowledgement failed");
    }
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
        return Ok(Some(callback_reply(
            403,
            "slack request verification is unavailable",
        )));
    };
    if secret.trim().is_empty() {
        return Ok(Some(callback_reply(
            403,
            "slack request verification is unavailable",
        )));
    }

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
    let Ok(timestamp) = timestamp_header.parse::<i64>() else {
        return Ok(Some(callback_reply(
            403,
            "slack request timestamp header is invalid",
        )));
    };
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

    let Ok(signature) = hex::decode(signature_hex) else {
        return Ok(Some(callback_reply(
            403,
            "slack request signature header is invalid",
        )));
    };
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
    !config.allowed_team_ids.is_empty()
        && team_id
            .map(|team_id| {
                config
                    .allowed_team_ids
                    .iter()
                    .any(|allowed| allowed == team_id)
            })
            .unwrap_or(false)
}

fn sender_is_allowed(config: &ChannelConfig, sender_id: &str, channel_type: Option<&str>) -> bool {
    if !matches!(channel_type, Some("im")) {
        return true;
    }

    match config.dm_policy {
        SlackDirectMessagePolicy::Deny => false,
        SlackDirectMessagePolicy::Allowlist => {
            id_matches_allowlist(&config.allowed_sender_ids, sender_id)
        }
        SlackDirectMessagePolicy::Open => true,
    }
}

fn id_matches_allowlist(allowlist: &[String], value: &str) -> bool {
    allowlist.iter().any(|allowed| allowed == value)
}

fn conversation_kind(channel_type: Option<&str>) -> String {
    match channel_type {
        Some("im") => "dm".to_string(),
        Some("mpim") => "group_dm".to_string(),
        Some("group") => "private_channel".to_string(),
        Some("app_home") => "app_home".to_string(),
        Some(kind) => kind.to_string(),
        None => "channel".to_string(),
    }
}

fn inbound_activation(
    config: &ChannelConfig,
    event: &SlackEventPayload,
    bot_user_id: &str,
) -> Option<InboundActivation> {
    let reason = match event.event_type.as_str() {
        "app_mention" => InboundActivation::REASON_DIRECT_MENTION,
        "message" if matches!(event.channel_type.as_deref(), Some("im")) => {
            InboundActivation::REASON_DIRECT_MESSAGE
        }
        "message"
            if event.parent_user_id.as_deref() == Some(bot_user_id)
                && matches!(config.activation, SlackActivationPolicy::MentionOrReply) =>
        {
            InboundActivation::REASON_REPLY_TO_AGENT
        }
        "message" if matches!(config.activation, SlackActivationPolicy::AllMessages) => {
            InboundActivation::REASON_ALL_MESSAGES
        }
        _ => return None,
    };

    Some(InboundActivation {
        reason: reason.to_string(),
        agent_account_id: Some(bot_user_id.to_string()),
        referenced_message_author_id: (reason == InboundActivation::REASON_REPLY_TO_AGENT)
            .then(|| bot_user_id.to_string()),
    })
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
    validate_requested_channel_consistency(message)?;
    let message_channel_id = message.channel_id.as_deref();
    let metadata_channel_id = message
        .metadata
        .get(ROUTE_CONVERSATION_ID)
        .map(String::as_str);
    if let Some(channel_id) = message_channel_id.or(metadata_channel_id) {
        return Ok(channel_id.to_string());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        return Ok(default_channel_id.clone());
    }
    Err(channel_target_denied(
        "Slack delivery requires a channel target or configured default channel",
    ))
}

fn validate_requested_channel_consistency(message: &OutboundMessage) -> Result<()> {
    if let (Some(message_channel_id), Some(metadata_channel_id)) = (
        message.channel_id.as_deref(),
        message
            .metadata
            .get(ROUTE_CONVERSATION_ID)
            .map(String::as_str),
    ) && message_channel_id != metadata_channel_id
    {
        return Err(channel_target_denied(
            "Slack message and conversation metadata name conflicting channel targets",
        ));
    }
    Ok(())
}

fn validate_channel_id(channel_id: &str) -> Result<()> {
    if channel_id.is_empty()
        || channel_id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(channel_target_denied(
            "Slack channel ids must be non-empty and contain no whitespace or control characters",
        ));
    }
    if channel_id == "*" {
        return Err(channel_target_denied(
            "Slack channel ids do not accept wildcard targets; use unrestricted_channel_access",
        ));
    }
    if channel_id.len() > MAX_SLACK_CHANNEL_ID_BYTES {
        return Err(channel_target_denied(
            "Slack channel ids must not exceed 512 bytes",
        ));
    }
    Ok(())
}

fn validate_outbound_config(config: &ChannelConfig) -> Result<()> {
    for channel_id in &config.allowed_channel_ids {
        validate_channel_id(channel_id)?;
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        validate_channel_id(default_channel_id)?;
        if !config.unrestricted_channel_access
            && !config.allowed_channel_ids.is_empty()
            && !config
                .allowed_channel_ids
                .iter()
                .any(|id| id == default_channel_id)
        {
            return Err(channel_target_denied(
                "Slack default_channel_id must be included in allowed_channel_ids",
            ));
        }
    }
    if config.unrestricted_channel_access && !config.allowed_channel_ids.is_empty() {
        return Err(channel_target_denied(
            "Slack unrestricted_channel_access cannot be combined with allowed_channel_ids",
        ));
    }
    Ok(())
}

fn authorize_channel_id(config: &ChannelConfig, channel_id: &str) -> Result<()> {
    validate_outbound_config(config)?;
    validate_channel_id(channel_id)?;
    if config.unrestricted_channel_access
        || config.allowed_channel_ids.iter().any(|id| id == channel_id)
    {
        Ok(())
    } else {
        Err(channel_target_denied(
            "Slack channel target is outside the configured outbound allowlist",
        ))
    }
}

fn authorize_webhook_delivery(config: &ChannelConfig, message: &OutboundMessage) -> Result<()> {
    validate_outbound_config(config)?;
    if config.unrestricted_channel_access {
        return Ok(());
    }
    let configured_channel_id = config.default_channel_id.as_deref().or_else(|| {
        (config.allowed_channel_ids.len() == 1).then(|| config.allowed_channel_ids[0].as_str())
    });
    let Some(configured_channel_id) = configured_channel_id else {
        return Err(channel_target_denied(
            "Slack incoming webhook delivery requires a default_channel_id when more than one channel is allowed",
        ));
    };
    authorize_channel_id(config, configured_channel_id)?;
    let requested_channel_id = message.channel_id.as_deref().or_else(|| {
        message
            .metadata
            .get(ROUTE_CONVERSATION_ID)
            .map(String::as_str)
    });
    if requested_channel_id.is_some_and(|requested| requested != configured_channel_id) {
        return Err(channel_target_denied(
            "Slack incoming webhook delivery uses its configured default channel",
        ));
    }
    Ok(())
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
    parent_user_id: Option<String>,
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
    use std::cell::Cell;
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

    fn authorized_channel_config() -> ChannelConfig {
        ChannelConfig {
            allowed_team_ids: vec!["T123".to_string()],
            allowed_channel_ids: vec!["C123".to_string()],
            ..ChannelConfig::default()
        }
    }

    fn authenticated_ingress_state() -> IngressState {
        IngressState {
            mode: IngressMode::Polling,
            status: "running".to_string(),
            endpoint: None,
            metadata: BTreeMap::from([(META_BOT_USER_ID.to_string(), "UBOT123".to_string())]),
        }
    }

    #[test]
    fn url_verification_returns_challenge_reply() {
        let payload = base_payload(
            r#"{"type":"url_verification","challenge":"challenge-token","team_id":"T123"}"#,
        );

        let config = authorized_channel_config();
        let ingress_state = authenticated_ingress_state();
        let response =
            handle_ingress_event(&config, Some(&ingress_state), &payload).expect("handle ingress");

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
    fn a_redelivered_event_is_not_handed_to_the_host_twice() {
        assert!(!socket_mode_event_was_delivered("Ev-dedupe-first"));
        record_socket_mode_event_delivered("Ev-dedupe-first");
        assert!(socket_mode_event_was_delivered("Ev-dedupe-first"));
        assert!(!socket_mode_event_was_delivered("Ev-dedupe-second"));
    }

    #[test]
    fn url_verification_is_not_blocked_by_the_event_team_allowlist() {
        let payload = base_payload(
            r#"{"type":"url_verification","challenge":"challenge-token","team_id":"T123"}"#,
        );
        let config = ChannelConfig {
            allowed_team_ids: vec!["T456".to_string()],
            ..Default::default()
        };

        let response = handle_ingress_event(&config, None, &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { callback_reply, .. } => {
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
                    "client_msg_id":"client-generated-id",
                    "ts":"1712860000.100200",
                    "event_ts":"1712860000.100200",
                    "thread_ts":"1712860000.000001"
                }
            }"#,
        );

        let config = authorized_channel_config();
        let ingress_state = authenticated_ingress_state();
        let response =
            handle_ingress_event(&config, Some(&ingress_state), &payload).expect("handle ingress");

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
                assert_eq!(event.account_id.as_deref(), Some("UBOT123"));
                assert_eq!(event.conversation.id, "C123");
                assert_eq!(event.conversation.kind, "channel");
                assert_eq!(
                    event.conversation.thread_id.as_deref(),
                    Some("1712860000.000001")
                );
                assert_eq!(
                    event.conversation.parent_message_id.as_deref(),
                    Some("1712860000.000001")
                );
                assert_eq!(event.actor.id, "U123");
                assert_eq!(event.message.id, "1712860000.100200");
                assert_eq!(event.message.content, "hello from slack");
                assert_eq!(
                    event.message.reply_to_message_id.as_deref(),
                    Some("1712860000.000001")
                );
                assert_eq!(
                    event
                        .activation
                        .as_ref()
                        .map(|activation| activation.reason.as_str()),
                    Some(InboundActivation::REASON_DIRECT_MENTION)
                );
                assert_eq!(
                    event.metadata.get(META_TRANSPORT).map(String::as_str),
                    Some(TRANSPORT_EVENTS_WEBHOOK)
                );
                assert_eq!(
                    event.metadata.get(META_ENDPOINT_ID).map(String::as_str),
                    Some("slack-events")
                );
                assert_eq!(
                    event.metadata.get(META_CLIENT_MSG_ID).map(String::as_str),
                    Some("client-generated-id")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn root_app_mention_uses_ts_as_message_id_and_thread_target() {
        let payload = base_payload(
            r#"{
                "type":"event_callback",
                "team_id":"T123",
                "api_app_id":"A123",
                "event_time":1712860000,
                "event":{
                    "type":"app_mention",
                    "channel":"C123",
                    "channel_type":"channel",
                    "user":"U123",
                    "text":"hello from slack",
                    "client_msg_id":"client-generated-id",
                    "ts":"1712860000.100200",
                    "event_ts":"1712860000.100201"
                }
            }"#,
        );

        let config = authorized_channel_config();
        let ingress_state = authenticated_ingress_state();
        let response =
            handle_ingress_event(&config, Some(&ingress_state), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => {
                assert_eq!(events.len(), 1);
                let event = &events[0];
                assert_eq!(event.event_id, "slack:C123:1712860000.100201");
                assert_eq!(event.message.id, "1712860000.100200");
                assert_eq!(
                    event.conversation.thread_id.as_deref(),
                    Some("1712860000.100200")
                );
                assert!(event.conversation.parent_message_id.is_none());
                assert!(event.message.reply_to_message_id.is_none());
                assert_eq!(
                    event.metadata.get(META_CLIENT_MSG_ID).map(String::as_str),
                    Some("client-generated-id")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn socket_mode_event_envelope(payload: serde_json::Value) -> SlackSocketEnvelope {
        SlackSocketEnvelope {
            envelope_type: "events_api".to_string(),
            envelope_id: Some("socket-env".to_string()),
            payload: Some(payload),
        }
    }

    #[test]
    fn socket_mode_uses_the_same_inbound_mapping_as_webhook() {
        let config = authorized_channel_config();
        let ingress_state = authenticated_ingress_state();

        let root = build_socket_mode_event(
            &config,
            Some(&ingress_state),
            &socket_mode_event_envelope(serde_json::json!({
                "type": "event_callback",
                "team_id": "T123",
                "event": {
                    "type": "app_mention",
                    "channel": "C123",
                    "channel_type": "channel",
                    "user": "U123",
                    "text": "hello from slack",
                    "client_msg_id": "socket-client-generated-id",
                    "ts": "1712860000.100200",
                    "event_ts": "1712860000.100201"
                }
            })),
        )
        .expect("root socket event")
        .expect("accepted root socket event");
        assert_eq!(root.event_id, "slack:C123:1712860000.100201");
        assert_eq!(root.message.id, "1712860000.100200");
        assert_eq!(
            root.conversation.thread_id.as_deref(),
            Some("1712860000.100200")
        );
        assert!(root.conversation.parent_message_id.is_none());
        assert!(root.message.reply_to_message_id.is_none());
        assert_eq!(
            root.metadata.get(META_CLIENT_MSG_ID).map(String::as_str),
            Some("socket-client-generated-id")
        );

        let reply = build_socket_mode_event(
            &config,
            Some(&ingress_state),
            &socket_mode_event_envelope(serde_json::json!({
                "type": "event_callback",
                "team_id": "T123",
                "event_id": "EvSocketThread",
                "event": {
                    "type": "app_mention",
                    "channel": "C123",
                    "channel_type": "channel",
                    "user": "U123",
                    "text": "hello in thread",
                    "client_msg_id": "socket-thread-client-id",
                    "ts": "1712860000.100300",
                    "event_ts": "1712860000.100300",
                    "thread_ts": "1712860000.000001"
                }
            })),
        )
        .expect("thread socket event")
        .expect("accepted thread socket event");
        assert_eq!(reply.event_id, "EvSocketThread");
        assert_eq!(reply.message.id, "1712860000.100300");
        assert_eq!(
            reply.conversation.thread_id.as_deref(),
            Some("1712860000.000001")
        );
        assert_eq!(
            reply.conversation.parent_message_id.as_deref(),
            Some("1712860000.000001")
        );
        assert_eq!(
            reply.message.reply_to_message_id.as_deref(),
            Some("1712860000.000001")
        );
        assert_eq!(
            reply.metadata.get(META_CLIENT_MSG_ID).map(String::as_str),
            Some("socket-thread-client-id")
        );

        let padded = build_socket_mode_event(
            &config,
            Some(&ingress_state),
            &socket_mode_event_envelope(serde_json::json!({
                "type": "event_callback",
                "team_id": "T123",
                "event_id": "EvSocketPadded",
                "event": {
                    "type": "app_mention",
                    "channel": "C123",
                    "channel_type": "channel",
                    "user": "U123",
                    "text": "hello from slack",
                    "ts": "1712860000.100200 "
                }
            })),
        )
        .expect("padded socket event");
        assert!(padded.is_none());
    }

    #[test]
    fn inbound_event_requires_provider_message_coordinates() {
        let config = authorized_channel_config();
        let ingress_state = authenticated_ingress_state();
        let cases = [
            (
                "missing message timestamp",
                r#"{
                    "type":"event_callback",
                    "team_id":"T123",
                    "event_id":"EvMissingTs",
                    "event":{
                        "type":"app_mention",
                        "channel":"C123",
                        "channel_type":"channel",
                        "user":"U123",
                        "text":"hello from slack",
                        "client_msg_id":"client-generated-id"
                    }
                }"#,
            ),
            (
                "empty message timestamp",
                r#"{
                    "type":"event_callback",
                    "team_id":"T123",
                    "event_id":"EvEmptyTs",
                    "event":{
                        "type":"app_mention",
                        "channel":"C123",
                        "channel_type":"channel",
                        "user":"U123",
                        "text":"hello from slack",
                        "ts":""
                    }
                }"#,
            ),
            (
                "blank message timestamp",
                r#"{
                    "type":"event_callback",
                    "team_id":"T123",
                    "event_id":"EvBlankTs",
                    "event":{
                        "type":"app_mention",
                        "channel":"C123",
                        "channel_type":"channel",
                        "user":"U123",
                        "text":"hello from slack",
                        "client_msg_id":"client-generated-id",
                        "ts":"   "
                    }
                }"#,
            ),
            (
                "padded message timestamp",
                r#"{
                    "type":"event_callback",
                    "team_id":"T123",
                    "event_id":"EvPaddedTs",
                    "event":{
                        "type":"app_mention",
                        "channel":"C123",
                        "channel_type":"channel",
                        "user":"U123",
                        "text":"hello from slack",
                        "ts":" 1712860000.100200"
                    }
                }"#,
            ),
            (
                "blank thread timestamp",
                r#"{
                    "type":"event_callback",
                    "team_id":"T123",
                    "event_id":"EvBlankThreadTs",
                    "event":{
                        "type":"app_mention",
                        "channel":"C123",
                        "channel_type":"channel",
                        "user":"U123",
                        "text":"hello from slack",
                        "ts":"1712860000.100200",
                        "thread_ts":"   "
                    }
                }"#,
            ),
            (
                "padded thread timestamp",
                r#"{
                    "type":"event_callback",
                    "team_id":"T123",
                    "event_id":"EvPaddedThreadTs",
                    "event":{
                        "type":"app_mention",
                        "channel":"C123",
                        "channel_type":"channel",
                        "user":"U123",
                        "text":"hello from slack",
                        "ts":"1712860000.100200",
                        "thread_ts":"1712860000.000001 "
                    }
                }"#,
            ),
        ];

        for (case, body) in cases {
            let payload = base_payload(body);
            let response = handle_ingress_event(&config, Some(&ingress_state), &payload)
                .expect("handle ingress");

            match response {
                PluginResponse::IngressEventsReceived { events, .. } => {
                    assert!(events.is_empty(), "{case}")
                }
                other => panic!("unexpected response for {case}: {other:?}"),
            }
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

        let config = authorized_channel_config();
        let ingress_state = authenticated_ingress_state();
        let response =
            handle_ingress_event(&config, Some(&ingress_state), &payload).expect("handle ingress");

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
                    "parent_user_id":"UBOT123",
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

        let config = authorized_channel_config();
        let ingress_state = authenticated_ingress_state();
        let response =
            handle_ingress_event(&config, Some(&ingress_state), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => {
                assert_eq!(events.len(), 1);
                let event = &events[0];
                assert_eq!(
                    event.message.reply_to_message_id.as_deref(),
                    Some("1712860000.000001")
                );
                assert_eq!(
                    event.conversation.thread_id.as_deref(),
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
    fn direct_message_is_dropped_when_allowlist_has_no_sender_match() {
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
                allowed_team_ids: vec!["T123".to_string()],
                dm_policy: SlackDirectMessagePolicy::Allowlist,
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
    fn an_operator_command_does_not_widen_channel_activation() {
        let unmentioned: SlackEventPayload = serde_json::from_value(serde_json::json!({
            "type": "message",
            "text": "stop posting",
            "channel_type": "channel"
        }))
        .expect("operator command event");
        assert!(inbound_activation(&ChannelConfig::default(), &unmentioned, "UBOT123").is_none());

        let mentioned: SlackEventPayload = serde_json::from_value(serde_json::json!({
            "type": "app_mention",
            "text": "<@UBOT123> stop posting",
            "channel_type": "channel"
        }))
        .expect("mentioned operator command event");
        let activation = inbound_activation(&ChannelConfig::default(), &mentioned, "UBOT123")
            .expect("a mentioned command carries mention provenance");
        assert_eq!(activation.reason, InboundActivation::REASON_DIRECT_MENTION);
        assert_eq!(activation.agent_account_id.as_deref(), Some("UBOT123"));
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
    fn resolve_channel_id_rejects_conflicting_message_and_metadata_targets() {
        let message = OutboundMessage {
            content: "reply".to_string(),
            attachments: Vec::new(),
            channel_id: Some("C123".to_string()),
            thread_ts: None,
            destination_url: None,
            metadata: BTreeMap::from([(ROUTE_CONVERSATION_ID.to_string(), "C456".to_string())]),
        };

        let error = resolve_channel_id(&ChannelConfig::default(), &message)
            .expect_err("conflicting targets must be rejected");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
    }

    #[test]
    fn empty_allowlist_denies_outbound_channel() {
        let error = authorize_channel_id(&ChannelConfig::default(), "C123")
            .expect_err("empty allowlist must deny");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
    }

    #[test]
    fn read_back_denies_channel_outside_allowlist_before_any_slack_call() {
        let config = ChannelConfig {
            bot_token_env: Some("SLACK_BOT_TOKEN".to_string()),
            allowed_channel_ids: vec!["C0BJSQDLURY".to_string()],
            ..ChannelConfig::default()
        };
        let reference = MessageRef {
            conversation_id: "C04UZHV2U5Q".to_string(),
            message_id: "1712860000.000001".to_string(),
            thread_id: None,
        };
        let error = authorize_read_back(&config, &reference)
            .expect_err("channel outside the allowlist must be denied");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
    }

    #[test]
    fn read_back_requires_a_message_id() {
        let config = ChannelConfig {
            bot_token_env: Some("SLACK_BOT_TOKEN".to_string()),
            allowed_channel_ids: vec!["C0BJSQDLURY".to_string()],
            ..ChannelConfig::default()
        };
        let reference = MessageRef {
            conversation_id: "C0BJSQDLURY".to_string(),
            message_id: "   ".to_string(),
            thread_id: None,
        };
        let error =
            authorize_read_back(&config, &reference).expect_err("blank message id must be denied");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
    }

    #[test]
    fn read_back_requires_a_bot_token() {
        let config = ChannelConfig {
            bot_token_env: Some("SLACK_BOT_TOKEN_UNSET_FOR_READ_BACK_TEST".to_string()),
            unrestricted_channel_access: true,
            ..ChannelConfig::default()
        };
        let reference = MessageRef {
            conversation_id: "C0BJSQDLURY".to_string(),
            message_id: "1712860000.000001".to_string(),
            thread_id: None,
        };
        let error = authorize_read_back(&config, &reference)
            .expect_err("read-back without a bot token must fail");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_none());
    }

    #[test]
    fn unrestricted_mode_requires_no_allowlist_and_allows_outbound_channel() {
        authorize_channel_id(
            &ChannelConfig {
                unrestricted_channel_access: true,
                ..ChannelConfig::default()
            },
            "C123",
        )
        .expect("explicit unrestricted access");

        let error = validate_outbound_config(&ChannelConfig {
            unrestricted_channel_access: true,
            allowed_channel_ids: vec!["C123".to_string()],
            ..ChannelConfig::default()
        })
        .expect_err("ambiguous target policy");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
    }

    #[test]
    fn default_channel_must_be_allowlisted() {
        let error = validate_outbound_config(&ChannelConfig {
            default_channel_id: Some("C999".to_string()),
            allowed_channel_ids: vec!["C123".to_string()],
            ..ChannelConfig::default()
        })
        .expect_err("default outside the allowlist");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
    }

    #[test]
    fn channel_ids_reject_control_characters() {
        for channel_id in ["C123\nC456", "C123\0C456"] {
            let error = validate_channel_id(channel_id).expect_err("control character");
            assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
        }
    }

    #[test]
    fn channel_id_length_matches_the_gateway_boundary() {
        validate_channel_id(&"C".repeat(MAX_SLACK_CHANNEL_ID_BYTES)).expect("512-byte channel id");
        let error = validate_channel_id(&"C".repeat(MAX_SLACK_CHANNEL_ID_BYTES + 1))
            .expect_err("513-byte channel id");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
    }

    #[test]
    fn inbound_acknowledgement_authorizes_before_reaction() {
        let config = ChannelConfig {
            allowed_channel_ids: vec!["C123".to_string()],
            ..ChannelConfig::default()
        };
        let denied_called = Cell::new(false);
        acknowledge_inbound_event_with(&config, "C999", Some("1712860000.100200"), |_, _| {
            denied_called.set(true);
            Ok(())
        });
        assert!(!denied_called.get());

        let allowed_called = Cell::new(false);
        acknowledge_inbound_event_with(&config, "C123", Some("1712860000.100200"), |_, _| {
            allowed_called.set(true);
            Ok(())
        });
        assert!(allowed_called.get());
    }

    #[test]
    fn denied_webhook_target_is_rejected_before_network_call() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let webhook_url = format!(
            "http://{}/services/configured",
            listener.local_addr().expect("address")
        );

        let error = deliver(
            &ChannelConfig {
                incoming_webhook_url: Some(webhook_url),
                default_channel_id: Some("C123".to_string()),
                allowed_channel_ids: vec!["C123".to_string()],
                ..ChannelConfig::default()
            },
            &OutboundMessage {
                content: "should not send".to_string(),
                attachments: Vec::new(),
                channel_id: Some("C999".to_string()),
                thread_ts: None,
                destination_url: None,
                metadata: BTreeMap::new(),
            },
        )
        .expect_err("unauthorized target");

        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
        assert!(
            listener.accept().is_err(),
            "denied delivery made a network call"
        );
    }

    #[test]
    fn push_uses_the_same_channel_target_authorizer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let webhook_url = format!(
            "http://{}/services/configured",
            listener.local_addr().expect("address")
        );
        let envelope = PluginRequestEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Push {
                config: ChannelConfig {
                    incoming_webhook_url: Some(webhook_url),
                    default_channel_id: Some("C123".to_string()),
                    allowed_channel_ids: vec!["C123".to_string()],
                    ..ChannelConfig::default()
                },
                message: OutboundMessage {
                    content: "should not send".to_string(),
                    attachments: Vec::new(),
                    channel_id: Some("C999".to_string()),
                    thread_ts: None,
                    destination_url: None,
                    metadata: BTreeMap::new(),
                },
            },
        };

        let error = handle_request(&envelope, &Arc::new(Mutex::new(())), &mut None)
            .expect_err("unauthorized Push target");
        assert!(error.downcast_ref::<ChannelTargetDenied>().is_some());
        assert!(
            listener.accept().is_err(),
            "denied Push made a network call"
        );
    }

    #[test]
    fn status_uses_the_same_channel_target_authorizer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let webhook_url = format!(
            "http://{}/services/configured",
            listener.local_addr().expect("address")
        );
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "hello".to_string(),
            conversation_id: Some("C999".to_string()),
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        let acceptance = send_status(
            &ChannelConfig {
                incoming_webhook_url: Some(webhook_url),
                allowed_channel_ids: vec!["C123".to_string()],
                default_channel_id: Some("C123".to_string()),
                ..ChannelConfig::default()
            },
            &update,
        )
        .expect("status response");

        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some(REJECT_CHANNEL_TARGET_DENIED)
        );
    }

    #[test]
    fn status_rejects_conflicting_top_level_and_metadata_channels() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "hello".to_string(),
            conversation_id: Some("C123".to_string()),
            thread_id: None,
            metadata: BTreeMap::from([(ROUTE_CONVERSATION_ID.to_string(), "C456".to_string())]),
        };

        let acceptance = send_status(&ChannelConfig::default(), &update).expect("status response");
        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some(REJECT_CHANNEL_TARGET_DENIED)
        );
    }

    #[test]
    fn channel_policy_projects_channel_ids_not_team_ids() {
        let policy = channel_policy(&ChannelConfig {
            allowed_team_ids: vec!["T123".to_string()],
            allowed_channel_ids: vec!["C123".to_string()],
            ..ChannelConfig::default()
        });

        assert_eq!(policy.allowed_conversation_ids, vec!["C123"]);
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
                allowed_channel_ids: vec!["C123".to_string()],
                ..ChannelConfig::default()
            },
            &OutboundMessage {
                content: "hello from incoming webhook".to_string(),
                attachments: Vec::new(),
                channel_id: None,
                thread_ts: None,
                destination_url: Some(
                    "https://caller-controlled.invalid/services/other".to_string(),
                ),
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
