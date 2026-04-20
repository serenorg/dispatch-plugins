use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use whatsapp_rust::TokioRuntime;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::proto_helpers::MessageExt;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::types::events::Event;
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::protocol::{
    ChannelConfig, InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope,
    InboundMessage,
};
use crate::store::{resolve_store_path, to_sqlite_url};

const PLATFORM_WHATSAPP: &str = "whatsapp";
const TRANSPORT_WEBSOCKET: &str = "websocket";
const STOP_JOIN_GRACE: Duration = Duration::from_secs(3);
const STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_POLL_TIMEOUT_SECS: u16 = 10;
const POLL_IDLE_GRACE: Duration = Duration::from_millis(1500);

pub struct IngressWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    event_rx: Receiver<InboundEventEnvelope>,
}

impl IngressWorker {
    pub fn drain_pending_events(&self) -> Vec<InboundEventEnvelope> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + STOP_JOIN_GRACE;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

pub fn start_ingress_worker(config: &ChannelConfig) -> Result<IngressWorker> {
    let (event_tx, event_rx) = channel::<InboundEventEnvelope>();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_receive_thread(
        config,
        event_tx,
        Arc::clone(&stop),
        "channel-whatsapp-receive",
    )
    .context("failed to spawn channel-whatsapp receive thread")?;

    Ok(IngressWorker {
        stop,
        handle: Some(handle),
        event_rx,
    })
}

pub fn poll_ingress_once(config: &ChannelConfig) -> Result<Vec<InboundEventEnvelope>> {
    let timeout = Duration::from_secs(u64::from(
        config
            .poll_timeout_secs
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_POLL_TIMEOUT_SECS),
    ));

    let (event_tx, event_rx) = channel::<InboundEventEnvelope>();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_receive_thread(config, event_tx, Arc::clone(&stop), "channel-whatsapp-poll")
        .context("failed to spawn channel-whatsapp poll thread")?;

    let started_at = Instant::now();
    let deadline = started_at + timeout;
    let mut events = Vec::new();

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        let wait_for = if events.is_empty() {
            deadline.saturating_duration_since(now)
        } else {
            std::cmp::min(POLL_IDLE_GRACE, deadline.saturating_duration_since(now))
        };

        match event_rx.recv_timeout(wait_for) {
            Ok(event) => events.push(event),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !events.is_empty() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    stop.store(true, Ordering::Relaxed);
    join_receive_thread(handle);
    Ok(events)
}

fn spawn_receive_thread(
    config: &ChannelConfig,
    event_tx: Sender<InboundEventEnvelope>,
    stop_flag: Arc<AtomicBool>,
    thread_name: &str,
) -> Result<JoinHandle<()>> {
    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Err(anyhow!(
            "WhatsApp session has not been linked yet; run `channel-whatsapp --link` first (store path: {})",
            store_path.display()
        ));
    }
    let sqlite_url = to_sqlite_url(&store_path);
    let thread_name = thread_name.to_string();

    std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("{thread_name} failed to build tokio runtime: {error}");
                    return;
                }
            };

            if let Err(error) = runtime.block_on(run_receive_loop(sqlite_url, event_tx, stop_flag))
            {
                eprintln!("{thread_name} terminated: {error}");
            }
        })
        .context("failed to spawn channel-whatsapp background thread")
}

fn join_receive_thread(handle: JoinHandle<()>) {
    let deadline = Instant::now() + STOP_JOIN_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if handle.is_finished() {
        let _ = handle.join();
    }
}

async fn run_receive_loop(
    sqlite_url: String,
    event_tx: Sender<InboundEventEnvelope>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let backend = Arc::new(
        SqliteStore::new(&sqlite_url)
            .await
            .with_context(|| format!("failed to open WhatsApp session store at `{sqlite_url}`"))?,
    );
    let sender = event_tx.clone();

    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .on_event(move |event, _client| {
            let sender = sender.clone();
            async move {
                if let Event::Message(message, info) = event
                    && let Some(inbound) = build_inbound_event_from_message(&message, &info)
                {
                    let _ = sender.send(inbound);
                }
            }
        })
        .build()
        .await
        .context("failed to build WhatsApp ingress bot")?;

    let client = bot.client();
    let bot_handle = bot.run().await.context("failed to start WhatsApp bot")?;
    tokio::pin!(bot_handle);

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            client.disconnect().await;
            break;
        }

        tokio::select! {
            result = &mut bot_handle => {
                result.map_err(|_| anyhow!("WhatsApp ingress task ended unexpectedly"))?;
                break;
            }
            _ = tokio::time::sleep(STOP_POLL_INTERVAL) => {}
        }
    }

    let _ = bot_handle.await;
    Ok(())
}

