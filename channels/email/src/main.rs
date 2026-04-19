use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use jiff::Timestamp;
use mail_parser::{MessageParser, MimeHeaders};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

mod imap_smtp;
mod protocol;

use imap_smtp::{
    FetchedEmail, MailboxStatus, OutgoingAttachment, OutgoingEmail, SentEmail, check_imap_health,
    ensure_configured, fetch_messages_since, has_imap_config, has_smtp_config, imap_mailbox,
    imap_password_env, latest_existing_uid, require_imap_config, send_email, smtp_connection_ok,
    smtp_from_email, smtp_password_env,
};
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ChannelEventNotification, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressMode, IngressState, OutboundMessage,
    PluginNotificationEnvelope, PluginRequest, PluginRequestEnvelope, PluginResponse,
    StatusAcceptance, capabilities, notification_to_jsonrpc, parse_jsonrpc_request, plugin_error,
    response_to_jsonrpc,
};

const META_PLATFORM: &str = "platform";
const META_IMAP_HOST: &str = "imap_host";
const META_IMAP_PORT: &str = "imap_port";
const META_IMAP_USERNAME: &str = "imap_username";
const META_IMAP_PASSWORD_ENV: &str = "imap_password_env";
const META_IMAP_MAILBOX: &str = "imap_mailbox";
const META_SMTP_HOST: &str = "smtp_host";
const META_SMTP_PORT: &str = "smtp_port";
const META_SMTP_USERNAME: &str = "smtp_username";
const META_SMTP_PASSWORD_ENV: &str = "smtp_password_env";
const META_SMTP_FROM_EMAIL: &str = "smtp_from_email";
const META_DEFAULT_RECIPIENT: &str = "default_recipient";
const META_DEFAULT_SUBJECT: &str = "default_subject";
const META_POLL_INTERVAL_SECS: &str = "poll_interval_secs";
const META_UID_VALIDITY: &str = "uid_validity";
const META_LAST_UID: &str = "last_uid";
const META_MAILBOX_EXISTS: &str = "mailbox_exists";
const META_MAILBOX_RECENT: &str = "mailbox_recent";
const META_TRANSPORT: &str = "transport";
const META_SUBJECT: &str = "subject";
const META_REPLY_ADDRESS: &str = "reply_address";
const META_MESSAGE_ID: &str = "message_id";
const META_REASON_CODE: &str = "reason_code";
const META_ATTACHMENT_COUNT: &str = "attachment_count";

const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const ROUTE_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";

const PLATFORM_EMAIL: &str = "email";
const TRANSPORT_IMAP: &str = "imap";
const DEFAULT_POLL_INTERVAL_SECS: u16 = 60;
const MIN_POLL_INTERVAL_SECS: u16 = 5;

struct IngressWorker {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Option<IngressState>>>,
    handle: JoinHandle<()>,
}

