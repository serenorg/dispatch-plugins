use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use hmac::{Hmac, Mac};
use jiff::Timestamp;
use serde::Deserialize;
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

mod protocol;
mod whatsapp_api;

use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ChannelPolicy, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressCallbackReply, IngressMode, IngressPayload,
    IngressState, OutboundAttachment, OutboundMessage, PluginRequest, PluginRequestEnvelope,
    PluginResponse, StatusAcceptance, StatusFrame, StatusKind, capabilities, plugin_error,
};
use whatsapp_api::{
    WhatsAppClient, WhatsAppMediaRef, WhatsAppMediaRequest, WhatsAppMediaType, WhatsAppUpload,
};

const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";

const META_REASON: &str = "reason";
const META_PLATFORM: &str = "platform";
const META_ACCESS_TOKEN_ENV: &str = "access_token_env";
const META_PHONE_NUMBER_ID: &str = "phone_number_id";
const META_API_VERSION: &str = "api_version";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_WEBHOOK_ENDPOINT: &str = "webhook_endpoint";
const META_VERIFY_TOKEN_ENV: &str = "verify_token_env";
const META_APP_SECRET_ENV: &str = "app_secret_env";
const META_HOST_ACTION: &str = "host_action";
const META_SIGNATURE_HEADER: &str = "signature_header";
const META_ACCOUNT_ID: &str = "account_id";
const META_TO_NUMBER: &str = "to_number";
const META_TRANSPORT: &str = "transport";
const META_ENDPOINT_ID: &str = "endpoint_id";
const META_PATH: &str = "path";
const META_MESSAGE_TYPE: &str = "message_type";
const META_ATTACHMENT_COUNT: &str = "attachment_count";
const META_ENTRY_ID: &str = "entry_id";
const META_DISPLAY_PHONE_NUMBER: &str = "display_phone_number";
const META_FIELD: &str = "field";
const META_STATUS_KIND: &str = "status_kind";
const META_REASON_CODE: &str = "reason_code";

