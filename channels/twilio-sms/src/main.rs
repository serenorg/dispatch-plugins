use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use hmac::{Hmac, Mac};
use jiff::Timestamp;
use sha1::Sha1;
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};
use url::form_urlencoded;

mod protocol;
mod twilio_api;

use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ConfiguredChannel, DeliveryReceipt,
    HealthReport, InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope,
    InboundMessage, IngressCallbackReply, IngressMode, IngressPayload, IngressState,
    OutboundAttachment, OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse,
    StatusAcceptance, capabilities, parse_jsonrpc_request, plugin_error, response_to_jsonrpc,
};
use twilio_api::TwilioClient;

const META_REASON: &str = "reason";
const META_PLATFORM: &str = "platform";
const META_ACCOUNT_SID_ENV: &str = "account_sid_env";
const META_AUTH_TOKEN_ENV: &str = "auth_token_env";
const META_FROM_NUMBER: &str = "from_number";
const META_MESSAGING_SERVICE_SID: &str = "messaging_service_sid";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_WEBHOOK_ENDPOINT: &str = "webhook_endpoint";
const META_WEBHOOK_AUTH_TOKEN_ENV: &str = "webhook_auth_token_env";
const META_ALLOWED_FROM_COUNT: &str = "allowed_from_count";
const META_ACCOUNT_STATUS: &str = "account_status";
const META_HOST_ACTION: &str = "host_action";
const META_SIGNATURE_HEADER: &str = "signature_header";
const META_ACCOUNT_SID: &str = "account_sid";
const META_TO_NUMBER: &str = "to_number";
const META_MESSAGE_STATUS: &str = "message_status";
const META_ATTACHMENT_COUNT: &str = "attachment_count";
const META_TRANSPORT: &str = "transport";
const META_ENDPOINT_ID: &str = "endpoint_id";
const META_PATH: &str = "path";
const META_MESSAGE_SID: &str = "message_sid";
const META_NUM_MEDIA: &str = "num_media";
const META_SMS_STATUS: &str = "sms_status";
const META_SMS_MESSAGE_SID: &str = "sms_message_sid";
const META_API_VERSION: &str = "api_version";
const META_SMS_SID: &str = "sms_sid";
const META_PROFILE_NAME: &str = "profile_name";
const META_CHANNEL: &str = "channel";
const META_EVENT_DIRECTION: &str = "event_direction";
const META_REASON_CODE: &str = "reason_code";

