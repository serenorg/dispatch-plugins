use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dispatch_channel_protocol::{ChannelEventBatch, MAX_CHANNEL_BATCH_EVENTS};
use dispatch_channel_runtime::{
    deliver_channel_event_batch_until, publish_channel_heartbeat, replay_pending_event_batch_until,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use whatsapp_rust::Client;
use whatsapp_rust::TokioRuntime;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::proto_helpers::MessageExt;
use whatsapp_rust::types::durability_hook::{
    InboundDurabilityHook, InboundMessage as WaInboundMessage,
};
use whatsapp_rust::types::events::Event;
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust_sqlite_storage::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::policy::WhatsAppPolicy;
use crate::protocol::{
    ChannelConfig, InboundActivation, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage,
};
use crate::store::{resolve_store_path, to_sqlite_url};

const PLATFORM_WHATSAPP: &str = "whatsapp";

/// Byte budget for one chunk's events, below the channel batch bound.
const CHUNK_EVENT_BYTES: usize = 1_500_000;
/// Batches one `poll_ingress` request may submit. Of the host's 32-frame
/// acceptance page, slots stay free for a replayed pending original, the
/// re-send of a chunk whose receipt wait was stopped, and the final state batch.
const POLL_BATCH_BUDGET: usize = 29;

/// Delivery state shared by the durability hook, the event callback, and the
/// receive loop that owns them.
struct HookDelivery {
    stdout_lock: Arc<Mutex<()>>,
    policy: WhatsAppPolicy,
    account_id: String,
    stop: Arc<AtomicBool>,
    /// Set when a batch was not durably accepted; its provider ACK is withheld
    /// and the server redelivers it only after a reconnect.
    failed: AtomicBool,
    delivered: AtomicUsize,
    poll_budget: Option<AtomicUsize>,
}

impl HookDelivery {
    fn new(
        stdout_lock: Arc<Mutex<()>>,
        policy: WhatsAppPolicy,
        account_id: String,
        stop: Arc<AtomicBool>,
        poll_mode: bool,
    ) -> Self {
        Self {
            stdout_lock,
            policy,
            account_id,
            stop,
            failed: AtomicBool::new(false),
            delivered: AtomicUsize::new(0),
            poll_budget: poll_mode.then(|| AtomicUsize::new(POLL_BATCH_BUDGET)),
        }
    }

    /// Durably deliver every chunk of one library batch, or fail the whole
    /// batch so the library withholds all of its provider ACKs.
    fn deliver(&self, messages: &[WaInboundMessage]) -> Result<()> {
        let stopped = || self.stop.load(Ordering::Relaxed);
        replay_pending_event_batch_until(&self.stdout_lock, &stopped)?;
        let chunks = inbound_event_chunks(
            messages
                .iter()
                .map(|item| (item.message.as_ref(), item.info.as_ref())),
            &self.policy,
            &self.account_id,
        )?;
        if let Some(budget) = &self.poll_budget
            && budget
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(chunks.len())
                })
                .is_err()
        {
            // Refuse before any chunk is submitted; the next poll receives it.
            self.stop.store(true, Ordering::Relaxed);
            return Err(anyhow!(
                "WhatsApp batch exceeds this poll's acceptance page and will be redelivered"
            ));
        }
        accept_chunks(chunks, |batch| {
            let count = batch.events.len();
            deliver_channel_event_batch_until(&self.stdout_lock, batch, &stopped)?;
            self.delivered.fetch_add(count, Ordering::Relaxed);
            Ok(())
        })?;
        Ok(())
    }
}

fn accept_chunks(
    chunks: Vec<Vec<InboundEventEnvelope>>,
    mut accept: impl FnMut(ChannelEventBatch) -> Result<()>,
) -> Result<()> {
    for events in chunks {
        accept(ChannelEventBatch {
            events,
            state: None,
            poll_after_ms: None,
        })?;
    }
    Ok(())
}

