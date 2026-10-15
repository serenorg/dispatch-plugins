use dispatch_channel_protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelEventBatch, ChannelHeartbeat, InboundEventEnvelope,
    IngressCallbackReply, IngressState, PluginInput, PluginNotificationEnvelope, PluginRequest,
    PluginRequestEnvelope, PluginRequestId, PluginResponse, heartbeat_to_jsonrpc,
};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

mod event_delivery;
mod journal_fs;
pub use event_delivery::{
    close_event_input, configure_event_storage, deliver_channel_event_batch,
    deliver_channel_event_batch_until, replay_pending_event_batch,
    replay_pending_event_batch_until, route_event_acceptance,
};
pub use journal_fs::create_event_journal_directory;

const MAX_CONSECUTIVE_RECEIVE_FAILURES: u32 = 5;
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(8);
const RECONNECT_STABILITY_WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("stdout lock poisoned")]
    StdoutLockPoisoned,
    #[error("channel event journal or receipt failed: {0}")]
    Journal(String),
    #[error("channel plugin stdin protocol failed: {0}")]
    Input(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct IngressWorker {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Option<IngressState>>>,
    handle: JoinHandle<()>,
}

#[derive(Debug, PartialEq, Eq)]
enum IngressWorkerExit {
    Shutdown,
    Terminal(IngressTerminalFailure),
}

#[derive(Debug, PartialEq, Eq)]
struct IngressTerminalFailure {
    reason: &'static str,
    retryable: bool,
    retry_count: u32,
    last_successful_receive_unix_ms: Option<u128>,
    message: String,
}

#[derive(Clone)]
pub struct StopSignal(Arc<AtomicBool>);

impl StopSignal {
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn sleep_until_stopped(&self, total: Duration) {
        let mut slept = Duration::ZERO;
        while slept < total && !self.is_stopped() {
            let step = std::cmp::min(Duration::from_millis(250), total - slept);
            thread::sleep(step);
            slept += step;
        }
    }
}

pub fn no_after_cycle<C>(_: &C, _: &StopSignal) {}

/// Plugin-owned ingress result. Event content never appears in a control response.
#[derive(Debug)]
pub enum IngressCycle {
    Events {
        events: Vec<InboundEventEnvelope>,
        callback_reply: Option<IngressCallbackReply>,
        state: Option<IngressState>,
        poll_after_ms: Option<u64>,
    },
    Control(PluginResponse),
}

impl IngressCycle {
    /// Deliver the batch durably, then build the content-free control response.
    pub fn publish(self, stdout_lock: &Arc<Mutex<()>>) -> Result<PluginResponse, RuntimeError> {
        match self {
            Self::Events {
                events,
                callback_reply,
                state,
                poll_after_ms,
            } => {
                deliver_channel_event_batch(
                    stdout_lock,
                    ChannelEventBatch {
                        events,
                        state: state.clone(),
                        poll_after_ms,
                    },
                )?;
                Ok(PluginResponse::IngressEventsReceived {
                    callback_reply,
                    state,
                    poll_after_ms,
                })
            }
            Self::Control(response) => Ok(response),
        }
    }
}

impl From<PluginResponse> for IngressCycle {
    fn from(response: PluginResponse) -> Self {
        Self::Control(response)
    }
}

/// One host request from plugin stdin, or the error that ended input.
type InputRequest<C, M> =
    Result<(PluginRequestId, PluginRequestEnvelope<PluginRequest<C, M>>), RuntimeError>;

/// Read plugin stdin on its own thread so receipts reach a waiting batch while
/// the request loop is busy. Host ingress requests configure the journal and
/// take the host's accepted cursor; receipts never reach the request loop.
pub fn spawn_plugin_input_reader<C, M, E, Parse>(
    stdout_lock: Arc<Mutex<()>>,
    parse: Parse,
) -> mpsc::Receiver<InputRequest<C, M>>
where
    C: Send + 'static,
    M: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    Parse: Fn(&str) -> Result<PluginInput<C, M>, E> + Send + 'static,
{
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let input = match line {
                Ok(line) if line.trim().is_empty() => continue,
                Ok(line) => parse(&line).map_err(|error| RuntimeError::Input(error.to_string())),
                Err(error) => Err(RuntimeError::Io(error)),
            };
            let outcome = match input {
                Ok(PluginInput::Request { id, mut envelope }) => {
                    configure_ingress_request(&stdout_lock, &mut envelope.request)
                        .map(|()| Some((id, *envelope)))
                }
                Ok(PluginInput::EventAcceptance { id, acceptance }) => {
                    route_event_acceptance(&stdout_lock, id, acceptance).map(|()| None)
                }
                Err(error) => Err(error),
            };
            match outcome {
                Ok(Some(request)) => {
                    if send.send(Ok(request)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = send.send(Err(error));
                    break;
                }
            }
        }
        close_event_input(&stdout_lock);
    });
    receive
}

