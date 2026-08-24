//! Load and inspect the native Signal session backing this plugin.
//!
//! `presage` is fundamentally async, but the rest of our channel
//! plugin protocol runs sync stdin/stdout JSON-RPC. We keep a single
//! multi-thread tokio runtime alive for the lifetime of the process
//! and `block_on` into it at the sync/async boundary in each handler.
//! The runtime also holds long-lived WebSocket/HTTP state for the
//! receive loop when it is later added.

use anyhow::{Context, Result, anyhow};
use presage::Manager;
use presage::libsignal_service::configuration::SignalServers;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage_store_sqlite::SqliteStore;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::runtime::{Builder, Runtime};

use crate::protocol::ChannelConfig;
use crate::store::{resolve_passphrase, resolve_store_path, to_sqlite_url};

/// Returns the process-wide tokio runtime used to drive presage.
///
/// The runtime is created lazily on first use and lives for the
/// lifetime of the plugin process. A multi-thread builder is used so
/// that a later background receive task can run concurrently with
/// sync-side `block_on` calls from the JSON-RPC loop.
pub fn tokio_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("channel-signal-tokio")
            .build()
            .expect("failed to build channel-signal tokio runtime")
    })
}

/// Outcome of inspecting the on-disk Signal session for a given config.
#[derive(Debug)]
pub enum SessionState {
    /// No store file exists yet; the operator has never linked this
    /// account. The `store_path` is the location the linking flow will
    /// create on success.
    NotYetLinked { store_path: PathBuf },
    /// A store file exists but does not contain registration data.
    /// This can happen if a previous linking attempt was interrupted
    /// before registration data was persisted.
    StoreEmpty { store_path: PathBuf },
    /// A fully registered session was loaded.
    Registered {
        store_path: PathBuf,
        summary: RegistrationSummary,
    },
}

/// Operator-facing summary of a loaded Signal session.
#[derive(Debug, Clone)]
pub struct RegistrationSummary {
    pub aci: String,
    pub phone_number: String,
    pub device_id: u32,
    pub device_name: Option<String>,
    pub servers: &'static str,
}

/// Inspect the Signal session for the given config without side effects.
///
/// Returns `NotYetLinked` when the store file is missing, `StoreEmpty`
/// when the store exists but holds no registration yet, and
/// `Registered` with a summary when a linked session is present.
pub fn load_session(config: &ChannelConfig) -> Result<SessionState> {
    let path = resolve_store_path(config)?;

    if !path.exists() {
        return Ok(SessionState::NotYetLinked { store_path: path });
    }

    let url = to_sqlite_url(&path);
    let passphrase = resolve_passphrase(config)?;
    let runtime = tokio_runtime();

    let store = runtime
        .block_on(SqliteStore::open_with_passphrase(
            &url,
            passphrase.as_deref(),
            OnNewIdentity::Trust,
        ))
        .with_context(|| {
            format!(
                "failed to open Signal session store at `{}`",
                path.display()
            )
        })?;

    match runtime.block_on(Manager::<SqliteStore, Registered>::load_registered(store)) {
        Ok(manager) => {
            let data = manager.registration_data();
            Ok(SessionState::Registered {
                store_path: path,
                summary: RegistrationSummary {
                    aci: data.service_ids.aci.to_string(),
                    phone_number: data.phone_number.to_string(),
                    device_id: data.device_id.unwrap_or(1),
                    device_name: data.device_name.clone(),
                    servers: signal_servers_label(&data.signal_servers),
                },
            })
        }
        Err(presage::Error::NotYetRegisteredError) => {
            Ok(SessionState::StoreEmpty { store_path: path })
        }
        Err(other) => Err(anyhow!(
            "failed to load Signal session from `{}`: {other}",
            path.display()
        )),
    }
}

fn signal_servers_label(servers: &SignalServers) -> &'static str {
    match servers {
        SignalServers::Production => "production",
        SignalServers::Staging => "staging",
    }
}