fn main() -> Result<()> {
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
    ingress_worker: &mut Option<IngressWorker>,
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
        PluginRequest::StartIngress { config, state } => {
            let started = start_ingress(config)?;
            let started = match (&started.mode, state.clone()) {
                (IngressMode::Polling, Some(state)) if state.mode == IngressMode::Polling => state,
                _ => started,
            };
            if matches!(started.mode, IngressMode::Polling) {
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
        PluginRequest::IngressEvent { .. } => Ok(plugin_error(
            "webhook_not_supported",
            "email ingress is IMAP-backed and does not accept webhook payloads",
        )),
        PluginRequest::Status { .. } => Ok(PluginResponse::StatusAccepted {
            status: rejected_status("not_supported", "email status frames are not supported"),
        }),
        PluginRequest::Shutdown => {
            let _ = stop_ingress_worker(ingress_worker);
            Ok(PluginResponse::Ok)
        }
    }
}

fn restart_ingress_worker(
    worker: &mut Option<IngressWorker>,
    config: ChannelConfig,
    state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
) {
    let _ = stop_ingress_worker(worker);
    *worker = Some(spawn_ingress_worker(config, state, stdout_lock));
}

fn spawn_ingress_worker(
    config: ChannelConfig,
    initial_state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
) -> IngressWorker {
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(Some(initial_state)));
    let stop_flag = Arc::clone(&stop);
    let shared_state = Arc::clone(&state);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            let prior_state = shared_state
                .lock()
                .expect("email ingress state poisoned")
                .clone();
            match poll_ingress(&config, prior_state) {
                Ok(PluginResponse::IngressEventsReceived {
                    events,
                    state,
                    poll_after_ms,
                    ..
                }) => {
                    if let Some(next_state) = state.clone() {
                        *shared_state.lock().expect("email ingress state poisoned") =
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
                        eprintln!("email ingress worker failed to emit notification: {error}");
                        break;
                    }
                }
                Ok(PluginResponse::Error { error }) => {
                    eprintln!(
                        "email ingress worker stopped after plugin error {}: {}",
                        error.code, error.message
                    );
                    break;
                }
                Ok(other) => {
                    eprintln!(
                        "email ingress worker received unexpected response variant: {:?}",
                        other
                    );
                    break;
                }
                Err(error) => {
                    eprintln!("email ingress worker stopped after receive failure: {error}");
                    break;
                }
            }

            sleep_until_next_cycle(&stop_flag, poll_interval_secs(&config));
        }
    });

    IngressWorker {
        stop,
        state,
        handle,
    }
}

fn stop_ingress_worker(worker: &mut Option<IngressWorker>) -> Option<IngressState> {
    let worker = worker.take()?;
    worker.stop.store(true, Ordering::Relaxed);
    let _ = worker.handle.join();
    worker.state.lock().ok().and_then(|state| (*state).clone())
}

