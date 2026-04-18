use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

mod protocol;
mod signal_api;

use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ConfiguredChannel, DeliveryReceipt,
    HealthReport, InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope,
    InboundMessage, IngressMode, IngressState, OutboundMessage, PluginRequest,
    PluginRequestEnvelope, PluginResponse, StatusAcceptance, StatusFrame, StatusKind, capabilities,
    plugin_error,
};
use signal_api::{SignalAttachmentPayload, SignalClient, SignalReceiveTransport, parse_target};

const META_PLATFORM: &str = "platform";
const META_BASE_URL: &str = "base_url";
const META_BASE_URL_ENV: &str = "base_url_env";
const META_ACCOUNT: &str = "account";
const META_DEFAULT_RECIPIENT: &str = "default_recipient";
const META_VERSION: &str = "version";
const META_BACKEND_MODE: &str = "backend_mode";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_POLL_TIMEOUT_SECS: &str = "poll_timeout_secs";
const META_REASON: &str = "reason";
const META_REASON_CODE: &str = "reason_code";
const META_STATUS_KIND: &str = "status_kind";
const META_SIGNAL_SOURCE_DEVICE: &str = "signal_source_device";
const META_SIGNAL_SOURCE_UUID: &str = "signal_source_uuid";
const META_SIGNAL_TIMESTAMP_MS: &str = "signal_timestamp_ms";
const META_SIGNAL_CONTENT_ORIGIN: &str = "signal_content_origin";
const META_TRANSPORT: &str = "transport";
const TRANSPORT_WEBSOCKET: &str = "websocket";

const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const PLATFORM_SIGNAL: &str = "signal";
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
        PluginRequest::PollIngress { config, .. } => poll_ingress(config),
        PluginRequest::IngressEvent { .. } => Ok(plugin_error(
            "ingress_not_supported",
            "signal uses poll_ingress; ingress_event is not supported",
        )),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Status { config, update } => Ok(PluginResponse::StatusAccepted {
            status: send_status(config, update)?,
        }),
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let client = signal_client(config)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SIGNAL.to_string());
    metadata.insert(META_BASE_URL.to_string(), client_base_url(config)?);
    metadata.insert(
        META_BASE_URL_ENV.to_string(),
        base_url_env(config).to_string(),
    );
    metadata.insert(
        META_INGRESS_MODE.to_string(),
        ingress_mode_name(IngressMode::Polling),
    );
    metadata.insert(
        META_POLL_TIMEOUT_SECS.to_string(),
        poll_timeout_secs(config).to_string(),
    );
    if let Some(account) = &config.account {
        metadata.insert(META_ACCOUNT.to_string(), account.clone());
    }
    if let Some(default_recipient) = &config.default_recipient {
        metadata.insert(
            META_DEFAULT_RECIPIENT.to_string(),
            default_recipient.clone(),
        );
    }

    let _ = client;
    Ok(ConfiguredChannel {
        metadata,
        policy: None,
        runtime: None,
    })
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let client = signal_client(config)?;
    let state = client.health()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SIGNAL.to_string());
    metadata.insert(META_BASE_URL.to_string(), client_base_url(config)?);
    if let Some(version) = state.version.clone() {
        metadata.insert(META_VERSION.to_string(), version);
    }
    if let Some(mode) = state.mode.clone() {
        metadata.insert(META_BACKEND_MODE.to_string(), mode);
    }
    if let Some(account) = &config.account {
        metadata.insert(META_ACCOUNT.to_string(), account.clone());
    }

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: config.account.clone(),
        display_name: Some("signal-cli".to_string()),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    let _ = inbound_account(config)?;
    let _ = signal_client(config)?;
    polling_state(config, "running")
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    let mut stopped = state.unwrap_or(polling_state(config, "running")?);
    stopped.mode = IngressMode::Polling;
    stopped.status = "stopped".to_string();
    Ok(stopped)
}