fn configure_ingress_request<C, M>(
    stdout_lock: &Arc<Mutex<()>>,
    request: &mut PluginRequest<C, M>,
) -> Result<(), RuntimeError> {
    let (PluginRequest::StartIngress { storage, state, .. }
    | PluginRequest::PollIngress { storage, state, .. }
    | PluginRequest::IngressEvent { storage, state, .. }) = request
    else {
        return Ok(());
    };
    configure_event_storage(stdout_lock, storage)?;
    // The authenticated host head owns the cursor, including an inherited one.
    if let Some(recovery) = &storage.recovery {
        *state = recovery.state.clone();
    }
    Ok(())
}

/// Provides shutdown-aware waits and in-cycle durable delivery to poll callbacks.
pub struct IngressPollContext<'a> {
    stop: &'a StopSignal,
    emit: &'a dyn Fn(ChannelEventBatch) -> Result<(), RuntimeError>,
    /// State of the latest batch confirmed during this cycle.
    delivered_state: &'a Mutex<Option<Option<IngressState>>>,
}

impl IngressPollContext<'_> {
    pub fn is_stopped(&self) -> bool {
        self.stop.is_stopped()
    }

    /// Wait until the duration elapses or the host requests shutdown.
    pub fn sleep_until_stopped(&self, total: Duration) {
        self.stop.sleep_until_stopped(total);
    }

    /// Persist and confirm events with the host before the poll returns.
    pub fn deliver(
        &self,
        events: Vec<InboundEventEnvelope>,
        state: Option<IngressState>,
        poll_after_ms: Option<u64>,
    ) -> Result<(), RuntimeError> {
        (self.emit)(ChannelEventBatch {
            events,
            state: state.clone(),
            poll_after_ms,
        })?;
        *self
            .delivered_state
            .lock()
            .expect("channel ingress delivery state poisoned") = Some(state);
        Ok(())
    }
}

pub fn restart_ingress_worker<C, Poll, After, E>(
    worker: &mut Option<IngressWorker>,
    config: C,
    initial_state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
    plugin_label: &'static str,
    poll: Poll,
    after_cycle: After,
) where
    C: Send + 'static,
    Poll: Fn(&C, Option<IngressState>, &IngressPollContext<'_>) -> Result<IngressCycle, E>
        + Send
        + 'static,
    After: Fn(&C, &StopSignal) + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let _ = stop_ingress_worker(worker);
    *worker = Some(spawn_ingress_worker(
        config,
        initial_state,
        stdout_lock,
        plugin_label,
        poll,
        after_cycle,
    ));
}

pub fn stop_ingress_worker(worker: &mut Option<IngressWorker>) -> Option<IngressState> {
    let worker = worker.take()?;
    worker.stop.store(true, Ordering::Relaxed);
    let _ = worker.handle.join();
    worker.state.lock().ok().and_then(|state| (*state).clone())
}