fn emit_channel_event_notification(
    stdout_lock: &Arc<Mutex<()>>,
    notification: ChannelEventNotification,
) -> Result<()> {
    let envelope = PluginNotificationEnvelope {
        protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
        notification,
    };
    let json = notification_to_jsonrpc(&envelope).map_err(|error| anyhow!(error))?;
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

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    ensure_configured(config)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_EMAIL.to_string());
    metadata.insert(
        META_POLL_INTERVAL_SECS.to_string(),
        poll_interval_secs(config).to_string(),
    );

    if has_imap_config(config) {
        metadata.insert(
            META_IMAP_HOST.to_string(),
            required_clone(&config.imap_host)?,
        );
        metadata.insert(
            META_IMAP_PORT.to_string(),
            config.imap_port.unwrap_or(993).to_string(),
        );
        metadata.insert(
            META_IMAP_USERNAME.to_string(),
            required_clone(&config.imap_username)?,
        );
        metadata.insert(
            META_IMAP_PASSWORD_ENV.to_string(),
            imap_password_env(config).to_string(),
        );
        metadata.insert(META_IMAP_MAILBOX.to_string(), imap_mailbox(config));
    }

    if has_smtp_config(config) {
        metadata.insert(
            META_SMTP_HOST.to_string(),
            required_clone(&config.smtp_host)?,
        );
        metadata.insert(
            META_SMTP_PORT.to_string(),
            config.smtp_port.unwrap_or(587).to_string(),
        );
        metadata.insert(
            META_SMTP_USERNAME.to_string(),
            config
                .smtp_username
                .clone()
                .or_else(|| config.imap_username.clone())
                .ok_or_else(|| anyhow!("SMTP username is required"))?,
        );
        metadata.insert(
            META_SMTP_PASSWORD_ENV.to_string(),
            smtp_password_env(config).to_string(),
        );
        metadata.insert(META_SMTP_FROM_EMAIL.to_string(), smtp_from_email(config)?);
    }

    if let Some(default_recipient) = &config.default_recipient {
        metadata.insert(
            META_DEFAULT_RECIPIENT.to_string(),
            default_recipient.clone(),
        );
    }
    if let Some(default_subject) = &config.default_subject {
        metadata.insert(META_DEFAULT_SUBJECT.to_string(), default_subject.clone());
    }

    Ok(ConfiguredChannel {
        metadata,
        policy: None,
        runtime: None,
    })
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    ensure_configured(config)?;

    let mut ok = true;
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_EMAIL.to_string());

    let account_id = if has_imap_config(config) {
        config.imap_username.clone()
    } else if has_smtp_config(config) {
        Some(smtp_from_email(config)?)
    } else {
        None
    };

    if has_imap_config(config) {
        let mailbox = check_imap_health(config)?;
        metadata.insert(
            META_IMAP_HOST.to_string(),
            required_clone(&config.imap_host)?,
        );
        metadata.insert(META_IMAP_MAILBOX.to_string(), mailbox.mailbox.clone());
        metadata.insert(META_MAILBOX_EXISTS.to_string(), mailbox.exists.to_string());
        metadata.insert(META_MAILBOX_RECENT.to_string(), mailbox.recent.to_string());
        if let Some(uid_validity) = mailbox.uid_validity {
            metadata.insert(META_UID_VALIDITY.to_string(), uid_validity.to_string());
        }
    }

    if has_smtp_config(config) {
        let smtp_ok = smtp_connection_ok(config)?;
        ok &= smtp_ok;
        metadata.insert(
            META_SMTP_HOST.to_string(),
            required_clone(&config.smtp_host)?,
        );
        metadata.insert(META_SMTP_FROM_EMAIL.to_string(), smtp_from_email(config)?);
    }

    Ok(HealthReport {
        ok,
        status: if ok { "ok" } else { "degraded" }.to_string(),
        account_id,
        display_name: config
            .smtp_from_name
            .clone()
            .or_else(|| config.imap_username.clone()),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    require_imap_config(config)?;
    let mailbox = check_imap_health(config)?;
    Ok(polling_state(
        config,
        "running",
        &mailbox,
        latest_existing_uid(&mailbox),
    ))
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    if !has_imap_config(config) {
        return Ok(state.unwrap_or_else(|| IngressState {
            mode: IngressMode::Polling,
            status: "stopped".to_string(),
            endpoint: None,
            metadata: BTreeMap::new(),
        }));
    }

    let mailbox = check_imap_health(config)?;
    let last_uid = state
        .as_ref()
        .and_then(|state| state.metadata.get(META_LAST_UID))
        .map(|value| parse_u32("state.metadata.last_uid", value))
        .transpose()?
        .or_else(|| latest_existing_uid(&mailbox));
    Ok(polling_state(config, "stopped", &mailbox, last_uid))
}

fn poll_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<PluginResponse> {
    require_imap_config(config)?;

    let last_uid = state
        .as_ref()
        .and_then(|state| state.metadata.get(META_LAST_UID))
        .map(|value| parse_u32("state.metadata.last_uid", value))
        .transpose()?;
    let prior_uid_validity = state
        .as_ref()
        .and_then(|state| state.metadata.get(META_UID_VALIDITY))
        .map(|value| parse_u32("state.metadata.uid_validity", value))
        .transpose()?;

    let poll = fetch_messages_since(config, last_uid)?;
    if prior_uid_validity.is_some() && poll.mailbox.uid_validity != prior_uid_validity {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: Some(polling_state(
                config,
                "running",
                &poll.mailbox,
                latest_existing_uid(&poll.mailbox),
            )),
            poll_after_ms: Some(u64::from(poll_interval_secs(config)) * 1_000),
        });
    }

    let mut events = Vec::new();
    for fetched in &poll.messages {
        if let Some(event) = build_inbound_event(config, &poll.mailbox, fetched)? {
            events.push(event);
        }
    }

    Ok(PluginResponse::IngressEventsReceived {
        events,
        callback_reply: None,
        state: Some(polling_state(
            config,
            "running",
            &poll.mailbox,
            poll.latest_uid,
        )),
        poll_after_ms: Some(u64::from(poll_interval_secs(config)) * 1_000),
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let outgoing = resolve_outgoing_email(config, message)?;
    let sent = send_email(config, &outgoing)?;
    Ok(delivery_receipt(config, &outgoing, &sent))
}

fn resolve_outgoing_email(
    config: &ChannelConfig,
    message: &OutboundMessage,
) -> Result<OutgoingEmail> {
    let recipient = message
        .metadata
        .get(ROUTE_CONVERSATION_ID)
        .cloned()
        .or_else(|| config.default_recipient.clone())
        .ok_or_else(|| {
            anyhow!(
                "email delivery requires message.metadata.conversation_id or config.default_recipient"
            )
        })?;
    let subject = message
        .metadata
        .get(META_SUBJECT)
        .cloned()
        .or_else(|| message.metadata.get(ROUTE_THREAD_ID).cloned())
        .or_else(|| config.default_subject.clone())
        .ok_or_else(|| {
            anyhow!("email delivery requires message.metadata.thread_id or config.default_subject")
        })?;
    let reply_to_message_id = message
        .metadata
        .get(ROUTE_REPLY_TO_MESSAGE_ID)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let subject = apply_reply_subject(subject, reply_to_message_id.is_some());

    let mut attachments = Vec::with_capacity(message.attachments.len());
    for attachment in &message.attachments {
        let data_base64 = attachment.data_base64.clone().ok_or_else(|| {
            anyhow!(
                "email outbound attachments require data_base64 for attachment `{}`",
                attachment.name
            )
        })?;
        let data = BASE64_STANDARD
            .decode(data_base64)
            .with_context(|| format!("attachment `{}` is not valid base64", attachment.name))?;
        attachments.push(OutgoingAttachment {
            name: attachment.name.clone(),
            mime_type: attachment.mime_type.clone(),
            data,
        });
    }

    Ok(OutgoingEmail {
        from_email: smtp_from_email(config)?,
        from_name: config.smtp_from_name.clone(),
        to: recipient,
        subject,
        body_text: message.content.clone(),
        in_reply_to: reply_to_message_id
            .clone()
            .map(|value| canonical_message_id(&value)),
        references: reply_to_message_id
            .into_iter()
            .map(|value| canonical_message_id(&value))
            .collect(),
        attachments,
    })
}

fn delivery_receipt(
    _config: &ChannelConfig,
    outgoing: &OutgoingEmail,
    sent: &SentEmail,
) -> DeliveryReceipt {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_EMAIL.to_string());
    metadata.insert(META_SUBJECT.to_string(), outgoing.subject.clone());
    metadata.insert(
        META_ATTACHMENT_COUNT.to_string(),
        outgoing.attachments.len().to_string(),
    );
    if let Some(in_reply_to) = &outgoing.in_reply_to {
        metadata.insert(ROUTE_REPLY_TO_MESSAGE_ID.to_string(), in_reply_to.clone());
    }

    DeliveryReceipt {
        message_id: sent.message_id.clone(),
        conversation_id: outgoing.to.clone(),
        metadata,
    }
}

