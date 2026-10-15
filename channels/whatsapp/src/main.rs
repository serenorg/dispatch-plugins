use anyhow::{Result, anyhow};
use dispatch_channel_protocol::{ChannelEventBatch, IngressMode, IngressState, parse_plugin_input};
use dispatch_channel_runtime::{
    deliver_channel_event_batch, spawn_plugin_input_reader, write_stdout_line,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

mod deliver;
mod ingress;
mod link;
mod policy;
mod protocol;
mod session;
mod status;
mod store;

use ingress::{IngressWorker, poll_ingress_once, start_ingress_worker};
use policy::WhatsAppPolicy;
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ConfiguredChannel, HealthReport, PluginRequest,
    PluginRequestEnvelope, PluginResponse, capabilities, plugin_error, response_to_jsonrpc,
};
use session::{SessionState, load_session};

const PLATFORM_WHATSAPP: &str = "whatsapp";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(first) = args.next() {
        if first == "--link" {
            return match link::LinkOptions::parse(args.collect::<Vec<_>>())? {
                link::ParsedLinkCommand::Run(options) => link::run(*options),
                link::ParsedLinkCommand::Help => {
                    print!("{}", link::HELP_TEXT);
                    Ok(())
                }
            };
        }
        return Err(anyhow!(
            "unknown argument `{first}`; channel-whatsapp supports `--link` for QR pairing or no arguments for JSON-RPC plugin mode"
        ));
    }

    let stdout_lock = Arc::new(Mutex::new(()));
    let mut ingress_worker: Option<IngressWorker> = None;

    for request in spawn_plugin_input_reader(
        Arc::clone(&stdout_lock),
        parse_plugin_input::<ChannelConfig, protocol::OutboundMessage>,
    ) {
        let (request_id, envelope) = request?;
        let should_exit = matches!(envelope.request, PluginRequest::Shutdown);
        let response = handle_request(&envelope, &stdout_lock, &mut ingress_worker);
        let json = response_to_jsonrpc(&request_id, &response).map_err(|error| anyhow!(error))?;
        write_stdout_line(&stdout_lock, &json)?;

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
    stdout_lock: &Arc<Mutex<()>>,
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
        PluginRequest::PollIngress { config, state, .. } => {
            poll_ingress(config, state.as_ref(), stdout_lock)
        }
        PluginRequest::StartIngress { config, state, .. } => {
            start_ingress(config, state.as_ref(), stdout_lock, ingress_worker)
        }
        PluginRequest::StopIngress { config, state } => {
            stop_ingress(config, state.as_ref(), ingress_worker)
        }
        PluginRequest::IngressEvent { .. } => not_implemented("ingress_event"),
        PluginRequest::Deliver { config, message } => {
            send_whatsapp_message(config, message, DeliveryKind::Deliver)
        }
        PluginRequest::Push { config, message } => {
            send_whatsapp_message(config, message, DeliveryKind::Push)
        }
        PluginRequest::GetMessage { .. } | PluginRequest::GetPermalink { .. } => plugin_error(
            "unsupported_request",
            "whatsapp does not support message read-back",
        ),
        PluginRequest::Status { config, update, .. } => handle_status(config, update),
    }
}

enum DeliveryKind {
    Deliver,
    Push,
}

fn send_whatsapp_message(
    config: &ChannelConfig,
    message: &protocol::OutboundMessage,
    kind: DeliveryKind,
) -> PluginResponse {
    let policy = match WhatsAppPolicy::from_config(config) {
        Ok(policy) => policy,
        Err(error) => return plugin_error("deliver_failed", format!("{error:#}")),
    };
    match deliver::deliver_text_message(config, &policy, message) {
        Ok(delivery) => match kind {
            DeliveryKind::Deliver => PluginResponse::Delivered { delivery },
            DeliveryKind::Push => PluginResponse::Pushed { delivery },
        },
        Err(error) => plugin_error("deliver_failed", format!("{error:#}")),
    }
}

fn handle_status(config: &ChannelConfig, frame: &protocol::StatusFrame) -> PluginResponse {
    let policy = match WhatsAppPolicy::from_config(config) {
        Ok(policy) => policy,
        Err(error) => return plugin_error("status_failed", format!("{error:#}")),
    };
    match status::handle_status(config, &policy, frame) {
        Ok(status) => PluginResponse::StatusAccepted { status },
        Err(error) => plugin_error("status_failed", format!("{error:#}")),
    }
}

fn poll_ingress(
    config: &ChannelConfig,
    _state: Option<&IngressState>,
    stdout_lock: &Arc<Mutex<()>>,
) -> PluginResponse {
    let (policy, account_id) = match ingress_policy_and_account(config) {
        Ok(value) => value,
        Err(error) => return plugin_error("poll_ingress_failed", format!("{error:#}")),
    };
    if let Err(error) = poll_ingress_once(config, policy, account_id, Arc::clone(stdout_lock)) {
        return plugin_error("poll_ingress_failed", format!("{error:#}"));
    }
    // Every poll ends with a confirmed state batch; an unchanged one replays
    // the accepted original.
    let state = Some(running_ingress_state(config, IngressMode::Polling));
    match deliver_channel_event_batch(
        stdout_lock,
        ChannelEventBatch {
            events: Vec::new(),
            state: state.clone(),
            poll_after_ms: None,
        },
    ) {
        Ok(()) => PluginResponse::IngressEventsReceived {
            callback_reply: None,
            state,
            poll_after_ms: None,
        },
        Err(error) => plugin_error("event_delivery_failed", error.to_string()),
    }
}

