use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fmt,
    io::Read,
    net::TcpStream,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

const DEFAULT_API_BASE: &str = "https://slack.com/api";
const REACTION_TIMEOUT: Duration = Duration::from_secs(2);
const MIN_SLACK_RETRY_AFTER: Duration = Duration::from_secs(1);
const MAX_SLACK_RETRY_AFTER: Duration = Duration::from_secs(60);
const SLACK_SOCKET_READ_SLICE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct SlackClient {
    bot_token: String,
    base_url: String,
}

#[derive(Debug)]
pub struct SlackSocketModeClient {
    app_token: String,
    base_url: String,
}

#[derive(Debug)]
pub enum SlackSocketModeError {
    Authentication { code: String },
    Protocol { code: String },
    RateLimited { retry_after: Duration },
    NotificationDelivery { message: String },
    Transport(anyhow::Error),
}

impl fmt::Display for SlackSocketModeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication { code } => {
                write!(formatter, "Slack Socket Mode authentication failed: {code}")
            }
            Self::Protocol { code } => {
                write!(formatter, "Slack Socket Mode protocol error: {code}")
            }
            Self::RateLimited { retry_after } => write!(
                formatter,
                "Slack Socket Mode connection was rate limited for {} seconds",
                retry_after.as_secs()
            ),
            Self::NotificationDelivery { message } => {
                write!(formatter, "Slack notification delivery failed: {message}")
            }
            Self::Transport(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for SlackSocketModeError {}

#[derive(Debug, Clone)]
pub enum SlackSocketReceiveOutcome {
    /// The delivery callback completed and the envelope was acknowledged.
    Delivered,
    Event(SlackSocketEnvelope),
    Timeout,
    Stopped,
    Disconnected,
}

#[derive(Debug)]
struct SlackNotificationDeliveryError(String);

impl fmt::Display for SlackNotificationDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SlackNotificationDeliveryError {}

pub fn notification_delivery_error(error: impl fmt::Display) -> anyhow::Error {
    anyhow::Error::new(SlackNotificationDeliveryError(error.to_string()))
}

/// Invokes the delivery callback for one `events_api` envelope before acknowledgement.
pub type SlackEnvelopeDelivery<'a> =
    &'a dyn Fn(&SlackSocketEnvelope) -> Result<SlackEnvelopeDisposition>;

/// Reports whether an `events_api` envelope was delivered or intentionally ignored.
pub enum SlackEnvelopeDisposition {
    Delivered,
    Ignored,
}

#[derive(Debug)]
enum SlackSocketMessageOutcome {
    Delivered,
    Event(SlackSocketEnvelope),
    Continue,
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct SlackIdentity {
    pub user_id: String,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SlackMessage {
    pub message_id: String,
    pub channel_id: String,
    pub thread_ts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackUpload {
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlackSocketEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: String,
    #[serde(default)]
    pub envelope_id: Option<String>,
    #[serde(default)]
    pub payload: Option<Value>,
}

impl SlackClient {
    pub fn from_env(bot_token_env: &str) -> Result<Self> {
        let bot_token = std::env::var(bot_token_env)
            .with_context(|| format!("{bot_token_env} is required for the slack channel"))?;
        Ok(Self {
            bot_token,
            base_url: slack_api_base_url(),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(base_url: &str) -> Self {
        Self {
            bot_token: "test-token".to_string(),
            base_url: base_url.to_string(),
        }
    }

    pub fn identity(&self) -> Result<SlackIdentity> {
        let body = self.post_json("auth.test", json!({}), "failed to query Slack bot identity")?;
        let user_id = body
            .get("user_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("slack auth.test response missing user_id"))?
            .to_string();
        Ok(SlackIdentity {
            user_id,
            team_id: body
                .get("team_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            team_name: body
                .get("team")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            user: body
                .get("user")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    pub fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        thread_ts: Option<&str>,
        upload: Option<&SlackUpload>,
    ) -> Result<SlackMessage> {
        if let Some(upload) = upload {
            return self.upload_file(channel_id, content, thread_ts, upload);
        }

        let mut payload = json!({
            "channel": channel_id,
            "text": content,
        });
        if let Some(thread_ts) = thread_ts {
            payload["thread_ts"] = Value::String(thread_ts.to_string());
        }
        let body = self.post_json("chat.postMessage", payload, "failed to send Slack message")?;
        let ts = body
            .get("ts")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("slack chat.postMessage response missing ts"))?
            .to_string();
        let channel_id = body
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or(channel_id)
            .to_string();
        Ok(SlackMessage {
            message_id: ts.clone(),
            channel_id,
            thread_ts: thread_ts.map(ToOwned::to_owned),
        })
    }

    pub fn add_reaction(
        &self,
        channel_id: &str,
        message_ts: &str,
        reaction_name: &str,
    ) -> Result<()> {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REACTION_TIMEOUT))
            .build()
            .new_agent();
        let body = self
            .post_json_response(
                Some(&agent),
                "reactions.add",
                json!({
                    "channel": channel_id,
                    "timestamp": message_ts,
                    "name": reaction_name,
                }),
                "failed to add Slack reaction",
            )
            .map_err(|_| anyhow!("failed to add Slack reaction"))?;

        match body.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(()),
            Some(false) if body.get("error").and_then(Value::as_str) == Some("already_reacted") => {
                Ok(())
            }
            _ => bail!("failed to add Slack reaction"),
        }
    }

    fn upload_file(
        &self,
        channel_id: &str,
        content: &str,
        thread_ts: Option<&str>,
        upload: &SlackUpload,
    ) -> Result<SlackMessage> {
        let request = self.post_json(
            "files.getUploadURLExternal",
            json!({
                "filename": upload.name,
                "length": upload.data.len(),
            }),
            "failed to request Slack file upload URL",
        )?;
        let upload_url = request
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("slack files.getUploadURLExternal response missing upload_url")
            })?;
        let file_id = request
            .get("file_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("slack files.getUploadURLExternal response missing file_id"))?
            .to_string();

        self.upload_file_bytes(upload_url, upload)?;

        let mut payload = json!({
            "files": [{
                "id": file_id,
                "title": upload.name,
            }],
            "channel_id": channel_id,
        });
        if !content.trim().is_empty() {
            payload["initial_comment"] = Value::String(content.to_string());
        }
        if let Some(thread_ts) = thread_ts {
            payload["thread_ts"] = Value::String(thread_ts.to_string());
        }

        let body = self.post_json(
            "files.completeUploadExternal",
            payload,
            "failed to complete Slack file upload",
        )?;
        let file = body
            .get("files")
            .and_then(Value::as_array)
            .and_then(|files| files.first())
            .ok_or_else(|| anyhow!("slack files.completeUploadExternal response missing files"))?;
        let message_id = file
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&file_id)
            .to_string();

        Ok(SlackMessage {
            message_id,
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.map(ToOwned::to_owned),
        })
    }

    fn post_json(&self, method: &str, payload: Value, context: &str) -> Result<Value> {
        let body = self.post_json_response(None, method, payload, context)?;
        let ok = body
            .get("ok")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("{context}: slack response missing ok flag"))?;
        if !ok {
            let error = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_slack_error");
            bail!("{context}: {error}");
        }
        Ok(body)
    }

    fn post_json_response(
        &self,
        agent: Option<&ureq::Agent>,
        method: &str,
        payload: Value,
        context: &str,
    ) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, method);
        let mut response = match agent {
            Some(agent) => agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", self.bot_token))
                .header("Content-Type", "application/json")
                .send_json(payload),
            None => ureq::post(&url)
                .header("Authorization", &format!("Bearer {}", self.bot_token))
                .header("Content-Type", "application/json")
                .send_json(payload),
        }
        .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }

    fn upload_file_bytes(&self, upload_url: &str, upload: &SlackUpload) -> Result<()> {
        let mut response = ureq::post(upload_url)
            .header("Content-Type", &upload.mime_type)
            .send(upload.data.as_slice())
            .map_err(|error| anyhow!("failed to upload Slack file bytes: {error}"))?;
        let body = read_text_body(&mut response, "failed to read Slack upload response")?;
        if !response.status().is_success() {
            bail!(
                "failed to upload Slack file bytes: HTTP {}: {}",
                response.status().as_u16(),
                body
            );
        }
        Ok(())
    }
}

impl SlackSocketModeClient {
    pub fn from_env(app_token_env: &str) -> Result<Self> {
        let app_token = std::env::var(app_token_env)
            .with_context(|| format!("{app_token_env} is required for Slack Socket Mode"))?;
        Ok(Self {
            app_token,
            base_url: slack_api_base_url(),
        })
    }

    #[cfg(test)]
    fn new_for_tests(base_url: &str) -> Self {
        Self {
            app_token: "test-app-token".to_string(),
            base_url: base_url.to_string(),
        }
    }

    /// Receive one envelope and invoke the optional delivery callback before acknowledgement.
    pub fn receive_event(
        &self,
        timeout_secs: u16,
        deliver: Option<SlackEnvelopeDelivery<'_>>,
        is_stopped: Option<&dyn Fn() -> bool>,
    ) -> std::result::Result<SlackSocketReceiveOutcome, SlackSocketModeError> {
        let websocket_url = self.open_connection_url()?;
        let (mut socket, _) = tungstenite::connect(websocket_url.as_str())
            .context("failed to connect Slack socket mode websocket")
            .map_err(SlackSocketModeError::Transport)?;
        let deadline = Instant::now() + websocket_timeout_window(timeout_secs);

        loop {
            if is_stopped.is_some_and(|is_stopped| is_stopped()) {
                return Ok(SlackSocketReceiveOutcome::Stopped);
            }
            let remaining = remaining_websocket_timeout(deadline);
            if remaining.is_zero() {
                return Ok(SlackSocketReceiveOutcome::Timeout);
            }
            configure_websocket_read_timeout(
                socket.get_mut(),
                std::cmp::min(remaining, SLACK_SOCKET_READ_SLICE),
            )
            .map_err(SlackSocketModeError::Transport)?;
            match socket.read() {
                Ok(Message::Text(text)) => {
                    match self
                        .handle_socket_message(&mut socket, text.as_str(), deliver)
                        .map_err(classify_socket_message_error)?
                    {
                        SlackSocketMessageOutcome::Delivered => {
                            return Ok(SlackSocketReceiveOutcome::Delivered);
                        }
                        SlackSocketMessageOutcome::Event(envelope) => {
                            return Ok(SlackSocketReceiveOutcome::Event(envelope));
                        }
                        SlackSocketMessageOutcome::Disconnected => {
                            return Ok(SlackSocketReceiveOutcome::Disconnected);
                        }
                        SlackSocketMessageOutcome::Continue => {}
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    let text = std::str::from_utf8(bytes.as_ref())
                        .context("Slack socket mode frame was not valid UTF-8")
                        .map_err(SlackSocketModeError::Transport)?;
                    match self
                        .handle_socket_message(&mut socket, text, deliver)
                        .map_err(classify_socket_message_error)?
                    {
                        SlackSocketMessageOutcome::Delivered => {
                            return Ok(SlackSocketReceiveOutcome::Delivered);
                        }
                        SlackSocketMessageOutcome::Event(envelope) => {
                            return Ok(SlackSocketReceiveOutcome::Event(envelope));
                        }
                        SlackSocketMessageOutcome::Disconnected => {
                            return Ok(SlackSocketReceiveOutcome::Disconnected);
                        }
                        SlackSocketMessageOutcome::Continue => {}
                    }
                }
                Ok(Message::Ping(payload)) => {
                    socket
                        .send(Message::Pong(payload))
                        .map_err(|error| SlackSocketModeError::Transport(error.into()))?;
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => return Ok(SlackSocketReceiveOutcome::Disconnected),
                Ok(Message::Frame(_)) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    if is_stopped.is_some_and(|is_stopped| is_stopped()) {
                        return Ok(SlackSocketReceiveOutcome::Stopped);
                    }
                    if Instant::now() >= deadline {
                        return Ok(SlackSocketReceiveOutcome::Timeout);
                    }
                }
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(SlackSocketReceiveOutcome::Disconnected);
                }
                Err(error) => {
                    return Err(SlackSocketModeError::Transport(
                        anyhow!(error).context("failed to read Slack socket mode frame"),
                    ));
                }
            }
        }
    }

    pub fn open_connection_url(&self) -> std::result::Result<String, SlackSocketModeError> {
        let url = format!("{}/apps.connections.open", self.base_url);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.app_token))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send("")
            .map_err(|error| {
                SlackSocketModeError::Transport(anyhow!(
                    "failed to open Slack socket mode connection: {error}"
                ))
            })?;
        if response.status().as_u16() == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_seconds)
                .unwrap_or(Duration::from_secs(1))
                .clamp(MIN_SLACK_RETRY_AFTER, MAX_SLACK_RETRY_AFTER);
            return Err(SlackSocketModeError::RateLimited { retry_after });
        }
        if !response.status().is_success() {
            return Err(SlackSocketModeError::Transport(anyhow!(
                "failed to open Slack socket mode connection: HTTP {}",
                response.status().as_u16()
            )));
        }
        let body = read_json_body(&mut response, "failed to open Slack socket mode connection")
            .map_err(SlackSocketModeError::Transport)?;
        let ok = body.get("ok").and_then(Value::as_bool).ok_or_else(|| {
            SlackSocketModeError::Protocol {
                code: "missing_ok".to_string(),
            }
        })?;
        if !ok {
            let code = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_slack_error")
                .to_string();
            if is_slack_authentication_error(&code) {
                return Err(SlackSocketModeError::Authentication { code });
            }
            if code == "ratelimited" {
                return Err(SlackSocketModeError::RateLimited {
                    retry_after: Duration::from_secs(1),
                });
            }
            return Err(SlackSocketModeError::Protocol { code });
        }
        body.get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| SlackSocketModeError::Protocol {
                code: "missing_url".to_string(),
            })
    }

    fn handle_socket_message(
        &self,
        socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
        text: &str,
        deliver: Option<SlackEnvelopeDelivery<'_>>,
    ) -> Result<SlackSocketMessageOutcome> {
        let envelope: SlackSocketEnvelope =
            serde_json::from_str(text).context("failed to parse Slack socket mode envelope")?;

        match envelope.envelope_type.as_str() {
            "hello" => Ok(SlackSocketMessageOutcome::Continue),
            "disconnect" => Ok(SlackSocketMessageOutcome::Disconnected),
            "events_api" => {
                let Some(deliver) = deliver else {
                    if let Some(envelope_id) = envelope.envelope_id.as_deref() {
                        acknowledge_socket_envelope(socket, envelope_id)?;
                    }
                    return Ok(SlackSocketMessageOutcome::Event(envelope));
                };
                // Delivery errors leave the envelope available for redelivery.
                let disposition = deliver(&envelope)?;
                if let Some(envelope_id) = envelope.envelope_id.as_deref() {
                    acknowledge_socket_envelope(socket, envelope_id)?;
                }
                match disposition {
                    SlackEnvelopeDisposition::Delivered => Ok(SlackSocketMessageOutcome::Delivered),
                    SlackEnvelopeDisposition::Ignored => Ok(SlackSocketMessageOutcome::Continue),
                }
            }
            _ => {
                if let Some(envelope_id) = envelope.envelope_id.as_deref() {
                    acknowledge_socket_envelope(socket, envelope_id)?;
                }
                Ok(SlackSocketMessageOutcome::Continue)
            }
        }
    }
}

