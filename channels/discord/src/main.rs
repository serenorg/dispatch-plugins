use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use dispatch_channel_protocol::{
    ChannelEventNotification, PluginNotificationEnvelope, notification_to_jsonrpc,
};
use dispatch_channel_runtime::write_stdout_line;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead},
    net::TcpStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket,
    protocol::{CloseFrame, frame::coding::CloseCode},
    stream::MaybeTlsStream,
};

mod discord_api;
mod protocol;

use discord_api::{DiscordClient, DiscordUpload};
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ChannelPolicy, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressCallbackReply, IngressMode, IngressPayload,
    IngressState, OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse,
    StatusAcceptance, StatusFrame, StatusKind, capabilities, parse_jsonrpc_request, plugin_error,
    response_to_jsonrpc,
};

const META_REASON: &str = "reason";
const META_PLATFORM: &str = "platform";
const META_BOT_TOKEN_ENV: &str = "bot_token_env";
const META_APPLICATION_ID: &str = "application_id";
const META_DEFAULT_CHANNEL_ID: &str = "default_channel_id";
const META_ALLOWED_GUILD_COUNT: &str = "allowed_guild_count";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_INTERACTION_ENDPOINT: &str = "interaction_endpoint";
const META_INTERACTION_PUBLIC_KEY_ENV: &str = "interaction_public_key_env";
const META_VERIFICATION_KEY_ENV: &str = "verification_key_env";
const META_HOST_ACTION: &str = "host_action";
const META_CHANNEL_ID: &str = "channel_id";
const META_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const META_THREAD_ID: &str = "thread_id";
const META_TRANSPORT: &str = "transport";
const META_ENDPOINT_ID: &str = "endpoint_id";
const META_PATH: &str = "path";
const META_INTERACTION_TYPE: &str = "interaction_type";
const META_GUILD_ID: &str = "guild_id";
const META_COMMAND_NAME: &str = "command_name";
const META_COMMAND_KIND: &str = "command_kind";
const META_CUSTOM_ID: &str = "custom_id";
const META_COMPONENT_TYPE: &str = "component_type";
const META_SOURCE_MESSAGE_ID: &str = "source_message_id";
const META_ATTACHMENT_COUNT: &str = "attachment_count";
const META_LOCALE: &str = "locale";
const META_GUILD_LOCALE: &str = "guild_locale";
const META_ACTOR_KIND: &str = "actor_kind";
const META_NICK: &str = "nick";
const META_RESOLVED_DESTINATION: &str = "resolved_destination";
const META_STATUS_KIND: &str = "status_kind";
const META_REASON_CODE: &str = "reason_code";

const PLATFORM_DISCORD: &str = "discord";
const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const ROUTE_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const TRANSPORT_INTERACTION_WEBHOOK: &str = "interaction_webhook";
const TRANSPORT_WEBSOCKET: &str = "websocket";
const DISCORD_STATUS_TEXT: &str = "discord_status_text";
const DISCORD_GATEWAY_BASE_URL: &str = "wss://gateway.discord.gg";

const HEADER_X_SIGNATURE_ED25519: &str = "X-Signature-Ed25519";
const HEADER_X_SIGNATURE_TIMESTAMP: &str = "X-Signature-Timestamp";

/// Accept Discord interaction signatures whose timestamp is within this many
/// seconds of the host clock. The ED25519 signature covers the timestamp, but
/// the timestamp itself is not bounded by the signature - without a freshness
/// window, an attacker who captured a valid interaction could replay it
/// indefinitely. Discord's own documentation recommends a small window.
const DISCORD_MAX_SIGNATURE_AGE_SECS: i64 = 300;

const INTERACTION_TYPE_PING: u8 = 1;
const INTERACTION_TYPE_APPLICATION_COMMAND: u8 = 2;
const INTERACTION_TYPE_MESSAGE_COMPONENT: u8 = 3;
const INTERACTION_TYPE_APPLICATION_COMMAND_AUTOCOMPLETE: u8 = 4;
const INTERACTION_TYPE_MODAL_SUBMIT: u8 = 5;

const COMMAND_KIND_CHAT_INPUT: u8 = 1;
const COMMAND_KIND_USER: u8 = 2;
const COMMAND_KIND_MESSAGE: u8 = 3;

const COMPONENT_KIND_BUTTON: u8 = 2;
const COMPONENT_KIND_STRING_SELECT: u8 = 3;
const COMPONENT_KIND_TEXT_INPUT: u8 = 4;
const COMPONENT_KIND_USER_SELECT: u8 = 5;
const COMPONENT_KIND_ROLE_SELECT: u8 = 6;
const COMPONENT_KIND_MENTIONABLE_SELECT: u8 = 7;
const COMPONENT_KIND_CHANNEL_SELECT: u8 = 8;

const DISCORD_GATEWAY_VERSION: u8 = 10;
const DISCORD_GATEWAY_BASE_INTENTS: u64 = 1 | (1 << 9) | (1 << 12);
const DISCORD_GATEWAY_MESSAGE_CONTENT_INTENT: u64 = 1 << 15;
const DISCORD_GATEWAY_READ_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let stdin = io::stdin().lock();
    let stdout_lock = Arc::new(Mutex::new(()));
    let mut ingress_worker: Option<DiscordIngressWorker> = None;

    for line in stdin.lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let (request_id, envelope) = parse_jsonrpc_request(&line)
            .map_err(|error| anyhow!("failed to parse channel request: {error}"))?;
        let should_exit = matches!(envelope.request, PluginRequest::Shutdown);

        let response = match handle_request(&envelope, &stdout_lock, &mut ingress_worker) {
            Ok(response) => response,
            Err(error) => plugin_error("internal_error", error.to_string()),
        };

        let json = response_to_jsonrpc(&request_id, &response).map_err(|error| anyhow!(error))?;
        write_stdout_line(&stdout_lock, &json)?;
        if should_exit {
            break;
        }
    }

    let _ = stop_ingress_worker(&mut ingress_worker);
    Ok(())
}

fn handle_request(
    envelope: &PluginRequestEnvelope,
    stdout_lock: &Arc<Mutex<()>>,
    ingress_worker: &mut Option<DiscordIngressWorker>,
) -> Result<PluginResponse> {
    if envelope.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return Ok(plugin_error(
            "unsupported_protocol_version",
            format!(
                "expected protocol_version {}, got {}",
                CHANNEL_PLUGIN_PROTOCOL_VERSION, envelope.protocol_version
            ),
        ));
    }

    match &envelope.request {
        PluginRequest::Capabilities => Ok(PluginResponse::Capabilities {
            capabilities: capabilities(),
        }),
        PluginRequest::Configure { config } => Ok(PluginResponse::Configured {
            configuration: Box::new(configure(config)?),
        }),
        PluginRequest::Health { config } => Ok(PluginResponse::Health {
            health: health(config)?,
        }),
        PluginRequest::PollIngress { config, state } => {
            handle_websocket_receive(config, state.as_ref())
        }
        PluginRequest::StartIngress { config, state } => {
            let started = start_ingress(config)?;
            let started = match (&started.mode, state.clone()) {
                (IngressMode::Websocket, Some(state)) if state.mode == IngressMode::Websocket => {
                    state
                }
                _ => started,
            };
            if matches!(started.mode, IngressMode::Websocket) {
                restart_ingress_worker(
                    ingress_worker,
                    config.clone(),
                    started.clone(),
                    Arc::clone(stdout_lock),
                );
            } else {
                let _ = stop_ingress_worker(ingress_worker);
            }
            Ok(PluginResponse::IngressStarted { state: started })
        }
        PluginRequest::StopIngress { config, state } => Ok(PluginResponse::IngressStopped {
            state: stop_ingress(
                config,
                stop_ingress_worker(ingress_worker).or(state.clone()),
            )?,
        }),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::IngressEvent {
            config, payload, ..
        } => handle_ingress_event(config, payload),
        PluginRequest::Status { config, update } => Ok(PluginResponse::StatusAccepted {
            status: send_status(config, update)?,
        }),
        PluginRequest::Shutdown => {
            let _ = stop_ingress_worker(ingress_worker);
            Ok(PluginResponse::Ok)
        }
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let bot_token_env = bot_token_env(config);
    read_required_env(bot_token_env)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_BOT_TOKEN_ENV.to_string(), bot_token_env.to_string());
    if let Some(application_id) = &config.application_id {
        metadata.insert(META_APPLICATION_ID.to_string(), application_id.clone());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        metadata.insert(
            META_DEFAULT_CHANNEL_ID.to_string(),
            default_channel_id.clone(),
        );
    }
    if !config.allowed_guild_ids.is_empty() {
        metadata.insert(
            META_ALLOWED_GUILD_COUNT.to_string(),
            config.allowed_guild_ids.len().to_string(),
        );
    }
    if let Some(endpoint) = resolved_endpoint(config) {
        let public_key_env = interaction_public_key_env(config);
        read_required_env(public_key_env)?;
        metadata.insert(
            META_INGRESS_MODE.to_string(),
            ingress_mode_name(IngressMode::InteractionWebhook),
        );
        metadata.insert(META_INTERACTION_ENDPOINT.to_string(), endpoint);
        metadata.insert(
            META_INTERACTION_PUBLIC_KEY_ENV.to_string(),
            public_key_env.to_string(),
        );
    }

    Ok(ConfiguredChannel {
        metadata,
        policy: Some(channel_policy(config)),
        runtime: None,
    })
}

