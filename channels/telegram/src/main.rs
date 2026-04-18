use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

mod protocol;
mod telegram_api;

use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ChannelPolicy, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressCallbackReply, IngressMode, IngressPayload,
    IngressState, OutboundAttachment, OutboundMessage, PluginRequest, PluginRequestEnvelope,
    PluginResponse, StatusAcceptance, StatusFrame, StatusKind, capabilities, plugin_error,
};
use telegram_api::{TelegramClient, TelegramMediaSource, TelegramUpload};

const META_BOT_TOKEN_ENV: &str = "bot_token_env";
const META_DEFAULT_CHAT_ID: &str = "default_chat_id";
const META_DEFAULT_MESSAGE_THREAD_ID: &str = "default_message_thread_id";
const META_ALLOWED_CHAT_COUNT: &str = "allowed_chat_count";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_WEBHOOK_ENDPOINT: &str = "webhook_endpoint";
const META_WEBHOOK_SECRET_ENV: &str = "webhook_secret_env";
const META_POLL_TIMEOUT_SECS: &str = "poll_timeout_secs";
const META_PLATFORM: &str = "platform";
const META_DROP_PENDING_UPDATES: &str = "drop_pending_updates";
const META_CHAT_ID: &str = "chat_id";
const META_MESSAGE_THREAD_ID: &str = "message_thread_id";
const META_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const META_DELIVERY_METHOD: &str = "delivery_method";
const META_ACTIVITY: &str = "activity";
const META_STATUS_KIND: &str = "status_kind";
const META_TELEGRAM_CONTENT_ORIGIN: &str = "telegram_content_origin";
const META_TELEGRAM_UPDATE_ID: &str = "telegram_update_id";
const META_TRANSPORT: &str = "transport";
const META_ENDPOINT_ID: &str = "endpoint_id";
const META_PATH: &str = "path";
const META_ATTACHMENT_COUNT: &str = "attachment_count";
const META_REASON_CODE: &str = "reason_code";
const META_NEXT_UPDATE_ID: &str = "next_update_id";

const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const ROUTE_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";

const PLATFORM_TELEGRAM: &str = "telegram";
const CONTENT_ORIGIN_TEXT: &str = "text";
const CONTENT_ORIGIN_CAPTION: &str = "caption";
const CONTENT_ORIGIN_EMPTY: &str = "empty";
const TRANSPORT_WEBHOOK: &str = "webhook";
const TRANSPORT_POLLING: &str = "polling";

fn main() -> Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let envelope: PluginRequestEnvelope =
            serde_json::from_str(&line).context("failed to parse channel request")?;

        let response = match handle_request(&envelope) {
            Ok(response) => response,
            Err(error) => plugin_error("internal_error", error.to_string()),
        };

        let json = serde_json::to_string(&response)?;
        writeln!(stdout, "{json}")?;
        stdout.flush()?;
    }

    Ok(())
}

fn handle_request(envelope: &PluginRequestEnvelope) -> Result<PluginResponse> {
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
        PluginRequest::StartIngress { config } => Ok(PluginResponse::IngressStarted {
            state: start_ingress(config)?,
        }),
        PluginRequest::StopIngress { config, state } => Ok(PluginResponse::IngressStopped {
            state: stop_ingress(config, state.clone())?,
        }),
        PluginRequest::PollIngress { config, state } => poll_ingress(config, state.clone()),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::IngressEvent { config, payload } => handle_ingress_event(config, payload),
        PluginRequest::Status { config, update } => Ok(PluginResponse::StatusAccepted {
            status: send_status(config, update)?,
        }),
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let bot_token_env = bot_token_env(config);
    read_required_env(bot_token_env)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_BOT_TOKEN_ENV.to_string(), bot_token_env.to_string());
    if let Some(default_chat_id) = &config.default_chat_id {
        metadata.insert(META_DEFAULT_CHAT_ID.to_string(), default_chat_id.clone());
    }
    if let Some(default_message_thread_id) = config.default_message_thread_id {
        metadata.insert(
            META_DEFAULT_MESSAGE_THREAD_ID.to_string(),
            default_message_thread_id.to_string(),
        );
    }
    if !config.allowed_chat_ids.is_empty() {
        metadata.insert(
            META_ALLOWED_CHAT_COUNT.to_string(),
            config.allowed_chat_ids.len().to_string(),
        );
    }
    let ingress_mode = selected_ingress_mode(config)?;
    metadata.insert(
        META_INGRESS_MODE.to_string(),
        ingress_mode_name(ingress_mode.clone()),
    );
    if let Some(endpoint) = resolved_endpoint(config) {
        metadata.insert(META_WEBHOOK_ENDPOINT.to_string(), endpoint);
        if has_optional_env(webhook_secret_env(config)) {
            metadata.insert(
                META_WEBHOOK_SECRET_ENV.to_string(),
                webhook_secret_env(config).to_string(),
            );
        }
    }
    if matches!(ingress_mode, IngressMode::Polling) {
        metadata.insert(
            META_POLL_TIMEOUT_SECS.to_string(),
            poll_timeout_secs(config).to_string(),
        );
    }

    Ok(ConfiguredChannel {
        metadata,
        policy: Some(channel_policy(config)),
        runtime: None,
    })
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let client = TelegramClient::from_env(bot_token_env(config))?;
    let identity = client.identity()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_TELEGRAM.to_string());
    if let Some(default_chat_id) = &config.default_chat_id {
        metadata.insert(META_DEFAULT_CHAT_ID.to_string(), default_chat_id.clone());
    }

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: Some(identity.id),
        display_name: Some(identity.username.unwrap_or(identity.first_name)),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    let client = TelegramClient::from_env(bot_token_env(config))?;
    match selected_ingress_mode(config)? {
        IngressMode::Webhook => {
            let endpoint = resolved_endpoint(config).ok_or_else(|| {
                anyhow!(
                    "telegram ingress requires webhook_public_url to register the Bot API webhook"
                )
            })?;
            let secret = std::env::var(webhook_secret_env(config)).ok();
            client.set_webhook(&endpoint, secret.as_deref(), config.drop_pending_updates)?;

            let mut metadata = BTreeMap::new();
            metadata.insert(META_PLATFORM.to_string(), PLATFORM_TELEGRAM.to_string());
            metadata.insert(
                META_DROP_PENDING_UPDATES.to_string(),
                config.drop_pending_updates.to_string(),
            );
            if secret.is_some() {
                metadata.insert(
                    META_WEBHOOK_SECRET_ENV.to_string(),
                    webhook_secret_env(config).to_string(),
                );
            }

            Ok(IngressState {
                mode: IngressMode::Webhook,
                status: "registered".to_string(),
                endpoint: Some(endpoint),
                metadata,
            })
        }
        IngressMode::Polling => {
            client.delete_webhook(config.drop_pending_updates)?;
            Ok(polling_state(config, None))
        }
        other => Err(anyhow!("telegram ingress mode {other:?} is not supported")),
    }
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    let ingress_mode = match state.as_ref() {
        Some(state) => state.mode.clone(),
        None => selected_ingress_mode(config)?,
    };

    match ingress_mode {
        IngressMode::Webhook => {
            let client = TelegramClient::from_env(bot_token_env(config))?;
            client.delete_webhook(config.drop_pending_updates)?;

            let mut stopped = state.unwrap_or(IngressState {
                mode: IngressMode::Webhook,
                status: "registered".to_string(),
                endpoint: resolved_endpoint(config),
                metadata: BTreeMap::new(),
            });
            stopped.status = "stopped".to_string();
            stopped
                .metadata
                .insert(META_PLATFORM.to_string(), PLATFORM_TELEGRAM.to_string());
            Ok(stopped)
        }
        IngressMode::Polling => {
            let mut stopped = state.unwrap_or_else(|| polling_state(config, None));
            stopped.status = "stopped".to_string();
            stopped
                .metadata
                .insert(META_PLATFORM.to_string(), PLATFORM_TELEGRAM.to_string());
            Ok(stopped)
        }
        other => Err(anyhow!("telegram ingress mode {other:?} is not supported")),
    }
}

