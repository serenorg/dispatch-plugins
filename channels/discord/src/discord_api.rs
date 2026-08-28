use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::{ErrorKind, Read},
    time::Duration,
};
use ureq::unversioned::multipart::{Form, Part};

const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";
pub const MESSAGE_CONTENT_LIMIT: usize = 2_000;
/// Units reserved for closing and reopening a continued code block.
const CODE_FENCE_MARKER_UNITS: usize = 8;
const CODE_FENCE_CHUNK_LIMIT: usize = MESSAGE_CONTENT_LIMIT - CODE_FENCE_MARKER_UNITS;
/// Maximum cumulative provider-requested wait per delivery.
const RATE_LIMIT_WAIT_BUDGET: Duration = Duration::from_secs(10);
/// Floor for a provider-stated retry wait, so a zero value does not spin.
const RATE_LIMIT_MIN_WAIT: Duration = Duration::from_millis(250);
/// Bound provider reads so read-back cannot occupy the plugin indefinitely.
const READ_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound typing requests so worker shutdown cannot wait indefinitely.
const TYPING_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Discord JSON error codes that prove an addressed resource is absent.
const UNKNOWN_CHANNEL: i64 = 10_003;
const UNKNOWN_MESSAGE: i64 = 10_008;

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
pub(super) struct DiscordFetchedMessage {
    pub(super) guild_id: Option<String>,
    pub(super) content: String,
    pub(super) author: Option<DiscordFetchedMessageAuthor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DiscordFetchedMessageAuthor {
    pub(super) id: String,
    pub(super) display_name: Option<String>,
    pub(super) username: Option<String>,
    pub(super) is_bot: bool,
}

#[derive(Debug)]
struct DiscordMessageTooLong {
    content_units: usize,
}

impl fmt::Display for DiscordMessageTooLong {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "message_too_long: Discord content uses {} characters; limit is {}",
            self.content_units, MESSAGE_CONTENT_LIMIT
        )
    }
}

impl std::error::Error for DiscordMessageTooLong {}

#[derive(Debug)]
struct DiscordTransportError {
    context: String,
    code: &'static str,
}

impl fmt::Display for DiscordTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.context, self.code)
    }
}

impl std::error::Error for DiscordTransportError {}

/// A provider response that cannot be trusted as an answer to the request that
/// produced it. `reason` is a fixed token so no response body reaches the host.
#[derive(Debug)]
struct DiscordInvalidResponse {
    context: String,
    reason: &'static str,
    /// Response field the reason refers to. Field names are this plugin's own
    /// tokens, never provider data.
    field: Option<&'static str>,
}

impl fmt::Display for DiscordInvalidResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.context, self.reason)?;
        if let Some(field) = self.field {
            write!(formatter, " field={field}")?;
        }
        Ok(())
    }
}

impl std::error::Error for DiscordInvalidResponse {}

#[derive(Debug)]
struct DiscordChunkDeliveryError {
    failed_chunk: usize,
    chunk_count: usize,
    completed_chunks: usize,
    source: anyhow::Error,
}

impl fmt::Display for DiscordChunkDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "discord_chunk_delivery_failed: chunk {} of {} failed after {} completed chunks: {}",
            self.failed_chunk, self.chunk_count, self.completed_chunks, self.source
        )
    }
}

impl std::error::Error for DiscordChunkDeliveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscordApiError {
    http_status: u16,
    code: Option<i64>,
    field: Option<String>,
    detail: Option<String>,
    /// Provider-stated wait before retrying, read from a 429 response.
    retry_after: Option<Duration>,
    context: String,
}