fn channel_policy(config: &ChannelConfig) -> ChannelPolicy {
    ChannelPolicy {
        owner_id: config.owner_id.clone(),
        allowed_sender_ids: config.allowed_sender_ids.clone(),
        allowed_conversation_ids: config.allowed_guild_ids.clone(),
        dm_policy: config.dm_policy.clone(),
        require_signature_validation: Some(true),
        allow_group_messages: None,
        max_attachment_bytes: None,
        metadata: BTreeMap::new(),
    }
}

struct DiscordIngressWorker {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Option<IngressState>>>,
    handle: JoinHandle<()>,
}

fn restart_ingress_worker(
    worker: &mut Option<DiscordIngressWorker>,
    config: ChannelConfig,
    state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
) {
    let _ = stop_ingress_worker(worker);
    let stop = Arc::new(AtomicBool::new(false));
    let shared_state = Arc::new(Mutex::new(Some(state.clone())));
    let worker_stop = Arc::clone(&stop);
    let worker_state = Arc::clone(&shared_state);
    let handle = thread::spawn(move || {
        run_discord_websocket_worker(config, state, worker_stop, worker_state, stdout_lock);
    });
    *worker = Some(DiscordIngressWorker {
        stop,
        state: shared_state,
        handle,
    });
}

fn stop_ingress_worker(worker: &mut Option<DiscordIngressWorker>) -> Option<IngressState> {
    let worker = worker.take()?;
    worker.stop.store(true, Ordering::Relaxed);
    let _ = worker.handle.join();
    worker.state.lock().ok().and_then(|state| (*state).clone())
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let identity = client.identity()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    if let Some(default_channel_id) = &config.default_channel_id {
        metadata.insert(
            META_DEFAULT_CHANNEL_ID.to_string(),
            default_channel_id.clone(),
        );
    }

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: Some(identity.id),
        display_name: Some(identity.global_name.unwrap_or(identity.username)),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    read_required_env(bot_token_env(config))?;

    if let Some(endpoint) = resolved_endpoint(config) {
        read_required_env(interaction_public_key_env(config))?;
        let mut metadata = BTreeMap::new();
        metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
        metadata.insert(
            META_VERIFICATION_KEY_ENV.to_string(),
            interaction_public_key_env(config).to_string(),
        );
        metadata.insert(
            META_HOST_ACTION.to_string(),
            "route Discord interaction POSTs to the reported endpoint and verify signatures with the configured public key".to_string(),
        );

        return Ok(IngressState {
            mode: IngressMode::InteractionWebhook,
            status: "configured".to_string(),
            endpoint: Some(endpoint),
            metadata,
        });
    }

    let mut metadata = websocket_state_metadata();
    metadata.insert(
        META_HOST_ACTION.to_string(),
        "keep a Discord websocket connection open and emit message events as channel.event notifications".to_string(),
    );

    Ok(IngressState {
        mode: IngressMode::Websocket,
        status: "running".to_string(),
        endpoint: None,
        metadata,
    })
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    read_required_env(bot_token_env(config))?;

    let mut stopped = state.unwrap_or_else(|| {
        if let Some(endpoint) = resolved_endpoint(config) {
            IngressState {
                mode: IngressMode::InteractionWebhook,
                status: "configured".to_string(),
                endpoint: Some(endpoint),
                metadata: BTreeMap::new(),
            }
        } else {
            websocket_state("running", None, None, None)
        }
    });
    stopped.status = "stopped".to_string();
    stopped
        .metadata
        .insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    Ok(stopped)
}

fn handle_ingress_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if !payload.method.eq_ignore_ascii_case("POST") {
        return Ok(ingress_rejection(
            405,
            "discord interactions expect POST requests",
        ));
    }

    if let Some(reply) = validate_ingress_signature(config, payload)? {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(reply),
            state: None,
            poll_after_ms: None,
        });
    }

    let interaction: DiscordInteraction = match serde_json::from_str(&payload.body) {
        Ok(interaction) => interaction,
        Err(_) => {
            return Ok(ingress_rejection(
                400,
                "invalid Discord interaction payload",
            ));
        }
    };

    if interaction.interaction_type == INTERACTION_TYPE_PING {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(discord_json_reply(json!({ "type": INTERACTION_TYPE_PING }))),
            state: None,
            poll_after_ms: None,
        });
    }

    if !guild_is_allowed(config, interaction.guild_id.as_deref()) {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(discord_ephemeral_message(
                "This Discord server is not allowed for this plugin.",
            )),
            state: None,
            poll_after_ms: None,
        });
    }

    let Some(event) = build_inbound_event(config, &interaction, payload)? else {
        let message =
            if interaction.interaction_type == INTERACTION_TYPE_APPLICATION_COMMAND_AUTOCOMPLETE {
                "Discord autocomplete interactions are not implemented by this plugin."
            } else {
                "This Discord interaction type is not implemented by this plugin."
            };
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(discord_ephemeral_message(message)),
            state: None,
            poll_after_ms: None,
        });
    };

    Ok(PluginResponse::IngressEventsReceived {
        events: vec![event],
        callback_reply: Some(discord_ephemeral_message(
            "Dispatch is processing your request.",
        )),
        state: None,
        poll_after_ms: None,
    })
}

fn handle_websocket_receive(
    config: &ChannelConfig,
    state: Option<&IngressState>,
) -> Result<PluginResponse> {
    discord_websocket_receive(config, state.cloned())
}

fn discord_websocket_receive(
    config: &ChannelConfig,
    state: Option<IngressState>,
) -> Result<PluginResponse> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let mut session = DiscordGatewaySession::from_state(state.as_ref());
    let gateway_url =
        discord_gateway_base_url(&session, || Ok(DISCORD_GATEWAY_BASE_URL.to_string()))?;
    let websocket_url = discord_gateway_websocket_url(&gateway_url);
    let (mut socket, _) = tungstenite::connect(websocket_url.as_str())
        .with_context(|| format!("failed to connect Discord websocket: {websocket_url}"))?;
    let deadline = Instant::now() + discord_websocket_timeout_window();

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(PluginResponse::IngressEventsReceived {
                events: Vec::new(),
                callback_reply: None,
                state: Some(session.to_state("running")),
                poll_after_ms: Some(1000),
            });
        }
        if now >= session.next_heartbeat {
            if session.awaiting_heartbeat_ack {
                close_discord_websocket_for_reconnect(&mut socket);
                return Ok(discord_gateway_action_response(
                    DiscordGatewayAction::Reconnect,
                    &session,
                ));
            }
            send_discord_session_heartbeat(&mut socket, &mut session)?;
        }
        configure_websocket_read_timeout(
            socket.get_mut(),
            std::cmp::min(
                DISCORD_GATEWAY_READ_TIMEOUT,
                deadline.saturating_duration_since(now),
            ),
        )?;
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(action) = handle_discord_gateway_text(
                    config,
                    &mut socket,
                    text.as_str(),
                    &mut session,
                    client.bot_token(),
                )? {
                    return Ok(discord_gateway_action_response(action, &session));
                }
            }
            Ok(Message::Binary(bytes)) => {
                let text = std::str::from_utf8(bytes.as_ref())
                    .context("Discord websocket binary frame was not valid UTF-8")?;
                if let Some(action) = handle_discord_gateway_text(
                    config,
                    &mut socket,
                    text,
                    &mut session,
                    client.bot_token(),
                )? {
                    return Ok(discord_gateway_action_response(action, &session));
                }
            }
            Ok(Message::Ping(payload)) => {
                socket.send(Message::Pong(payload))?;
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(frame)) => {
                let close_action = handle_discord_close_frame(&mut session, frame.as_ref());
                return Ok(PluginResponse::IngressEventsReceived {
                    events: Vec::new(),
                    callback_reply: None,
                    state: Some(session.to_state(discord_close_status(close_action))),
                    poll_after_ms: discord_close_poll_after(close_action),
                });
            }
            Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(PluginResponse::IngressEventsReceived {
                    events: Vec::new(),
                    callback_reply: None,
                    state: Some(session.to_state("closed")),
                    poll_after_ms: Some(1000),
                });
            }
            Err(error) => return Err(error).context("failed to read Discord websocket frame"),
        }
    }
}

fn run_discord_websocket_worker(
    config: ChannelConfig,
    initial_state: IngressState,
    stop: Arc<AtomicBool>,
    shared_state: Arc<Mutex<Option<IngressState>>>,
    stdout_lock: Arc<Mutex<()>>,
) {
    let mut state = Some(initial_state);
    let mut failure_backoff = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        match run_discord_websocket_session(
            &config,
            state.clone(),
            &stop,
            &shared_state,
            &stdout_lock,
        ) {
            Ok(next_state) => {
                let should_stop = next_state.status == "stopped";
                state = Some(next_state);
                failure_backoff = Duration::from_secs(1);
                if should_stop {
                    break;
                }
            }
            Err(error) => {
                eprintln!("discord websocket worker reconnecting after receive failure: {error:#}");
                sleep_until_stopped(&stop, failure_backoff);
                failure_backoff = std::cmp::min(failure_backoff * 2, Duration::from_secs(60));
                continue;
            }
        }
        sleep_until_stopped(&stop, Duration::from_secs(1));
    }
}