fn build_inbound_event(
    config: &ChannelConfig,
    mailbox: &MailboxStatus,
    fetched: &FetchedEmail,
) -> Result<Option<InboundEventEnvelope>> {
    let parsed = MessageParser::default()
        .parse(&fetched.raw_message)
        .ok_or_else(|| anyhow!("failed to parse fetched email UID {}", fetched.uid))?;
    let sender = parsed
        .from()
        .and_then(|addresses| addresses.first())
        .and_then(address_email)
        .map(|email| {
            (
                email.to_string(),
                parsed
                    .from()
                    .and_then(|addresses| addresses.first())
                    .and_then(|addr| addr.name())
                    .map(str::to_string),
            )
        });
    let reply_address = parsed
        .reply_to()
        .and_then(|addresses| addresses.first())
        .and_then(address_email)
        .map(str::to_string)
        .or_else(|| sender.as_ref().map(|(email, _)| email.clone()));

    let Some((sender_email, sender_name)) = sender else {
        return Ok(None);
    };
    if is_self_authored_sender(config, &sender_email) {
        return Ok(None);
    }
    let Some(conversation_id) = reply_address else {
        return Ok(None);
    };

    let subject = parsed
        .subject()
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .unwrap_or("(no subject)")
        .to_string();
    let message_id = parsed
        .message_id()
        .map(canonical_message_id)
        .unwrap_or_else(|| synthetic_message_id(fetched.uid));
    let reply_to_message_id = parsed.in_reply_to().as_text().map(canonical_message_id);
    let content = parsed
        .body_text(0)
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty())
        .unwrap_or_else(|| format!("Subject: {subject}"));
    let received_at = parsed
        .date()
        .map(|date| date.to_rfc3339())
        .unwrap_or_else(|| Timestamp::now().to_string());

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(META_PLATFORM.to_string(), PLATFORM_EMAIL.to_string());
    event_metadata.insert(META_TRANSPORT.to_string(), TRANSPORT_IMAP.to_string());
    event_metadata.insert(META_IMAP_MAILBOX.to_string(), mailbox.mailbox.clone());
    event_metadata.insert(META_LAST_UID.to_string(), fetched.uid.to_string());
    event_metadata.insert(META_SUBJECT.to_string(), subject.clone());
    event_metadata.insert(META_REPLY_ADDRESS.to_string(), conversation_id.clone());
    event_metadata.insert(META_MESSAGE_ID.to_string(), message_id.clone());
    if let Some(uid_validity) = mailbox.uid_validity {
        event_metadata.insert(META_UID_VALIDITY.to_string(), uid_validity.to_string());
    }

    let mut message_metadata = event_metadata.clone();
    let attachments = build_inbound_attachments(&parsed);
    message_metadata.insert(
        META_ATTACHMENT_COUNT.to_string(),
        attachments.len().to_string(),
    );

    Ok(Some(InboundEventEnvelope {
        event_id: format!("imap:{}:{}", mailbox.mailbox, fetched.uid),
        platform: PLATFORM_EMAIL.to_string(),
        event_type: "message.received".to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: conversation_id,
            kind: "email".to_string(),
            thread_id: Some(subject),
            parent_message_id: reply_to_message_id.clone(),
        },
        actor: InboundActor {
            id: sender_email,
            display_name: sender_name,
            username: None,
            is_bot: false,
            metadata: BTreeMap::new(),
        },
        message: InboundMessage {
            id: message_id,
            content,
            content_type: "text/plain".to_string(),
            reply_to_message_id,
            attachments,
            metadata: message_metadata,
        },
        account_id: None,
        metadata: event_metadata,
    }))
}

