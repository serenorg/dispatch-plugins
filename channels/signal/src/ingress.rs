//! Persistent ingress worker for the native presage-backed Signal plugin.
//!
//! `presage::Manager::receive_messages` returns an async `Stream` that
//! borrows the Manager exclusively. We run the whole receive loop on a
//! single background tokio task inside the plugin-wide runtime, and
//! bridge inbound messages back to the sync JSON-RPC loop via a
//! `std::sync::mpsc::Sender`. The outer stdio loop drains the
//! receiver between requests and emits `channel.event` notifications
//! to dispatch.
//!
//! Outbound calls (deliver, push, status) load a fresh Manager from
//! the shared SQLite store each time. The store is an `Arc<SqlitePool>`
//! internally so cloning it is cheap and SQLite's own locking
//! serializes the writes.

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use jiff::Timestamp;
use presage::Manager;
use presage::libsignal_service::content::{Content, ContentBody};
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage_store_sqlite::SqliteStore;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::runtime::Builder;

use crate::protocol::{
    ChannelConfig, InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope,
    InboundMessage,
};
use crate::store::{resolve_passphrase, resolve_store_path, to_sqlite_url};

const PLATFORM_SIGNAL: &str = "signal";
const TRANSPORT_WEBSOCKET: &str = "websocket";
const RECEIVE_STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);
const STOP_JOIN_GRACE: Duration = Duration::from_secs(3);
const SIGNAL_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

/// A running Signal receive worker. Owns the background OS thread
/// (which in turn runs a single-thread tokio runtime for presage) and
/// the sync-side `mpsc::Receiver` that the outer JSON-RPC loop drains
/// between requests.
///
/// The receive task must run on its own thread with a current-thread
/// tokio runtime because `presage::Manager::receive_messages` returns
/// a `!Send` stream: the underlying libsignal stores hold non-Send
/// types (`Rc<UnsafeCell<_>>` internally) which cannot cross task
/// boundaries on a multi-thread work-stealing runtime.
pub struct IngressWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    event_rx: Receiver<InboundEventEnvelope>,
}

impl IngressWorker {
    /// Pop every queued inbound event the receive task has produced.
    pub fn drain_pending_events(&self) -> Vec<InboundEventEnvelope> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Signal the background task to stop and wait briefly for it to
    /// exit.
    ///
    /// The receive loop polls its stop flag on a short timeout while
    /// waiting for the next Signal frame. That lets us join the worker
    /// during restart or shutdown without leaving a stale thread alive
    /// to consume the next inbound message.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + STOP_JOIN_GRACE;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            if !handle.is_finished() {
                tracing::warn!(
                    grace_ms = STOP_JOIN_GRACE.as_millis() as u64,
                    "channel-signal receive worker did not stop within grace period; joining anyway - process exit may block until the current Signal call returns"
                );
            }
            let _ = handle.join();
        }
    }
}

/// Spawn a new Signal receive worker on a dedicated OS thread running
/// its own current-thread tokio runtime. Fails if the session has not
/// been linked yet (caller should run `channel-signal --link` first).
pub fn start_ingress_worker(config: &ChannelConfig) -> Result<IngressWorker> {
    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Err(anyhow!(
            "Signal session has not been linked yet; run `channel-signal --link` first (store path: {})",
            store_path.display()
        ));
    }
    let url = to_sqlite_url(&store_path);
    let passphrase = resolve_passphrase(config)?;

    let (event_tx, event_rx) = channel::<InboundEventEnvelope>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);

    let handle = std::thread::Builder::new()
        .name("channel-signal-receive".to_string())
        .stack_size(SIGNAL_THREAD_STACK_SIZE)
        .spawn(move || {
            let runtime = match Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(error) => {
                    eprintln!(
                        "channel-signal receive worker failed to build tokio runtime: {error}"
                    );
                    return;
                }
            };
            if let Err(error) =
                runtime.block_on(run_receive_loop(url, passphrase, event_tx, stop_flag))
            {
                eprintln!("channel-signal receive loop terminated: {error}");
            }
        })
        .context("failed to spawn channel-signal receive thread")?;

    Ok(IngressWorker {
        stop,
        handle: Some(handle),
        event_rx,
    })
}

