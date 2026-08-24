//! Persistent ingress worker for the native presage-backed Signal plugin.
//!
//! `presage::Manager::receive_messages` returns an async `Stream` that
//! borrows the Manager exclusively. We run the whole receive loop on a
//! single background tokio task inside the plugin-wide runtime. The worker
//! writes `channel.event` notifications under the shared stdout lock and sends
//! empty liveness notifications while the receive stream remains open.
//!
//! Outbound calls (deliver, push, status) load a fresh Manager from
//! the shared SQLite store each time. The store is an `Arc<SqlitePool>`
//! internally so cloning it is cheap and SQLite's own locking
//! serializes the writes.

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use jiff::Timestamp;
use presage::Manager;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage_store_sqlite::SqliteStore;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::runtime::Builder;

use crate::protocol::{
    ChannelConfig, InboundActivation, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, SignalChannelPolicy,
};
use crate::store::{resolve_passphrase, resolve_store_path, to_sqlite_url};

const PLATFORM_SIGNAL: &str = "signal";
const TRANSPORT_WEBSOCKET: &str = "websocket";
const RECEIVE_STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);
const STOP_JOIN_GRACE: Duration = Duration::from_secs(3);
const SIGNAL_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
/// Send three liveness notifications within the shortest 90-second host grace.
const LIVENESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(60);
/// Clear reconnect history only after one minute with an open receive stream.
const SESSION_STABILITY_WINDOW: Duration = Duration::from_secs(60);
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// A running Signal receive worker. Owns the background OS thread,
/// which in turn runs a single-thread tokio runtime for presage.
///
/// The receive task must run on its own thread with a current-thread
/// tokio runtime because `presage::Manager::receive_messages` returns
/// a `!Send` stream: the underlying libsignal stores hold non-Send
/// types (`Rc<UnsafeCell<_>>` internally) which cannot cross task
/// boundaries on a multi-thread work-stealing runtime.
pub struct IngressWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl IngressWorker {
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
                eprintln!(
                    "channel-signal receive worker did not stop within {} ms; exiting so the host can replace the plugin",
                    STOP_JOIN_GRACE.as_millis()
                );
                std::process::exit(1);
            }
            let _ = handle.join();
        }
    }
}

/// Start supervised ingress on a dedicated OS thread and write notifications under the shared stdout lock. Fails if the session has not been linked yet.
pub fn start_ingress_worker(
    config: &ChannelConfig,
    stdout_lock: Arc<Mutex<()>>,
    policy: SignalChannelPolicy,
    account_id: String,
) -> Result<IngressWorker> {
    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Err(anyhow!(
            "Signal session has not been linked yet; run `channel-signal --link` first (store path: {})",
            store_path.display()
        ));
    }
    let url = to_sqlite_url(&store_path);
    let passphrase = resolve_passphrase(config)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);

    let handle = std::thread::Builder::new()
        .name("channel-signal-receive".to_string())
        .stack_size(SIGNAL_THREAD_STACK_SIZE)
        .spawn(move || {
            supervise_receive_loop(url, passphrase, stdout_lock, stop_flag, policy, account_id)
        })
        .context("failed to spawn channel-signal receive thread")?;

    Ok(IngressWorker {
        stop,
        handle: Some(handle),
    })
}

