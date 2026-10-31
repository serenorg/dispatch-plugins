//! Dedicated Slack app provider operations. The host owns authorization and durable storage.

mod manifest;
mod operation;
mod provider;
#[cfg(test)]
mod test_support;
mod wire;

pub use manifest::{AppIdentity, AppOptions, AppScope, manifest};
pub use operation::LogicalProgress;
pub use operation::OperationResult;
pub use operation::{AppCommand, AppInput, Checkpoint, Disposition, OperationHost, execute};
pub use provider::{
    AppCredentials, BotIdentity, Channel, ConfigurationToken, OAuthGrant, Provider,
};
pub use wire::run;

use serde::{Deserialize, Serialize};

/// Only bounded provider codes cross the process boundary; response text never does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppError {
    InvalidInput { field: String },
    Provider { code: String },
    InvalidManifest { pointer: Option<String> },
    Unavailable {},
    RecoveryRequired {},
    StorageUnavailable {},
    BindingMismatch {},
    EnterpriseInstall {},
    AppMismatch {},
    WorkspaceMismatch {},
    MissingRequiredScope {},
    Continue {},
    Stopped {},
    RetryExhausted {},
}

impl AppError {
    pub fn invalid(field: &str) -> Self {
        Self::InvalidInput {
            field: field.into(),
        }
    }

    pub fn revoked(&self) -> bool {
        matches!(self, Self::Provider { code } if matches!(code.as_str(), "invalid_auth" | "token_revoked" | "account_inactive" | "not_authed"))
    }
}

pub type AppResult<T> = Result<T, AppError>;

pub fn valid_id(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

pub fn valid_client_id(value: &str) -> bool {
    (3..=64).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && value.bytes().filter(|b| *b == b'.').count() == 1
}

pub fn valid_secret(value: &str) -> bool {
    (16..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn valid_bot_token(value: &str) -> bool {
    value.starts_with("xoxb-")
        && (10..=512).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_graphic())
}

pub fn valid_configuration_token(value: &str) -> bool {
    value.starts_with("xoxe")
        && (10..=1024).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_graphic())
}
