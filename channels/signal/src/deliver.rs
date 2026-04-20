//! Outbound delivery for the native presage-backed Signal plugin.
//!
//! Sends are implemented by spinning a short-lived dedicated OS
//! thread that owns its own current-thread tokio runtime and its own
//! Manager loaded from the shared SQLite store. This mirrors the
//! receive worker's pattern and satisfies presage's `!Send` store
//! constraint (libsignal stores hold `Rc<UnsafeCell<_>>` internally
//! and cannot cross work-stealing tokio workers).
//!
//! For v0.1.0 we support plain text outbound only. The recipient is
//! resolved from `message.metadata.conversation_id` first, then
//! `config.default_recipient`, and must be a Signal ServiceId string
//! (either a raw ACI UUID or a `PNI:<uuid>` / `ACI:<uuid>` form).
//! Attachments, groups, and usernames will land in follow-up commits.

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use jiff::Timestamp;
use presage::Manager;
use presage::libsignal_service::content::DataMessage;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage_store_sqlite::SqliteStore;
use std::collections::BTreeMap;
use tokio::runtime::Builder;

use crate::protocol::{ChannelConfig, DeliveryReceipt, OutboundAttachment, OutboundMessage};
use crate::store::{resolve_passphrase, resolve_store_path, to_sqlite_url};

const PLATFORM_SIGNAL: &str = "signal";
const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const SIGNAL_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Send a Signal text message (with optional attachments) to the
/// configured recipient. Blocks the calling thread until the send
/// completes or fails.
///
/// For v0.1.0 only `data_base64` attachments are supported. Attachment
/// URLs and storage-key references return an error - dispatch stages
/// media locally and the caller should inline it as base64.
pub fn deliver_text_message(
    config: &ChannelConfig,
    message: &OutboundMessage,
) -> Result<DeliveryReceipt> {
    let recipient_raw = resolve_recipient_raw(config, message)?;
    let recipient = parse_service_id(&recipient_raw)?;
    let content = message.content.trim().to_string();

    let prepared_attachments = prepare_outbound_attachments(&message.attachments)?;
    if content.is_empty() && prepared_attachments.is_empty() {
        return Err(anyhow!(
            "channel-signal requires a non-empty `message.content` or at least one attachment for outbound delivery"
        ));
    }

    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Err(anyhow!(
            "Signal session has not been linked yet; run `channel-signal --link` first (store path: {})",
            store_path.display()
        ));
    }
    let url = to_sqlite_url(&store_path);
    let passphrase = resolve_passphrase(config)?;
    let attachment_count = prepared_attachments.len();

    let handle = std::thread::Builder::new()
        .name("channel-signal-deliver".to_string())
        .stack_size(SIGNAL_THREAD_STACK_SIZE)
        .spawn(move || -> Result<u64> {
            let runtime = Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build send-thread tokio runtime")?;
            runtime.block_on(send_message_inner(
                url,
                passphrase,
                recipient,
                content,
                prepared_attachments,
            ))
        })
        .context("failed to spawn channel-signal send thread")?;

    let timestamp_ms = handle
        .join()
        .map_err(|_| anyhow!("channel-signal send thread panicked"))??;

    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), PLATFORM_SIGNAL.to_string());
    metadata.insert("transport".to_string(), "websocket".to_string());
    metadata.insert("signal_timestamp_ms".to_string(), timestamp_ms.to_string());
    metadata.insert("attachment_count".to_string(), attachment_count.to_string());
    Ok(DeliveryReceipt {
        message_id: timestamp_ms.to_string(),
        conversation_id: recipient_raw,
        metadata,
    })
}

/// Decode base64-inlined attachment payloads and pair each with an
/// `AttachmentSpec` that carries the Signal-side metadata presage
/// needs when uploading.
fn prepare_outbound_attachments(
    attachments: &[OutboundAttachment],
) -> Result<Vec<(AttachmentSpec, Vec<u8>)>> {
    attachments
        .iter()
        .map(|attachment| {
            let data_base64 = attachment.data_base64.as_deref().ok_or_else(|| {
                anyhow!(
                    "channel-signal attachment `{}` must set `data_base64`; URL and storage-key attachments are not supported yet",
                    attachment.name
                )
            })?;
            let contents = BASE64_STANDARD.decode(data_base64).with_context(|| {
                format!("channel-signal attachment `{}` is not valid base64", attachment.name)
            })?;
            let spec = AttachmentSpec {
                content_type: attachment.mime_type.clone(),
                length: contents.len(),
                file_name: Some(attachment.name.clone()),
                preview: None,
                voice_note: None,
                borderless: None,
                width: None,
                height: None,
                caption: None,
                blur_hash: None,
            };
            Ok((spec, contents))
        })
        .collect()
}

