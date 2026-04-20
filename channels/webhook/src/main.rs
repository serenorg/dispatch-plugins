use anyhow::{Context, Result, anyhow};
use jiff::Timestamp;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    io::{self, BufRead, Write},
};

mod protocol;
mod webhook_api;

use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ConfiguredChannel, DeliveryReceipt,
    HealthReport, InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope,
    InboundMessage, IngressCallbackReply, IngressMode, IngressPayload, IngressState,
    OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse, StatusAcceptance,
    StatusFrame, StatusKind, capabilities, parse_jsonrpc_request, plugin_error,
    response_to_jsonrpc,
};

fn main() -> Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let (request_id, envelope) = parse_jsonrpc_request(&line)
            .map_err(|error| anyhow!("failed to parse channel request: {error}"))?;

        let response = match handle_request(&envelope) {
            Ok(response) => response,
            Err(error) => plugin_error("internal_error", error.to_string()),
        };

        let json = response_to_jsonrpc(&request_id, &response).map_err(|error| anyhow!(error))?;
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
        PluginRequest::PollIngress { .. } => Ok(plugin_error(
            "polling_not_supported",
            "webhook ingress is webhook-only; use ingress_event or listen bindings",
        )),
        PluginRequest::StartIngress { config, .. } => Ok(PluginResponse::IngressStarted {
            state: start_ingress(config)?,
        }),
        PluginRequest::StopIngress { config, state } => Ok(PluginResponse::IngressStopped {
            state: stop_ingress(config, state.clone())?,
        }),
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
        PluginRequest::Shutdown => Ok(PluginResponse::Ok),
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let mut metadata = BTreeMap::new();

    if let Some(outbound_url) = &config.outbound_url {
        validate_url(outbound_url, "outbound_url")?;
        metadata.insert("outbound_url".to_string(), outbound_url.clone());
    }
    if let Some(endpoint) = resolved_endpoint(config) {
        metadata.insert("ingress_mode".to_string(), "webhook".to_string());
        metadata.insert("webhook_endpoint".to_string(), endpoint);
    }
    if config.outbound_bearer_token_env.is_some() {
        metadata.insert(
            "outbound_bearer_token_env".to_string(),
            outbound_bearer_token_env(config).to_string(),
        );
    }
    if config.ingress_secret_env.is_some() || config.webhook_public_url.is_some() {
        metadata.insert(
            "ingress_secret_header".to_string(),
            ingress_secret_header(config).to_string(),
        );
        if has_optional_env(ingress_secret_env(config)) {
            metadata.insert(
                "ingress_secret_env".to_string(),
                ingress_secret_env(config).to_string(),
            );
        }
    }
    if let Some(healthcheck_url) = &config.healthcheck_url {
        validate_url(healthcheck_url, "healthcheck_url")?;
        metadata.insert("healthcheck_url".to_string(), healthcheck_url.clone());
    }
    metadata.insert(
        "static_header_count".to_string(),
        config.static_headers.len().to_string(),
    );

    Ok(ConfiguredChannel {
        metadata,
        policy: None,
        runtime: None,
    })
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    if let Some(outbound_url) = &config.outbound_url {
        validate_url(outbound_url, "outbound_url")?;
    }
    if let Some(healthcheck_url) = &config.healthcheck_url {
        validate_url(healthcheck_url, "healthcheck_url")?;
    }

    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), "webhook".to_string());
    if let Some(status_code) = webhook_api::healthcheck(config)? {
        metadata.insert("healthcheck_status".to_string(), status_code.to_string());
    } else {
        metadata.insert(
            "healthcheck_status".to_string(),
            "not_configured".to_string(),
        );
    }
    if let Some(endpoint) = resolved_endpoint(config) {
        metadata.insert("ingress_endpoint".to_string(), endpoint);
    }

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: None,
        display_name: Some("generic-webhook".to_string()),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    let endpoint = resolved_endpoint(config)
        .ok_or_else(|| anyhow!("webhook ingress requires webhook_public_url"))?;

    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), "webhook".to_string());
    metadata.insert(
        "host_action".to_string(),
        "route inbound POST requests to the reported endpoint and optionally verify the shared secret header".to_string(),
    );
    metadata.insert(
        "ingress_secret_header".to_string(),
        ingress_secret_header(config).to_string(),
    );
    if has_optional_env(ingress_secret_env(config)) {
        let _ = webhook_api::ingress_secret(config)?;
        metadata.insert(
            "ingress_secret_env".to_string(),
            ingress_secret_env(config).to_string(),
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
        validate_url(&endpoint, "resolved ingress endpoint")?;
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
        .insert("platform".to_string(), "webhook".to_string());
    Ok(stopped)
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let (message_id, conversation_id) = webhook_api::deliver(config, message)?;

    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), "webhook".to_string());
    if let Some(destination_url) = &message.destination_url {
        metadata.insert("destination_url".to_string(), destination_url.clone());
    } else if let Some(outbound_url) = &config.outbound_url {
        metadata.insert("destination_url".to_string(), outbound_url.clone());
    }
    if let Some(subject) = &message.subject {
        metadata.insert("subject".to_string(), subject.clone());
    }

    Ok(DeliveryReceipt {
        message_id,
        conversation_id,
        metadata,
    })
}