fn run_discord_websocket_session(
    config: &ChannelConfig,
    state: Option<IngressState>,
    stop: &AtomicBool,
    shared_state: &Arc<Mutex<Option<IngressState>>>,
    stdout_lock: &Arc<Mutex<()>>,
) -> Result<IngressState> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let mut session = DiscordGatewaySession::from_state(state.as_ref());
    let gateway_url =
        discord_gateway_base_url(&session, || Ok(DISCORD_GATEWAY_BASE_URL.to_string()))?;
    let websocket_url = discord_gateway_websocket_url(&gateway_url);
    let (mut socket, _) = tungstenite::connect(websocket_url.as_str())
        .with_context(|| format!("failed to connect Discord websocket: {websocket_url}"))?;

    loop {
        if stop.load(Ordering::Relaxed) {
            close_discord_websocket_for_stop(&mut socket);
            return Ok(session.to_state("running"));
        }
        let now = Instant::now();
        if now >= session.next_heartbeat {
            if session.awaiting_heartbeat_ack {
                let state = session.to_state("reconnect");
                set_worker_state(shared_state, state.clone());
                close_discord_websocket_for_reconnect(&mut socket);
                return Ok(state);
            }
            send_discord_session_heartbeat(&mut socket, &mut session)?;
        }
        configure_websocket_read_timeout(socket.get_mut(), DISCORD_GATEWAY_READ_TIMEOUT)?;
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(action) = handle_discord_gateway_text(
                    config,
                    &mut socket,
                    text.as_str(),
                    &mut session,
                    client.bot_token(),
                )? {
                    match action {
                        DiscordGatewayAction::Event(event) => {
                            let state = session.to_state("running");
                            set_worker_state(shared_state, state.clone());
                            emit_channel_event_notification(
                                stdout_lock,
                                vec![*event],
                                Some(state),
                                Some(0),
                            )?;
                        }
                        DiscordGatewayAction::Reconnect => {
                            let state = session.to_state("reconnect");
                            set_worker_state(shared_state, state.clone());
                            return Ok(state);
                        }
                    }
                }
            }
            Ok(Message::Binary(bytes)) => {
                let text = std::str::from_utf8(bytes.as_ref())
                    .context("Discord websocket binary frame was not valid UTF-8")?;
                if let Some(action) = handle_discord_gateway_text(
                    config,
                    &mut socket,
                    text,
                    &mut session,
                    client.bot_token(),
                )? {
                    match action {
                        DiscordGatewayAction::Event(event) => {
                            let state = session.to_state("running");
                            set_worker_state(shared_state, state.clone());
                            emit_channel_event_notification(
                                stdout_lock,
                                vec![*event],
                                Some(state),
                                Some(0),
                            )?;
                        }
                        DiscordGatewayAction::Reconnect => {
                            let state = session.to_state("reconnect");
                            set_worker_state(shared_state, state.clone());
                            return Ok(state);
                        }
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                socket.send(Message::Pong(payload))?;
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(frame)) => {
                let close_action = handle_discord_close_frame(&mut session, frame.as_ref());
                let state = session.to_state(discord_close_status(close_action));
                set_worker_state(shared_state, state.clone());
                return Ok(state);
            }
            Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                let state = session.to_state("closed");
                set_worker_state(shared_state, state.clone());
                return Ok(state);
            }
            Err(error) => return Err(error).context("failed to read Discord websocket frame"),
        }
    }
}

fn set_worker_state(shared_state: &Arc<Mutex<Option<IngressState>>>, state: IngressState) {
    if let Ok(mut guard) = shared_state.lock() {
        *guard = Some(state);
    }
}

fn sleep_until_stopped(stop: &AtomicBool, duration: Duration) {
    let mut slept = Duration::ZERO;
    while slept < duration && !stop.load(Ordering::Relaxed) {
        let step = std::cmp::min(Duration::from_millis(250), duration - slept);
        thread::sleep(step);
        slept += step;
    }
}

fn emit_channel_event_notification(
    stdout_lock: &Arc<Mutex<()>>,
    events: Vec<InboundEventEnvelope>,
    state: Option<IngressState>,
    poll_after_ms: Option<u64>,
) -> Result<()> {
    let envelope = PluginNotificationEnvelope {
        protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
        notification: ChannelEventNotification {
            events,
            state,
            poll_after_ms,
        },
    };
    let json = notification_to_jsonrpc(&envelope)
        .map_err(|error| anyhow!("failed to encode channel event notification: {error}"))?;
    write_stdout_line(stdout_lock, &json).context("failed to write channel event notification")
}

struct DiscordGatewaySession {
    last_sequence: Option<u64>,
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    awaiting_heartbeat_ack: bool,
    identified: bool,
}

impl DiscordGatewaySession {
    fn from_state(state: Option<&IngressState>) -> Self {
        Self {
            last_sequence: state
                .and_then(|state| state.metadata.get("sequence"))
                .and_then(|value| value.parse::<u64>().ok()),
            session_id: state
                .and_then(|state| state.metadata.get("session_id"))
                .cloned(),
            resume_gateway_url: state
                .and_then(|state| state.metadata.get("resume_gateway_url"))
                .cloned(),
            heartbeat_interval: Duration::from_secs(45),
            next_heartbeat: Instant::now() + Duration::from_secs(45),
            awaiting_heartbeat_ack: false,
            identified: false,
        }
    }

    fn can_resume(&self) -> bool {
        self.session_id.is_some()
            && self.last_sequence.is_some()
            && self.resume_gateway_url.is_some()
    }

    fn reset_resume(&mut self) {
        self.session_id = None;
        self.last_sequence = None;
        self.resume_gateway_url = None;
    }

    fn to_state(&self, status: &str) -> IngressState {
        websocket_state(
            status,
            self.last_sequence,
            self.session_id.as_deref(),
            self.resume_gateway_url.as_deref(),
        )
    }
}

enum DiscordGatewayAction {
    Event(Box<InboundEventEnvelope>),
    Reconnect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscordGatewayCloseAction {
    Resume,
    Reidentify,
    Stop,
}

fn discord_gateway_action_response(
    action: DiscordGatewayAction,
    session: &DiscordGatewaySession,
) -> PluginResponse {
    match action {
        DiscordGatewayAction::Event(event) => PluginResponse::IngressEventsReceived {
            events: vec![*event],
            callback_reply: None,
            state: Some(session.to_state("running")),
            poll_after_ms: Some(0),
        },
        DiscordGatewayAction::Reconnect => PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: Some(session.to_state("reconnect")),
            poll_after_ms: Some(1000),
        },
    }
}