fn poll_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<PluginResponse> {
    let ingress_mode = match state.as_ref() {
        Some(state) => state.mode.clone(),
        None => selected_ingress_mode(config)?,
    };
    if !matches!(ingress_mode, IngressMode::Polling) {
        return Ok(plugin_error(
            "polling_not_supported",
            "telegram poll_ingress requires polling ingress mode",
        ));
    }

    let client = TelegramClient::from_env(bot_token_env(config))?;
    let next_update_id = state
        .as_ref()
        .and_then(|state| state.metadata.get(META_NEXT_UPDATE_ID))
        .map(|value| parse_i64("state.metadata.next_update_id", value))
        .transpose()?;
    let updates = client.get_updates(next_update_id, poll_timeout_secs(config))?;

    let mut events = Vec::new();
    let mut max_update_id = next_update_id.map(|offset| offset.saturating_sub(1));
    for update_value in updates {
        let update: TelegramUpdate = match serde_json::from_value(update_value) {
            Ok(update) => update,
            Err(_) => continue,
        };
        max_update_id = Some(match max_update_id {
            Some(current) => current.max(update.update_id),
            None => update.update_id,
        });

        let Some((event_type, message)) = update.event_and_message() else {
            continue;
        };

        let conversation_id = telegram_id_to_string(&message.chat.id)?;
        if !chat_is_allowed(config, &conversation_id) {
            continue;
        }

        let event = build_inbound_event(
            &update,
            event_type,
            message,
            TelegramIngressContext {
                transport: TRANSPORT_POLLING,
                endpoint_id: None,
                path: None,
                host_received_at: None,
            },
            conversation_id,
        )?;
        if !sender_is_allowed(
            config,
            &event.actor.id,
            event.conversation.kind.eq_ignore_ascii_case("private"),
            &event.conversation.kind,
        ) {
            continue;
        }
        events.push(event);
    }

    let next_update_id = max_update_id.map(|value| value.saturating_add(1));
    Ok(PluginResponse::IngressEventsReceived {
        events,
        callback_reply: None,
        state: Some(polling_state(config, next_update_id)),
        poll_after_ms: Some(250),
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let chat_id = resolve_chat_id(config, message)?;
    ensure_allowed_chat(config, &chat_id)?;
    let message_thread_id = resolve_message_thread_id(config, message)?;
    let reply_to_message_id = resolve_reply_to_message_id(message)?;
    let client = TelegramClient::from_env(bot_token_env(config))?;
    let (sent, delivery_method) = match message.attachments.as_slice() {
        [] => (
            client.send_message(
                &chat_id,
                &message.content,
                reply_to_message_id,
                message_thread_id,
            )?,
            "sendMessage",
        ),
        [attachment] => {
            let source = telegram_attachment_source(attachment)?;
            let caption = (!message.content.is_empty()).then_some(message.content.as_str());
            let sent = match telegram_delivery_method(attachment) {
                TelegramDeliveryMethod::Photo => client.send_photo(
                    &chat_id,
                    source.as_media_source(),
                    caption,
                    reply_to_message_id,
                    message_thread_id,
                )?,
                TelegramDeliveryMethod::Document => client.send_document(
                    &chat_id,
                    source.as_media_source(),
                    caption,
                    reply_to_message_id,
                    message_thread_id,
                )?,
            };
            (sent, telegram_delivery_method_name(attachment))
        }
        _ => bail!("telegram delivery supports at most one attachment"),
    };

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_TELEGRAM.to_string());
    metadata.insert(
        META_DELIVERY_METHOD.to_string(),
        delivery_method.to_string(),
    );
    metadata.insert(META_CHAT_ID.to_string(), sent.chat_id.clone());
    if let Some(message_thread_id) = message_thread_id {
        metadata.insert(
            META_MESSAGE_THREAD_ID.to_string(),
            message_thread_id.to_string(),
        );
    }
    if let Some(reply_to_message_id) = reply_to_message_id {
        metadata.insert(
            META_REPLY_TO_MESSAGE_ID.to_string(),
            reply_to_message_id.to_string(),
        );
    }

    Ok(DeliveryReceipt {
        message_id: sent.message_id,
        conversation_id: sent.chat_id,
        metadata,
    })
}

fn handle_ingress_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if !payload.method.eq_ignore_ascii_case("POST") {
        return Ok(ingress_rejection(
            405,
            "telegram ingress expects POST requests",
        ));
    }

    if let Some(reply) = validate_ingress_secret(config, payload)? {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(reply),
            state: None,
            poll_after_ms: None,
        });
    }

    let update: TelegramUpdate = match serde_json::from_str(&payload.body) {
        Ok(update) => update,
        Err(_) => return Ok(ingress_rejection(400, "invalid Telegram update payload")),
    };

    let Some((event_type, message)) = update.event_and_message() else {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: None,
            poll_after_ms: None,
        });
    };

    let conversation_id = telegram_id_to_string(&message.chat.id)?;
    if !chat_is_allowed(config, &conversation_id) {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: None,
            poll_after_ms: None,
        });
    }

    let event = build_inbound_event(
        &update,
        event_type,
        message,
        TelegramIngressContext {
            transport: TRANSPORT_WEBHOOK,
            endpoint_id: payload.endpoint_id.as_deref(),
            path: Some(payload.path.as_str()),
            host_received_at: payload.received_at.as_deref(),
        },
        conversation_id,
    )?;
    if !sender_is_allowed(
        config,
        &event.actor.id,
        event.conversation.kind.eq_ignore_ascii_case("private"),
        &event.conversation.kind,
    ) {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: None,
            poll_after_ms: None,
        });
    }
    Ok(PluginResponse::IngressEventsReceived {
        events: vec![event],
        callback_reply: None,
        state: None,
        poll_after_ms: None,
    })
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let Some(activity) = resolve_status_activity(update) else {
        return Ok(rejected_status(
            "unsupported_kind",
            format!("telegram does not render status kind {:?}", update.kind),
        ));
    };

    let Some(chat_id) = update
        .conversation_id
        .as_deref()
        .or(config.default_chat_id.as_deref())
    else {
        return Ok(rejected_status(
            "missing_conversation_id",
            "telegram status frames require update.conversation_id or config.default_chat_id",
        ));
    };

    if !chat_is_allowed(config, chat_id) {
        return Ok(rejected_status(
            "chat_not_allowed",
            format!("telegram chat {chat_id} is not permitted by config.allowed_chat_ids"),
        ));
    }

    let message_thread_id = match update.thread_id.as_deref() {
        Some(thread_id) => Some(parse_i64("update.thread_id", thread_id)?),
        None => config.default_message_thread_id,
    };

    let client = TelegramClient::from_env(bot_token_env(config))?;
    client.send_chat_action(chat_id, &activity, message_thread_id)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_TELEGRAM.to_string());
    metadata.insert(META_CHAT_ID.to_string(), chat_id.to_string());
    metadata.insert(META_ACTIVITY.to_string(), activity);
    metadata.insert(META_STATUS_KIND.to_string(), status_kind_name(&update.kind));
    if let Some(message_thread_id) = message_thread_id {
        metadata.insert(
            META_MESSAGE_THREAD_ID.to_string(),
            message_thread_id.to_string(),
        );
    }

    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

