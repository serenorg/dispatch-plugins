//! Resolution for the SQLite store path that backs the Signal session.
//!
//! The native presage-backed plugin owns its own session state on disk
//! at a well-known per-account path. Operators can override the path via
//! `ChannelConfig::sqlite_store_path`; otherwise the plugin uses
//! `$XDG_CONFIG_HOME/dispatch/channels/signal/<account>/store.db` or
//! `$HOME/.config/...` on Unix-like systems.
//!
//! These helpers are wired up ahead of their call sites so the storage
//! and session-loading pipeline can be built up in focused commits.
//! Subsequent commits (configure, health, linking, receive, deliver)
//! will exercise them from main.rs.

#![allow(dead_code)]

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

use crate::protocol::ChannelConfig;

const DEFAULT_ACCOUNT: &str = "default";

/// Resolve the final on-disk path for a Signal session store, consulting
/// (in order) the explicit `sqlite_store_path`, an account-scoped default
/// under the XDG config directory, and finally a `$HOME`-relative
/// fallback when no XDG variables are set.
pub fn resolve_store_path(config: &ChannelConfig) -> Result<PathBuf> {
    if let Some(explicit) = config
        .sqlite_store_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(expand_user(explicit));
    }

    let mut base = default_store_root()?;
    base.push(account_dir(config));
    base.push("store.db");
    Ok(base)
}

/// Returns the base directory under which per-account Signal stores live
/// when no explicit path is configured.
fn default_store_root() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.trim().is_empty()
    {
        let mut path = PathBuf::from(xdg);
        path.push("dispatch/channels/signal");
        return Ok(path);
    }

    let home = std::env::var("HOME").map_err(|_| {
        anyhow!(
            "neither XDG_CONFIG_HOME nor HOME is set; cannot resolve default Signal store location"
        )
    })?;
    if home.trim().is_empty() {
        return Err(anyhow!(
            "HOME is empty; cannot resolve default Signal store location"
        ));
    }
    let mut path = PathBuf::from(home);
    path.push(".config/dispatch/channels/signal");
    Ok(path)
}

fn account_dir(config: &ChannelConfig) -> &str {
    config
        .account
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ACCOUNT)
}

/// Expand a leading `~/` in a path string. Other `~` forms are left
/// alone (we do not try to resolve other users' home directories).
fn expand_user(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
        && !home.trim().is_empty()
    {
        let mut expanded = PathBuf::from(home);
        expanded.push(stripped);
        return expanded;
    }
    PathBuf::from(path)
}

/// Convert an on-disk store path into the `sqlite://` URL form that
/// `presage-store-sqlite` (and its underlying `sqlx` layer) expect.
pub fn to_sqlite_url(path: &Path) -> String {
    // sqlx's Sqlite URL parser accepts `sqlite://<path>` where the path
    // is interpreted as a file path. Absolute paths need the leading
    // slash preserved, yielding `sqlite:///absolute/path`.
    let display = path.display().to_string();
    if display.starts_with('/') {
        format!("sqlite://{display}")
    } else {
        format!("sqlite:{display}")
    }
}

/// Resolve an optional store passphrase from the configured env var.
pub fn resolve_passphrase(config: &ChannelConfig) -> Result<Option<String>> {
    let Some(env_name) = config
        .passphrase_env
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    use anyhow::Context as _;

    let value = std::env::var(env_name).with_context(|| {
        format!("channel-signal passphrase_env `{env_name}` is set but not readable")
    })?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests in this module mutate process-wide env variables (HOME,
    // XDG_CONFIG_HOME). Serialize them so cargo's default parallel test
    // runner does not interleave env state between threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn resolve_store_path_uses_explicit_setting() {
        let config = ChannelConfig {
            sqlite_store_path: Some("/tmp/dispatch-signal/store.db".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_store_path(&config).unwrap(),
            PathBuf::from("/tmp/dispatch-signal/store.db")
        );
    }

    #[test]
    fn resolve_store_path_expands_home_prefix() {
        let _guard = lock_env();
        // SAFETY: guarded by ENV_LOCK against interleaving with sibling tests.
        unsafe {
            std::env::set_var("HOME", "/tmp/dispatch-signal-home");
        }
        let config = ChannelConfig {
            sqlite_store_path: Some("~/signal/store.db".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_store_path(&config).unwrap(),
            PathBuf::from("/tmp/dispatch-signal-home/signal/store.db")
        );
    }

    #[test]
    fn resolve_store_path_defaults_to_account_subdir_when_xdg_set() {
        let _guard = lock_env();
        // SAFETY: guarded by ENV_LOCK against interleaving with sibling tests.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/dispatch-signal-xdg");
        }
        let config = ChannelConfig {
            account: Some("work".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_store_path(&config).unwrap(),
            PathBuf::from("/tmp/dispatch-signal-xdg/dispatch/channels/signal/work/store.db")
        );
    }

    #[test]
    fn resolve_store_path_falls_back_to_home_config() {
        let _guard = lock_env();
        // SAFETY: guarded by ENV_LOCK against interleaving with sibling tests.
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::set_var("HOME", "/tmp/dispatch-signal-home2");
        }
        let config = ChannelConfig::default();

        assert_eq!(
            resolve_store_path(&config).unwrap(),
            PathBuf::from(
                "/tmp/dispatch-signal-home2/.config/dispatch/channels/signal/default/store.db"
            )
        );
    }

    #[test]
    fn to_sqlite_url_prefixes_absolute_paths() {
        assert_eq!(
            to_sqlite_url(&PathBuf::from("/var/lib/dispatch/store.db")),
            "sqlite:///var/lib/dispatch/store.db"
        );
    }

    #[test]
    fn to_sqlite_url_allows_relative_paths() {
        assert_eq!(to_sqlite_url(&PathBuf::from("store.db")), "sqlite:store.db");
    }

    #[test]
    fn resolve_passphrase_returns_none_when_unset() {
        let config = ChannelConfig::default();
        assert_eq!(resolve_passphrase(&config).unwrap(), None);
    }

    #[test]
    fn resolve_passphrase_uses_configured_env_var() {
        let _guard = lock_env();
        // SAFETY: guarded by ENV_LOCK against interleaving with sibling tests.
        unsafe {
            std::env::set_var("CHANNEL_SIGNAL_TEST_PASSPHRASE", "secret");
        }
        let config = ChannelConfig {
            passphrase_env: Some("CHANNEL_SIGNAL_TEST_PASSPHRASE".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_passphrase(&config).unwrap(),
            Some("secret".to_string())
        );
    }
}