impl fmt::Display for DiscordApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: discord_api_error http_status={}",
            self.context, self.http_status
        )?;
        if let Some(code) = self.code {
            write!(formatter, " code={code}")?;
        }
        if let Some(field) = &self.field {
            write!(formatter, " field={field}")?;
        }
        if let Some(detail) = &self.detail {
            write!(formatter, " detail={detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for DiscordApiError {}

/// Provider-reported shape of a channel, used to prove thread parentage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordChannelInfo {
    /// Discord channel type discriminant.
    pub kind: u8,
    /// Channel a thread descends from. Absent for a top-level channel.
    pub parent_id: Option<String>,
    /// Guild owning the channel, when the provider reports one.
    pub guild_id: Option<String>,
    /// Other accounts in a direct-message channel. Discord omits the current
    /// bot account, so a type-1 DM normally contains exactly one recipient.
    pub recipient_ids: Vec<String>,
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

    pub fn bot_token(&self) -> &str {
        &self.bot_token
    }

    /// Read channel metadata for a conversation the gateway did not describe.
    ///
    /// The type discriminant is required: a caller that cannot read it cannot
    /// tell a channel from a thread, and therefore cannot decide which
    /// allowlist applies.
    pub fn channel(&self, channel_id: &str) -> Result<DiscordChannelInfo> {
        let url = format!("{}/channels/{}", self.base_url, channel_id);
        let context = "failed to query Discord channel";
        let body = self.get_json_with_rate_limit(&url, context)?;
        let kind = body
            .get("type")
            .and_then(Value::as_u64)
            .and_then(|kind| u8::try_from(kind).ok())
            .ok_or_else(|| {
                invalid_response_field(context, "Discord returned an invalid field", "type")
            })?;
        let recipient_ids = match body.get("recipients") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(recipients)) => recipients
                .iter()
                .map(|recipient| required_response_string(recipient, "id", context))
                .collect::<Result<Vec<_>>>()?,
            Some(_) => {
                return Err(invalid_response_field(
                    context,
                    "Discord returned an invalid field",
                    "recipients",
                ));
            }
        };
        Ok(DiscordChannelInfo {
            kind,
            parent_id: optional_response_string(&body, "parent_id", context)?,
            guild_id: optional_response_string(&body, "guild_id", context)?,
            recipient_ids,
        })
    }

    /// Fetch one exact message without listing neighboring channel history.
    pub(super) fn fetch_message(
        &self,
        channel_id: &str,
        message_id: &str,
        expected_guild_id: Option<&str>,
    ) -> Result<Option<DiscordFetchedMessage>> {
        let url = format!(
            "{}/channels/{}/messages/{}",
            self.base_url, channel_id, message_id
        );
        let context = "failed to fetch Discord message";
        let body = match self.get_json_with_rate_limit(&url, context) {
            Ok(body) => body,
            Err(error) if message_reference_is_absent(&error) => return Ok(None),
            Err(error) => return Err(error),
        };

        let id = required_response_string(&body, "id", context)?;
        let returned_channel_id = required_response_string(&body, "channel_id", context)?;
        if id != message_id || returned_channel_id != channel_id {
            return Err(invalid_response(
                context,
                "Discord returned different message coordinates",
            ));
        }

        let member_nick = match body.get("member") {
            None | Some(Value::Null) => None,
            Some(member @ Value::Object(_)) => optional_response_string(member, "nick", context)?,
            Some(_) => {
                return Err(invalid_response_field(
                    context,
                    "Discord returned an invalid field",
                    "member",
                ));
            }
        };
        let webhook_author = optional_response_string(&body, "webhook_id", context)?.is_some();
        let author = match body.get("author") {
            None | Some(Value::Null) => None,
            Some(author @ Value::Object(_)) => Some(DiscordFetchedMessageAuthor {
                id: required_response_string(author, "id", context)?,
                display_name: member_nick.or(optional_response_string(
                    author,
                    "global_name",
                    context,
                )?),
                username: optional_response_string(author, "username", context)?,
                is_bot: optional_response_bool(author, "bot", context)?.unwrap_or(false)
                    || webhook_author,
            }),
            Some(_) => {
                return Err(invalid_response_field(
                    context,
                    "Discord returned an invalid field",
                    "author",
                ));
            }
        };

        let guild_id = optional_response_string(&body, "guild_id", context)?;
        // Discord omits `guild_id` on this route, so absence is expected; a
        // reported guild that disagrees with the authorized channel is not.
        if guild_id
            .as_deref()
            .is_some_and(|guild_id| expected_guild_id != Some(guild_id))
        {
            return Err(invalid_response(
                context,
                "Discord returned a different message guild",
            ));
        }

        Ok(Some(DiscordFetchedMessage {
            guild_id,
            content: required_response_string(&body, "content", context)?,
            author,
        }))
    }

    #[cfg(test)]
    fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        reply_to_message_id: Option<&str>,
        upload: Option<&DiscordUpload>,
    ) -> Result<DiscordMessage> {
        self.send_message_with_nonce(channel_id, content, reply_to_message_id, upload, None)
    }

    fn send_message_with_nonce(
        &self,
        channel_id: &str,
        content: &str,
        reply_to_message_id: Option<&str>,
        upload: Option<&DiscordUpload>,
        nonce: Option<&str>,
    ) -> Result<DiscordMessage> {
        let content_units = message_content_units(content);
        if content_units > MESSAGE_CONTENT_LIMIT {
            return Err(anyhow!(DiscordMessageTooLong { content_units }));
        }
        let url = format!("{}/channels/{}/messages", self.base_url, channel_id);
        let mut payload = json!({
            "content": content,
        });
        if let Some(reply_to_message_id) = reply_to_message_id {
            payload["message_reference"] = json!({
                "message_id": reply_to_message_id,
            });
        }
        if let Some(nonce) = nonce {
            payload["nonce"] = json!(nonce);
            payload["enforce_nonce"] = json!(true);
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
        let context = "failed to send Discord message";
        let id = required_response_string(&body, "id", context)?;
        let returned_channel_id = required_response_string(&body, "channel_id", context)?;
        if returned_channel_id != channel_id {
            return Err(invalid_response(
                context,
                "Discord returned a different delivery channel",
            ));
        }
        Ok(DiscordMessage {
            id,
            channel_id: returned_channel_id,
        })
    }

    pub fn send_message_chunks(
        &self,
        channel_id: &str,
        content: &str,
        reply_to_message_id: Option<&str>,
        upload: Option<&DiscordUpload>,
    ) -> Result<Vec<DiscordMessage>> {
        let chunks = render_message_chunks(content);
        let chunk_count = chunks.len();
        let mut messages = Vec::with_capacity(chunk_count);

        let mut wait_budget = RATE_LIMIT_WAIT_BUDGET;
        for (index, chunk) in chunks.into_iter().enumerate() {
            let nonce = reply_to_message_id.map(|reply_to_message_id| {
                message_chunk_nonce(
                    channel_id,
                    reply_to_message_id,
                    &chunk,
                    index,
                    (index == 0).then_some(upload).flatten(),
                )
            });
            let send = || {
                self.send_message_with_nonce(
                    channel_id,
                    &chunk,
                    (index == 0).then_some(reply_to_message_id).flatten(),
                    (index == 0).then_some(upload).flatten(),
                    nonce.as_deref(),
                )
            };
            let mut result = send();
            // Discord rejects a rate-limited request before message creation.
            while let Some(wait) = rate_limit_wait(result.as_ref().err(), wait_budget) {
                std::thread::sleep(wait);
                wait_budget -= wait;
                result = send();
            }
            match result {
                Ok(message) => messages.push(message),
                Err(source) => {
                    return Err(anyhow!(DiscordChunkDeliveryError {
                        failed_chunk: index + 1,
                        chunk_count,
                        completed_chunks: messages.len(),
                        source,
                    }));
                }
            }
        }

        Ok(messages)
    }

    pub fn trigger_typing(&self, channel_id: &str) -> Result<()> {
        let url = format!("{}/channels/{}/typing", self.base_url, channel_id);
        let mut response = ureq::post(url)
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(TYPING_REQUEST_TIMEOUT))
            .build()
            .header("Authorization", &format!("Bot {}", self.bot_token))
            .send_empty()
            .map_err(|error| transport_error("failed to trigger Discord typing", error))?;
        read_empty_response(&mut response, "failed to trigger Discord typing")
    }

    fn get_json(&self, url: &str, context: &str) -> Result<Value> {
        let mut response = ureq::get(url)
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(READ_REQUEST_TIMEOUT))
            .build()
            .header("Authorization", &format!("Bot {}", self.bot_token))
            .call()
            .map_err(|error| transport_error(context, error))?;
        read_json_body(&mut response, context)
    }

    fn get_json_with_rate_limit(&self, url: &str, context: &str) -> Result<Value> {
        let mut wait_budget = RATE_LIMIT_WAIT_BUDGET;
        loop {
            match self.get_json(url, context) {
                Ok(body) => return Ok(body),
                Err(error) => {
                    let Some(wait) = rate_limit_wait(Some(&error), wait_budget) else {
                        return Err(error);
                    };
                    std::thread::sleep(wait);
                    wait_budget -= wait;
                }
            }
        }
    }

    fn post_json(&self, url: &str, payload: Value, context: &str) -> Result<Value> {
        let mut response = ureq::post(url)
            .config()
            .http_status_as_error(false)
            .build()
            .header("Authorization", &format!("Bot {}", self.bot_token))
            .header("Content-Type", "application/json")
            .send_json(payload)
            .map_err(|error| transport_error(context, error))?;
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
            .config()
            .http_status_as_error(false)
            .build()
            .header("Authorization", &format!("Bot {}", self.bot_token))
            .send(form)
            .map_err(|error| transport_error(context, error))?;
        read_json_body(&mut response, context)
    }
}