fn build_inbound_attachments(parsed: &mail_parser::Message<'_>) -> Vec<InboundAttachment> {
    parsed
        .attachments()
        .enumerate()
        .map(|(index, attachment)| {
            let mut extras = BTreeMap::new();
            if let Some(content_type) = attachment.content_type() {
                extras.insert("content_type".to_string(), content_type.c_type.to_string());
                if let Some(subtype) = content_type.c_subtype.as_deref() {
                    extras.insert("content_subtype".to_string(), subtype.to_string());
                }
            }
            InboundAttachment {
                id: Some(format!("attachment-{index}")),
                kind: "email_attachment".to_string(),
                url: None,
                mime_type: attachment.content_type().map(content_type_string),
                size_bytes: Some(attachment.len() as u64),
                name: attachment.attachment_name().map(str::to_string),
                storage_key: None,
                extracted_text: attachment
                    .contents()
                    .is_ascii()
                    .then(|| String::from_utf8_lossy(attachment.contents()).to_string())
                    .filter(|text| !text.is_empty() && text.len() <= 4_096),
                extras,
            }
        })
        .collect()
}

fn is_self_authored_sender(config: &ChannelConfig, sender_email: &str) -> bool {
    let sender = sender_email.trim().to_ascii_lowercase();
    [
        config.imap_username.as_deref(),
        config.smtp_username.as_deref(),
        config.smtp_from_email.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .any(|value| value.eq_ignore_ascii_case(&sender))
}

fn polling_state(
    config: &ChannelConfig,
    status: &str,
    mailbox: &MailboxStatus,
    last_uid: Option<u32>,
) -> IngressState {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_EMAIL.to_string());
    metadata.insert(META_TRANSPORT.to_string(), TRANSPORT_IMAP.to_string());
    metadata.insert(META_IMAP_MAILBOX.to_string(), mailbox.mailbox.clone());
    metadata.insert(
        META_POLL_INTERVAL_SECS.to_string(),
        poll_interval_secs(config).to_string(),
    );
    metadata.insert(META_MAILBOX_EXISTS.to_string(), mailbox.exists.to_string());
    metadata.insert(META_MAILBOX_RECENT.to_string(), mailbox.recent.to_string());
    if let Some(uid_validity) = mailbox.uid_validity {
        metadata.insert(META_UID_VALIDITY.to_string(), uid_validity.to_string());
    }
    if let Some(last_uid) = last_uid {
        metadata.insert(META_LAST_UID.to_string(), last_uid.to_string());
    }

    IngressState {
        mode: IngressMode::Polling,
        status: status.to_string(),
        endpoint: None,
        metadata,
    }
}