/// Default receive timeout for one-shot `poll_ingress` calls, in
/// seconds. Balanced so that the call usually returns within a single
/// WebSocket handshake + QueueEmpty roundtrip on a linked session
/// with no backlog, but stays generous enough for a larger backlog to
/// drain in the same cycle. Tunable via
/// `ChannelConfig::poll_timeout_secs`.
const DEFAULT_POLL_TIMEOUT_SECS: u16 = 10;

/// Run a single `poll_ingress` cycle and return all inbound events
/// that presage delivers before the first `QueueEmpty` marker or the
/// configured timeout elapses, whichever comes first.
pub fn poll_ingress_once(config: &ChannelConfig) -> Result<Vec<InboundEventEnvelope>> {
    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Err(anyhow!(
            "Signal session has not been linked yet; run `channel-signal --link` first (store path: {})",
            store_path.display()
        ));
    }
    let url = to_sqlite_url(&store_path);
    let passphrase = resolve_passphrase(config)?;
    let timeout_secs = config
        .poll_timeout_secs
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_POLL_TIMEOUT_SECS);
    let timeout = std::time::Duration::from_secs(u64::from(timeout_secs));

    let handle = std::thread::Builder::new()
        .name("channel-signal-poll".to_string())
        .stack_size(SIGNAL_THREAD_STACK_SIZE)
        .spawn(move || -> Result<Vec<InboundEventEnvelope>> {
            let runtime = Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build poll-thread tokio runtime")?;
            runtime.block_on(run_poll_once(url, passphrase, timeout))
        })
        .context("failed to spawn channel-signal poll thread")?;

    handle
        .join()
        .map_err(|_| anyhow!("channel-signal poll thread panicked"))?
}

async fn run_poll_once(
    url: String,
    passphrase: Option<String>,
    timeout: std::time::Duration,
) -> Result<Vec<InboundEventEnvelope>> {
    let store =
        SqliteStore::open_with_passphrase(&url, passphrase.as_deref(), OnNewIdentity::Trust)
            .await
            .with_context(|| format!("failed to open Signal session store at `{url}`"))?;
    let mut manager = Manager::<SqliteStore, Registered>::load_registered(store)
        .await
        .map_err(|error| anyhow!("failed to load Signal session: {error}"))?;
    let messages = manager
        .receive_messages()
        .await
        .map_err(|error| anyhow!("failed to open Signal receive stream: {error}"))?;
    let mut messages = std::pin::pin!(messages);

    // Drain messages until we see QueueEmpty, the stream ends, or the
    // deadline elapses. Events accumulated before a timeout are
    // preserved and returned to the caller.
    let deadline = tokio::time::Instant::now() + timeout;
    let mut events: Vec<InboundEventEnvelope> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, messages.next()).await {
            Ok(Some(Received::Content(content))) => {
                if let Some(event) = build_inbound_event_from_content(&content) {
                    events.push(event);
                }
            }
            Ok(Some(Received::QueueEmpty)) => break,
            Ok(Some(Received::Contacts)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(events)
}

async fn run_receive_loop(
    url: String,
    passphrase: Option<String>,
    event_tx: Sender<InboundEventEnvelope>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let store =
        SqliteStore::open_with_passphrase(&url, passphrase.as_deref(), OnNewIdentity::Trust)
            .await
            .with_context(|| format!("failed to open Signal session store at `{url}`"))?;
    let mut manager = Manager::<SqliteStore, Registered>::load_registered(store)
        .await
        .map_err(|error| anyhow!("failed to load Signal session: {error}"))?;
    let messages = manager
        .receive_messages()
        .await
        .map_err(|error| anyhow!("failed to open Signal receive stream: {error}"))?;
    let mut messages = std::pin::pin!(messages);

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        let received = match tokio::time::timeout(RECEIVE_STOP_POLL_INTERVAL, messages.next()).await
        {
            Ok(Some(received)) => received,
            Ok(None) => break,
            Err(_) => continue,
        };

        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match received {
            Received::Content(content) => {
                if let Some(event) = build_inbound_event_from_content(&content)
                    && event_tx.send(event).is_err()
                {
                    // Outer loop has dropped the receiver; we are
                    // being torn down.
                    break;
                }
            }
            Received::QueueEmpty | Received::Contacts => {}
        }
    }
    Ok(())
}