fn handle_discord_gateway_text(
    config: &ChannelConfig,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    text: &str,
    session: &mut DiscordGatewaySession,
    bot_token: &str,
) -> Result<Option<DiscordGatewayAction>> {
    let payload: DiscordGatewayPayload =
        serde_json::from_str(text).context("failed to parse Discord websocket payload")?;
    if let Some(sequence) = payload.sequence {
        session.last_sequence = Some(sequence);
    }

    match payload.op {
        0 => {
            match payload.event_type.as_deref() {
                Some("READY") => {
                    debug_discord_gateway(format_args!("received READY"));
                    if let Some(session_id) = payload.data.get("session_id").and_then(Value::as_str)
                    {
                        session.session_id = Some(session_id.to_string());
                    }
                    if let Some(resume_gateway_url) = payload
                        .data
                        .get("resume_gateway_url")
                        .and_then(Value::as_str)
                    {
                        session.resume_gateway_url = Some(resume_gateway_url.to_string());
                    }
                    return Ok(None);
                }
                Some("RESUMED") => return Ok(None),
                Some("MESSAGE_CREATE") => {}
                other => {
                    debug_discord_gateway(format_args!("ignoring gateway event {:?}", other));
                    return Ok(None);
                }
            }
            let message: DiscordGatewayMessage = serde_json::from_value(payload.data)
                .context("failed to parse Discord MESSAGE_CREATE payload")?;
            let Some(event) = build_websocket_inbound_event(config, &message)? else {
                return Ok(None);
            };
            Ok(Some(DiscordGatewayAction::Event(Box::new(event))))
        }
        1 => {
            send_discord_session_heartbeat(socket, session)?;
            Ok(None)
        }
        7 => Ok(Some(DiscordGatewayAction::Reconnect)),
        9 => {
            if !payload.data.as_bool().unwrap_or(false) {
                session.reset_resume();
            }
            Ok(Some(DiscordGatewayAction::Reconnect))
        }
        10 => {
            if let Some(interval) = payload
                .data
                .get("heartbeat_interval")
                .and_then(Value::as_u64)
            {
                session.heartbeat_interval = Duration::from_millis(interval);
                session.next_heartbeat = Instant::now() + session.heartbeat_interval;
            }
            if !session.identified {
                if session.can_resume() {
                    send_discord_resume(socket, bot_token, session)?;
                } else {
                    send_discord_identify(socket, bot_token, config)?;
                }
                session.identified = true;
            }
            Ok(None)
        }
        11 => {
            session.awaiting_heartbeat_ack = false;
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn send_discord_identify(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    token: &str,
    config: &ChannelConfig,
) -> Result<()> {
    let payload = json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": discord_gateway_intents(config),
            "properties": {
                "os": "linux",
                "browser": "dispatch",
                "device": "dispatch"
            }
        }
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .context("failed to identify Discord websocket")
}

fn discord_gateway_intents(config: &ChannelConfig) -> u64 {
    let mut intents = DISCORD_GATEWAY_BASE_INTENTS;
    if config.message_content_intent.unwrap_or(false) {
        intents |= DISCORD_GATEWAY_MESSAGE_CONTENT_INTENT;
    }
    intents
}

fn handle_discord_close_frame(
    session: &mut DiscordGatewaySession,
    frame: Option<&CloseFrame>,
) -> DiscordGatewayCloseAction {
    let action = discord_gateway_close_action(frame);
    if matches!(
        action,
        DiscordGatewayCloseAction::Reidentify | DiscordGatewayCloseAction::Stop
    ) {
        session.reset_resume();
    }
    log_discord_close_frame(frame, action);
    action
}

fn discord_gateway_close_action(frame: Option<&CloseFrame>) -> DiscordGatewayCloseAction {
    let Some(frame) = frame else {
        return DiscordGatewayCloseAction::Resume;
    };
    match u16::from(frame.code) {
        4003 | 4007 | 4009 => DiscordGatewayCloseAction::Reidentify,
        4004 | 4010 | 4011 | 4012 | 4013 | 4014 => DiscordGatewayCloseAction::Stop,
        _ => DiscordGatewayCloseAction::Resume,
    }
}

fn discord_close_status(action: DiscordGatewayCloseAction) -> &'static str {
    match action {
        DiscordGatewayCloseAction::Resume | DiscordGatewayCloseAction::Reidentify => "closed",
        DiscordGatewayCloseAction::Stop => "stopped",
    }
}

fn discord_close_poll_after(action: DiscordGatewayCloseAction) -> Option<u64> {
    match action {
        DiscordGatewayCloseAction::Resume | DiscordGatewayCloseAction::Reidentify => Some(1000),
        DiscordGatewayCloseAction::Stop => None,
    }
}

fn log_discord_close_frame(frame: Option<&CloseFrame>, action: DiscordGatewayCloseAction) {
    if let Some(frame) = frame {
        eprintln!(
            "discord websocket closed: code={} reason={} action={:?}",
            u16::from(frame.code),
            frame.reason,
            action
        );
    } else {
        eprintln!("discord websocket closed without a close frame action={action:?}");
    }
}

fn debug_discord_gateway(args: std::fmt::Arguments<'_>) {
    let enabled = std::env::var("DISCORD_GATEWAY_DEBUG")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if enabled {
        eprintln!("discord gateway debug: {args}");
    }
}

fn send_discord_resume(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    token: &str,
    session: &DiscordGatewaySession,
) -> Result<()> {
    let payload = json!({
        "op": 6,
        "d": {
            "token": token,
            "session_id": session.session_id.as_deref().unwrap_or_default(),
            "seq": session.last_sequence,
        }
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .context("failed to resume Discord websocket")
}

fn send_discord_heartbeat(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    last_sequence: Option<u64>,
) -> Result<()> {
    socket
        .send(Message::Text(
            json!({ "op": 1, "d": last_sequence }).to_string().into(),
        ))
        .context("failed to send Discord websocket heartbeat")
}

fn send_discord_session_heartbeat(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    session: &mut DiscordGatewaySession,
) -> Result<()> {
    send_discord_heartbeat(socket, session.last_sequence)?;
    session.awaiting_heartbeat_ack = true;
    session.next_heartbeat = Instant::now() + session.heartbeat_interval;
    Ok(())
}

fn close_discord_websocket_for_stop(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    let close = CloseFrame {
        code: CloseCode::Normal,
        reason: "stopping".into(),
    };
    let _ = socket.close(Some(close));
}

fn close_discord_websocket_for_reconnect(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    let _ = socket.close(None);
}

fn build_websocket_inbound_event(
    config: &ChannelConfig,
    message: &DiscordGatewayMessage,
) -> Result<Option<InboundEventEnvelope>> {
    if message.author.bot.unwrap_or(false) {
        debug_discord_gateway(format_args!(
            "ignoring bot-authored message id={} channel={}",
            message.id, message.channel_id
        ));
        return Ok(None);
    }
    if !guild_is_allowed(config, message.guild_id.as_deref()) {
        debug_discord_gateway(format_args!(
            "ignoring message id={} from disallowed guild={:?}",
            message.id, message.guild_id
        ));
        return Ok(None);
    }
    if !sender_is_allowed(config, &message.author.id, message.guild_id.as_deref()) {
        debug_discord_gateway(format_args!(
            "ignoring message id={} from disallowed sender={}",
            message.id, message.author.id
        ));
        return Ok(None);
    }
    debug_discord_gateway(format_args!(
        "accepting message id={} channel={} guild={:?} content_bytes={}",
        message.id,
        message.channel_id,
        message.guild_id,
        message.content.len()
    ));

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(META_TRANSPORT.to_string(), TRANSPORT_WEBSOCKET.to_string());
    if let Some(guild_id) = &message.guild_id {
        event_metadata.insert(META_GUILD_ID.to_string(), guild_id.clone());
    }
    let mut actor_metadata = BTreeMap::new();
    actor_metadata.insert(META_ACTOR_KIND.to_string(), "user".to_string());

    Ok(Some(InboundEventEnvelope {
        event_id: message.id.clone(),
        platform: PLATFORM_DISCORD.to_string(),
        event_type: "message.created".to_string(),
        received_at: message
            .timestamp
            .clone()
            .unwrap_or_else(|| Timestamp::now().to_string()),
        conversation: InboundConversationRef {
            id: message.channel_id.clone(),
            kind: if message.guild_id.is_some() {
                "channel".to_string()
            } else {
                "direct_message".to_string()
            },
            thread_id: None,
            parent_message_id: message
                .message_reference
                .as_ref()
                .and_then(|reference| reference.message_id.clone()),
        },
        actor: InboundActor {
            id: message.author.id.clone(),
            display_name: message
                .author
                .global_name
                .clone()
                .or_else(|| Some(message.author.username.clone())),
            username: Some(message.author.username.clone()),
            is_bot: false,
            metadata: actor_metadata,
        },
        message: InboundMessage {
            id: message.id.clone(),
            content: message.content.clone(),
            content_type: "text/plain".to_string(),
            reply_to_message_id: message
                .message_reference
                .as_ref()
                .and_then(|reference| reference.message_id.clone()),
            attachments: message.attachments.iter().map(gateway_attachment).collect(),
            metadata: BTreeMap::new(),
        },
        account_id: None,
        metadata: event_metadata,
    }))
}

fn gateway_attachment(attachment: &DiscordGatewayAttachment) -> InboundAttachment {
    InboundAttachment {
        id: Some(attachment.id.clone()),
        kind: "file".to_string(),
        url: Some(attachment.url.clone()),
        mime_type: attachment.content_type.clone(),
        size_bytes: attachment.size,
        name: Some(attachment.filename.clone()),
        storage_key: None,
        extracted_text: None,
        extras: BTreeMap::new(),
    }
}

fn websocket_state(
    status: &str,
    sequence: Option<u64>,
    session_id: Option<&str>,
    resume_gateway_url: Option<&str>,
) -> IngressState {
    let mut state = IngressState {
        mode: IngressMode::Websocket,
        status: status.to_string(),
        endpoint: None,
        metadata: websocket_state_metadata(),
    };
    if let Some(sequence) = sequence {
        state
            .metadata
            .insert("sequence".to_string(), sequence.to_string());
    }
    if let Some(session_id) = session_id {
        state
            .metadata
            .insert("session_id".to_string(), session_id.to_string());
    }
    if let Some(resume_gateway_url) = resume_gateway_url {
        state.metadata.insert(
            "resume_gateway_url".to_string(),
            resume_gateway_url.to_string(),
        );
    }
    state
}

fn websocket_state_metadata() -> BTreeMap<String, String> {
    BTreeMap::from([
        (META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string()),
        (META_TRANSPORT.to_string(), TRANSPORT_WEBSOCKET.to_string()),
    ])
}

fn discord_websocket_timeout_window() -> Duration {
    std::env::var("DISCORD_GATEWAY_RECEIVE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300))
}

fn configure_websocket_read_timeout(
    stream: &mut MaybeTlsStream<TcpStream>,
    timeout: Duration,
) -> Result<()> {
    let tcp = match stream {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Ok(()),
    };
    tcp.set_read_timeout(Some(timeout))
        .context("failed to configure Discord websocket read timeout")
}

fn discord_gateway_base_url<F>(session: &DiscordGatewaySession, fallback: F) -> Result<String>
where
    F: FnOnce() -> Result<String>,
{
    session
        .resume_gateway_url
        .clone()
        .filter(|_| session.can_resume())
        .map(Ok)
        .unwrap_or_else(fallback)
}

fn discord_gateway_websocket_url(gateway_url: &str) -> String {
    let mut base = gateway_url.to_string();
    if !base.contains('?') && !base.ends_with('/') {
        base.push('/');
    }
    format!(
        "{}{}v={}&encoding=json",
        base,
        if base.contains('?') { "&" } else { "?" },
        DISCORD_GATEWAY_VERSION
    )
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let content = render_status_message(update);
    if content.trim().is_empty() {
        return Ok(rejected_status(
            "missing_message",
            "discord status frames require a message or discord_status_text override",
        ));
    }

    let message = OutboundMessage {
        content,
        attachments: Vec::new(),
        channel_id: update
            .conversation_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_CONVERSATION_ID).cloned()),
        thread_id: update
            .thread_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_THREAD_ID).cloned()),
        reply_to_message_id: update.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned(),
        metadata: BTreeMap::new(),
    };

    let delivery = match deliver(config, &message) {
        Ok(delivery) => delivery,
        Err(error) => return Ok(rejected_status("delivery_failed", error.to_string())),
    };

    let mut metadata = delivery.metadata.clone();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    metadata.insert(META_STATUS_KIND.to_string(), status_kind_name(&update.kind));
    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let destination = resolve_destination(config, message)?;
    let reply_to_message_id = resolve_reply_to_message_id(message);
    let upload = discord_upload(message)?;
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let posted = client.send_message(
        &destination,
        &message.content,
        reply_to_message_id.as_deref(),
        upload.as_ref(),
    )?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    metadata.insert(META_CHANNEL_ID.to_string(), posted.channel_id.clone());
    metadata.insert(META_RESOLVED_DESTINATION.to_string(), destination.clone());
    if upload.is_some() {
        metadata.insert(META_ATTACHMENT_COUNT.to_string(), "1".to_string());
    }
    if let Some(reply_to_message_id) = &reply_to_message_id {
        metadata.insert(
            META_REPLY_TO_MESSAGE_ID.to_string(),
            reply_to_message_id.clone(),
        );
    }
    if let Some(thread_id) = resolve_thread_id(message) {
        metadata.insert(META_THREAD_ID.to_string(), thread_id.clone());
    }

    Ok(DeliveryReceipt {
        message_id: posted.id,
        conversation_id: posted.channel_id,
        metadata,
    })
}

