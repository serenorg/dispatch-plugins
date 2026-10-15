//! Persistent ingress worker for the native presage-backed Signal plugin.
//!
//! One worker owns the registered Manager and its websocket tasks. Outbound
//! commands use a clone sharing the receive Manager's cached sockets, while
//! the receive future independently waits for host receipts or reconnects.

use anyhow::{Context, Result, anyhow};
use dispatch_channel_protocol::ChannelEventBatch;
use dispatch_channel_runtime::{
    deliver_channel_event_batch_until, publish_channel_heartbeat, replay_pending_event_batch_until,
};
use futures::StreamExt;
use jiff::Timestamp;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::{Manager, manager::Registered};
use presage_store_sqlite::SqliteStore;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::runtime::Builder;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::LocalSet;

use crate::content_store::SignalStore;
use crate::protocol::{
    ChannelConfig, DeliveryReceipt, InboundActivation, InboundActor, InboundAttachment,
    InboundConversationRef, InboundEventEnvelope, InboundMessage, OutboundMessage,
    SignalChannelPolicy, StatusAcceptance, StatusFrame,
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
    commands: mpsc::UnboundedSender<WorkerCommand>,
    config: ChannelConfig,
    policy: SignalChannelPolicy,
}

enum WorkerCommand {
    Deliver {
        config: ChannelConfig,
        message: OutboundMessage,
        policy: SignalChannelPolicy,
        reply: std::sync::mpsc::Sender<Result<DeliveryReceipt>>,
    },
    Status {
        frame: StatusFrame,
        policy: SignalChannelPolicy,
        reply: std::sync::mpsc::Sender<Result<StatusAcceptance>>,
    },
}

impl IngressWorker {
    fn authorize_command(
        &self,
        config: &ChannelConfig,
        policy: &SignalChannelPolicy,
    ) -> Result<()> {
        if config != &self.config || policy != &self.policy {
            return Err(anyhow!(
                "Signal configuration changed; restart ingress before sending"
            ));
        }
        if self.stop.load(Ordering::Relaxed) {
            return Err(anyhow!("Signal receive worker is stopping"));
        }
        Ok(())
    }

    pub fn deliver(
        &self,
        config: &ChannelConfig,
        message: &OutboundMessage,
        policy: &SignalChannelPolicy,
    ) -> Result<DeliveryReceipt> {
        self.authorize_command(config, policy)?;
        let (reply, result) = std::sync::mpsc::channel();
        self.commands
            .send(WorkerCommand::Deliver {
                config: config.clone(),
                message: message.clone(),
                policy: policy.clone(),
                reply,
            })
            .map_err(|_| {
                anyhow!("Signal receive worker is unavailable; message was not submitted")
            })?;
        result.recv().map_err(|_| {
            anyhow!(
                "Signal receive worker stopped before delivery completed; send outcome is unknown"
            )
        })?
    }

    pub fn status(
        &self,
        config: &ChannelConfig,
        frame: &StatusFrame,
        policy: &SignalChannelPolicy,
    ) -> Result<StatusAcceptance> {
        self.authorize_command(config, policy)?;
        let (reply, result) = std::sync::mpsc::channel();
        self.commands
            .send(WorkerCommand::Status {
                frame: frame.clone(),
                policy: policy.clone(),
                reply,
            })
            .map_err(|_| {
                anyhow!("Signal receive worker is unavailable; status was not submitted")
            })?;
        result.recv().map_err(|_| {
            anyhow!(
                "Signal receive worker stopped before status completed; send outcome is unknown"
            )
        })?
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
    let (commands, receiver) = mpsc::unbounded_channel();
    let worker_policy = policy.clone();

    let handle = std::thread::Builder::new()
        .name("channel-signal-receive".to_string())
        .stack_size(SIGNAL_THREAD_STACK_SIZE)
        .spawn(move || {
            let outcome = run_signal_driver(run_worker(
                url,
                passphrase,
                stdout_lock,
                stop_flag,
                worker_policy,
                account_id,
                receiver,
            ));
            if let Err(error) = outcome {
                terminate_worker("worker", &format!("{error:#}"), 0);
            }
        })
        .context("failed to spawn channel-signal receive thread")?;

    Ok(IngressWorker {
        stop,
        handle: Some(handle),
        commands,
        config: config.clone(),
        policy,
    })
}

/// Presage's receive stream spawns local sync tasks on the owning runtime.
fn run_signal_driver<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Signal tokio runtime")?;
    runtime.block_on(LocalSet::new().run_until(future))
}

