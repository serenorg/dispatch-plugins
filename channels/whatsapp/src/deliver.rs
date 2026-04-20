use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use std::collections::BTreeMap;
use std::time::Duration;
use whatsapp_rust::Jid;
use whatsapp_rust::TokioRuntime;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::download::MediaType;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::upload::UploadResponse;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::protocol::{ChannelConfig, DeliveryReceipt, OutboundAttachment, OutboundMessage};
use crate::session::{SessionState, load_session};
use crate::store::{resolve_store_path, to_sqlite_url};

const PLATFORM_WHATSAPP: &str = "whatsapp";
const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const POST_SEND_FLUSH_DELAY: Duration = Duration::from_millis(1500);

#[derive(Debug)]
struct PreparedSend {
    content: String,
    attachment: Option<PreparedAttachment>,
}

#[derive(Debug)]
struct PreparedAttachment {
    name: String,
    mime_type: String,
    kind: AttachmentKind,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachmentKind {
    Image,
    Video,
    Audio,
    Document,
}

impl AttachmentKind {
    fn from_mime(mime_type: &str) -> Self {
        if mime_type.starts_with("image/") {
            Self::Image
        } else if mime_type.starts_with("video/") {
            Self::Video
        } else if mime_type.starts_with("audio/") {
            Self::Audio
        } else {
            Self::Document
        }
    }

    fn media_type(self) -> MediaType {
        match self {
            Self::Image => MediaType::Image,
            Self::Video => MediaType::Video,
            Self::Audio => MediaType::Audio,
            Self::Document => MediaType::Document,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
        }
    }

    fn supports_caption(self) -> bool {
        !matches!(self, Self::Audio)
    }
}

pub fn deliver_text_message(
    config: &ChannelConfig,
    message: &OutboundMessage,
) -> Result<DeliveryReceipt> {
    let recipient_raw = resolve_recipient_raw(config, message)?;
    let recipient = parse_recipient_jid(&recipient_raw)?;
    let prepared = prepare_outbound_payload(message)?;

    match load_session(config)? {
        SessionState::Registered { .. } => {}
        SessionState::NotYetLinked { store_path } | SessionState::StoreEmpty { store_path } => {
            return Err(anyhow!(
                "WhatsApp session has not been linked yet; run `channel-whatsapp --link` first (store path: {})",
                store_path.display()
            ));
        }
    }

    let store_path = resolve_store_path(config)?;
    let sqlite_url = to_sqlite_url(&store_path);
    let attachment_count = usize::from(prepared.attachment.is_some());
    let attachment_kind = prepared
        .attachment
        .as_ref()
        .map(|attachment| attachment.kind.label().to_string());
    let handle = std::thread::Builder::new()
        .name("channel-whatsapp-deliver".to_string())
        .spawn(move || -> Result<String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build send-thread tokio runtime")?;
            runtime.block_on(send_message_inner(sqlite_url, recipient, prepared))
        })
        .context("failed to spawn channel-whatsapp send thread")?;

    let sent = handle
        .join()
        .map_err(|_| anyhow!("channel-whatsapp send thread panicked"))??;

    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), PLATFORM_WHATSAPP.to_string());
    metadata.insert("transport".to_string(), "websocket".to_string());
    metadata.insert("attachment_count".to_string(), attachment_count.to_string());
    if let Some(kind) = attachment_kind {
        metadata.insert("attachment_kind".to_string(), kind);
    }

    Ok(DeliveryReceipt {
        message_id: sent,
        conversation_id: recipient_raw,
        metadata,
    })
}

fn prepare_outbound_payload(message: &OutboundMessage) -> Result<PreparedSend> {
    let content = message.content.trim().to_string();
    let attachment = match message.attachments.as_slice() {
        [] => None,
        [attachment] => Some(prepare_outbound_attachment(attachment)?),
        _ => {
            return Err(anyhow!(
                "channel-whatsapp v0.1.0 supports at most one outbound attachment per message"
            ));
        }
    };

    if content.is_empty() && attachment.is_none() {
        return Err(anyhow!(
            "channel-whatsapp outbound delivery requires a non-empty `message.content` or one `data_base64` attachment"
        ));
    }

    if let Some(attachment) = &attachment
        && !content.is_empty()
        && !attachment.kind.supports_caption()
    {
        return Err(anyhow!(
            "channel-whatsapp audio attachments cannot include `message.content` in v0.1.0; send the text separately or send the file as a document attachment"
        ));
    }

    Ok(PreparedSend {
        content,
        attachment,
    })
}