fn resolved_endpoint(config: &ChannelConfig) -> Option<String> {
    let base = config.webhook_public_url.as_deref()?.trim_end_matches('/');
    let path = config
        .webhook_path
        .as_deref()
        .unwrap_or("/telegram/updates")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn selected_ingress_mode(config: &ChannelConfig) -> Result<IngressMode> {
    match config.ingress_mode.as_deref() {
        Some("webhook") => Ok(IngressMode::Webhook),
        Some("polling") => Ok(IngressMode::Polling),
        Some(other) => Err(anyhow!(
            "telegram ingress_mode must be `webhook` or `polling`, got `{other}`"
        )),
        None => {
            if config.webhook_public_url.is_some() {
                Ok(IngressMode::Webhook)
            } else {
                Ok(IngressMode::Polling)
            }
        }
    }
}

fn poll_timeout_secs(config: &ChannelConfig) -> u16 {
    config.poll_timeout_secs.unwrap_or(20).clamp(1, 25)
}

fn polling_state(config: &ChannelConfig, next_update_id: Option<i64>) -> IngressState {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_TELEGRAM.to_string());
    metadata.insert(
        META_POLL_TIMEOUT_SECS.to_string(),
        poll_timeout_secs(config).to_string(),
    );
    if let Some(next_update_id) = next_update_id {
        metadata.insert(META_NEXT_UPDATE_ID.to_string(), next_update_id.to_string());
    }

    IngressState {
        mode: IngressMode::Polling,
        status: "running".to_string(),
        endpoint: None,
        metadata,
    }
}

fn channel_policy(config: &ChannelConfig) -> ChannelPolicy {
    ChannelPolicy {
        owner_id: config.owner_id.clone(),
        allowed_sender_ids: config.allowed_sender_ids.clone(),
        allowed_conversation_ids: config.allowed_chat_ids.clone(),
        dm_policy: config.dm_policy.clone(),
        require_signature_validation: Some(true),
        allow_group_messages: config.allow_group_messages,
        max_attachment_bytes: None,
        metadata: BTreeMap::new(),
    }
}

