use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use dispatch_channel_protocol as proto;
use dispatch_channel_runtime::{
    IngressWorker, StopSignal, restart_ingress_worker as restart_runtime_ingress_worker,
    stop_ingress_worker, write_stdout_line,
};
use jiff::Timestamp;
use mail_parser::{MessageParser, MimeHeaders};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, BufRead},
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;

mod imap_smtp;

use imap_smtp::{
    check_imap_health, ensure_configured, fetch_messages_since, has_imap_config, has_smtp_config,
    imap_host, imap_mailbox, imap_password_env, imap_port, latest_existing_uid,
    require_imap_config, send_email, smtp_connection_ok, smtp_from_email, smtp_host,
    smtp_password_env, smtp_port,
};

pub use imap_smtp::{FetchedEmail, MailboxStatus, OutgoingAttachment, OutgoingEmail, SentEmail};
pub use proto::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities,
    ChannelEventNotification, ConfiguredChannel, DeliveryReceipt, HealthReport, InboundActor,
    InboundAttachment, InboundConversationRef, InboundEventEnvelope, InboundMessage, IngressMode,
    IngressState, OutboundMessageEnvelope, PluginNotificationEnvelope, PluginResponse,
    StatusAcceptance, ThreadingModel, notification_to_jsonrpc, parse_jsonrpc_request, plugin_error,
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
const META_CC_COUNT: &str = "cc_count";
const META_BCC_COUNT: &str = "bcc_count";
const META_BODY_SOURCE: &str = "body_source";

const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const ROUTE_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const ROUTE_CC: &str = "cc";
const ROUTE_BCC: &str = "bcc";

const CONVERSATION_KIND_EMAIL: &str = "email";
const TRANSPORT_IMAP: &str = "imap";
const DEFAULT_POLL_INTERVAL_SECS: u16 = 60;
const MIN_POLL_INTERVAL_SECS: u16 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_password_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_mailbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_password_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_from_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_from_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_recipient: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_bcc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_secs: Option<u16>,
}

pub type OutboundMessage = OutboundMessageEnvelope;
pub type PluginRequest = proto::PluginRequest<ChannelConfig, OutboundMessage>;
pub type PluginRequestEnvelope = proto::PluginRequestEnvelope<PluginRequest>;
pub type EmailResult<T> = std::result::Result<T, EmailCoreError>;

#[derive(Debug, Error)]
pub enum EmailCoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("failed to read stdin: {0}")]
    ReadStdin(io::Error),
    #[error(transparent)]
    Runtime(#[from] dispatch_channel_runtime::RuntimeError),
    #[error("failed to parse channel request: {0}")]
    RequestParse(proto::JsonRpcMessageError),
    #[error("failed to encode channel response: {0}")]
    ResponseEncode(proto::JsonRpcMessageError),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("health check failed: {0}")]
    Health(String),
    #[error("failed to resolve outgoing email: {0}")]
    ResolveOutgoingEmail(String),
    #[error("failed to build inbound email event: {0}")]
    BuildInboundEvent(String),
}

pub trait EmailPreset: Send + Sync + 'static {
    const PLUGIN_ID: &'static str;
    const PLATFORM: &'static str;
    const DISPLAY_NAME: &'static str = Self::PLATFORM;
    const DEFAULT_IMAP_HOST: Option<&'static str>;
    const DEFAULT_IMAP_PORT: u16 = 993;
    const DEFAULT_SMTP_HOST: Option<&'static str>;
    const DEFAULT_SMTP_PORT: u16 = 587;
    const DEFAULT_IMAP_PASSWORD_ENV: &'static str;
    const DEFAULT_SMTP_PASSWORD_ENV: &'static str;
}