/// Convert a Signal `Content` into the Dispatch inbound event shape.
/// For v0.1.0 this handles direct-message text plus attachment
/// metadata. Attachment bytes are NOT downloaded here: `presage`'s
/// receive stream borrows the Manager mutably, so
/// `Manager::get_attachment` cannot be called while iterating. The
/// inbound event surfaces `name`, `mime_type`, and `size_bytes` from
/// each `AttachmentPointer` so the agent knows attachments exist and
/// can request explicit fetching later. Full inbound download lands
/// in a follow-up commit.
fn build_inbound_event_from_content(content: &Content) -> Option<InboundEventEnvelope> {
    let data_message = match &content.body {
        ContentBody::DataMessage(msg) => msg,
        _ => return None,
    };
    let text = data_message.body.as_deref().map(str::trim).unwrap_or("");
    let attachments = build_inbound_attachments(&data_message.attachments);

    // Drop messages that carry neither text nor attachments (ACKs,
    // typing indicators routed through DataMessage, etc.).
    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let sender = content.metadata.sender.service_id_string();
    let timestamp_ms = content.metadata.timestamp as i64;

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert("platform".to_string(), PLATFORM_SIGNAL.to_string());
    event_metadata.insert("transport".to_string(), TRANSPORT_WEBSOCKET.to_string());
    event_metadata.insert("signal_timestamp_ms".to_string(), timestamp_ms.to_string());

    let mut message_metadata = event_metadata.clone();
    message_metadata.insert(
        "attachment_count".to_string(),
        attachments.len().to_string(),
    );

    let received_at = Timestamp::from_millisecond(timestamp_ms)
        .unwrap_or_else(|_| Timestamp::now())
        .to_string();

    let content_text = if text.is_empty() {
        format!("({} attachment(s))", attachments.len())
    } else {
        text.to_string()
    };

    Some(InboundEventEnvelope {
        event_id: format!("signal:{sender}:{timestamp_ms}"),
        platform: PLATFORM_SIGNAL.to_string(),
        event_type: "message.received".to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: sender.clone(),
            kind: "signal_direct".to_string(),
            thread_id: None,
            parent_message_id: None,
        },
        actor: InboundActor {
            id: sender,
            display_name: None,
            username: None,
            is_bot: false,
            metadata: BTreeMap::new(),
        },
        message: InboundMessage {
            id: timestamp_ms.to_string(),
            content: content_text,
            content_type: "text/plain".to_string(),
            reply_to_message_id: None,
            attachments,
            metadata: message_metadata,
        },
        account_id: None,
        metadata: event_metadata,
    })
}

/// Build `InboundAttachment` metadata from an AttachmentPointer. Only
/// metadata (name, mime, size) is surfaced; the encrypted bytes
/// remain on Signal's CDN until an explicit download path is added.
fn build_inbound_attachments(
    pointers: &[presage::libsignal_service::proto::AttachmentPointer],
) -> Vec<InboundAttachment> {
    pointers
        .iter()
        .enumerate()
        .map(|(index, pointer)| {
            let mut extras = BTreeMap::new();
            if let Some(digest) = &pointer.digest {
                extras.insert("digest_hex".to_string(), hex_encode(digest));
            }
            if let Some(width) = pointer.width {
                extras.insert("width".to_string(), width.to_string());
            }
            if let Some(height) = pointer.height {
                extras.insert("height".to_string(), height.to_string());
            }
            if let Some(caption) = &pointer.caption {
                extras.insert("caption".to_string(), caption.clone());
            }
            InboundAttachment {
                id: Some(format!("attachment-{index}")),
                kind: "signal_attachment".to_string(),
                url: None,
                mime_type: pointer.content_type.clone(),
                size_bytes: pointer.size.map(u64::from),
                name: pointer.file_name.clone(),
                storage_key: None,
                extracted_text: None,
                extras,
            }
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_waits_for_worker_thread_to_exit() {
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_finished = Arc::clone(&finished);
        let (_event_tx, event_rx) = channel::<InboundEventEnvelope>();

        let handle = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
            worker_finished.store(true, Ordering::Relaxed);
        });

        let worker = IngressWorker {
            stop,
            handle: Some(handle),
            event_rx,
        };

        worker.stop();
        assert!(finished.load(Ordering::Relaxed));
    }
}