fn discord_upload(message: &OutboundMessage) -> Result<Option<DiscordUpload>> {
    match message.attachments.as_slice() {
        [] => Ok(None),
        [attachment] => {
            if attachment.url.is_some() {
                bail!(
                    "discord outbound attachments require data_base64; url attachments are not supported"
                );
            }
            if attachment.storage_key.is_some() {
                bail!(
                    "discord outbound attachments require data_base64; storage_key attachments are not supported"
                );
            }
            let Some(data_base64) = attachment.data_base64.as_deref() else {
                bail!("discord outbound attachments require data_base64");
            };
            let data = BASE64_STANDARD.decode(data_base64).with_context(|| {
                format!(
                    "invalid base64 attachment payload for `{}`",
                    attachment.name
                )
            })?;
            Ok(Some(DiscordUpload {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                data,
            }))
        }
        _ => bail!("discord delivery supports at most one attachment"),
    }
}

fn build_inbound_event(
    config: &ChannelConfig,
    interaction: &DiscordInteraction,
    payload: &IngressPayload,
) -> Result<Option<InboundEventEnvelope>> {
    let event_type = interaction_type_name(interaction.interaction_type);
    let Some(event_type) = event_type else {
        return Ok(None);
    };

    let Some(actor) = inbound_actor(interaction) else {
        return Ok(None);
    };
    if !sender_is_allowed(config, &actor.id, interaction.guild_id.as_deref()) {
        return Ok(None);
    }
    let Some(conversation_id) = interaction.channel_id.clone() else {
        return Ok(None);
    };

    let received_at = received_at(payload.received_at.as_deref());
    let (content, mut message_metadata) = interaction_message(interaction);
    let attachments = extract_source_attachments(interaction, &mut message_metadata);
    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(
        META_TRANSPORT.to_string(),
        TRANSPORT_INTERACTION_WEBHOOK.to_string(),
    );
    event_metadata.insert(META_INTERACTION_TYPE.to_string(), event_type.to_string());
    event_metadata.insert(
        META_APPLICATION_ID.to_string(),
        interaction.application_id.clone(),
    );
    if let Some(endpoint_id) = &payload.endpoint_id {
        event_metadata.insert(META_ENDPOINT_ID.to_string(), endpoint_id.clone());
    }
    if !payload.path.is_empty() {
        event_metadata.insert(META_PATH.to_string(), payload.path.clone());
    }
    if let Some(guild_id) = &interaction.guild_id {
        event_metadata.insert(META_GUILD_ID.to_string(), guild_id.clone());
    }
    if let Some(locale) = &interaction.locale {
        event_metadata.insert(META_LOCALE.to_string(), locale.clone());
    }
    if let Some(guild_locale) = &interaction.guild_locale {
        event_metadata.insert(META_GUILD_LOCALE.to_string(), guild_locale.clone());
    }

    let parent_message_id = interaction
        .message
        .as_ref()
        .map(|message| message.id.clone());
    if let Some(source_message_id) = &parent_message_id {
        message_metadata.insert(
            META_SOURCE_MESSAGE_ID.to_string(),
            source_message_id.clone(),
        );
    }

    Ok(Some(InboundEventEnvelope {
        event_id: interaction.id.clone(),
        platform: PLATFORM_DISCORD.to_string(),
        event_type: event_type.to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: conversation_id,
            kind: conversation_kind(interaction),
            thread_id: None,
            parent_message_id: parent_message_id.clone(),
        },
        actor,
        message: InboundMessage {
            id: source_message_id_or_interaction_id(interaction),
            content,
            content_type: "text/plain".to_string(),
            reply_to_message_id: parent_message_id,
            attachments,
            metadata: message_metadata,
        },
        account_id: Some(interaction.application_id.clone()),
        metadata: event_metadata,
    }))
}

fn extract_source_attachments(
    interaction: &DiscordInteraction,
    message_metadata: &mut BTreeMap<String, String>,
) -> Vec<InboundAttachment> {
    let Some(source_message) = interaction.message.as_ref() else {
        return Vec::new();
    };

    let mut attachments = Vec::new();
    for attachment in &source_message.attachments {
        attachments.push(InboundAttachment {
            id: Some(attachment.id.clone()),
            kind: attachment_kind(attachment.content_type.as_deref()),
            url: Some(attachment.url.clone()),
            mime_type: attachment.content_type.clone(),
            size_bytes: attachment.size,
            name: Some(attachment.filename.clone()),
            storage_key: None,
            extracted_text: attachment.description.clone(),
            extras: BTreeMap::new(),
        });
    }

    if !attachments.is_empty() {
        message_metadata.insert(
            META_ATTACHMENT_COUNT.to_string(),
            attachments.len().to_string(),
        );
    }

    attachments
}

fn attachment_kind(content_type: Option<&str>) -> String {
    match content_type.and_then(|value| value.split('/').next()) {
        Some("image") => "image".to_string(),
        Some("audio") => "audio".to_string(),
        Some("video") => "video".to_string(),
        _ => "file".to_string(),
    }
}

fn interaction_message(interaction: &DiscordInteraction) -> (String, BTreeMap<String, String>) {
    let mut metadata = BTreeMap::new();
    let Some(data) = interaction.data.as_ref() else {
        return (String::new(), metadata);
    };

    match interaction.interaction_type {
        INTERACTION_TYPE_APPLICATION_COMMAND => {
            if let Some(name) = &data.name {
                metadata.insert(META_COMMAND_NAME.to_string(), name.clone());
            }
            if let Some(command_type) = data.command_type {
                metadata.insert(
                    META_COMMAND_KIND.to_string(),
                    command_kind_name(command_type).to_string(),
                );
            }
            let content = render_command_content(data);
            (content, metadata)
        }
        INTERACTION_TYPE_MESSAGE_COMPONENT => {
            if let Some(custom_id) = &data.custom_id {
                metadata.insert(META_CUSTOM_ID.to_string(), custom_id.clone());
            }
            if let Some(component_type) = data.component_type {
                metadata.insert(
                    META_COMPONENT_TYPE.to_string(),
                    component_type_name(component_type).to_string(),
                );
            }
            let content = render_component_content(data);
            (content, metadata)
        }
        INTERACTION_TYPE_MODAL_SUBMIT => {
            if let Some(custom_id) = &data.custom_id {
                metadata.insert(META_CUSTOM_ID.to_string(), custom_id.clone());
            }
            let content = render_modal_content(data);
            (content, metadata)
        }
        _ => (String::new(), metadata),
    }
}

fn render_command_content(data: &DiscordInteractionData) -> String {
    let Some(name) = data.name.as_deref() else {
        return "/command".to_string();
    };
    let mut rendered = format!("/{name}");
    let options = render_command_options(&data.options);
    if !options.is_empty() {
        rendered.push(' ');
        rendered.push_str(&options.join(" "));
    }
    rendered
}

fn render_command_options(options: &[DiscordCommandOption]) -> Vec<String> {
    let mut rendered = Vec::new();
    for option in options {
        if option.options.is_empty() {
            if let Some(value) = &option.value {
                rendered.push(format!("{}={}", option.name, render_json_scalar(value)));
            } else {
                rendered.push(option.name.clone());
            }
            continue;
        }

        rendered.push(option.name.clone());
        rendered.extend(render_command_options(&option.options));
    }
    rendered
}

