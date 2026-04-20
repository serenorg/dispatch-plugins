use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::runtime::{Builder, Runtime};
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::store::traits::DeviceStore;

use crate::protocol::ChannelConfig;
use crate::store::{resolve_store_path, to_sqlite_url};

pub fn tokio_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("channel-whatsapp-tokio")
            .build()
            .expect("failed to build channel-whatsapp tokio runtime")
    })
}

#[derive(Debug)]
pub enum SessionState {
    NotYetLinked {
        store_path: PathBuf,
    },
    StoreEmpty {
        store_path: PathBuf,
    },
    Registered {
        store_path: PathBuf,
        summary: RegistrationSummary,
    },
}

#[derive(Debug, Clone)]
pub struct RegistrationSummary {
    pub phone_jid: Option<String>,
    pub lid_jid: Option<String>,
    pub device_id: i32,
    pub push_name: Option<String>,
}

pub fn load_session(config: &ChannelConfig) -> Result<SessionState> {
    let store_path = resolve_store_path(config)?;
    if !store_path.exists() {
        return Ok(SessionState::NotYetLinked { store_path });
    }

    let runtime = tokio_runtime();
    let sqlite_url = to_sqlite_url(&store_path);
    let store = runtime
        .block_on(SqliteStore::new(&sqlite_url))
        .with_context(|| {
            format!(
                "failed to open WhatsApp session store at `{}`",
                store_path.display()
            )
        })?;

    let exists = runtime.block_on(store.exists()).with_context(|| {
        format!(
            "failed to inspect WhatsApp device state in `{}`",
            store_path.display()
        )
    })?;
    if !exists {
        return Ok(SessionState::StoreEmpty { store_path });
    }

    let device = runtime
        .block_on(store.load())
        .with_context(|| {
            format!(
                "failed to load WhatsApp device state from `{}`",
                store_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow!(
                "WhatsApp store `{}` reported an existing device row but returned no device data",
                store_path.display()
            )
        })?;

    if device.pn.is_none() && device.lid.is_none() && device.account.is_none() {
        return Ok(SessionState::StoreEmpty { store_path });
    }

    let push_name = (!device.push_name.trim().is_empty()).then(|| device.push_name.clone());

    Ok(SessionState::Registered {
        store_path,
        summary: RegistrationSummary {
            phone_jid: device.pn.as_ref().map(ToString::to_string),
            lid_jid: device.lid.as_ref().map(ToString::to_string),
            device_id: store.device_id(),
            push_name,
        },
    })
}