const PLATFORM_WHATSAPP: &str = "whatsapp";
const TRANSPORT_WEBHOOK: &str = "webhook";
const HEADER_X_HUB_SIGNATURE_256: &str = "X-Hub-Signature-256";
const WHATSAPP_STATUS_TEXT: &str = "whatsapp_status_text";

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
        PluginRequest::PollIngress { .. } => Ok(plugin_error(
            "polling_not_supported",
            "whatsapp uses webhook ingress; poll_ingress is not supported",
        )),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::IngressEvent {
            config, payload, ..
        } => handle_ingress_event(config, payload),
        PluginRequest::Status { config, update } => Ok(PluginResponse::StatusAccepted {
            status: send_status(config, update)?,
        }),
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        META_ACCESS_TOKEN_ENV.to_string(),
        access_token_env(config).to_string(),
    );
    if let Some(phone_number_id) = &config.phone_number_id {
        metadata.insert(META_PHONE_NUMBER_ID.to_string(), phone_number_id.clone());
    }
    metadata.insert(
        META_API_VERSION.to_string(),
        api_version(config).to_string(),
    );
    if let Some(endpoint) = resolved_endpoint(config) {
        validate_url(&endpoint, "whatsapp webhook endpoint")?;
        metadata.insert(META_INGRESS_MODE.to_string(), "webhook".to_string());
        metadata.insert(META_WEBHOOK_ENDPOINT.to_string(), endpoint);
        metadata.insert(
            META_VERIFY_TOKEN_ENV.to_string(),
            verify_token_env(config).to_string(),
        );
    }
    if config.app_secret_env.is_some() {
        metadata.insert(
            META_APP_SECRET_ENV.to_string(),
            app_secret_env(config).to_string(),
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
        allowed_conversation_ids: Vec::new(),
        dm_policy: config.dm_policy.clone(),
        require_signature_validation: Some(config.app_secret_env.is_some()),
        allow_group_messages: None,
        max_attachment_bytes: None,
        metadata: BTreeMap::new(),
    }
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let client = WhatsAppClient::from_env(access_token_env(config), Some(api_version(config)))?;
    let identity = client.identity()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_WHATSAPP.to_string());
    metadata.insert(
        META_API_VERSION.to_string(),
        api_version(config).to_string(),
    );
    if let Some(phone_number_id) = &config.phone_number_id {
        metadata.insert(META_PHONE_NUMBER_ID.to_string(), phone_number_id.clone());
    }

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: Some(identity.id),
        display_name: Some(
            identity
                .name
                .unwrap_or_else(|| "whatsapp-business".to_string()),
        ),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    let endpoint = resolved_endpoint(config)
        .ok_or_else(|| anyhow!("whatsapp ingress requires webhook_public_url"))?;
    validate_url(&endpoint, "whatsapp webhook endpoint")?;

    let client = WhatsAppClient::from_env(access_token_env(config), Some(api_version(config)))?;
    let identity = client.identity()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_WHATSAPP.to_string());
    metadata.insert(
        META_HOST_ACTION.to_string(),
        "configure the Meta webhook callback URL, validate hub.verify_token against the configured verify token, and optionally verify X-Hub-Signature-256 if an app secret is available".to_string(),
    );
    metadata.insert(
        META_VERIFY_TOKEN_ENV.to_string(),
        verify_token_env(config).to_string(),
    );
    metadata.insert(
        META_SIGNATURE_HEADER.to_string(),
        HEADER_X_HUB_SIGNATURE_256.to_string(),
    );
    metadata.insert(META_ACCOUNT_ID.to_string(), identity.id);
    if has_optional_env(app_secret_env(config)) {
        metadata.insert(
            META_APP_SECRET_ENV.to_string(),
            app_secret_env(config).to_string(),
        );
    }

    Ok(IngressState {
        mode: IngressMode::Webhook,
        status: "configured".to_string(),
        endpoint: Some(endpoint),
        metadata,
    })
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    if let Some(endpoint) = resolved_endpoint(config) {
        validate_url(&endpoint, "whatsapp webhook endpoint")?;
    }

    let mut stopped = state.unwrap_or(IngressState {
        mode: IngressMode::Webhook,
        status: "configured".to_string(),
        endpoint: resolved_endpoint(config),
        metadata: BTreeMap::new(),
    });
    stopped.status = "stopped".to_string();
    stopped
        .metadata
        .insert(META_PLATFORM.to_string(), PLATFORM_WHATSAPP.to_string());
    Ok(stopped)
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let to_number = resolve_to_number(message)?;
    let phone_number_id = config
        .phone_number_id
        .as_deref()
        .ok_or_else(|| anyhow!("whatsapp delivery requires config.phone_number_id"))?;
    let client = WhatsAppClient::from_env(access_token_env(config), Some(api_version(config)))?;
    let reply_to_message_id = resolve_reply_to_message_id(message);
    let (sent, message_type) = match message.attachments.as_slice() {
        [] => (
            client.send_message(
                phone_number_id,
                &to_number,
                &message.content,
                reply_to_message_id.as_deref(),
            )?,
            "text",
        ),
        [attachment] => {
            let media_type = whatsapp_media_type(attachment);
            let media_ref = whatsapp_media_ref(&client, phone_number_id, attachment)?;
            let caption = whatsapp_caption(media_type, message)?;
            let filename = matches!(media_type, WhatsAppMediaType::Document)
                .then_some(attachment.name.as_str());
            let sent = client.send_media_message(
                phone_number_id,
                &to_number,
                WhatsAppMediaRequest {
                    media_type,
                    media_ref: media_ref.as_media_ref(),
                    caption,
                    filename,
                    reply_to_message_id: reply_to_message_id.as_deref(),
                },
            )?;
            (sent, whatsapp_media_type_name(media_type))
        }
        _ => bail!("whatsapp delivery supports at most one attachment per message"),
    };

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_WHATSAPP.to_string());
    metadata.insert(META_TO_NUMBER.to_string(), sent.to_number.clone());
    metadata.insert(
        META_PHONE_NUMBER_ID.to_string(),
        phone_number_id.to_string(),
    );
    metadata.insert(META_MESSAGE_TYPE.to_string(), message_type.to_string());

    Ok(DeliveryReceipt {
        message_id: sent.id,
        conversation_id: sent.to_number,
        metadata,
    })
}