fn prepare_outbound_attachment(attachment: &OutboundAttachment) -> Result<PreparedAttachment> {
    if attachment.url.is_some() || attachment.storage_key.is_some() {
        return Err(anyhow!(
            "channel-whatsapp attachment `{}` must inline `data_base64`; URL and storage-key attachments are not supported in v0.1.0",
            attachment.name
        ));
    }

    let data_base64 = attachment.data_base64.as_deref().ok_or_else(|| {
        anyhow!(
            "channel-whatsapp attachment `{}` must set `data_base64`",
            attachment.name
        )
    })?;
    let bytes = BASE64_STANDARD.decode(data_base64).with_context(|| {
        format!(
            "channel-whatsapp attachment `{}` is not valid base64",
            attachment.name
        )
    })?;

    Ok(PreparedAttachment {
        name: attachment.name.clone(),
        mime_type: attachment.mime_type.clone(),
        kind: AttachmentKind::from_mime(&attachment.mime_type),
        bytes,
    })
}

async fn send_message_inner(
    sqlite_url: String,
    recipient: Jid,
    prepared: PreparedSend,
) -> Result<String> {
    let backend = std::sync::Arc::new(
        SqliteStore::new(&sqlite_url)
            .await
            .with_context(|| format!("failed to open WhatsApp session store at `{sqlite_url}`"))?,
    );

    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .build()
        .await
        .context("failed to build WhatsApp bot for outbound delivery")?;

    let client = bot.client();
    let bot_handle = bot.run().await.context("failed to start WhatsApp bot")?;
    client
        .wait_for_socket(CONNECT_TIMEOUT)
        .await
        .context("timed out waiting for linked WhatsApp session socket")?;
    let recipient = resolve_chat_recipient(&client, &recipient).await?;

    let outbound = match prepared.attachment {
        Some(attachment) => {
            let PreparedAttachment {
                name,
                mime_type,
                kind,
                bytes,
            } = attachment;
            let upload = client
                .upload(bytes, kind.media_type())
                .await
                .context("failed to upload WhatsApp attachment")?;
            build_attachment_message(name, mime_type, kind, upload, &prepared.content)?
        }
        None => wa::Message {
            conversation: Some(prepared.content),
            ..Default::default()
        },
    };

    let message_id = client
        .send_message(recipient, outbound)
        .await
        .context("failed to send WhatsApp message")?;

    tokio::time::sleep(POST_SEND_FLUSH_DELAY).await;
    client.disconnect().await;
    let _ = bot_handle.await;
    Ok(message_id)
}

async fn resolve_chat_recipient(client: &whatsapp_rust::Client, recipient: &Jid) -> Result<Jid> {
    if recipient.is_lid()
        && let Some(phone_number) = client
            .get_phone_number_from_lid(&recipient.to_string())
            .await
    {
        return format!("{phone_number}@s.whatsapp.net")
            .parse::<Jid>()
            .with_context(|| {
                format!(
                    "failed to convert mapped WhatsApp phone number `{phone_number}` into a chat JID"
                )
            });
    }

    Ok(recipient.clone())
}

fn build_attachment_message(
    name: String,
    mime_type: String,
    kind: AttachmentKind,
    upload: UploadResponse,
    content: &str,
) -> Result<wa::Message> {
    let caption = (!content.is_empty()).then(|| content.to_string());

    Ok(match kind {
        AttachmentKind::Image => wa::Message {
            image_message: Some(Box::new(wa::message::ImageMessage {
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_sha256: Some(upload.file_sha256),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_length: Some(upload.file_length),
                mimetype: Some(mime_type),
                caption,
                ..Default::default()
            })),
            ..Default::default()
        },
        AttachmentKind::Video => wa::Message {
            video_message: Some(Box::new(wa::message::VideoMessage {
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_sha256: Some(upload.file_sha256),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_length: Some(upload.file_length),
                mimetype: Some(mime_type),
                caption,
                ..Default::default()
            })),
            ..Default::default()
        },
        AttachmentKind::Audio => wa::Message {
            audio_message: Some(Box::new(wa::message::AudioMessage {
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_sha256: Some(upload.file_sha256),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_length: Some(upload.file_length),
                mimetype: Some(mime_type),
                ptt: Some(false),
                ..Default::default()
            })),
            ..Default::default()
        },
        AttachmentKind::Document => wa::Message {
            document_message: Some(Box::new(wa::message::DocumentMessage {
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_sha256: Some(upload.file_sha256),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_length: Some(upload.file_length),
                mimetype: Some(mime_type),
                title: Some(name.clone()),
                file_name: Some(name),
                caption,
                ..Default::default()
            })),
            ..Default::default()
        },
    })
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
                "channel-whatsapp outbound delivery requires `message.metadata.conversation_id` or `config.default_recipient`"
            )
        })
}

