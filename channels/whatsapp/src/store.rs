#![allow(dead_code)]

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

use crate::protocol::ChannelConfig;

const DEFAULT_ACCOUNT: &str = "default";

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

fn default_store_root() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.trim().is_empty()
    {
        let mut path = PathBuf::from(xdg);
        path.push("dispatch/channels/whatsapp");
        return Ok(path);
    }

    let home = std::env::var("HOME").map_err(|_| {
        anyhow!(
            "neither XDG_CONFIG_HOME nor HOME is set; cannot resolve default WhatsApp store location"
        )
    })?;
    if home.trim().is_empty() {
        return Err(anyhow!(
            "HOME is empty; cannot resolve default WhatsApp store location"
        ));
    }

    let mut path = PathBuf::from(home);
    path.push(".config/dispatch/channels/whatsapp");
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

pub fn to_sqlite_url(path: &Path) -> String {
    let display = path.display().to_string();
    if display.starts_with('/') {
        format!("sqlite://{display}")
    } else {
        format!("sqlite:{display}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn resolve_store_path_uses_explicit_setting() {
        let config = ChannelConfig {
            sqlite_store_path: Some("/tmp/dispatch-whatsapp/store.db".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_store_path(&config).unwrap(),
            PathBuf::from("/tmp/dispatch-whatsapp/store.db")
        );
    }

    #[test]
    fn resolve_store_path_expands_home_prefix() {
        let _guard = lock_env();
        unsafe {
            std::env::set_var("HOME", "/tmp/dispatch-whatsapp-home");
        }

        let config = ChannelConfig {
            sqlite_store_path: Some("~/whatsapp/store.db".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_store_path(&config).unwrap(),
            PathBuf::from("/tmp/dispatch-whatsapp-home/whatsapp/store.db")
        );
    }

    #[test]
    fn resolve_store_path_defaults_to_account_subdir_when_xdg_set() {
        let _guard = lock_env();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/dispatch-whatsapp-xdg");
        }

        let config = ChannelConfig {
            account: Some("work".to_string()),
            ..ChannelConfig::default()
        };

        assert_eq!(
            resolve_store_path(&config).unwrap(),
            PathBuf::from("/tmp/dispatch-whatsapp-xdg/dispatch/channels/whatsapp/work/store.db")
        );
    }

    #[test]
    fn resolve_store_path_falls_back_to_home_config() {
        let _guard = lock_env();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::set_var("HOME", "/tmp/dispatch-whatsapp-home2");
        }

        assert_eq!(
            resolve_store_path(&ChannelConfig::default()).unwrap(),
            PathBuf::from(
                "/tmp/dispatch-whatsapp-home2/.config/dispatch/channels/whatsapp/default/store.db"
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
}