fn handle_ingress_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if let Some(reply) = validate_ingress_secret(config, payload)? {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(reply),
            state: None,
            poll_after_ms: None,
        });
    }

    let body = parse_payload_body(&payload.body);
    let content_type = content_type(payload);
    let received_at = received_at(payload.received_at.as_deref())?;
    let event = build_inbound_event(config, payload, body.as_ref(), &content_type, &received_at)?;

    Ok(PluginResponse::IngressEventsReceived {
        events: vec![event],
        callback_reply: None,
        state: None,
        poll_after_ms: None,
    })
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let destination = update
        .metadata
        .get("destination_url")
        .or(config.outbound_url.as_ref())
        .ok_or_else(|| {
            anyhow!(
                "webhook status frames require update.metadata.destination_url or config.outbound_url"
            )
        })?;

    webhook_api::send_status(config, destination, update)?;

    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), "webhook".to_string());
    metadata.insert("destination_url".to_string(), destination.clone());
    metadata.insert("status_kind".to_string(), status_kind_name(&update.kind));
    if let Some(conversation_id) = &update.conversation_id {
        metadata.insert("conversation_id".to_string(), conversation_id.clone());
    }
    if let Some(thread_id) = &update.thread_id {
        metadata.insert("thread_id".to_string(), thread_id.clone());
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
        .unwrap_or("/webhook/inbound")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn ingress_secret_env(config: &ChannelConfig) -> &str {
    config
        .ingress_secret_env
        .as_deref()
        .unwrap_or("WEBHOOK_INGRESS_SECRET")
}

fn ingress_secret_header(config: &ChannelConfig) -> &str {
    config
        .ingress_secret_header
        .as_deref()
        .unwrap_or("X-Dispatch-Webhook-Secret")
}

fn outbound_bearer_token_env(config: &ChannelConfig) -> &str {
    config
        .outbound_bearer_token_env
        .as_deref()
        .unwrap_or("WEBHOOK_OUTBOUND_BEARER_TOKEN")
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

fn validate_ingress_secret(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let Ok(expected_secret) = webhook_api::ingress_secret(config) else {
        return Ok(None);
    };

    let Some(actual_secret) = header_value(&payload.headers, ingress_secret_header(config)) else {
        return Ok(Some(callback_reply(
            403,
            "webhook ingress secret header missing",
        )));
    };

    if actual_secret != expected_secret {
        return Ok(Some(callback_reply(403, "webhook ingress secret mismatch")));
    }

    Ok(None)
}

fn build_inbound_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
    body: Option<&Value>,
    content_type: &str,
    received_at: &str,
) -> Result<InboundEventEnvelope> {
    let conversation = resolve_conversation(payload, body);
    let actor = resolve_actor(payload, body);
    let message = resolve_message(payload, body, content_type);
    let event_type = body
        .and_then(|value| lookup_string(value, &["event_type", "type"]))
        .unwrap_or_else(|| "webhook.received".to_string());
    let account_id = body.and_then(|value| lookup_string(value, &["account_id"]));
    let event_id = body
        .and_then(|value| lookup_string(value, &["event_id"]))
        .or_else(|| header_value(&payload.headers, "X-Dispatch-Event-Id"))
        .unwrap_or_else(|| generated_event_id(payload, &message.id));

    let mut metadata = BTreeMap::new();
    metadata.insert("transport".to_string(), "webhook".to_string());
    metadata.insert("method".to_string(), payload.method.clone());
    metadata.insert("path".to_string(), payload.path.clone());
    metadata.insert("content_type".to_string(), content_type.to_string());
    metadata.insert(
        "secret_validated".to_string(),
        payload.trust_verified.to_string(),
    );
    if let Some(endpoint_id) = &payload.endpoint_id {
        metadata.insert("endpoint_id".to_string(), endpoint_id.clone());
    }
    if let Some(configured_endpoint) = resolved_endpoint(config) {
        metadata.insert("configured_endpoint".to_string(), configured_endpoint);
    }
    if !payload.query.is_empty() {
        metadata.insert(
            "query_keys".to_string(),
            payload.query.keys().cloned().collect::<Vec<_>>().join(","),
        );
    }
    if body.is_some() {
        metadata.insert("body_format".to_string(), "json".to_string());
    } else {
        metadata.insert("body_format".to_string(), "text".to_string());
    }

    Ok(InboundEventEnvelope {
        event_id,
        platform: "webhook".to_string(),
        event_type,
        received_at: received_at.to_string(),
        conversation,
        actor,
        message,
        account_id,
        metadata,
    })
}

fn resolve_conversation(payload: &IngressPayload, body: Option<&Value>) -> InboundConversationRef {
    let id = body
        .and_then(|value| lookup_string(value, &["conversation_id"]))
        .or_else(|| nested_string(body, &["conversation", "id"]))
        .or_else(|| payload.query.get("conversation_id").cloned())
        .or_else(|| header_value(&payload.headers, "X-Dispatch-Conversation-Id"))
        .or_else(|| payload.endpoint_id.clone())
        .unwrap_or_else(|| payload.path.clone());

    let kind = nested_string(body, &["conversation", "kind"])
        .or_else(|| body.and_then(|value| lookup_string(value, &["conversation_kind"])))
        .unwrap_or_else(|| "webhook".to_string());

    let thread_id = body
        .and_then(|value| lookup_string(value, &["thread_id"]))
        .or_else(|| nested_string(body, &["conversation", "thread_id"]))
        .or_else(|| payload.query.get("thread_id").cloned())
        .or_else(|| header_value(&payload.headers, "X-Dispatch-Thread-Id"));

    let parent_message_id = body
        .and_then(|value| lookup_string(value, &["parent_message_id"]))
        .or_else(|| nested_string(body, &["conversation", "parent_message_id"]));

    InboundConversationRef {
        id,
        kind,
        thread_id,
        parent_message_id,
    }
}

fn resolve_actor(payload: &IngressPayload, body: Option<&Value>) -> InboundActor {
    let id = body
        .and_then(|value| lookup_string(value, &["actor_id"]))
        .or_else(|| nested_string(body, &["actor", "id"]))
        .or_else(|| header_value(&payload.headers, "X-Dispatch-Actor-Id"))
        .unwrap_or_else(|| "webhook-sender".to_string());

    let display_name = body
        .and_then(|value| lookup_string(value, &["actor_display_name"]))
        .or_else(|| nested_string(body, &["actor", "display_name"]));
    let username = body
        .and_then(|value| lookup_string(value, &["actor_username"]))
        .or_else(|| nested_string(body, &["actor", "username"]));
    let is_bot = body
        .and_then(|value| lookup_bool(value, &["actor_is_bot"]))
        .or_else(|| nested_bool(body, &["actor", "is_bot"]))
        .unwrap_or(false);

    let mut metadata = BTreeMap::new();
    if let Some(actor_type) = body
        .and_then(|value| lookup_string(value, &["actor_type"]))
        .or_else(|| nested_string(body, &["actor", "type"]))
    {
        metadata.insert("type".to_string(), actor_type);
    }

    InboundActor {
        id,
        display_name,
        username,
        is_bot,
        metadata,
    }
}

fn resolve_message(
    payload: &IngressPayload,
    body: Option<&Value>,
    default_content_type: &str,
) -> InboundMessage {
    let id = body
        .and_then(|value| lookup_string(value, &["message_id"]))
        .or_else(|| nested_string(body, &["message", "id"]))
        .or_else(|| header_value(&payload.headers, "X-Dispatch-Message-Id"))
        .unwrap_or_else(|| generated_message_id(payload));

    let content = body
        .and_then(message_content_from_json)
        .unwrap_or_else(|| payload.body.clone());

    let content_type = body
        .and_then(|value| lookup_string(value, &["content_type"]))
        .or_else(|| nested_string(body, &["message", "content_type"]))
        .unwrap_or_else(|| default_content_type.to_string());

    let reply_to_message_id = body
        .and_then(|value| lookup_string(value, &["reply_to_message_id"]))
        .or_else(|| nested_string(body, &["message", "reply_to_message_id"]));

    let attachments = body.map(extract_attachments).unwrap_or_default();

    let mut metadata = BTreeMap::new();
    if let Some(subject) = body
        .and_then(|value| lookup_string(value, &["subject"]))
        .or_else(|| nested_string(body, &["message", "subject"]))
    {
        metadata.insert("subject".to_string(), subject);
    }

    InboundMessage {
        id,
        content,
        content_type,
        reply_to_message_id,
        attachments,
        metadata,
    }
}

fn extract_attachments(body: &Value) -> Vec<InboundAttachment> {
    let attachments = body
        .get("attachments")
        .and_then(Value::as_array)
        .or_else(|| {
            body.get("message")
                .and_then(|message| message.get("attachments"))
                .and_then(Value::as_array)
        });

    attachments
        .into_iter()
        .flatten()
        .filter_map(|attachment| {
            let url = attachment.get("url").and_then(Value::as_str)?.to_string();
            Some(InboundAttachment {
                id: attachment.get("id").and_then(string_value),
                kind: attachment
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("file")
                    .to_string(),
                url: Some(url),
                mime_type: attachment.get("mime_type").and_then(string_value),
                size_bytes: attachment.get("size_bytes").and_then(u64_value),
                name: attachment.get("name").and_then(string_value),
                storage_key: attachment.get("storage_key").and_then(string_value),
                extracted_text: attachment.get("extracted_text").and_then(string_value),
                extras: attachment
                    .get("extras")
                    .and_then(Value::as_object)
                    .map(string_map)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn parse_payload_body(body: &str) -> Option<Value> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

fn content_type(payload: &IngressPayload) -> String {
    header_value(&payload.headers, "Content-Type")
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("application/octet-stream")
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| "text/plain".to_string())
}

fn header_value(headers: &BTreeMap<String, String>, needle: &str) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(needle))
        .map(|(_, value)| value.clone())
}

fn callback_reply(status: u16, body: &str) -> IngressCallbackReply {
    IngressCallbackReply {
        status,
        content_type: Some("text/plain".to_string()),
        body: body.to_string(),
    }
}

fn lookup_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(string_value)
}

fn lookup_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(Value::as_bool)
}

fn nested_string(body: Option<&Value>, path: &[&str]) -> Option<String> {
    let mut current = body?;
    for segment in path {
        current = current.get(*segment)?;
    }
    string_value(current)
}

fn nested_bool(body: Option<&Value>, path: &[&str]) -> Option<bool> {
    let mut current = body?;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_bool()
}

fn string_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn u64_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn string_map(map: &serde_json::Map<String, Value>) -> BTreeMap<String, String> {
    map.iter()
        .filter_map(|(key, value)| string_value(value).map(|value| (key.clone(), value)))
        .collect()
}

fn message_content_from_json(body: &Value) -> Option<String> {
    body.get("message")
        .and_then(|message| message.get("content"))
        .and_then(string_value)
        .or_else(|| body.get("content").and_then(string_value))
}

fn generated_event_id(payload: &IngressPayload, message_id: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.method.hash(&mut hasher);
    payload.path.hash(&mut hasher);
    payload.body.hash(&mut hasher);
    message_id.hash(&mut hasher);
    if let Some(endpoint_id) = &payload.endpoint_id {
        endpoint_id.hash(&mut hasher);
    }
    format!("webhook-evt-{:016x}", hasher.finish())
}

fn generated_message_id(payload: &IngressPayload) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.method.hash(&mut hasher);
    payload.path.hash(&mut hasher);
    payload.body.hash(&mut hasher);
    format!("webhook-msg-{:016x}", hasher.finish())
}