/// Journal writes and receipt waits block, so they run off the receive runtime.
async fn deliver_off_runtime(
    delivery: Arc<HookDelivery>,
    messages: Vec<WaInboundMessage>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || delivery.deliver(&messages))
        .await
        .map_err(|error| anyhow!("WhatsApp durable delivery task failed: {error}"))?
}

struct DurableIngressHook {
    delivery: Arc<HookDelivery>,
}

#[whatsapp_rust::async_trait]
impl InboundDurabilityHook for DurableIngressHook {
    async fn on_messages(&self, _client: Arc<Client>, batch: &[WaInboundMessage]) -> Result<()> {
        let result = deliver_off_runtime(Arc::clone(&self.delivery), batch.to_vec()).await;
        if result.is_err() {
            self.delivery.failed.store(true, Ordering::Relaxed);
        }
        result
    }
}

/// Map one library batch to channel chunks of at most 32 events within the
/// byte budget. A repeated provider identity in the batch is delivered once.
fn inbound_event_chunks<'a>(
    messages: impl IntoIterator<Item = (&'a wa::Message, &'a MessageInfo)>,
    policy: &WhatsAppPolicy,
    account_id: &str,
) -> Result<Vec<Vec<InboundEventEnvelope>>> {
    let mut chunks = Vec::new();
    let mut events = Vec::new();
    let mut bytes = 0;
    let mut seen = BTreeSet::new();
    for (message, info) in messages {
        let Some(event) = build_inbound_event_from_message(message, info, policy, account_id)
        else {
            continue;
        };
        if !seen.insert(event.event_id.clone()) {
            continue;
        }
        let event_bytes = serde_json::to_vec(&event)?.len() + 1;
        if !events.is_empty()
            && (events.len() == MAX_CHANNEL_BATCH_EVENTS || bytes + event_bytes > CHUNK_EVENT_BYTES)
        {
            chunks.push(std::mem::take(&mut events));
            bytes = 0;
        }
        bytes += event_bytes;
        events.push(event);
    }
    if !events.is_empty() {
        chunks.push(events);
    }
    Ok(chunks)
}

const TRANSPORT_WEBSOCKET: &str = "websocket";
const STOP_JOIN_GRACE: Duration = Duration::from_secs(3);
const STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_POLL_TIMEOUT_SECS: u16 = 10;
const POLL_IDLE_GRACE: Duration = Duration::from_millis(1500);
const POLL_CHECK_INTERVAL: Duration = Duration::from_millis(50);
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
    policy: WhatsAppPolicy,
    account_id: String,
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
        .spawn(move || {
            supervise_receive_loop(sqlite_url, policy, account_id, stdout_lock, stop_flag)
        })
        .context("failed to spawn channel-whatsapp receive thread")?;

    Ok(IngressWorker {
        stop,
        handle: Some(handle),
    })
}

/// Reconnect transient failures and exit after a terminal failure so the host can replace the plugin.
fn supervise_receive_loop(
    sqlite_url: String,
    policy: WhatsAppPolicy,
    account_id: String,
    stdout_lock: Arc<Mutex<()>>,
    stop: Arc<AtomicBool>,
) {
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
            policy.clone(),
            account_id.clone(),
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

/// Receive for one `poll_ingress` request. The durability hook delivers each
/// batch to the host before WhatsApp is acknowledged; this call only decides
/// when the request has received enough.
pub fn poll_ingress_once(
    config: &ChannelConfig,
    policy: WhatsAppPolicy,
    account_id: String,
    stdout_lock: Arc<Mutex<()>>,
) -> Result<()> {
    let timeout = Duration::from_secs(u64::from(
        config
            .poll_timeout_secs
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_POLL_TIMEOUT_SECS),
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let delivery = Arc::new(HookDelivery::new(
        stdout_lock,
        policy,
        account_id,
        Arc::clone(&stop),
        true,
    ));
    let handle = spawn_receive_thread(config, Arc::clone(&delivery), "channel-whatsapp-poll")
        .context("failed to spawn channel-whatsapp poll thread")?;

    let deadline = Instant::now() + timeout;
    let mut delivered = 0;
    let mut idle_since = None;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) && !handle.is_finished() {
        std::thread::sleep(POLL_CHECK_INTERVAL);
        let now_delivered = delivery.delivered.load(Ordering::Relaxed);
        if now_delivered != delivered {
            delivered = now_delivered;
            idle_since = Some(Instant::now());
        }
        if idle_since.is_some_and(|since: Instant| since.elapsed() >= POLL_IDLE_GRACE) {
            break;
        }
    }

    stop.store(true, Ordering::Relaxed);
    join_receive_thread(handle);
    if delivery.failed.load(Ordering::Relaxed) && delivery.delivered.load(Ordering::Relaxed) == 0 {
        return Err(anyhow!(
            "WhatsApp messages were not durably accepted; they will be redelivered"
        ));
    }
    Ok(())
}

fn spawn_receive_thread(
    config: &ChannelConfig,
    delivery: Arc<HookDelivery>,
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

            if let Err(error) = runtime.block_on(run_poll_receive_loop(sqlite_url, delivery)) {
                eprintln!("{thread_name} terminated: {error}");
            }
        })
        .context("failed to spawn channel-whatsapp background thread")
}

