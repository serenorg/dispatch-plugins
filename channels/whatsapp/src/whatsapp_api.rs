use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::Read;
use ureq::unversioned::multipart::{Form, Part};

const DEFAULT_API_VERSION: &str = "v18.0";
const DEFAULT_API_BASE: &str = "https://graph.facebook.com";

#[derive(Debug)]
pub struct WhatsAppClient {
    access_token: String,
    api_base: String,
    api_version: String,
}

#[derive(Debug, Clone)]
pub struct WhatsAppIdentity {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WhatsAppMessage {
    pub id: String,
    pub to_number: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsAppUpload {
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct WhatsAppMediaRequest<'a> {
    pub media_type: WhatsAppMediaType,
    pub media_ref: WhatsAppMediaRef<'a>,
    pub caption: Option<&'a str>,
    pub filename: Option<&'a str>,
    pub reply_to_message_id: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhatsAppMediaType {
    Image,
    Document,
    Video,
    Audio,
}

impl WhatsAppMediaType {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Document => "document",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhatsAppMediaRef<'a> {
    Link(&'a str),
    Id(&'a str),
}

impl WhatsAppClient {
    pub fn from_env(access_token_env: &str, api_version: Option<&str>) -> Result<Self> {
        let access_token = std::env::var(access_token_env)
            .with_context(|| format!("{access_token_env} is required for the whatsapp channel"))?;
        Ok(Self {
            access_token,
            api_base: DEFAULT_API_BASE.to_string(),
            api_version: api_version.unwrap_or(DEFAULT_API_VERSION).to_string(),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(base_url: &str, api_version: &str) -> Self {
        Self {
            access_token: "test-token".to_string(),
            api_base: base_url.to_string(),
            api_version: api_version.to_string(),
        }
    }

    pub fn identity(&self) -> Result<WhatsAppIdentity> {
        let url = format!("{}/{}/me?fields=id,name", self.api_base, self.api_version);
        let body = self.get_json(&url, "failed to query WhatsApp Graph identity")?;
        Ok(WhatsAppIdentity {
            id: body
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("whatsapp identity response missing id"))?
                .to_string(),
            name: body
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    pub fn send_message(
        &self,
        phone_number_id: &str,
        to_number: &str,
        content: &str,
        reply_to_message_id: Option<&str>,
    ) -> Result<WhatsAppMessage> {
        let url = format!(
            "{}/{}/{}/messages",
            self.api_base, self.api_version, phone_number_id
        );
        let mut payload = json!({
            "messaging_product": "whatsapp",
            "to": to_number,
            "type": "text",
            "text": {
                "body": content
            }
        });
        if let Some(reply_to_message_id) = reply_to_message_id {
            payload["context"] = json!({
                "message_id": reply_to_message_id
            });
        }

        let body = self.post_json(&url, payload, "failed to send WhatsApp message")?;
        let message_id = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.first())
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("whatsapp message response missing messages[0].id"))?
            .to_string();

        Ok(WhatsAppMessage {
            id: message_id,
            to_number: to_number.to_string(),
        })
    }

    pub fn send_media_message(
        &self,
        phone_number_id: &str,
        to_number: &str,
        request: WhatsAppMediaRequest<'_>,
    ) -> Result<WhatsAppMessage> {
        let url = format!(
            "{}/{}/{}/messages",
            self.api_base, self.api_version, phone_number_id
        );
        let mut media_payload = match request.media_ref {
            WhatsAppMediaRef::Link(link) => json!({ "link": link }),
            WhatsAppMediaRef::Id(id) => json!({ "id": id }),
        };
        if let Some(caption) = request.caption.filter(|caption| !caption.is_empty()) {
            media_payload["caption"] = Value::String(caption.to_string());
        }
        if let Some(filename) = request.filename.filter(|filename| !filename.is_empty()) {
            media_payload["filename"] = Value::String(filename.to_string());
        }

        let type_name = request.media_type.wire_name();
        let mut payload = json!({
            "messaging_product": "whatsapp",
            "to": to_number,
            "type": type_name,
            type_name: media_payload,
        });
        if let Some(reply_to_message_id) = request.reply_to_message_id {
            payload["context"] = json!({
                "message_id": reply_to_message_id
            });
        }

        let body = self.post_json(&url, payload, "failed to send WhatsApp media message")?;
        let message_id = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.first())
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("whatsapp message response missing messages[0].id"))?
            .to_string();

        Ok(WhatsAppMessage {
            id: message_id,
            to_number: to_number.to_string(),
        })
    }

    pub fn upload_media(&self, phone_number_id: &str, upload: &WhatsAppUpload) -> Result<String> {
        let url = format!(
            "{}/{}/{}/media",
            self.api_base, self.api_version, phone_number_id
        );
        let file_part = Part::bytes(upload.data.as_slice())
            .file_name(&upload.name)
            .mime_str(&upload.mime_type)
            .map_err(|error| {
                anyhow!("failed to upload WhatsApp media: invalid mime type: {error}")
            })?;
        let form = Form::new()
            .text("messaging_product", "whatsapp")
            .part("file", file_part);
        let body = self.post_multipart(&url, form, "failed to upload WhatsApp media")?;
        body.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("whatsapp media upload response missing id"))
    }

    fn get_json(&self, url: &str, context: &str) -> Result<Value> {
        let mut response = ureq::get(url)
            .header("Authorization", &format!("Bearer {}", self.access_token))
            .call()
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }

    fn post_json(&self, url: &str, payload: Value, context: &str) -> Result<Value> {
        let mut response = ureq::post(url)
            .header("Authorization", &format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .send_json(payload)
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }

    fn post_multipart<'a>(&self, url: &str, form: Form<'a>, context: &str) -> Result<Value> {
        let mut response = ureq::post(url)
            .header("Authorization", &format!("Bearer {}", self.access_token))
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
