use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
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
/// Send three liveness notifications within the shortest 90-second host grace.
const LIVENESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const CONNECTION_GRACE: Duration = Duration::from_secs(60);
/// Poll fast enough for an initial-connect stop to finish within the join grace.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(60);
/// Clear reconnect history only after one minute of continuous readiness.
const SESSION_STABILITY_WINDOW: Duration = Duration::from_secs(60);
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
const CONNECTION_CONNECTING: u8 = 0;
const CONNECTION_DISCONNECTED: u8 = 1;
const CONNECTION_READY: u8 = 2;
const CONNECTION_LOGGED_OUT: u8 = 3;
const LOGGED_OUT_ERROR: &str = "WhatsApp session logged out";

pub struct IngressWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl IngressWorker {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + STOP_JOIN_GRACE;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            if !handle.is_finished() {
                eprintln!(
                    "channel-whatsapp receive worker did not stop within {} ms; exiting so the host can replace the plugin",
                    STOP_JOIN_GRACE.as_millis()
                );
                std::process::exit(1);
            }
            let _ = handle.join();
        }
    }
}

/// Start supervised ingress that writes notifications under the shared stdout lock.
pub fn start_ingress_worker(
    config: &ChannelConfig,
    stdout_lock: Arc<Mutex<()>>,
) -> Result<IngressWorker> {
    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Err(anyhow!(
            "WhatsApp session has not been linked yet; run `channel-whatsapp --link` first (store path: {})",
            store_path.display()
        ));
    }
    let sqlite_url = to_sqlite_url(&store_path);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);

    let handle = std::thread::Builder::new()
        .name("channel-whatsapp-receive".to_string())
        .spawn(move || supervise_receive_loop(sqlite_url, stdout_lock, stop_flag))
        .context("failed to spawn channel-whatsapp receive thread")?;

    Ok(IngressWorker {
        stop,
        handle: Some(handle),
    })
}

/// Reconnect transient failures and exit after a terminal failure so the host can replace the plugin.
fn supervise_receive_loop(sqlite_url: String, stdout_lock: Arc<Mutex<()>>, stop: Arc<AtomicBool>) {
    let mut consecutive_failures = 0_u32;
    let mut backoff = INITIAL_RECONNECT_BACKOFF;

    while !stop.load(Ordering::Relaxed) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                terminate_worker(
                    "runtime",
                    &format!("failed to build tokio runtime: {error}"),
                    0,
                );
            }
        };

        let mut connected_at = None;
        let mut stable_connection = false;
        let outcome = runtime.block_on(run_receive_loop(
            sqlite_url.clone(),
            &stdout_lock,
            &stop,
            &mut connected_at,
            &mut stable_connection,
        ));
        if stable_connection || session_was_stable(connected_at.map(|started| started.elapsed())) {
            consecutive_failures = 0;
            backoff = INITIAL_RECONNECT_BACKOFF;
        }

        let reason = match classify_session_end(outcome, stop.load(Ordering::Relaxed)) {
            SessionEnd::Stopped => return,
            SessionEnd::Failed(reason) => reason,
        };
        if reason.contains(LOGGED_OUT_ERROR) {
            terminate_worker("auth", &reason, consecutive_failures);
        }
        consecutive_failures += 1;
        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            terminate_worker("transport", &reason, consecutive_failures);
        }
        eprintln!(
            "channel-whatsapp receive worker reconnecting: reason=transport retry_count={consecutive_failures} backoff_ms={} message={reason}",
            backoff.as_millis()
        );
        sleep_until_stopped(&stop, reconnect_backoff(backoff));
        backoff = std::cmp::min(backoff * 2, MAX_RECONNECT_BACKOFF);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SessionEnd {
    Stopped,
    Failed(String),
}

/// Treat only a requested stop as a healthy session end.
fn classify_session_end(outcome: Result<()>, stop_requested: bool) -> SessionEnd {
    if stop_requested {
        return SessionEnd::Stopped;
    }
    match outcome {
        Ok(()) => SessionEnd::Failed("WhatsApp receive stream ended".to_string()),
        Err(error) => SessionEnd::Failed(format!("{error:#}")),
    }
}