fn poll_ingress(config: &ChannelConfig) -> Result<PluginResponse> {
    let account = inbound_account(config)?;
    let client = signal_client(config)?;
    let (raw_messages, receive_transport) = client.receive_messages(poll_timeout_secs(config))?;
    let mut events = Vec::new();
    for raw_message in &raw_messages {
        if let Some(event) = build_inbound_event(raw_message, &account, receive_transport)? {
            events.push(event);
        }
    }

    Ok(PluginResponse::IngressEventsReceived {
        events,
        callback_reply: None,
        state: Some(polling_state(config, "running")?),
        poll_after_ms: Some(1_000),
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let recipient = resolve_recipient(config, message)?;
    let target = parse_target(&recipient)?;
    let client = signal_client(config)?;
    let attachments = signal_attachments(message)?;
    let sent = client.send_message(&target, &message.content, &attachments)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_SIGNAL.to_string());
    metadata.insert(ROUTE_CONVERSATION_ID.to_string(), recipient.clone());

    Ok(DeliveryReceipt {
        message_id: sent.message_id,
        conversation_id: recipient,
        metadata,
    })
}

fn signal_attachments(message: &OutboundMessage) -> Result<Vec<SignalAttachmentPayload>> {
    let mut attachments = Vec::with_capacity(message.attachments.len());
    for attachment in &message.attachments {
        let data_base64 = match attachment.data_base64.clone() {
            Some(data_base64) => data_base64,
            None if attachment.url.is_some() => {
                bail!(
                    "signal outbound attachments require data_base64; url attachments are not supported"
                );
            }
            None if attachment.storage_key.is_some() => {
                bail!(
                    "signal outbound attachments require data_base64; storage_key attachments are not supported"
                );
            }
            None => {
                bail!(
                    "signal outbound attachments require data_base64 for attachment `{}`",
                    attachment.name
                );
            }
        };
        attachments.push(SignalAttachmentPayload {
            name: attachment.name.clone(),
            data_base64,
        });
    }
    Ok(attachments)
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let recipient = update
        .conversation_id
        .as_deref()
        .or_else(|| {
            update
                .metadata
                .get(ROUTE_CONVERSATION_ID)
                .map(String::as_str)
        })
        .or(config.default_recipient.as_deref())
        .ok_or_else(|| {
            anyhow!(
                "signal status frames require update.conversation_id or config.default_recipient"
            )
        });

    let recipient = match recipient {
        Ok(recipient) => recipient.to_string(),
        Err(error) => {
            return Ok(rejected_status(
                "missing_conversation_id",
                error.to_string(),
            ));
        }
    };

    let client = signal_client(config)?;
    let target = parse_target(&recipient)?;
    if let Err(error) = client.send_typing(&target) {
        return Ok(rejected_status("delivery_failed", error.to_string()));
    }

    Ok(StatusAcceptance {
        accepted: true,
        metadata: BTreeMap::from([
            (META_PLATFORM.to_string(), PLATFORM_SIGNAL.to_string()),
            (ROUTE_CONVERSATION_ID.to_string(), recipient),
            (META_STATUS_KIND.to_string(), status_kind_name(&update.kind)),
        ]),
    })
}

fn build_inbound_event(
    raw_message: &Value,
    account: &str,
    receive_transport: SignalReceiveTransport,
) -> Result<Option<InboundEventEnvelope>> {
    let envelope = raw_message.get("envelope").unwrap_or(raw_message);
    if envelope.get("receiptMessage").is_some()
        || envelope.get("typingMessage").is_some()
        || envelope.get("syncMessage").is_some()
    {
        return Ok(None);
    }

    let Some(data_message) = envelope.get("dataMessage") else {
        return Ok(None);
    };

    let source_id = string_field(envelope, &["sourceNumber", "source", "sourceUuid"])
        .ok_or_else(|| anyhow!("signal receive payload is missing source identity"))?;
    let conversation_id =
        string_field(data_message, &["groupInfo.groupId"]).unwrap_or_else(|| source_id.clone());
    let conversation_kind = if string_field(data_message, &["groupInfo.groupId"]).is_some() {
        "group"
    } else {
        "private"
    };

    let attachments = attachments(data_message);
    let content = string_field(data_message, &["message"]).unwrap_or_default();
    if content.is_empty() && attachments.is_empty() {
        return Ok(None);
    }

    let timestamp_ms = number_field(data_message, &["timestamp"])
        .or_else(|| number_field(envelope, &["timestamp"]))
        .unwrap_or_else(|| Timestamp::now().as_millisecond());
    let message_id = timestamp_ms.to_string();
    let received_at = received_at(timestamp_ms)?;
    let source_uuid = string_field(envelope, &["sourceUuid"]);
    let source_device = number_field(envelope, &["sourceDevice"]).map(|value| value.to_string());

    let mut actor_metadata = BTreeMap::new();
    if let Some(source_uuid) = &source_uuid {
        actor_metadata.insert(META_SIGNAL_SOURCE_UUID.to_string(), source_uuid.clone());
    }
    if let Some(source_device) = &source_device {
        actor_metadata.insert(META_SIGNAL_SOURCE_DEVICE.to_string(), source_device.clone());
    }

    let mut message_metadata = BTreeMap::new();
    if content.is_empty() {
        message_metadata.insert(
            META_SIGNAL_CONTENT_ORIGIN.to_string(),
            "attachment".to_string(),
        );
    } else {
        message_metadata.insert(META_SIGNAL_CONTENT_ORIGIN.to_string(), "text".to_string());
    }

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(META_PLATFORM.to_string(), PLATFORM_SIGNAL.to_string());
    event_metadata.insert(
        META_TRANSPORT.to_string(),
        signal_transport_name(receive_transport).to_string(),
    );
    event_metadata.insert(
        META_SIGNAL_TIMESTAMP_MS.to_string(),
        timestamp_ms.to_string(),
    );
    if let Some(source_uuid) = &source_uuid {
        event_metadata.insert(META_SIGNAL_SOURCE_UUID.to_string(), source_uuid.clone());
    }
    if let Some(source_device) = &source_device {
        event_metadata.insert(META_SIGNAL_SOURCE_DEVICE.to_string(), source_device.clone());
    }

    Ok(Some(InboundEventEnvelope {
        event_id: format!("signal:{conversation_id}:{message_id}"),
        platform: PLATFORM_SIGNAL.to_string(),
        event_type: "message.received".to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: conversation_id.clone(),
            kind: conversation_kind.to_string(),
            thread_id: None,
            parent_message_id: None,
        },
        actor: InboundActor {
            id: source_id,
            display_name: string_field(envelope, &["sourceName"]),
            username: source_uuid,
            is_bot: false,
            metadata: actor_metadata,
        },
        message: InboundMessage {
            id: message_id,
            content,
            content_type: "text/plain".to_string(),
            reply_to_message_id: None,
            attachments,
            metadata: message_metadata,
        },
        account_id: Some(account.to_string()),
        metadata: event_metadata,
    }))
}