async fn run_worker(
    url: String,
    passphrase: Option<String>,
    stdout_lock: Arc<Mutex<()>>,
    stop: Arc<AtomicBool>,
    policy: SignalChannelPolicy,
    account_id: String,
    commands: mpsc::UnboundedReceiver<WorkerCommand>,
) -> Result<()> {
    let mut consecutive_failures = 0;
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    let manager = loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let loaded = async {
            let store = SqliteStore::open_with_passphrase(
                &url,
                passphrase.as_deref(),
                OnNewIdentity::Trust,
            )
            .await
            .with_context(|| format!("failed to open Signal session store at `{url}`"))?;
            SignalStore::load_registered(store, &policy).await
        }
        .await;
        match loaded {
            Ok(manager) => break manager,
            Err(error) => {
                record_session_failure(&format!("{error:#}"), &mut consecutive_failures, backoff);
                sleep_until_stopped(&stop, reconnect_backoff(backoff)).await;
                backoff = std::cmp::min(backoff * 2, MAX_RECONNECT_BACKOFF);
            }
        }
    };

    let connection_gate = AsyncMutex::new(());
    drive_worker_tasks(
        supervise_receive_loop(
            manager.clone(),
            &stdout_lock,
            &stop,
            &policy,
            &account_id,
            (consecutive_failures, backoff),
            &connection_gate,
        ),
        run_commands(manager, commands, &connection_gate),
    )
    .await
}

async fn drive_worker_tasks(
    receive: impl Future<Output = Result<()>>,
    commands: impl Future<Output = Result<()>>,
) -> Result<()> {
    tokio::select! {
        result = receive => result,
        result = commands => result,
    }
}

async fn run_commands(
    mut manager: Manager<SignalStore, Registered>,
    mut commands: mpsc::UnboundedReceiver<WorkerCommand>,
    connection_gate: &AsyncMutex<()>,
) -> Result<()> {
    while let Some(command) = commands.recv().await {
        // Reconnecting must not replace a socket during an outbound send.
        let _connection = connection_gate.lock().await;
        match command {
            WorkerCommand::Deliver {
                config,
                message,
                policy,
                reply,
            } => {
                let result =
                    crate::deliver::deliver_with_manager(&mut manager, &config, &message, &policy)
                        .await;
                let _ = reply.send(result);
            }
            WorkerCommand::Status {
                frame,
                policy,
                reply,
            } => {
                let result =
                    crate::status::status_with_manager(&mut manager, &frame, &policy).await;
                let _ = reply.send(result);
            }
        }
    }
    Err(anyhow!("Signal worker command channel closed"))
}

/// Reconnect transient failures and exit after a terminal failure so the host can replace the plugin.
async fn supervise_receive_loop(
    mut manager: Manager<SignalStore, Registered>,
    stdout_lock: &Arc<Mutex<()>>,
    stop: &Arc<AtomicBool>,
    policy: &SignalChannelPolicy,
    account_id: &str,
    retry_state: (u32, Duration),
    connection_gate: &AsyncMutex<()>,
) -> Result<()> {
    let (mut consecutive_failures, mut backoff) = retry_state;
    while !stop.load(Ordering::Relaxed) {
        let mut connected_at = None;
        let outcome = run_receive_loop(
            &mut manager,
            stdout_lock,
            stop,
            &mut connected_at,
            policy,
            account_id,
            connection_gate,
        )
        .await;
        if session_was_stable(connected_at.map(|started| started.elapsed())) {
            consecutive_failures = 0;
            backoff = INITIAL_RECONNECT_BACKOFF;
        }

        let reason = match classify_session_end(outcome, stop.load(Ordering::Relaxed)) {
            SessionEnd::Stopped => return Ok(()),
            SessionEnd::Failed(reason) => reason,
        };
        record_session_failure(&reason, &mut consecutive_failures, backoff);
        sleep_until_stopped(stop, reconnect_backoff(backoff)).await;
        backoff = std::cmp::min(backoff * 2, MAX_RECONNECT_BACKOFF);
    }
    Ok(())
}