fn terminate_worker(reason: &str, message: &str, retry_count: u32) -> ! {
    eprintln!(
        "channel-whatsapp ingress worker terminated: reason={reason} retryable=false retry_count={retry_count} message={message}"
    );
    std::process::exit(1);
}

fn sleep_until_stopped(stop: &AtomicBool, total: Duration) {
    let mut slept = Duration::ZERO;
    while slept < total && !stop.load(Ordering::Relaxed) {
        let step = std::cmp::min(Duration::from_millis(250), total - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

/// Action for the last connection state reported by the upstream client.
#[derive(Debug, PartialEq, Eq)]
enum ConnectionAction {
    /// Connected and fully ready. Heartbeats are allowed.
    Ready,
    /// Not connected and still inside the reconnect grace. Heartbeats are suppressed.
    AwaitingReconnect,
    /// The upstream client did not recover inside the grace, so the session
    /// ends and the supervisor rebuilds it.
    ReconnectGraceExpired,
    /// Credentials are gone. Reconnecting cannot fix this.
    LoggedOut,
}

/// Map the last connection status to the next session action.
fn connection_action(status: u8, disconnected_for: Option<Duration>) -> ConnectionAction {
    match status {
        CONNECTION_LOGGED_OUT => ConnectionAction::LoggedOut,
        CONNECTION_READY => ConnectionAction::Ready,
        _ => match disconnected_for {
            Some(elapsed) if elapsed >= CONNECTION_GRACE => ConnectionAction::ReconnectGraceExpired,
            _ => ConnectionAction::AwaitingReconnect,
        },
    }
}

/// Mark readiness only if no connection callback has reported a newer state.
fn promote_connecting_to_ready(connection_status: &AtomicU8) -> u8 {
    match connection_status.compare_exchange(
        CONNECTION_CONNECTING,
        CONNECTION_READY,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => CONNECTION_READY,
        Err(current) => current,
    }
}

fn session_was_stable(connected_duration: Option<Duration>) -> bool {
    connected_duration.is_some_and(|duration| duration >= SESSION_STABILITY_WINDOW)
}

/// Spread reconnects across processes so replicas that fail together do not
/// retry in lockstep.
fn reconnect_backoff(base: Duration) -> Duration {
    let jitter_bound_ms = (base.as_millis() / 4).max(1) as u64;
    base.saturating_add(Duration::from_millis(jitter_seed() % jitter_bound_ms))
}

fn jitter_seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or_default()
        ^ u64::from(std::process::id()).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ sequence.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= seed >> 30;
    seed = seed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= seed >> 27;
    seed = seed.wrapping_mul(0x94d0_49bb_1331_11eb);
    seed ^ (seed >> 31)
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

            if let Err(error) =
                runtime.block_on(run_poll_receive_loop(sqlite_url, event_tx, &stop_flag))
            {
                eprintln!("{thread_name} terminated: {error}");
            }
        })
        .context("failed to spawn channel-whatsapp background thread")
}

