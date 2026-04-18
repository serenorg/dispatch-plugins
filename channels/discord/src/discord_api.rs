use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::Read;
use ureq::unversioned::multipart::{Form, Part};

const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Debug)]
pub struct DiscordClient {
    bot_token: String,
    base_url: String,
}

#[derive(Debug, Clone)]
pub struct DiscordIdentity {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DiscordMessage {
    pub id: String,
    pub channel_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordUpload {
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

impl DiscordClient {
    pub fn from_env(bot_token_env: &str) -> Result<Self> {
        let bot_token = std::env::var(bot_token_env)
            .with_context(|| format!("{bot_token_env} is required for the discord channel"))?;
        Ok(Self {
            bot_token,
            base_url: DEFAULT_API_BASE.to_string(),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(base_url: &str) -> Self {
        Self {
            bot_token: "test-token".to_string(),
            base_url: base_url.to_string(),
        }
    }

    pub fn identity(&self) -> Result<DiscordIdentity> {
        let url = format!("{}/users/@me", self.base_url);
        let body = self.get_json(&url, "failed to query Discord bot identity")?;
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("discord identity response missing id"))?
            .to_string();
        let username = body
            .get("username")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("discord identity response missing username"))?
            .to_string();
        let global_name = body
            .get("global_name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(DiscordIdentity {
            id,
            username,
            global_name,
        })
    }

    pub fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        reply_to_message_id: Option<&str>,
        upload: Option<&DiscordUpload>,
    ) -> Result<DiscordMessage> {
        let url = format!("{}/channels/{}/messages", self.base_url, channel_id);
        let mut payload = json!({
            "content": content,
        });
        if let Some(reply_to_message_id) = reply_to_message_id {
            payload["message_reference"] = json!({
                "message_id": reply_to_message_id,
            });
        }

        let body = match upload {
            Some(upload) => self.post_multipart_message(
                &url,
                payload,
                upload,
                "failed to send Discord message",
            )?,
            None => self.post_json(&url, payload, "failed to send Discord message")?,
        };
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("discord message response missing id"))?
            .to_string();
        let channel_id = body
            .get("channel_id")
            .and_then(Value::as_str)
            .unwrap_or(channel_id)
            .to_string();
        Ok(DiscordMessage { id, channel_id })
    }

    fn get_json(&self, url: &str, context: &str) -> Result<Value> {
        let mut response = ureq::get(url)
            .header("Authorization", &format!("Bot {}", self.bot_token))
            .call()
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }

    fn post_json(&self, url: &str, payload: Value, context: &str) -> Result<Value> {
        let mut response = ureq::post(url)
            .header("Authorization", &format!("Bot {}", self.bot_token))
            .header("Content-Type", "application/json")
            .send_json(payload)
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }

    fn post_multipart_message(
        &self,
        url: &str,
        payload: Value,
        upload: &DiscordUpload,
        context: &str,
    ) -> Result<Value> {
        let payload_json = payload.to_string();
        let file_part = Part::bytes(upload.data.as_slice())
            .file_name(&upload.name)
            .mime_str(&upload.mime_type)
            .map_err(|error| anyhow!("{context}: invalid mime type: {error}"))?;
        let form = Form::new()
            .text("payload_json", &payload_json)
            .part("files[0]", file_part);
        let mut response = ureq::post(url)
            .header("Authorization", &format!("Bot {}", self.bot_token))
            .send(form)
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    #[derive(Debug)]
    struct CapturedRequest {
        request_line: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    #[test]
    fn send_message_posts_multipart_attachment_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_request(&mut stream);
            let body = br#"{"id":"msg-1","channel_id":"chan-1"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write response headers");
            stream.write_all(body).expect("write response body");
            stream.flush().expect("flush response");
            request
        });

        let client = DiscordClient::new_for_tests(&format!("http://{address}/api/v10"));
        let message = client
            .send_message(
                "chan-1",
                "hello from discord",
                Some("parent-1"),
                Some(&DiscordUpload {
                    name: "report.txt".to_string(),
                    mime_type: "text/plain".to_string(),
                    data: b"hello".to_vec(),
                }),
            )
            .expect("send message");

        assert_eq!(message.id, "msg-1");
        assert_eq!(message.channel_id, "chan-1");

        let request = server.join().expect("server thread");
        assert_eq!(
            request.request_line,
            "POST /api/v10/channels/chan-1/messages HTTP/1.1"
        );
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bot test-token")
        );
        let content_type = request
            .headers
            .get("content-type")
            .expect("content-type header");
        assert!(
            content_type.starts_with("multipart/form-data; boundary="),
            "unexpected content type: {content_type}"
        );
        let body = String::from_utf8_lossy(&request.body);
        assert!(body.contains("payload_json"));
        assert!(body.contains("\"content\":\"hello from discord\""));
        assert!(body.contains("\"message_id\":\"parent-1\""));
        assert!(body.contains("report.txt"));
        assert!(body.contains("hello"));
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
}