const PLATFORM_TWILIO_SMS: &str = "twilio_sms";
const HEADER_X_TWILIO_SIGNATURE: &str = "X-Twilio-Signature";
const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const TRANSPORT_WEBHOOK: &str = "webhook";
const TWIML_EMPTY_RESPONSE: &str = "<Response></Response>";
const CONVERSATION_KIND_PHONE_NUMBER: &str = "phone_number";
const EVENT_TYPE_INCOMING_MESSAGE: &str = "incoming_message";

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
        PluginRequest::Status { .. } => Ok(PluginResponse::StatusAccepted {
            status: StatusAcceptance {
                accepted: false,
                metadata: BTreeMap::from([
                    (META_REASON_CODE.to_string(), "not_supported".to_string()),
                    (
                        META_REASON.to_string(),
                        "twilio sms status frames are not wired yet".to_string(),
                    ),
                ]),
            },
        }),
        PluginRequest::Shutdown => Ok(PluginResponse::Ok),
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        META_ACCOUNT_SID_ENV.to_string(),
        account_sid_env(config).to_string(),
    );
    metadata.insert(
        META_AUTH_TOKEN_ENV.to_string(),
        auth_token_env(config).to_string(),
    );
    if let Some(from_number) = &config.from_number {
        metadata.insert(META_FROM_NUMBER.to_string(), from_number.clone());
    }
    if let Some(messaging_service_sid) = &config.messaging_service_sid {
        metadata.insert(
            META_MESSAGING_SERVICE_SID.to_string(),
            messaging_service_sid.clone(),
        );
    }
    if let Some(endpoint) = resolved_endpoint(config) {
        validate_url(&endpoint, "twilio webhook endpoint")?;
        metadata.insert(
            META_INGRESS_MODE.to_string(),
            ingress_mode_name(IngressMode::Webhook),
        );
        metadata.insert(META_WEBHOOK_ENDPOINT.to_string(), endpoint);
    }
    if config.webhook_auth_token_env.is_some() {
        metadata.insert(
            META_WEBHOOK_AUTH_TOKEN_ENV.to_string(),
            webhook_auth_token_env(config).to_string(),
        );
    }
    if !config.allowed_from_numbers.is_empty() {
        metadata.insert(
            META_ALLOWED_FROM_COUNT.to_string(),
            config.allowed_from_numbers.len().to_string(),
        );
    }

    Ok(ConfiguredChannel {
        metadata,
        policy: None,
        runtime: None,
    })
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let client = TwilioClient::from_env(account_sid_env(config), auth_token_env(config))?;
    let identity = client.identity()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_TWILIO_SMS.to_string());
    if let Some(status) = &identity.status {
        metadata.insert(META_ACCOUNT_STATUS.to_string(), status.clone());
    }
    if let Some(from_number) = &config.from_number {
        metadata.insert(META_FROM_NUMBER.to_string(), from_number.clone());
    }
    if let Some(messaging_service_sid) = &config.messaging_service_sid {
        metadata.insert(
            META_MESSAGING_SERVICE_SID.to_string(),
            messaging_service_sid.clone(),
        );
    }

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: Some(identity.sid),
        display_name: Some(
            identity
                .friendly_name
                .unwrap_or_else(|| "twilio-account".to_string()),
        ),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    let endpoint = resolved_endpoint(config)
        .ok_or_else(|| anyhow!("twilio ingress requires webhook_public_url"))?;
    validate_url(&endpoint, "twilio webhook endpoint")?;

    let client = TwilioClient::from_env(account_sid_env(config), auth_token_env(config))?;
    let identity = client.identity()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_TWILIO_SMS.to_string());
    metadata.insert(
        META_HOST_ACTION.to_string(),
        "route Twilio inbound SMS webhooks to the reported endpoint and verify X-Twilio-Signature using the account auth token".to_string(),
    );
    metadata.insert(
        META_SIGNATURE_HEADER.to_string(),
        HEADER_X_TWILIO_SIGNATURE.to_string(),
    );
    metadata.insert(META_ACCOUNT_SID.to_string(), identity.sid);
    if let Some(status) = identity.status {
        metadata.insert(META_ACCOUNT_STATUS.to_string(), status);
    }
    if has_optional_env(webhook_auth_token_env(config)) {
        metadata.insert(
            META_WEBHOOK_AUTH_TOKEN_ENV.to_string(),
            webhook_auth_token_env(config).to_string(),
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
        validate_url(&endpoint, "twilio webhook endpoint")?;
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
        .insert(META_PLATFORM.to_string(), PLATFORM_TWILIO_SMS.to_string());
    Ok(stopped)
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let to_number = resolve_to_number(message)?;
    let media_urls = outbound_media_urls(message)?;
    let client = TwilioClient::from_env(account_sid_env(config), auth_token_env(config))?;
    let sent = client.send_message(
        &to_number,
        &message.content,
        config.from_number.as_deref(),
        config.messaging_service_sid.as_deref(),
        &media_urls,
    )?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_TWILIO_SMS.to_string());
    metadata.insert(META_TO_NUMBER.to_string(), sent.to.clone());
    if !media_urls.is_empty() {
        metadata.insert(
            META_ATTACHMENT_COUNT.to_string(),
            media_urls.len().to_string(),
        );
    }
    if let Some(from) = &sent.from {
        metadata.insert(META_FROM_NUMBER.to_string(), from.clone());
    }
    if let Some(status) = &sent.status {
        metadata.insert(META_MESSAGE_STATUS.to_string(), status.clone());
    }

    Ok(DeliveryReceipt {
        message_id: sent.sid,
        conversation_id: sent.to,
        metadata,
    })
}