pub fn capabilities<P: EmailPreset>() -> ChannelCapabilities {
    ChannelCapabilities {
        plugin_id: P::PLUGIN_ID.to_string(),
        platform: P::PLATFORM.to_string(),
        ingress_modes: vec![IngressMode::Polling],
        outbound_message_types: vec!["text".to_string()],
        threading_model: ThreadingModel::CallerDefined,
        attachment_support: true,
        reply_verification_support: false,
        account_scoped_config: true,
        accepts_push: true,
        accepts_status_frames: false,
        attachment_sources: vec![AttachmentSource::DataBase64],
        max_attachment_bytes: None,
    }
}

pub fn run<P: EmailPreset>() -> EmailResult<()> {
    let stdin = io::stdin().lock();
    let stdout_lock = Arc::new(Mutex::new(()));
    let mut ingress_worker: Option<IngressWorker> = None;

    for line in stdin.lines() {
        let line = line.map_err(EmailCoreError::ReadStdin)?;
        if line.trim().is_empty() {
            continue;
        }

        let (request_id, envelope): (_, PluginRequestEnvelope) =
            parse_jsonrpc_request(&line).map_err(EmailCoreError::RequestParse)?;
        let should_exit = matches!(envelope.request, PluginRequest::Shutdown);

        let response = match handle_request::<P>(&envelope, &stdout_lock, &mut ingress_worker) {
            Ok(response) => response,
            Err(error) => plugin_error("internal_error", error.to_string()),
        };

        let json =
            response_to_jsonrpc(&request_id, &response).map_err(EmailCoreError::ResponseEncode)?;
        write_stdout_line(&stdout_lock, &json)?;
        if should_exit {
            break;
        }
    }

    let _ = stop_ingress_worker(&mut ingress_worker);
    Ok(())
}

fn handle_request<P: EmailPreset>(
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
            capabilities: capabilities::<P>(),
        }),
        PluginRequest::Configure { config } => Ok(PluginResponse::Configured {
            configuration: Box::new(configure::<P>(config)?),
        }),
        PluginRequest::Health { config } => Ok(PluginResponse::Health {
            health: health::<P>(config)?,
        }),
        PluginRequest::StartIngress { config, state } => {
            let started = start_ingress::<P>(config)?;
            let started = match (&started.mode, state.clone()) {
                (IngressMode::Polling, Some(state)) if state.mode == IngressMode::Polling => state,
                _ => started,
            };
            if matches!(started.mode, IngressMode::Polling) {
                restart_ingress_worker::<P>(
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
            state: stop_ingress::<P>(
                config,
                stop_ingress_worker(ingress_worker).or(state.clone()),
            )?,
        }),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver::<P>(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver::<P>(config, message)?,
        }),
        PluginRequest::IngressEvent { .. } => Ok(plugin_error(
            "webhook_not_supported",
            format!(
                "{} ingress is IMAP-backed and does not accept webhook payloads",
                P::DISPLAY_NAME
            ),
        )),
        PluginRequest::Status { .. } => Ok(PluginResponse::StatusAccepted {
            status: rejected_status(
                "not_supported",
                format!("{} status frames are not supported", P::DISPLAY_NAME),
            ),
        }),
        PluginRequest::Shutdown => {
            let _ = stop_ingress_worker(ingress_worker);
            Ok(PluginResponse::Ok)
        }
    }
}

fn restart_ingress_worker<P: EmailPreset>(
    worker: &mut Option<IngressWorker>,
    config: ChannelConfig,
    state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
) {
    restart_runtime_ingress_worker(
        worker,
        config,
        state,
        stdout_lock,
        P::DISPLAY_NAME,
        email_poll_ingress::<P>,
        email_after_cycle,
    );
}

fn email_poll_ingress<P: EmailPreset>(
    config: &ChannelConfig,
    state: Option<IngressState>,
) -> Result<PluginResponse> {
    poll_ingress::<P>(config, state)
}

fn email_after_cycle(config: &ChannelConfig, stop: &StopSignal) {
    stop.sleep_until_stopped(Duration::from_secs(u64::from(poll_interval_secs(config))));
}