fn handle_ingress_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if payload.method.eq_ignore_ascii_case("GET") {
        return handle_verification_request(config, payload);
    }
    if !payload.method.eq_ignore_ascii_case("POST") {
        return Ok(ingress_rejection(
            405,
            "whatsapp ingress expects GET verification or POST webhooks",
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

    let webhook: WhatsAppWebhookPayload = match serde_json::from_str(&payload.body) {
        Ok(webhook) => webhook,
        Err(_) => return Ok(ingress_rejection(400, "invalid WhatsApp webhook payload")),
    };

    let events = extract_inbound_events(config, payload, &webhook)?;
    Ok(PluginResponse::IngressEventsReceived {
        events,
        callback_reply: None,
        state: None,
        poll_after_ms: None,
    })
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let content = render_status_message(update);
    if content.trim().is_empty() {
        return Ok(rejected_status(
            "missing_message",
            "whatsapp status frames require a message or whatsapp_status_text override",
        ));
    }

    let message = OutboundMessage {
        content,
        to_number: update
            .conversation_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_CONVERSATION_ID).cloned()),
        reply_to_message_id: if config.reply_to_message {
            update.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned()
        } else {
            None
        },
        attachments: Vec::new(),
        metadata: BTreeMap::new(),
    };
    let delivery = match deliver(config, &message) {
        Ok(delivery) => delivery,
        Err(error) => return Ok(rejected_status("delivery_failed", error.to_string())),
    };

    let mut metadata = delivery.metadata.clone();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_WHATSAPP.to_string());
    metadata.insert(META_STATUS_KIND.to_string(), status_kind_name(&update.kind));
    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

fn handle_verification_request(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    let Some(mode) = payload.query.get("hub.mode") else {
        return Ok(ingress_rejection(
            400,
            "whatsapp verification requires hub.mode",
        ));
    };
    if mode != "subscribe" {
        return Ok(ingress_rejection(
            400,
            "whatsapp verification requires hub.mode=subscribe",
        ));
    }

    let expected_verify_token = std::env::var(verify_token_env(config)).with_context(|| {
        format!(
            "{} is required for whatsapp verification",
            verify_token_env(config)
        )
    })?;
    let actual_verify_token = payload
        .query
        .get("hub.verify_token")
        .ok_or_else(|| anyhow!("whatsapp verification requires hub.verify_token"))?;
    if actual_verify_token != &expected_verify_token {
        return Ok(ingress_rejection(
            403,
            "whatsapp verification token mismatch",
        ));
    }

    let Some(challenge) = payload.query.get("hub.challenge") else {
        return Ok(ingress_rejection(
            400,
            "whatsapp verification requires hub.challenge",
        ));
    };

    Ok(PluginResponse::IngressEventsReceived {
        events: Vec::new(),
        callback_reply: Some(IngressCallbackReply {
            status: 200,
            content_type: Some("text/plain; charset=utf-8".to_string()),
            body: challenge.clone(),
        }),
        state: None,
        poll_after_ms: None,
    })
}

fn validate_ingress_signature(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let Ok(app_secret) = std::env::var(app_secret_env(config)) else {
        return Ok(None);
    };
    let Some(signature_header) = header_value(&payload.headers, HEADER_X_HUB_SIGNATURE_256) else {
        return Ok(Some(callback_reply(
            403,
            "whatsapp signature header missing",
        )));
    };
    let Some(signature_hex) = signature_header.strip_prefix("sha256=") else {
        return Ok(Some(callback_reply(
            403,
            "whatsapp signature must use sha256= format",
        )));
    };
    let signature =
        hex::decode(signature_hex).map_err(|_| anyhow!("invalid whatsapp signature header"))?;

    let mut mac = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes())
        .context("failed to initialize WhatsApp signature verifier")?;
    mac.update(payload.body.as_bytes());
    if mac.verify_slice(&signature).is_err() {
        return Ok(Some(callback_reply(403, "whatsapp signature mismatch")));
    }
    Ok(None)
}