fn handle_ingress_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if !payload.method.eq_ignore_ascii_case("POST") && !payload.method.eq_ignore_ascii_case("GET") {
        return Ok(ingress_reply(
            405,
            "twilio sms ingress expects GET or POST webhooks",
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

    let params = incoming_params(payload);
    let Some(event) = build_inbound_event(config, payload, &params)? else {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(twiml_empty_reply()),
            state: None,
            poll_after_ms: None,
        });
    };

    Ok(PluginResponse::IngressEventsReceived {
        events: vec![event],
        callback_reply: Some(twiml_empty_reply()),
        state: None,
        poll_after_ms: None,
    })
}

fn build_inbound_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
    params: &BTreeMap<String, String>,
) -> Result<Option<InboundEventEnvelope>> {
    let Some(from_number) = param(params, "From") else {
        return Ok(None);
    };
    if !from_number_is_allowed(config, from_number) {
        return Ok(None);
    }

    let Some(message_sid) = param(params, "MessageSid").or_else(|| param(params, "SmsSid")) else {
        return Ok(None);
    };
    let Some(to_number) = param(params, "To") else {
        return Ok(None);
    };

    let sms_status = param(params, "SmsStatus").or_else(|| param(params, "MessageStatus"));
    if let Some(status) = sms_status
        && status != "received"
        && status != "inbound"
    {
        return Ok(None);
    }

    let body = param(params, "Body").unwrap_or_default().to_string();
    let attachments = extract_attachments(params);
    if body.is_empty() && attachments.is_empty() {
        return Ok(None);
    }

    let mut message_metadata = BTreeMap::new();
    if let Some(profile_name) = param(params, "ProfileName") {
        message_metadata.insert(META_PROFILE_NAME.to_string(), profile_name.to_string());
    }
    if let Some(channel) = param(params, "ChannelToAddress") {
        message_metadata.insert(META_CHANNEL.to_string(), channel.to_string());
    }
    if let Some(num_media) = param(params, "NumMedia") {
        message_metadata.insert(META_NUM_MEDIA.to_string(), num_media.to_string());
    }
    if let Some(api_version) = param(params, "ApiVersion") {
        message_metadata.insert(META_API_VERSION.to_string(), api_version.to_string());
    }

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(META_TRANSPORT.to_string(), TRANSPORT_WEBHOOK.to_string());
    event_metadata.insert(META_TO_NUMBER.to_string(), to_number.to_string());
    event_metadata.insert(META_MESSAGE_SID.to_string(), message_sid.to_string());
    event_metadata.insert(META_EVENT_DIRECTION.to_string(), "incoming".to_string());
    if let Some(endpoint_id) = &payload.endpoint_id {
        event_metadata.insert(META_ENDPOINT_ID.to_string(), endpoint_id.clone());
    }
    if !payload.path.is_empty() {
        event_metadata.insert(META_PATH.to_string(), payload.path.clone());
    }
    if let Some(account_sid) = param(params, "AccountSid") {
        event_metadata.insert(META_ACCOUNT_SID.to_string(), account_sid.to_string());
    }
    if let Some(message_status) = param(params, "MessageStatus") {
        event_metadata.insert(META_MESSAGE_STATUS.to_string(), message_status.to_string());
    }
    if let Some(sms_status) = param(params, "SmsStatus") {
        event_metadata.insert(META_SMS_STATUS.to_string(), sms_status.to_string());
    }
    if let Some(sms_message_sid) = param(params, "SmsMessageSid") {
        event_metadata.insert(
            META_SMS_MESSAGE_SID.to_string(),
            sms_message_sid.to_string(),
        );
    }
    if let Some(sms_sid) = param(params, "SmsSid") {
        event_metadata.insert(META_SMS_SID.to_string(), sms_sid.to_string());
    }

    Ok(Some(InboundEventEnvelope {
        event_id: message_sid.to_string(),
        platform: PLATFORM_TWILIO_SMS.to_string(),
        event_type: EVENT_TYPE_INCOMING_MESSAGE.to_string(),
        received_at: received_at(payload.received_at.as_deref()),
        conversation: InboundConversationRef {
            id: from_number.to_string(),
            kind: CONVERSATION_KIND_PHONE_NUMBER.to_string(),
            thread_id: None,
            parent_message_id: None,
        },
        actor: InboundActor {
            id: from_number.to_string(),
            display_name: param(params, "ProfileName").map(ToOwned::to_owned),
            username: None,
            is_bot: false,
            metadata: BTreeMap::new(),
        },
        message: InboundMessage {
            id: message_sid.to_string(),
            content: body,
            content_type: "text/plain".to_string(),
            reply_to_message_id: None,
            attachments,
            metadata: message_metadata,
        },
        account_id: param(params, "AccountSid").map(ToOwned::to_owned),
        metadata: event_metadata,
    }))
}