fn resolve_chat_id(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
    if let Some(chat_id) = &message.chat_id {
        return Ok(chat_id.clone());
    }
    if let Some(chat_id) = message.metadata.get(ROUTE_CONVERSATION_ID) {
        return Ok(chat_id.clone());
    }
    if let Some(default_chat_id) = &config.default_chat_id {
        return Ok(default_chat_id.clone());
    }
    Err(anyhow!(
        "telegram delivery requires message.chat_id or config.default_chat_id"
    ))
}

fn resolve_message_thread_id(
    config: &ChannelConfig,
    message: &OutboundMessage,
) -> Result<Option<i64>> {
    if let Some(message_thread_id) = message.message_thread_id {
        return Ok(Some(message_thread_id));
    }
    if let Some(thread_id) = message.metadata.get(ROUTE_THREAD_ID) {
        return Ok(Some(parse_i64("message.metadata.thread_id", thread_id)?));
    }
    Ok(config.default_message_thread_id)
}

fn resolve_reply_to_message_id(message: &OutboundMessage) -> Result<Option<i64>> {
    if let Some(reply_to_message_id) = message.reply_to_message_id {
        return Ok(Some(reply_to_message_id));
    }
    if let Some(reply_to_message_id) = message.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID) {
        return Ok(Some(parse_i64(
            "message.metadata.reply_to_message_id",
            reply_to_message_id,
        )?));
    }
    Ok(None)
}

fn validate_ingress_secret(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let Ok(expected_secret) = std::env::var(webhook_secret_env(config)) else {
        return Ok(None);
    };

    let Some(actual_secret) = header_value(&payload.headers, "X-Telegram-Bot-Api-Secret-Token")
    else {
        return Ok(Some(callback_reply(
            403,
            "telegram webhook secret token header missing",
        )));
    };

    if actual_secret != expected_secret {
        return Ok(Some(callback_reply(
            403,
            "telegram webhook secret token mismatch",
        )));
    }

    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramDeliveryMethod {
    Photo,
    Document,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramAttachmentSource {
    Reference(String),
    Upload(TelegramUpload),
}

impl TelegramAttachmentSource {
    fn as_media_source(&self) -> TelegramMediaSource<'_> {
        match self {
            Self::Reference(reference) => TelegramMediaSource::Reference(reference),
            Self::Upload(upload) => TelegramMediaSource::Upload(upload),
        }
    }
}

fn telegram_attachment_source(attachment: &OutboundAttachment) -> Result<TelegramAttachmentSource> {
    if let Some(data_base64) = attachment.data_base64.as_deref() {
        let data = BASE64_STANDARD.decode(data_base64).with_context(|| {
            format!(
                "invalid base64 attachment payload for `{}`",
                attachment.name
            )
        })?;
        return Ok(TelegramAttachmentSource::Upload(TelegramUpload {
            name: attachment.name.clone(),
            mime_type: attachment.mime_type.clone(),
            data,
        }));
    }
    if let Some(url) = attachment.url.as_deref() {
        return Ok(TelegramAttachmentSource::Reference(url.to_string()));
    }
    if let Some(storage_key) = attachment.storage_key.as_deref() {
        return Ok(TelegramAttachmentSource::Reference(
            storage_key
                .strip_prefix("telegram:file:")
                .unwrap_or(storage_key)
                .to_string(),
        ));
    }
    bail!(
        "telegram attachment delivery requires attachment.data_base64, attachment.url, or attachment.storage_key"
    )
}

fn telegram_delivery_method(attachment: &OutboundAttachment) -> TelegramDeliveryMethod {
    if attachment.mime_type.starts_with("image/") {
        TelegramDeliveryMethod::Photo
    } else {
        TelegramDeliveryMethod::Document
    }
}

fn telegram_delivery_method_name(attachment: &OutboundAttachment) -> &'static str {
    match telegram_delivery_method(attachment) {
        TelegramDeliveryMethod::Photo => "sendPhoto",
        TelegramDeliveryMethod::Document => "sendDocument",
    }
}

fn build_inbound_event(
    update: &TelegramUpdate,
    event_type: &str,
    message: &TelegramInboundMessage,
    ingress: TelegramIngressContext<'_>,
    conversation_id: String,
) -> Result<InboundEventEnvelope> {
    let actor = inbound_actor(message)?;
    let reply_to_message_id = message
        .reply_to_message
        .as_ref()
        .map(|reply| reply.message_id.to_string());
    let received_at = received_at(ingress.host_received_at, message.date)?;
    let content = message
        .text
        .clone()
        .or_else(|| message.caption.clone())
        .unwrap_or_default();

    let mut message_metadata = BTreeMap::new();
    let attachments = extract_attachments(message, &mut message_metadata);
    if message.text.is_some() {
        message_metadata.insert(
            META_TELEGRAM_CONTENT_ORIGIN.to_string(),
            CONTENT_ORIGIN_TEXT.to_string(),
        );
    } else if message.caption.is_some() {
        message_metadata.insert(
            META_TELEGRAM_CONTENT_ORIGIN.to_string(),
            CONTENT_ORIGIN_CAPTION.to_string(),
        );
    } else {
        message_metadata.insert(
            META_TELEGRAM_CONTENT_ORIGIN.to_string(),
            CONTENT_ORIGIN_EMPTY.to_string(),
        );
    }

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(
        META_TELEGRAM_UPDATE_ID.to_string(),
        update.update_id.to_string(),
    );
    event_metadata.insert(META_TRANSPORT.to_string(), ingress.transport.to_string());
    if let Some(endpoint_id) = ingress.endpoint_id {
        event_metadata.insert(META_ENDPOINT_ID.to_string(), endpoint_id.to_string());
    }
    if let Some(path) = ingress.path.filter(|path| !path.is_empty()) {
        event_metadata.insert(META_PATH.to_string(), path.to_string());
    }

    Ok(InboundEventEnvelope {
        event_id: format!("telegram:{}:{}", update.update_id, message.message_id),
        platform: PLATFORM_TELEGRAM.to_string(),
        event_type: event_type.to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: conversation_id,
            kind: message.chat.kind.clone(),
            thread_id: message.message_thread_id.map(|value| value.to_string()),
            parent_message_id: reply_to_message_id.clone(),
        },
        actor,
        message: InboundMessage {
            id: message.message_id.to_string(),
            content,
            content_type: "text/plain".to_string(),
            reply_to_message_id,
            attachments,
            metadata: message_metadata,
        },
        account_id: None,
        metadata: event_metadata,
    })
}

