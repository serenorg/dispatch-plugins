use dispatch_channel_protocol::parse_plugin_input;
use dispatch_channel_runtime::{IngressCycle, spawn_plugin_input_reader, write_stdout_line};
use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use serde_json::Value;
use sha1::Sha1;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};
use url::form_urlencoded;

mod bluebubbles_api;
mod protocol;

use bluebubbles_api::{
    BlueBubblesClient, ImessageDmChat, ImessageError, Result, password_from_env,
    validate_outbound_message,
};
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ConfiguredChannel, DeliveryReceipt,
    HealthReport, InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope,
    InboundMessage, IngressCallbackReply, IngressMode, IngressPayload, IngressState, MessageRef,
    OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse, StatusAcceptance,
    capabilities, plugin_error, response_to_jsonrpc,
};

fn main() -> io::Result<()> {
    let stdout_lock = Arc::new(Mutex::new(()));
    for request in spawn_plugin_input_reader(
        Arc::clone(&stdout_lock),
        parse_plugin_input::<ChannelConfig, OutboundMessage>,
    ) {
        let (request_id, envelope) = request.map_err(io::Error::other)?;
        let response = handle_request(&envelope, &stdout_lock)
            .unwrap_or_else(|error| plugin_error(error.code(), error.code()));
        let json = response_to_jsonrpc(&request_id, &response)
            .map_err(|_| io::Error::other("invalid_channel_response"))?;
        write_stdout_line(&stdout_lock, &json).map_err(io::Error::other)?;
        if matches!(envelope.request, PluginRequest::Shutdown) {
            break;
        }
    }
    Ok(())
}

fn handle_request(
    envelope: &PluginRequestEnvelope,
    stdout_lock: &Arc<Mutex<()>>,
) -> Result<PluginResponse> {
    if envelope.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return Err(ImessageError::UnsupportedProtocolVersion);
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
            let state = ingress_state(config, IngressStatus::Configured)?;
            BlueBubblesClient::from_env(config)?.ping()?;
            Ok(PluginResponse::IngressStarted { state })
        }
        PluginRequest::StopIngress { config, .. } => Ok(PluginResponse::IngressStopped {
            state: ingress_state(config, IngressStatus::Stopped)?,
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
            handle_ingress_event(payload, password.as_deref(), |chat| {
                BlueBubblesClient::from_env(config)?.verify_dm_chat(chat)
            })?
            .publish(stdout_lock)
            .map_err(|_| ImessageError::EventDeliveryFailed)
        }
        PluginRequest::PollIngress { .. } => Err(ImessageError::PollingNotSupported),
        PluginRequest::GetMessage { .. } | PluginRequest::GetPermalink { .. } => {
            Err(ImessageError::UnsupportedRequest)
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
                .map_err(|_| ImessageError::InvalidWebhookUrl)?;
            url.set_path(&format!("{}/imessage", url.path().trim_end_matches('/')));
            Ok(url.to_string())
        })
        .transpose()
}

enum IngressStatus {
    Configured,
    Stopped,
}

impl IngressStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Stopped => "stopped",
        }
    }
}