fn render_component_content(data: &DiscordInteractionData) -> String {
    let custom_id = data.custom_id.as_deref().unwrap_or("component");
    if !data.values.is_empty() {
        return format!("component {custom_id}: {}", data.values.join(", "));
    }
    format!("component {custom_id}")
}

fn render_modal_content(data: &DiscordInteractionData) -> String {
    let custom_id = data.custom_id.as_deref().unwrap_or("modal");
    let values = collect_modal_values(&data.components);
    if values.is_empty() {
        return format!("modal {custom_id}");
    }
    format!("modal {custom_id}: {}", values.join(", "))
}

fn collect_modal_values(rows: &[DiscordComponentRow]) -> Vec<String> {
    let mut values = Vec::new();
    for row in rows {
        values.extend(collect_component_values(&row.components));
    }
    values
}

fn collect_component_values(components: &[DiscordComponentValue]) -> Vec<String> {
    let mut values = Vec::new();
    for component in components {
        if let Some(value) = &component.value {
            if let Some(custom_id) = &component.custom_id {
                values.push(format!("{custom_id}={value}"));
            } else {
                values.push(value.clone());
            }
        }
        if !component.values.is_empty() {
            if let Some(custom_id) = &component.custom_id {
                values.push(format!("{custom_id}={}", component.values.join("|")));
            } else {
                values.extend(component.values.clone());
            }
        }
        if !component.components.is_empty() {
            values.extend(collect_component_values(&component.components));
        }
    }
    values
}

fn render_json_scalar(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn inbound_actor(interaction: &DiscordInteraction) -> Option<InboundActor> {
    if let Some(member) = &interaction.member
        && let Some(user) = &member.user
    {
        let mut metadata = BTreeMap::new();
        metadata.insert(META_ACTOR_KIND.to_string(), "member".to_string());
        if let Some(nick) = &member.nick {
            metadata.insert(META_NICK.to_string(), nick.clone());
        }
        return Some(InboundActor {
            id: user.id.clone(),
            display_name: member
                .nick
                .clone()
                .or_else(|| user.global_name.clone())
                .or_else(|| Some(user.username.clone())),
            username: Some(user.username.clone()),
            is_bot: user.bot.unwrap_or(false),
            metadata,
        });
    }

    interaction.user.as_ref().map(|user| InboundActor {
        id: user.id.clone(),
        display_name: user
            .global_name
            .clone()
            .or_else(|| Some(user.username.clone())),
        username: Some(user.username.clone()),
        is_bot: user.bot.unwrap_or(false),
        metadata: BTreeMap::from([(META_ACTOR_KIND.to_string(), "user".to_string())]),
    })
}

fn validate_ingress_signature(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let public_key = read_required_env(interaction_public_key_env(config))?;
    validate_discord_signature(&public_key, payload)
}

fn validate_discord_signature(
    public_key_hex: &str,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    let Some(signature_hex) = header_value(&payload.headers, HEADER_X_SIGNATURE_ED25519) else {
        return Ok(Some(callback_reply(
            401,
            "discord request signature header missing",
        )));
    };
    let Some(timestamp) = header_value(&payload.headers, HEADER_X_SIGNATURE_TIMESTAMP) else {
        return Ok(Some(callback_reply(
            401,
            "discord request timestamp header missing",
        )));
    };

    // Reject signatures whose timestamp is too far from now. Discord sends
    // Unix epoch seconds; treat unparseable values as untrusted.
    let Ok(signature_ts) = timestamp.parse::<i64>() else {
        return Ok(Some(callback_reply(
            401,
            "discord request timestamp is not a valid unix epoch",
        )));
    };
    let now = Timestamp::now().as_second();
    if now.abs_diff(signature_ts) > DISCORD_MAX_SIGNATURE_AGE_SECS as u64 {
        return Ok(Some(callback_reply(
            401,
            "discord request timestamp outside the accepted window",
        )));
    }

    let public_key_bytes =
        hex::decode(public_key_hex).context("invalid Discord interaction public key")?;
    let public_key_bytes: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| anyhow!("Discord interaction public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .context("invalid Discord interaction public key bytes")?;

    let signature_bytes =
        hex::decode(signature_hex).context("invalid X-Signature-Ed25519 header")?;
    let signature = Signature::try_from(signature_bytes.as_slice())
        .map_err(|_| anyhow!("invalid Discord interaction signature bytes"))?;

    let signed_message = format!("{timestamp}{}", payload.body);
    if verifying_key
        .verify(signed_message.as_bytes(), &signature)
        .is_err()
    {
        return Ok(Some(callback_reply(401, "invalid request signature")));
    }

    Ok(None)
}

fn resolve_destination(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
    if let Some(thread_id) = resolve_thread_id(message) {
        return Ok(thread_id.clone());
    }
    if let Some(channel_id) = &message.channel_id {
        return Ok(channel_id.clone());
    }
    if let Some(channel_id) = message.metadata.get(ROUTE_CONVERSATION_ID) {
        return Ok(channel_id.clone());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        return Ok(default_channel_id.clone());
    }
    Err(anyhow!(
        "discord delivery requires message.thread_id, message.channel_id, or config.default_channel_id"
    ))
}

fn resolve_thread_id(message: &OutboundMessage) -> Option<&String> {
    message
        .thread_id
        .as_ref()
        .or_else(|| message.metadata.get(ROUTE_THREAD_ID))
}

fn resolve_reply_to_message_id(message: &OutboundMessage) -> Option<String> {
    message
        .reply_to_message_id
        .clone()
        .or_else(|| message.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned())
}

fn resolved_endpoint(config: &ChannelConfig) -> Option<String> {
    let base = config.webhook_public_url.as_deref()?.trim_end_matches('/');
    let path = config
        .webhook_path
        .as_deref()
        .unwrap_or("/discord/interactions")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn bot_token_env(config: &ChannelConfig) -> &str {
    config
        .bot_token_env
        .as_deref()
        .unwrap_or("DISCORD_BOT_TOKEN")
}

fn interaction_public_key_env(config: &ChannelConfig) -> &str {
    config
        .interaction_public_key_env
        .as_deref()
        .unwrap_or("DISCORD_INTERACTION_PUBLIC_KEY")
}

fn conversation_kind(interaction: &DiscordInteraction) -> String {
    if interaction.guild_id.is_some() {
        "channel".to_string()
    } else {
        "direct_message".to_string()
    }
}

fn guild_is_allowed(config: &ChannelConfig, guild_id: Option<&str>) -> bool {
    config.allowed_guild_ids.is_empty()
        || guild_id
            .map(|guild_id| {
                config
                    .allowed_guild_ids
                    .iter()
                    .any(|allowed| allowed == guild_id)
            })
            .unwrap_or(false)
}

fn sender_is_allowed(config: &ChannelConfig, sender_id: &str, guild_id: Option<&str>) -> bool {
    if let Some(owner_id) = config.owner_id.as_deref()
        && owner_id != sender_id
    {
        return false;
    }

    if guild_id.is_some() {
        return true;
    }

    match config.dm_policy.as_deref() {
        Some("deny") => false,
        Some("pairing") => id_matches_allowlist(&config.allowed_sender_ids, sender_id),
        Some("open") | None => true,
        Some(_) => true,
    }
}

fn id_matches_allowlist(allowlist: &[String], value: &str) -> bool {
    allowlist
        .iter()
        .any(|allowed| allowed == "*" || allowed == value)
}

fn source_message_id_or_interaction_id(interaction: &DiscordInteraction) -> String {
    interaction
        .message
        .as_ref()
        .map(|message| message.id.clone())
        .unwrap_or_else(|| interaction.id.clone())
}

fn received_at(host_received_at: Option<&str>) -> String {
    host_received_at
        .map(str::to_owned)
        .unwrap_or_else(|| Timestamp::now().to_string())
}

fn ingress_rejection(status: u16, message: &str) -> PluginResponse {
    PluginResponse::IngressEventsReceived {
        events: Vec::new(),
        callback_reply: Some(callback_reply(status, message)),
        state: None,
        poll_after_ms: None,
    }
}

fn callback_reply(status: u16, message: &str) -> IngressCallbackReply {
    IngressCallbackReply {
        status,
        content_type: Some("text/plain; charset=utf-8".to_string()),
        body: message.to_string(),
    }
}

fn discord_json_reply(body: Value) -> IngressCallbackReply {
    IngressCallbackReply {
        status: 200,
        content_type: Some("application/json".to_string()),
        body: body.to_string(),
    }
}

fn discord_ephemeral_message(message: &str) -> IngressCallbackReply {
    discord_json_reply(json!({
        "type": 4,
        "data": {
            "content": message,
            "flags": 64
        }
    }))
}

fn render_status_message(update: &StatusFrame) -> String {
    if let Some(status_text) = update.metadata.get(DISCORD_STATUS_TEXT) {
        return status_text.clone();
    }

    let prefix = match update.kind {
        StatusKind::Processing => "Processing",
        StatusKind::Completed => "Completed",
        StatusKind::Cancelled => "Cancelled",
        StatusKind::OperationStarted => "Started",
        StatusKind::OperationFinished => "Finished",
        StatusKind::ApprovalNeeded => "Approval needed",
        StatusKind::Info => "Info",
        StatusKind::Delivering => "Delivering",
        StatusKind::AuthRequired => "Authentication required",
        _ => "Status",
    };

    if update.message.trim().is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {}", update.message)
    }
}

fn interaction_type_name(interaction_type: u8) -> Option<&'static str> {
    match interaction_type {
        INTERACTION_TYPE_APPLICATION_COMMAND => Some("application_command"),
        INTERACTION_TYPE_MESSAGE_COMPONENT => Some("message_component"),
        INTERACTION_TYPE_MODAL_SUBMIT => Some("modal_submit"),
        _ => None,
    }
}

fn command_kind_name(command_type: u8) -> &'static str {
    match command_type {
        COMMAND_KIND_CHAT_INPUT => "chat_input",
        COMMAND_KIND_USER => "user",
        COMMAND_KIND_MESSAGE => "message",
        _ => "unknown",
    }
}

