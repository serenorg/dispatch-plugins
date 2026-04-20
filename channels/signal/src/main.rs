//! Native Rust Signal channel plugin (presage-backed).
//!
//! The plugin keeps the outer JSON-RPC loop synchronous over stdio,
//! but runs presage operations on dedicated OS threads with their own
//! current-thread tokio runtimes. This keeps the plugin compatible
//! with Dispatch's line-oriented process model while satisfying
//! presage's `!Send` store and stream types.

use anyhow::{Context, Result, anyhow};
use dispatch_channel_protocol::{
    ChannelEventNotification, HealthReport, InboundEventEnvelope, IngressMode, IngressState,
    PluginNotificationEnvelope, notification_to_jsonrpc,
};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

mod deliver;
mod ingress;
mod link;
mod protocol;
mod session;
mod status;
mod store;

use ingress::{IngressWorker, poll_ingress_once, start_ingress_worker};
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ConfiguredChannel, OutboundMessage,
    PluginRequest, PluginRequestEnvelope, PluginResponse, capabilities, parse_jsonrpc_request,
    plugin_error, response_to_jsonrpc,
};
use session::{SessionState, load_session};

const PLATFORM_SIGNAL: &str = "signal";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(first) = args.next() {
        if first == "--link" {
            return match link::LinkOptions::parse(args.collect::<Vec<_>>())? {
                link::ParsedLinkCommand::Run(options) => link::run(options),
                link::ParsedLinkCommand::Help => {
                    print!("{}", link::HELP_TEXT);
                    Ok(())
                }
            };
        }
        return Err(anyhow!(
            "unknown argument `{first}`; channel-signal supports `--link` for QR pairing or no arguments for JSON-RPC plugin mode"
        ));
    }

    let stdin = io::stdin().lock();
    let stdout_lock = Arc::new(Mutex::new(()));
    let mut ingress_worker: Option<IngressWorker> = None;

    for line in stdin.lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let (request_id, envelope) = parse_jsonrpc_request(&line)
            .map_err(|error| anyhow!("failed to parse channel request: {error}"))?;
        let should_exit = matches!(envelope.request, PluginRequest::Shutdown);

        let response = handle_request(&envelope, &mut ingress_worker);

        let json = response_to_jsonrpc(&request_id, &response).map_err(|error| anyhow!(error))?;
        write_stdout_line(&stdout_lock, &json)?;

        // Emit any queued inbound events as channel.event notifications.
        if let Some(worker) = ingress_worker.as_ref() {
            let events = worker.drain_pending_events();
            if !events.is_empty() {
                emit_channel_event_notifications(&stdout_lock, events)?;
            }
        }

        if should_exit {
            break;
        }
    }

    if let Some(worker) = ingress_worker.take() {
        worker.stop();
    }
    Ok(())
}

fn handle_request(
    envelope: &PluginRequestEnvelope,
    ingress_worker: &mut Option<IngressWorker>,
) -> PluginResponse {
    if envelope.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return plugin_error(
            "unsupported_protocol_version",
            format!(
                "expected protocol_version {}, got {}",
                CHANNEL_PLUGIN_PROTOCOL_VERSION, envelope.protocol_version
            ),
        );
    }

    match &envelope.request {
        PluginRequest::Capabilities => PluginResponse::Capabilities {
            capabilities: capabilities(),
        },
        PluginRequest::Shutdown => {
            if let Some(worker) = ingress_worker.take() {
                worker.stop();
            }
            PluginResponse::Ok
        }
        PluginRequest::Configure { config } => configure(config),
        PluginRequest::Health { config } => health(config),
        PluginRequest::PollIngress { config, state } => poll_ingress(config, state.as_ref()),
        PluginRequest::StartIngress { config, state } => {
            start_ingress(config, state.as_ref(), ingress_worker)
        }
        PluginRequest::StopIngress { config, state } => {
            stop_ingress(config, state.as_ref(), ingress_worker)
        }
        PluginRequest::Deliver { config, message } => {
            send_signal_message(config, message, DeliveryKind::Deliver)
        }
        PluginRequest::Push { config, message } => {
            send_signal_message(config, message, DeliveryKind::Push)
        }
        PluginRequest::IngressEvent { .. } => not_implemented("ingress_event"),
        PluginRequest::Status { config, update } => match status::handle_status(config, update) {
            Ok(acceptance) => PluginResponse::StatusAccepted { status: acceptance },
            Err(error) => plugin_error("status_failed", error.to_string()),
        },
    }
}

fn configure(config: &ChannelConfig) -> PluginResponse {
    match load_session(config) {
        Ok(state) => PluginResponse::Configured {
            configuration: Box::new(ConfiguredChannel {
                metadata: session_metadata(&state),
                policy: None,
                runtime: None,
            }),
        },
        Err(error) => plugin_error("configure_failed", error.to_string()),
    }
}

fn health(config: &ChannelConfig) -> PluginResponse {
    match load_session(config) {
        Ok(state) => PluginResponse::Health {
            health: health_report(&state),
        },
        Err(error) => plugin_error("health_failed", error.to_string()),
    }
}