fn classify_socket_message_error(error: anyhow::Error) -> SlackSocketModeError {
    match error.downcast::<SlackNotificationDeliveryError>() {
        Ok(error) => SlackSocketModeError::NotificationDelivery { message: error.0 },
        Err(error) => SlackSocketModeError::Transport(error),
    }
}

/// Parse Slack `Retry-After` delta-seconds, including fractional values.
fn parse_retry_after_seconds(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let seconds = value.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs(seconds.ceil() as u64))
}

fn is_slack_authentication_error(code: &str) -> bool {
    matches!(
        code,
        "account_inactive"
            | "invalid_auth"
            | "missing_scope"
            | "not_authed"
            | "not_allowed_token_type"
            | "token_expired"
            | "token_revoked"
    )
}

pub fn send_incoming_webhook(url: &str, content: &str) -> Result<SlackMessage> {
    let mut response = ureq::post(url)
        .header("Content-Type", "application/json")
        .send_json(json!({ "text": content }))
        .map_err(|_| anyhow!("failed to send configured Slack incoming webhook"))?;
    let body = read_text_body(
        &mut response,
        "failed to read Slack incoming webhook response",
    )?;
    if !response.status().is_success() {
        bail!(
            "failed to send Slack incoming webhook: HTTP {}: {}",
            response.status().as_u16(),
            body
        );
    }
    let timestamp_ms = Timestamp::now().as_millisecond();
    Ok(SlackMessage {
        message_id: format!("webhook-{timestamp_ms}"),
        channel_id: "incoming_webhook".to_string(),
        thread_ts: None,
    })
}

