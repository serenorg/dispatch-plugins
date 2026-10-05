use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use serde_json::Value;
use sha1::Sha1;
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};
use url::form_urlencoded;

mod bluebubbles_api;
mod protocol;

use bluebubbles_api::{BlueBubblesClient, Result, destination, is_dm_chat_guid, password_from_env};
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ConfiguredChannel, DeliveryReceipt,
    HealthReport, InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope,
    InboundMessage, IngressCallbackReply, IngressMode, IngressPayload, IngressState, MessageRef,
    OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse, StatusAcceptance,
    capabilities, parse_jsonrpc_request, plugin_error, response_to_jsonrpc,
};

fn main() -> io::Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (request_id, envelope) = parse_jsonrpc_request(&line)
            .map_err(|_| io::Error::other("invalid_channel_request"))?;
        let response = handle_request(&envelope).unwrap_or_else(|code| plugin_error(code, code));
        let json = response_to_jsonrpc(&request_id, &response)
            .map_err(|_| io::Error::other("invalid_channel_response"))?;
        writeln!(stdout, "{json}")?;
        stdout.flush()?;
        if matches!(envelope.request, PluginRequest::Shutdown) {
            break;
        }
    }
    Ok(())
}

fn handle_request(envelope: &PluginRequestEnvelope) -> Result<PluginResponse> {
    if envelope.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return Err("unsupported_protocol_version");
    }
    match &envelope.request {
        PluginRequest::Capabilities => Ok(PluginResponse::Capabilities {
            capabilities: capabilities(),
        }),
        PluginRequest::Configure { config } => {
            let _ = webhook_endpoint(config)?;
            Ok(PluginResponse::Configured {
                configuration: Box::new(ConfiguredChannel {
                    metadata: platform_metadata(),
                    policy: None,
                    runtime: None,
                }),
            })
        }
        PluginRequest::Health { config } => {
            BlueBubblesClient::from_env(config)?.ping()?;
            Ok(PluginResponse::Health {
                health: HealthReport {
                    ok: true,
                    status: "ok".to_string(),
                    account_id: None,
                    display_name: None,
                    metadata: platform_metadata(),
                },
            })
        }
        PluginRequest::StartIngress { config, .. } => {
            let state = ingress_state(config, "configured")?;
            BlueBubblesClient::from_env(config)?.ping()?;
            Ok(PluginResponse::IngressStarted { state })
        }
        PluginRequest::StopIngress { config, .. } => Ok(PluginResponse::IngressStopped {
            state: ingress_state(config, "stopped")?,
        }),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::IngressEvent {
            config, payload, ..
        } => {
            let password = if payload.trust_verified {
                None
            } else {
                password_from_env(config).ok()
            };
            Ok(handle_ingress_event(payload, password.as_deref()))
        }
        PluginRequest::PollIngress { .. } => Err("polling_not_supported"),
        PluginRequest::GetMessage { .. } | PluginRequest::GetPermalink { .. } => {
            Err("unsupported_request")
        }
        PluginRequest::Status { .. } => Ok(PluginResponse::StatusAccepted {
            status: StatusAcceptance {
                accepted: false,
                metadata: BTreeMap::from([(
                    "reason_code".to_string(),
                    "not_supported".to_string(),
                )]),
            },
        }),
        PluginRequest::Shutdown => Ok(PluginResponse::Ok),
    }
}

fn platform_metadata() -> BTreeMap<String, String> {
    BTreeMap::from([("platform".to_string(), "imessage".to_string())])
}

fn webhook_endpoint(config: &ChannelConfig) -> Result<Option<String>> {
    config
        .webhook_public_url
        .as_deref()
        .map(|value| {
            let mut url = bluebubbles_api::validate_base_url(value)
                .map_err(|_| "imessage_invalid_webhook_url")?;
            url.set_path(&format!("{}/imessage", url.path().trim_end_matches('/')));
            Ok(url.to_string())
        })
        .transpose()
}