fn start_ingress(
    config: &ChannelConfig,
    _restored_state: Option<&IngressState>,
    stdout_lock: &Arc<Mutex<()>>,
    ingress_worker: &mut Option<IngressWorker>,
) -> PluginResponse {
    if let Some(worker) = ingress_worker.take() {
        worker.stop();
    }

    let (policy, account_id) = match ingress_policy_and_account(config) {
        Ok(value) => value,
        Err(error) => return plugin_error("start_ingress_failed", format!("{error:#}")),
    };
    match start_ingress_worker(config, policy, account_id, Arc::clone(stdout_lock)) {
        Ok(worker) => {
            *ingress_worker = Some(worker);
            PluginResponse::IngressStarted {
                state: running_ingress_state(config, IngressMode::Websocket),
            }
        }
        Err(error) => plugin_error("start_ingress_failed", format!("{error:#}")),
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

fn configure(config: &ChannelConfig) -> PluginResponse {
    let policy = match WhatsAppPolicy::from_config(config) {
        Ok(policy) => policy,
        Err(error) => return plugin_error("configure_failed", format!("{error:#}")),
    };
    match load_session(config) {
        Ok(state) => PluginResponse::Configured {
            configuration: Box::new(ConfiguredChannel {
                metadata: session_metadata(&state, Some(&policy)),
                policy: Some(policy.to_channel_policy()),
                runtime: None,
            }),
        },
        Err(error) => plugin_error("configure_failed", format!("{error:#}")),
    }
}

fn health(config: &ChannelConfig) -> PluginResponse {
    let policy = match WhatsAppPolicy::from_config(config) {
        Ok(policy) => policy,
        Err(error) => return plugin_error("health_failed", format!("{error:#}")),
    };
    match load_session(config) {
        Ok(state) => PluginResponse::Health {
            health: health_report(&state, Some(&policy)),
        },
        Err(error) => plugin_error("health_failed", format!("{error:#}")),
    }
}

fn session_metadata(
    state: &SessionState,
    policy: Option<&WhatsAppPolicy>,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), PLATFORM_WHATSAPP.to_string());
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
            metadata.insert("device_id".to_string(), summary.device_id.to_string());
            if let Some(phone_jid) = &summary.phone_jid {
                metadata.insert("phone_jid".to_string(), phone_jid.clone());
            }
            if let Some(lid_jid) = &summary.lid_jid {
                metadata.insert("lid_jid".to_string(), lid_jid.clone());
            }
            if let Some(push_name) = &summary.push_name {
                metadata.insert("push_name".to_string(), push_name.clone());
            }
        }
    }
    if let Some(policy) = policy {
        metadata.extend(policy.diagnostics());
    }
    metadata
}

fn health_report(state: &SessionState, policy: Option<&WhatsAppPolicy>) -> HealthReport {
    let metadata = session_metadata(state, policy);
    match state {
        SessionState::Registered { summary, .. } => HealthReport {
            ok: true,
            status: "ok".to_string(),
            account_id: policy
                .and_then(WhatsAppPolicy::self_chat_account_id)
                .or_else(|| summary.phone_jid.clone())
                .or_else(|| summary.lid_jid.clone()),
            display_name: summary.push_name.clone(),
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

fn ingress_policy_and_account(config: &ChannelConfig) -> Result<(WhatsAppPolicy, String)> {
    let policy = WhatsAppPolicy::from_config(config)?;
    policy.validate_for_ingress()?;
    let SessionState::Registered { summary, .. } = load_session(config)? else {
        return Err(anyhow!(
            "WhatsApp session has not been linked yet; run `channel-whatsapp --link` first"
        ));
    };
    policy
        .self_chat_account_id()
        .or(summary.phone_jid)
        .or(summary.lid_jid)
        .ok_or_else(|| anyhow!("WhatsApp linked session does not provide a direct account JID"))
        .map(|account_id| (policy, account_id))
}

fn running_ingress_state(config: &ChannelConfig, mode: IngressMode) -> IngressState {
    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), PLATFORM_WHATSAPP.to_string());
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
        mode,
        status: "running".to_string(),
        endpoint: None,
        metadata,
    }
}

fn stopped_ingress_state(config: &ChannelConfig) -> IngressState {
    let mut state = running_ingress_state(config, IngressMode::Websocket);
    state.status = "stopped".to_string();
    state
}

fn not_implemented(kind: &str) -> PluginResponse {
    plugin_error(
        "not_implemented",
        format!("channel-whatsapp does not implement `{kind}` yet"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingress_state_reports_the_selected_transport_mode() {
        let config = ChannelConfig::default();
        assert_eq!(
            running_ingress_state(&config, IngressMode::Polling).mode,
            IngressMode::Polling
        );
        assert_eq!(
            running_ingress_state(&config, IngressMode::Websocket).mode,
            IngressMode::Websocket
        );
    }
}