pub fn configure<P: EmailPreset>(config: &ChannelConfig) -> EmailResult<ConfiguredChannel> {
    configure_inner::<P>(config).map_err(|error| EmailCoreError::Configuration(error.to_string()))
}

fn configure_inner<P: EmailPreset>(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    ensure_configured::<P>(config)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), P::PLATFORM.to_string());
    metadata.insert(
        META_POLL_INTERVAL_SECS.to_string(),
        poll_interval_secs(config).to_string(),
    );

    if has_imap_config::<P>(config) {
        metadata.insert(META_IMAP_HOST.to_string(), imap_host::<P>(config)?);
        metadata.insert(
            META_IMAP_PORT.to_string(),
            imap_port::<P>(config).to_string(),
        );
        metadata.insert(
            META_IMAP_USERNAME.to_string(),
            required_clone(&config.imap_username)?,
        );
        metadata.insert(
            META_IMAP_PASSWORD_ENV.to_string(),
            imap_password_env::<P>(config).to_string(),
        );
        metadata.insert(META_IMAP_MAILBOX.to_string(), imap_mailbox(config));
    }

    if has_smtp_config::<P>(config) {
        metadata.insert(META_SMTP_HOST.to_string(), smtp_host::<P>(config)?);
        metadata.insert(
            META_SMTP_PORT.to_string(),
            smtp_port::<P>(config).to_string(),
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
            smtp_password_env::<P>(config).to_string(),
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

pub fn health<P: EmailPreset>(config: &ChannelConfig) -> EmailResult<HealthReport> {
    health_inner::<P>(config).map_err(|error| EmailCoreError::Health(error.to_string()))
}

fn health_inner<P: EmailPreset>(config: &ChannelConfig) -> Result<HealthReport> {
    ensure_configured::<P>(config)?;

    let mut ok = true;
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), P::PLATFORM.to_string());

    let account_id = if has_imap_config::<P>(config) {
        config.imap_username.clone()
    } else if has_smtp_config::<P>(config) {
        Some(smtp_from_email(config)?)
    } else {
        None
    };

    if has_imap_config::<P>(config) {
        let mailbox = check_imap_health::<P>(config)?;
        metadata.insert(META_IMAP_HOST.to_string(), imap_host::<P>(config)?);
        metadata.insert(META_IMAP_MAILBOX.to_string(), mailbox.mailbox.clone());
        metadata.insert(META_MAILBOX_EXISTS.to_string(), mailbox.exists.to_string());
        metadata.insert(META_MAILBOX_RECENT.to_string(), mailbox.recent.to_string());
        if let Some(uid_validity) = mailbox.uid_validity {
            metadata.insert(META_UID_VALIDITY.to_string(), uid_validity.to_string());
        }
    }

    if has_smtp_config::<P>(config) {
        let smtp_ok = smtp_connection_ok::<P>(config)?;
        ok &= smtp_ok;
        metadata.insert(META_SMTP_HOST.to_string(), smtp_host::<P>(config)?);
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

fn start_ingress<P: EmailPreset>(config: &ChannelConfig) -> Result<IngressState> {
    require_imap_config::<P>(config)?;
    let mailbox = check_imap_health::<P>(config)?;
    Ok(polling_state::<P>(
        config,
        "running",
        &mailbox,
        latest_existing_uid(&mailbox),
    ))
}

fn stop_ingress<P: EmailPreset>(
    config: &ChannelConfig,
    state: Option<IngressState>,
) -> Result<IngressState> {
    if !has_imap_config::<P>(config) {
        return Ok(state.unwrap_or_else(|| IngressState {
            mode: IngressMode::Polling,
            status: "stopped".to_string(),
            endpoint: None,
            metadata: BTreeMap::new(),
        }));
    }

    let mailbox = check_imap_health::<P>(config)?;
    let last_uid = state
        .as_ref()
        .and_then(|state| state.metadata.get(META_LAST_UID))
        .map(|value| parse_u32("state.metadata.last_uid", value))
        .transpose()?
        .or_else(|| latest_existing_uid(&mailbox));
    Ok(polling_state::<P>(config, "stopped", &mailbox, last_uid))
}

fn poll_ingress<P: EmailPreset>(
    config: &ChannelConfig,
    state: Option<IngressState>,
) -> Result<PluginResponse> {
    require_imap_config::<P>(config)?;

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

    let poll = fetch_messages_since::<P>(config, last_uid)?;
    if prior_uid_validity.is_some() && poll.mailbox.uid_validity != prior_uid_validity {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: Some(polling_state::<P>(
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
        if let Some(event) = build_inbound_event::<P>(config, &poll.mailbox, fetched)? {
            events.push(event);
        }
    }

    Ok(PluginResponse::IngressEventsReceived {
        events,
        callback_reply: None,
        state: Some(polling_state::<P>(
            config,
            "running",
            &poll.mailbox,
            poll.latest_uid,
        )),
        poll_after_ms: Some(u64::from(poll_interval_secs(config)) * 1_000),
    })
}

fn deliver<P: EmailPreset>(
    config: &ChannelConfig,
    message: &OutboundMessage,
) -> Result<DeliveryReceipt> {
    let outgoing = resolve_outgoing_email::<P>(config, message)?;
    let sent = send_email::<P>(config, &outgoing)?;
    Ok(delivery_receipt::<P>(&outgoing, &sent))
}

pub fn resolve_outgoing_email<P: EmailPreset>(
    config: &ChannelConfig,
    message: &OutboundMessage,
) -> EmailResult<OutgoingEmail> {
    resolve_outgoing_email_inner::<P>(config, message)
        .map_err(|error| EmailCoreError::ResolveOutgoingEmail(error.to_string()))
}

fn resolve_outgoing_email_inner<P: EmailPreset>(
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

    let cc = resolve_address_list(message.metadata.get(ROUTE_CC), config.default_cc.as_deref());
    let bcc = resolve_address_list(
        message.metadata.get(ROUTE_BCC),
        config.default_bcc.as_deref(),
    );

    let mut attachments = Vec::with_capacity(message.attachments.len());
    for attachment in &message.attachments {
        let data_base64 = attachment.data_base64.clone().ok_or_else(|| {
            anyhow!(
                "{} outbound attachments require data_base64 for attachment `{}`",
                P::DISPLAY_NAME,
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
        cc,
        bcc,
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

/// Parse a comma-separated address list. Empty entries are dropped.
/// Per-message metadata wins over config default when both are present.
fn resolve_address_list(metadata: Option<&String>, config_default: Option<&str>) -> Vec<String> {
    let source = metadata
        .map(String::as_str)
        .or(config_default.filter(|value| !value.trim().is_empty()));
    source
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn delivery_receipt<P: EmailPreset>(outgoing: &OutgoingEmail, sent: &SentEmail) -> DeliveryReceipt {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), P::PLATFORM.to_string());
    metadata.insert(META_SUBJECT.to_string(), outgoing.subject.clone());
    metadata.insert(
        META_ATTACHMENT_COUNT.to_string(),
        outgoing.attachments.len().to_string(),
    );
    metadata.insert(META_CC_COUNT.to_string(), outgoing.cc.len().to_string());
    metadata.insert(META_BCC_COUNT.to_string(), outgoing.bcc.len().to_string());
    if let Some(in_reply_to) = &outgoing.in_reply_to {
        metadata.insert(ROUTE_REPLY_TO_MESSAGE_ID.to_string(), in_reply_to.clone());
    }

    DeliveryReceipt {
        message_id: sent.message_id.clone(),
        conversation_id: outgoing.to.clone(),
        metadata,
    }
}

pub fn build_inbound_event<P: EmailPreset>(
    config: &ChannelConfig,
    mailbox: &MailboxStatus,
    fetched: &FetchedEmail,
) -> EmailResult<Option<InboundEventEnvelope>> {
    build_inbound_event_inner::<P>(config, mailbox, fetched)
        .map_err(|error| EmailCoreError::BuildInboundEvent(error.to_string()))
}

fn build_inbound_event_inner<P: EmailPreset>(
    config: &ChannelConfig,
    mailbox: &MailboxStatus,
    fetched: &FetchedEmail,
) -> Result<Option<InboundEventEnvelope>> {
    let parsed = MessageParser::default()
        .parse(&fetched.raw_message)
        .ok_or_else(|| anyhow!("failed to parse fetched email UID {}", fetched.uid))?;
    if is_auto_generated(&parsed) {
        return Ok(None);
    }
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
    let (content, body_source) = extract_body_content(&parsed, &subject);
    let received_at = parsed
        .date()
        .map(|date| date.to_rfc3339())
        .unwrap_or_else(|| Timestamp::now().to_string());

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(META_PLATFORM.to_string(), P::PLATFORM.to_string());
    event_metadata.insert(META_TRANSPORT.to_string(), TRANSPORT_IMAP.to_string());
    event_metadata.insert(META_IMAP_MAILBOX.to_string(), mailbox.mailbox.clone());
    event_metadata.insert(META_LAST_UID.to_string(), fetched.uid.to_string());
    event_metadata.insert(META_SUBJECT.to_string(), subject.clone());
    event_metadata.insert(META_REPLY_ADDRESS.to_string(), conversation_id.clone());
    event_metadata.insert(META_MESSAGE_ID.to_string(), message_id.clone());
    event_metadata.insert(META_BODY_SOURCE.to_string(), body_source.to_string());
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
        platform: P::PLATFORM.to_string(),
        event_type: "message.received".to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: conversation_id,
            kind: CONVERSATION_KIND_EMAIL.to_string(),
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

/// Returns true when the message was produced by an automated system we should not
/// reply to (bounces, auto-replies, mailing-list digests, etc.). Matches the
/// heuristics used by hermes-agent's email platform:
///   - `Auto-Submitted` is present and not literally `no`
///   - `Precedence` is one of `bulk` / `list` / `junk` / `auto_reply`
///   - `List-Unsubscribe` is present (mailing lists set this)
fn is_auto_generated(parsed: &mail_parser::Message<'_>) -> bool {
    if let Some(value) = parsed.header_raw("Auto-Submitted") {
        let lowered = value.trim().to_ascii_lowercase();
        let lowered = lowered
            .split_once(|c: char| c == ';' || c.is_whitespace())
            .map(|(head, _)| head)
            .unwrap_or(lowered.as_str());
        if !lowered.is_empty() && lowered != "no" {
            return true;
        }
    }
    if let Some(value) = parsed.header_raw("Precedence") {
        let lowered = value.trim().to_ascii_lowercase();
        if matches!(lowered.as_str(), "bulk" | "list" | "junk" | "auto_reply") {
            return true;
        }
    }
    if parsed
        .header_raw("List-Unsubscribe")
        .is_some_and(|value| !value.trim().is_empty())
    {
        return true;
    }
    false
}

/// Returns the best-effort plaintext body of the message together with a short
/// tag describing the source (`text`, `html`, or `subject`).
///
/// mail-parser's `body_text()` transparently falls back to an auto-converted
/// HTML body when no text/plain part exists; we check the raw part type first
/// so that when only an HTML part is present we run our own HTML -> text
/// conversion (which handles `<script>`/`<style>` stripping and block-level
/// line breaks) rather than mail-parser's simpler auto-conversion.
fn extract_body_content(
    parsed: &mail_parser::Message<'_>,
    subject: &str,
) -> (String, &'static str) {
    let has_text_part = parsed
        .text_part(0)
        .is_some_and(|part| matches!(part.body, mail_parser::PartType::Text(_)));
    if has_text_part && let Some(body) = parsed.body_text(0) {
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            return (trimmed.to_string(), "text");
        }
    }
    if let Some(html) = parsed.body_html(0) {
        let text = html_to_plain_text(html.as_ref());
        if !text.is_empty() {
            return (text, "html");
        }
    }
    (format!("Subject: {subject}"), "subject")
}

/// Minimal HTML -> plaintext conversion for the HTML-only fallback path.
/// This is intentionally simple: drop `<script>` / `<style>` blocks, turn
/// block-level tags into newlines, strip every other tag, decode the common
/// HTML entities, and collapse consecutive blank lines. This is not a full
/// HTML renderer; it is a rescue path for messages with no text/plain part.
fn html_to_plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &html[i..];
        if rest.starts_with('<') {
            let Some(end_rel) = rest.find('>') else {
                break;
            };
            let tag_body = &rest[1..end_rel];
            let tag_name_end = tag_body
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .unwrap_or(tag_body.len());
            let tag_name = tag_body[..tag_name_end].to_ascii_lowercase();
            let tag_name_trimmed = tag_name.trim_start_matches('/');
            // Skip the entire body of <script>/<style> blocks.
            if matches!(tag_name_trimmed, "script" | "style") && !tag_name.starts_with('/') {
                let close_tag = format!("</{tag_name_trimmed}");
                let close = rest[end_rel + 1..]
                    .to_ascii_lowercase()
                    .find(&close_tag)
                    .map(|rel| end_rel + 1 + rel);
                if let Some(close_pos) = close {
                    let after_close = rest[close_pos..].find('>').map(|r| close_pos + r + 1);
                    if let Some(after) = after_close {
                        i += after;
                        continue;
                    }
                }
                // unclosed: drop the rest of the document.
                break;
            }
            // Map block-level tags to line breaks so paragraphs stay legible.
            if matches!(
                tag_name_trimmed,
                "br" | "p"
                    | "div"
                    | "li"
                    | "tr"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "blockquote"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "ul"
                    | "ol"
                    | "table"
            ) && !out.ends_with('\n')
            {
                out.push('\n');
            }
            i += end_rel + 1;
            continue;
        }
        if rest.starts_with('&')
            && let Some(semi) = rest[..rest.len().min(12)].find(';')
        {
            let entity = &rest[1..semi];
            if let Some(decoded) = decode_html_entity(entity) {
                out.push_str(decoded);
                i += semi + 1;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    // Collapse runs of blank lines and trim.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_blank = false;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_blank && !collapsed.is_empty() {
                collapsed.push('\n');
                prev_blank = true;
            }
        } else {
            if !collapsed.is_empty() {
                collapsed.push('\n');
            }
            collapsed.push_str(trimmed);
            prev_blank = false;
        }
    }
    collapsed.trim().to_string()
}

fn decode_html_entity(entity: &str) -> Option<&'static str> {
    match entity {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" | "#39" => Some("'"),
        "nbsp" | "#160" => Some(" "),
        _ => None,
    }
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

fn polling_state<P: EmailPreset>(
    config: &ChannelConfig,
    status: &str,
    mailbox: &MailboxStatus,
    last_uid: Option<u32>,
) -> IngressState {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), P::PLATFORM.to_string());
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

    struct GenericEmailPreset;

    impl EmailPreset for GenericEmailPreset {
        const PLUGIN_ID: &'static str = "email";
        const PLATFORM: &'static str = "email";
        const DEFAULT_IMAP_HOST: Option<&'static str> = None;
        const DEFAULT_SMTP_HOST: Option<&'static str> = None;
        const DEFAULT_IMAP_PASSWORD_ENV: &'static str = "EMAIL_IMAP_PASSWORD";
        const DEFAULT_SMTP_PASSWORD_ENV: &'static str = "EMAIL_SMTP_PASSWORD";
    }

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

        let event = build_inbound_event::<GenericEmailPreset>(
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

        let event = build_inbound_event::<GenericEmailPreset>(
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

        let outgoing =
            resolve_outgoing_email::<GenericEmailPreset>(&config, &message).expect("resolve");

        assert_eq!(outgoing.to, "user@example.com");
        assert_eq!(outgoing.subject, "Re: Dispatch thread");
        assert_eq!(outgoing.in_reply_to.as_deref(), Some("<prior@example.com>"));
        assert_eq!(outgoing.references, vec!["<prior@example.com>".to_string()]);
        assert!(outgoing.cc.is_empty());
        assert!(outgoing.bcc.is_empty());
    }

    #[test]
    fn resolve_outgoing_email_takes_cc_and_bcc_from_metadata() {
        let config = ChannelConfig {
            smtp_host: Some("smtp.example.com".to_string()),
            smtp_from_email: Some("bot@example.com".to_string()),
            smtp_username: Some("bot@example.com".to_string()),
            smtp_password_env: Some("EMAIL_SMTP_PASSWORD".to_string()),
            default_subject: Some("Alert".to_string()),
            default_cc: Some("oncall-default@example.com".to_string()),
            default_bcc: Some("archive-default@example.com".to_string()),
            ..ChannelConfig::default()
        };
        let message = OutboundMessage {
            content: "heads up".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: Vec::new(),
            metadata: BTreeMap::from([
                (
                    ROUTE_CONVERSATION_ID.to_string(),
                    "user@example.com".to_string(),
                ),
                (
                    ROUTE_CC.to_string(),
                    "alice@example.com, Bob <bob@example.com>".to_string(),
                ),
                (ROUTE_BCC.to_string(), "audit@example.com".to_string()),
            ]),
        };

        let outgoing =
            resolve_outgoing_email::<GenericEmailPreset>(&config, &message).expect("resolve");

        // Per-message metadata wins over config defaults.
        assert_eq!(
            outgoing.cc,
            vec![
                "alice@example.com".to_string(),
                "Bob <bob@example.com>".to_string()
            ]
        );
        assert_eq!(outgoing.bcc, vec!["audit@example.com".to_string()]);
    }

    #[test]
    fn resolve_outgoing_email_falls_back_to_default_cc_and_bcc() {
        let config = ChannelConfig {
            smtp_host: Some("smtp.example.com".to_string()),
            smtp_from_email: Some("bot@example.com".to_string()),
            smtp_username: Some("bot@example.com".to_string()),
            smtp_password_env: Some("EMAIL_SMTP_PASSWORD".to_string()),
            default_subject: Some("Alert".to_string()),
            default_cc: Some(" leadership@example.com , oncall@example.com ,  ".to_string()),
            default_bcc: Some("archive@example.com".to_string()),
            ..ChannelConfig::default()
        };
        let message = OutboundMessage {
            content: "heads up".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: Vec::new(),
            metadata: BTreeMap::from([(
                ROUTE_CONVERSATION_ID.to_string(),
                "user@example.com".to_string(),
            )]),
        };

        let outgoing =
            resolve_outgoing_email::<GenericEmailPreset>(&config, &message).expect("resolve");

        assert_eq!(
            outgoing.cc,
            vec![
                "leadership@example.com".to_string(),
                "oncall@example.com".to_string()
            ]
        );
        assert_eq!(outgoing.bcc, vec!["archive@example.com".to_string()]);
    }

    #[test]
    fn resolve_outgoing_email_empty_cc_and_bcc_override_defaults() {
        let config = ChannelConfig {
            smtp_host: Some("smtp.example.com".to_string()),
            smtp_from_email: Some("bot@example.com".to_string()),
            smtp_username: Some("bot@example.com".to_string()),
            smtp_password_env: Some("EMAIL_SMTP_PASSWORD".to_string()),
            default_subject: Some("Alert".to_string()),
            default_cc: Some("leadership@example.com".to_string()),
            default_bcc: Some("archive@example.com".to_string()),
            ..ChannelConfig::default()
        };
        let message = OutboundMessage {
            content: "heads up".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: Vec::new(),
            metadata: BTreeMap::from([
                (
                    ROUTE_CONVERSATION_ID.to_string(),
                    "user@example.com".to_string(),
                ),
                (ROUTE_CC.to_string(), " , ".to_string()),
                (ROUTE_BCC.to_string(), "".to_string()),
            ]),
        };

        let outgoing =
            resolve_outgoing_email::<GenericEmailPreset>(&config, &message).expect("resolve");

        assert!(outgoing.cc.is_empty());
        assert!(outgoing.bcc.is_empty());
    }

    #[test]
    fn build_inbound_event_drops_auto_submitted_messages() {
        let raw = br#"From: Bounce Daemon <mailer-daemon@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <bounce-1@example.com>
Subject: Delivery Status Notification
Auto-Submitted: auto-replied
Content-Type: text/plain; charset="utf-8"

Your message could not be delivered.
"#;

        let event = build_inbound_event::<GenericEmailPreset>(
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
        .expect("build event");

        assert!(event.is_none());
    }

    #[test]
    fn build_inbound_event_drops_bulk_precedence() {
        let raw = br#"From: Marketing <news@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <news-1@example.com>
Subject: Weekly digest
Precedence: bulk
Content-Type: text/plain; charset="utf-8"

Marketing content here.
"#;

        let event = build_inbound_event::<GenericEmailPreset>(
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
        .expect("build event");

        assert!(event.is_none());
    }

    #[test]
    fn build_inbound_event_drops_list_messages() {
        let raw = br#"From: List <list@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <list-1@example.com>
Subject: [dev] patch
List-Unsubscribe: <mailto:list-unsubscribe@example.com>
Content-Type: text/plain; charset="utf-8"

Message body.
"#;

        let event = build_inbound_event::<GenericEmailPreset>(
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
        .expect("build event");

        assert!(event.is_none());
    }

    #[test]
    fn build_inbound_event_keeps_empty_list_unsubscribe() {
        let raw = br#"From: Alice <alice@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <ok-2@example.com>
Subject: Hello
List-Unsubscribe:
Content-Type: text/plain; charset="utf-8"

normal message
"#;

        let event = build_inbound_event::<GenericEmailPreset>(
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

        assert_eq!(event.message.content, "normal message");
    }

    #[test]
    fn build_inbound_event_keeps_auto_submitted_no() {
        let raw = br#"From: Alice <alice@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <ok-1@example.com>
Subject: Greetings
Auto-Submitted: no
Content-Type: text/plain; charset="utf-8"

hi there
"#;

        let event = build_inbound_event::<GenericEmailPreset>(
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

        assert_eq!(event.message.content, "hi there");
        assert_eq!(
            event.metadata.get(META_BODY_SOURCE).map(String::as_str),
            Some("text")
        );
    }

    #[test]
    fn build_inbound_event_falls_back_to_html_body_when_text_absent() {
        let raw = br#"From: Alice <alice@example.com>
Date: Sat, 20 Nov 2021 14:22:01 -0800
Message-ID: <html-1@example.com>
Subject: HTML only
Content-Type: text/html; charset="utf-8"

<html><body><p>Hello <b>world</b>!</p><p>Line two &amp; done.</p><script>alert('x')</script></body></html>
"#;

        let event = build_inbound_event::<GenericEmailPreset>(
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

        assert_eq!(event.message.content, "Hello world!\nLine two & done.");
        assert_eq!(
            event.metadata.get(META_BODY_SOURCE).map(String::as_str),
            Some("html")
        );
    }

    #[test]
    fn html_to_plain_text_strips_tags_and_decodes_entities() {
        let html =
            "<div>Hello <b>&amp; goodbye</b>&nbsp;now.</div><style>x{}</style><p>Next line.</p>";
        assert_eq!(html_to_plain_text(html), "Hello & goodbye now.\nNext line.");
    }
}