fn read_json_body(response: &mut ureq::http::Response<ureq::Body>, context: &str) -> Result<Value> {
    let status = response.status();
    let mut body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .reader();
    let mut text = String::new();
    body.read_to_string(&mut text)
        .with_context(|| format!("{context}: failed to read response body"))?;
    if !status.is_success() {
        bail!("{context}: HTTP {}: {}", status.as_u16(), text);
    }
    serde_json::from_str(&text)
        .with_context(|| format!("{context}: failed to parse response body as JSON"))
}

fn read_text_body(
    response: &mut ureq::http::Response<ureq::Body>,
    context: &str,
) -> Result<String> {
    let mut body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .reader();
    let mut text = String::new();
    body.read_to_string(&mut text)
        .with_context(|| format!("{context}: failed to read response body"))?;
    Ok(text)
}

fn acknowledge_socket_envelope(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    envelope_id: &str,
) -> Result<()> {
    let ack = json!({ "envelope_id": envelope_id }).to_string();
    socket
        .send(Message::Text(ack.into()))
        .context("failed to acknowledge Slack socket mode envelope")
}

fn configure_websocket_read_timeout(
    stream: &mut MaybeTlsStream<TcpStream>,
    timeout: Duration,
) -> Result<()> {
    let tcp = match stream {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Ok(()),
    };
    tcp.set_read_timeout(Some(timeout))
        .context("failed to configure Slack socket mode read timeout")
}

