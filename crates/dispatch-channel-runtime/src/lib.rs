use dispatch_channel_protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelEventNotification, InboundEventEnvelope, IngressState,
    JsonRpcMessageError, PluginNotificationEnvelope, PluginResponse, notification_to_jsonrpc,
};
use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const MAX_CONSECUTIVE_RECEIVE_FAILURES: u32 = 5;
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(8);
const RECONNECT_STABILITY_WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("stdout lock poisoned")]
    StdoutLockPoisoned,
    #[error("failed to encode channel event notification: {0}")]
    NotificationEncode(JsonRpcMessageError),
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

/// Provides shutdown-aware waits and in-cycle stdout delivery to poll callbacks.
pub struct IngressPollContext<'a> {
    stop: &'a StopSignal,
    emit: &'a dyn Fn(ChannelEventNotification) -> Result<(), RuntimeError>,
}

impl IngressPollContext<'_> {
    pub fn is_stopped(&self) -> bool {
        self.stop.is_stopped()
    }

    /// Wait until the duration elapses or the host requests shutdown.
    pub fn sleep_until_stopped(&self, total: Duration) {
        self.stop.sleep_until_stopped(total);
    }

    /// Serialize and flush events to plugin stdout before the poll returns.
    pub fn deliver(
        &self,
        events: Vec<InboundEventEnvelope>,
        state: Option<IngressState>,
        poll_after_ms: Option<u64>,
    ) -> Result<(), RuntimeError> {
        (self.emit)(ChannelEventNotification {
            events,
            state,
            poll_after_ms,
        })
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
    Poll: Fn(&C, Option<IngressState>, &IngressPollContext<'_>) -> Result<PluginResponse, E>
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
    Poll: Fn(&C, Option<IngressState>, &IngressPollContext<'_>) -> Result<PluginResponse, E>
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
        let result = run_ingress_worker_loop(
            &config,
            &shared_state,
            &stop_signal,
            &poll,
            &after_cycle,
            |notification| emit_channel_event_notification(&stdout_lock, notification),
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
    Poll: Fn(&C, Option<IngressState>, &IngressPollContext<'_>) -> Result<PluginResponse, E>,
    After: Fn(&C, &StopSignal),
    E: std::fmt::Display,
    Emit: Fn(ChannelEventNotification) -> Result<(), RuntimeError>,
{
    let context = IngressPollContext {
        stop: stop_signal,
        emit: &emit,
    };
    let mut consecutive_failures = 0;
    let mut last_successful_receive_unix_ms = None;
    let mut last_receive_failure_at = None;
    while !stop_signal.is_stopped() {
        let prior_state = shared_state
            .lock()
            .expect("channel ingress state poisoned")
            .clone();
        match poll(config, prior_state, &context) {
            Ok(PluginResponse::IngressEventsReceived {
                events,
                state,
                poll_after_ms,
                ..
            }) => {
                last_successful_receive_unix_ms = Some(unix_time_millis());
                if let Some(next_state) = state.clone() {
                    *shared_state.lock().expect("channel ingress state poisoned") =
                        Some(next_state);
                }
                if let Err(error) = emit(ChannelEventNotification {
                    events,
                    state,
                    poll_after_ms,
                }) {
                    return IngressWorkerExit::Terminal(IngressTerminalFailure {
                        reason: "notification_delivery",
                        retryable: false,
                        retry_count: 0,
                        last_successful_receive_unix_ms,
                        message: error.to_string(),
                    });
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
            Ok(PluginResponse::Error { error }) => {
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
    if code.contains("notification_delivery") {
        "notification_delivery"
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

fn emit_channel_event_notification(
    stdout_lock: &Arc<Mutex<()>>,
    notification: ChannelEventNotification,
) -> Result<(), RuntimeError> {
    let envelope = PluginNotificationEnvelope {
        protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
        notification,
    };
    let json = notification_to_jsonrpc(&envelope).map_err(RuntimeError::NotificationEncode)?;
    write_stdout_line(stdout_lock, &json)
}

#[cfg(test)]
mod tests {
    use super::{
        IngressPollContext, IngressWorkerExit, StopSignal, classify_plugin_error,
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
    fn notification_delivery_errors_keep_their_terminal_reason() {
        assert_eq!(
            classify_plugin_error("notification_delivery_failed"),
            "notification_delivery"
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
                    Ok(PluginResponse::IngressEventsReceived {
                        events: vec![test_event()],
                        callback_reply: None,
                        state: None,
                        poll_after_ms: None,
                    })
                }
            },
            &|_, signal| signal.0.store(true, Ordering::Relaxed),
            move |notification| {
                emitted_for_callback
                    .lock()
                    .expect("emitted notifications lock")
                    .push(notification);
                Ok(())
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        let notifications = emitted.lock().expect("emitted notifications lock");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].events[0].event_id, "event-1");
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
            &move |_, _, context: &IngressPollContext<'_>| -> Result<PluginResponse, &str> {
                context
                    .deliver(vec![test_event()], None, Some(1000))
                    .expect("delivery should succeed");
                delivered_flag.store(
                    observed.lock().expect("emitted notifications lock").len() == 1,
                    Ordering::Relaxed,
                );
                stop_during_poll.store(true, Ordering::Relaxed);
                Ok(PluginResponse::IngressEventsReceived {
                    events: Vec::new(),
                    callback_reply: None,
                    state: None,
                    poll_after_ms: Some(1000),
                })
            },
            &|_, _| {},
            move |notification| {
                emitted_for_callback
                    .lock()
                    .expect("emitted notifications lock")
                    .push(notification);
                Ok(())
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        assert!(
            delivered_before_return.load(Ordering::Relaxed),
            "the event must be on the wire before the poll returns"
        );
        let notifications = emitted.lock().expect("emitted notifications lock");
        assert_eq!(notifications.len(), 2, "event then liveness heartbeat");
        assert_eq!(notifications[0].events[0].event_id, "event-1");
        assert!(notifications[1].events.is_empty());
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
            &|_, _, context: &IngressPollContext<'_>| -> Result<PluginResponse, &str> {
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
            &|_, _, _| -> Result<PluginResponse, &str> {
                Ok(PluginResponse::Error {
                    error: dispatch_channel_protocol::PluginErrorPayload {
                        code: "invalid_auth".to_string(),
                        message: "credentials rejected".to_string(),
                    },
                })
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
            &move |_, _, _| -> Result<PluginResponse, &str> {
                stop_during_poll.store(true, Ordering::Relaxed);
                Ok(PluginResponse::Error {
                    error: dispatch_channel_protocol::PluginErrorPayload {
                        code: "transport_closed".to_string(),
                        message: "poll interrupted by shutdown".to_string(),
                    },
                })
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
            &move |_, _, _| -> Result<PluginResponse, &str> {
                stop_during_poll.store(true, Ordering::Relaxed);
                Ok(PluginResponse::IngressEventsReceived {
                    events: vec![test_event()],
                    callback_reply: None,
                    state: None,
                    poll_after_ms: None,
                })
            },
            &|_, _| panic!("after-cycle hook must not run after shutdown"),
            move |notification| {
                emitted_for_callback
                    .lock()
                    .expect("emitted notifications lock")
                    .push(notification);
                Ok(())
            },
        );

        assert_eq!(result, IngressWorkerExit::Shutdown);
        let notifications = emitted.lock().expect("emitted notifications lock");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].events[0].event_id, "event-1");
    }
}