#[derive(Clone, Copy)]
struct TelegramIngressContext<'a> {
    transport: &'a str,
    endpoint_id: Option<&'a str>,
    path: Option<&'a str>,
    host_received_at: Option<&'a str>,
}

fn extract_attachments(
    message: &TelegramInboundMessage,
    message_metadata: &mut BTreeMap<String, String>,
) -> Vec<InboundAttachment> {
    let mut attachments = Vec::new();

    if let Some(photo) = largest_photo(&message.photo) {
        let mut extras = BTreeMap::new();
        insert_extra(&mut extras, "width", Some(photo.width));
        insert_extra(&mut extras, "height", Some(photo.height));
        if let Some(file_unique_id) = &photo.file_unique_id {
            extras.insert("file_unique_id".to_string(), file_unique_id.clone());
        }
        attachments.push(InboundAttachment {
            id: Some(photo.file_id.clone()),
            kind: "image".to_string(),
            url: None,
            mime_type: None,
            size_bytes: photo.file_size,
            name: None,
            storage_key: Some(telegram_file_storage_key(&photo.file_id)),
            extracted_text: None,
            extras,
        });
    }

    if let Some(document) = &message.document {
        attachments.push(file_attachment(
            &document.file_id,
            "file",
            document.mime_type.clone(),
            document.file_size,
            document.file_name.clone(),
            None,
            document.file_unique_id.as_ref(),
        ));
    }

    if let Some(audio) = &message.audio {
        let mut extras = BTreeMap::new();
        insert_extra(&mut extras, "duration", audio.duration);
        if let Some(file_unique_id) = &audio.file_unique_id {
            extras.insert("file_unique_id".to_string(), file_unique_id.clone());
        }
        attachments.push(InboundAttachment {
            id: Some(audio.file_id.clone()),
            kind: "audio".to_string(),
            url: None,
            mime_type: audio.mime_type.clone(),
            size_bytes: audio.file_size,
            name: audio.file_name.clone(),
            storage_key: Some(telegram_file_storage_key(&audio.file_id)),
            extracted_text: None,
            extras,
        });
    }

    if let Some(video) = &message.video {
        let mut extras = BTreeMap::new();
        insert_extra(&mut extras, "duration", video.duration);
        insert_extra(&mut extras, "width", Some(video.width));
        insert_extra(&mut extras, "height", Some(video.height));
        if let Some(file_unique_id) = &video.file_unique_id {
            extras.insert("file_unique_id".to_string(), file_unique_id.clone());
        }
        attachments.push(InboundAttachment {
            id: Some(video.file_id.clone()),
            kind: "video".to_string(),
            url: None,
            mime_type: video.mime_type.clone(),
            size_bytes: video.file_size,
            name: video.file_name.clone(),
            storage_key: Some(telegram_file_storage_key(&video.file_id)),
            extracted_text: None,
            extras,
        });
    }

    if let Some(voice) = &message.voice {
        let mut extras = BTreeMap::new();
        insert_extra(&mut extras, "duration", voice.duration);
        if let Some(file_unique_id) = &voice.file_unique_id {
            extras.insert("file_unique_id".to_string(), file_unique_id.clone());
        }
        attachments.push(InboundAttachment {
            id: Some(voice.file_id.clone()),
            kind: "audio".to_string(),
            url: None,
            mime_type: voice.mime_type.clone(),
            size_bytes: voice.file_size,
            name: None,
            storage_key: Some(telegram_file_storage_key(&voice.file_id)),
            extracted_text: None,
            extras,
        });
    }

    if let Some(animation) = &message.animation {
        let mut extras = BTreeMap::new();
        insert_extra(&mut extras, "duration", animation.duration);
        insert_extra(&mut extras, "width", Some(animation.width));
        insert_extra(&mut extras, "height", Some(animation.height));
        if let Some(file_unique_id) = &animation.file_unique_id {
            extras.insert("file_unique_id".to_string(), file_unique_id.clone());
        }
        attachments.push(InboundAttachment {
            id: Some(animation.file_id.clone()),
            kind: "video".to_string(),
            url: None,
            mime_type: animation.mime_type.clone(),
            size_bytes: animation.file_size,
            name: animation.file_name.clone(),
            storage_key: Some(telegram_file_storage_key(&animation.file_id)),
            extracted_text: None,
            extras,
        });
    }

    if !attachments.is_empty() {
        message_metadata.insert(
            META_ATTACHMENT_COUNT.to_string(),
            attachments.len().to_string(),
        );
    }

    attachments
}

fn file_attachment(
    file_id: &str,
    kind: &str,
    mime_type: Option<String>,
    size_bytes: Option<u64>,
    name: Option<String>,
    extracted_text: Option<String>,
    file_unique_id: Option<&String>,
) -> InboundAttachment {
    let mut extras = BTreeMap::new();
    if let Some(file_unique_id) = file_unique_id {
        extras.insert("file_unique_id".to_string(), file_unique_id.clone());
    }
    InboundAttachment {
        id: Some(file_id.to_string()),
        kind: kind.to_string(),
        url: None,
        mime_type,
        size_bytes,
        name,
        storage_key: Some(telegram_file_storage_key(file_id)),
        extracted_text,
        extras,
    }
}

fn largest_photo(photos: &[TelegramPhotoSize]) -> Option<&TelegramPhotoSize> {
    photos
        .iter()
        .max_by_key(|photo| photo.file_size.unwrap_or_default())
        .or_else(|| photos.last())
}

