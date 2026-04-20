use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

#[derive(Debug, Clone)]
pub struct SignalClient {
    base_url: String,
    account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalTarget {
    Recipient(String),
    Group(String),
    Username(String),
}

#[derive(Debug, Clone)]
pub struct SignalSendReceipt {
    pub message_id: String,
}

#[derive(Debug, Clone)]
pub struct SignalAttachmentPayload {
    pub name: String,
    pub data_base64: String,
}

#[derive(Debug, Clone)]
pub struct SignalHealth {
    pub version: Option<String>,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalReceiveTransport {
    Polling,
    Websocket,
}

impl SignalClient {
    pub fn new(base_url: String, account: Option<String>) -> Self {
        Self { base_url, account }
    }

    pub fn health(&self) -> Result<SignalHealth> {
        let mut response = ureq::get(&format!("{}/v1/about", self.base_url))
            .call()
            .context("signal health check failed")?;
        if response.status().as_u16() >= 400 {
            bail!("signal health check returned HTTP {}", response.status());
        }

        let body = response
            .body_mut()
            .read_to_string()
            .context("read signal health body")?;
        let about: Value = serde_json::from_str(&body).context("parse signal about response")?;
        let version = about
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mode = about.get("mode").and_then(Value::as_str).map(str::to_owned);
        let accounts = self.list_accounts()?;

        let _ = accounts;
        Ok(SignalHealth { version, mode })
    }