/// Discord states the limit in characters. UTF-16 code units are the widest
/// reading of that, so a chunk sized this way is accepted under either count.
fn message_content_units(content: &str) -> usize {
    content.chars().map(char::len_utf16).sum()
}

fn split_message_content(content: &str) -> Vec<&str> {
    split_message_content_with_limit(content, MESSAGE_CONTENT_LIMIT)
}

fn split_message_content_with_limit(content: &str, limit: usize) -> Vec<&str> {
    if content.is_empty() {
        return vec![content];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;
    while message_content_units(remaining) > limit {
        let hard_end = bounded_content_end(remaining, limit);
        let candidate = &remaining[..hard_end];
        let split_at = preferred_split(candidate, limit).unwrap_or(hard_end);
        let split_at = backtick_run_start(remaining, split_at);
        chunks.push(&remaining[..split_at]);
        remaining = &remaining[split_at..];
    }
    chunks.push(remaining);
    chunks
}

/// Keep a split out of a backtick run so delimiter parity remains intact.
fn backtick_run_start(content: &str, split_at: usize) -> usize {
    let bytes = content.as_bytes();
    if split_at == 0 || split_at >= bytes.len() || bytes[split_at] != b'`' {
        return split_at;
    }
    let mut start = split_at;
    while start > 0 && bytes[start - 1] == b'`' {
        start -= 1;
    }
    // Retain the hard split when moving it would prevent progress.
    if start == 0 { split_at } else { start }
}

fn render_message_chunks(content: &str) -> Vec<String> {
    if message_content_units(content) <= MESSAGE_CONTENT_LIMIT || !content.contains("```") {
        return split_message_content(content)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
    }

    // Track triple-backtick parity across the original chunks.
    let raw_chunks = split_message_content_with_limit(content, CODE_FENCE_CHUNK_LIMIT);
    let chunk_count = raw_chunks.len();
    let mut code_fence_open = false;
    raw_chunks
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            let mut chunk = String::with_capacity(raw.len() + 8);
            if code_fence_open {
                chunk.push_str("```\n");
            }
            chunk.push_str(raw);
            if raw.match_indices("```").count() % 2 == 1 {
                code_fence_open = !code_fence_open;
            }
            if code_fence_open && index + 1 < chunk_count {
                chunk.push_str("\n```");
            }
            chunk
        })
        .collect()
}

fn bounded_content_end(content: &str, limit: usize) -> usize {
    let mut units = 0;
    let mut end = 0;
    for (index, character) in content.char_indices() {
        let width = character.len_utf16();
        if units + width > limit {
            break;
        }
        units += width;
        end = index + character.len_utf8();
    }
    end
}

fn preferred_split(candidate: &str, limit: usize) -> Option<usize> {
    candidate
        .rfind("\n\n")
        .map(|index| index + 2)
        .filter(|index| preferred_boundary_is_full(candidate, *index, limit))
        .or_else(|| {
            candidate
                .rfind('\n')
                .map(|index| index + 1)
                .filter(|index| preferred_boundary_is_full(candidate, *index, limit))
        })
        .or_else(|| {
            candidate
                .char_indices()
                .rev()
                .find(|(index, character)| {
                    character.is_whitespace()
                        && preferred_boundary_is_full(
                            candidate,
                            *index + character.len_utf8(),
                            limit,
                        )
                })
                .map(|(index, character)| index + character.len_utf8())
        })
}

fn preferred_boundary_is_full(candidate: &str, index: usize, limit: usize) -> bool {
    index > 0 && message_content_units(&candidate[..index]) >= limit / 2
}

/// Return the permitted wait for a rate-limited request.
fn rate_limit_wait(error: Option<&anyhow::Error>, budget: Duration) -> Option<Duration> {
    let api_error = error?
        .chain()
        .find_map(|source| source.downcast_ref::<DiscordApiError>())?;
    if api_error.http_status != 429 {
        return None;
    }
    let wait = api_error.retry_after?.max(RATE_LIMIT_MIN_WAIT);
    (wait <= budget).then_some(wait)
}

/// Parse the provider wait, preferring the more precise response body.
fn parse_retry_after(header_seconds: Option<f64>, body: Option<&Value>) -> Option<Duration> {
    let body_seconds = body
        .and_then(|body| body.get("retry_after"))
        .and_then(Value::as_f64);
    body_seconds
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .or_else(|| header_seconds.and_then(|seconds| Duration::try_from_secs_f64(seconds).ok()))
}

fn transport_error(context: &str, error: ureq::Error) -> anyhow::Error {
    let code = match error {
        ureq::Error::Timeout(_) => "discord_timeout",
        _ => "discord_transport_error",
    };
    anyhow!(DiscordTransportError {
        context: context.to_string(),
        code,
    })
}