fn build_inbound_event_from_message(
    message: &wa::Message,
    info: &MessageInfo,
) -> Option<InboundEventEnvelope> {
    if info.source.is_from_me {
        return None;
    }

    let attachments = build_inbound_attachments(message);
    let content = message
        .text_content()
        .or_else(|| message.get_caption())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if content.is_none() && attachments.is_empty() {
        return None;
    }

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert("platform".to_string(), PLATFORM_WHATSAPP.to_string());
    event_metadata.insert("transport".to_string(), TRANSPORT_WEBSOCKET.to_string());
    event_metadata.insert("message_type".to_string(), info.r#type.clone());
    if !info.media_type.trim().is_empty() {
        event_metadata.insert("media_type".to_string(), info.media_type.clone());
    }

    let mut message_metadata = BTreeMap::new();
    message_metadata.insert(
        "attachment_count".to_string(),
        attachments.len().to_string(),
    );

    Some(InboundEventEnvelope {
        event_id: info.id.to_string(),
        platform: PLATFORM_WHATSAPP.to_string(),
        event_type: "message.received".to_string(),
        received_at: info.timestamp.to_rfc3339(),
        conversation: InboundConversationRef {
            id: info.source.chat.to_string(),
            kind: if info.source.is_group {
                "group_chat".to_string()
            } else {
                "chat".to_string()
            },
            thread_id: None,
            parent_message_id: None,
        },
        actor: InboundActor {
            id: info.source.sender.to_string(),
            display_name: (!info.push_name.trim().is_empty()).then(|| info.push_name.clone()),
            username: None,
            is_bot: false,
            metadata: BTreeMap::new(),
        },
        message: InboundMessage {
            id: info.id.to_string(),
            content: content.unwrap_or_else(|| format!("({} attachment(s))", attachments.len())),
            content_type: "text/plain".to_string(),
            reply_to_message_id: None,
            attachments,
            metadata: message_metadata,
        },
        account_id: None,
        metadata: event_metadata,
    })
}

fn build_inbound_attachments(message: &wa::Message) -> Vec<InboundAttachment> {
    let base_message = message.get_base_message();
    let mut attachments = Vec::new();

    if let Some(image) = &base_message.image_message {
        attachments.push(build_image_attachment(image));
    }
    if let Some(video) = &base_message.video_message {
        attachments.push(build_video_attachment(video));
    }
    if let Some(audio) = &base_message.audio_message {
        attachments.push(build_audio_attachment(audio));
    }
    if let Some(document) = &base_message.document_message {
        attachments.push(build_document_attachment(document));
    }

    for (index, attachment) in attachments.iter_mut().enumerate() {
        attachment.id = Some(format!("attachment-{index}"));
    }

    attachments
}

fn build_image_attachment(image: &wa::message::ImageMessage) -> InboundAttachment {
    let mut extras = BTreeMap::new();
    if let Some(width) = image.width {
        extras.insert("width".to_string(), width.to_string());
    }
    if let Some(height) = image.height {
        extras.insert("height".to_string(), height.to_string());
    }
    if let Some(caption) = image
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        extras.insert("caption".to_string(), caption.to_string());
    }

    InboundAttachment {
        id: None,
        kind: "image".to_string(),
        url: None,
        mime_type: image.mimetype.clone(),
        size_bytes: image.file_length,
        name: None,
        storage_key: None,
        extracted_text: None,
        extras,
    }
}

fn build_video_attachment(video: &wa::message::VideoMessage) -> InboundAttachment {
    let mut extras = BTreeMap::new();
    if let Some(width) = video.width {
        extras.insert("width".to_string(), width.to_string());
    }
    if let Some(height) = video.height {
        extras.insert("height".to_string(), height.to_string());
    }
    if let Some(seconds) = video.seconds {
        extras.insert("duration_seconds".to_string(), seconds.to_string());
    }
    if let Some(caption) = video
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        extras.insert("caption".to_string(), caption.to_string());
    }

    InboundAttachment {
        id: None,
        kind: "video".to_string(),
        url: None,
        mime_type: video.mimetype.clone(),
        size_bytes: video.file_length,
        name: None,
        storage_key: None,
        extracted_text: None,
        extras,
    }
}