    pub fn send_message(
        &self,
        target: &SignalTarget,
        content: &str,
        attachments: &[SignalAttachmentPayload],
    ) -> Result<SignalSendReceipt> {
        let mut payload = json!({
            "number": self.resolved_account()?,
            "message": content,
            "recipients": target_recipients(target),
        });

        if let Some(object) = payload.as_object_mut()
            && !attachments.is_empty()
        {
            object.insert(
                "base64_attachments".to_string(),
                Value::Array(
                    attachments
                        .iter()
                        .map(|attachment| Value::String(attachment_data(attachment)))
                        .collect(),
                ),
            );
        }

        let request = serde_json::to_string(&payload).context("serialize signal send payload")?;
        let mut response = ureq::post(&format!("{}/v2/send", self.base_url))
            .content_type("application/json")
            .send(&request)
            .context("signal send failed")?;
        let body = response
            .body_mut()
            .read_to_string()
            .context("read signal send body")?;
        let result: Value = serde_json::from_str(&body).context("parse signal send response")?;
        let message_id = result
            .get("timestamp")
            .and_then(|value| match value {
                Value::String(value) => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_string());

        Ok(SignalSendReceipt { message_id })
    }

    pub fn send_typing(&self, target: &SignalTarget) -> Result<()> {
        let account = self.resolved_account()?;
        let payload = json!({
            "recipient": target_recipient(target),
        });
        let request = serde_json::to_string(&payload).context("serialize signal typing payload")?;
        let response = ureq::put(&format!(
            "{}/v1/typing-indicator/{}",
            self.base_url, account
        ))
        .content_type("application/json")
        .send(&request)
        .context("signal typing indicator failed")?;
        if response.status().as_u16() != 204 {
            bail!(
                "signal typing indicator returned HTTP {}",
                response.status()
            );
        }
        Ok(())
    }

    pub fn receive_messages(
        &self,
        timeout_secs: u16,
    ) -> Result<(Vec<Value>, SignalReceiveTransport)> {
        if matches!(self.api_mode()?.as_deref(), Some("json-rpc")) {
            return self
                .receive_messages_websocket(timeout_secs)
                .map(|events| (events, SignalReceiveTransport::Websocket));
        }

        let account = self.resolved_account()?;
        let mut response = ureq::get(&format!("{}/v1/receive/{}", self.base_url, account))
            .query("timeout", timeout_secs.to_string())
            .call()
            .context("signal receive failed")?;
        if response.status().as_u16() >= 400 {
            bail!("signal receive returned HTTP {}", response.status());
        }
        let body = response
            .body_mut()
            .read_to_string()
            .context("read signal receive body")?;
        if body.trim().is_empty() {
            return Ok((Vec::new(), SignalReceiveTransport::Polling));
        }
        let result: Value = serde_json::from_str(&body).context("parse signal receive response")?;
        Ok((
            normalize_receive_result(result),
            SignalReceiveTransport::Polling,
        ))
    }

    fn receive_messages_websocket(&self, timeout_secs: u16) -> Result<Vec<Value>> {
        let account = self.resolved_account()?;
        let url = websocket_receive_url(&self.base_url, &account, timeout_secs)?;
        let (mut socket, _) = tungstenite::connect(url.as_str())
            .with_context(|| format!("signal websocket receive connect failed for {url}"))?;
        let deadline = Instant::now() + websocket_timeout_window(timeout_secs);
        let mut events = Vec::new();

        loop {
            let remaining = remaining_websocket_timeout(deadline);
            if remaining.is_zero() {
                break;
            }
            configure_websocket_read_timeout(socket.get_mut(), remaining)
                .context("configure signal websocket read timeout")?;
            match socket.read() {
                Ok(Message::Text(frame)) => {
                    let value: Value = serde_json::from_str(frame.as_str())
                        .context("parse signal websocket receive frame")?;
                    events.extend(normalize_receive_result(value));
                }
                Ok(Message::Binary(frame)) => {
                    let value: Value = serde_json::from_slice(&frame)
                        .context("parse binary signal websocket receive frame")?;
                    events.extend(normalize_receive_result(value));
                }
                Ok(Message::Ping(payload)) => {
                    socket
                        .send(Message::Pong(payload))
                        .context("reply to signal websocket ping")?;
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => break,
                Ok(Message::Frame(_)) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    break;
                }
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => break,
                Err(error) => return Err(error).context("signal websocket receive failed"),
            }
        }

        Ok(events)
    }

    fn resolved_account(&self) -> Result<String> {
        if let Some(account) = &self.account {
            return Ok(account.clone());
        }

        let accounts = self.list_accounts()?;
        match accounts.as_slice() {
            [account] => Ok(account.clone()),
            [] => bail!(
                "signal backend has no linked accounts; set config.account or link a Signal device"
            ),
            _ => bail!("signal backend exposes multiple accounts; set config.account explicitly"),
        }
    }

    fn list_accounts(&self) -> Result<Vec<String>> {
        let mut response = ureq::get(&format!("{}/v1/accounts", self.base_url))
            .call()
            .context("signal list accounts failed")?;
        let body = response
            .body_mut()
            .read_to_string()
            .context("read signal accounts body")?;
        serde_json::from_str(&body).context("parse signal accounts response")
    }

    fn api_mode(&self) -> Result<Option<String>> {
        let mut response = ureq::get(&format!("{}/v1/about", self.base_url))
            .call()
            .context("signal about request failed")?;
        let body = response
            .body_mut()
            .read_to_string()
            .context("read signal about body")?;
        let about: Value = serde_json::from_str(&body).context("parse signal about response")?;
        Ok(about.get("mode").and_then(Value::as_str).map(str::to_owned))
    }
}

fn normalize_receive_result(value: Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items,
        Value::Object(mut object) => match object.remove("messages") {
            Some(Value::Array(items)) => items,
            Some(other) => vec![other],
            None => vec![Value::Object(object)],
        },
        other => vec![other],
    }
}

fn attachment_data(attachment: &SignalAttachmentPayload) -> String {
    let file_name = attachment
        .name
        .chars()
        .map(|ch| match ch {
            ';' | ',' | '\r' | '\n' => '_',
            _ => ch,
        })
        .collect::<String>();
    format!(
        "data:application/octet-stream;filename={file_name};base64,{}",
        attachment.data_base64
    )
}

fn websocket_receive_url(base_url: &str, account: &str, timeout_secs: u16) -> Result<String> {
    let scheme = if let Some(rest) = base_url.strip_prefix("http://") {
        ("ws://", rest)
    } else if let Some(rest) = base_url.strip_prefix("https://") {
        ("wss://", rest)
    } else {
        bail!("signal base url `{base_url}` must start with http:// or https://");
    };
    Ok(format!(
        "{}{}/v1/receive/{}?timeout={}",
        scheme.0, scheme.1, account, timeout_secs
    ))
}

fn configure_websocket_read_timeout(
    stream: &mut MaybeTlsStream<std::net::TcpStream>,
    timeout: Duration,
) -> std::io::Result<()> {
    let tcp = match stream {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Ok(()),
    };
    tcp.set_read_timeout(Some(timeout))
}

fn websocket_timeout_window(timeout_secs: u16) -> Duration {
    Duration::from_secs(u64::from(timeout_secs.max(1)) + 1)
}

fn remaining_websocket_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

pub fn normalize_base_url(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("signal base url must not be empty");
    }
    let url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    Ok(url.trim_end_matches('/').to_string())
}

pub fn parse_target(raw: &str) -> Result<SignalTarget> {
    let mut value = raw.trim();
    if value.is_empty() {
        bail!("signal recipient is required");
    }
    if let Some(stripped) = value.strip_prefix("signal:") {
        value = stripped.trim();
    }
    if let Some(group_id) = value.strip_prefix("group:") {
        return Ok(SignalTarget::Group(group_id.trim().to_string()));
    }
    if let Some(username) = value.strip_prefix("username:") {
        return Ok(SignalTarget::Username(username.trim().to_string()));
    }
    if let Some(username) = value.strip_prefix("u:") {
        return Ok(SignalTarget::Username(username.trim().to_string()));
    }
    Ok(SignalTarget::Recipient(value.to_string()))
}

fn target_recipients(target: &SignalTarget) -> Value {
    match target {
        SignalTarget::Recipient(recipient) => json!([recipient]),
        SignalTarget::Group(group_id) => json!([group_id]),
        SignalTarget::Username(username) => json!([username]),
    }
}

fn target_recipient(target: &SignalTarget) -> String {
    match target {
        SignalTarget::Recipient(recipient) => recipient.clone(),
        SignalTarget::Group(group_id) => group_id.clone(),
        SignalTarget::Username(username) => username.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_accepts_phone_group_and_username_forms() {
        assert_eq!(
            parse_target("+15551234567").expect("phone"),
            SignalTarget::Recipient("+15551234567".to_string())
        );
        assert_eq!(
            parse_target("signal:group:test-group").expect("group"),
            SignalTarget::Group("test-group".to_string())
        );
        assert_eq!(
            parse_target("username:dispatch").expect("username"),
            SignalTarget::Username("dispatch".to_string())
        );
    }

    #[test]
    fn normalize_base_url_adds_scheme_and_trims_trailing_slash() {
        assert_eq!(
            normalize_base_url("127.0.0.1:8080/").expect("normalize"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn normalize_receive_result_accepts_array_and_messages_wrapper() {
        assert_eq!(
            normalize_receive_result(json!([{"id": 1}, {"id": 2}])).len(),
            2
        );
        assert_eq!(
            normalize_receive_result(json!({"messages": [{"id": 1}]})).len(),
            1
        );
    }

    #[test]
    fn target_recipients_map_supported_target_types() {
        assert_eq!(
            target_recipients(&SignalTarget::Recipient("+15551234567".to_string())),
            json!(["+15551234567"])
        );
        assert_eq!(
            target_recipients(&SignalTarget::Group("group-1".to_string())),
            json!(["group-1"])
        );
        assert_eq!(
            target_recipients(&SignalTarget::Username("dispatch".to_string())),
            json!(["dispatch"])
        );
    }

    #[test]
    fn target_recipient_returns_single_wire_value() {
        assert_eq!(
            target_recipient(&SignalTarget::Recipient("+15551234567".to_string())),
            "+15551234567"
        );
        assert_eq!(
            target_recipient(&SignalTarget::Group("group-1".to_string())),
            "group-1"
        );
    }

    #[test]
    fn attachment_data_keeps_filename_and_base64_payload() {
        let attachment = SignalAttachmentPayload {
            name: "hello.txt".to_string(),
            data_base64: "aGVsbG8=".to_string(),
        };
        assert_eq!(
            attachment_data(&attachment),
            "data:application/octet-stream;filename=hello.txt;base64,aGVsbG8="
        );
    }

    #[test]
    fn websocket_receive_url_rewrites_http_and_https() {
        assert_eq!(
            websocket_receive_url("http://127.0.0.1:8080", "+15551234567", 5).unwrap(),
            "ws://127.0.0.1:8080/v1/receive/+15551234567?timeout=5"
        );
        assert_eq!(
            websocket_receive_url("https://signal.example", "+15551234567", 2).unwrap(),
            "wss://signal.example/v1/receive/+15551234567?timeout=2"
        );
    }
}