pub fn write_stdout_line(stdout_lock: &Arc<Mutex<()>>, line: &str) -> Result<(), RuntimeError> {
    let _guard = stdout_lock
        .lock()
        .map_err(|_| RuntimeError::StdoutLockPoisoned)?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

pub fn publish_channel_heartbeat(stdout_lock: &Arc<Mutex<()>>) -> Result<(), RuntimeError> {
    let envelope = PluginNotificationEnvelope {
        protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
        notification: ChannelHeartbeat {
            metadata: BTreeMap::new(),
        },
    };
    let json =
        heartbeat_to_jsonrpc(&envelope).map_err(|error| RuntimeError::Input(error.to_string()))?;
    write_stdout_line(stdout_lock, &json)
}

fn spawn_ingress_worker<C, Poll, After, E>(
    config: C,
    initial_state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
    plugin_label: &'static str,
    poll: Poll,
    after_cycle: After,
) -> IngressWorker
where
    C: Send + 'static,
    Poll: Fn(&C, Option<IngressState>, &IngressPollContext<'_>) -> Result<IngressCycle, E>
        + Send
        + 'static,
    After: Fn(&C, &StopSignal) + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(Some(initial_state)));
    let stop_signal = StopSignal(Arc::clone(&stop));
    let shared_state = Arc::clone(&state);
    let handle = thread::spawn(move || {
        let stopped = || stop_signal.is_stopped();
        if let Err(error) = replay_pending_event_batch_until(&stdout_lock, &stopped) {
            if stop_signal.is_stopped() {
                return;
            }
            eprintln!("{plugin_label} ingress replay failed: {error}");
            std::process::exit(1);
        }
        let result = run_ingress_worker_loop(
            &config,
            &shared_state,
            &stop_signal,
            &poll,
            &after_cycle,
            |batch| deliver_channel_event_batch_until(&stdout_lock, batch, &stopped),
        );
        if let IngressWorkerExit::Terminal(failure) = result {
            eprintln!(
                "{plugin_label} ingress worker terminated: reason={} retryable={} retry_count={} last_successful_receive_unix_ms={} message={}",
                failure.reason,
                failure.retryable,
                failure.retry_count,
                failure
                    .last_successful_receive_unix_ms
                    .map_or_else(|| "none".to_string(), |value| value.to_string()),
                failure.message
            );
            // Exit so host supervision observes terminal worker loss.
            std::process::exit(1);
        }
    });

    IngressWorker {
        stop,
        state,
        handle,
    }
}

fn run_ingress_worker_loop<C, Poll, After, E, Emit>(
    config: &C,
    shared_state: &Arc<Mutex<Option<IngressState>>>,
    stop_signal: &StopSignal,
    poll: &Poll,
    after_cycle: &After,
    emit: Emit,
) -> IngressWorkerExit
where
    Poll: Fn(&C, Option<IngressState>, &IngressPollContext<'_>) -> Result<IngressCycle, E>,
    After: Fn(&C, &StopSignal),
    E: std::fmt::Display,
    Emit: Fn(ChannelEventBatch) -> Result<(), RuntimeError>,
{
    let delivered_state = Mutex::new(None);
    let context = IngressPollContext {
        stop: stop_signal,
        emit: &emit,
        delivered_state: &delivered_state,
    };
    let mut consecutive_failures = 0;
    let mut last_successful_receive_unix_ms = None;
    let mut last_receive_failure_at = None;
    while !stop_signal.is_stopped() {
        *delivered_state
            .lock()
            .expect("channel ingress delivery state poisoned") = None;
        let prior_state = shared_state
            .lock()
            .expect("channel ingress state poisoned")
            .clone();
        match poll(config, prior_state, &context) {
            Ok(IngressCycle::Events {
                events,
                state,
                poll_after_ms,
                ..
            }) => {
                last_successful_receive_unix_ms = Some(unix_time_millis());
                // The cursor advances only with a confirmed batch; an in-cycle
                // delivery covers the result only when it carried the same state.
                let already_confirmed = events.is_empty()
                    && delivered_state
                        .lock()
                        .expect("channel ingress delivery state poisoned")
                        .as_ref()
                        == Some(&state);
                if !already_confirmed
                    && let Err(error) = emit(ChannelEventBatch {
                        events,
                        state: state.clone(),
                        poll_after_ms,
                    })
                {
                    if stop_signal.is_stopped() {
                        return IngressWorkerExit::Shutdown;
                    }
                    return IngressWorkerExit::Terminal(IngressTerminalFailure {
                        reason: "event_delivery",
                        retryable: false,
                        retry_count: 0,
                        last_successful_receive_unix_ms,
                        message: error.to_string(),
                    });
                }
                // A state-free result keeps the cursor confirmed earlier in the cycle.
                let confirmed_state = state.or_else(|| {
                    delivered_state
                        .lock()
                        .expect("channel ingress delivery state poisoned")
                        .clone()
                        .flatten()
                });
                if let Some(next_state) = confirmed_state {
                    *shared_state.lock().expect("channel ingress state poisoned") =
                        Some(next_state);
                }
                if stop_signal.is_stopped() {
                    return IngressWorkerExit::Shutdown;
                }
                after_cycle(config, stop_signal);
                if last_receive_failure_at.is_some_and(|failed_at: Instant| {
                    failed_at.elapsed() >= RECONNECT_STABILITY_WINDOW
                }) {
                    consecutive_failures = 0;
                    last_receive_failure_at = None;
                }
            }
            Ok(IngressCycle::Control(PluginResponse::Error { error })) => {
                if stop_signal.is_stopped() {
                    return IngressWorkerExit::Shutdown;
                }
                return IngressWorkerExit::Terminal(IngressTerminalFailure {
                    reason: classify_plugin_error(&error.code),
                    retryable: false,
                    retry_count: 0,
                    last_successful_receive_unix_ms,
                    message: format!("{}: {}", error.code, error.message),
                });
            }
            Ok(other) => {
                if stop_signal.is_stopped() {
                    return IngressWorkerExit::Shutdown;
                }
                return IngressWorkerExit::Terminal(IngressTerminalFailure {
                    reason: "protocol",
                    retryable: false,
                    retry_count: 0,
                    last_successful_receive_unix_ms,
                    message: format!("unexpected response variant: {other:?}"),
                });
            }
            Err(error) => {
                if stop_signal.is_stopped() {
                    return IngressWorkerExit::Shutdown;
                }
                consecutive_failures += 1;
                last_receive_failure_at = Some(Instant::now());
                if consecutive_failures >= MAX_CONSECUTIVE_RECEIVE_FAILURES {
                    return IngressWorkerExit::Terminal(IngressTerminalFailure {
                        reason: "transport",
                        retryable: true,
                        retry_count: consecutive_failures,
                        last_successful_receive_unix_ms,
                        message: error.to_string(),
                    });
                }
                let backoff = reconnect_backoff(consecutive_failures);
                eprintln!(
                    "channel ingress receive failed; reconnecting: reason=transport retryable=true retry_count={} backoff_ms={} message={error}",
                    consecutive_failures,
                    backoff.as_millis()
                );
                stop_signal.sleep_until_stopped(backoff);
            }
        }
    }
    IngressWorkerExit::Shutdown
}

fn classify_plugin_error(code: &str) -> &'static str {
    let code = code.to_ascii_lowercase();
    if code.contains("event_delivery") {
        "event_delivery"
    } else if ["auth", "credential", "permission", "token", "unauthorized"]
        .iter()
        .any(|marker| code.contains(marker))
    {
        "auth"
    } else {
        "protocol"
    }
}