fn extract_inbound_events(
    config: &ChannelConfig,
    payload: &IngressPayload,
    webhook: &WhatsAppWebhookPayload,
) -> Result<Vec<InboundEventEnvelope>> {
    let mut events = Vec::new();

    for entry in &webhook.entry {
        for change in &entry.changes {
            let Some(value) = &change.value else {
                continue;
            };
            for message in &value.messages {
                let received_at =
                    received_at(payload.received_at.as_deref(), message.timestamp.as_deref())?;
                let actor_id = message.from.clone();
                let event_metadata = build_event_metadata(payload, entry, change, value, message);
                let content = message_content(message);
                let mut message_metadata = BTreeMap::from([(
                    META_MESSAGE_TYPE.to_string(),
                    message
                        .message_type
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                )]);
                let attachments = extract_attachments(message, &mut message_metadata);
                let display_name = contact_name(&value.contacts, &actor_id);
                if !sender_is_allowed(config, &actor_id) {
                    continue;
                }
                let parent_message_id = message
                    .context
                    .as_ref()
                    .and_then(|context| context.id.clone());

                events.push(InboundEventEnvelope {
                    event_id: message.id.clone(),
                    platform: PLATFORM_WHATSAPP.to_string(),
                    event_type: "message".to_string(),
                    received_at,
                    conversation: InboundConversationRef {
                        id: actor_id.clone(),
                        kind: "phone_number".to_string(),
                        thread_id: None,
                        parent_message_id: parent_message_id.clone(),
                    },
                    actor: InboundActor {
                        id: actor_id,
                        display_name,
                        username: None,
                        is_bot: false,
                        metadata: BTreeMap::new(),
                    },
                    message: InboundMessage {
                        id: message.id.clone(),
                        content,
                        content_type: "text/plain".to_string(),
                        reply_to_message_id: parent_message_id,
                        attachments,
                        metadata: message_metadata,
                    },
                    account_id: value
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.phone_number_id.clone())
                        .or_else(|| entry.id.clone()),
                    metadata: event_metadata,
                });
            }
        }
    }

    Ok(events)
}

fn build_event_metadata(
    payload: &IngressPayload,
    entry: &WhatsAppEntry,
    change: &WhatsAppChange,
    value: &WhatsAppValue,
    message: &WhatsAppInboundMessage,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_TRANSPORT.to_string(), TRANSPORT_WEBHOOK.to_string());
    metadata.insert(
        META_MESSAGE_TYPE.to_string(),
        message
            .message_type
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
    );
    if let Some(endpoint_id) = &payload.endpoint_id {
        metadata.insert(META_ENDPOINT_ID.to_string(), endpoint_id.clone());
    }
    if !payload.path.is_empty() {
        metadata.insert(META_PATH.to_string(), payload.path.clone());
    }
    if let Some(entry_id) = &entry.id {
        metadata.insert(META_ENTRY_ID.to_string(), entry_id.clone());
    }
    if let Some(field) = &change.field {
        metadata.insert(META_FIELD.to_string(), field.clone());
    }
    if let Some(metadata_value) = &value.metadata {
        if let Some(phone_number_id) = &metadata_value.phone_number_id {
            metadata.insert(META_PHONE_NUMBER_ID.to_string(), phone_number_id.clone());
        }
        if let Some(display_phone_number) = &metadata_value.display_phone_number {
            metadata.insert(
                META_DISPLAY_PHONE_NUMBER.to_string(),
                display_phone_number.clone(),
            );
        }
    }
    metadata
}

fn message_content(message: &WhatsAppInboundMessage) -> String {
    if let Some(text) = &message.text {
        return text.body.clone();
    }
    if let Some(image) = &message.image
        && let Some(caption) = &image.caption
    {
        return caption.clone();
    }
    if let Some(document) = &message.document
        && let Some(caption) = &document.caption
    {
        return caption.clone();
    }
    if let Some(video) = &message.video
        && let Some(caption) = &video.caption
    {
        return caption.clone();
    }
    if let Some(button) = &message.button {
        return button.text.clone();
    }
    if let Some(interactive) = &message.interactive {
        if let Some(button_reply) = &interactive.button_reply {
            return button_reply.title.clone();
        }
        if let Some(list_reply) = &interactive.list_reply {
            return list_reply.title.clone();
        }
    }
    String::new()
}

