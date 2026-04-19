use dispatch_channel_protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelEventNotification, IngressState, JsonRpcMessageError,
    PluginNotificationEnvelope, PluginResponse, notification_to_jsonrpc,
};
use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use thiserror::Error;

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
    Poll: Fn(&C, Option<IngressState>) -> Result<PluginResponse, E> + Send + 'static,
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
    Poll: Fn(&C, Option<IngressState>) -> Result<PluginResponse, E> + Send + 'static,
    After: Fn(&C, &StopSignal) + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(Some(initial_state)));
    let stop_signal = StopSignal(Arc::clone(&stop));
    let shared_state = Arc::clone(&state);
    let handle = thread::spawn(move || {
        while !stop_signal.is_stopped() {
            let prior_state = shared_state
                .lock()
                .expect("channel ingress state poisoned")
                .clone();
            match poll(&config, prior_state) {
                Ok(PluginResponse::IngressEventsReceived {
                    events,
                    state,
                    poll_after_ms,
                    ..
                }) => {
                    if let Some(next_state) = state.clone() {
                        *shared_state.lock().expect("channel ingress state poisoned") =
                            Some(next_state);
                    }
                    if let Err(error) = emit_channel_event_notification(
                        &stdout_lock,
                        ChannelEventNotification {
                            events,
                            state,
                            poll_after_ms,
                        },
                    ) {
                        eprintln!(
                            "{plugin_label} ingress worker failed to emit notification: {error}"
                        );
                        break;
                    }
                    after_cycle(&config, &stop_signal);
                }
                Ok(PluginResponse::Error { error }) => {
                    eprintln!(
                        "{plugin_label} ingress worker stopped after plugin error {}: {}",
                        error.code, error.message
                    );
                    break;
                }
                Ok(other) => {
                    eprintln!(
                        "{plugin_label} ingress worker received unexpected response variant: {:?}",
                        other
                    );
                    break;
                }
                Err(error) => {
                    eprintln!(
                        "{plugin_label} ingress worker stopped after receive failure: {error}"
                    );
                    break;
                }
            }
        }
    });

    IngressWorker {
        stop,
        state,
        handle,
    }
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