fn reconnect_backoff(retry_count: u32) -> Duration {
    let exponent = retry_count.saturating_sub(1).min(8);
    let base = INITIAL_RECONNECT_BACKOFF
        .saturating_mul(1_u32 << exponent)
        .min(MAX_RECONNECT_BACKOFF);
    let jitter_bound_ms = (base.as_millis() / 4).max(1) as u64;
    base.saturating_add(Duration::from_millis(jitter_seed() % jitter_bound_ms))
}

/// Mix per-process and per-attempt inputs into reconnect jitter.
fn jitter_seed() -> u64 {
    use std::sync::atomic::AtomicU64;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = unix_time_millis() as u64
        ^ (u64::from(std::process::id()).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        ^ sequence.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    // Finalize with SplitMix64 so adjacent seeds diverge.
    seed ^= seed >> 30;
    seed = seed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= seed >> 27;
    seed = seed.wrapping_mul(0x94d0_49bb_1331_11eb);
    seed ^ (seed >> 31)
}

fn unix_time_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::{
        IngressCycle, IngressPollContext, IngressWorkerExit, StopSignal, classify_plugin_error,
        run_ingress_worker_loop,
    };
    use dispatch_channel_protocol::{
        InboundActor, InboundConversationRef, InboundEventEnvelope, InboundMessage, IngressMode,
        IngressState, PluginResponse,
    };
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn event_delivery_errors_keep_their_terminal_reason() {
        assert_eq!(
            classify_plugin_error("event_delivery_failed"),
            "event_delivery"
        );
    }

    fn test_event() -> InboundEventEnvelope {
        InboundEventEnvelope {
            event_id: "event-1".to_string(),
            platform: "test".to_string(),
            event_type: "message.received".to_string(),
            received_at: "2026-08-04T00:00:00Z".to_string(),
            conversation: InboundConversationRef {
                id: "conversation-1".to_string(),
                kind: "channel".to_string(),
                thread_id: None,
                parent_message_id: None,
                workspace_id: None,
                parent_conversation_id: None,
            },
            actor: InboundActor {
                id: "actor-1".to_string(),
                display_name: None,
                username: None,
                is_bot: false,
                metadata: BTreeMap::new(),
            },
            message: InboundMessage {
                id: "message-1".to_string(),
                content: "hello".to_string(),
                content_type: "text/plain".to_string(),
                reply_to_message_id: None,
                attachments: Vec::new(),
                metadata: BTreeMap::new(),
            },
            account_id: None,
            activation: None,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn retryable_receive_failure_recovers_and_emits_event() {
        let calls = AtomicU32::new(0);
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = StopSignal(Arc::clone(&stop));
        let state = Arc::new(Mutex::new(Some(IngressState {
            mode: IngressMode::Polling,
            status: "starting".to_string(),
            endpoint: None,
            metadata: BTreeMap::new(),
        })));
        let emitted_for_callback = Arc::clone(&emitted);

        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &|_, _, _| {
                if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err("temporary transport failure")
                } else {
                    Ok(IngressCycle::Events {
                        events: vec![test_event()],
                        callback_reply: None,
                        state: None,
                        poll_after_ms: None,
                    })
                }
            },
            &|_, signal| signal.0.store(true, Ordering::Relaxed),
            move |batch| {
                emitted_for_callback
                    .lock()
                    .expect("emitted batches lock")
                    .push(batch);
                Ok(())
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        let batches = emitted.lock().expect("emitted batches lock");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].events[0].event_id, "event-1");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn poll_context_delivery_reaches_the_host_inside_the_cycle() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = StopSignal(Arc::clone(&stop));
        let state = Arc::new(Mutex::new(None));
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let emitted_for_callback = Arc::clone(&emitted);
        let stop_during_poll = Arc::clone(&stop);
        let delivered_before_return = Arc::new(AtomicBool::new(false));
        let delivered_flag = Arc::clone(&delivered_before_return);
        let observed = Arc::clone(&emitted);

        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &move |_, _, context: &IngressPollContext<'_>| -> Result<IngressCycle, &str> {
                context
                    .deliver(vec![test_event()], None, Some(1000))
                    .expect("delivery should succeed");
                delivered_flag.store(
                    observed.lock().expect("emitted batches lock").len() == 1,
                    Ordering::Relaxed,
                );
                stop_during_poll.store(true, Ordering::Relaxed);
                Ok(IngressCycle::Events {
                    events: Vec::new(),
                    callback_reply: None,
                    state: None,
                    poll_after_ms: Some(1000),
                })
            },
            &|_, _| {},
            move |batch| {
                emitted_for_callback
                    .lock()
                    .expect("emitted batches lock")
                    .push(batch);
                Ok(())
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        assert!(
            delivered_before_return.load(Ordering::Relaxed),
            "the event must be confirmed before the poll returns"
        );
        let batches = emitted.lock().expect("emitted batches lock");
        assert_eq!(
            batches.len(),
            1,
            "an unchanged cycle result is already confirmed"
        );
        assert_eq!(batches[0].events[0].event_id, "event-1");
    }

    #[test]
    fn cursor_advances_only_after_its_own_batch_is_confirmed() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = StopSignal(Arc::clone(&stop));
        let state = Arc::new(Mutex::new(None));
        let cursor = |value: &str| IngressState {
            mode: IngressMode::Polling,
            status: "running".to_string(),
            endpoint: None,
            metadata: BTreeMap::from([("cursor".to_string(), value.to_string())]),
        };
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let emitted_for_callback = Arc::clone(&emitted);
        let stop_during_poll = Arc::clone(&stop);
        let delivered_cursor = cursor("1");
        let returned_cursor = cursor("2");

        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &move |_, _, context: &IngressPollContext<'_>| -> Result<IngressCycle, &str> {
                context
                    .deliver(vec![test_event()], Some(delivered_cursor.clone()), None)
                    .expect("delivery should succeed");
                stop_during_poll.store(true, Ordering::Relaxed);
                Ok(IngressCycle::Events {
                    events: Vec::new(),
                    callback_reply: None,
                    state: Some(returned_cursor.clone()),
                    poll_after_ms: None,
                })
            },
            &|_, _| {},
            move |batch| {
                emitted_for_callback
                    .lock()
                    .expect("emitted batches lock")
                    .push(batch);
                Ok(())
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        let batches = emitted.lock().expect("emitted batches lock");
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[1].state, Some(cursor("2")));
        assert_eq!(*state.lock().unwrap(), Some(cursor("2")));
    }

    #[test]
    fn failed_delivery_keeps_the_previous_cursor_and_stops_quietly_on_shutdown() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = StopSignal(Arc::clone(&stop));
        let previous = IngressState {
            mode: IngressMode::Polling,
            status: "running".to_string(),
            endpoint: None,
            metadata: BTreeMap::from([("cursor".to_string(), "1".to_string())]),
        };
        let state = Arc::new(Mutex::new(Some(previous.clone())));
        let mut next = previous.clone();
        next.metadata.insert("cursor".to_string(), "2".to_string());
        let stop_during_delivery = Arc::clone(&stop);

        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &move |_, _, _| -> Result<IngressCycle, &str> {
                Ok(IngressCycle::Events {
                    events: vec![test_event()],
                    callback_reply: None,
                    state: Some(next.clone()),
                    poll_after_ms: None,
                })
            },
            &|_, _| {},
            move |_| {
                stop_during_delivery.store(true, Ordering::Relaxed);
                Err(super::RuntimeError::Journal(
                    "event acceptance wait stopped".to_string(),
                ))
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        assert_eq!(*state.lock().unwrap(), Some(previous));
    }

    #[test]
    fn poll_context_sleep_ends_when_the_worker_is_asked_to_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = StopSignal(Arc::clone(&stop));
        let state = Arc::new(Mutex::new(None));
        let stop_from_another_thread = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            stop_from_another_thread.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &|_, _, context: &IngressPollContext<'_>| -> Result<IngressCycle, &str> {
                context.sleep_until_stopped(Duration::from_secs(60));
                Err("rate limited")
            },
            &|_, _| {},
            |_| Ok(()),
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown must interrupt the rate-limit wait"
        );
    }

    #[test]
    fn terminal_plugin_error_never_leaves_an_idle_worker() {
        let stop_signal = StopSignal(Arc::new(AtomicBool::new(false)));
        let state = Arc::new(Mutex::new(None));
        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &|_, _, _| -> Result<IngressCycle, &str> {
                Ok(IngressCycle::Control(PluginResponse::Error {
                    error: dispatch_channel_protocol::PluginErrorPayload {
                        code: "invalid_auth".to_string(),
                        message: "credentials rejected".to_string(),
                    },
                }))
            },
            &|_, _| {},
            |_| Ok(()),
        );

        let IngressWorkerExit::Terminal(failure) = result else {
            panic!("terminal plugin errors must stop the worker");
        };
        assert_eq!(failure.reason, "auth");
        assert!(!failure.retryable);
    }

    #[test]
    fn shutdown_during_poll_does_not_force_a_terminal_exit() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = StopSignal(Arc::clone(&stop));
        let state = Arc::new(Mutex::new(None));
        let stop_during_poll = Arc::clone(&stop);
        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &move |_, _, _| -> Result<IngressCycle, &str> {
                stop_during_poll.store(true, Ordering::Relaxed);
                Ok(IngressCycle::Control(PluginResponse::Error {
                    error: dispatch_channel_protocol::PluginErrorPayload {
                        code: "transport_closed".to_string(),
                        message: "poll interrupted by shutdown".to_string(),
                    },
                }))
            },
            &|_, _| {},
            |_| Ok(()),
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
    }

    #[test]
    fn event_accepted_before_shutdown_is_emitted() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = StopSignal(Arc::clone(&stop));
        let state = Arc::new(Mutex::new(None));
        let stop_during_poll = Arc::clone(&stop);
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let emitted_for_callback = Arc::clone(&emitted);
        let result = run_ingress_worker_loop(
            &(),
            &state,
            &stop_signal,
            &move |_, _, _| -> Result<IngressCycle, &str> {
                stop_during_poll.store(true, Ordering::Relaxed);
                Ok(IngressCycle::Events {
                    events: vec![test_event()],
                    callback_reply: None,
                    state: None,
                    poll_after_ms: None,
                })
            },
            &|_, _| panic!("after-cycle hook must not run after shutdown"),
            move |batch| {
                emitted_for_callback
                    .lock()
                    .expect("emitted batches lock")
                    .push(batch);
                Ok(())
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        let batches = emitted.lock().expect("emitted batches lock");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].events[0].event_id, "event-1");
    }
}
