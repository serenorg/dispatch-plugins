use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::time::Duration;
use whatsapp_rust::ChatStateType;
use whatsapp_rust::Jid;
use whatsapp_rust::TokioRuntime;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::protocol::{ChannelConfig, StatusAcceptance, StatusFrame, StatusKind};
use crate::session::{SessionState, load_session};
use crate::store::{resolve_store_path, to_sqlite_url};

const PLATFORM_WHATSAPP: &str = "whatsapp";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub fn handle_status(config: &ChannelConfig, frame: &StatusFrame) -> Result<StatusAcceptance> {
    let state = chat_state_for_kind(frame.kind.clone());

    let mut metadata = BTreeMap::new();
    metadata.insert("platform".to_string(), PLATFORM_WHATSAPP.to_string());

    let Some(state) = state else {
        metadata.insert("chat_state".to_string(), "ignored".to_string());
        return Ok(StatusAcceptance {
            accepted: true,
            metadata,
        });
    };

    let conversation_id = frame
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "channel-whatsapp status frame requires `conversation_id` to address the typing indicator"
            )
        })?
        .to_string();
    let recipient = parse_recipient_jid(&conversation_id)?;

    match load_session(config)? {
        SessionState::Registered { .. } => {}
        SessionState::NotYetLinked { store_path } | SessionState::StoreEmpty { store_path } => {
            return Err(anyhow!(
                "WhatsApp session has not been linked yet; run `channel-whatsapp --link` first (store path: {})",
                store_path.display()
            ));
        }
    }

    let store_path = resolve_store_path(config)?;
    let sqlite_url = to_sqlite_url(&store_path);
    let handle = std::thread::Builder::new()
        .name("channel-whatsapp-status".to_string())
        .spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build status-thread tokio runtime")?;
            runtime.block_on(send_chat_state(sqlite_url, recipient, state))
        })
        .context("failed to spawn channel-whatsapp status thread")?;

    handle
        .join()
        .map_err(|_| anyhow!("channel-whatsapp status thread panicked"))??;

    metadata.insert(
        "chat_state".to_string(),
        chat_state_label(state).to_string(),
    );
    metadata.insert("conversation_id".to_string(), conversation_id);
    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

fn chat_state_for_kind(kind: StatusKind) -> Option<ChatStateType> {
    match kind {
        StatusKind::Processing | StatusKind::Delivering | StatusKind::OperationStarted => {
            Some(ChatStateType::Composing)
        }
        StatusKind::Completed | StatusKind::Cancelled | StatusKind::OperationFinished => {
            Some(ChatStateType::Paused)
        }
        StatusKind::Info
        | StatusKind::ApprovalNeeded
        | StatusKind::AuthRequired
        | StatusKind::Unknown => None,
        _ => None,
    }
}

fn chat_state_label(state: ChatStateType) -> &'static str {
    match state {
        ChatStateType::Composing => "composing",
        ChatStateType::Paused => "paused",
        ChatStateType::Recording => "recording",
    }
}

fn parse_recipient_jid(raw: &str) -> Result<Jid> {
    if !raw.contains('@') {
        return Err(anyhow!(
            "channel-whatsapp status recipient `{raw}` is not a WhatsApp JID; use a full JID like `15551234567@s.whatsapp.net`"
        ));
    }

    if !raw.ends_with("@s.whatsapp.net") && !raw.ends_with("@lid") {
        return Err(anyhow!(
            "channel-whatsapp v0.1.0 status frames only support direct-message JIDs ending in `@s.whatsapp.net` or `@lid`; got `{raw}`"
        ));
    }

    raw.parse::<Jid>()
        .with_context(|| format!("failed to parse WhatsApp recipient JID `{raw}`"))
}

async fn send_chat_state(sqlite_url: String, recipient: Jid, state: ChatStateType) -> Result<()> {
    let backend = std::sync::Arc::new(
        SqliteStore::new(&sqlite_url)
            .await
            .with_context(|| format!("failed to open WhatsApp session store at `{sqlite_url}`"))?,
    );

    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .build()
        .await
        .context("failed to build WhatsApp bot for status handling")?;

    let client = bot.client();
    let bot_handle = bot.run().await.context("failed to start WhatsApp bot")?;
    client
        .wait_for_socket(CONNECT_TIMEOUT)
        .await
        .context("timed out waiting for linked WhatsApp session socket")?;
    let recipient = resolve_chat_recipient(&client, &recipient).await?;

    match state {
        ChatStateType::Composing => client
            .chatstate()
            .send_composing(&recipient)
            .await
            .context("failed to send WhatsApp composing indicator")?,
        ChatStateType::Paused => client
            .chatstate()
            .send_paused(&recipient)
            .await
            .context("failed to send WhatsApp paused indicator")?,
        ChatStateType::Recording => client
            .chatstate()
            .send_recording(&recipient)
            .await
            .context("failed to send WhatsApp recording indicator")?,
    }

    client.disconnect().await;
    let _ = bot_handle.await;
    Ok(())
}

async fn resolve_chat_recipient(client: &whatsapp_rust::Client, recipient: &Jid) -> Result<Jid> {
    if recipient.is_lid()
        && let Some(phone_number) = client
            .get_phone_number_from_lid(&recipient.to_string())
            .await
    {
        return format!("{phone_number}@s.whatsapp.net")
            .parse::<Jid>()
            .with_context(|| {
                format!(
                    "failed to convert mapped WhatsApp phone number `{phone_number}` into a chat JID"
                )
            });
    }

    Ok(recipient.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_state_for_kind_maps_turn_taking_states() {
        assert_eq!(
            chat_state_for_kind(StatusKind::Processing),
            Some(ChatStateType::Composing)
        );
        assert_eq!(
            chat_state_for_kind(StatusKind::Delivering),
            Some(ChatStateType::Composing)
        );
        assert_eq!(
            chat_state_for_kind(StatusKind::Completed),
            Some(ChatStateType::Paused)
        );
        assert_eq!(
            chat_state_for_kind(StatusKind::OperationFinished),
            Some(ChatStateType::Paused)
        );
    }

    #[test]
    fn chat_state_for_kind_ignores_non_typing_statuses() {
        assert_eq!(chat_state_for_kind(StatusKind::Info), None);
        assert_eq!(chat_state_for_kind(StatusKind::ApprovalNeeded), None);
        assert_eq!(chat_state_for_kind(StatusKind::AuthRequired), None);
        assert_eq!(chat_state_for_kind(StatusKind::Unknown), None);
    }

    #[test]
    fn parse_recipient_jid_accepts_direct_message_jids() {
        let jid = parse_recipient_jid("15551234567@s.whatsapp.net").unwrap();
        assert_eq!(jid.to_string(), "15551234567@s.whatsapp.net");
    }

    #[test]
    fn parse_recipient_jid_rejects_bare_phone_numbers() {
        let error = parse_recipient_jid("15551234567").unwrap_err();
        assert!(error.to_string().contains("is not a WhatsApp JID"));
    }

    #[test]
    fn parse_recipient_jid_rejects_group_jids() {
        let error = parse_recipient_jid("120363161500776365@g.us").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only support direct-message JIDs")
        );
    }
}
