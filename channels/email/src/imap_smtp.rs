use crate::protocol::ChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use imap::ClientBuilder;
use lettre::{
    Message, SmtpTransport, Transport,
    message::{Attachment, Mailbox, MultiPart, SinglePart, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_IMAP_PORT: u16 = 993;
const DEFAULT_SMTP_PORT: u16 = 587;
const DEFAULT_IMAP_PASSWORD_ENV: &str = "EMAIL_IMAP_PASSWORD";
const DEFAULT_SMTP_PASSWORD_ENV: &str = "EMAIL_SMTP_PASSWORD";
const DEFAULT_IMAP_MAILBOX: &str = "INBOX";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxStatus {
    pub mailbox: String,
    pub uid_next: Option<u32>,
    pub uid_validity: Option<u32>,
    pub exists: u32,
    pub recent: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedEmail {
    pub uid: u32,
    pub raw_message: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapPollResult {
    pub mailbox: MailboxStatus,
    pub latest_uid: Option<u32>,
    pub messages: Vec<FetchedEmail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingEmail {
    pub from_email: String,
    pub from_name: Option<String>,
    pub to: String,
    pub subject: String,
    pub body_text: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<OutgoingAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentEmail {
    pub message_id: String,
}

pub fn check_imap_health(config: &ChannelConfig) -> Result<MailboxStatus> {
    require_imap_config(config)?;
    let host = imap_host(config)?;
    let port = imap_port(config);
    let username = imap_username(config)?;
    let password = read_required_env(imap_password_env(config))?;
    let mailbox_name = imap_mailbox(config);

    let client = ClientBuilder::new(&host, port)
        .connect()
        .with_context(|| format!("failed to connect to IMAP server {host}:{port}"))?;
    let mut session = client
        .login(&username, &password)
        .map_err(|error| error.0)
        .with_context(|| format!("failed to login to IMAP mailbox `{mailbox_name}`"))?;

    let selected = session
        .select(&mailbox_name)
        .with_context(|| format!("failed to select IMAP mailbox `{mailbox_name}`"))?;
    let status = mailbox_status(&mailbox_name, &selected);
    let _ = session.logout();
    Ok(status)
}

pub fn fetch_messages_since(
    config: &ChannelConfig,
    last_uid: Option<u32>,
) -> Result<ImapPollResult> {
    require_imap_config(config)?;
    let host = imap_host(config)?;
    let port = imap_port(config);
    let username = imap_username(config)?;
    let password = read_required_env(imap_password_env(config))?;
    let mailbox_name = imap_mailbox(config);

    let client = ClientBuilder::new(&host, port)
        .connect()
        .with_context(|| format!("failed to connect to IMAP server {host}:{port}"))?;
    let mut session = client
        .login(&username, &password)
        .map_err(|error| error.0)
        .with_context(|| format!("failed to login to IMAP mailbox `{mailbox_name}`"))?;

    let selected = session
        .select(&mailbox_name)
        .with_context(|| format!("failed to select IMAP mailbox `{mailbox_name}`"))?;
    let mailbox = mailbox_status(&mailbox_name, &selected);

    let mut uids: Vec<u32> = session
        .uid_search("ALL")
        .context("failed to search IMAP mailbox UIDs")?
        .into_iter()
        .collect();
    uids.sort_unstable();

    if let Some(last_uid) = last_uid {
        uids.retain(|uid| *uid > last_uid);
    }

    let mut messages = Vec::new();
    for chunk in uids.chunks(50) {
        let fetches = session
            .uid_fetch(join_uid_set(chunk), "(UID RFC822)")
            .context("failed to fetch IMAP messages")?;
        for fetch in fetches.iter() {
            let Some(uid) = fetch.uid else {
                continue;
            };
            let Some(body) = fetch.body() else {
                continue;
            };
            messages.push(FetchedEmail {
                uid,
                raw_message: body.to_vec(),
            });
        }
    }

    messages.sort_by_key(|message| message.uid);
    let latest_uid = messages
        .last()
        .map(|message| message.uid)
        .or_else(|| latest_existing_uid(&mailbox));
    let _ = session.logout();

    Ok(ImapPollResult {
        mailbox,
        latest_uid,
        messages,
    })
}

pub fn send_email(config: &ChannelConfig, email: &OutgoingEmail) -> Result<SentEmail> {
    require_smtp_config(config)?;
    let transport = smtp_transport(config)?;
    let message_id = generated_message_id(&email.from_email);

    let from = Mailbox::new(
        email
            .from_name
            .clone()
            .filter(|name| !name.trim().is_empty()),
        email
            .from_email
            .parse()
            .with_context(|| format!("invalid from address `{}`", email.from_email))?,
    );
    let to = email
        .to
        .parse()
        .with_context(|| format!("invalid recipient address `{}`", email.to))?;

    let mut builder = Message::builder()
        .from(from)
        .to(to)
        .subject(&email.subject)
        .message_id(Some(message_id.clone()));

    if let Some(in_reply_to) = &email.in_reply_to {
        builder = builder.in_reply_to(in_reply_to.clone());
    }
    for reference in &email.references {
        builder = builder.references(reference.clone());
    }

    let message = if email.attachments.is_empty() {
        builder
            .header(ContentType::TEXT_PLAIN)
            .body(email.body_text.clone())
            .context("failed to build outbound email")?
    } else {
        let mut multipart =
            MultiPart::mixed().singlepart(SinglePart::plain(email.body_text.clone()));
        for attachment in &email.attachments {
            let content_type = ContentType::parse(&attachment.mime_type).with_context(|| {
                format!("invalid attachment MIME type `{}`", attachment.mime_type)
            })?;
            multipart = multipart.singlepart(
                Attachment::new(attachment.name.clone())
                    .body(attachment.data.clone(), content_type),
            );
        }
        builder
            .multipart(multipart)
            .context("failed to build outbound email with attachments")?
    };

    transport
        .send(&message)
        .context("failed to deliver outbound email")?;

    Ok(SentEmail { message_id })
}

pub fn smtp_connection_ok(config: &ChannelConfig) -> Result<bool> {
    require_smtp_config(config)?;
    smtp_transport(config)?
        .test_connection()
        .context("failed to test SMTP connection")
}

pub fn latest_existing_uid(status: &MailboxStatus) -> Option<u32> {
    status
        .uid_next
        .and_then(|next_uid| next_uid.checked_sub(1))
        .filter(|uid| *uid > 0)
}

pub fn require_imap_config(config: &ChannelConfig) -> Result<()> {
    let _ = imap_host(config)?;
    let _ = imap_username(config)?;
    let _ = read_required_env(imap_password_env(config))?;
    Ok(())
}

pub fn require_smtp_config(config: &ChannelConfig) -> Result<()> {
    let _ = smtp_host(config)?;
    let _ = smtp_username(config)?;
    let _ = read_required_env(smtp_password_env(config))?;
    let _ = smtp_from_email(config)?;
    Ok(())
}

pub fn imap_mailbox(config: &ChannelConfig) -> String {
    config
        .imap_mailbox
        .clone()
        .filter(|mailbox| !mailbox.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAP_MAILBOX.to_string())
}

pub fn imap_password_env(config: &ChannelConfig) -> &str {
    config
        .imap_password_env
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(DEFAULT_IMAP_PASSWORD_ENV)
}

pub fn smtp_password_env(config: &ChannelConfig) -> &str {
    config
        .smtp_password_env
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            config
                .imap_password_env
                .as_deref()
                .filter(|name| !name.trim().is_empty())
        })
        .unwrap_or(DEFAULT_SMTP_PASSWORD_ENV)
}

fn mailbox_status(mailbox_name: &str, mailbox: &imap::types::Mailbox) -> MailboxStatus {
    MailboxStatus {
        mailbox: mailbox_name.to_string(),
        uid_next: mailbox.uid_next,
        uid_validity: mailbox.uid_validity,
        exists: mailbox.exists,
        recent: mailbox.recent,
    }
}

fn smtp_transport(config: &ChannelConfig) -> Result<SmtpTransport> {
    let host = smtp_host(config)?;
    let port = smtp_port(config);
    let username = smtp_username(config)?;
    let password = read_required_env(smtp_password_env(config))?;
    let credentials = Credentials::new(username, password);

    let builder = if port == 465 {
        SmtpTransport::relay(&host)?
    } else {
        SmtpTransport::starttls_relay(&host)?
    };

    Ok(builder.credentials(credentials).port(port).build())
}

fn imap_host(config: &ChannelConfig) -> Result<String> {
    required_string(
        config.imap_host.clone(),
        "config.imap_host is required for IMAP ingress",
    )
}

fn imap_port(config: &ChannelConfig) -> u16 {
    config.imap_port.unwrap_or(DEFAULT_IMAP_PORT)
}

fn imap_username(config: &ChannelConfig) -> Result<String> {
    required_string(
        config.imap_username.clone(),
        "config.imap_username is required for IMAP ingress",
    )
}

fn smtp_host(config: &ChannelConfig) -> Result<String> {
    required_string(
        config.smtp_host.clone(),
        "config.smtp_host is required for SMTP delivery",
    )
}

fn smtp_port(config: &ChannelConfig) -> u16 {
    config.smtp_port.unwrap_or(DEFAULT_SMTP_PORT)
}

fn smtp_username(config: &ChannelConfig) -> Result<String> {
    if let Some(username) = config
        .smtp_username
        .clone()
        .filter(|username| !username.trim().is_empty())
    {
        return Ok(username);
    }
    imap_username(config)
}

pub fn smtp_from_email(config: &ChannelConfig) -> Result<String> {
    if let Some(address) = config
        .smtp_from_email
        .clone()
        .filter(|address| !address.trim().is_empty())
    {
        return Ok(address);
    }
    smtp_username(config)
}

fn generated_message_id(from_email: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let domain = from_email
        .split_once('@')
        .map(|(_, domain)| domain)
        .filter(|domain| !domain.trim().is_empty())
        .unwrap_or("dispatch.local");
    format!("<dispatch-{nanos}@{domain}>")
}

fn join_uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn required_string(value: Option<String>, message: &str) -> Result<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!(message.to_string()))
}

fn read_required_env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| anyhow!("environment variable {name} is required"))
}

pub fn has_imap_config(config: &ChannelConfig) -> bool {
    config
        .imap_host
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        && config
            .imap_username
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
}

pub fn has_smtp_config(config: &ChannelConfig) -> bool {
    config
        .smtp_host
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

pub fn ensure_configured(config: &ChannelConfig) -> Result<()> {
    if !has_imap_config(config) && !has_smtp_config(config) {
        bail!("email plugin requires IMAP ingress, SMTP delivery, or both");
    }
    if has_imap_config(config) {
        require_imap_config(config)?;
    }
    if has_smtp_config(config) {
        require_smtp_config(config)?;
    }
    Ok(())
}