fn poll_interval_secs(config: &ChannelConfig) -> u16 {
    config
        .poll_interval_secs
        .map(|secs| secs.max(MIN_POLL_INTERVAL_SECS))
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
}

fn sleep_until_next_cycle(stop: &Arc<AtomicBool>, total_secs: u16) {
    let deadline = Duration::from_secs(u64::from(total_secs));
    let mut slept = Duration::ZERO;
    while slept < deadline && !stop.load(Ordering::Relaxed) {
        let step = std::cmp::min(Duration::from_millis(250), deadline - slept);
        thread::sleep(step);
        slept += step;
    }
}

fn rejected_status(reason_code: &str, message: impl Into<String>) -> StatusAcceptance {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_REASON_CODE.to_string(), reason_code.to_string());
    metadata.insert("message".to_string(), message.into());
    StatusAcceptance {
        accepted: false,
        metadata,
    }
}

fn address_email<'a>(addr: &'a mail_parser::Addr<'a>) -> Option<&'a str> {
    addr.address().filter(|address| !address.trim().is_empty())
}

fn content_type_string(content_type: &mail_parser::ContentType<'_>) -> String {
    match content_type.c_subtype.as_deref() {
        Some(subtype) => format!("{}/{}", content_type.c_type, subtype),
        None => content_type.c_type.to_string(),
    }
}

fn synthetic_message_id(uid: u32) -> String {
    format!("<uid-{uid}@dispatch.local>")
}

fn canonical_message_id(value: &str) -> String {
    let value = value.trim();
    if value.starts_with('<') && value.ends_with('>') {
        value.to_string()
    } else {
        format!("<{value}>")
    }
}

fn apply_reply_subject(subject: String, is_reply: bool) -> String {
    if is_reply && !subject.to_ascii_lowercase().starts_with("re:") {
        format!("Re: {subject}")
    } else {
        subject
    }
}

fn parse_u32(field: &str, value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("{field} must be a valid integer"))
}