/// Collect inbound events into a channel for a single `poll_ingress` cycle.
///
/// The one-shot path answers a host request, so events travel back in the
/// response rather than as notifications. Unlike the supervised worker, it has
/// no reconnect: the host owns the retry by issuing the next poll.
async fn run_poll_receive_loop(
    sqlite_url: String,
    event_tx: Sender<InboundEventEnvelope>,
    stop_flag: &AtomicBool,
) -> Result<()> {
    let connection_status = Arc::new(AtomicU8::new(CONNECTION_CONNECTING));
    let mut bot = build_ingress_bot(&sqlite_url, event_tx, connection_status).await?;
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

async fn build_ingress_bot(
    sqlite_url: &str,
    event_tx: Sender<InboundEventEnvelope>,
    connection_status: Arc<AtomicU8>,
) -> Result<Bot> {
    let backend = Arc::new(
        SqliteStore::new(sqlite_url)
            .await
            .with_context(|| format!("failed to open WhatsApp session store at `{sqlite_url}`"))?,
    );

    Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .on_event(move |event, _client| {
            let sender = event_tx.clone();
            let connection_status = Arc::clone(&connection_status);
            async move {
                match event {
                    Event::Connected(_) => {
                        connection_status.store(CONNECTION_READY, Ordering::Relaxed);
                    }
                    Event::Disconnected(_) => {
                        connection_status.store(CONNECTION_DISCONNECTED, Ordering::Relaxed);
                    }
                    Event::LoggedOut(_) => {
                        connection_status.store(CONNECTION_LOGGED_OUT, Ordering::Relaxed);
                    }
                    Event::Message(message, info) => {
                        if let Some(inbound) = build_inbound_event_from_message(&message, &info) {
                            let _ = sender.send(inbound);
                        }
                    }
                    _ => {}
                }
            }
        })
        .build()
        .await
        .context("failed to build WhatsApp ingress bot")
}

fn join_receive_thread(handle: JoinHandle<()>) {
    let deadline = Instant::now() + STOP_JOIN_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if !handle.is_finished() {
        tracing::warn!(
            grace_ms = STOP_JOIN_GRACE.as_millis() as u64,
            "channel-whatsapp receive worker did not finish within grace period; joining anyway"
        );
    }
    // Join unconditionally so a slow worker cannot detach and keep
    // consuming inbound messages after a restart replaces it.
    let _ = handle.join();
}

async fn run_receive_loop(
    sqlite_url: String,
    stdout_lock: &Arc<Mutex<()>>,
    stop_flag: &AtomicBool,
    connected_at: &mut Option<Instant>,
    stable_connection: &mut bool,
) -> Result<()> {
    let (event_tx, event_rx) = channel::<InboundEventEnvelope>();
    let connection_status = Arc::new(AtomicU8::new(CONNECTION_CONNECTING));
    let mut bot = build_ingress_bot(&sqlite_url, event_tx, Arc::clone(&connection_status)).await?;

    let client = bot.client();
    let bot_handle = bot.run().await.context("failed to start WhatsApp bot")?;
    let ready = client.wait_for_connected(CONNECTION_GRACE);
    tokio::pin!(ready);
    // Check stop requests during initial readiness so the worker stays within the join grace.
    let readiness = loop {
        tokio::select! {
            result = &mut ready => break Some(result),
            _ = tokio::time::sleep(READINESS_POLL_INTERVAL) => {
                if stop_flag.load(Ordering::Relaxed) {
                    break None;
                }
                if connection_status.load(Ordering::Relaxed) == CONNECTION_LOGGED_OUT {
                    break Some(Err(anyhow!(LOGGED_OUT_ERROR)));
                }
            }
        }
    };
    let Some(readiness) = readiness else {
        client.disconnect().await;
        bot_handle.abort();
        let _ = bot_handle.await;
        return Ok(());
    };
    if let Err(error) = readiness {
        client.disconnect().await;
        bot_handle.abort();
        let _ = bot_handle.await;
        return Err(error)
            .context("WhatsApp did not become ready before the connection grace expired");
    }
    let initial_status = promote_connecting_to_ready(&connection_status);
    tokio::pin!(bot_handle);

    let mut last_heartbeat = Instant::now();
    let mut disconnected_since = None;
    match connection_action(initial_status, None) {
        ConnectionAction::Ready => {
            *connected_at = Some(Instant::now());
            // Publish the first heartbeat only after readiness is established.
            emit_events(stdout_lock, Vec::new())?;
        }
        ConnectionAction::AwaitingReconnect => {
            *connected_at = None;
            disconnected_since = Some(Instant::now());
        }
        ConnectionAction::LoggedOut => {
            client.disconnect().await;
            bot_handle.as_ref().get_ref().abort();
            return Err(anyhow!(LOGGED_OUT_ERROR));
        }
        ConnectionAction::ReconnectGraceExpired => unreachable!(),
    }

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            client.disconnect().await;
            break;
        }

        tokio::select! {
            result = &mut bot_handle => {
                drain_and_emit(stdout_lock, &event_rx)?;
                result.map_err(|_| anyhow!("WhatsApp ingress task ended unexpectedly"))?;
                return Err(anyhow!("WhatsApp ingress task stopped without an error"));
            }
            _ = tokio::time::sleep(STOP_POLL_INTERVAL) => {}
        }

        let emitted = drain_and_emit(stdout_lock, &event_rx)?;
        *stable_connection |= session_was_stable(connected_at.map(|started| started.elapsed()));
        let disconnected_for = disconnected_since.map(|since: Instant| since.elapsed());
        match connection_action(connection_status.load(Ordering::Relaxed), disconnected_for) {
            ConnectionAction::LoggedOut => {
                client.disconnect().await;
                bot_handle.as_ref().get_ref().abort();
                return Err(anyhow!(LOGGED_OUT_ERROR));
            }
            ConnectionAction::Ready => {
                disconnected_since = None;
                if connected_at.is_none() {
                    *connected_at = Some(Instant::now());
                    if !emitted {
                        emit_events(stdout_lock, Vec::new())?;
                    }
                    last_heartbeat = Instant::now();
                }
            }
            ConnectionAction::AwaitingReconnect => {
                *connected_at = None;
                disconnected_since.get_or_insert_with(Instant::now);
                continue;
            }
            ConnectionAction::ReconnectGraceExpired => {
                client.disconnect().await;
                bot_handle.as_ref().get_ref().abort();
                return Err(anyhow!(
                    "WhatsApp did not reconnect before the connection grace expired"
                ));
            }
        }
        if emitted || last_heartbeat.elapsed() >= LIVENESS_HEARTBEAT_INTERVAL {
            if !emitted {
                emit_events(stdout_lock, Vec::new())?;
            }
            last_heartbeat = Instant::now();
        }
    }

    let _ = bot_handle.await;
    Ok(())
}