fn component_type_name(component_type: u8) -> &'static str {
    match component_type {
        COMPONENT_KIND_BUTTON => "button",
        COMPONENT_KIND_STRING_SELECT => "string_select",
        COMPONENT_KIND_TEXT_INPUT => "text_input",
        COMPONENT_KIND_USER_SELECT => "user_select",
        COMPONENT_KIND_ROLE_SELECT => "role_select",
        COMPONENT_KIND_MENTIONABLE_SELECT => "mentionable_select",
        COMPONENT_KIND_CHANNEL_SELECT => "channel_select",
        _ => "unknown",
    }
}

fn ingress_mode_name(mode: IngressMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "interaction_webhook".to_string())
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn rejected_status(reason_code: &str, reason: impl Into<String>) -> StatusAcceptance {
    StatusAcceptance {
        accepted: false,
        metadata: BTreeMap::from([
            (META_REASON_CODE.to_string(), reason_code.to_string()),
            (META_REASON.to_string(), reason.into()),
        ]),
    }
}

fn status_kind_name(kind: &StatusKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn read_required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required for the discord channel"))
}

#[derive(Debug, Deserialize)]
struct DiscordInteraction {
    id: String,
    application_id: String,
    #[serde(rename = "type")]
    interaction_type: u8,
    #[serde(default)]
    data: Option<DiscordInteractionData>,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    member: Option<DiscordInteractionMember>,
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    message: Option<DiscordSourceMessage>,
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    guild_locale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionData {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    command_type: Option<u8>,
    #[serde(default)]
    custom_id: Option<String>,
    #[serde(default)]
    component_type: Option<u8>,
    #[serde(default)]
    options: Vec<DiscordCommandOption>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    components: Vec<DiscordComponentRow>,
}

#[derive(Debug, Deserialize)]
struct DiscordCommandOption {
    name: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    options: Vec<DiscordCommandOption>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionMember {
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    nick: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    bot: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DiscordSourceMessage {
    id: String,
    #[serde(default)]
    attachments: Vec<DiscordSourceAttachment>,
}

#[derive(Debug, Deserialize)]
struct DiscordSourceAttachment {
    id: String,
    filename: String,
    url: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayPayload {
    op: u64,
    #[serde(default, rename = "s")]
    sequence: Option<u64>,
    #[serde(default, rename = "t")]
    event_type: Option<String>,
    #[serde(default, rename = "d")]
    data: Value,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayMessage {
    id: String,
    channel_id: String,
    content: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    guild_id: Option<String>,
    author: DiscordUser,
    #[serde(default)]
    attachments: Vec<DiscordGatewayAttachment>,
    #[serde(default)]
    message_reference: Option<DiscordGatewayMessageReference>,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayAttachment {
    id: String,
    filename: String,
    url: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayMessageReference {
    #[serde(default)]
    message_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordComponentRow {
    #[serde(default)]
    components: Vec<DiscordComponentValue>,
}

#[derive(Debug, Deserialize)]
struct DiscordComponentValue {
    #[serde(default)]
    custom_id: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    components: Vec<DiscordComponentValue>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::OutboundAttachment;
    use ed25519_dalek::{Signer, SigningKey};

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn base_payload(body: &str) -> IngressPayload {
        IngressPayload {
            endpoint_id: Some("discord:/discord/interactions".to_string()),
            method: "POST".to_string(),
            path: "/discord/interactions".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: body.to_string(),
            trust_verified: true,
            received_at: Some("2026-04-11T21:00:00Z".to_string()),
        }
    }

    #[test]
    fn ping_returns_pong_callback() {
        let payload = base_payload(r#"{"id":"1","application_id":"app-1","type":1}"#);

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(events.is_empty());
                let reply = callback_reply.expect("pong reply");
                assert_eq!(reply.status, 200);
                assert_eq!(reply.content_type.as_deref(), Some("application/json"));
                assert_eq!(reply.body, r#"{"type":1}"#);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn command_interaction_maps_to_inbound_event() {
        let payload = base_payload(
            r#"{
                "id":"interaction-1",
                "application_id":"app-1",
                "type":2,
                "guild_id":"guild-1",
                "channel_id":"channel-1",
                "locale":"en-US",
                "guild_locale":"en-US",
                "member":{
                    "nick":"Dispatch User",
                    "user":{
                        "id":"user-1",
                        "username":"dispatch-user",
                        "global_name":"Dispatch User"
                    }
                },
                "data":{
                    "name":"ask",
                    "type":1,
                    "options":[
                        {
                            "name":"query",
                            "value":"hello world"
                        }
                    ]
                }
            }"#,
        );

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert_eq!(events.len(), 1);
                let reply = callback_reply.expect("interaction ack");
                assert_eq!(reply.status, 200);
                assert!(reply.body.contains("Dispatch is processing your request."));

                let event = &events[0];
                assert_eq!(event.event_id, "interaction-1");
                assert_eq!(event.platform, PLATFORM_DISCORD);
                assert_eq!(event.event_type, "application_command");
                assert_eq!(event.account_id.as_deref(), Some("app-1"));
                assert_eq!(event.conversation.id, "channel-1");
                assert_eq!(event.conversation.kind, "channel");
                assert_eq!(event.actor.id, "user-1");
                assert_eq!(event.actor.display_name.as_deref(), Some("Dispatch User"));
                assert_eq!(event.message.id, "interaction-1");
                assert_eq!(event.message.content, "/ask query=hello world");
                assert_eq!(
                    event.metadata.get(META_TRANSPORT).map(String::as_str),
                    Some(TRANSPORT_INTERACTION_WEBHOOK)
                );
                assert_eq!(
                    event.metadata.get(META_ENDPOINT_ID).map(String::as_str),
                    Some("discord:/discord/interactions")
                );
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_COMMAND_NAME)
                        .map(String::as_str),
                    Some("ask")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn websocket_message_maps_to_inbound_event() {
        let config = ChannelConfig::default();
        let message = DiscordGatewayMessage {
            id: "msg-1".to_string(),
            channel_id: "channel-1".to_string(),
            content: "hello websocket".to_string(),
            timestamp: Some("2026-05-04T00:00:00Z".to_string()),
            guild_id: Some("guild-1".to_string()),
            author: DiscordUser {
                id: "user-1".to_string(),
                username: "sender".to_string(),
                global_name: Some("Sender".to_string()),
                bot: Some(false),
            },
            attachments: Vec::new(),
            message_reference: None,
        };

        let event = build_websocket_inbound_event(&config, &message)
            .unwrap()
            .expect("message should map");

        assert_eq!(event.event_id, "msg-1");
        assert_eq!(event.event_type, "message.created");
        assert_eq!(event.message.content, "hello websocket");
        assert_eq!(event.conversation.id, "channel-1");
        assert_eq!(
            event.metadata.get(META_TRANSPORT).map(String::as_str),
            Some(TRANSPORT_WEBSOCKET)
        );
    }

    #[test]
    fn start_ingress_without_webhook_uses_websocket_mode() {
        let guard = EnvGuard::set("DISCORD_BOT_TOKEN", "token");
        let config = ChannelConfig::default();

        let state = start_ingress(&config).unwrap();

        assert_eq!(state.mode, IngressMode::Websocket);
        assert_eq!(
            state.metadata.get(META_TRANSPORT).map(String::as_str),
            Some(TRANSPORT_WEBSOCKET)
        );
        drop(guard);
    }

    #[test]
    fn websocket_intents_do_not_request_message_content_by_default() {
        let config = ChannelConfig::default();

        assert_eq!(
            discord_gateway_intents(&config),
            DISCORD_GATEWAY_BASE_INTENTS
        );
    }

    #[test]
    fn websocket_intents_can_request_message_content() {
        let config = ChannelConfig {
            message_content_intent: Some(true),
            ..ChannelConfig::default()
        };

        assert_eq!(
            discord_gateway_intents(&config),
            DISCORD_GATEWAY_BASE_INTENTS | DISCORD_GATEWAY_MESSAGE_CONTENT_INTENT
        );
    }

    #[test]
    fn websocket_state_preserves_resume_metadata() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
        );
        let session = DiscordGatewaySession::from_state(Some(&state));

        assert_eq!(session.last_sequence, Some(42));
        assert_eq!(session.session_id.as_deref(), Some("session-1"));
        assert_eq!(
            session.resume_gateway_url.as_deref(),
            Some("wss://resume.discord.gg")
        );
        assert!(session.can_resume());
    }

    #[test]
    fn websocket_resume_url_is_preferred_over_gateway_lookup() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
        );
        let session = DiscordGatewaySession::from_state(Some(&state));

        let gateway_url = discord_gateway_base_url(&session, || {
            Err(anyhow!(
                "gateway lookup should not be used when resume URL exists"
            ))
        })
        .expect("resume URL");

        assert_eq!(gateway_url, "wss://resume.discord.gg");
        assert_eq!(
            discord_gateway_websocket_url(&gateway_url),
            "wss://resume.discord.gg/?v=10&encoding=json"
        );
    }

    #[test]
    fn websocket_resume_requires_resume_gateway_url() {
        let state = websocket_state("running", Some(42), Some("session-1"), None);
        let session = DiscordGatewaySession::from_state(Some(&state));

        let gateway_url =
            discord_gateway_base_url(&session, || Ok("wss://gateway.discord.gg".to_string()))
                .expect("fallback URL");

        assert!(!session.can_resume());
        assert_eq!(gateway_url, "wss://gateway.discord.gg");
    }

    #[test]
    fn websocket_non_reconnectable_close_stops_worker() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
        );
        let mut session = DiscordGatewaySession::from_state(Some(&state));
        let frame = CloseFrame {
            code: CloseCode::from(4014),
            reason: "disallowed intents".into(),
        };

        let action = handle_discord_close_frame(&mut session, Some(&frame));

        assert_eq!(action, DiscordGatewayCloseAction::Stop);
        assert!(!session.can_resume());
        assert_eq!(discord_close_status(action), "stopped");
        assert_eq!(discord_close_poll_after(action), None);
    }

    #[test]
    fn websocket_invalid_sequence_close_reidentifies() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
        );
        let mut session = DiscordGatewaySession::from_state(Some(&state));
        let frame = CloseFrame {
            code: CloseCode::from(4007),
            reason: "invalid seq".into(),
        };

        let action = handle_discord_close_frame(&mut session, Some(&frame));

        assert_eq!(action, DiscordGatewayCloseAction::Reidentify);
        assert!(!session.can_resume());
        assert_eq!(discord_close_status(action), "closed");
        assert_eq!(discord_close_poll_after(action), Some(1000));
    }