fn parse_recipient_jid(raw: &str) -> Result<Jid> {
    if !raw.contains('@') {
        return Err(anyhow!(
            "channel-whatsapp recipient `{raw}` is not a WhatsApp JID; use a full JID like `15551234567@s.whatsapp.net`"
        ));
    }

    if !raw.ends_with("@s.whatsapp.net") && !raw.ends_with("@lid") {
        return Err(anyhow!(
            "channel-whatsapp v0.1.0 only supports direct-message JIDs ending in `@s.whatsapp.net` or `@lid`; got `{raw}`"
        ));
    }

    raw.parse::<Jid>()
        .with_context(|| format!("failed to parse WhatsApp recipient JID `{raw}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbound_message() -> OutboundMessage {
        OutboundMessage {
            content: "hello".to_string(),
            content_type: None,
            attachments: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    fn outbound_attachment(name: &str, mime_type: &str, data_base64: &str) -> OutboundAttachment {
        OutboundAttachment {
            name: name.to_string(),
            mime_type: mime_type.to_string(),
            data_base64: Some(data_base64.to_string()),
            url: None,
            storage_key: None,
        }
    }

    #[test]
    fn resolve_recipient_prefers_message_metadata() {
        let mut message = outbound_message();
        message.metadata.insert(
            ROUTE_CONVERSATION_ID.to_string(),
            "15551234567@s.whatsapp.net".to_string(),
        );

        let config = ChannelConfig {
            default_recipient: Some("fallback@s.whatsapp.net".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_recipient_raw(&config, &message).unwrap(),
            "15551234567@s.whatsapp.net"
        );
    }

    #[test]
    fn resolve_recipient_falls_back_to_config() {
        let config = ChannelConfig {
            default_recipient: Some("15551234567@s.whatsapp.net".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_recipient_raw(&config, &outbound_message()).unwrap(),
            "15551234567@s.whatsapp.net"
        );
    }

    #[test]
    fn parse_recipient_accepts_direct_message_jid() {
        let jid = parse_recipient_jid("15551234567@s.whatsapp.net").unwrap();
        assert_eq!(jid.to_string(), "15551234567@s.whatsapp.net");
    }

    #[test]
    fn parse_recipient_rejects_bare_phone_number() {
        let error = parse_recipient_jid("15551234567").unwrap_err();
        assert!(error.to_string().contains("is not a WhatsApp JID"));
    }

    #[test]
    fn parse_recipient_rejects_group_jid_for_v0_1_0() {
        let error = parse_recipient_jid("120363161500776365@g.us").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only supports direct-message JIDs")
        );
    }

    #[test]
    fn prepare_outbound_payload_accepts_single_image_attachment() {
        let mut message = outbound_message();
        message.content = "caption".to_string();
        message
            .attachments
            .push(outbound_attachment("photo.png", "image/png", "aGVsbG8="));

        let prepared = prepare_outbound_payload(&message).unwrap();
        let attachment = prepared.attachment.expect("attachment");
        assert_eq!(prepared.content, "caption");
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.bytes, b"hello");
    }

    #[test]
    fn prepare_outbound_payload_accepts_attachment_without_text() {
        let mut message = outbound_message();
        message.content.clear();
        message.attachments.push(outbound_attachment(
            "report.pdf",
            "application/pdf",
            "aGVsbG8=",
        ));

        let prepared = prepare_outbound_payload(&message).unwrap();
        let attachment = prepared.attachment.expect("attachment");
        assert!(prepared.content.is_empty());
        assert_eq!(attachment.kind, AttachmentKind::Document);
    }

    #[test]
    fn prepare_outbound_payload_rejects_multiple_attachments() {
        let mut message = outbound_message();
        message.content.clear();
        message
            .attachments
            .push(outbound_attachment("one.png", "image/png", "aGVsbG8="));
        message
            .attachments
            .push(outbound_attachment("two.png", "image/png", "aGVsbG8="));

        let error = prepare_outbound_payload(&message).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("at most one outbound attachment")
        );
    }

    #[test]
    fn prepare_outbound_payload_rejects_audio_with_text() {
        let mut message = outbound_message();
        message
            .attachments
            .push(outbound_attachment("clip.ogg", "audio/ogg", "aGVsbG8="));

        let error = prepare_outbound_payload(&message).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("audio attachments cannot include")
        );
    }

    #[test]
    fn prepare_outbound_attachment_requires_data_base64() {
        let attachment = OutboundAttachment {
            name: "photo.png".to_string(),
            mime_type: "image/png".to_string(),
            data_base64: None,
            url: None,
            storage_key: None,
        };

        let error = prepare_outbound_attachment(&attachment).unwrap_err();
        assert!(error.to_string().contains("must set `data_base64`"));
    }

    #[test]
    fn prepare_outbound_attachment_rejects_non_inline_sources() {
        let attachment = OutboundAttachment {
            name: "photo.png".to_string(),
            mime_type: "image/png".to_string(),
            data_base64: Some("aGVsbG8=".to_string()),
            url: Some("https://example.com/photo.png".to_string()),
            storage_key: None,
        };

        let error = prepare_outbound_attachment(&attachment).unwrap_err();
        assert!(error.to_string().contains("must inline `data_base64`"));
    }
}