fn telegram_file_storage_key(file_id: &str) -> String {
    format!("telegram:file:{file_id}")
}

fn insert_extra(extras: &mut BTreeMap<String, String>, key: &str, value: Option<impl ToString>) {
    if let Some(value) = value {
        extras.insert(key.to_string(), value.to_string());
    }
}

fn ingress_mode_name(mode: IngressMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| TRANSPORT_WEBHOOK.to_string())
}

fn status_kind_name(kind: &StatusKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn inbound_actor(message: &TelegramInboundMessage) -> Result<InboundActor> {
    if let Some(from) = &message.from {
        return Ok(InboundActor {
            id: telegram_id_to_string(&from.id)?,
            display_name: Some(display_name(
                from.first_name.as_deref(),
                from.last_name.as_deref(),
                from.username.as_deref(),
            )),
            username: from.username.clone(),
            is_bot: from.is_bot.unwrap_or(false),
            metadata: BTreeMap::new(),
        });
    }

    if let Some(sender_chat) = &message.sender_chat {
        return Ok(InboundActor {
            id: telegram_id_to_string(&sender_chat.id)?,
            display_name: sender_chat
                .title
                .clone()
                .or_else(|| sender_chat.username.clone()),
            username: sender_chat.username.clone(),
            is_bot: false,
            metadata: BTreeMap::from([("actor_kind".to_string(), "sender_chat".to_string())]),
        });
    }

    Ok(InboundActor {
        id: telegram_id_to_string(&message.chat.id)?,
        display_name: message
            .chat
            .title
            .clone()
            .or_else(|| message.chat.username.clone()),
        username: message.chat.username.clone(),
        is_bot: false,
        metadata: BTreeMap::from([("actor_kind".to_string(), "chat".to_string())]),
    })
}

fn received_at(host_received_at: Option<&str>, message_date: i64) -> Result<String> {
    if let Some(host_received_at) = host_received_at {
        return Ok(host_received_at.to_string());
    }

    Ok(Timestamp::from_second(message_date)
        .context("telegram update date is out of range")?
        .to_string())
}

fn resolve_status_activity(update: &StatusFrame) -> Option<String> {
    if let Some(activity) = update.metadata.get("telegram_activity") {
        return Some(activity.clone());
    }

    match &update.kind {
        StatusKind::Processing
        | StatusKind::OperationStarted
        | StatusKind::Info
        | StatusKind::Delivering => Some("typing".to_string()),
        _ => None,
    }
}

fn ensure_allowed_chat(config: &ChannelConfig, chat_id: &str) -> Result<()> {
    if chat_is_allowed(config, chat_id) {
        return Ok(());
    }

    Err(anyhow!(
        "telegram chat {chat_id} is not permitted by config.allowed_chat_ids"
    ))
}

fn chat_is_allowed(config: &ChannelConfig, chat_id: &str) -> bool {
    config.allowed_chat_ids.is_empty()
        || config
            .allowed_chat_ids
            .iter()
            .any(|allowed| allowed == chat_id)
}

fn sender_is_allowed(
    config: &ChannelConfig,
    sender_id: &str,
    is_direct_message: bool,
    conversation_kind: &str,
) -> bool {
    if let Some(owner_id) = config.owner_id.as_deref()
        && owner_id != sender_id
    {
        return false;
    }

    if !is_direct_message && matches!(config.allow_group_messages, Some(false)) {
        let _ = conversation_kind;
        return false;
    }

    if !is_direct_message {
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

fn rejected_status(reason_code: &str, reason: impl Into<String>) -> StatusAcceptance {
    StatusAcceptance {
        accepted: false,
        metadata: BTreeMap::from([
            (META_REASON_CODE.to_string(), reason_code.to_string()),
            ("reason".to_string(), reason.into()),
        ]),
    }
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn parse_i64(field_name: &str, value: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .with_context(|| format!("{field_name} must be a valid 64-bit integer"))
}

fn telegram_id_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        other => Err(anyhow!(
            "expected a Telegram id string or number, got {other}"
        )),
    }
}

fn display_name(
    first_name: Option<&str>,
    last_name: Option<&str>,
    username: Option<&str>,
) -> String {
    match (first_name, last_name) {
        (Some(first_name), Some(last_name)) => format!("{first_name} {last_name}"),
        (Some(first_name), None) => first_name.to_string(),
        (None, Some(last_name)) => last_name.to_string(),
        (None, None) => username.unwrap_or("telegram").to_string(),
    }
}

fn bot_token_env(config: &ChannelConfig) -> &str {
    config
        .bot_token_env
        .as_deref()
        .unwrap_or("TELEGRAM_BOT_TOKEN")
}

fn webhook_secret_env(config: &ChannelConfig) -> &str {
    config
        .webhook_secret_env
        .as_deref()
        .unwrap_or("TELEGRAM_WEBHOOK_SECRET")
}

fn read_required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required for the telegram channel"))
}

fn has_optional_env(name: &str) -> bool {
    std::env::var(name).is_ok()
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramInboundMessage>,
    #[serde(default)]
    edited_message: Option<TelegramInboundMessage>,
    #[serde(default)]
    channel_post: Option<TelegramInboundMessage>,
    #[serde(default)]
    edited_channel_post: Option<TelegramInboundMessage>,
}

impl TelegramUpdate {
    fn event_and_message(&self) -> Option<(&'static str, &TelegramInboundMessage)> {
        self.message
            .as_ref()
            .map(|message| ("message.received", message))
            .or_else(|| {
                self.edited_message
                    .as_ref()
                    .map(|message| ("message.edited", message))
            })
            .or_else(|| {
                self.channel_post
                    .as_ref()
                    .map(|message| ("channel_post.received", message))
            })
            .or_else(|| {
                self.edited_channel_post
                    .as_ref()
                    .map(|message| ("channel_post.edited", message))
            })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramInboundMessage {
    message_id: i64,
    #[serde(default)]
    message_thread_id: Option<i64>,
    date: i64,
    chat: TelegramChat,
    #[serde(default)]
    from: Option<TelegramUser>,
    #[serde(default)]
    sender_chat: Option<TelegramChat>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Vec<TelegramPhotoSize>,
    #[serde(default)]
    document: Option<TelegramDocument>,
    #[serde(default)]
    audio: Option<TelegramAudio>,
    #[serde(default)]
    video: Option<TelegramVideo>,
    #[serde(default)]
    voice: Option<TelegramVoice>,
    #[serde(default)]
    animation: Option<TelegramAnimation>,
    #[serde(default)]
    reply_to_message: Option<TelegramReplyMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    width: u32,
    height: u32,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramDocument {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramAudio {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramVideo {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    width: u32,
    height: u32,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramVoice {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramAnimation {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    width: u32,
    height: u32,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramReplyMessage {
    message_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramUser {
    id: Value,
    #[serde(default)]
    is_bot: Option<bool>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramChat {
    id: Value,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingress_event_parses_telegram_message_into_dispatch_event() {
        let config = ChannelConfig::default();
        let payload = IngressPayload {
            endpoint_id: Some("telegram-main".to_string()),
            method: "POST".to_string(),
            path: "/telegram/updates".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: serde_json::json!({
                "update_id": 812345678,
                "message": {
                    "message_id": 321,
                    "date": 1_710_000_000,
                    "message_thread_id": 44,
                    "chat": {
                        "id": -1001234567890_i64,
                        "type": "supergroup",
                        "title": "Dispatch Ops"
                    },
                    "from": {
                        "id": 9001,
                        "is_bot": false,
                        "username": "chris",
                        "first_name": "Chris"
                    },
                    "text": "Status report",
                    "reply_to_message": {
                        "message_id": 111
                    }
                }
            })
            .to_string(),
            trust_verified: true,
            received_at: Some("2026-04-10T21:12:13Z".to_string()),
        };

        let response = handle_ingress_event(&config, &payload).expect("ingress event succeeds");
        let PluginResponse::IngressEventsReceived {
            events,
            callback_reply,
            ..
        } = response
        else {
            panic!("unexpected response variant");
        };

        assert!(callback_reply.is_none());
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_id, "telegram:812345678:321");
        assert_eq!(event.event_type, "message.received");
        assert_eq!(event.received_at, "2026-04-10T21:12:13Z");
        assert_eq!(event.conversation.id, "-1001234567890");
        assert_eq!(event.conversation.kind, "supergroup");
        assert_eq!(event.conversation.thread_id.as_deref(), Some("44"));
        assert_eq!(event.conversation.parent_message_id.as_deref(), Some("111"));
        assert_eq!(event.actor.id, "9001");
        assert_eq!(event.actor.username.as_deref(), Some("chris"));
        assert_eq!(event.message.id, "321");
        assert_eq!(event.message.content, "Status report");
        assert_eq!(
            event
                .message
                .metadata
                .get("telegram_content_origin")
                .map(String::as_str),
            Some("text")
        );
        assert_eq!(
            event.metadata.get("telegram_update_id").map(String::as_str),
            Some("812345678")
        );
        assert_eq!(
            event.metadata.get("endpoint_id").map(String::as_str),
            Some("telegram-main")
        );
    }

    #[test]
    fn ingress_event_rejects_wrong_secret_without_emitting_events() {
        let config = ChannelConfig {
            webhook_secret_env: Some("DISPATCH_TELEGRAM_SECRET".to_string()),
            ..ChannelConfig::default()
        };
        set_env_for_test("DISPATCH_TELEGRAM_SECRET", Some("expected-secret"));

        let payload = IngressPayload {
            endpoint_id: None,
            method: "POST".to_string(),
            path: "/telegram/updates".to_string(),
            headers: BTreeMap::from([(
                "X-Telegram-Bot-Api-Secret-Token".to_string(),
                "wrong-secret".to_string(),
            )]),
            query: BTreeMap::new(),
            raw_query: None,
            body: "{}".to_string(),
            trust_verified: false,
            received_at: None,
        };

        let response = handle_ingress_event(&config, &payload).expect("secret check succeeds");
        set_env_for_test("DISPATCH_TELEGRAM_SECRET", None);

        let PluginResponse::IngressEventsReceived {
            events,
            callback_reply,
            ..
        } = response
        else {
            panic!("unexpected response variant");
        };

        assert!(events.is_empty());
        let reply = callback_reply.expect("callback reply");
        assert_eq!(reply.status, 403);
        assert!(reply.body.contains("mismatch"));
    }

    #[test]
    fn ingress_event_preserves_media_attachments_without_fake_urls() {
        let config = ChannelConfig::default();
        let payload = IngressPayload {
            endpoint_id: Some("telegram-main".to_string()),
            method: "POST".to_string(),
            path: "/telegram/updates".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: serde_json::json!({
                "update_id": 812345679,
                "message": {
                    "message_id": 322,
                    "date": 1_710_000_001,
                    "chat": {
                        "id": -1001234567890_i64,
                        "type": "supergroup",
                        "title": "Dispatch Ops"
                    },
                    "from": {
                        "id": 9001,
                        "is_bot": false,
                        "username": "chris",
                        "first_name": "Chris"
                    },
                    "caption": "see attached",
                    "photo": [
                        {
                            "file_id": "small-photo",
                            "file_unique_id": "photo-1",
                            "width": 90,
                            "height": 90,
                            "file_size": 1000
                        },
                        {
                            "file_id": "large-photo",
                            "file_unique_id": "photo-2",
                            "width": 1280,
                            "height": 720,
                            "file_size": 4096
                        }
                    ]
                }
            })
            .to_string(),
            trust_verified: true,
            received_at: Some("2026-04-10T21:12:14Z".to_string()),
        };

        let response = handle_ingress_event(&config, &payload).expect("ingress event succeeds");
        let PluginResponse::IngressEventsReceived { events, .. } = response else {
            panic!("unexpected response variant");
        };

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.message.content, "see attached");
        assert_eq!(
            event
                .message
                .metadata
                .get(META_TELEGRAM_CONTENT_ORIGIN)
                .map(String::as_str),
            Some(CONTENT_ORIGIN_CAPTION)
        );
        assert_eq!(
            event
                .message
                .metadata
                .get(META_ATTACHMENT_COUNT)
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(event.message.attachments.len(), 1);

        let attachment = &event.message.attachments[0];
        assert_eq!(attachment.id.as_deref(), Some("large-photo"));
        assert_eq!(attachment.kind, "image");
        assert!(attachment.url.is_none());
        assert_eq!(attachment.size_bytes, Some(4096));
        assert_eq!(
            attachment.storage_key.as_deref(),
            Some("telegram:file:large-photo")
        );
        assert_eq!(
            attachment.extras.get("file_unique_id").map(String::as_str),
            Some("photo-2")
        );
        assert_eq!(
            attachment.extras.get("width").map(String::as_str),
            Some("1280")
        );
        assert_eq!(
            attachment.extras.get("height").map(String::as_str),
            Some("720")
        );
    }

    #[test]
    fn ingress_event_drops_group_message_when_group_messages_are_disabled() {
        let payload = IngressPayload {
            endpoint_id: Some("telegram-main".to_string()),
            method: "POST".to_string(),
            path: "/telegram/updates".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: serde_json::json!({
                "update_id": 812345680,
                "message": {
                    "message_id": 400,
                    "date": 1_710_000_002,
                    "chat": {
                        "id": -1001234567890_i64,
                        "type": "supergroup",
                        "title": "Dispatch Ops"
                    },
                    "from": {
                        "id": 9001,
                        "is_bot": false,
                        "username": "chris",
                        "first_name": "Chris"
                    },
                    "text": "hello from the group"
                }
            })
            .to_string(),
            trust_verified: true,
            received_at: Some("2026-04-10T21:12:13Z".to_string()),
        };

        let response = handle_ingress_event(
            &ChannelConfig {
                allow_group_messages: Some(false),
                ..ChannelConfig::default()
            },
            &payload,
        )
        .expect("ingress event succeeds");

        let PluginResponse::IngressEventsReceived { events, .. } = response else {
            panic!("unexpected response variant");
        };
        assert!(events.is_empty());
    }

    #[test]
    fn status_activity_maps_dispatch_processing_to_typing() {
        let update = StatusFrame {
            kind: StatusKind::Processing,
            message: "Working".to_string(),
            conversation_id: Some("1234".to_string()),
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        assert_eq!(resolve_status_activity(&update).as_deref(), Some("typing"));
    }

    #[test]
    fn unsupported_status_kind_is_rejected_without_api_call() {
        let config = ChannelConfig::default();
        let update = StatusFrame {
            kind: StatusKind::ApprovalNeeded,
            message: "Needs approval".to_string(),
            conversation_id: Some("1234".to_string()),
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        let acceptance = send_status(&config, &update).expect("status evaluation succeeds");
        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance.metadata.get("reason_code").map(String::as_str),
            Some("unsupported_kind")
        );
    }

    #[test]
    fn telegram_attachment_source_prefers_inline_then_url_then_storage_key() {
        let inline_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: Some("ZmFrZQ==".to_string()),
            url: Some("https://example.com/photo.jpg".to_string()),
            storage_key: Some("telegram:file:cached-photo".to_string()),
        };
        assert_eq!(
            telegram_attachment_source(&inline_attachment).expect("inline source"),
            TelegramAttachmentSource::Upload(TelegramUpload {
                name: "photo.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                data: b"fake".to_vec(),
            })
        );

        let url_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: None,
            url: Some("https://example.com/photo.jpg".to_string()),
            storage_key: Some("telegram:file:cached-photo".to_string()),
        };
        assert_eq!(
            telegram_attachment_source(&url_attachment).expect("url source"),
            TelegramAttachmentSource::Reference("https://example.com/photo.jpg".to_string())
        );

        let storage_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: None,
            url: None,
            storage_key: Some("telegram:file:cached-photo".to_string()),
        };
        assert_eq!(
            telegram_attachment_source(&storage_attachment).expect("storage source"),
            TelegramAttachmentSource::Reference("cached-photo".to_string())
        );
    }

    #[test]
    fn telegram_attachment_source_rejects_invalid_inline_data() {
        let attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: Some("%%%".to_string()),
            url: None,
            storage_key: None,
        };

        let error = telegram_attachment_source(&attachment).expect_err("invalid inline data");
        assert!(
            error
                .to_string()
                .contains("invalid base64 attachment payload")
        );
    }

    #[test]
    fn telegram_delivery_method_uses_photo_for_images_and_document_otherwise() {
        let image_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: None,
            url: Some("https://example.com/photo.jpg".to_string()),
            storage_key: None,
        };
        assert_eq!(
            telegram_delivery_method(&image_attachment),
            TelegramDeliveryMethod::Photo
        );
        assert_eq!(
            telegram_delivery_method_name(&image_attachment),
            "sendPhoto"
        );

        let document_attachment = OutboundAttachment {
            name: "report.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            data_base64: None,
            url: Some("https://example.com/report.pdf".to_string()),
            storage_key: None,
        };
        assert_eq!(
            telegram_delivery_method(&document_attachment),
            TelegramDeliveryMethod::Document
        );
        assert_eq!(
            telegram_delivery_method_name(&document_attachment),
            "sendDocument"
        );
    }

    fn set_env_for_test(name: &str, value: Option<&str>) {
        // Tests mutate process-global environment in a narrow, synchronous scope.
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