fn websocket_timeout_window(timeout_secs: u16) -> Duration {
    Duration::from_secs(u64::from(timeout_secs.max(1)) + 1)
}

fn remaining_websocket_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn slack_api_base_url() -> String {
    std::env::var("SLACK_API_BASE_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread;
    use std::time::Duration;

    #[derive(Debug)]
    struct CapturedRequest {
        request_line: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    #[derive(Debug)]
    struct StubResponse {
        content_type: &'static str,
        body: String,
    }

    #[test]
    fn send_message_without_thread_ts_does_not_report_thread() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(false)
            .expect("listener blocking mode");
        let address = listener.local_addr().expect("listener addr");
        let base_url = format!("http://{address}/api");
        let (request_tx, request_rx) = mpsc::channel();
        let responses = vec![StubResponse {
            content_type: "application/json",
            body: r#"{"ok":true,"channel":"D123","ts":"1712860000.000001"}"#.to_string(),
        }];

        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept connection");
                let request = read_request(&mut stream);
                request_tx.send(request).expect("send request");
                write_response(&mut stream, &response);
            }
        });

        let client = SlackClient::new_for_tests(&base_url);
        let message = client
            .send_message("D123", "hello from slack", None, None)
            .expect("send message");

        assert_eq!(message.message_id, "1712860000.000001");
        assert_eq!(message.channel_id, "D123");
        assert_eq!(message.thread_ts, None);

        let request = request_rx.recv().expect("chat.postMessage request");
        assert_eq!(request.request_line, "POST /api/chat.postMessage HTTP/1.1");
        let payload: Value = serde_json::from_slice(&request.body).expect("parse request json");
        assert_eq!(payload["channel"], "D123");
        assert_eq!(payload["text"], "hello from slack");
        assert!(payload.get("thread_ts").is_none());

        server.join().expect("server thread");
    }

    #[test]
    fn send_message_uploads_file_then_completes_external_upload() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(false)
            .expect("listener blocking mode");
        let address = listener.local_addr().expect("listener addr");
        let base_url = format!("http://{address}/api");
        let upload_url = format!("http://{address}/upload");
        let (request_tx, request_rx) = mpsc::channel();
        let responses = vec![
            StubResponse {
                content_type: "application/json",
                body: format!(r#"{{"ok":true,"upload_url":"{upload_url}","file_id":"F123"}}"#),
            },
            StubResponse {
                content_type: "text/plain",
                body: "ok".to_string(),
            },
            StubResponse {
                content_type: "application/json",
                body: r#"{"ok":true,"files":[{"id":"F123"}]}"#.to_string(),
            },
        ];

        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept connection");
                let request = read_request(&mut stream);
                request_tx.send(request).expect("send request");
                write_response(&mut stream, &response);
            }
        });

        let client = SlackClient::new_for_tests(&base_url);
        let message = client
            .send_message(
                "C123",
                "attachment comment",
                Some("1712860000.000001"),
                Some(&SlackUpload {
                    name: "report.txt".to_string(),
                    mime_type: "text/plain".to_string(),
                    data: b"hello".to_vec(),
                }),
            )
            .expect("send message");

        assert_eq!(message.message_id, "F123");
        assert_eq!(message.channel_id, "C123");
        assert_eq!(message.thread_ts.as_deref(), Some("1712860000.000001"));

        let request1 = request_rx.recv().expect("upload url request");
        assert_eq!(
            request1.request_line,
            "POST /api/files.getUploadURLExternal HTTP/1.1"
        );
        assert_eq!(
            request1.headers.get("authorization").map(String::as_str),
            Some("Bearer test-token")
        );
        let payload1: Value = serde_json::from_slice(&request1.body).expect("parse request json");
        assert_eq!(payload1["filename"], "report.txt");
        assert_eq!(payload1["length"], 5);

        let request2 = request_rx.recv().expect("upload bytes request");
        assert_eq!(request2.request_line, "POST /upload HTTP/1.1");
        assert_eq!(
            request2.headers.get("content-type").map(String::as_str),
            Some("text/plain")
        );
        assert_eq!(request2.body, b"hello");

        let request3 = request_rx.recv().expect("complete upload request");
        assert_eq!(
            request3.request_line,
            "POST /api/files.completeUploadExternal HTTP/1.1"
        );
        assert_eq!(
            request3.headers.get("authorization").map(String::as_str),
            Some("Bearer test-token")
        );
        let payload3: Value = serde_json::from_slice(&request3.body).expect("parse request json");
        assert_eq!(payload3["channel_id"], "C123");
        assert_eq!(payload3["initial_comment"], "attachment comment");
        assert_eq!(payload3["thread_ts"], "1712860000.000001");
        assert_eq!(payload3["files"][0]["id"], "F123");
        assert_eq!(payload3["files"][0]["title"], "report.txt");

        server.join().expect("server thread");
    }

    #[test]
    fn socket_connection_rejects_invalid_auth_as_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let _request = read_request(&mut stream);
            write_raw_response(
                &mut stream,
                "200 OK",
                &[],
                r#"{"ok":false,"error":"invalid_auth"}"#,
            );
        });

        let client = SlackSocketModeClient::new_for_tests(&format!("http://{address}/api"));
        let error = client
            .open_connection_url()
            .expect_err("invalid credentials must fail");

        assert!(matches!(
            error,
            SlackSocketModeError::Authentication { ref code } if code == "invalid_auth"
        ));
        server.join().expect("server thread");
    }

    #[test]
    fn socket_connection_honors_retry_after() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let _request = read_request(&mut stream);
            write_raw_response(
                &mut stream,
                "429 Too Many Requests",
                &[("Retry-After", "17")],
                r#"{"ok":false,"error":"ratelimited"}"#,
            );
        });

        let client = SlackSocketModeClient::new_for_tests(&format!("http://{address}/api"));
        let error = client
            .open_connection_url()
            .expect_err("rate limit must delay retry");

        assert!(matches!(
            error,
            SlackSocketModeError::RateLimited { retry_after }
                if retry_after == Duration::from_secs(17)
        ));
        server.join().expect("server thread");
    }

    #[test]
    fn retry_after_accepts_the_forms_slack_sends() {
        assert_eq!(
            parse_retry_after_seconds("30"),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after_seconds("  12  "),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after_seconds("1.5"),
            Some(Duration::from_secs(2))
        );
        assert_eq!(parse_retry_after_seconds("later"), None);
        assert_eq!(parse_retry_after_seconds("-3"), None);
    }

    #[test]
    fn events_api_envelope_is_acknowledged_only_after_delivery() {
        let (mut socket, server) = connected_socket_pair();
        let client = SlackSocketModeClient::new_for_tests("http://127.0.0.1/api");
        let delivered = Arc::new(AtomicBool::new(false));
        let delivered_for_callback = Arc::clone(&delivered);

        let outcome = client
            .handle_socket_message(
                &mut socket,
                r#"{"type":"events_api","envelope_id":"env-1","payload":{}}"#,
                Some(&|_: &SlackSocketEnvelope| {
                    delivered_for_callback.store(true, Ordering::SeqCst);
                    Ok(SlackEnvelopeDisposition::Delivered)
                }),
            )
            .expect("delivered envelope should be acknowledged");

        assert!(matches!(outcome, SlackSocketMessageOutcome::Delivered));
        assert!(delivered.load(Ordering::SeqCst));
        let ack = server.join().expect("server thread");
        assert_eq!(
            ack.as_deref(),
            Some(r#"{"envelope_id":"env-1"}"#),
            "Slack must be acknowledged after the host has the event"
        );
    }

    #[test]
    fn failed_delivery_leaves_the_envelope_unacknowledged_for_redelivery() {
        let (mut socket, server) = connected_socket_pair();
        let client = SlackSocketModeClient::new_for_tests("http://127.0.0.1/api");

        let error = client
            .handle_socket_message(
                &mut socket,
                r#"{"type":"events_api","envelope_id":"env-1","payload":{}}"#,
                Some(&|_: &SlackSocketEnvelope| bail!("host pipe closed")),
            )
            .expect_err("a failed delivery must not be acknowledged");

        assert!(error.to_string().contains("host pipe closed"));
        assert_eq!(
            server.join().expect("server thread"),
            None,
            "an unacknowledged envelope is what makes Slack redeliver it"
        );
    }

    fn connected_socket_pair() -> (
        WebSocket<MaybeTlsStream<TcpStream>>,
        thread::JoinHandle<Option<String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener addr");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept connection");
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .expect("server read timeout");
            let mut socket = tungstenite::accept(stream).expect("websocket handshake");
            match socket.read() {
                Ok(Message::Text(text)) => Some(text.to_string()),
                _ => None,
            }
        });

        let stream = TcpStream::connect(address).expect("connect to test server");
        let (socket, _) =
            tungstenite::client(format!("ws://{address}/"), MaybeTlsStream::Plain(stream))
                .expect("client websocket handshake");
        (socket, server)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> CapturedRequest {
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("set read timeout");

        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read request");
            assert!(read > 0, "connection closed before headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = find_header_end(&buffer) {
                break position;
            }
        };

        let header_text = String::from_utf8(buffer[..header_end].to_vec()).expect("header utf-8");
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().expect("request line").to_string();
        let mut headers = BTreeMap::new();
        let mut content_length = 0_usize;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line.split_once(':').expect("header separator");
            let normalized = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if normalized == "content-length" {
                content_length = value.parse::<usize>().expect("content length");
            }
            headers.insert(normalized, value);
        }

        let body_start = header_end + 4;
        while buffer.len() < body_start + content_length {
            let read = stream.read(&mut chunk).expect("read request body");
            assert!(read > 0, "connection closed before full body");
            buffer.extend_from_slice(&chunk[..read]);
        }

        CapturedRequest {
            request_line,
            headers,
            body: buffer[body_start..body_start + content_length].to_vec(),
        }
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn write_response(stream: &mut std::net::TcpStream, response: &StubResponse) {
        let body = response.body.as_bytes();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.content_type,
            body.len()
        )
        .expect("write response headers");
        stream.write_all(body).expect("write response body");
        stream.flush().expect("flush response");
    }

    fn write_raw_response(
        stream: &mut std::net::TcpStream,
        status: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) {
        write!(stream, "HTTP/1.1 {status}\r\n").expect("write response status");
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").expect("write response header");
        }
        write!(
            stream,
            "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write response body");
        stream.flush().expect("flush response");
    }
}