/// Emit every queued inbound event, reporting whether anything was written.
fn drain_and_emit(
    stdout_lock: &Arc<Mutex<()>>,
    event_rx: &Receiver<InboundEventEnvelope>,
) -> Result<bool> {
    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    if events.is_empty() {
        return Ok(false);
    }
    emit_events(stdout_lock, events)?;
    Ok(true)
}

fn emit_events(stdout_lock: &Arc<Mutex<()>>, events: Vec<InboundEventEnvelope>) -> Result<()> {
    crate::emit_channel_event_notifications(stdout_lock, events)
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

    /// A receive session that ends on its own has lost its connection. Treating
    /// that as a clean exit is what leaves the JSON-RPC parent alive with no
    /// receiver, silently dropping every later message.
    #[test]
    fn a_session_that_ends_without_a_stop_request_is_a_failure() {
        assert_eq!(
            classify_session_end(Ok(()), false),
            SessionEnd::Failed("WhatsApp receive stream ended".to_string())
        );
        assert_eq!(
            classify_session_end(Err(anyhow!("socket closed")), false),
            SessionEnd::Failed("socket closed".to_string())
        );
    }

    /// Shutdown tears the session down too. That must not be counted as a
    /// supervision failure, or a normal stop would exit the process nonzero.
    #[test]
    fn a_session_ended_by_shutdown_is_not_a_failure() {
        assert_eq!(classify_session_end(Ok(()), true), SessionEnd::Stopped);
        assert_eq!(
            classify_session_end(Err(anyhow!("socket closed")), true),
            SessionEnd::Stopped
        );
    }

    #[test]
    fn reconnect_backoff_stays_within_its_jitter_bound() {
        let base = Duration::from_secs(4);
        for _ in 0..16 {
            let delay = reconnect_backoff(base);
            assert!(delay >= base);
            assert!(delay < base + Duration::from_millis(1000));
        }
    }

    #[test]
    fn only_a_ready_connection_can_clear_failure_history() {
        assert!(!session_was_stable(None));
        assert!(!session_was_stable(Some(
            SESSION_STABILITY_WINDOW - Duration::from_millis(1)
        )));
        assert!(session_was_stable(Some(SESSION_STABILITY_WINDOW)));
    }

    /// A heartbeat while WhatsApp is disconnected is exactly what makes a dead
    /// channel look healthy, so the disconnected states must never resolve to
    /// `Ready`.
    #[test]
    fn no_heartbeat_is_allowed_until_whatsapp_is_ready() {
        assert_eq!(
            connection_action(CONNECTION_READY, None),
            ConnectionAction::Ready
        );
        assert_eq!(
            connection_action(CONNECTION_CONNECTING, None),
            ConnectionAction::AwaitingReconnect
        );
        assert_eq!(
            connection_action(CONNECTION_DISCONNECTED, None),
            ConnectionAction::AwaitingReconnect
        );
        // Freshly built sessions stay silent until readiness is established.
        assert_eq!(
            connection_action(CONNECTION_CONNECTING, Some(Duration::ZERO)),
            ConnectionAction::AwaitingReconnect
        );
    }

    #[test]
    fn readiness_does_not_overwrite_a_connection_callback() {
        let connecting = AtomicU8::new(CONNECTION_CONNECTING);
        assert_eq!(promote_connecting_to_ready(&connecting), CONNECTION_READY);
        assert_eq!(connecting.load(Ordering::Relaxed), CONNECTION_READY);

        let disconnected = AtomicU8::new(CONNECTION_DISCONNECTED);
        assert_eq!(
            promote_connecting_to_ready(&disconnected),
            CONNECTION_DISCONNECTED
        );
        assert_eq!(
            disconnected.load(Ordering::Relaxed),
            CONNECTION_DISCONNECTED
        );

        let logged_out = AtomicU8::new(CONNECTION_LOGGED_OUT);
        assert_eq!(
            promote_connecting_to_ready(&logged_out),
            CONNECTION_LOGGED_OUT
        );
        assert_eq!(logged_out.load(Ordering::Relaxed), CONNECTION_LOGGED_OUT);
    }

    #[test]
    fn a_disconnect_ends_the_session_only_after_the_reconnect_grace() {
        assert_eq!(
            connection_action(CONNECTION_DISCONNECTED, Some(CONNECTION_GRACE / 2)),
            ConnectionAction::AwaitingReconnect
        );
        assert_eq!(
            connection_action(CONNECTION_DISCONNECTED, Some(CONNECTION_GRACE)),
            ConnectionAction::ReconnectGraceExpired
        );
        assert_eq!(
            connection_action(
                CONNECTION_DISCONNECTED,
                Some(CONNECTION_GRACE + Duration::from_secs(1))
            ),
            ConnectionAction::ReconnectGraceExpired
        );
    }

    /// A reconnect inside the grace returns to `Ready`, which is what re-arms
    /// heartbeats in the receive loop.
    #[test]
    fn a_reconnect_inside_the_grace_restores_the_ready_state() {
        assert_eq!(
            connection_action(CONNECTION_READY, Some(CONNECTION_GRACE / 2)),
            ConnectionAction::Ready
        );
    }

    /// Logout is an authentication failure, not a transport blip. It must win
    /// over the reconnect grace so the supervisor never retries it.
    #[test]
    fn logout_fails_immediately_regardless_of_the_reconnect_grace() {
        assert_eq!(
            connection_action(CONNECTION_LOGGED_OUT, None),
            ConnectionAction::LoggedOut
        );
        assert_eq!(
            connection_action(CONNECTION_LOGGED_OUT, Some(Duration::ZERO)),
            ConnectionAction::LoggedOut
        );
        assert_eq!(
            connection_action(CONNECTION_LOGGED_OUT, Some(CONNECTION_GRACE * 2)),
            ConnectionAction::LoggedOut
        );
    }

    /// The heartbeat has to fire several times inside the host's shortest
    /// liveness grace, otherwise one slow cycle marks a healthy channel dead.
    #[test]
    fn heartbeat_cadence_fits_inside_the_host_liveness_grace() {
        const HOST_MINIMUM_LIVENESS_GRACE: Duration = Duration::from_secs(90);
        assert!(LIVENESS_HEARTBEAT_INTERVAL * 3 <= HOST_MINIMUM_LIVENESS_GRACE);
    }

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