fn poll_ingress(config: &ChannelConfig, _state: Option<&IngressState>) -> PluginResponse {
    match poll_ingress_once(config) {
        Ok(events) => PluginResponse::IngressEventsReceived {
            events,
            callback_reply: None,
            state: Some(running_ingress_state(config)),
            poll_after_ms: None,
        },
        Err(error) => plugin_error("poll_ingress_failed", error.to_string()),
    }
}

fn start_ingress(
    config: &ChannelConfig,
    _restored_state: Option<&IngressState>,
    ingress_worker: &mut Option<IngressWorker>,
) -> PluginResponse {
    // Tear down any previous worker on re-start.
    if let Some(worker) = ingress_worker.take() {
        worker.stop();
    }

    match start_ingress_worker(config) {
        Ok(worker) => {
            *ingress_worker = Some(worker);
            PluginResponse::IngressStarted {
                state: running_ingress_state(config),
            }
        }
        Err(error) => plugin_error("start_ingress_failed", error.to_string()),
    }
}

enum DeliveryKind {
    Deliver,
    Push,
}

fn send_signal_message(
    config: &ChannelConfig,
    message: &OutboundMessage,
    kind: DeliveryKind,
) -> PluginResponse {
    match deliver::deliver_text_message(config, message) {
        Ok(delivery) => match kind {
            DeliveryKind::Deliver => PluginResponse::Delivered { delivery },
            DeliveryKind::Push => PluginResponse::Pushed { delivery },
        },
        Err(error) => plugin_error("deliver_failed", error.to_string()),
    }
}

fn stop_ingress(
    config: &ChannelConfig,
    _state: Option<&IngressState>,
    ingress_worker: &mut Option<IngressWorker>,
) -> PluginResponse {
    if let Some(worker) = ingress_worker.take() {
        worker.stop();
    }
    PluginResponse::IngressStopped {
        state: stopped_ingress_state(config),
    }
}

fn running_ingress_state(config: &ChannelConfig) -> IngressState {
    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), PLATFORM_SIGNAL.to_string());
    metadata.insert("transport".to_string(), "websocket".to_string());
    if let Some(account) = config
        .account
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        metadata.insert("account".to_string(), account.to_string());
    }
    IngressState {
        mode: IngressMode::Polling,
        status: "running".to_string(),
        endpoint: None,
        metadata,
    }
}

fn stopped_ingress_state(config: &ChannelConfig) -> IngressState {
    let mut state = running_ingress_state(config);
    state.status = "stopped".to_string();
    state
}

fn session_metadata(state: &SessionState) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), PLATFORM_SIGNAL.to_string());
    match state {
        SessionState::NotYetLinked { store_path } => {
            metadata.insert("session_state".to_string(), "not_linked".to_string());
            metadata.insert(
                "sqlite_store_path".to_string(),
                store_path.display().to_string(),
            );
        }
        SessionState::StoreEmpty { store_path } => {
            metadata.insert("session_state".to_string(), "store_empty".to_string());
            metadata.insert(
                "sqlite_store_path".to_string(),
                store_path.display().to_string(),
            );
        }
        SessionState::Registered {
            store_path,
            summary,
        } => {
            metadata.insert("session_state".to_string(), "registered".to_string());
            metadata.insert(
                "sqlite_store_path".to_string(),
                store_path.display().to_string(),
            );
            metadata.insert("aci".to_string(), summary.aci.clone());
            metadata.insert("phone_number".to_string(), summary.phone_number.clone());
            metadata.insert("device_id".to_string(), summary.device_id.to_string());
            metadata.insert("signal_servers".to_string(), summary.servers.to_string());
            if let Some(device_name) = &summary.device_name {
                metadata.insert("device_name".to_string(), device_name.clone());
            }
        }
    }
    metadata
}

fn health_report(state: &SessionState) -> HealthReport {
    let metadata = session_metadata(state);
    match state {
        SessionState::Registered { summary, .. } => HealthReport {
            ok: true,
            status: "ok".to_string(),
            account_id: Some(summary.aci.clone()),
            display_name: summary.device_name.clone(),
            metadata,
        },
        SessionState::NotYetLinked { .. } | SessionState::StoreEmpty { .. } => HealthReport {
            ok: false,
            status: "not_linked".to_string(),
            account_id: None,
            display_name: None,
            metadata,
        },
    }
}

fn not_implemented(operation: &str) -> PluginResponse {
    plugin_error(
        "not_implemented",
        format!(
            "channel-signal operation `{operation}` is not yet available; the plugin is being migrated to the native Rust presage client"
        ),
    )
}

fn emit_channel_event_notifications(
    stdout_lock: &Arc<Mutex<()>>,
    events: Vec<InboundEventEnvelope>,
) -> Result<()> {
    let envelope = PluginNotificationEnvelope {
        protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
        notification: ChannelEventNotification {
            events,
            state: None,
            poll_after_ms: None,
        },
    };
    let json = notification_to_jsonrpc(&envelope).map_err(|error| anyhow!(error.to_string()))?;
    write_stdout_line(stdout_lock, &json)
}

fn write_stdout_line(stdout_lock: &Arc<Mutex<()>>, line: &str) -> Result<()> {
    let _guard = stdout_lock
        .lock()
        .map_err(|_| anyhow!("stdout lock poisoned"))?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}