fn sender_is_allowed(config: &ChannelConfig, sender_id: &str) -> bool {
    if let Some(owner_id) = config.owner_id.as_deref()
        && owner_id != sender_id
    {
        return false;
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

fn extract_attachments(
    message: &WhatsAppInboundMessage,
    message_metadata: &mut BTreeMap<String, String>,
) -> Vec<InboundAttachment> {
    let mut attachments = Vec::new();

    if let Some(image) = &message.image {
        attachments.push(whatsapp_attachment(
            &image.id,
            "image",
            image.mime_type.clone(),
            None,
            image.sha256.as_ref(),
            image.caption.clone(),
            image.animated.map(|value| ("animated", value)),
        ));
    }
    if let Some(document) = &message.document {
        attachments.push(whatsapp_attachment(
            &document.id,
            "file",
            document.mime_type.clone(),
            document.filename.clone(),
            document.sha256.as_ref(),
            document.caption.clone(),
            None,
        ));
    }
    if let Some(audio) = &message.audio {
        attachments.push(whatsapp_attachment(
            &audio.id,
            "audio",
            audio.mime_type.clone(),
            None,
            audio.sha256.as_ref(),
            None,
            None,
        ));
    }
    if let Some(video) = &message.video {
        attachments.push(whatsapp_attachment(
            &video.id,
            "video",
            video.mime_type.clone(),
            None,
            video.sha256.as_ref(),
            video.caption.clone(),
            None,
        ));
    }
    if let Some(sticker) = &message.sticker {
        attachments.push(whatsapp_attachment(
            &sticker.id,
            "image",
            sticker.mime_type.clone(),
            None,
            sticker.sha256.as_ref(),
            None,
            sticker.animated.map(|value| ("animated", value)),
        ));
    }

    if !attachments.is_empty() {
        message_metadata.insert(
            META_ATTACHMENT_COUNT.to_string(),
            attachments.len().to_string(),
        );
    }

    attachments
}

fn whatsapp_attachment(
    id: &str,
    kind: &str,
    mime_type: Option<String>,
    name: Option<String>,
    sha256: Option<&String>,
    extracted_text: Option<String>,
    bool_extra: Option<(&str, bool)>,
) -> InboundAttachment {
    let mut extras = BTreeMap::new();
    if let Some(sha256) = sha256 {
        extras.insert("sha256".to_string(), sha256.clone());
    }
    if let Some((key, value)) = bool_extra {
        extras.insert(key.to_string(), value.to_string());
    }
    InboundAttachment {
        id: Some(id.to_string()),
        kind: kind.to_string(),
        url: None,
        mime_type,
        size_bytes: None,
        name,
        storage_key: Some(whatsapp_media_storage_key(id)),
        extracted_text,
        extras,
    }
}

fn whatsapp_media_storage_key(id: &str) -> String {
    format!("whatsapp:media:{id}")
}

fn contact_name(contacts: &[WhatsAppContact], actor_id: &str) -> Option<String> {
    contacts
        .iter()
        .find(|contact| contact.wa_id.as_deref() == Some(actor_id))
        .or_else(|| contacts.first())
        .and_then(|contact| contact.profile.as_ref())
        .and_then(|profile| profile.name.clone())
}

fn received_at(host_received_at: Option<&str>, message_timestamp: Option<&str>) -> Result<String> {
    if let Some(host_received_at) = host_received_at {
        return Ok(host_received_at.to_string());
    }
    let Some(message_timestamp) = message_timestamp else {
        return Ok(Timestamp::now().to_string());
    };
    let timestamp = message_timestamp
        .parse::<i64>()
        .with_context(|| format!("invalid whatsapp message timestamp `{message_timestamp}`"))?;
    Ok(Timestamp::from_second(timestamp)
        .context("whatsapp message timestamp is out of range")?
        .to_string())
}

fn resolve_to_number(message: &OutboundMessage) -> Result<String> {
    message
        .to_number
        .clone()
        .or_else(|| message.metadata.get(ROUTE_CONVERSATION_ID).cloned())
        .ok_or_else(|| anyhow!("whatsapp delivery requires message.to_number"))
}

fn resolve_reply_to_message_id(message: &OutboundMessage) -> Option<String> {
    message
        .reply_to_message_id
        .clone()
        .or_else(|| message.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WhatsAppResolvedMediaRef {
    Link(String),
    Id(String),
}

impl WhatsAppResolvedMediaRef {
    fn as_media_ref(&self) -> WhatsAppMediaRef<'_> {
        match self {
            Self::Link(link) => WhatsAppMediaRef::Link(link),
            Self::Id(id) => WhatsAppMediaRef::Id(id),
        }
    }
}

fn whatsapp_media_ref(
    client: &WhatsAppClient,
    phone_number_id: &str,
    attachment: &OutboundAttachment,
) -> Result<WhatsAppResolvedMediaRef> {
    if let Some(data_base64) = attachment.data_base64.as_deref() {
        let data = BASE64_STANDARD.decode(data_base64).with_context(|| {
            format!(
                "invalid base64 attachment payload for `{}`",
                attachment.name
            )
        })?;
        let media_id = client.upload_media(
            phone_number_id,
            &WhatsAppUpload {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                data,
            },
        )?;
        return Ok(WhatsAppResolvedMediaRef::Id(media_id));
    }
    if let Some(url) = attachment.url.as_deref() {
        validate_url(url, "whatsapp attachment url")?;
        return Ok(WhatsAppResolvedMediaRef::Link(url.to_string()));
    }
    if let Some(storage_key) = attachment.storage_key.as_deref() {
        return Ok(WhatsAppResolvedMediaRef::Id(
            storage_key
                .strip_prefix("whatsapp:media:")
                .unwrap_or(storage_key)
                .to_string(),
        ));
    }
    bail!(
        "whatsapp attachment delivery requires attachment.data_base64, attachment.url, or attachment.storage_key"
    )
}

fn whatsapp_media_type(attachment: &OutboundAttachment) -> WhatsAppMediaType {
    if attachment.mime_type.starts_with("image/") {
        WhatsAppMediaType::Image
    } else if attachment.mime_type.starts_with("video/") {
        WhatsAppMediaType::Video
    } else if attachment.mime_type.starts_with("audio/") {
        WhatsAppMediaType::Audio
    } else {
        WhatsAppMediaType::Document
    }
}

fn whatsapp_media_type_name(media_type: WhatsAppMediaType) -> &'static str {
    match media_type {
        WhatsAppMediaType::Image => "image",
        WhatsAppMediaType::Document => "document",
        WhatsAppMediaType::Video => "video",
        WhatsAppMediaType::Audio => "audio",
    }
}

fn whatsapp_caption(
    media_type: WhatsAppMediaType,
    message: &OutboundMessage,
) -> Result<Option<&str>> {
    if message.content.is_empty() {
        return Ok(None);
    }
    if matches!(media_type, WhatsAppMediaType::Audio) {
        bail!("whatsapp audio attachments do not support captions; message.content must be empty");
    }
    Ok(Some(message.content.as_str()))
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

fn render_status_message(update: &StatusFrame) -> String {
    if let Some(status_text) = update.metadata.get(WHATSAPP_STATUS_TEXT) {
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
        .unwrap_or("/whatsapp/webhook")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn access_token_env(config: &ChannelConfig) -> &str {
    config
        .access_token_env
        .as_deref()
        .unwrap_or("WHATSAPP_ACCESS_TOKEN")
}

fn verify_token_env(config: &ChannelConfig) -> &str {
    config
        .verify_token_env
        .as_deref()
        .unwrap_or("WHATSAPP_VERIFY_TOKEN")
}

fn app_secret_env(config: &ChannelConfig) -> &str {
    config
        .app_secret_env
        .as_deref()
        .unwrap_or("WHATSAPP_APP_SECRET")
}

fn api_version(config: &ChannelConfig) -> &str {
    config.api_version.as_deref().unwrap_or("v18.0")
}

fn has_optional_env(name: &str) -> bool {
    std::env::var(name).is_ok()
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
struct WhatsAppWebhookPayload {
    #[serde(default)]
    entry: Vec<WhatsAppEntry>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    changes: Vec<WhatsAppChange>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppChange {
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    value: Option<WhatsAppValue>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppValue {
    #[serde(default)]
    metadata: Option<WhatsAppValueMetadata>,
    #[serde(default)]
    contacts: Vec<WhatsAppContact>,
    #[serde(default)]
    messages: Vec<WhatsAppInboundMessage>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppValueMetadata {
    #[serde(default)]
    display_phone_number: Option<String>,
    #[serde(default)]
    phone_number_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppContact {
    #[serde(default)]
    wa_id: Option<String>,
    #[serde(default)]
    profile: Option<WhatsAppProfile>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppProfile {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppInboundMessage {
    from: String,
    id: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type", default)]
    message_type: Option<String>,
    #[serde(default)]
    text: Option<WhatsAppText>,
    #[serde(default)]
    image: Option<WhatsAppMedia>,
    #[serde(default)]
    document: Option<WhatsAppDocument>,
    #[serde(default)]
    audio: Option<WhatsAppMedia>,
    #[serde(default)]
    video: Option<WhatsAppMedia>,
    #[serde(default)]
    sticker: Option<WhatsAppSticker>,
    #[serde(default)]
    context: Option<WhatsAppMessageContext>,
    #[serde(default)]
    button: Option<WhatsAppButton>,
    #[serde(default)]
    interactive: Option<WhatsAppInteractive>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppText {
    body: String,
}

#[derive(Debug, Deserialize)]
struct WhatsAppMedia {
    id: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    animated: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppDocument {
    id: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppSticker {
    id: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    animated: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppMessageContext {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppButton {
    text: String,
}

#[derive(Debug, Deserialize)]
struct WhatsAppInteractive {
    #[serde(default)]
    button_reply: Option<WhatsAppInteractiveReply>,
    #[serde(default)]
    list_reply: Option<WhatsAppInteractiveReply>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppInteractiveReply {
    title: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_payload(method: &str) -> IngressPayload {
        IngressPayload {
            endpoint_id: Some("whatsapp:webhook".to_string()),
            method: method.to_string(),
            path: "/whatsapp/webhook".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: String::new(),
            trust_verified: true,
            received_at: Some("2026-04-11T20:00:00Z".to_string()),
        }
    }

    #[test]
    fn verification_get_returns_challenge() {
        let mut payload = base_payload("GET");
        payload
            .query
            .insert("hub.mode".to_string(), "subscribe".to_string());
        payload
            .query
            .insert("hub.verify_token".to_string(), "secret-token".to_string());
        payload
            .query
            .insert("hub.challenge".to_string(), "challenge-value".to_string());
        unsafe {
            std::env::set_var("WHATSAPP_VERIFY_TOKEN", "secret-token");
        }

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("verification");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(events.is_empty());
                let reply = callback_reply.expect("challenge reply");
                assert_eq!(reply.status, 200);
                assert_eq!(reply.body, "challenge-value");
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let mut payload = base_payload("POST");
        payload.trust_verified = false;
        payload.body = "{\"entry\":[]}".to_string();
        payload.headers.insert(
            HEADER_X_HUB_SIGNATURE_256.to_string(),
            "sha256=0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        );
        unsafe {
            std::env::set_var("WHATSAPP_APP_SECRET", "app-secret");
        }

        let reply =
            validate_ingress_signature(&ChannelConfig::default(), &payload).expect("signature");
        let reply = reply.expect("rejection");
        assert_eq!(reply.status, 403);
        assert_eq!(reply.body, "whatsapp signature mismatch");
    }

    #[test]
    fn inbound_post_maps_message_event() {
        let mut payload = base_payload("POST");
        payload.body = r#"{
            "entry":[
                {
                    "id":"WABA123",
                    "changes":[
                        {
                            "field":"messages",
                            "value":{
                                "metadata":{
                                    "display_phone_number":"15551234567",
                                    "phone_number_id":"PN123"
                                },
                                "contacts":[
                                    {
                                        "wa_id":"15550001111",
                                        "profile":{"name":"Alice"}
                                    }
                                ],
                                "messages":[
                                    {
                                        "from":"15550001111",
                                        "id":"wamid.HBgN",
                                        "timestamp":"1712860000",
                                        "type":"text",
                                        "text":{"body":"hello from whatsapp"},
                                        "context":{"id":"wamid.parent"}
                                    }
                                ]
                            }
                        }
                    ]
                }
            ]
        }"#
        .to_string();

        let response = handle_ingress_event(&ChannelConfig::default(), &payload).expect("ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(callback_reply.is_none());
                assert_eq!(events.len(), 1);
                let event = &events[0];
                assert_eq!(event.event_id, "wamid.HBgN");
                assert_eq!(event.platform, PLATFORM_WHATSAPP);
                assert_eq!(event.conversation.id, "15550001111");
                assert_eq!(event.conversation.kind, "phone_number");
                assert_eq!(event.actor.display_name.as_deref(), Some("Alice"));
                assert_eq!(event.message.content, "hello from whatsapp");
                assert_eq!(
                    event.message.reply_to_message_id.as_deref(),
                    Some("wamid.parent")
                );
                assert_eq!(event.account_id.as_deref(), Some("PN123"));
                assert_eq!(
                    event.metadata.get(META_PHONE_NUMBER_ID).map(String::as_str),
                    Some("PN123")
                );
                assert_eq!(
                    event.metadata.get(META_ENDPOINT_ID).map(String::as_str),
                    Some("whatsapp:webhook")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn inbound_post_preserves_media_attachments_without_urls() {
        let mut payload = base_payload("POST");
        payload.body = r#"{
            "entry":[
                {
                    "id":"WABA123",
                    "changes":[
                        {
                            "field":"messages",
                            "value":{
                                "metadata":{
                                    "display_phone_number":"15551234567",
                                    "phone_number_id":"PN123"
                                },
                                "contacts":[
                                    {
                                        "wa_id":"15550001111",
                                        "profile":{"name":"Alice"}
                                    }
                                ],
                                "messages":[
                                    {
                                        "from":"15550001111",
                                        "id":"wamid.MEDIA",
                                        "timestamp":"1712860000",
                                        "type":"image",
                                        "image":{
                                            "id":"media-123",
                                            "mime_type":"image/jpeg",
                                            "sha256":"abc123",
                                            "caption":"see attached"
                                        }
                                    }
                                ]
                            }
                        }
                    ]
                }
            ]
        }"#
        .to_string();

        let response = handle_ingress_event(&ChannelConfig::default(), &payload).expect("ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => {
                assert_eq!(events.len(), 1);
                let event = &events[0];
                assert_eq!(event.message.content, "see attached");
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
                assert_eq!(attachment.id.as_deref(), Some("media-123"));
                assert_eq!(attachment.kind, "image");
                assert!(attachment.url.is_none());
                assert_eq!(attachment.mime_type.as_deref(), Some("image/jpeg"));
                assert_eq!(
                    attachment.storage_key.as_deref(),
                    Some("whatsapp:media:media-123")
                );
                assert_eq!(
                    attachment.extras.get("sha256").map(String::as_str),
                    Some("abc123")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn inbound_post_drops_sender_when_pairing_policy_has_no_allowlist_match() {
        let mut payload = base_payload("POST");
        payload.body = r#"{
            "entry":[
                {
                    "id":"WABA123",
                    "changes":[
                        {
                            "field":"messages",
                            "value":{
                                "metadata":{
                                    "display_phone_number":"15551234567",
                                    "phone_number_id":"PN123"
                                },
                                "contacts":[
                                    {
                                        "wa_id":"15550001111",
                                        "profile":{"name":"Alice"}
                                    }
                                ],
                                "messages":[
                                    {
                                        "from":"15550001111",
                                        "id":"wamid.HBgN",
                                        "timestamp":"1712860000",
                                        "type":"text",
                                        "text":{"body":"hello from whatsapp"}
                                    }
                                ]
                            }
                        }
                    ]
                }
            ]
        }"#
        .to_string();

        let response = handle_ingress_event(
            &ChannelConfig {
                dm_policy: Some("pairing".to_string()),
                ..ChannelConfig::default()
            },
            &payload,
        )
        .expect("ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => assert!(events.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn render_status_message_uses_override() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "ignored".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::from([(WHATSAPP_STATUS_TEXT.to_string(), "custom".to_string())]),
        };

        assert_eq!(render_status_message(&update), "custom");
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
    fn whatsapp_media_ref_prefers_url_and_strips_media_prefix() {
        let client = WhatsAppClient::new_for_tests("http://127.0.0.1:9", "v18.0");
        let url_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: None,
            url: Some("https://example.com/photo.jpg".to_string()),
            storage_key: Some("whatsapp:media:media-123".to_string()),
        };
        assert_eq!(
            whatsapp_media_ref(&client, "PN123", &url_attachment).expect("url media ref"),
            WhatsAppResolvedMediaRef::Link("https://example.com/photo.jpg".to_string())
        );

        let stored_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: None,
            url: None,
            storage_key: Some("whatsapp:media:media-123".to_string()),
        };
        assert_eq!(
            whatsapp_media_ref(&client, "PN123", &stored_attachment).expect("stored media ref"),
            WhatsAppResolvedMediaRef::Id("media-123".to_string())
        );
    }

    #[test]
    fn whatsapp_media_ref_rejects_invalid_inline_data() {
        let client = WhatsAppClient::new_for_tests("http://127.0.0.1:9", "v18.0");
        let attachment = OutboundAttachment {
            name: "clip.mp3".to_string(),
            mime_type: "audio/mpeg".to_string(),
            data_base64: Some("%%%".to_string()),
            url: None,
            storage_key: None,
        };

        assert!(
            whatsapp_media_ref(&client, "PN123", &attachment)
                .expect_err("invalid inline data rejected")
                .to_string()
                .contains("invalid base64 attachment payload")
        );
    }

    #[test]
    fn whatsapp_caption_rejects_audio_caption_text() {
        let message = OutboundMessage {
            content: "listen to this".to_string(),
            to_number: Some("15551234567".to_string()),
            reply_to_message_id: None,
            attachments: Vec::new(),
            metadata: BTreeMap::new(),
        };

        assert!(
            whatsapp_caption(WhatsAppMediaType::Audio, &message)
                .expect_err("audio caption rejected")
                .to_string()
                .contains("do not support captions")
        );
    }
}