fn ingress_state(config: &ChannelConfig, status: IngressStatus) -> Result<IngressState> {
    Ok(IngressState {
        mode: IngressMode::Webhook,
        status: status.as_str().to_string(),
        endpoint: webhook_endpoint(config)?,
        metadata: platform_metadata(),
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let conversation_id = validate_outbound_message(message)?.guid().to_string();
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

fn handle_ingress_event(
    payload: &IngressPayload,
    password: Option<&str>,
    verify_chat: impl FnOnce(ImessageDmChat<'_>) -> Result<()>,
) -> Result<IngressCycle> {
    if !payload.trust_verified && !verify_password(payload, password) {
        return Ok(ingress_reply(401, vec![]));
    }
    if !payload.method.eq_ignore_ascii_case("POST") {
        return Ok(ingress_reply(405, vec![]));
    }
    let Ok(body) = serde_json::from_str::<Value>(&payload.body) else {
        return Ok(ingress_reply(400, vec![]));
    };
    let Some(event) = build_inbound_event(&body, payload) else {
        return Ok(ingress_reply(200, vec![]));
    };
    let chat = ImessageDmChat::parse(&event.conversation.id).ok_or(ImessageError::DmRequired)?;
    if chat.requires_provider_binding() {
        match verify_chat(chat) {
            Ok(()) => {}
            Err(ImessageError::DmRequired) => return Ok(ingress_reply(200, vec![])),
            Err(error) => return Err(error),
        }
    }
    Ok(ingress_reply(200, vec![event]))
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

fn ingress_reply(status: u16, events: Vec<InboundEventEnvelope>) -> IngressCycle {
    IngressCycle::Events {
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
    if let Some(is_group) = message.get("isGroup")
        && is_group.as_bool()?
    {
        return None;
    }
    let actor = message
        .get("handle")?
        .get("address")?
        .as_str()
        .filter(|s| !s.is_empty())?;
    let id = message.get("guid")?.as_str().filter(|s| !s.is_empty())?;
    let chat = inbound_chat(message, actor)?;
    if !chat.matches_message_service(message.get("handle")?.get("service")) {
        return None;
    }
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
            id: chat.guid().to_string(),
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

fn inbound_chat<'a>(message: &'a Value, sender: &str) -> Option<ImessageDmChat<'a>> {
    let mut coordinates = Vec::new();
    if let Some(chats) = message.get("chats") {
        for chat in chats.as_array()? {
            coordinates.push(chat.get("guid")?.as_str()?);
        }
    }
    if let Some(guid) = message.get("chatGuid") {
        coordinates.push(guid.as_str()?);
    }
    let first = ImessageDmChat::parse(coordinates.first()?)?;
    // Every supplied coordinate must bind the same DM to its sender.
    if !first.matches_sender(sender) || coordinates.iter().any(|guid| *guid != first.guid()) {
        return None;
    }
    Some(first)
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
    use bluebubbles_api::test_support::{HttpFixture, chat_response};
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
            path: "/imessage".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::from([("password".to_string(), "test-secret".to_string())]),
            raw_query: None,
            body: body.to_string(),
            trust_verified: false,
            received_at: Some("2026-10-05T12:00:00Z".to_string()),
        }
    }

    fn outcome(payload: &IngressPayload, secret: Option<&str>) -> (Vec<InboundEventEnvelope>, u16) {
        let IngressCycle::Events {
            events,
            callback_reply,
            ..
        } = handle_ingress_event(payload, secret, |_| {
            panic!("unexpected provider verification")
        })
        .unwrap()
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
        email["data"]["chatGuid"] = json!("iMessage;-;customer@example.com");
        email["data"]["text"] = Value::Null;
        let (events, status) = outcome(&payload(email), Some("test-secret"));
        assert_eq!(status, 200);
        assert_eq!(events[0].actor.id, "Customer@Example.com");
        assert_eq!(events[0].conversation.id, "iMessage;-;customer@example.com");
        assert_eq!(events[0].message.attachments.len(), 1);

        let mut other_email = fixture();
        other_email["data"]["handle"]["address"] = json!("other@example.com");
        other_email["data"]["chats"][0]["guid"] = json!("iMessage;-;customer@example.com");
        assert!(
            outcome(&payload(other_email), Some("test-secret"))
                .0
                .is_empty()
        );
    }

    #[test]
    fn chat_coordinates_must_agree_and_match_the_sender() {
        for chats in [
            json!([{"guid": "iMessage;-;+15557654321"}]),
            json!([
                {"guid": "iMessage;-;+15551234567"},
                {"guid": "iMessage;-;+15557654321"}
            ]),
            json!([
                {"guid": "iMessage;-;+15551234567"},
                {"guid": "iMessage;+;group-1"}
            ]),
            json!([
                {"guid": "iMessage;-;+15551234567"},
                {"guid": "SMS;-;+15551234567"}
            ]),
            json!([{"guid": "iMessage;-;+15551234567"}, {}]),
            json!("not an array"),
        ] {
            let mut body = fixture();
            body["data"]["chats"] = chats;
            body["data"]["chatGuid"] = json!("iMessage;-;+15551234567");
            let (events, status) = outcome(&payload(body), Some("test-secret"));
            assert_eq!(status, 200);
            assert!(events.is_empty());
        }

        for sender in ["+15557654321", "15551234567", "+1 (555) 123-4567", " "] {
            let mut body = fixture();
            body["data"]["handle"]["address"] = json!(sender);
            assert!(outcome(&payload(body), Some("test-secret")).0.is_empty());
        }

        let mut body = fixture();
        body["data"]["chats"] = json!([
            {"guid": "iMessage;-;+15551234567"},
            {"guid": "iMessage;-;+15551234567"}
        ]);
        body["data"]["chatGuid"] = json!("iMessage;-;+15551234567");
        let (events, status) = outcome(&payload(body), Some("test-secret"));
        assert_eq!(status, 200);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor.id, "+15551234567");
        assert_eq!(events[0].conversation.id, "iMessage;-;+15551234567");
    }

    #[test]
    fn ingress_accepts_only_imessage_dm_coordinates() {
        for guid in [
            "SMS;-;+15551234567",
            "other;-;+15551234567",
            "iMessage;-;",
            "iMessage;-;   ",
            "iMessage;+;group-1",
        ] {
            let mut body = fixture();
            body["data"]["chats"][0]["guid"] = json!(guid);
            let (events, status) = outcome(&payload(body), Some("test-secret"));
            assert_eq!(status, 200);
            assert!(events.is_empty(), "{guid}");
        }
    }

    #[test]
    fn deliver_and_push_reject_unsupported_messages_before_credentials() {
        let stdout_lock = Arc::new(Mutex::new(()));
        let config = ChannelConfig {
            bluebubbles_url_env: Some(String::new()),
            ..Default::default()
        };
        for guid in ["SMS;-;+15551234567", "other;-;+15551234567", "iMessage;-; "] {
            let message = OutboundMessage {
                content: "private text".to_string(),
                metadata: BTreeMap::from([("conversation_id".to_string(), guid.to_string())]),
                ..Default::default()
            };
            for request in [
                PluginRequest::Deliver {
                    config: config.clone(),
                    message: message.clone(),
                },
                PluginRequest::Push {
                    config: config.clone(),
                    message: message.clone(),
                },
            ] {
                let envelope = PluginRequestEnvelope {
                    protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                    request,
                };
                assert_eq!(
                    handle_request(&envelope, &stdout_lock).unwrap_err(),
                    ImessageError::DmRequired
                );
            }
        }
        let message: OutboundMessage = serde_json::from_value(json!({
            "content": "private text",
            "metadata": {"conversation_id": "iMessage;-;customer@example.com"},
            "attachments": [{"name": "photo.png", "mime_type": "image/png", "url": "https://example.com/photo.png"}]
        }))
        .unwrap();
        for request in [
            PluginRequest::Deliver {
                config: config.clone(),
                message: message.clone(),
            },
            PluginRequest::Push {
                config: config.clone(),
                message: message.clone(),
            },
        ] {
            let envelope = PluginRequestEnvelope {
                protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                request,
            };
            assert_eq!(
                handle_request(&envelope, &stdout_lock).unwrap_err(),
                ImessageError::OutboundAttachmentsUnsupported
            );
        }
    }

    #[test]
    fn ignored_events_and_failed_passwords_never_emit_messages() {
        for case in [
            "self",
            "group",
            "group_flag",
            "invalid_group_flag",
            "updated",
            "missing_chat",
            "conflicting_chat",
            "missing_direction",
        ] {
            let mut body = fixture();
            match case {
                "self" => body["data"]["isFromMe"] = json!(true),
                "group" => body["data"]["chats"][0]["guid"] = json!("iMessage;+;group-1"),
                "group_flag" => body["data"]["isGroup"] = json!(true),
                "invalid_group_flag" => body["data"]["isGroup"] = json!("false"),
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

    fn unspecified_chat_fixture() -> Value {
        let mut body = fixture();
        body["data"]["chats"][0]["guid"] = json!("any;-;+15551234567");
        body["data"]["handle"]["service"] = json!("iMessage");
        body
    }

    #[test]
    fn unspecified_ingress_requires_the_exact_provider_imessage_dm_binding() {
        let guid = "any;-;+15551234567";
        for (status, body, event_count, error) in [
            (200, chat_response(guid, 45).to_string(), 1, None),
            (200, json!({"status": 200, "data": []}).to_string(), 0, None),
            (
                200,
                chat_response(guid, 43).to_string(),
                0,
                Some(ImessageError::BlueBubblesInvalidResponse),
            ),
            (
                200,
                chat_response("any;-;+15557654321", 45).to_string(),
                0,
                Some(ImessageError::BlueBubblesInvalidResponse),
            ),
            (
                200,
                "{invalid".to_string(),
                0,
                Some(ImessageError::BlueBubblesInvalidResponse),
            ),
            (
                500,
                "private provider error".to_string(),
                0,
                Some(ImessageError::BlueBubblesChatVerificationFailed),
            ),
        ] {
            let server = HttpFixture::new(vec![(status, body)]);
            let client = server.client();
            let result = handle_ingress_event(
                &payload(unspecified_chat_fixture()),
                Some("test-secret"),
                |chat| client.verify_dm_chat(chat),
            );
            if let Some(error) = error {
                assert_eq!(result.unwrap_err(), error);
            } else {
                let IngressCycle::Events {
                    events,
                    callback_reply,
                    ..
                } = result.unwrap()
                else {
                    panic!("expected ingress events");
                };
                assert_eq!(callback_reply.unwrap().status, 200);
                assert_eq!(events.len(), event_count);
                if let Some(event) = events.first() {
                    assert_eq!(event.conversation.id, guid);
                    assert_eq!(event.actor.id, "+15551234567");
                    assert_eq!(event.message.content, "hello");
                }
            }
            let requests = server.recorded_requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].target,
                "/api/v1/message/query?password=fixture-secret"
            );
        }

        let mut body = unspecified_chat_fixture();
        body["data"]["handle"]
            .as_object_mut()
            .unwrap()
            .remove("service");
        let server = HttpFixture::new(vec![(200, chat_response(guid, 45).to_string())]);
        let client = server.client();
        let IngressCycle::Events { events, .. } =
            handle_ingress_event(&payload(body), Some("test-secret"), |chat| {
                client.verify_dm_chat(chat)
            })
            .unwrap()
        else {
            panic!("expected ingress events");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].conversation.id, guid);
        assert_eq!(server.recorded_requests().len(), 1);
    }

    #[test]
    fn unsupported_service_and_failed_auth_never_query_the_provider() {
        for guid in ["iMessage;-;+15551234567", "any;-;+15551234567"] {
            for service in [json!("SMS"), json!("RCS"), json!("unknown"), Value::Null] {
                let mut body = unspecified_chat_fixture();
                body["data"]["chats"][0]["guid"] = json!(guid);
                body["data"]["handle"]["service"] = service;
                let IngressCycle::Events {
                    events,
                    callback_reply,
                    ..
                } = handle_ingress_event(&payload(body), Some("test-secret"), |_| {
                    panic!("unsupported service must not query")
                })
                .unwrap()
                else {
                    panic!("expected ingress events");
                };
                assert!(events.is_empty());
                assert_eq!(callback_reply.unwrap().status, 200);
            }
        }
        let IngressCycle::Events {
            events,
            callback_reply,
            ..
        } = handle_ingress_event(
            &payload(unspecified_chat_fixture()),
            Some("wrong-secret"),
            |_| panic!("failed authentication must not query"),
        )
        .unwrap()
        else {
            panic!("expected ingress events");
        };
        assert!(events.is_empty());
        assert_eq!(callback_reply.unwrap().status, 401);
    }

    #[test]
    fn public_webhook_urls_require_https() {
        for url in [
            "http://public.example",
            "http://192.168.1.10",
            "http://localhost",
        ] {
            let config = ChannelConfig {
                webhook_public_url: Some(url.to_string()),
                ..Default::default()
            };
            assert_eq!(
                webhook_endpoint(&config),
                Err(ImessageError::InvalidWebhookUrl)
            );
        }
        for (base, endpoint) in [
            ("https://public.example", "https://public.example/imessage"),
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080/imessage"),
            ("http://[::1]:8080", "http://[::1]:8080/imessage"),
        ] {
            let config = ChannelConfig {
                webhook_public_url: Some(base.to_string()),
                ..Default::default()
            };
            assert_eq!(webhook_endpoint(&config), Ok(Some(endpoint.to_string())));
        }
    }
}
