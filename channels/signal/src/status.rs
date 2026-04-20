//! Status-frame handling for the native presage-backed Signal plugin.
//!
//! Dispatch emits `status` frames to tell a channel "the agent is
//! processing", "the agent is delivering", "the operation finished",
//! etc. For Signal we map the turn-taking subset of those frames to
//! Signal typing indicators. Other frames are accepted but ignored.

use anyhow::{Context, Result, anyhow};
use presage::Manager;
use presage::libsignal_service::content::ContentBody;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::proto::TypingMessage;
use presage::libsignal_service::proto::typing_message::Action;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage_store_sqlite::SqliteStore;
use tokio::runtime::Builder;

use crate::protocol::{ChannelConfig, StatusAcceptance, StatusFrame, StatusKind};
use crate::store::{resolve_passphrase, resolve_store_path, to_sqlite_url};

const SIGNAL_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Accept a `status` frame, translate it into a Signal typing
/// indicator if applicable, and return a StatusAcceptance reporting
/// what happened. Unknown / unsupported kinds are accepted without
/// any upstream traffic.
pub fn handle_status(config: &ChannelConfig, frame: &StatusFrame) -> Result<StatusAcceptance> {
    let action = match frame.kind {
        StatusKind::Processing | StatusKind::Delivering | StatusKind::OperationStarted => {
            Some(Action::Started)
        }
        StatusKind::Completed | StatusKind::Cancelled | StatusKind::OperationFinished => {
            Some(Action::Stopped)
        }
        StatusKind::Info
        | StatusKind::ApprovalNeeded
        | StatusKind::AuthRequired
        | StatusKind::Unknown => None,
        _ => None,
    };

    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("platform".to_string(), "signal".to_string());

    let Some(action) = action else {
        metadata.insert("typing_action".to_string(), "ignored".to_string());
        return Ok(StatusAcceptance {
            accepted: true,
            metadata,
        });
    };

    let recipient_raw = frame
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "channel-signal status frame requires `conversation_id` to address the typing indicator"
            )
        })?
        .to_string();
    let recipient = parse_service_id(&recipient_raw)?;

    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Err(anyhow!(
            "Signal session has not been linked yet; run `channel-signal --link` first (store path: {})",
            store_path.display()
        ));
    }
    let url = to_sqlite_url(&store_path);
    let passphrase = resolve_passphrase(config)?;

    let handle = std::thread::Builder::new()
        .name("channel-signal-status".to_string())
        .stack_size(SIGNAL_THREAD_STACK_SIZE)
        .spawn(move || -> Result<()> {
            let runtime = Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build status-thread tokio runtime")?;
            runtime.block_on(send_typing(url, passphrase, recipient, action))
        })
        .context("failed to spawn channel-signal status thread")?;

    handle
        .join()
        .map_err(|_| anyhow!("channel-signal status thread panicked"))??;

    metadata.insert(
        "typing_action".to_string(),
        match action {
            Action::Started => "started".to_string(),
            Action::Stopped => "stopped".to_string(),
        },
    );
    metadata.insert("recipient".to_string(), recipient_raw);
    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

async fn send_typing(
    url: String,
    passphrase: Option<String>,
    recipient: ServiceId,
    action: Action,
) -> Result<()> {
    let store =
        SqliteStore::open_with_passphrase(&url, passphrase.as_deref(), OnNewIdentity::Trust)
            .await
            .with_context(|| format!("failed to open Signal session store at `{url}`"))?;
    let mut manager = Manager::<SqliteStore, Registered>::load_registered(store)
        .await
        .map_err(|error| anyhow!("failed to load Signal session: {error}"))?;

    let timestamp_ms = jiff::Timestamp::now().as_millisecond() as u64;
    let typing = TypingMessage {
        timestamp: Some(timestamp_ms),
        action: Some(action as i32),
        group_id: None,
    };

    manager
        .send_message(recipient, ContentBody::TypingMessage(typing), timestamp_ms)
        .await
        .map_err(|error| anyhow!("failed to send Signal typing indicator: {error}"))?;
    Ok(())
}

fn parse_service_id(raw: &str) -> Result<ServiceId> {
    if let Some(service_id) = ServiceId::parse_from_service_id_string(raw) {
        return Ok(service_id);
    }
    if let Some(rest) = raw.strip_prefix("ACI:")
        && let Ok(uuid) = Uuid::parse_str(rest)
    {
        return Ok(Aci::from(uuid).into());
    }
    let uuid = Uuid::parse_str(raw).with_context(|| {
        format!(
            "channel-signal status recipient `{raw}` is neither a bare UUID nor an `ACI:<uuid>` / `PNI:<uuid>` ServiceId"
        )
    })?;
    Ok(Aci::from(uuid).into())
}