fn extract_attachments(params: &BTreeMap<String, String>) -> Vec<InboundAttachment> {
    let count = param(params, "NumMedia")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut attachments = Vec::new();
    for index in 0..count {
        let url_key = format!("MediaUrl{index}");
        let Some(url) = param(params, &url_key) else {
            continue;
        };
        let content_type_key = format!("MediaContentType{index}");
        let mime_type = param(params, &content_type_key).map(ToOwned::to_owned);
        attachments.push(InboundAttachment {
            id: Some(index.to_string()),
            kind: attachment_kind(mime_type.as_deref()),
            url: Some(url.to_string()),
            mime_type,
            size_bytes: None,
            name: None,
            storage_key: None,
            extracted_text: None,
            extras: BTreeMap::new(),
        });
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

fn validate_ingress_signature(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let auth_token = webhook_auth_token(config)?;
    let params = incoming_params(payload);
    validate_twilio_signature(
        &auth_token,
        payload,
        &params,
        resolved_request_url(config, payload)?,
    )
}

fn webhook_auth_token(config: &ChannelConfig) -> Result<String> {
    if has_optional_env(webhook_auth_token_env(config)) {
        return std::env::var(webhook_auth_token_env(config)).with_context(|| {
            format!(
                "{} is required for twilio sms ingress",
                webhook_auth_token_env(config)
            )
        });
    }

    std::env::var(auth_token_env(config)).with_context(|| {
        format!(
            "{} is required for twilio sms ingress",
            auth_token_env(config)
        )
    })
}

fn validate_twilio_signature(
    auth_token: &str,
    payload: &IngressPayload,
    params: &BTreeMap<String, String>,
    request_url: String,
) -> Result<Option<IngressCallbackReply>> {
    let Some(signature_header) = header_value(&payload.headers, HEADER_X_TWILIO_SIGNATURE) else {
        return Ok(Some(callback_reply(
            403,
            "twilio request signature header missing",
        )));
    };

    let mut signing_input = request_url;
    for (name, value) in params {
        signing_input.push_str(name);
        signing_input.push_str(value);
    }

    let mut mac =
        Hmac::<Sha1>::new_from_slice(auth_token.as_bytes()).context("invalid Twilio auth token")?;
    mac.update(signing_input.as_bytes());
    let expected_signature = mac.finalize().into_bytes();
    let provided_signature = BASE64_STANDARD
        .decode(signature_header)
        .map_err(|_| anyhow!("invalid X-Twilio-Signature header"))?;

    if provided_signature != expected_signature.as_slice() {
        return Ok(Some(callback_reply(403, "twilio signature mismatch")));
    }

    Ok(None)
}

fn resolved_request_url(config: &ChannelConfig, payload: &IngressPayload) -> Result<String> {
    let base = config
        .webhook_public_url
        .as_deref()
        .ok_or_else(|| anyhow!("twilio signature verification requires config.webhook_public_url"))?
        .trim_end_matches('/');
    let path = payload.path.trim_start_matches('/');
    let mut url = format!("{base}/{path}");
    if let Some(raw_query) = payload
        .raw_query
        .as_deref()
        .filter(|query| !query.is_empty())
    {
        url.push('?');
        url.push_str(raw_query);
    }
    Ok(url)
}

fn incoming_params(payload: &IngressPayload) -> BTreeMap<String, String> {
    if payload.method.eq_ignore_ascii_case("GET") {
        return payload.query.clone();
    }

    form_urlencoded::parse(payload.body.as_bytes())
        .into_owned()
        .collect()
}

fn resolve_to_number(message: &OutboundMessage) -> Result<String> {
    message
        .to_number
        .clone()
        .or_else(|| message.metadata.get(ROUTE_CONVERSATION_ID).cloned())
        .ok_or_else(|| anyhow!("twilio delivery requires message.to_number"))
}

fn outbound_media_urls(message: &OutboundMessage) -> Result<Vec<String>> {
    let mut media_urls = Vec::with_capacity(message.attachments.len());
    for attachment in &message.attachments {
        media_urls.push(outbound_media_url(attachment)?);
    }
    Ok(media_urls)
}

fn outbound_media_url(attachment: &OutboundAttachment) -> Result<String> {
    if attachment.data_base64.is_some() {
        bail!(
            "twilio sms attachment delivery requires attachment.url; data_base64 is not supported"
        );
    }
    if attachment.storage_key.is_some() {
        bail!(
            "twilio sms attachment delivery requires attachment.url; storage_key is not supported"
        );
    }
    let Some(url) = attachment.url.as_deref() else {
        bail!("twilio sms attachment delivery requires attachment.url");
    };
    validate_url(url, "twilio media url")?;
    Ok(url.to_string())
}

fn resolved_endpoint(config: &ChannelConfig) -> Option<String> {
    let base = config.webhook_public_url.as_deref()?.trim_end_matches('/');
    let path = config
        .webhook_path
        .as_deref()
        .unwrap_or("/twilio/sms")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn account_sid_env(config: &ChannelConfig) -> &str {
    config
        .account_sid_env
        .as_deref()
        .unwrap_or("TWILIO_ACCOUNT_SID")
}

fn auth_token_env(config: &ChannelConfig) -> &str {
    config
        .auth_token_env
        .as_deref()
        .unwrap_or("TWILIO_AUTH_TOKEN")
}

fn webhook_auth_token_env(config: &ChannelConfig) -> &str {
    config
        .webhook_auth_token_env
        .as_deref()
        .unwrap_or("TWILIO_WEBHOOK_AUTH_TOKEN")
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

fn from_number_is_allowed(config: &ChannelConfig, from_number: &str) -> bool {
    config.allowed_from_numbers.is_empty()
        || config
            .allowed_from_numbers
            .iter()
            .any(|allowed| allowed == from_number)
}

fn received_at(host_received_at: Option<&str>) -> String {
    host_received_at
        .map(str::to_owned)
        .unwrap_or_else(|| Timestamp::now().to_string())
}

fn ingress_reply(status: u16, message: &str) -> PluginResponse {
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

fn twiml_empty_reply() -> IngressCallbackReply {
    IngressCallbackReply {
        status: 200,
        content_type: Some("text/xml; charset=utf-8".to_string()),
        body: TWIML_EMPTY_RESPONSE.to_string(),
    }
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn param<'a>(params: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    params.get(name).map(String::as_str)
}

fn ingress_mode_name(mode: IngressMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "webhook".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_payload(method: &str) -> IngressPayload {
        IngressPayload {
            endpoint_id: Some("twilio:/twilio/sms".to_string()),
            method: method.to_string(),
            path: "/twilio/sms".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: String::new(),
            trust_verified: true,
            received_at: Some("2026-04-11T22:00:00Z".to_string()),
        }
    }

    #[test]
    fn inbound_post_maps_sms_event() {
        let mut payload = base_payload("POST");
        payload.body = "MessageSid=SM123&SmsSid=SM123&AccountSid=AC123&From=%2B15550001111&To=%2B15550002222&Body=hello+from+twilio&NumMedia=1&MediaUrl0=https%3A%2F%2Fexample.com%2Fphoto.jpg&MediaContentType0=image%2Fjpeg&SmsStatus=received".to_string();

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                let reply = callback_reply.expect("twiml reply");
                assert_eq!(reply.status, 200);
                assert_eq!(reply.body, TWIML_EMPTY_RESPONSE);
                assert_eq!(events.len(), 1);

                let event = &events[0];
                assert_eq!(event.event_id, "SM123");
                assert_eq!(event.platform, PLATFORM_TWILIO_SMS);
                assert_eq!(event.event_type, EVENT_TYPE_INCOMING_MESSAGE);
                assert_eq!(event.account_id.as_deref(), Some("AC123"));
                assert_eq!(event.conversation.id, "+15550001111");
                assert_eq!(event.conversation.kind, CONVERSATION_KIND_PHONE_NUMBER);
                assert_eq!(event.actor.id, "+15550001111");
                assert_eq!(event.message.content, "hello from twilio");
                assert_eq!(event.message.attachments.len(), 1);
                assert_eq!(
                    event.message.attachments[0].mime_type.as_deref(),
                    Some("image/jpeg")
                );
                assert_eq!(
                    event.metadata.get(META_ENDPOINT_ID).map(String::as_str),
                    Some("twilio:/twilio/sms")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn status_callback_is_ignored_but_acknowledged() {
        let mut payload = base_payload("POST");
        payload.body =
            "MessageSid=SM123&AccountSid=AC123&MessageStatus=delivered&To=%2B15550002222"
                .to_string();

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(events.is_empty());
                let reply = callback_reply.expect("twiml reply");
                assert_eq!(reply.status, 200);
                assert_eq!(reply.body, TWIML_EMPTY_RESPONSE);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let mut payload = base_payload("POST");
        payload.trust_verified = false;
        payload.body =
            "MessageSid=SM123&From=%2B15550001111&To=%2B15550002222&Body=hello".to_string();
        payload.headers.insert(
            HEADER_X_TWILIO_SIGNATURE.to_string(),
            "ZmFrZQ==".to_string(),
        );

        let reply = validate_twilio_signature(
            "secret",
            &payload,
            &incoming_params(&payload),
            "https://example.com/twilio/sms".to_string(),
        )
        .expect("validate signature")
        .expect("rejection reply");

        assert_eq!(reply.status, 403);
        assert_eq!(reply.body, "twilio signature mismatch");
    }

    #[test]
    fn outbound_media_url_accepts_https_urls() {
        let attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: None,
            url: Some("https://example.com/photo.jpg".to_string()),
            storage_key: None,
        };

        assert_eq!(
            outbound_media_url(&attachment).expect("valid media url"),
            "https://example.com/photo.jpg"
        );
    }

    #[test]
    fn outbound_media_url_rejects_non_url_attachment_sources() {
        let inline_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: Some("ZmFrZQ==".to_string()),
            url: None,
            storage_key: None,
        };
        assert!(
            outbound_media_url(&inline_attachment)
                .expect_err("inline data should fail")
                .to_string()
                .contains("data_base64")
        );

        let staged_attachment = OutboundAttachment {
            name: "photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            data_base64: None,
            url: None,
            storage_key: Some("cache://photo.jpg".to_string()),
        };
        assert!(
            outbound_media_url(&staged_attachment)
                .expect_err("storage key should fail")
                .to_string()
                .contains("storage_key")
        );
    }
}
