use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde_json::json;
use std::io::Read;

use crate::protocol::{ChannelConfig, OutboundMessage, StatusFrame};

pub fn healthcheck(config: &ChannelConfig) -> Result<Option<u16>> {
    let Some(url) = config.healthcheck_url.as_deref() else {
        return Ok(None);
    };

    let response = ureq::get(url)
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| anyhow!("failed to probe webhook healthcheck URL: {error}"))?;
    Ok(Some(response.status().as_u16()))
}

pub fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<(String, String)> {
    let destination = message
        .destination_url
        .as_deref()
        .or_else(|| message.metadata.get("destination_url").map(String::as_str))
        .or(config.outbound_url.as_deref())
        .ok_or_else(|| {
            anyhow!("webhook delivery requires message.destination_url or config.outbound_url")
        })?;

    let timestamp_ms = Timestamp::now().as_millisecond();
    let message_id = format!("webhook-{timestamp_ms}");
    let conversation_id = message
        .conversation_id
        .clone()
        .or_else(|| message.metadata.get("conversation_id").cloned())
        .unwrap_or_else(|| "webhook".to_string());

    let payload = json!({
        "kind": "message",
        "message_id": message_id,
        "conversation_id": conversation_id,
        "subject": message.subject,
        "content": message.content,
        "attachments": message.attachments,
        "metadata": message.metadata,
    });

    post_json(
        config,
        destination,
        payload,
        "failed to deliver outbound webhook",
    )?;

    Ok((message_id, conversation_id))
}

pub fn send_status(config: &ChannelConfig, destination: &str, update: &StatusFrame) -> Result<()> {
    let payload = json!({
        "kind": "status",
        "status_kind": update.kind,
        "message": update.message,
        "conversation_id": update.conversation_id,
        "thread_id": update.thread_id,
        "metadata": update.metadata,
    });

    post_json(
        config,
        destination,
        payload,
        "failed to send webhook status frame",
    )
}

fn post_json(
    config: &ChannelConfig,
    destination: &str,
    payload: serde_json::Value,
    error_context: &str,
) -> Result<()> {
    let mut request = ureq::post(destination)
        .header("Content-Type", "application/json")
        .header("User-Agent", "channel-webhook/0.1.0");

    for (name, value) in &config.static_headers {
        request = request.header(name, value);
    }
    if let Ok(token) = outbound_bearer_token(config) {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }

    let mut response = request
        .send_json(payload)
        .map_err(|error| anyhow!("{error_context}: {error}"))?;
    read_body(&mut response, "failed to read outbound webhook response")?;
    if !response.status().is_success() {
        bail!(
            "outbound webhook returned HTTP {}",
            response.status().as_u16()
        );
    }

    Ok(())
}

pub fn outbound_bearer_token(config: &ChannelConfig) -> Result<String> {
    let env_name = config
        .outbound_bearer_token_env
        .as_deref()
        .unwrap_or("WEBHOOK_OUTBOUND_BEARER_TOKEN");
    std::env::var(env_name)
        .with_context(|| format!("{env_name} is required for outbound webhook auth"))
}

pub fn ingress_secret(config: &ChannelConfig) -> Result<String> {
    let env_name = config
        .ingress_secret_env
        .as_deref()
        .unwrap_or("WEBHOOK_INGRESS_SECRET");
    std::env::var(env_name).with_context(|| format!("{env_name} is required for ingress auth"))
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>, context: &str) -> Result<String> {
    let mut body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .reader();
    let mut text = String::new();
    body.read_to_string(&mut text)
        .with_context(|| context.to_string())?;
    Ok(text)
}
