use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, net::TcpStream, time::Duration};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

const DEFAULT_API_BASE: &str = "https://slack.com/api";

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
        let url = format!("{}/{}", self.base_url, method);
        let mut response = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .send_json(payload)
            .map_err(|error| anyhow!("{context}: {error}"))?;
        let body = read_json_body(&mut response, context)?;
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

    pub fn receive_event(&self, timeout_secs: u16) -> Result<Option<SlackSocketEnvelope>> {
        let websocket_url = self.open_connection_url()?;
        let (mut socket, _) = tungstenite::connect(websocket_url.as_str()).with_context(|| {
            format!("failed to connect Slack socket mode websocket: {websocket_url}")
        })?;
        configure_websocket_read_timeout(socket.get_mut(), timeout_secs)?;

        loop {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    if let Some(envelope) =
                        self.handle_socket_message(&mut socket, text.as_str())?
                    {
                        return Ok(Some(envelope));
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    let text = std::str::from_utf8(bytes.as_ref())
                        .context("Slack socket mode frame was not valid UTF-8")?;
                    if let Some(envelope) = self.handle_socket_message(&mut socket, text)? {
                        return Ok(Some(envelope));
                    }
                }
                Ok(Message::Ping(payload)) => {
                    socket.send(Message::Pong(payload))?;
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => return Ok(None),
                Ok(Message::Frame(_)) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None);
                }
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(None);
                }
                Err(error) => return Err(error).context("failed to read Slack socket mode frame"),
            }
        }
    }

    pub fn open_connection_url(&self) -> Result<String> {
        let url = format!("{}/apps.connections.open", self.base_url);
        let mut response = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", self.app_token))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send("")
            .map_err(|error| anyhow!("failed to open Slack socket mode connection: {error}"))?;
        let body = read_json_body(&mut response, "failed to open Slack socket mode connection")?;
        body.get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("Slack apps.connections.open response missing url"))
    }

    fn handle_socket_message(
        &self,
        socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
        text: &str,
    ) -> Result<Option<SlackSocketEnvelope>> {
        let envelope: SlackSocketEnvelope =
            serde_json::from_str(text).context("failed to parse Slack socket mode envelope")?;

        match envelope.envelope_type.as_str() {
            "hello" => Ok(None),
            "disconnect" => Ok(None),
            "events_api" => {
                if let Some(envelope_id) = envelope.envelope_id.as_deref() {
                    acknowledge_socket_envelope(socket, envelope_id)?;
                }
                Ok(Some(envelope))
            }
            _ => {
                if let Some(envelope_id) = envelope.envelope_id.as_deref() {
                    acknowledge_socket_envelope(socket, envelope_id)?;
                }
                Ok(None)
            }
        }
    }
}

pub fn send_incoming_webhook(url: &str, content: &str) -> Result<SlackMessage> {
    let mut response = ureq::post(url)
        .header("Content-Type", "application/json")
        .send_json(json!({ "text": content }))
        .map_err(|error| anyhow!("failed to send Slack incoming webhook: {error}"))?;
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
    timeout_secs: u16,
) -> Result<()> {
    let timeout = Some(Duration::from_secs(u64::from(timeout_secs.max(1)) + 1));
    let tcp = match stream {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Ok(()),
    };
    tcp.set_read_timeout(timeout)
        .context("failed to configure Slack socket mode read timeout")
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
    use std::sync::mpsc;
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
}