/// Hold one connection open for a single `poll_ingress` request. The host
/// owns the retry by issuing the next poll.
async fn run_poll_receive_loop(sqlite_url: String, delivery: Arc<HookDelivery>) -> Result<()> {
    let connection_status = Arc::new(AtomicU8::new(CONNECTION_CONNECTING));
    let bot = build_ingress_bot(&sqlite_url, Arc::clone(&delivery), connection_status).await?;
    let mut bot_handle = bot.spawn();

    loop {
        if delivery.stop.load(Ordering::Relaxed) {
            bot_handle.shutdown().await;
            return Ok(());
        }

        tokio::select! {
            _ = &mut bot_handle => return Err(anyhow!("WhatsApp ingress task ended unexpectedly")),
            _ = tokio::time::sleep(STOP_POLL_INTERVAL) => {}
        }
    }
}

async fn build_ingress_bot(
    sqlite_url: &str,
    delivery: Arc<HookDelivery>,
    connection_status: Arc<AtomicU8>,
) -> Result<Bot> {
    let backend = SqliteStore::new(sqlite_url)
        .await
        .with_context(|| format!("failed to open WhatsApp session store at `{sqlite_url}`"))?;

    let builder = Bot::builder();
    let builder = if delivery.policy.is_self_chat() {
        builder.skip_history_sync()
    } else {
        builder
    };
    builder
        .with_backend(backend)
        .with_inbound_durability_hook(DurableIngressHook {
            delivery: Arc::clone(&delivery),
        })
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .on_event(move |event, _client| {
            let delivery = Arc::clone(&delivery);
            let connection_status = Arc::clone(&connection_status);
            async move {
                match &*event {
                    Event::Connected(_) => {
                        connection_status.store(CONNECTION_READY, Ordering::Relaxed);
                    }
                    Event::Disconnected(_) => {
                        connection_status.store(CONNECTION_DISCONNECTED, Ordering::Relaxed);
                    }
                    Event::LoggedOut(_) => {
                        connection_status.store(CONNECTION_LOGGED_OUT, Ordering::Relaxed);
                    }
                    // The library acknowledges PDO recoveries and newsletters
                    // on its own path, so this event is their only delivery.
                    Event::Messages(batch) if !batch.hook_committed => {
                        let messages = batch.messages.to_vec();
                        if deliver_off_runtime(delivery, messages).await.is_err() {
                            eprintln!(
                                "channel-whatsapp could not durably deliver a message the provider already acknowledged"
                            );
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
    policy: WhatsAppPolicy,
    account_id: String,
    stdout_lock: &Arc<Mutex<()>>,
    stop_flag: &Arc<AtomicBool>,
    connected_at: &mut Option<Instant>,
    stable_connection: &mut bool,
) -> Result<()> {
    replay_pending_event_batch_until(stdout_lock, &|| stop_flag.load(Ordering::Relaxed))?;
    let delivery = Arc::new(HookDelivery::new(
        Arc::clone(stdout_lock),
        policy,
        account_id,
        Arc::clone(stop_flag),
        false,
    ));
    let connection_status = Arc::new(AtomicU8::new(CONNECTION_CONNECTING));
    let bot = build_ingress_bot(
        &sqlite_url,
        Arc::clone(&delivery),
        Arc::clone(&connection_status),
    )
    .await?;

    let client = bot.client();
    let mut bot_handle = bot.spawn();
    let ready = client.wait_for_connected(CONNECTION_GRACE);
    tokio::pin!(ready);
    // Check stop requests during initial readiness so the worker stays within the join grace.
    let readiness = loop {
        tokio::select! {
            result = &mut ready => break Some(result.map_err(anyhow::Error::from)),
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
    let mut last_heartbeat = Instant::now();
    let mut disconnected_since = None;
    let mut delivered = delivery.delivered.load(Ordering::Relaxed);
    match connection_action(initial_status, None) {
        ConnectionAction::Ready => {
            *connected_at = Some(Instant::now());
            // Publish the first heartbeat only after readiness is established.
            publish_channel_heartbeat(stdout_lock)?;
        }
        ConnectionAction::AwaitingReconnect => {
            *connected_at = None;
            disconnected_since = Some(Instant::now());
        }
        ConnectionAction::LoggedOut => {
            client.disconnect().await;
            bot_handle.abort();
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
            _ = &mut bot_handle => {
                return Err(anyhow!("WhatsApp ingress task stopped without an error"));
            }
            _ = tokio::time::sleep(STOP_POLL_INTERVAL) => {}
        }

        if delivery.failed.load(Ordering::Relaxed) && !stop_flag.load(Ordering::Relaxed) {
            // WhatsApp redelivers an unacknowledged batch only on a new connection.
            client.disconnect().await;
            bot_handle.abort();
            return Err(anyhow!(
                "inbound messages were not durably accepted; reconnecting for redelivery"
            ));
        }
        let now_delivered = delivery.delivered.load(Ordering::Relaxed);
        let emitted = now_delivered != delivered;
        delivered = now_delivered;
        *stable_connection |= session_was_stable(connected_at.map(|started| started.elapsed()));
        let disconnected_for = disconnected_since.map(|since: Instant| since.elapsed());
        match connection_action(connection_status.load(Ordering::Relaxed), disconnected_for) {
            ConnectionAction::LoggedOut => {
                client.disconnect().await;
                bot_handle.abort();
                return Err(anyhow!(LOGGED_OUT_ERROR));
            }
            ConnectionAction::Ready => {
                disconnected_since = None;
                if connected_at.is_none() {
                    *connected_at = Some(Instant::now());
                    if !emitted {
                        publish_channel_heartbeat(stdout_lock)?;
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
                bot_handle.abort();
                return Err(anyhow!(
                    "WhatsApp did not reconnect before the connection grace expired"
                ));
            }
        }
        if emitted || last_heartbeat.elapsed() >= LIVENESS_HEARTBEAT_INTERVAL {
            if !emitted {
                publish_channel_heartbeat(stdout_lock)?;
            }
            last_heartbeat = Instant::now();
        }
    }

    bot_handle.shutdown().await;
    Ok(())
}

fn build_inbound_event_from_message(
    message: &wa::Message,
    info: &MessageInfo,
    policy: &WhatsAppPolicy,
    account_id: &str,
) -> Option<InboundEventEnvelope> {
    if info.source.is_group {
        return None;
    }
    let (sender_id, conversation_id) = if policy.is_self_chat() {
        if !info.source.is_from_me
            || !policy.accepts_self_chat(&info.source.chat, &info.source.sender)
        {
            return None;
        }
        let chat_id = info.source.chat.to_non_ad().to_string();
        (chat_id.clone(), chat_id)
    } else {
        if info.source.is_from_me {
            return None;
        }
        (info.source.sender.to_string(), info.source.chat.to_string())
    };
    if !policy.allows_inbound_sender(&sender_id)
        || crate::policy::normalize_direct_jid(&sender_id, "inbound sender").is_err()
        || crate::policy::normalize_direct_jid(&conversation_id, "inbound conversation").is_err()
    {
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
    if !info.r#type.trim().is_empty() {
        event_metadata.insert("message_type".to_string(), info.r#type.clone());
    }
    if !info.media_type.trim().is_empty() {
        event_metadata.insert("media_type".to_string(), info.media_type.clone());
    }

    let mut message_metadata = BTreeMap::new();
    message_metadata.insert(
        "attachment_count".to_string(),
        attachments.len().to_string(),
    );

    Some(InboundEventEnvelope {
        event_id: inbound_event_id(info),
        platform: PLATFORM_WHATSAPP.to_string(),
        event_type: "message.received".to_string(),
        received_at: info.timestamp.to_rfc3339(),
        conversation: InboundConversationRef {
            id: conversation_id,
            kind: "dm".to_string(),
            thread_id: None,
            parent_message_id: None,
            workspace_id: None,
            parent_conversation_id: None,
        },
        actor: InboundActor {
            id: sender_id,
            display_name: None,
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
        account_id: Some(account_id.to_string()),
        activation: Some(InboundActivation {
            reason: InboundActivation::REASON_DIRECT_MESSAGE.to_string(),
            agent_account_id: Some(account_id.to_string()),
            referenced_message_author_id: None,
        }),
        metadata: event_metadata,
    })
}

fn inbound_event_id(info: &MessageInfo) -> String {
    let mut digest = Sha256::new();
    digest.update(b"whatsapp-inbound-message");
    for component in [
        info.source.chat.to_string(),
        info.source.sender.to_string(),
        info.id.to_string(),
    ] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component.as_bytes());
    }
    format!("wa:{}", URL_SAFE_NO_PAD.encode(digest.finalize()))
}

fn build_inbound_attachments(message: &wa::Message) -> Vec<InboundAttachment> {
    let base_message = message.get_base_message();
    let mut attachments = Vec::new();

    if let Some(image) = base_message.image_message.as_option() {
        attachments.push(build_image_attachment(image));
    }
    if let Some(video) = base_message.video_message.as_option() {
        attachments.push(build_video_attachment(video));
    }
    if let Some(audio) = base_message.audio_message.as_option() {
        attachments.push(build_audio_attachment(audio));
    }
    if let Some(document) = base_message.document_message.as_option() {
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

    const SENDER_ID: &str = "15557654321@s.whatsapp.net";
    const ACCOUNT_ID: &str = "15551234567@s.whatsapp.net";

    fn policy() -> WhatsAppPolicy {
        WhatsAppPolicy::from_config(&ChannelConfig {
            dm_policy: Some("allowlist".to_string()),
            allowed_dm_sender_ids: vec![SENDER_ID.to_string()],
            ..ChannelConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn provider_message_identity_includes_chat_and_sender() {
        let first = inbound_info();
        let mut other_chat = first.clone();
        other_chat.source.chat = "15550000000@s.whatsapp.net".parse().unwrap();
        let mut other_sender = first.clone();
        other_sender.source.sender = "15559999999@s.whatsapp.net".parse().unwrap();
        assert_eq!(inbound_event_id(&first), inbound_event_id(&first.clone()));
        assert_ne!(inbound_event_id(&first), inbound_event_id(&other_chat));
        assert_ne!(inbound_event_id(&first), inbound_event_id(&other_sender));
    }

    #[test]
    fn identical_library_input_rechunks_to_identical_originals() {
        let message = wa::Message {
            conversation: Some("hello".to_string()),
            ..Default::default()
        };
        let infos: Vec<_> = (0..400)
            .map(|index| {
                let mut info = inbound_info();
                info.id = format!("wamid.{index}");
                info.ephemeral_expiration = Some(3600);
                info
            })
            .collect();
        let chunks = inbound_event_chunks(
            infos.iter().map(|info| (&message, info)),
            &policy(),
            ACCOUNT_ID,
        )
        .unwrap();
        assert_eq!(chunks.len(), 13);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MAX_CHANNEL_BATCH_EVENTS)
        );
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), 400);
        for chunk in &chunks {
            let batch = ChannelEventBatch {
                events: chunk.clone(),
                state: None,
                poll_after_ms: None,
            };
            assert!(dispatch_channel_protocol::ChannelEventRequest::new(batch, None).is_ok());
        }
        // Fields the library derives only on first dispatch do not reach the
        // event, so a redelivery re-parsed from the stanza maps identically.
        let redelivered: Vec<_> = infos
            .iter()
            .cloned()
            .map(|mut info| {
                info.is_offline = true;
                info.ephemeral_expiration = None;
                info
            })
            .collect();
        let replayed = inbound_event_chunks(
            redelivered.iter().map(|info| (&message, info)),
            &policy(),
            ACCOUNT_ID,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&replayed).unwrap(),
            serde_json::to_vec(&chunks).unwrap()
        );
    }

    #[test]
    fn partial_chunk_acceptance_requires_exact_replay_of_all_provider_messages() {
        let message = wa::Message {
            conversation: Some("hello".to_string()),
            ..Default::default()
        };
        let infos: Vec<_> = (0..400)
            .map(|index| {
                let mut info = inbound_info();
                info.id = format!("wamid.{index}");
                info
            })
            .collect();
        let original = inbound_event_chunks(
            infos.iter().map(|info| (&message, info)),
            &policy(),
            ACCOUNT_ID,
        )
        .unwrap();
        let mut accepted = Vec::new();
        let failure = accept_chunks(original.clone(), |batch| {
            if accepted.len() == 5 {
                return Err(anyhow!("host refused sixth chunk"));
            }
            accepted.push(serde_json::to_vec(&batch).unwrap());
            Ok(())
        });
        assert!(failure.unwrap_err().to_string().contains("sixth chunk"));
        assert_eq!(accepted.len(), 5);

        let replayed = inbound_event_chunks(
            infos.iter().map(|info| (&message, info)),
            &policy(),
            ACCOUNT_ID,
        )
        .unwrap();
        let mut replay_frames = Vec::new();
        accept_chunks(replayed, |batch| {
            replay_frames.push(serde_json::to_vec(&batch).unwrap());
            Ok(())
        })
        .unwrap();
        assert_eq!(replay_frames.len(), 13);
        assert_eq!(&replay_frames[..5], accepted.as_slice());
        assert_eq!(
            replay_frames
                .iter()
                .map(|frame| serde_json::from_slice::<ChannelEventBatch>(frame)
                    .unwrap()
                    .events
                    .len())
                .sum::<usize>(),
            400
        );
    }

    #[test]
    fn redelivered_push_name_does_not_change_the_event() {
        // A sender can rename their profile between provider redeliveries.
        let message = wa::Message {
            conversation: Some("hello".to_string()),
            ..Default::default()
        };
        let first = inbound_info();
        let mut renamed = first.clone();
        renamed.push_name = "Renamed".to_string();
        let original =
            build_inbound_event_from_message(&message, &first, &policy(), ACCOUNT_ID).unwrap();
        let replayed =
            build_inbound_event_from_message(&message, &renamed, &policy(), ACCOUNT_ID).unwrap();
        assert_eq!(original.event_id, replayed.event_id);
        assert_eq!(
            serde_json::to_vec(&original).unwrap(),
            serde_json::to_vec(&replayed).unwrap()
        );
    }

    #[test]
    fn a_repeated_provider_identity_in_one_batch_is_delivered_once() {
        let message = wa::Message {
            conversation: Some("hello".to_string()),
            ..Default::default()
        };
        let info = inbound_info();
        let chunks = inbound_event_chunks(
            [(&message, &info), (&message, &info)],
            &policy(),
            ACCOUNT_ID,
        )
        .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1);
    }

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
                sender: SENDER_ID.parse().unwrap(),
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
    fn self_chat_accepts_other_devices_in_pn_and_lid_form_only() {
        let summary = crate::session::RegistrationSummary {
            phone_jid: Some("15551234567:7@s.whatsapp.net".to_string()),
            lid_jid: Some("987654321:7@lid".to_string()),
            // The store row ID is independent of the companion JID device.
            device_id: 1,
            push_name: None,
        };
        let policy = WhatsAppPolicy::for_self_chat(
            &ChannelConfig {
                self_chat_only: true,
                dm_policy: Some("open".to_string()),
                ..ChannelConfig::default()
            },
            &summary,
        )
        .unwrap();
        let message = wa::Message {
            conversation: Some("hello".to_string()),
            ..Default::default()
        };
        for chat in [ACCOUNT_ID, "987654321@lid"] {
            let mut info = inbound_info();
            info.source.chat = chat.parse().unwrap();
            info.source.is_from_me = true;
            for device in [0, 1, 3] {
                info.source.sender = info.source.chat.with_device(device);
                let event =
                    build_inbound_event_from_message(&message, &info, &policy, ACCOUNT_ID).unwrap();
                assert_eq!(event.conversation.id, chat);
                assert_eq!(event.actor.id, chat);
            }
            info.source.sender = info.source.chat.with_device(7);
            assert!(
                build_inbound_event_from_message(&message, &info, &policy, ACCOUNT_ID).is_none()
            );
            info.source.sender = info.source.chat.with_device(0);
            info.source.chat = SENDER_ID.parse().unwrap();
            assert!(
                build_inbound_event_from_message(&message, &info, &policy, ACCOUNT_ID).is_none()
            );
            info.source.chat = chat.parse().unwrap();
            info.source.sender = info.source.chat.clone();
            info.source.is_from_me = false;
            assert!(
                build_inbound_event_from_message(&message, &info, &policy, ACCOUNT_ID).is_none()
            );
            info.source.sender = SENDER_ID.parse().unwrap();
            info.source.is_from_me = true;
            assert!(
                build_inbound_event_from_message(&message, &info, &policy, ACCOUNT_ID).is_none()
            );
            info.source.sender = info.source.chat.clone();
            info.source.is_from_me = true;
            info.source.is_group = true;
            assert!(
                build_inbound_event_from_message(&message, &info, &policy, ACCOUNT_ID).is_none()
            );
        }
    }

    #[test]
    fn build_inbound_event_keeps_attachment_only_document_messages() {
        let message = wa::Message {
            document_message: wa::message::DocumentMessage {
                file_name: Some("report.pdf".to_string()),
                mimetype: Some("application/pdf".to_string()),
                file_length: Some(42),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };

        let event =
            build_inbound_event_from_message(&message, &inbound_info(), &policy(), ACCOUNT_ID)
                .unwrap();
        assert_eq!(event.event_type, "message.received");
        assert_eq!(event.platform, "whatsapp");
        assert_eq!(event.conversation.kind, "dm");
        assert_eq!(event.account_id.as_deref(), Some(ACCOUNT_ID));
        assert_eq!(
            event
                .activation
                .as_ref()
                .map(|activation| activation.reason.as_str()),
            Some(InboundActivation::REASON_DIRECT_MESSAGE)
        );
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
            image_message: wa::message::ImageMessage {
                mimetype: Some("image/png".to_string()),
                caption: Some("look".to_string()),
                file_length: Some(128),
                width: Some(640),
                height: Some(480),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };

        let event =
            build_inbound_event_from_message(&message, &inbound_info(), &policy(), ACCOUNT_ID)
                .unwrap();
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

    #[test]
    fn build_inbound_event_rejects_groups_and_senders_outside_the_allowlist() {
        let message = wa::Message {
            conversation: Some("hello".to_string()),
            ..Default::default()
        };
        let mut group = inbound_info();
        group.source.is_group = true;
        assert!(
            build_inbound_event_from_message(&message, &group, &policy(), ACCOUNT_ID).is_none()
        );

        let mut unlisted = inbound_info();
        unlisted.source.sender = "15559876543@s.whatsapp.net".parse().unwrap();
        assert!(
            build_inbound_event_from_message(&message, &unlisted, &policy(), ACCOUNT_ID).is_none()
        );
    }
}