fn build_audio_attachment(audio: &wa::message::AudioMessage) -> InboundAttachment {
    let mut extras = BTreeMap::new();
    if let Some(seconds) = audio.seconds {
        extras.insert("duration_seconds".to_string(), seconds.to_string());
    }
    if let Some(ptt) = audio.ptt {
        extras.insert("ptt".to_string(), ptt.to_string());
    }

    InboundAttachment {
        id: None,
        kind: "audio".to_string(),
        url: None,
        mime_type: audio.mimetype.clone(),
        size_bytes: audio.file_length,
        name: None,
        storage_key: None,
        extracted_text: None,
        extras,
    }
}

fn build_document_attachment(document: &wa::message::DocumentMessage) -> InboundAttachment {
    let mut extras = BTreeMap::new();
    if let Some(width) = document.thumbnail_width {
        extras.insert("thumbnail_width".to_string(), width.to_string());
    }
    if let Some(height) = document.thumbnail_height {
        extras.insert("thumbnail_height".to_string(), height.to_string());
    }
    if let Some(caption) = document
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        extras.insert("caption".to_string(), caption.to_string());
    }

    InboundAttachment {
        id: None,
        kind: "document".to_string(),
        url: None,
        mime_type: document.mimetype.clone(),
        size_bytes: document.file_length,
        name: document
            .file_name
            .clone()
            .or_else(|| document.title.clone()),
        storage_key: None,
        extracted_text: None,
        extras,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::types::message::MessageSource;

    fn inbound_info() -> MessageInfo {
        MessageInfo {
            source: MessageSource {
                chat: "15551234567@s.whatsapp.net".parse().unwrap(),
                sender: "15557654321@s.whatsapp.net".parse().unwrap(),
                is_from_me: false,
                is_group: false,
                ..Default::default()
            },
            id: "wamid.test".to_string(),
            push_name: "Tester".to_string(),
            r#type: "chat".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn build_inbound_event_keeps_attachment_only_document_messages() {
        let message = wa::Message {
            document_message: Some(Box::new(wa::message::DocumentMessage {
                file_name: Some("report.pdf".to_string()),
                mimetype: Some("application/pdf".to_string()),
                file_length: Some(42),
                ..Default::default()
            })),
            ..Default::default()
        };

        let event = build_inbound_event_from_message(&message, &inbound_info()).unwrap();
        assert_eq!(event.event_type, "message.received");
        assert_eq!(event.platform, "whatsapp");
        assert_eq!(
            event.metadata.get("platform").map(String::as_str),
            Some("whatsapp")
        );
        assert_eq!(
            event.metadata.get("transport").map(String::as_str),
            Some("websocket")
        );
        assert_eq!(event.message.content, "(1 attachment(s))");
        assert_eq!(event.message.attachments.len(), 1);
        assert_eq!(event.message.attachments[0].kind, "document");
        assert_eq!(
            event.message.attachments[0].mime_type.as_deref(),
            Some("application/pdf")
        );
        assert_eq!(event.message.attachments[0].size_bytes, Some(42));
        assert_eq!(
            event.message.attachments[0].name.as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            event
                .message
                .metadata
                .get("attachment_count")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn build_inbound_event_surfaces_image_attachment_metadata() {
        let message = wa::Message {
            image_message: Some(Box::new(wa::message::ImageMessage {
                mimetype: Some("image/png".to_string()),
                caption: Some("look".to_string()),
                file_length: Some(128),
                width: Some(640),
                height: Some(480),
                ..Default::default()
            })),
            ..Default::default()
        };

        let event = build_inbound_event_from_message(&message, &inbound_info()).unwrap();
        assert_eq!(event.message.content, "look");
        assert_eq!(event.message.attachments.len(), 1);
        assert_eq!(event.message.attachments[0].kind, "image");
        assert_eq!(
            event.message.attachments[0]
                .extras
                .get("width")
                .map(String::as_str),
            Some("640")
        );
        assert_eq!(
            event.message.attachments[0]
                .extras
                .get("height")
                .map(String::as_str),
            Some("480")
        );
        assert_eq!(
            event.message.attachments[0]
                .extras
                .get("caption")
                .map(String::as_str),
            Some("look")
        );
    }
}
