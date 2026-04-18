use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::Read;
use ureq::unversioned::multipart::{Form, Part};

const DEFAULT_API_BASE: &str = "https://api.telegram.org";

#[derive(Debug)]
pub struct TelegramClient {
    bot_token: String,
    base_url: String,
}

#[derive(Debug, Clone)]
pub struct TelegramIdentity {
    pub id: String,
    pub username: Option<String>,
    pub first_name: String,
}

#[derive(Debug, Clone)]
pub struct TelegramMessage {
    pub message_id: String,
    pub chat_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramUpload {
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelegramMediaSource<'a> {
    Reference(&'a str),
    Upload(&'a TelegramUpload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramMediaRequest<'a> {
    method: &'a str,
    field_name: &'a str,
    chat_id: &'a str,
    media: TelegramMediaSource<'a>,
    caption: Option<&'a str>,
    reply_to_message_id: Option<i64>,
    message_thread_id: Option<i64>,
    context: &'a str,
}

impl TelegramClient {
    pub fn from_env(bot_token_env: &str) -> Result<Self> {
        let bot_token = std::env::var(bot_token_env)
            .with_context(|| format!("{bot_token_env} is required for the telegram channel"))?;
        Ok(Self {
            bot_token,
            base_url: DEFAULT_API_BASE.to_string(),
        })
    }

    pub fn identity(&self) -> Result<TelegramIdentity> {
        let body = self.call_api("getMe", None, "failed to query Telegram bot identity")?;
        let id = value_to_string(
            body.get("id")
                .ok_or_else(|| anyhow!("telegram identity response missing id"))?,
        )?;
        let first_name = body
            .get("first_name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("telegram identity response missing first_name"))?
            .to_string();
        let username = body
            .get("username")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(TelegramIdentity {
            id,
            username,
            first_name,
        })
    }

    pub fn set_webhook(
        &self,
        webhook_url: &str,
        secret_token: Option<&str>,
        drop_pending_updates: bool,
    ) -> Result<()> {
        let mut payload = json!({
            "url": webhook_url,
            "drop_pending_updates": drop_pending_updates,
        });
        if let Some(secret_token) = secret_token {
            payload["secret_token"] = Value::String(secret_token.to_string());
        }
        let _ = self.call_api(
            "setWebhook",
            Some(payload),
            "failed to set Telegram webhook",
        )?;
        Ok(())
    }

    pub fn delete_webhook(&self, drop_pending_updates: bool) -> Result<()> {
        let payload = json!({
            "drop_pending_updates": drop_pending_updates,
        });
        let _ = self.call_api(
            "deleteWebhook",
            Some(payload),
            "failed to delete Telegram webhook",
        )?;
        Ok(())
    }

    pub fn get_updates(&self, offset: Option<i64>, timeout_secs: u16) -> Result<Vec<Value>> {
        let mut payload = json!({
            "limit": 100,
            "timeout": timeout_secs,
        });
        if let Some(offset) = offset {
            payload["offset"] = Value::Number(offset.into());
        }

        let body = self.call_api(
            "getUpdates",
            Some(payload),
            "failed to poll Telegram updates",
        )?;
        body.as_array()
            .cloned()
            .ok_or_else(|| anyhow!("failed to poll Telegram updates: result was not an array"))
    }

    pub fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        reply_to_message_id: Option<i64>,
        message_thread_id: Option<i64>,
    ) -> Result<TelegramMessage> {
        let mut payload = json!({
            "chat_id": chat_id,
            "text": content,
        });
        if let Some(reply_to_message_id) = reply_to_message_id {
            payload["reply_parameters"] = json!({
                "message_id": reply_to_message_id,
            });
        }
        if let Some(message_thread_id) = message_thread_id {
            payload["message_thread_id"] = Value::Number(message_thread_id.into());
        }

        let body = self.call_api(
            "sendMessage",
            Some(payload),
            "failed to send Telegram message",
        )?;
        let message_id = body
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("telegram message response missing message_id"))?
            .to_string();
        let chat_id = value_to_string(
            body.get("chat")
                .and_then(|chat| chat.get("id"))
                .ok_or_else(|| anyhow!("telegram message response missing chat.id"))?,
        )?;
        Ok(TelegramMessage {
            message_id,
            chat_id,
        })
    }

    pub fn send_photo(
        &self,
        chat_id: &str,
        photo: TelegramMediaSource<'_>,
        caption: Option<&str>,
        reply_to_message_id: Option<i64>,
        message_thread_id: Option<i64>,
    ) -> Result<TelegramMessage> {
        self.send_media(TelegramMediaRequest {
            method: "sendPhoto",
            field_name: "photo",
            chat_id,
            media: photo,
            caption,
            reply_to_message_id,
            message_thread_id,
            context: "failed to send Telegram photo",
        })
    }

    pub fn send_document(
        &self,
        chat_id: &str,
        document: TelegramMediaSource<'_>,
        caption: Option<&str>,
        reply_to_message_id: Option<i64>,
        message_thread_id: Option<i64>,
    ) -> Result<TelegramMessage> {
        self.send_media(TelegramMediaRequest {
            method: "sendDocument",
            field_name: "document",
            chat_id,
            media: document,
            caption,
            reply_to_message_id,
            message_thread_id,
            context: "failed to send Telegram document",
        })
    }

    pub fn send_chat_action(
        &self,
        chat_id: &str,
        action: &str,
        message_thread_id: Option<i64>,
    ) -> Result<()> {
        let mut payload = json!({
            "chat_id": chat_id,
            "action": action,
        });
        if let Some(message_thread_id) = message_thread_id {
            payload["message_thread_id"] = Value::Number(message_thread_id.into());
        }

        let _ = self.call_api(
            "sendChatAction",
            Some(payload),
            "failed to send Telegram chat action",
        )?;
        Ok(())
    }

    fn call_api(&self, method: &str, payload: Option<Value>, context: &str) -> Result<Value> {
        let url = format!("{}/bot{}/{}", self.base_url, self.bot_token, method);
        let mut response = match payload {
            Some(payload) => ureq::post(&url)
                .header("Content-Type", "application/json")
                .send_json(payload),
            None => ureq::get(&url).call(),
        }
        .map_err(|error| anyhow!("{context}: {error}"))?;
        let body = read_json_body(&mut response, context)?;
        let ok = body
            .get("ok")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("{context}: telegram response missing ok flag"))?;
        if !ok {
            let description = body
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown Telegram API error");
            bail!("{context}: {description}");
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{context}: telegram response missing result"))
    }

    fn call_api_multipart<'a>(&self, method: &str, form: Form<'a>, context: &str) -> Result<Value> {
        let url = format!("{}/bot{}/{}", self.base_url, self.bot_token, method);
        let mut response = ureq::post(&url)
            .send(form)
            .map_err(|error| anyhow!("{context}: {error}"))?;
        let body = read_json_body(&mut response, context)?;
        let ok = body
            .get("ok")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("{context}: telegram response missing ok flag"))?;
        if !ok {
            let description = body
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown Telegram API error");
            bail!("{context}: {description}");
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{context}: telegram response missing result"))
    }

    fn send_media(&self, request: TelegramMediaRequest<'_>) -> Result<TelegramMessage> {
        let body = match request.media {
            TelegramMediaSource::Reference(reference) => {
                let mut payload = json!({
                    "chat_id": request.chat_id,
                });
                payload[request.field_name] = Value::String(reference.to_string());
                if let Some(caption) = request.caption.filter(|caption| !caption.is_empty()) {
                    payload["caption"] = Value::String(caption.to_string());
                }
                if let Some(reply_to_message_id) = request.reply_to_message_id {
                    payload["reply_parameters"] = json!({
                        "message_id": reply_to_message_id,
                    });
                }
                if let Some(message_thread_id) = request.message_thread_id {
                    payload["message_thread_id"] = Value::Number(message_thread_id.into());
                }
                self.call_api(request.method, Some(payload), request.context)?
            }
            TelegramMediaSource::Upload(upload) => {
                let chat_id_text = request.chat_id.to_string();
                let caption_text = request
                    .caption
                    .filter(|caption| !caption.is_empty())
                    .map(str::to_owned);
                let reply_parameters_text =
                    request.reply_to_message_id.map(|reply_to_message_id| {
                        json!({
                            "message_id": reply_to_message_id,
                        })
                        .to_string()
                    });
                let message_thread_id_text = request
                    .message_thread_id
                    .map(|message_thread_id| message_thread_id.to_string());

                let media_part = Part::bytes(upload.data.as_slice())
                    .file_name(&upload.name)
                    .mime_str(&upload.mime_type)
                    .map_err(|error| anyhow!("{}: invalid mime type: {error}", request.context))?;
                let mut form = Form::new()
                    .text("chat_id", &chat_id_text)
                    .part(request.field_name, media_part);
                if let Some(caption_text) = caption_text.as_deref() {
                    form = form.text("caption", caption_text);
                }
                if let Some(reply_parameters_text) = reply_parameters_text.as_deref() {
                    form = form.text("reply_parameters", reply_parameters_text);
                }
                if let Some(message_thread_id_text) = message_thread_id_text.as_deref() {
                    form = form.text("message_thread_id", message_thread_id_text);
                }
                self.call_api_multipart(request.method, form, request.context)?
            }
        };
        telegram_message_from_result(body)
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

fn value_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        other => Err(anyhow!(
            "expected string-compatible Telegram id, got {other}"
        )),
    }
}

fn telegram_message_from_result(body: Value) -> Result<TelegramMessage> {
    let message_id = body
        .get("message_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("telegram message response missing message_id"))?
        .to_string();
    let chat_id = value_to_string(
        body.get("chat")
            .and_then(|chat| chat.get("id"))
            .ok_or_else(|| anyhow!("telegram message response missing chat.id"))?,
    )?;
    Ok(TelegramMessage {
        message_id,
        chat_id,
    })
}