async fn send_message_inner(
    url: String,
    passphrase: Option<String>,
    recipient: ServiceId,
    content: String,
    attachments: Vec<(AttachmentSpec, Vec<u8>)>,
) -> Result<u64> {
    let store =
        SqliteStore::open_with_passphrase(&url, passphrase.as_deref(), OnNewIdentity::Trust)
            .await
            .with_context(|| format!("failed to open Signal session store at `{url}`"))?;
    let mut manager = Manager::<SqliteStore, Registered>::load_registered(store)
        .await
        .map_err(|error| anyhow!("failed to load Signal session: {error}"))?;

    let uploaded = if attachments.is_empty() {
        Vec::new()
    } else {
        manager
            .upload_attachments(attachments)
            .await
            .map_err(|error| anyhow!("failed to upload Signal attachments: {error}"))?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| anyhow!("attachment upload rejected by Signal: {error}"))?
    };

    let timestamp_ms = Timestamp::now().as_millisecond() as u64;
    let data_message = DataMessage {
        body: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        timestamp: Some(timestamp_ms),
        attachments: uploaded,
        ..Default::default()
    };

    manager
        .send_message(recipient, data_message, timestamp_ms)
        .await
        .map_err(|error| anyhow!("failed to send Signal message: {error}"))?;

    Ok(timestamp_ms)
}

fn resolve_recipient_raw(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
    if let Some(value) = message
        .metadata
        .get(ROUTE_CONVERSATION_ID)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(value.to_string());
    }
    config
        .default_recipient
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "channel-signal outbound delivery requires `message.metadata.conversation_id` or `config.default_recipient`"
            )
        })
}

fn parse_service_id(raw: &str) -> Result<ServiceId> {
    // Signal's wire format: bare UUID -> ACI, `PNI:<uuid>` -> PNI.
    if let Some(service_id) = ServiceId::parse_from_service_id_string(raw) {
        return Ok(service_id);
    }
    // Accept an explicit `ACI:<uuid>` operator-facing form for clarity,
    // even though Signal's own parser treats bare UUIDs as ACI.
    if let Some(rest) = raw.strip_prefix("ACI:")
        && let Ok(uuid) = Uuid::parse_str(rest)
    {
        return Ok(Aci::from(uuid).into());
    }
    let uuid = Uuid::parse_str(raw).with_context(|| {
        format!(
            "channel-signal recipient `{raw}` is neither a bare UUID (ACI), an `ACI:<uuid>` or `PNI:<uuid>` ServiceId, nor a linked profile"
        )
    })?;
    Ok(Aci::from(uuid).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_with_metadata(content: &str, entries: &[(&str, &str)]) -> OutboundMessage {
        let mut metadata = BTreeMap::new();
        for (k, v) in entries {
            metadata.insert((*k).to_string(), (*v).to_string());
        }
        OutboundMessage {
            content: content.to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: Vec::new(),
            metadata,
        }
    }

    #[test]
    fn resolve_recipient_raw_prefers_metadata_over_default() {
        let config = ChannelConfig {
            default_recipient: Some("ACI:00000000-0000-0000-0000-000000000001".to_string()),
            ..ChannelConfig::default()
        };
        let message = message_with_metadata(
            "hi",
            &[(
                "conversation_id",
                "ACI:00000000-0000-0000-0000-000000000099",
            )],
        );
        assert_eq!(
            resolve_recipient_raw(&config, &message).unwrap(),
            "ACI:00000000-0000-0000-0000-000000000099"
        );
    }

    #[test]
    fn resolve_recipient_raw_falls_back_to_config_default() {
        let config = ChannelConfig {
            default_recipient: Some("ACI:00000000-0000-0000-0000-000000000001".to_string()),
            ..ChannelConfig::default()
        };
        let message = message_with_metadata("hi", &[]);
        assert_eq!(
            resolve_recipient_raw(&config, &message).unwrap(),
            "ACI:00000000-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn resolve_recipient_raw_errors_without_either() {
        let config = ChannelConfig::default();
        let message = message_with_metadata("hi", &[]);
        let error = resolve_recipient_raw(&config, &message).unwrap_err();
        assert!(error.to_string().contains("conversation_id"));
    }

    #[test]
    fn parse_service_id_accepts_bare_uuid_as_aci() {
        let raw = "00000000-0000-0000-0000-000000000001";
        let parsed = parse_service_id(raw).unwrap();
        assert_eq!(
            parsed.service_id_string(),
            "00000000-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn parse_service_id_accepts_explicit_aci_prefix() {
        let raw = "ACI:00000000-0000-0000-0000-000000000042";
        let parsed = parse_service_id(raw).unwrap();
        // Signal's wire form for ACI is the bare UUID; the explicit
        // `ACI:` prefix is our operator-facing convenience.
        assert_eq!(
            parsed.service_id_string(),
            "00000000-0000-0000-0000-000000000042"
        );
    }

    #[test]
    fn parse_service_id_accepts_pni_prefix() {
        let raw = "PNI:00000000-0000-0000-0000-000000000042";
        let parsed = parse_service_id(raw).unwrap();
        assert_eq!(parsed.service_id_string(), raw);
    }

    #[test]
    fn parse_service_id_rejects_phone_numbers() {
        let error = parse_service_id("+15551234567").unwrap_err();
        assert!(error.to_string().contains("neither"));
    }
}