fn received_at(value: Option<&str>) -> Result<String> {
    if let Some(value) = value {
        return Ok(value.to_string());
    }

    Ok(Timestamp::now().to_string())
}

fn status_kind_name(kind: &StatusKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| format!("{kind:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn ingress_event_parses_normalized_json_payload() {
        let config = ChannelConfig::default();
        let payload = IngressPayload {
            endpoint_id: Some("dispatch_webhook".to_string()),
            method: "POST".to_string(),
            path: "/dispatch/webhook".to_string(),
            headers: BTreeMap::from([(
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )]),
            query: BTreeMap::new(),
            raw_query: None,
            body: serde_json::json!({
                "event_id": "evt_123",
                "event_type": "message.created",
                "conversation": {
                    "id": "customer-123",
                    "kind": "external",
                    "thread_id": "thread-9"
                },
                "actor": {
                    "id": "acct-7",
                    "display_name": "Webhook Sender",
                    "username": "sender",
                    "is_bot": true
                },
                "message": {
                    "id": "msg-42",
                    "content": "hello from webhook",
                    "content_type": "text/plain",
                    "attachments": [
                        {
                            "id": "file-1",
                            "kind": "image",
                            "url": "https://example.com/image.png",
                            "mime_type": "image/png",
                            "size_bytes": 512
                        }
                    ]
                }
            })
            .to_string(),
            trust_verified: true,
            received_at: Some("2026-04-11T00:00:00Z".to_string()),
        };

        let response = handle_ingress_event(&config, &payload).expect("ingress succeeds");
        let PluginResponse::IngressEventsReceived {
            events,
            callback_reply,
            ..
        } = response
        else {
            panic!("expected ingress response");
        };

        assert!(callback_reply.is_none());
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_id, "evt_123");
        assert_eq!(event.event_type, "message.created");
        assert_eq!(event.platform, "webhook");
        assert_eq!(event.received_at, "2026-04-11T00:00:00Z");
        assert_eq!(event.conversation.id, "customer-123");
        assert_eq!(event.conversation.thread_id.as_deref(), Some("thread-9"));
        assert_eq!(event.actor.id, "acct-7");
        assert_eq!(event.actor.display_name.as_deref(), Some("Webhook Sender"));
        assert!(event.actor.is_bot);
        assert_eq!(event.message.id, "msg-42");
        assert_eq!(event.message.content, "hello from webhook");
        assert_eq!(event.message.content_type, "text/plain");
        assert_eq!(event.message.attachments.len(), 1);
        assert_eq!(
            event.metadata.get("endpoint_id").map(String::as_str),
            Some("dispatch_webhook")
        );
    }

    #[test]
    fn ingress_event_rejects_wrong_secret_without_emitting_events() {
        let config = ChannelConfig {
            ingress_secret_env: Some("WEBHOOK_TEST_INGRESS_SECRET".to_string()),
            ..ChannelConfig::default()
        };
        // Test-only environment mutation is scoped to this process.
        unsafe {
            std::env::set_var("WEBHOOK_TEST_INGRESS_SECRET", "expected-secret");
        }

        let payload = IngressPayload {
            endpoint_id: None,
            method: "POST".to_string(),
            path: "/dispatch/webhook".to_string(),
            headers: BTreeMap::from([(
                "X-Dispatch-Webhook-Secret".to_string(),
                "wrong-secret".to_string(),
            )]),
            query: BTreeMap::new(),
            raw_query: None,
            body: "{\"content\":\"hello\"}".to_string(),
            trust_verified: false,
            received_at: None,
        };

        let response = handle_ingress_event(&config, &payload).expect("ingress succeeds");
        let PluginResponse::IngressEventsReceived {
            events,
            callback_reply,
            ..
        } = response
        else {
            panic!("expected ingress response");
        };

        assert!(events.is_empty());
        let reply = callback_reply.expect("callback reply");
        assert_eq!(reply.status, 403);
        assert_eq!(reply.body, "webhook ingress secret mismatch");

        // Test-only environment mutation is scoped to this process.
        unsafe {
            std::env::remove_var("WEBHOOK_TEST_INGRESS_SECRET");
        }
    }

    #[test]
    fn status_sends_json_payload_to_outbound_url() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            stream
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("set read timeout");
            let mut request = String::new();
            let mut buffer = [0_u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => request.push_str(&String::from_utf8_lossy(&buffer[..read])),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(error) => panic!("read request: {error}"),
                }
            }
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            request
        });

        let config = ChannelConfig {
            outbound_url: Some(format!("http://{address}/status")),
            ..ChannelConfig::default()
        };
        let update = StatusFrame {
            kind: StatusKind::Processing,
            message: "Working".to_string(),
            conversation_id: Some("customer-123".to_string()),
            thread_id: Some("thread-9".to_string()),
            metadata: BTreeMap::new(),
        };

        let accepted = send_status(&config, &update).expect("status accepted");
        let request = handle.join().expect("join request thread");

        assert!(accepted.accepted);
        assert_eq!(
            accepted.metadata.get("status_kind").map(String::as_str),
            Some("processing")
        );
        assert!(request.starts_with("POST /status HTTP/1.1\r\n"));
        let body = request
            .split("\r\n\r\n")
            .nth(1)
            .expect("http body should be present");
        let body: Value = serde_json::from_str(body).expect("parse request body");
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("status"));
        assert_eq!(
            body.get("status_kind").and_then(Value::as_str),
            Some("processing")
        );
        assert_eq!(
            body.get("conversation_id").and_then(Value::as_str),
            Some("customer-123")
        );
    }
}