fn response_body_error(context: &str, error: std::io::Error) -> anyhow::Error {
    let code = match error.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => "discord_timeout",
        _ => "discord_transport_error",
    };
    anyhow!(DiscordTransportError {
        context: context.to_string(),
        code,
    })
}

fn invalid_response(context: &str, reason: &'static str) -> anyhow::Error {
    anyhow!(DiscordInvalidResponse {
        context: context.to_string(),
        reason,
        field: None,
    })
}

fn invalid_response_field(
    context: &str,
    reason: &'static str,
    field: &'static str,
) -> anyhow::Error {
    anyhow!(DiscordInvalidResponse {
        context: context.to_string(),
        reason,
        field: Some(field),
    })
}

fn required_response_string(body: &Value, field: &'static str, context: &str) -> Result<String> {
    match body.get(field) {
        Some(Value::String(value)) => Ok(value.clone()),
        None => Err(invalid_response_field(
            context,
            "Discord response is missing a required field",
            field,
        )),
        Some(_) => Err(invalid_response_field(
            context,
            "Discord returned an invalid field",
            field,
        )),
    }
}

fn optional_response_string(
    body: &Value,
    field: &'static str,
    context: &str,
) -> Result<Option<String>> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid_response_field(
            context,
            "Discord returned an invalid field",
            field,
        )),
    }
}

fn optional_response_bool(
    body: &Value,
    field: &'static str,
    context: &str,
) -> Result<Option<bool>> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid_response_field(
            context,
            "Discord returned an invalid field",
            field,
        )),
    }
}

pub(super) fn message_reference_is_absent(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<DiscordApiError>())
        .is_some_and(|error| {
            error.http_status == 404
                && matches!(error.code, Some(UNKNOWN_CHANNEL | UNKNOWN_MESSAGE))
        })
}

fn message_chunk_nonce(
    channel_id: &str,
    reply_to_message_id: &str,
    content: &str,
    index: usize,
    upload: Option<&DiscordUpload>,
) -> String {
    let mut digest = Sha256::new();
    digest_field(&mut digest, channel_id.as_bytes());
    digest_field(&mut digest, reply_to_message_id.as_bytes());
    digest_field(&mut digest, &(index as u64).to_be_bytes());
    digest_field(&mut digest, content.as_bytes());
    if let Some(upload) = upload {
        digest_field(&mut digest, upload.name.as_bytes());
        digest_field(&mut digest, upload.mime_type.as_bytes());
        digest_field(&mut digest, &upload.data);
    }
    format!("dispatch-{}", hex::encode(&digest.finalize()[..8]))
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

pub fn delivery_error_code(error: &anyhow::Error) -> Option<&'static str> {
    if let Some(partial) = error
        .chain()
        .find_map(|source| source.downcast_ref::<DiscordChunkDeliveryError>())
        && partial.completed_chunks > 0
    {
        return Some("partial_delivery");
    }
    if error
        .chain()
        .any(|source| source.is::<DiscordMessageTooLong>())
    {
        return Some("message_too_long");
    }
    if let Some(transport) = error
        .chain()
        .find_map(|source| source.downcast_ref::<DiscordTransportError>())
    {
        return Some(match transport.code {
            "discord_timeout" => "provider_timeout",
            _ => "provider_transport_error",
        });
    }
    if error
        .chain()
        .any(|source| source.is::<DiscordInvalidResponse>())
    {
        return Some("provider_invalid_response");
    }
    error
        .chain()
        .find_map(|source| source.downcast_ref::<DiscordApiError>())
        .map(|error| match error.http_status {
            400 => "provider_invalid_request",
            401 => "provider_authentication_failed",
            403 => "provider_permission_denied",
            404 => "provider_not_found",
            429 => "provider_rate_limited",
            500..=599 => "provider_unavailable",
            _ => "provider_error",
        })
}

fn read_json_body(response: &mut ureq::http::Response<ureq::Body>, context: &str) -> Result<Value> {
    let status = response.status();
    let retry_after_header = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok());
    let mut body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .reader();
    let mut text = String::new();
    body.read_to_string(&mut text)
        .map_err(|error| response_body_error(context, error))?;
    if !status.is_success() {
        let parsed = serde_json::from_str::<Value>(&text).ok();
        let code = parsed
            .as_ref()
            .and_then(|body| body.get("code"))
            .and_then(Value::as_i64);
        let (field, detail) = parsed
            .as_ref()
            .and_then(|body| body.get("errors"))
            .and_then(|errors| find_error_detail(errors, &mut Vec::new()))
            .unwrap_or((None, None));
        return Err(anyhow!(DiscordApiError {
            http_status: status.as_u16(),
            code,
            field,
            detail,
            retry_after: parse_retry_after(retry_after_header, parsed.as_ref()),
            context: context.to_string(),
        }));
    }
    serde_json::from_str(&text)
        .map_err(|_| invalid_response(context, "Discord returned malformed JSON"))
}

fn read_empty_response(
    response: &mut ureq::http::Response<ureq::Body>,
    context: &str,
) -> Result<()> {
    if response.status() == ureq::http::StatusCode::NO_CONTENT {
        return Ok(());
    }
    if response.status().is_success() {
        // A non-204 success does not prove that typing was triggered.
        return Err(invalid_response(
            context,
            "Discord answered a no-content route with a status other than 204",
        ));
    }
    read_json_body(response, context).map(|_| ())
}