fn ingress_state(config: &ChannelConfig, status: &str) -> Result<IngressState> {
    Ok(IngressState {
        mode: IngressMode::Webhook,
        status: status.to_string(),
        endpoint: webhook_endpoint(config)?,
        metadata: platform_metadata(),
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let conversation_id = destination(message)?.to_string();
    let message_id = BlueBubblesClient::from_env(config)?.send_message(message)?;
    Ok(DeliveryReceipt {
        reference: MessageRef {
            conversation_id,
            message_id,
            thread_id: None,
        },
        metadata: platform_metadata(),
    })
}

fn handle_ingress_event(payload: &IngressPayload, password: Option<&str>) -> PluginResponse {
    if !payload.trust_verified && !verify_password(payload, password) {
        return ingress_reply(401, vec![]);
    }
    if !payload.method.eq_ignore_ascii_case("POST") {
        return ingress_reply(405, vec![]);
    }
    let Ok(body) = serde_json::from_str::<Value>(&payload.body) else {
        return ingress_reply(400, vec![]);
    };
    ingress_reply(
        200,
        build_inbound_event(&body, payload).into_iter().collect(),
    )
}

fn verify_password(payload: &IngressPayload, expected: Option<&str>) -> bool {
    let Some(expected) = expected.filter(|value| !value.is_empty()) else {
        return false;
    };
    // Prefer the original query when available, so duplicate passwords fail closed.
    let actual = if let Some(raw) = &payload.raw_query {
        let mut passwords =
            form_urlencoded::parse(raw.as_bytes()).filter(|(name, _)| name == "password");
        let first = passwords.next().map(|(_, value)| value.into_owned());
        if passwords.next().is_some() {
            return false;
        }
        first
    } else {
        payload.query.get("password").cloned()
    };
    let Some(actual) = actual else {
        return false;
    };
    // Compare fixed-size MACs via the crypto library's constant-time verifier.
    // The fixed key is domain separation, not a secret or a provider signature.
    let Ok(mut expected_mac) = Hmac::<Sha1>::new_from_slice(b"channel-imessage/password") else {
        return false;
    };
    let mut actual_mac = expected_mac.clone();
    expected_mac.update(expected.as_bytes());
    actual_mac.update(actual.as_bytes());
    actual_mac
        .verify_slice(&expected_mac.finalize().into_bytes())
        .is_ok()
}

fn ingress_reply(status: u16, events: Vec<InboundEventEnvelope>) -> PluginResponse {
    PluginResponse::IngressEventsReceived {
        events,
        callback_reply: Some(IngressCallbackReply {
            status,
            content_type: Some("text/plain".to_string()),
            body: if status == 200 { "ok" } else { "rejected" }.to_string(),
        }),
        state: None,
        poll_after_ms: None,
    }
}

fn build_inbound_event(body: &Value, payload: &IngressPayload) -> Option<InboundEventEnvelope> {
    if body.get("type")?.as_str()? != "new-message" {
        return None;
    }
    let message = body.get("data")?;
    if message.get("isFromMe")?.as_bool()? {
        return None;
    }
    let actor = message
        .get("handle")?
        .get("address")?
        .as_str()
        .filter(|s| !s.is_empty())?;
    let id = message.get("guid")?.as_str().filter(|s| !s.is_empty())?;
    let listed_chat = message
        .get("chats")
        .and_then(Value::as_array)
        .and_then(|chats| chats.first())
        .and_then(|chat| chat.get("guid"))
        .and_then(Value::as_str);
    let direct_chat = message.get("chatGuid").and_then(Value::as_str);
    // Conflicting coordinates must never let a group event become a DM.
    if let (Some(listed), Some(direct)) = (listed_chat, direct_chat)
        && listed != direct
    {
        return None;
    }
    let chat_guid = listed_chat
        .or(direct_chat)
        .filter(|guid| is_dm_chat_guid(guid))?;
    let text = message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let attachments = message
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(attachment)
        .collect::<Vec<_>>();
    if text.is_empty() && attachments.is_empty() {
        return None;
    }
    Some(InboundEventEnvelope {
        event_id: id.to_string(),
        platform: "imessage".to_string(),
        event_type: "incoming_message".to_string(),
        received_at: payload
            .received_at
            .clone()
            .unwrap_or_else(|| Timestamp::now().to_string()),
        conversation: InboundConversationRef {
            id: chat_guid.to_string(),
            kind: "direct_message".to_string(),
            thread_id: None,
            parent_message_id: None,
            workspace_id: None,
            parent_conversation_id: None,
        },
        actor: InboundActor {
            id: actor.to_string(),
            display_name: None,
            username: None,
            is_bot: false,
            metadata: BTreeMap::new(),
        },
        message: InboundMessage {
            id: id.to_string(),
            content: text.to_string(),
            content_type: "text/plain".to_string(),
            reply_to_message_id: None,
            attachments,
            metadata: BTreeMap::new(),
        },
        account_id: None,
        activation: None,
        metadata: BTreeMap::from([("transport".to_string(), "webhook".to_string())]),
    })
}

fn attachment(value: &Value) -> Option<InboundAttachment> {
    value.as_object()?;
    let mime_type = value.get("mimeType").and_then(Value::as_str);
    let kind = match mime_type.and_then(|mime| mime.split('/').next()) {
        Some("image") => "image",
        Some("audio") => "audio",
        Some("video") => "video",
        _ => "file",
    };
    Some(InboundAttachment {
        id: value
            .get("guid")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        kind: kind.to_string(),
        url: None,
        mime_type: mime_type.map(ToOwned::to_owned),
        size_bytes: value.get("totalBytes").and_then(Value::as_u64),
        name: value
            .get("transferName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        storage_key: None,
        extracted_text: None,
        extras: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        // OpenClaw monitor.test.ts's new-message shape, with the chat coordinate
        // required for DM routing and attachment metadata from monitor.ts.
        json!({
            "type": "new-message",
            "data": {
                "text": "hello", "handle": {"address": "+15551234567"},
                "isGroup": false, "isFromMe": false, "guid": "msg-1", "date": 123456,
                "chats": [{"guid": "iMessage;-;+15551234567"}],
                "attachments": [{"guid": "attachment-1", "mimeType": "image/png",
                    "transferName": "photo.png", "totalBytes": 42}]
            }
        })
    }

    fn payload(body: Value) -> IngressPayload {
        IngressPayload {
            endpoint_id: Some("route-1".to_string()),
            method: "POST".to_string(),
            path: "/webhooks/route-1".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::from([("password".to_string(), "test-secret".to_string())]),
            raw_query: None,
            body: body.to_string(),
            trust_verified: false,
            received_at: Some("2026-10-05T12:00:00Z".to_string()),
        }
    }

    fn outcome(payload: &IngressPayload, secret: Option<&str>) -> (Vec<InboundEventEnvelope>, u16) {
        let PluginResponse::IngressEventsReceived {
            events,
            callback_reply,
            ..
        } = handle_ingress_event(payload, secret)
        else {
            panic!("expected ingress response");
        };
        (events, callback_reply.expect("HTTP reply").status)
    }

    #[test]
    fn new_message_preserves_provider_coordinates_and_attachment_metadata() {
        let request = payload(fixture());
        let (events, status) = outcome(&request, Some("test-secret"));
        assert_eq!(status, 200);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.actor.id, "+15551234567");
        assert_eq!(event.conversation.id, "iMessage;-;+15551234567");
        assert_eq!(event.conversation.thread_id, None);
        assert_eq!(event.message.content, "hello");
        assert_eq!(event.message.id, "msg-1");
        assert_eq!(event.event_id, "msg-1");
        assert_eq!(event.received_at, "2026-10-05T12:00:00Z");
        assert_eq!(
            event.message.attachments,
            vec![InboundAttachment {
                id: Some("attachment-1".to_string()),
                kind: "image".to_string(),
                url: None,
                mime_type: Some("image/png".to_string()),
                size_bytes: Some(42),
                name: Some("photo.png".to_string()),
                storage_key: None,
                extracted_text: None,
                extras: BTreeMap::new(),
            }]
        );
        assert!(
            !serde_json::to_string(event)
                .unwrap()
                .contains("test-secret")
        );

        let mut email = fixture();
        email["data"]["handle"]["address"] = json!("Customer@Example.com");
        email["data"].as_object_mut().unwrap().remove("chats");
        email["data"]["chatGuid"] = json!("iMessage;-;Customer@Example.com");
        email["data"]["text"] = Value::Null;
        let (events, status) = outcome(&payload(email), Some("test-secret"));
        assert_eq!(status, 200);
        assert_eq!(events[0].actor.id, "Customer@Example.com");
        assert_eq!(events[0].conversation.id, "iMessage;-;Customer@Example.com");
        assert_eq!(events[0].message.attachments.len(), 1);
    }

    #[test]
    fn ignored_events_and_failed_passwords_never_emit_messages() {
        for case in [
            "self",
            "group",
            "updated",
            "missing_chat",
            "conflicting_chat",
            "missing_direction",
        ] {
            let mut body = fixture();
            match case {
                "self" => body["data"]["isFromMe"] = json!(true),
                "group" => body["data"]["chats"][0]["guid"] = json!("iMessage;+;group-1"),
                "updated" => body["type"] = json!("updated-message"),
                "missing_chat" => {
                    body["data"].as_object_mut().unwrap().remove("chats");
                }
                "conflicting_chat" => body["data"]["chatGuid"] = json!("iMessage;+;group-1"),
                "missing_direction" => {
                    body["data"].as_object_mut().unwrap().remove("isFromMe");
                }
                _ => unreachable!(),
            }
            let (events, status) = outcome(&payload(body), Some("test-secret"));
            assert_eq!(status, 200, "{case}");
            assert!(events.is_empty(), "{case}");
        }
        for supplied in [
            None,
            Some("wrong-secret"),
            Some("test-secreu"),
            Some("test-secret-extra"),
        ] {
            let mut request = payload(fixture());
            request.query.clear();
            if let Some(value) = supplied {
                request
                    .query
                    .insert("password".to_string(), value.to_string());
            }
            let (events, status) = outcome(&request, Some("test-secret"));
            assert_eq!(status, 401);
            assert!(events.is_empty());
        }
        for configured in [None, Some("")] {
            let (events, status) = outcome(&payload(fixture()), configured);
            assert_eq!(status, 401);
            assert!(events.is_empty());
        }
    }

    #[test]
    fn raw_query_auth_and_host_trust_are_explicit() {
        let mut request = payload(fixture());
        request.query.clear();
        request.raw_query = Some("password=p%2B%26%3D%20word".to_string());
        assert_eq!(outcome(&request, Some("p+&= word")).0.len(), 1);
        request.raw_query = Some("password=test-secret&password=wrong".to_string());
        assert_eq!(outcome(&request, Some("test-secret")).1, 401);
        request.raw_query = None;
        request
            .headers
            .insert("x-password".to_string(), "test-secret".to_string());
        assert_eq!(outcome(&request, Some("test-secret")).1, 401);
        request.trust_verified = true;
        assert_eq!(outcome(&request, None).0.len(), 1);
        request.body = "{invalid".to_string();
        let (events, status) = outcome(&request, None);
        assert!(events.is_empty());
        assert_eq!(status, 400);
    }
}