fn signal_transport_name(transport: SignalReceiveTransport) -> &'static str {
    match transport {
        SignalReceiveTransport::Polling => TRANSPORT_POLLING,
        SignalReceiveTransport::Websocket => TRANSPORT_WEBSOCKET,
    }
}

fn signal_client(config: &ChannelConfig) -> Result<SignalClient> {
    Ok(SignalClient::new(
        client_base_url(config)?,
        config.account.clone(),
    ))
}

fn client_base_url(config: &ChannelConfig) -> Result<String> {
    if let Some(base_url) = &config.base_url {
        return signal_api::normalize_base_url(base_url);
    }
    let value = std::env::var(base_url_env(config)).with_context(|| {
        format!(
            "{} is required unless config.base_url is set",
            base_url_env(config)
        )
    })?;
    signal_api::normalize_base_url(&value)
}

fn base_url_env(config: &ChannelConfig) -> &str {
    config.base_url_env.as_deref().unwrap_or("SIGNAL_RPC_URL")
}

fn inbound_account(config: &ChannelConfig) -> Result<String> {
    config
        .account
        .clone()
        .ok_or_else(|| anyhow!("signal polling ingress requires config.account"))
}

fn poll_timeout_secs(config: &ChannelConfig) -> u16 {
    config.poll_timeout_secs.unwrap_or(5).max(1)
}

fn polling_state(config: &ChannelConfig, status: &str) -> Result<IngressState> {
    let account = inbound_account(config)?;
    let mut metadata = BTreeMap::new();
    metadata.insert(META_ACCOUNT.to_string(), account);
    metadata.insert(
        META_POLL_TIMEOUT_SECS.to_string(),
        poll_timeout_secs(config).to_string(),
    );
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
        .unwrap_or_else(|| "unknown".to_string())
}

fn string_field(value: &Value, paths: &[&str]) -> Option<String> {
    for path in paths {
        let mut current = Some(value);
        for segment in path.split('.') {
            current = current.and_then(|value| value.get(segment));
        }
        let Some(current) = current else {
            continue;
        };
        if let Some(string) = current.as_str() {
            return Some(string.to_string());
        }
    }
    None
}

fn number_field(value: &Value, paths: &[&str]) -> Option<i64> {
    for path in paths {
        let mut current = Some(value);
        for segment in path.split('.') {
            current = current.and_then(|value| value.get(segment));
        }
        let Some(current) = current else {
            continue;
        };
        if let Some(number) = current.as_i64() {
            return Some(number);
        }
        if let Some(string) = current.as_str()
            && let Ok(number) = string.parse::<i64>()
        {
            return Some(number);
        }
    }
    None
}

fn attachments(data_message: &Value) -> Vec<InboundAttachment> {
    let Some(items) = data_message.get("attachments").and_then(Value::as_array) else {
        return Vec::new();
    };

    items
        .iter()
        .map(|attachment| InboundAttachment {
            id: string_field(attachment, &["id"]),
            kind: "file".to_string(),
            url: None,
            mime_type: string_field(attachment, &["contentType"]),
            size_bytes: number_field(attachment, &["size"]).map(|value| value as u64),
            name: string_field(attachment, &["filename"]),
            storage_key: string_field(attachment, &["storedFilename"]),
            extracted_text: None,
            extras: BTreeMap::new(),
        })
        .collect()
}