fn record_session_failure(reason: &str, consecutive_failures: &mut u32, backoff: Duration) {
    *consecutive_failures += 1;
    if *consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
        terminate_worker("transport", reason, *consecutive_failures);
    }
    eprintln!(
        "channel-signal receive worker reconnecting: reason=transport retry_count={consecutive_failures} backoff_ms={} message={reason}",
        backoff.as_millis()
    );
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

async fn sleep_until_stopped(stop: &AtomicBool, total: Duration) {
    let mut slept = Duration::ZERO;
    while slept < total && !stop.load(Ordering::Relaxed) {
        let step = std::cmp::min(Duration::from_millis(250), total - slept);
        tokio::time::sleep(step).await;
        slept += step;
    }
}

async fn track_connection<T>(
    open: impl std::future::Future<Output = Result<T>>,
    connected_at: &mut Option<Instant>,
) -> Result<T> {
    *connected_at = None;
    let stream = open.await?;
    *connected_at = Some(Instant::now());
    Ok(stream)
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
/// Batches one `poll_ingress` request may submit. Of the host's 32-frame
/// acceptance page, slots stay free for a replayed pending original and the
/// final state batch.
const POLL_BATCH_BUDGET: usize = 30;

/// Run a single `poll_ingress` cycle, durably delivering inbound events that
/// presage yields before the first `QueueEmpty` marker, the configured
/// timeout, or the batch budget, and return how many were accepted.
pub fn poll_ingress_once(
    config: &ChannelConfig,
    policy: &SignalChannelPolicy,
    account_id: &str,
    stdout_lock: Arc<Mutex<()>>,
) -> Result<usize> {
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
        .spawn(move || -> Result<usize> {
            run_signal_driver(run_poll_once(
                url,
                passphrase,
                timeout,
                policy,
                account_id,
                &stdout_lock,
            ))
        })
        .context("failed to spawn channel-signal poll thread")?;

    handle
        .join()
        .map_err(|_| anyhow!("channel-signal poll thread panicked"))?
}

async fn deliver_content_event<D, DF>(
    event: Option<InboundEventEnvelope>,
    deliver: D,
) -> Result<bool>
where
    D: FnOnce(Vec<InboundEventEnvelope>) -> DF,
    DF: Future<Output = Result<()>>,
{
    let admitted = event.is_some();
    if let Some(event) = event {
        deliver(vec![event]).await?;
    }
    Ok(admitted)
}

async fn run_poll_once(
    url: String,
    passphrase: Option<String>,
    timeout: std::time::Duration,
    policy: SignalChannelPolicy,
    account_id: String,
    stdout_lock: &Arc<Mutex<()>>,
) -> Result<usize> {
    replay_pending_event_batch_until(stdout_lock, &|| false)?;
    let store =
        SqliteStore::open_with_passphrase(&url, passphrase.as_deref(), OnNewIdentity::Trust)
            .await
            .with_context(|| format!("failed to open Signal session store at `{url}`"))?;
    let mut manager = SignalStore::load_registered(store, &policy).await?;
    let messages = manager
        .receive_messages()
        .await
        .map_err(|error| anyhow!("failed to open Signal receive stream: {error}"))?;
    let mut messages = std::pin::pin!(messages);

    // Upstream acknowledges before yielding content. The host journal is the
    // first durable event-delivery boundary that this plugin controls.
    let deadline = tokio::time::Instant::now() + timeout;
    let mut accepted = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, messages.next()).await {
            Ok(Some(Received::Content(content))) => {
                if deliver_content_event(
                    build_inbound_event_from_content(&content, &policy, &account_id),
                    |events| emit_events(stdout_lock, None, events),
                )
                .await?
                {
                    accepted += 1;
                }
                if accepted == POLL_BATCH_BUDGET {
                    break;
                }
            }
            Ok(Some(Received::QueueEmpty)) => break,
            Ok(Some(Received::Contacts)) => {}
            Ok(Some(Received::DecryptionError(_))) => {
                eprintln!("channel-signal message skipped: reason=decryption_error");
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(accepted)
}

async fn run_receive_loop(
    manager: &mut Manager<SignalStore, Registered>,
    stdout_lock: &Arc<Mutex<()>>,
    stop_flag: &Arc<AtomicBool>,
    connected_at: &mut Option<Instant>,
    policy: &SignalChannelPolicy,
    account_id: &str,
    connection_gate: &AsyncMutex<()>,
) -> Result<()> {
    replay_events(stdout_lock, stop_flag).await?;
    let connection = connection_gate.lock().await;
    let messages = track_connection(
        async {
            manager
                .receive_messages()
                .await
                .map_err(|error| anyhow!("failed to open Signal receive stream: {error}"))
        },
        connected_at,
    )
    .await?;
    drop(connection);
    let mut messages = std::pin::pin!(messages);

    // Publish the first heartbeat only after the receive stream opens.
    publish_channel_heartbeat(stdout_lock)?;
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

        // Upstream acknowledged this envelope before yielding it, so content
        // is journaled even when stop was requested; the receipt wait then
        // observes the stop and leaves the batch for replay.
        let mut emitted = false;
        match received {
            Some(Received::Content(content)) => {
                emitted = deliver_content_event(
                    build_inbound_event_from_content(&content, policy, account_id),
                    |events| emit_events(stdout_lock, Some(stop_flag), events),
                )
                .await?;
            }
            Some(Received::DecryptionError(_)) => {
                eprintln!("channel-signal message skipped: reason=decryption_error");
            }
            Some(Received::Contacts | Received::QueueEmpty) | None => {}
        }

        if emitted || last_heartbeat.elapsed() >= LIVENESS_HEARTBEAT_INTERVAL {
            if !emitted {
                publish_channel_heartbeat(stdout_lock)?;
            }
            last_heartbeat = Instant::now();
        }
    }
}

/// Receipt waits must not block the runtime that also serves outbound commands.
async fn replay_events(stdout_lock: &Arc<Mutex<()>>, stop_flag: &Arc<AtomicBool>) -> Result<()> {
    let stdout_lock = Arc::clone(stdout_lock);
    let stop_flag = Arc::clone(stop_flag);
    tokio::task::spawn_blocking(move || {
        replay_pending_event_batch_until(&stdout_lock, &|| stop_flag.load(Ordering::Relaxed))
    })
    .await
    .map_err(|error| anyhow!("Signal durable replay task failed: {error}"))??;
    Ok(())
}

/// Journal a batch and await its host receipt off the receive runtime.
async fn emit_events(
    stdout_lock: &Arc<Mutex<()>>,
    stop_flag: Option<&Arc<AtomicBool>>,
    events: Vec<InboundEventEnvelope>,
) -> Result<()> {
    let stdout_lock = Arc::clone(stdout_lock);
    let stop_flag = stop_flag.cloned();
    tokio::task::spawn_blocking(move || {
        deliver_channel_event_batch_until(
            &stdout_lock,
            ChannelEventBatch {
                events,
                state: None,
                poll_after_ms: None,
            },
            &|| {
                stop_flag
                    .as_ref()
                    .is_some_and(|stop| stop.load(Ordering::Relaxed))
            },
        )
    })
    .await
    .map_err(|error| anyhow!("Signal durable delivery task failed: {error}"))??;
    Ok(())
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
    let (data_message, sender, timestamp_ms) =
        if let Some((own_aci, device_id)) = policy.self_chat_account() {
            let ContentBody::SynchronizeMessage(sync) = &content.body else {
                return None;
            };
            let sent = sync.sent.as_ref()?;
            if u32::from(content.metadata.sender_device) == device_id
                || content.metadata.sender.service_id_string() != own_aci
                || sent.parse_destination_service_id()?.service_id_string() != own_aci
            {
                return None;
            }
            (
                sent.message.as_ref()?,
                own_aci.to_string(),
                i64::try_from(sent.timestamp?).ok()?,
            )
        } else {
            let ContentBody::DataMessage(message) = &content.body else {
                return None;
            };
            (
                message,
                content.metadata.sender.service_id_string(),
                content.metadata.timestamp.timestamp_millis(),
            )
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

    inbound_event(sender, timestamp_ms, text, attachments, policy, account_id)
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

    // A replayed envelope must map to identical bytes, so an out-of-range
    // sender timestamp clamps deterministically instead of using the clock.
    let received_at = Timestamp::from_millisecond(timestamp_ms)
        .unwrap_or(if timestamp_ms < 0 {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        })
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
    use tokio::sync::oneshot;

    fn service_id(value: u8) -> String {
        format!("00000000-0000-0000-0000-0000000000{value:02}")
    }

    #[test]
    fn signal_driver_runs_local_tasks_and_preserves_errors() {
        let value = run_signal_driver(async {
            let value = std::rc::Rc::new(std::cell::Cell::new(0));
            let task_value = value.clone();
            tokio::task::spawn_local(async move {
                tokio::task::yield_now().await;
                task_value.set(42);
            })
            .await?;
            Ok(value.get())
        })
        .unwrap();
        assert_eq!(value, 42);

        let error = run_signal_driver(async {
            tokio::task::spawn_local(async { Err::<(), _>(anyhow!("local operation failed")) })
                .await?
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "local operation failed");
    }

    fn worker_command_harness() -> (IngressWorker, mpsc::UnboundedReceiver<WorkerCommand>) {
        let config = ChannelConfig {
            dm_policy: DmPolicy::Open,
            sqlite_store_path: Some("/nonexistent/signal-worker/store.db".to_string()),
            ..ChannelConfig::default()
        };
        let policy = SignalChannelPolicy::from_config(&config).unwrap();
        let (commands, receiver) = mpsc::unbounded_channel();
        (
            IngressWorker {
                stop: Arc::new(AtomicBool::new(false)),
                handle: None,
                commands,
                config,
                policy,
            },
            receiver,
        )
    }

    #[test]
    fn worker_delivery_preserves_policy_and_returns_transport_errors_without_retry() {
        let (worker, mut commands) = worker_command_harness();
        let expected_config = worker.config.clone();
        let expected_policy = worker.policy.clone();
        let message = OutboundMessage {
            content: "hello".to_string(),
            content_type: None,
            attachments: Vec::new(),
            metadata: BTreeMap::from([("conversation_id".to_string(), service_id(1))]),
        };
        let expected_message = message.clone();
        let handle = std::thread::spawn(move || {
            let WorkerCommand::Deliver {
                config,
                message,
                policy,
                reply,
            } = commands.blocking_recv().unwrap()
            else {
                panic!("expected delivery command");
            };
            assert_eq!(config, expected_config);
            assert_eq!(policy, expected_policy);
            assert_eq!(message, expected_message);
            reply
                .send(Err(anyhow!("transport rejected the message")))
                .unwrap();
            commands
        });
        let error = worker
            .deliver(&worker.config, &message, &worker.policy)
            .unwrap_err();
        assert_eq!(error.to_string(), "transport rejected the message");
        // The caller has returned the failure without submitting it again.
        let mut commands = handle.join().unwrap();
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn active_worker_rejects_changed_configuration_and_competing_poll() {
        let (worker, mut commands) = worker_command_harness();
        let message = OutboundMessage {
            content: "hello".to_string(),
            content_type: None,
            attachments: Vec::new(),
            metadata: BTreeMap::from([("conversation_id".to_string(), service_id(1))]),
        };
        let mut changed = worker.config.clone();
        changed.account = Some("another-account".to_string());
        let error = worker
            .deliver(&changed, &message, &worker.policy)
            .unwrap_err();
        assert!(error.to_string().contains("configuration changed"));
        assert!(commands.try_recv().is_err());

        let request = crate::protocol::PluginRequestEnvelope {
            protocol_version: crate::protocol::CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: crate::protocol::PluginRequest::PollIngress {
                config: worker.config.clone(),
                state: None,
                storage: dispatch_channel_protocol::ChannelIngressStorage {
                    journal_path: "/nonexistent/signal-worker/producer.journal".to_string(),
                    recovery: None,
                },
            },
        };
        let mut worker = Some(worker);
        let response = crate::handle_request(&request, &Arc::new(Mutex::new(())), &mut worker);
        let crate::protocol::PluginResponse::Error { error } = response else {
            panic!("expected poll rejection");
        };
        assert_eq!(error.code, "poll_ingress_failed");
        assert!(error.message.contains("stop ingress before polling"));
        assert!(worker.is_some());
        assert!(commands.try_recv().is_err());

        // A stopping worker refuses new sends instead of queueing them.
        let worker = worker.unwrap();
        worker.stop.store(true, Ordering::Relaxed);
        let error = worker
            .deliver(&worker.config, &message, &worker.policy)
            .unwrap_err();
        assert!(error.to_string().contains("stopping"));
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn event_delivery_waits_for_the_host_receipt_and_propagates_failure() {
        let sender = service_id(1);
        let policy = SignalChannelPolicy::from_config(&ChannelConfig {
            dm_policy: DmPolicy::Allowlist,
            allowed_dm_sender_ids: vec![sender.clone()],
            ..ChannelConfig::default()
        })
        .unwrap();
        let event =
            inbound_event(sender, 42, "hello", Vec::new(), &policy, &service_id(9)).unwrap();
        let (delivery_started, started) = oneshot::channel();
        let (receipt, accepted) = oneshot::channel();
        let expected_event = event.clone();
        let task = tokio::spawn(deliver_content_event(
            Some(event.clone()),
            move |events| async move {
                assert_eq!(
                    serde_json::to_value(&events).unwrap(),
                    serde_json::to_value([expected_event]).unwrap()
                );
                delivery_started.send(()).unwrap();
                accepted.await.map_err(anyhow::Error::from)
            },
        ));
        started.await.unwrap();
        assert!(!task.is_finished());
        receipt.send(()).unwrap();
        assert!(task.await.unwrap().unwrap());

        let error = deliver_content_event(Some(event), |_| async {
            Err(anyhow!("host did not persist the event"))
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("host did not persist"));
    }

    fn allowed_event() -> InboundEventEnvelope {
        let sender = service_id(1);
        let policy = SignalChannelPolicy::from_config(&ChannelConfig {
            dm_policy: DmPolicy::Allowlist,
            allowed_dm_sender_ids: vec![sender.clone()],
            ..ChannelConfig::default()
        })
        .unwrap();
        inbound_event(sender, 42, "hello", Vec::new(), &policy, &service_id(9)).unwrap()
    }

    /// Configure a private host journal like the host would.
    fn host_journal(stdout_lock: &Arc<Mutex<()>>, name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "channel-signal-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dispatch_channel_runtime::create_event_journal_directory(&directory).unwrap();
        let journal = directory.join("producer.journal");
        dispatch_channel_runtime::configure_event_storage(
            stdout_lock,
            &dispatch_channel_protocol::ChannelIngressStorage {
                journal_path: journal.to_string_lossy().into_owned(),
                recovery: None,
            },
        )
        .unwrap();
        journal
    }

    fn journaled_request(
        journal: &std::path::Path,
    ) -> Option<dispatch_channel_protocol::ChannelEventRequest> {
        let line = std::fs::read_to_string(journal).ok()?;
        match dispatch_channel_protocol::parse_jsonrpc_message(&line).ok()? {
            dispatch_channel_protocol::PluginMessage::EventRequest { request, .. } => Some(request),
            _ => None,
        }
    }

    #[test]
    fn worker_commands_progress_during_delivery_and_replay_receipt_waits() {
        run_signal_driver(async {
            for replay in [false, true] {
                let stdout_lock = Arc::new(Mutex::new(()));
                let journal = host_journal(&stdout_lock, "worker-receipt");
                let stop = Arc::new(AtomicBool::new(replay));
                if replay {
                    emit_events(&stdout_lock, Some(&stop), vec![allowed_event()])
                        .await
                        .unwrap_err();
                    stop.store(false, Ordering::Relaxed);
                }
                // A receipt wait that blocked this runtime would also block the
                // command that routes the receipt; stop turns that into a failure.
                let watchdog = Arc::clone(&stop);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(15));
                    watchdog.store(true, Ordering::Relaxed);
                });
                let command_completed = AtomicBool::new(false);
                let outcome = drive_worker_tasks(
                    async {
                        if replay {
                            replay_events(&stdout_lock, &stop).await?;
                        } else {
                            emit_events(&stdout_lock, Some(&stop), vec![allowed_event()]).await?;
                        }
                        assert!(command_completed.load(Ordering::Relaxed));
                        Ok(())
                    },
                    async {
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                        loop {
                            if tokio::time::Instant::now() >= deadline {
                                dispatch_channel_runtime::close_event_input(&stdout_lock);
                                return Err(anyhow!(
                                    "worker receipt wait blocked command processing"
                                ));
                            }
                            if let Some(request) = journaled_request(&journal) {
                                tokio::task::spawn_local(async { tokio::task::yield_now().await })
                                    .await?;
                                command_completed.store(true, Ordering::Relaxed);
                                dispatch_channel_runtime::route_event_acceptance(
                                    &stdout_lock,
                                    request.request_id(),
                                    request.acceptance().unwrap(),
                                )?;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    },
                )
                .await;
                dispatch_channel_runtime::close_event_input(&stdout_lock);
                outcome?;
                assert!(journaled_request(&journal).is_none());
                std::fs::remove_dir_all(journal.parent().unwrap())?;
            }
            Ok(())
        })
        .unwrap();
    }

    #[tokio::test]
    async fn journaled_event_completes_only_with_its_exact_host_receipt() {
        let stdout_lock = Arc::new(Mutex::new(()));
        let journal = host_journal(&stdout_lock, "receipt");
        let event = allowed_event();
        let event_id = event.event_id.clone();
        let task = tokio::spawn({
            let stdout_lock = Arc::clone(&stdout_lock);
            async move { emit_events(&stdout_lock, None, vec![event]).await }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let request = loop {
            if let Some(request) = journaled_request(&journal) {
                break request;
            }
            if tokio::time::Instant::now() >= deadline {
                dispatch_channel_runtime::close_event_input(&stdout_lock);
                let _ = task.await;
                panic!("event was not journaled before the deadline");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(request.batch.events[0].event_id, event_id);
        assert!(!task.is_finished(), "delivery waits for the host receipt");
        while !task.is_finished() {
            if tokio::time::Instant::now() >= deadline {
                dispatch_channel_runtime::close_event_input(&stdout_lock);
                let _ = task.await;
                panic!("event receipt was not completed before the deadline");
            }
            dispatch_channel_runtime::route_event_acceptance(
                &stdout_lock,
                request.request_id(),
                request.acceptance().unwrap(),
            )
            .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.await.unwrap().unwrap();
        assert!(
            journaled_request(&journal).is_none(),
            "the receipt clears the pending batch"
        );
        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn content_received_during_stop_stays_journaled_for_replay() {
        let stdout_lock = Arc::new(Mutex::new(()));
        let journal = host_journal(&stdout_lock, "stop");
        let event = allowed_event();
        let stop = Arc::new(AtomicBool::new(true));
        let error = emit_events(&stdout_lock, Some(&stop), vec![event.clone()])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stopped"));
        let pending = journaled_request(&journal).expect("the batch is journaled");
        assert_eq!(
            serde_json::to_value(&pending.batch.events).unwrap(),
            serde_json::to_value([event]).unwrap()
        );
        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn filtered_content_does_not_submit_a_host_batch() {
        let submitted = AtomicBool::new(false);
        assert!(
            !deliver_content_event(None, |_| async {
                submitted.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await
            .unwrap()
        );
        assert!(!submitted.load(Ordering::Relaxed));
    }

    #[test]
    fn note_to_self_accepts_only_own_transcripts_from_other_devices() {
        use presage::libsignal_service::content::{
            AttachmentPointer, Metadata, SyncMessage, sync_message,
        };
        use presage::libsignal_service::protocol::ServiceId;
        let own = service_id(9);
        let friend = service_id(1);
        let own_id = ServiceId::parse_from_service_id_string(&own).unwrap();
        let policy = SignalChannelPolicy::for_self_chat(
            &ChannelConfig {
                self_chat_only: true,
                dm_policy: DmPolicy::Open,
                ..ChannelConfig::default()
            },
            &own,
            7,
        )
        .unwrap();
        let metadata = Metadata {
            sender: own_id,
            destination: own_id,
            sender_device: 1_u32.try_into().unwrap(),
            timestamp: Default::default(),
            server_timestamp: Default::default(),
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        };
        let mut sent = sync_message::Sent {
            destination_service_id: Some(own.clone()),
            timestamp: Some(42),
            message: Some(DataMessage {
                body: Some("hello".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let transcript = |sent: sync_message::Sent, metadata: Metadata| {
            Content::from_body(
                ContentBody::SynchronizeMessage(SyncMessage {
                    sent: Some(sent),
                    ..Default::default()
                }),
                metadata,
            )
        };
        for device in [1_u32, 3] {
            let mut metadata = metadata.clone();
            metadata.sender_device = device.try_into().unwrap();
            let event = build_inbound_event_from_content(
                &transcript(sent.clone(), metadata),
                &policy,
                &own,
            )
            .unwrap();
            assert_eq!(event.conversation.id, own);
            assert_eq!(event.actor.id, own);
            assert_eq!(event.message.id, "42");
        }
        let mut plugin_metadata = metadata.clone();
        plugin_metadata.sender_device = 7_u32.try_into().unwrap();
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), plugin_metadata),
                &policy,
                &own
            )
            .is_none()
        );
        sent.destination_service_id = Some(friend.clone());
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), metadata.clone()),
                &policy,
                &own
            )
            .is_none()
        );
        // Signal gives binary destinations precedence over string destinations.
        sent.destination_service_id_binary = Some(own_id.service_id_binary());
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), metadata.clone()),
                &policy,
                &own
            )
            .is_some()
        );
        sent.destination_service_id_binary = None;
        sent.destination_service_id = Some(format!("PNI:{own}"));
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), metadata.clone()),
                &policy,
                &own
            )
            .is_none()
        );
        sent.destination_service_id = Some(own.clone());
        let mut friend_metadata = metadata.clone();
        friend_metadata.sender = ServiceId::parse_from_service_id_string(&friend).unwrap();
        let dm = Content::from_body(sent.message.clone().unwrap(), friend_metadata.clone());
        assert!(build_inbound_event_from_content(&dm, &policy, &own).is_none());
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), friend_metadata),
                &policy,
                &own
            )
            .is_none()
        );
        sent.message.as_mut().unwrap().group_v2 = Some(GroupContextV2::default());
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), metadata.clone()),
                &policy,
                &own
            )
            .is_none()
        );
        sent.message = Some(DataMessage::default());
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), metadata.clone()),
                &policy,
                &own
            )
            .is_none()
        );
        sent.message
            .as_mut()
            .unwrap()
            .attachments
            .push(AttachmentPointer {
                content_type: Some("image/png".to_string()),
                ..Default::default()
            });
        assert!(
            build_inbound_event_from_content(
                &transcript(sent.clone(), metadata.clone()),
                &policy,
                &own
            )
            .is_some()
        );
        sent.message = None;
        assert!(
            build_inbound_event_from_content(&transcript(sent, metadata), &policy, &own).is_none()
        );
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

        let (mut worker, _commands) = worker_command_harness();
        worker.stop = stop;
        worker.handle = Some(handle);

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

    #[tokio::test]
    async fn the_stability_clock_starts_only_after_the_stream_opens() {
        let mut connected_at = None;
        let pending = track_connection(std::future::pending::<Result<()>>(), &mut connected_at);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), pending)
                .await
                .is_err()
        );
        assert!(connected_at.is_none());
        assert!(
            track_connection(
                async { Err::<(), _>(anyhow!("open failed")) },
                &mut connected_at
            )
            .await
            .is_err()
        );
        assert!(connected_at.is_none());
        track_connection(async { Ok(()) }, &mut connected_at)
            .await
            .unwrap();
        assert!(connected_at.is_some());
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