    #[test]
    fn websocket_missing_close_code_preserves_resume_metadata() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
        );
        let mut session = DiscordGatewaySession::from_state(Some(&state));

        let action = handle_discord_close_frame(&mut session, None);

        assert_eq!(action, DiscordGatewayCloseAction::Resume);
        assert!(session.can_resume());
        assert_eq!(
            session.resume_gateway_url.as_deref(),
            Some("wss://resume.discord.gg")
        );
    }

    #[test]
    fn websocket_message_ignores_bot_author() {
        let config = ChannelConfig::default();
        let message = DiscordGatewayMessage {
            id: "msg-1".to_string(),
            channel_id: "channel-1".to_string(),
            content: "ignore me".to_string(),
            timestamp: None,
            guild_id: None,
            author: DiscordUser {
                id: "bot-1".to_string(),
                username: "bot".to_string(),
                global_name: None,
                bot: Some(true),
            },
            attachments: Vec::new(),
            message_reference: None,
        };

        assert!(
            build_websocket_inbound_event(&config, &message)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn component_interaction_preserves_source_message_attachments() {
        let payload = base_payload(
            r#"{
                "id":"interaction-2",
                "application_id":"app-1",
                "type":3,
                "guild_id":"guild-1",
                "channel_id":"channel-1",
                "member":{
                    "user":{
                        "id":"user-2",
                        "username":"dispatch-user"
                    }
                },
                "data":{
                    "custom_id":"approve",
                    "component_type":2
                },
                "message":{
                    "id":"message-99",
                    "attachments":[
                        {
                            "id":"attachment-1",
                            "filename":"design.png",
                            "url":"https://cdn.discordapp.com/attachments/design.png",
                            "content_type":"image/png",
                            "size":4096,
                            "description":"wireframe"
                        }
                    ]
                }
            }"#,
        );

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert_eq!(events.len(), 1);
                assert!(callback_reply.is_some());

                let event = &events[0];
                assert_eq!(event.event_type, "message_component");
                assert_eq!(event.message.id, "message-99");
                assert_eq!(
                    event.message.reply_to_message_id.as_deref(),
                    Some("message-99")
                );
                assert_eq!(event.message.attachments.len(), 1);

                let attachment = &event.message.attachments[0];
                assert_eq!(attachment.id.as_deref(), Some("attachment-1"));
                assert_eq!(attachment.kind, "image");
                assert_eq!(
                    attachment.url.as_deref(),
                    Some("https://cdn.discordapp.com/attachments/design.png")
                );
                assert_eq!(attachment.mime_type.as_deref(), Some("image/png"));
                assert_eq!(attachment.size_bytes, Some(4096));
                assert_eq!(attachment.name.as_deref(), Some("design.png"));
                assert_eq!(attachment.extracted_text.as_deref(), Some("wireframe"));
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_ATTACHMENT_COUNT)
                        .map(String::as_str),
                    Some("1")
                );
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_CUSTOM_ID)
                        .map(String::as_str),
                    Some("approve")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let mut payload = base_payload(r#"{"id":"1","application_id":"app-1","type":1}"#);
        payload.trust_verified = false;
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_ED25519.to_string(), "00".repeat(64));
        payload.headers.insert(
            HEADER_X_SIGNATURE_TIMESTAMP.to_string(),
            Timestamp::now().as_second().to_string(),
        );

        let reply = validate_discord_signature("11".repeat(32).as_str(), &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(reply.body, "invalid request signature");
    }

    #[test]
    fn valid_signature_is_accepted() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        let timestamp = Timestamp::now().as_second().to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), timestamp);

        let reply =
            validate_discord_signature(&verify_key_hex, &payload).expect("validate signature");

        assert!(reply.is_none());
    }

    #[test]
    fn stale_timestamp_is_rejected() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        // Sign with a timestamp well outside the freshness window so we cannot
        // be replayed even with a cryptographically valid signature.
        let stale_timestamp =
            (Timestamp::now().as_second() - DISCORD_MAX_SIGNATURE_AGE_SECS - 60).to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{stale_timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), stale_timestamp);

        let reply = validate_discord_signature(&verify_key_hex, &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(
            reply.body,
            "discord request timestamp outside the accepted window"
        );
    }

    #[test]
    fn future_timestamp_outside_window_is_rejected() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        let future_timestamp =
            (Timestamp::now().as_second() + DISCORD_MAX_SIGNATURE_AGE_SECS + 60).to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{future_timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), future_timestamp);

        let reply = validate_discord_signature(&verify_key_hex, &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(
            reply.body,
            "discord request timestamp outside the accepted window"
        );
    }

    #[test]
    fn absurdly_negative_timestamp_is_rejected_without_panicking() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        let timestamp = i64::MIN.to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), timestamp);

        let reply = validate_discord_signature(&verify_key_hex, &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(
            reply.body,
            "discord request timestamp outside the accepted window"
        );
    }

    #[test]
    fn direct_message_is_dropped_when_pairing_policy_has_no_allowlist_match() {
        let payload = base_payload(
            r#"{
                "id":"interaction-3",
                "application_id":"app-1",
                "type":2,
                "channel_id":"dm-channel-1",
                "member":{
                    "user":{
                        "id":"user-9",
                        "username":"dispatch-user"
                    }
                },
                "data":{
                    "name":"ask",
                    "type":1,
                    "options":[
                        {
                            "name":"query",
                            "value":"hello world"
                        }
                    ]
                }
            }"#,
        );

        let response = handle_ingress_event(
            &ChannelConfig {
                dm_policy: Some("pairing".to_string()),
                ..ChannelConfig::default()
            },
            &payload,
        )
        .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => assert!(events.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn render_status_message_uses_override() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "ignored".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::from([(DISCORD_STATUS_TEXT.to_string(), "custom".to_string())]),
        };

        assert_eq!(render_status_message(&update), "custom");
    }

    #[test]
    fn send_status_rejects_missing_destination() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "hello".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        let acceptance = send_status(&ChannelConfig::default(), &update).expect("send status");

        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some("delivery_failed")
        );
    }

    #[test]
    fn discord_upload_decodes_inline_attachment_data() {
        let message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: None,
                storage_key: None,
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };

        let upload = discord_upload(&message)
            .expect("inline attachment")
            .expect("upload present");
        assert_eq!(upload.name, "report.txt");
        assert_eq!(upload.mime_type, "text/plain");
        assert_eq!(upload.data, b"hello");
    }

    #[test]
    fn discord_upload_rejects_url_and_storage_key_sources() {
        let url_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: Some("https://example.com/report.txt".to_string()),
                storage_key: None,
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            discord_upload(&url_message)
                .expect_err("url attachments rejected")
                .to_string()
                .contains("url attachments are not supported")
        );

        let staged_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: None,
                storage_key: Some("cache://report.txt".to_string()),
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            discord_upload(&staged_message)
                .expect_err("storage_key attachments rejected")
                .to_string()
                .contains("storage_key attachments are not supported")
        );
    }
}