/// Reconnect transient failures and exit after a terminal failure so the host can replace the plugin.
fn supervise_receive_loop(
    url: String,
    passphrase: Option<String>,
    stdout_lock: Arc<Mutex<()>>,
    stop: Arc<AtomicBool>,
    policy: SignalChannelPolicy,
    account_id: String,
) {
    let mut consecutive_failures = 0_u32;
    let mut backoff = INITIAL_RECONNECT_BACKOFF;

    while !stop.load(Ordering::Relaxed) {
        let runtime = match Builder::new_current_thread().enable_all().build() {
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
        let outcome = runtime.block_on(run_receive_loop(
            url.clone(),
            passphrase.clone(),
            &stdout_lock,
            &stop,
            &mut connected_at,
            &policy,
            &account_id,
        ));
        if session_was_stable(connected_at.map(|started| started.elapsed())) {
            consecutive_failures = 0;
            backoff = INITIAL_RECONNECT_BACKOFF;
        }

        let reason = match classify_session_end(outcome, stop.load(Ordering::Relaxed)) {
            SessionEnd::Stopped => return,
            SessionEnd::Failed(reason) => reason,
        };
        consecutive_failures += 1;
        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            terminate_worker("transport", &reason, consecutive_failures);
        }
        eprintln!(
            "channel-signal receive worker reconnecting: reason=transport retry_count={consecutive_failures} backoff_ms={} message={reason}",
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
        Ok(()) => SessionEnd::Failed("Signal receive stream ended".to_string()),
        Err(error) => SessionEnd::Failed(format!("{error:#}")),
    }
}

fn terminate_worker(reason: &str, message: &str, retry_count: u32) -> ! {
    eprintln!(
        "channel-signal ingress worker terminated: reason={reason} retryable=false retry_count={retry_count} message={message}"
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
pub fn poll_ingress_once(
    config: &ChannelConfig,
    policy: &SignalChannelPolicy,
    account_id: &str,
) -> Result<Vec<InboundEventEnvelope>> {
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
    let policy = policy.clone();
    let account_id = account_id.to_string();

    let handle = std::thread::Builder::new()
        .name("channel-signal-poll".to_string())
        .stack_size(SIGNAL_THREAD_STACK_SIZE)
        .spawn(move || -> Result<Vec<InboundEventEnvelope>> {
            let runtime = Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build poll-thread tokio runtime")?;
            runtime.block_on(run_poll_once(url, passphrase, timeout, policy, account_id))
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
    policy: SignalChannelPolicy,
    account_id: String,
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
                if let Some(event) =
                    build_inbound_event_from_content(&content, &policy, &account_id)
                {
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
    stdout_lock: &Arc<Mutex<()>>,
    stop_flag: &AtomicBool,
    connected_at: &mut Option<Instant>,
    policy: &SignalChannelPolicy,
    account_id: &str,
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
    *connected_at = Some(Instant::now());

    // Publish the first heartbeat only after the receive stream opens.
    emit_events(stdout_lock, Vec::new())?;
    let mut last_heartbeat = Instant::now();

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let received = match tokio::time::timeout(RECEIVE_STOP_POLL_INTERVAL, messages.next()).await
        {
            Ok(Some(received)) => Some(received),
            // An unrequested stream end is a transport failure.
            Ok(None) => return Err(anyhow!("Signal receive stream closed")),
            Err(_) => None,
        };

        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut emitted = false;
        if let Some(Received::Content(content)) = received
            && let Some(event) = build_inbound_event_from_content(&content, policy, account_id)
        {
            emit_events(stdout_lock, vec![event])?;
            emitted = true;
        }

        if emitted || last_heartbeat.elapsed() >= LIVENESS_HEARTBEAT_INTERVAL {
            if !emitted {
                emit_events(stdout_lock, Vec::new())?;
            }
            last_heartbeat = Instant::now();
        }
    }
}

fn emit_events(stdout_lock: &Arc<Mutex<()>>, events: Vec<InboundEventEnvelope>) -> Result<()> {
    crate::emit_channel_event_notifications(stdout_lock, events)
}

/// Convert a Signal `Content` into the Dispatch inbound event shape.
/// This handles direct-message text plus attachment
/// metadata. Attachment bytes are NOT downloaded here: `presage`'s
/// receive stream borrows the Manager mutably, so
/// `Manager::get_attachment` cannot be called while iterating. The
/// inbound event surfaces `name`, `mime_type`, and `size_bytes` from
/// each `AttachmentPointer` so the agent knows attachments exist and
/// can request explicit fetching later. Full inbound download lands
/// in a follow-up commit.
fn build_inbound_event_from_content(
    content: &Content,
    policy: &SignalChannelPolicy,
    account_id: &str,
) -> Option<InboundEventEnvelope> {
    let data_message = match &content.body {
        ContentBody::DataMessage(msg) => msg,
        _ => return None,
    };
    if !is_direct_message(data_message) {
        return None;
    }
    let text = data_message.body.as_deref().map(str::trim).unwrap_or("");
    let attachments = build_inbound_attachments(&data_message.attachments);

    // Drop messages that carry neither text nor attachments (ACKs,
    // typing indicators routed through DataMessage, etc.).
    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let timestamp_ms = content.metadata.timestamp as i64;

    inbound_event(
        content.metadata.sender.service_id_string(),
        timestamp_ms,
        text,
        attachments,
        policy,
        account_id,
    )
}

fn is_direct_message(message: &DataMessage) -> bool {
    message.group_v2.is_none()
}

fn inbound_event(
    sender: String,
    timestamp_ms: i64,
    text: &str,
    attachments: Vec<InboundAttachment>,
    policy: &SignalChannelPolicy,
    account_id: &str,
) -> Option<InboundEventEnvelope> {
    if !policy.allows_inbound_sender(&sender) {
        return None;
    }

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
            kind: "dm".to_string(),
            thread_id: None,
            parent_message_id: None,
            workspace_id: None,
            parent_conversation_id: None,
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
        account_id: Some(account_id.to_string()),
        activation: Some(InboundActivation {
            reason: InboundActivation::REASON_DIRECT_MESSAGE.to_string(),
            agent_account_id: Some(account_id.to_string()),
            referenced_message_author_id: None,
        }),
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
    use crate::protocol::{DmPolicy, SignalChannelPolicy};
    use presage::libsignal_service::content::GroupContextV2;

    fn service_id(value: u8) -> String {
        format!("00000000-0000-0000-0000-0000000000{value:02}")
    }

    #[test]
    fn stop_waits_for_worker_thread_to_exit() {
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_finished = Arc::clone(&finished);

        let handle = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
            worker_finished.store(true, Ordering::Relaxed);
        });

        let worker = IngressWorker {
            stop,
            handle: Some(handle),
        };

        worker.stop();
        assert!(finished.load(Ordering::Relaxed));
    }

    /// A receive stream that ends on its own has lost its connection. Treating
    /// that as a clean exit is what leaves the JSON-RPC parent alive with no
    /// receiver, silently dropping every later message.
    #[test]
    fn a_session_that_ends_without_a_stop_request_is_a_failure() {
        assert_eq!(
            classify_session_end(Ok(()), false),
            SessionEnd::Failed("Signal receive stream ended".to_string())
        );
        assert_eq!(
            classify_session_end(Err(anyhow!("stream closed")), false),
            SessionEnd::Failed("stream closed".to_string())
        );
    }

    /// Shutdown tears the session down too. That must not be counted as a
    /// supervision failure, or a normal stop would exit the process nonzero.
    #[test]
    fn a_session_ended_by_shutdown_is_not_a_failure() {
        assert_eq!(classify_session_end(Ok(()), true), SessionEnd::Stopped);
        assert_eq!(
            classify_session_end(Err(anyhow!("stream closed")), true),
            SessionEnd::Stopped
        );
    }

    /// The heartbeat has to fire several times inside the host's shortest
    /// liveness grace, otherwise one slow cycle marks a healthy channel dead.
    #[test]
    fn heartbeat_cadence_fits_inside_the_host_liveness_grace() {
        const HOST_MINIMUM_LIVENESS_GRACE: Duration = Duration::from_secs(90);
        assert!(LIVENESS_HEARTBEAT_INTERVAL * 3 <= HOST_MINIMUM_LIVENESS_GRACE);
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
    fn only_an_open_receive_stream_can_clear_failure_history() {
        assert!(!session_was_stable(None));
        assert!(!session_was_stable(Some(
            SESSION_STABILITY_WINDOW - Duration::from_millis(1)
        )));
        assert!(session_was_stable(Some(SESSION_STABILITY_WINDOW)));
    }

    /// The threshold above is only meaningful if the clock starts when the
    /// stream is genuinely open. Starting it earlier would let a slow store
    /// open or session load count as connected time and clear the failure
    /// history for a session that never received anything.
    ///
    /// Exercising this needs a live Signal session, so pin the ordering in the
    /// source until an injectable stream abstraction exists.
    #[test]
    fn the_stability_clock_starts_only_after_the_stream_opens() {
        const SOURCE: &str = include_str!("ingress.rs");
        let start = SOURCE
            .find("async fn run_receive_loop(")
            .expect("run_receive_loop is present");
        let body = &SOURCE[start..];
        let stream_open = body
            .find(".receive_messages()")
            .expect("the stream is opened in run_receive_loop");
        let clock_start = body
            .find("*connected_at = Some(")
            .expect("the stability clock is armed in run_receive_loop");

        assert!(
            stream_open < clock_start,
            "connected_at must be armed after receive_messages() succeeds"
        );
    }

    #[test]
    fn inbound_events_have_authenticated_direct_message_provenance() {
        let sender = service_id(1);
        let account_id = service_id(9);
        let policy = SignalChannelPolicy::from_config(&ChannelConfig {
            dm_policy: DmPolicy::Allowlist,
            allowed_dm_sender_ids: vec![sender.clone()],
            ..ChannelConfig::default()
        })
        .unwrap();

        let event = inbound_event(sender.clone(), 1, "hello", Vec::new(), &policy, &account_id)
            .expect("authorized event");
        assert_eq!(event.platform, PLATFORM_SIGNAL);
        assert_eq!(event.conversation.id, sender);
        assert_eq!(event.conversation.kind, "dm");
        assert_eq!(event.account_id.as_deref(), Some(account_id.as_str()));
        assert_eq!(
            event
                .activation
                .as_ref()
                .map(|activation| activation.reason.as_str()),
            Some(InboundActivation::REASON_DIRECT_MESSAGE)
        );
        assert_eq!(
            event
                .activation
                .as_ref()
                .and_then(|activation| activation.agent_account_id.as_deref()),
            Some(account_id.as_str())
        );
    }

    #[test]
    fn inbound_filter_drops_unauthorized_sender_before_notification() {
        let policy = SignalChannelPolicy::from_config(&ChannelConfig {
            dm_policy: DmPolicy::Allowlist,
            allowed_dm_sender_ids: vec![service_id(1)],
            ..ChannelConfig::default()
        })
        .unwrap();

        assert!(
            inbound_event(
                service_id(2),
                1,
                "hello",
                Vec::new(),
                &policy,
                &service_id(9)
            )
            .is_none()
        );
    }

    #[test]
    fn group_messages_are_not_reclassified_as_direct_messages() {
        assert!(is_direct_message(&DataMessage::default()));
        assert!(!is_direct_message(&DataMessage {
            group_v2: Some(GroupContextV2::default()),
            ..DataMessage::default()
        }));
    }
}