fn received_at(timestamp_ms: i64) -> Result<String> {
    Ok(Timestamp::from_millisecond(timestamp_ms)
        .context("signal message timestamp is out of range")?
        .to_string())
}

fn resolve_recipient(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
    message
        .metadata
        .get(ROUTE_CONVERSATION_ID)
        .cloned()
        .or_else(|| config.default_recipient.clone())
        .ok_or_else(|| anyhow!("signal delivery requires message.metadata.conversation_id or config.default_recipient"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_recipient_prefers_metadata_then_config_default() {
        let message = OutboundMessage {
            content: "reply".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: Vec::new(),
            metadata: BTreeMap::from([(
                ROUTE_CONVERSATION_ID.to_string(),
                "signal:+15551234567".to_string(),
            )]),
        };

        assert_eq!(
            resolve_recipient(&ChannelConfig::default(), &message).expect("recipient"),
            "signal:+15551234567"
        );
    }

    #[test]
    fn send_status_rejects_missing_destination() {
        let acceptance = send_status(
            &ChannelConfig::default(),
            &StatusFrame {
                kind: StatusKind::Info,
                message: "working".to_string(),
                conversation_id: None,
                thread_id: None,
                metadata: BTreeMap::new(),
            },
        )
        .expect("status result");

        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some("missing_conversation_id")
        );
    }

    #[test]
    fn build_inbound_event_parses_private_message() {
        let raw = json!({
            "envelope": {
                "sourceNumber": "+15551234567",
                "sourceName": "Alice",
                "sourceUuid": "user-123",
                "sourceDevice": 2,
                "timestamp": 1712880000123_i64,
                "dataMessage": {
                    "timestamp": 1712880000123_i64,
                    "message": "hello from signal"
                }
            }
        });

        let event = build_inbound_event(&raw, "+15557654321", SignalReceiveTransport::Polling)
            .expect("parse event")
            .expect("event present");

        assert_eq!(event.platform, PLATFORM_SIGNAL);
        assert_eq!(event.conversation.id, "+15551234567");
        assert_eq!(event.conversation.kind, "private");
        assert_eq!(event.actor.id, "+15551234567");
        assert_eq!(event.actor.display_name.as_deref(), Some("Alice"));
        assert_eq!(event.message.content, "hello from signal");
        assert_eq!(event.account_id.as_deref(), Some("+15557654321"));
        assert_eq!(
            event.metadata.get(META_TRANSPORT).map(String::as_str),
            Some("polling")
        );
    }

    #[test]
    fn build_inbound_event_parses_group_message() {
        let raw = json!({
            "envelope": {
                "sourceNumber": "+15551234567",
                "timestamp": 1712880000123_i64,
                "dataMessage": {
                    "timestamp": 1712880000123_i64,
                    "message": "hello group",
                    "groupInfo": {
                        "groupId": "group-1"
                    }
                }
            }
        });

        let event = build_inbound_event(&raw, "+15557654321", SignalReceiveTransport::Polling)
            .expect("parse event")
            .expect("event present");

        assert_eq!(event.conversation.id, "group-1");
        assert_eq!(event.conversation.kind, "group");
        assert_eq!(event.actor.id, "+15551234567");
    }

    #[test]
    fn build_inbound_event_marks_websocket_transport() {
        let raw = json!({
            "envelope": {
                "sourceNumber": "+15551234567",
                "timestamp": 1712880000123_i64,
                "dataMessage": {
                    "timestamp": 1712880000123_i64,
                    "message": "hello websocket"
                }
            }
        });

        let event = build_inbound_event(&raw, "+15557654321", SignalReceiveTransport::Websocket)
            .expect("parse event")
            .expect("event present");

        assert_eq!(
            event.metadata.get(META_TRANSPORT).map(String::as_str),
            Some("websocket")
        );
    }

    #[test]
    fn build_inbound_event_ignores_receipts() {
        let raw = json!({
            "envelope": {
                "sourceNumber": "+15551234567",
                "receiptMessage": {
                    "isDelivery": true
                }
            }
        });

        assert!(
            build_inbound_event(&raw, "+15557654321", SignalReceiveTransport::Polling)
                .expect("parse receipt")
                .is_none()
        );
    }

    #[test]
    fn start_ingress_requires_account() {
        let error = start_ingress(&ChannelConfig::default()).expect_err("missing account");
        assert!(error.to_string().contains("config.account"));
    }

    #[test]
    fn polling_state_reports_configured_timeout_secs() {
        let config = ChannelConfig {
            account: Some("+15557654321".to_string()),
            poll_timeout_secs: Some(60),
            ..ChannelConfig::default()
        };

        let state = polling_state(&config, "running").expect("state");

        assert_eq!(
            state
                .metadata
                .get(META_POLL_TIMEOUT_SECS)
                .map(String::as_str),
            Some("60")
        );
    }
}