fn required_clone(value: &Option<String>) -> Result<String> {
    value
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("required string missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_inbound_event_uses_reply_to_subject_and_attachment_metadata() {
        let raw = br#"From: Sender <sender@example.com>
Reply-To: Queue <queue@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <msg-1@example.com>
In-Reply-To: <msg-0@example.com>
Subject: Dispatch test
Content-Type: multipart/mixed; boundary="b"

--b
Content-Type: text/plain; charset="utf-8"

hello from email
--b
Content-Type: text/plain; name="note.txt"
Content-Disposition: attachment; filename="note.txt"

attachment body
--b--
"#;

        let event = build_inbound_event(
            &ChannelConfig::default(),
            &MailboxStatus {
                mailbox: "INBOX".to_string(),
                uid_next: Some(11),
                uid_validity: Some(22),
                exists: 5,
                recent: 1,
            },
            &FetchedEmail {
                uid: 10,
                raw_message: raw.to_vec(),
            },
        )
        .expect("build event")
        .expect("event present");

        assert_eq!(event.conversation.id, "queue@example.com");
        assert_eq!(
            event.conversation.thread_id.as_deref(),
            Some("Dispatch test")
        );
        assert_eq!(event.message.id, "<msg-1@example.com>");
        assert_eq!(
            event.message.reply_to_message_id.as_deref(),
            Some("<msg-0@example.com>")
        );
        assert_eq!(event.actor.id, "sender@example.com");
        assert_eq!(event.message.content, "hello from email");
        assert_eq!(event.message.attachments.len(), 1);
        assert_eq!(
            event.message.attachments[0].name.as_deref(),
            Some("note.txt")
        );
    }

    #[test]
    fn build_inbound_event_skips_self_authored_sender() {
        let raw = br#"From: Dispatch Bot <bot@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <msg-1@example.com>
Subject: Loop
Content-Type: text/plain; charset="utf-8"

hello from self
"#;

        let event = build_inbound_event(
            &ChannelConfig {
                imap_username: Some("bot@example.com".to_string()),
                smtp_from_email: Some("bot@example.com".to_string()),
                ..ChannelConfig::default()
            },
            &MailboxStatus {
                mailbox: "INBOX".to_string(),
                uid_next: Some(11),
                uid_validity: Some(22),
                exists: 5,
                recent: 1,
            },
            &FetchedEmail {
                uid: 10,
                raw_message: raw.to_vec(),
            },
        )
        .expect("build event");

        assert!(event.is_none());
    }

    #[test]
    fn resolve_outgoing_email_prefers_routing_metadata() {
        let config = ChannelConfig {
            smtp_host: Some("smtp.example.com".to_string()),
            smtp_username: Some("bot@example.com".to_string()),
            smtp_password_env: Some("EMAIL_SMTP_PASSWORD".to_string()),
            smtp_from_email: Some("bot@example.com".to_string()),
            default_recipient: Some("fallback@example.com".to_string()),
            default_subject: Some("Fallback subject".to_string()),
            ..ChannelConfig::default()
        };
        let message = OutboundMessage {
            content: "reply body".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: Vec::new(),
            metadata: BTreeMap::from([
                (
                    ROUTE_CONVERSATION_ID.to_string(),
                    "user@example.com".to_string(),
                ),
                (ROUTE_THREAD_ID.to_string(), "Dispatch thread".to_string()),
                (
                    ROUTE_REPLY_TO_MESSAGE_ID.to_string(),
                    "<prior@example.com>".to_string(),
                ),
            ]),
        };

        let outgoing = resolve_outgoing_email(&config, &message).expect("resolve outgoing");

        assert_eq!(outgoing.to, "user@example.com");
        assert_eq!(outgoing.subject, "Re: Dispatch thread");
        assert_eq!(outgoing.in_reply_to.as_deref(), Some("<prior@example.com>"));
        assert_eq!(outgoing.references, vec!["<prior@example.com>".to_string()]);
    }
}
