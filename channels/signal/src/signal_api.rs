use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::Write;
use tempfile::NamedTempFile;

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
}

impl SignalClient {
    pub fn new(base_url: String, account: Option<String>) -> Self {
        Self { base_url, account }
    }

    pub fn health(&self) -> Result<SignalHealth> {
        let response = ureq::get(&format!("{}/api/v1/check", self.base_url))
            .call()
            .context("signal health check failed")?;
        if response.status().as_u16() >= 400 {
            bail!("signal health check returned HTTP {}", response.status());
        }

        let version = self
            .rpc_request::<Value>("version", None)
            .ok()
            .and_then(|value| {
                value
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| value.as_str().map(str::to_owned))
            });

        Ok(SignalHealth { version })
    }

    pub fn send_message(
        &self,
        target: &SignalTarget,
        content: &str,
        attachments: &[SignalAttachmentPayload],
    ) -> Result<SignalSendReceipt> {
        let mut params = target_params(target);
        params["message"] = Value::String(content.to_string());
        let attachment_files = attachment_files(attachments)?;
        if let Some(object) = params.as_object_mut()
            && !attachment_files.is_empty()
        {
            object.insert(
                "attachment".to_string(),
                Value::Array(
                    attachment_files
                        .iter()
                        .map(|file| Value::String(file.path().display().to_string()))
                        .collect(),
                ),
            );
        }
        self.insert_account(&mut params);

        let result = self.rpc_request::<Value>("send", Some(params))?;
        let message_id = result
            .get("timestamp")
            .and_then(Value::as_i64)
            .map(|value| value.to_string())
            .or_else(|| {
                result
                    .get("messageId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unknown".to_string());

        Ok(SignalSendReceipt { message_id })
    }

    pub fn send_typing(&self, target: &SignalTarget) -> Result<()> {
        let mut params = target_params(target);
        self.insert_account(&mut params);
        let _: Value = self.rpc_request("sendTyping", Some(params))?;
        Ok(())
    }

    pub fn receive_messages(&self, timeout_secs: u16) -> Result<Vec<Value>> {
        let mut params = json!({
            "timeout": timeout_secs,
        });
        self.insert_account(&mut params);
        let result = self.rpc_request::<Value>("receive", Some(params))?;
        Ok(normalize_receive_result(result))
    }

    fn insert_account(&self, params: &mut Value) {
        if let Some(account) = &self.account
            && let Some(object) = params.as_object_mut()
        {
            object.insert("account".to_string(), Value::String(account.clone()));
        }
    }

    fn rpc_request<T: DeserializeOwned>(&self, method: &str, params: Option<Value>) -> Result<T> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "dispatch-signal",
            "method": method,
            "params": params.unwrap_or_else(|| json!({})),
        })
        .to_string();
        let mut response = ureq::post(&format!("{}/api/v1/rpc", self.base_url))
            .content_type("application/json")
            .send(&request)
            .with_context(|| format!("signal RPC {method} failed"))?;

        if response.status() == 201 {
            return serde_json::from_value(Value::Null).context("parse empty signal RPC response");
        }

        let body = response
            .body_mut()
            .read_to_string()
            .context("read signal RPC body")?;
        let envelope: SignalRpcEnvelope<Value> =
            serde_json::from_str(&body).context("parse signal RPC response")?;

        if let Some(error) = envelope.error {
            let message = error.message.unwrap_or_else(|| "unknown error".to_string());
            return Err(anyhow!("signal RPC {method} failed: {message}"));
        }

        let Some(result) = envelope.result else {
            return Err(anyhow!("signal RPC {method} returned no result"));
        };

        serde_json::from_value(result).context("decode signal RPC result")
    }
}

fn attachment_files(attachments: &[SignalAttachmentPayload]) -> Result<Vec<NamedTempFile>> {
    let mut files = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(attachment.data_base64.as_bytes())
            .with_context(|| format!("failed to decode base64 attachment `{}`", attachment.name))?;
        let mut file = NamedTempFile::new().context("create signal attachment temp file")?;
        file.write_all(&bytes)
            .with_context(|| format!("write signal attachment `{}`", attachment.name))?;
        file.flush()
            .with_context(|| format!("flush signal attachment `{}`", attachment.name))?;
        files.push(file);
    }
    Ok(files)
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

fn target_params(target: &SignalTarget) -> Value {
    match target {
        SignalTarget::Recipient(recipient) => json!({ "recipient": [recipient] }),
        SignalTarget::Group(group_id) => json!({ "groupId": group_id }),
        SignalTarget::Username(username) => json!({ "username": [username] }),
    }
}

#[derive(Debug, serde::Deserialize)]
struct SignalRpcEnvelope<T> {
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error: Option<SignalRpcError>,
}

#[derive(Debug, serde::Deserialize)]
struct SignalRpcError {
    #[serde(default)]
    message: Option<String>,
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
    fn attachment_files_decode_base64_payloads() {
        let files = attachment_files(&[SignalAttachmentPayload {
            name: "hello.txt".to_string(),
            data_base64: "aGVsbG8=".to_string(),
        }])
        .expect("attachment files");

        let content = std::fs::read_to_string(files[0].path()).expect("read attachment file");
        assert_eq!(content, "hello");
    }
}