fn find_error_detail(
    value: &Value,
    path: &mut Vec<String>,
) -> Option<(Option<String>, Option<String>)> {
    let object = value.as_object()?;
    if let Some(detail) = object
        .get("_errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .and_then(safe_token)
    {
        let field = (!path.is_empty()).then(|| path.join("."));
        return Some((field, Some(detail)));
    }

    for (key, child) in object {
        if key == "_errors" {
            continue;
        }
        let Some(key) = safe_token(key) else {
            continue;
        };
        path.push(key);
        if let Some(detail) = find_error_detail(child, path) {
            return Some(detail);
        }
        path.pop();
    }
    None
}

fn safe_token(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then(|| value.to_string())
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
    fn channel_rejects_malformed_provider_shape() {
        for body in [
            json!({ "guild_id": "333" }),
            json!({ "type": "0", "guild_id": "333" }),
            json!({ "type": 0, "guild_id": 333 }),
            json!({ "type": 11, "guild_id": "333", "parent_id": false }),
            json!({ "type": 1, "recipients": {} }),
            json!({ "type": 1, "recipients": [{ "id": 444 }] }),
        ] {
            let (base_url, server) = spawn_provider(vec![("200 OK", body.to_string())]);
            let error = DiscordClient::new_for_tests(&base_url)
                .channel("111")
                .expect_err("malformed channel shape");

            assert_eq!(
                delivery_error_code(&error),
                Some("provider_invalid_response")
            );
            server.join().expect("server thread");
        }
    }

    #[test]
    fn fetch_message_reads_only_the_exact_provider_route() {
        let body = json!({
            "id": "222222222222222222",
            "channel_id": "111111111111111111",
            "guild_id": "333333333333333333",
            "content": "hello from Discord",
            "author": {
                "id": "444444444444444444",
                "username": "dispatch-bot",
                "global_name": "Dispatch Bot",
                "bot": true
            },
            "member": { "nick": "Agent" }
        });
        let (base_url, server) = spawn_provider(vec![("200 OK", body.to_string())]);
        let message = DiscordClient::new_for_tests(&base_url)
            .fetch_message(
                "111111111111111111",
                "222222222222222222",
                Some("333333333333333333"),
            )
            .expect("fetch exact message")
            .expect("message exists");

        assert_eq!(message.guild_id.as_deref(), Some("333333333333333333"));
        assert_eq!(message.content, "hello from Discord");
        assert_eq!(
            message.author,
            Some(DiscordFetchedMessageAuthor {
                id: "444444444444444444".to_string(),
                display_name: Some("Agent".to_string()),
                username: Some("dispatch-bot".to_string()),
                is_bot: true,
            })
        );

        let requests = server.join().expect("server thread");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].request_line,
            "GET /api/v10/channels/111111111111111111/messages/222222222222222222 HTTP/1.1"
        );
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some("Bot test-token")
        );
    }

    #[test]
    fn fetch_message_distinguishes_webhook_authors_from_null_webhook_ids() {
        for (webhook_id, expected_is_bot) in [(Value::Null, false), (json!("555"), true)] {
            let body = json!({
                "id": "222",
                "channel_id": "111",
                "guild_id": "333",
                "content": "hello",
                "webhook_id": webhook_id,
                "author": { "id": "444", "username": "sender", "bot": false }
            });
            let (base_url, server) = spawn_provider(vec![("200 OK", body.to_string())]);
            let message = DiscordClient::new_for_tests(&base_url)
                .fetch_message("111", "222", Some("333"))
                .expect("fetch message")
                .expect("message exists");

            assert_eq!(
                message.author.as_ref().map(|author| author.is_bot),
                Some(expected_is_bot)
            );
            server.join().expect("server thread");
        }
    }

    #[test]
    fn fetch_message_maps_only_documented_absence_codes_to_not_found() {
        for code in [UNKNOWN_CHANNEL, UNKNOWN_MESSAGE] {
            let (base_url, server) = spawn_provider(vec![(
                "404 Not Found",
                json!({ "code": code, "message": "PRIVATE SENTINEL" }).to_string(),
            )]);
            let message = DiscordClient::new_for_tests(&base_url)
                .fetch_message("111", "222", Some("333"))
                .expect("documented absence");

            assert!(message.is_none());
            server.join().expect("server thread");
        }

        let (base_url, server) = spawn_provider(vec![(
            "404 Not Found",
            json!({ "code": 10_004, "message": "PRIVATE SENTINEL" }).to_string(),
        )]);
        let error = DiscordClient::new_for_tests(&base_url)
            .fetch_message("111", "222", Some("333"))
            .expect_err("other not-found responses are operational failures");

        assert_eq!(delivery_error_code(&error), Some("provider_not_found"));
        assert!(!error.to_string().contains("PRIVATE SENTINEL"));
        server.join().expect("server thread");
    }

    #[test]
    fn fetch_message_retries_a_bounded_provider_rate_limit() {
        let message = json!({
            "id": "222",
            "channel_id": "111",
            "guild_id": "333",
            "content": "hello"
        });
        let (base_url, server) = spawn_provider(vec![
            (
                "429 Too Many Requests",
                json!({ "code": 20_028, "retry_after": 0.01 }).to_string(),
            ),
            ("200 OK", message.to_string()),
        ]);
        let fetched = DiscordClient::new_for_tests(&base_url)
            .fetch_message("111", "222", Some("333"))
            .expect("rate-limited fetch")
            .expect("message exists");

        assert_eq!(fetched.content, "hello");
        let requests = server.join().expect("server thread");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].request_line, requests[1].request_line);
    }

    #[test]
    fn fetch_message_rejects_mismatched_provider_coordinates() {
        for body in [
            json!({ "id": "other", "channel_id": "111", "guild_id": "333" }),
            json!({ "id": "222", "channel_id": "other", "guild_id": "333" }),
            json!({ "id": "222", "channel_id": "111", "guild_id": "other" }),
        ] {
            let (base_url, server) = spawn_provider(vec![("200 OK", body.to_string())]);
            let error = DiscordClient::new_for_tests(&base_url)
                .fetch_message("111", "222", Some("333"))
                .expect_err("mismatched coordinates");

            assert_eq!(
                delivery_error_code(&error),
                Some("provider_invalid_response")
            );
            server.join().expect("server thread");
        }
    }

    #[test]
    fn fetch_message_rejects_malformed_provider_fields() {
        for body in [
            json!({ "id": "222", "channel_id": "111", "guild_id": "333" }),
            json!({ "id": "222", "channel_id": "111", "guild_id": "333", "content": 1 }),
            json!({ "id": "222", "channel_id": "111", "guild_id": 333, "content": "ok" }),
            json!({ "id": "222", "channel_id": "111", "guild_id": "333", "content": "ok", "author": "user" }),
            json!({ "id": "222", "channel_id": "111", "guild_id": "333", "content": "ok", "author": { "id": 444 } }),
            json!({ "id": "222", "channel_id": "111", "guild_id": "333", "content": "ok", "member": "member" }),
            json!({ "id": "222", "channel_id": "111", "guild_id": "333", "content": "ok", "webhook_id": 555 }),
        ] {
            let (base_url, server) = spawn_provider(vec![("200 OK", body.to_string())]);
            let error = DiscordClient::new_for_tests(&base_url)
                .fetch_message("111", "222", Some("333"))
                .expect_err("malformed message fields");

            assert_eq!(
                delivery_error_code(&error),
                Some("provider_invalid_response")
            );
            server.join().expect("server thread");
        }
    }

    #[test]
    fn fetch_message_keeps_provider_failures_distinct_and_redacted() {
        for (status, code, expected) in [
            ("401 Unauthorized", 0, "provider_authentication_failed"),
            ("403 Forbidden", 50_001, "provider_permission_denied"),
            ("500 Internal Server Error", 0, "provider_unavailable"),
        ] {
            let (base_url, server) = spawn_provider(vec![(
                status,
                json!({ "code": code, "message": "PRIVATE SENTINEL" }).to_string(),
            )]);
            let error = DiscordClient::new_for_tests(&base_url)
                .fetch_message("111", "222", Some("333"))
                .expect_err("provider failure");

            assert_eq!(delivery_error_code(&error), Some(expected));
            assert!(!error.to_string().contains("PRIVATE SENTINEL"));
            server.join().expect("server thread");
        }
    }

    #[test]
    fn fetch_message_classifies_malformed_success_responses() {
        let malformed = "PRIVATE SENTINEL {".to_string();
        let (base_url, server) = spawn_provider(vec![("200 OK", malformed)]);
        let error = DiscordClient::new_for_tests(&base_url)
            .fetch_message("111", "222", Some("333"))
            .expect_err("malformed provider response");

        assert_eq!(
            delivery_error_code(&error),
            Some("provider_invalid_response")
        );
        assert!(!error.to_string().contains("PRIVATE SENTINEL"));
        server.join().expect("server thread");
    }

    #[test]
    fn message_content_at_the_provider_limit_stays_in_one_chunk() {
        let content = "a".repeat(MESSAGE_CONTENT_LIMIT);

        let chunks = split_message_content(&content);

        assert_eq!(chunks, vec![content.as_str()]);
    }

    #[test]
    fn message_content_over_the_provider_limit_is_complete_and_bounded() {
        let content = "a".repeat(MESSAGE_CONTENT_LIMIT + 52);

        let chunks = split_message_content(&content);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), content);
        assert!(
            chunks
                .iter()
                .all(|chunk| message_content_units(chunk) <= MESSAGE_CONTENT_LIMIT)
        );
    }

    #[test]
    fn message_chunks_use_unicode_safe_boundaries() {
        let content = format!("{}{}", "🙂".repeat(1_001), "e\u{301}".repeat(20));

        let chunks = split_message_content(&content);

        assert_eq!(chunks.concat(), content);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| message_content_units(chunk) <= MESSAGE_CONTENT_LIMIT)
        );
    }

    #[test]
    fn message_chunks_prefer_paragraph_boundaries() {
        let content = format!("{}\n\n{}", "a".repeat(1_200), "b".repeat(1_200));

        let chunks = split_message_content(&content);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with("\n\n"));
        assert_eq!(chunks.concat(), content);
    }

    #[test]
    fn message_chunks_do_not_stop_at_an_early_boundary() {
        let content = format!("intro\n\n{}", "a".repeat(MESSAGE_CONTENT_LIMIT + 1));

        let chunks = split_message_content(&content);

        assert_eq!(chunks.len(), 2);
        assert_eq!(message_content_units(chunks[0]), MESSAGE_CONTENT_LIMIT);
        assert_eq!(chunks.concat(), content);
    }

    #[test]
    fn message_chunks_balance_fenced_code() {
        let content = format!("```rust\n{}\n```", "a".repeat(MESSAGE_CONTENT_LIMIT + 1));

        let chunks = render_message_chunks(&content);

        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| message_content_units(chunk) <= MESSAGE_CONTENT_LIMIT)
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.match_indices("```").count() % 2 == 0)
        );
        let reconstructed = format!(
            "{}{}",
            chunks[0].strip_suffix("\n```").expect("continuation close"),
            chunks[1].strip_prefix("```\n").expect("continuation open")
        );
        assert_eq!(reconstructed, content);
    }

    #[test]
    fn message_chunks_never_divide_a_fence_delimiter() {
        // The hard boundary lands inside the closing delimiter of this block.
        let content = format!("```\n{}```{}", "a".repeat(1_987), "b".repeat(1_500));

        let chunks = render_message_chunks(&content);

        assert!(
            chunks
                .iter()
                .all(|chunk| message_content_units(chunk) <= MESSAGE_CONTENT_LIMIT)
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.match_indices("```").count() % 2 == 0),
            "a divided delimiter would leave a code block open to the end of the message"
        );
    }

    #[test]
    fn message_chunks_keep_every_backtick_run_intact() {
        let content = format!("{}```{}", "a".repeat(1_999), "b".repeat(1_000));

        let chunks = split_message_content(&content);

        assert_eq!(chunks.concat(), content);
        for pair in chunks.windows(2) {
            assert!(
                !(pair[0].ends_with('`') && pair[1].starts_with('`')),
                "a backtick run was divided across a chunk boundary"
            );
        }
    }

    #[test]
    fn a_run_longer_than_one_chunk_still_advances() {
        let content = "`".repeat(MESSAGE_CONTENT_LIMIT * 2 + 5);

        let chunks = split_message_content(&content);

        assert_eq!(chunks.concat(), content);
        assert!(
            chunks
                .iter()
                .all(|chunk| message_content_units(chunk) <= MESSAGE_CONTENT_LIMIT)
        );
    }

    #[test]
    fn rate_limited_chunks_wait_for_the_provider_and_then_succeed() {
        // The first chunk is rate limited once, then both chunks are accepted.
        let (base_url, server) = spawn_provider(vec![
            (
                "429 Too Many Requests",
                r#"{"message":"PRIVATE SENTINEL","retry_after":0.05,"global":false}"#.to_string(),
            ),
            created_message("msg-1"),
            created_message("msg-2"),
        ]);
        let client = DiscordClient::new_for_tests(&base_url);
        let content = "a".repeat(MESSAGE_CONTENT_LIMIT + 1);

        let messages = client
            .send_message_chunks("chan-1", &content, None, None)
            .expect("rate limited chunk is retried");

        assert_eq!(messages.len(), 2);
        let requests = server.join().expect("server thread");
        assert_eq!(requests.len(), 3, "the limited chunk is sent again");
        let first: Value = serde_json::from_slice(&requests[0].body).expect("first request JSON");
        let retried: Value = serde_json::from_slice(&requests[1].body).expect("retry request JSON");
        assert_eq!(first["content"], retried["content"]);
    }

    #[test]
    fn a_rate_limit_beyond_the_wait_budget_is_not_retried() {
        let error = anyhow!(DiscordApiError {
            http_status: 429,
            code: None,
            field: None,
            detail: None,
            retry_after: Some(Duration::from_secs(120)),
            context: "failed to send Discord message".to_string(),
        });

        assert_eq!(rate_limit_wait(Some(&error), RATE_LIMIT_WAIT_BUDGET), None);
        assert_eq!(delivery_error_code(&error), Some("provider_rate_limited"));
    }

    #[test]
    fn a_rate_limit_without_a_valid_wait_is_not_retried() {
        for retry_after in [
            None,
            parse_retry_after(None, Some(&json!({ "retry_after": 1e300 }))),
        ] {
            let error = anyhow!(DiscordApiError {
                http_status: 429,
                code: None,
                field: None,
                detail: None,
                retry_after,
                context: "failed to send Discord message".to_string(),
            });

            assert_eq!(rate_limit_wait(Some(&error), RATE_LIMIT_WAIT_BUDGET), None);
            assert_eq!(delivery_error_code(&error), Some("provider_rate_limited"));
        }
    }

    #[test]
    fn an_invalid_body_retry_interval_falls_back_to_the_header() {
        assert_eq!(
            parse_retry_after(Some(1.5), Some(&json!({ "retry_after": 1e300 }))),
            Some(Duration::from_millis(1_500))
        );
    }

    #[test]
    fn send_message_rejects_over_limit_content_before_transport() {
        let client = DiscordClient::new_for_tests("http://127.0.0.1:1/api/v10");
        let content = "a".repeat(MESSAGE_CONTENT_LIMIT + 1);

        let error = client
            .send_message("chan-1", &content, None, None)
            .expect_err("over-limit message");

        assert!(error.to_string().starts_with("message_too_long:"));
        assert_eq!(delivery_error_code(&error), Some("message_too_long"));
    }

    #[test]
    fn send_message_rejects_mismatched_provider_coordinates() {
        let (base_url, server) = spawn_provider(vec![(
            "200 OK",
            json!({ "id": "msg-1", "channel_id": "channel-other" }).to_string(),
        )]);
        let error = DiscordClient::new_for_tests(&base_url)
            .send_message("chan-1", "hello", None, None)
            .expect_err("mismatched delivery coordinates");

        assert_eq!(
            delivery_error_code(&error),
            Some("provider_invalid_response")
        );
        server.join().expect("server thread");
    }

    #[test]
    fn trigger_typing_posts_an_empty_request_and_accepts_no_content() {
        let (base_url, server) = spawn_provider(vec![("204 No Content", String::new())]);
        let client = DiscordClient::new_for_tests(&base_url);

        client.trigger_typing("chan-1").expect("trigger typing");

        let requests = server.join().expect("server thread");
        let request = &requests[0];
        assert_eq!(
            request.request_line,
            "POST /api/v10/channels/chan-1/typing HTTP/1.1"
        );
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bot test-token")
        );
        assert!(request.body.is_empty());
    }

    #[test]
    fn trigger_typing_refuses_a_success_other_than_no_content() {
        let (base_url, server) = spawn_provider(vec![("200 OK", r#"{"ok":true}"#.to_string())]);

        let error = DiscordClient::new_for_tests(&base_url)
            .trigger_typing("chan-1")
            .expect_err("only 204 proves the request took effect");

        assert_eq!(
            delivery_error_code(&error),
            Some("provider_invalid_response")
        );
        assert!(!error.to_string().contains("chan-1"));
        server.join().expect("server thread");
    }

    #[test]
    fn trigger_typing_keeps_provider_failures_redacted_and_classified() {
        let (base_url, server) = spawn_provider(vec![(
            "403 Forbidden",
            r#"{"message":"PRIVATE SENTINEL","code":50013}"#.to_string(),
        )]);
        let client = DiscordClient::new_for_tests(&base_url);

        let error = client.trigger_typing("chan-1").expect_err("typing denied");

        assert_eq!(
            delivery_error_code(&error),
            Some("provider_permission_denied")
        );
        assert!(!error.to_string().contains("PRIVATE SENTINEL"));
        server.join().expect("server thread");
    }

    #[test]
    fn trigger_typing_classifies_rate_limits_and_transport_failures() {
        let (base_url, server) = spawn_provider(vec![(
            "429 Too Many Requests",
            r#"{"message":"PRIVATE SENTINEL","retry_after":1.0}"#.to_string(),
        )]);
        let rate_limited = DiscordClient::new_for_tests(&base_url)
            .trigger_typing("chan-1")
            .expect_err("typing rate limited");
        assert_eq!(
            delivery_error_code(&rate_limited),
            Some("provider_rate_limited")
        );
        assert!(!rate_limited.to_string().contains("PRIVATE SENTINEL"));
        server.join().expect("server thread");

        let transport = DiscordClient::new_for_tests("http://127.0.0.1:1/api/v10")
            .trigger_typing("chan-1")
            .expect_err("typing transport failure");
        assert_eq!(
            delivery_error_code(&transport),
            Some("provider_transport_error")
        );
        assert!(!transport.to_string().contains("chan-1"));
    }

    #[test]
    fn send_message_chunks_preserves_order_and_replies_only_once() {
        let (base_url, server) =
            spawn_provider(vec![created_message("msg-1"), created_message("msg-2")]);
        let client = DiscordClient::new_for_tests(&base_url);
        let content = "a".repeat(MESSAGE_CONTENT_LIMIT + 1);
        let messages = client
            .send_message_chunks("chan-1", &content, Some("parent-1"), None)
            .expect("send chunks");

        assert_eq!(
            messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["msg-1", "msg-2"]
        );
        let requests = server.join().expect("server thread");
        let first: Value = serde_json::from_slice(&requests[0].body).expect("first request JSON");
        let second: Value = serde_json::from_slice(&requests[1].body).expect("second request JSON");
        assert_eq!(
            first["content"].as_str().map(message_content_units),
            Some(MESSAGE_CONTENT_LIMIT)
        );
        assert_eq!(first["message_reference"]["message_id"], "parent-1");
        assert_eq!(first["enforce_nonce"], true);
        assert_eq!(first["nonce"].as_str().map(str::len), Some(25));
        assert_eq!(second["content"], "a");
        assert!(second.get("message_reference").is_none());
        assert_eq!(second["enforce_nonce"], true);
        assert_eq!(second["nonce"].as_str().map(str::len), Some(25));
        assert_ne!(first["nonce"], second["nonce"]);
    }

    #[test]
    fn send_message_chunks_uploads_an_attachment_only_once() {
        let (base_url, server) =
            spawn_provider(vec![created_message("msg-1"), created_message("msg-2")]);
        let client = DiscordClient::new_for_tests(&base_url);
        let content = "a".repeat(MESSAGE_CONTENT_LIMIT + 1);
        let upload = DiscordUpload {
            name: "report.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: b"report".to_vec(),
        };
        client
            .send_message_chunks("chan-1", &content, Some("parent-1"), Some(&upload))
            .expect("send chunks");

        let requests = server.join().expect("server thread");
        assert!(
            requests[0]
                .headers
                .get("content-type")
                .is_some_and(|value| value.starts_with("multipart/form-data; boundary="))
        );
        let first_body = String::from_utf8_lossy(&requests[0].body);
        assert!(first_body.contains("report.txt"));
        assert!(first_body.contains("parent-1"));
        assert!(first_body.contains("enforce_nonce"));

        assert_eq!(
            requests[1].headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        let second: Value = serde_json::from_slice(&requests[1].body).expect("second request JSON");
        assert!(second.get("message_reference").is_none());
        assert!(second.get("attachments").is_none());
    }

    #[test]
    fn chunk_failure_reports_progress_and_redacted_provider_classification() {
        let (base_url, server) = spawn_provider(vec![
            created_message("msg-1"),
            (
                "400 Bad Request",
                r#"{"message":"PRIVATE SENTINEL","code":50035,"errors":{"content":{"_errors":[{"code":"BASE_TYPE_MAX_LENGTH","message":"PRIVATE SENTINEL"}]}}}"#.to_string(),
            ),
        ]);
        let client = DiscordClient::new_for_tests(&base_url);
        let content = "a".repeat(MESSAGE_CONTENT_LIMIT + 1);
        let error = client
            .send_message_chunks("chan-1", &content, None, None)
            .expect_err("second chunk fails");

        let partial = error
            .downcast_ref::<DiscordChunkDeliveryError>()
            .expect("partial delivery error");
        assert_eq!(partial.failed_chunk, 2);
        assert_eq!(partial.chunk_count, 2);
        assert_eq!(partial.completed_chunks, 1);
        assert_eq!(delivery_error_code(&error), Some("partial_delivery"));
        let message = error.to_string();
        assert!(message.contains("http_status=400"));
        assert!(message.contains("code=50035"));
        assert!(message.contains("field=content"));
        assert!(message.contains("detail=BASE_TYPE_MAX_LENGTH"));
        assert!(!message.contains("PRIVATE SENTINEL"));
        server.join().expect("server thread");
    }

    #[test]
    fn provider_statuses_have_stable_delivery_error_codes() {
        for (status, expected) in [
            (400, "provider_invalid_request"),
            (401, "provider_authentication_failed"),
            (403, "provider_permission_denied"),
            (404, "provider_not_found"),
            (429, "provider_rate_limited"),
            (500, "provider_unavailable"),
        ] {
            let error = anyhow!(DiscordApiError {
                http_status: status,
                code: None,
                field: None,
                detail: None,
                retry_after: None,
                context: "failed to send Discord message".to_string(),
            });
            assert_eq!(delivery_error_code(&error), Some(expected));
        }
    }

    #[test]
    fn send_message_posts_multipart_attachment_payload() {
        let (base_url, server) = spawn_provider(vec![created_message("msg-1")]);
        let client = DiscordClient::new_for_tests(&base_url);
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

        let requests = server.join().expect("server thread");
        let request = &requests[0];
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

    /// Answer one canned response per expected request and return what arrived.
    fn spawn_provider(
        responses: Vec<(&'static str, String)>,
    ) -> (String, thread::JoinHandle<Vec<CapturedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener addr");
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept connection");
                requests.push(read_request(&mut stream));
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write response");
                stream.flush().expect("flush response");
            }
            requests
        });
        (format!("http://{address}/api/v10"), handle)
    }

    fn created_message(id: &str) -> (&'static str, String) {
        (
            "200 OK",
            format!(r#"{{"id":"{id}","channel_id":"chan-1"}}"#),
        )
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
