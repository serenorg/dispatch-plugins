use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::provider::{AppIcon, OAuthGrant};
use super::{
    AppCredentials, AppError, AppIdentity, AppOptions, AppResult, AppScope, Channel,
    ConfigurationToken, Provider, manifest, valid_bot_token, valid_id,
};

const MAX_ATTEMPTS: u8 = 6;

/// Immutable input loaded through the authenticated operation broker, never from invocation JSON.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppInput {
    pub command: AppCommand,
    pub credentials: Option<AppCredentials>,
    pub configuration: Option<ConfigurationToken>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppCommand {
    Manifest {
        identity: AppIdentity,
        options: AppOptions,
        callback_url: String,
        events_url: Option<String>,
    },
    Register {},
    Authorize {
        callback_url: String,
        state: String,
        options: AppOptions,
    },
    Apply {
        identity: AppIdentity,
        options: AppOptions,
        callback_url: String,
        events_url: Option<String>,
        icon: Option<AppIcon>,
    },
    ConfigureEvents {
        identity: AppIdentity,
        options: AppOptions,
        callback_url: String,
        events_url: String,
    },
    Exchange {
        code: String,
        callback_url: String,
        options: AppOptions,
        expected_team_id: Option<String>,
    },
    Verify {},
    Revoke {},
    Uninstall {},
    Channels {
        private_channels: bool,
        cursor: Option<String>,
    },
    InspectChannels {
        channels: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
    Validate,
    ValidateWithRotatedToken,
    Create,
    CreateWithRotatedToken,
    Update,
    UpdateWithRotatedToken,
    Icon,
    IconWithRotatedToken,
    Rotate,
    Exchange,
    Verify,
    Revoke,
    Uninstall,
    Channels,
    InspectChannel,
}

impl Step {
    fn repeatable(self) -> bool {
        !matches!(
            self,
            Self::Create | Self::CreateWithRotatedToken | Self::Rotate | Self::Exchange
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StepState {
    InFlight { attempt: u8 },
    Succeeded { receipt: Value },
    Failed { attempt: u8, error: AppError },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFailure {
    error: AppError,
    attempt: u8,
}

/// Opaque to Core. Every receipt and token is kept only in the protected organization store.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    steps: BTreeMap<Step, StepState>,
    last_failure: Option<StepFailure>,
    recovery_required: bool,
    inspected_channels: BTreeMap<String, Channel>,
    pub result: Option<OperationResult>,
}

/// Protected terminal output; Core never needs to decode the provider checkpoint to obtain it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationResult {
    Manifest {
        manifest: Value,
        requested_scopes: Vec<AppScope>,
        create_url: String,
        settings_url: Option<String>,
        manifest_settings_url: Option<String>,
    },
    Registered {
        credentials: AppCredentials,
    },
    Authorization {
        authorize_url: String,
        requested_scopes: Vec<AppScope>,
    },
    Application {
        credentials: AppCredentials,
        configuration: ConfigurationToken,
        requested_scopes: Vec<AppScope>,
    },
    EventsConfigured {
        configuration: ConfigurationToken,
    },
    Installation {
        credentials: AppCredentials,
        granted_scopes: Vec<String>,
        team_name: Option<String>,
        enterprise_id: Option<String>,
    },
    Verified {},
    CredentialsRevoked {},
    Channels {
        channels: Vec<Channel>,
        next_cursor: Option<String>,
    },
    ChannelInspections {
        channels: Vec<Channel>,
    },
}

impl std::fmt::Debug for OperationResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationResult")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Disposition {
    Running {},
    Succeeded {},
    Refused { error: AppError },
    RecoveryRequired {},
    Retry { after_seconds: u16 },
    Stopped {},
}

/// Only these validated facts may cross from provider state into Core's logical lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LogicalProgress {
    Application { credentials: AppCredentials },
    Configuration { credentials: ConfigurationToken },
}

impl LogicalProgress {
    pub fn validate(&self) -> AppResult<()> {
        match self {
            Self::Application { credentials } => {
                credentials.validate()?;
                if credentials.bot_token.is_some()
                    || credentials.team_id.is_some()
                    || credentials.bot_user_id.is_some()
                {
                    return Err(AppError::invalid("application_progress"));
                }
                Ok(())
            }
            Self::Configuration { credentials } => {
                credentials.validate()?;
                if credentials.refresh_token.is_none() {
                    return Err(AppError::invalid("configuration_progress"));
                }
                Ok(())
            }
        }
    }
}

/// Acknowledgement means checkpoint, protected progress and Core reference/lifecycle adoption are durable together.
pub trait OperationHost {
    fn checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
        disposition: &Disposition,
        progress: Option<&LogicalProgress>,
        result: Option<&OperationResult>,
    ) -> AppResult<()>;
}

fn step(
    host: &mut impl OperationHost,
    checkpoint: &mut Checkpoint,
    name: Step,
    application: Option<&AppCredentials>,
    call: impl FnOnce() -> AppResult<Value>,
) -> AppResult<Value> {
    if let Some(credentials) = application {
        if !matches!(name, Step::Update | Step::UpdateWithRotatedToken) {
            return Err(AppError::invalid("application_progress"));
        }
        credentials.validate()?;
    }
    let attempt = match checkpoint.steps.get(&name) {
        Some(StepState::Succeeded { receipt }) => return Ok(receipt.clone()),
        Some(StepState::InFlight { .. }) if !name.repeatable() => {
            return Err(AppError::RecoveryRequired {});
        }
        Some(StepState::InFlight { attempt } | StepState::Failed { attempt, .. }) => {
            attempt.saturating_add(1)
        }
        None => 1,
    };
    if attempt > MAX_ATTEMPTS {
        return Err(AppError::RetryExhausted {});
    }
    if let Some(StepState::Failed { error, .. }) = checkpoint.steps.get(&name)
        && !transient(error)
    {
        return Err(error.clone());
    }
    let mut next = checkpoint.clone();
    next.steps.insert(name, StepState::InFlight { attempt });
    next.last_failure = None;
    // A failed CAS acknowledgement must prevent the provider call.
    host.checkpoint(&next, &Disposition::Running {}, None, None)?;
    *checkpoint = next;
    match call() {
        Ok(receipt) => {
            let mut next = checkpoint.clone();
            next.steps.insert(
                name,
                StepState::Succeeded {
                    receipt: receipt.clone(),
                },
            );
            let progress = match progress_for_receipt(name, &receipt, application) {
                Ok(progress) => progress,
                Err(_) => {
                    next.recovery_required = true;
                    host.checkpoint(&next, &Disposition::RecoveryRequired {}, None, None)?;
                    *checkpoint = next;
                    return Err(AppError::RecoveryRequired {});
                }
            };
            host.checkpoint(&next, &Disposition::Running {}, progress.as_ref(), None)?;
            *checkpoint = next;
            Ok(receipt)
        }
        Err(error) => {
            if !name.repeatable() && !definitive_refusal(&error) {
                // Keep the in-flight marker. Neither another request nor a restart may repeat it.
                let mut next = checkpoint.clone();
                next.recovery_required = true;
                host.checkpoint(&next, &Disposition::RecoveryRequired {}, None, None)?;
                *checkpoint = next;
                return Err(AppError::RecoveryRequired {});
            }
            let mut next = checkpoint.clone();
            next.steps.insert(
                name,
                StepState::Failed {
                    attempt,
                    error: error.clone(),
                },
            );
            next.last_failure = Some(StepFailure {
                attempt,
                error: error.clone(),
            });
            host.checkpoint(&next, &Disposition::Running {}, None, None)?;
            *checkpoint = next;
            Err(error)
        }
    }
}

fn progress_for_receipt(
    step: Step,
    receipt: &Value,
    application: Option<&AppCredentials>,
) -> AppResult<Option<LogicalProgress>> {
    let progress = match (step, application) {
        (Step::Create | Step::CreateWithRotatedToken, _) => LogicalProgress::Application {
            credentials: Provider::created_credentials(receipt)?,
        },
        (Step::Rotate, _) => LogicalProgress::Configuration {
            credentials: Provider::rotated_configuration(receipt)?,
        },
        (Step::Update | Step::UpdateWithRotatedToken, Some(credentials)) => {
            LogicalProgress::Application {
                credentials: AppCredentials {
                    bot_token: None,
                    team_id: None,
                    bot_user_id: None,
                    ..credentials.clone()
                },
            }
        }
        _ => return Ok(None),
    };
    progress.validate()?;
    Ok(Some(progress))
}

fn transient(error: &AppError) -> bool {
    matches!(error, AppError::Unavailable {})
        || matches!(error, AppError::Provider { code } if matches!(code.as_str(), "ratelimited" | "internal_error" | "fatal_error" | "service_unavailable" | "request_timeout"))
}

fn definitive_refusal(error: &AppError) -> bool {
    matches!(error, AppError::InvalidManifest { .. })
        || matches!(error, AppError::Provider { code } if matches!(code.as_str(), "invalid_auth" | "not_authed" | "token_revoked" | "account_inactive" | "not_allowed_token_type" | "missing_scope" | "no_permission" | "invalid_token" | "token_expired" | "enterprise_is_restricted" | "org_login_required" | "invalid_team_id"))
}

/// Execute one invocation; a lifecycle owner schedules saved retry dispositions.
pub fn execute(
    provider: &Provider,
    input: &AppInput,
    checkpoint: &mut Checkpoint,
    host: &mut impl OperationHost,
) -> AppResult<Value> {
    let result = execute_inner(provider, input, checkpoint, host);
    if let Err(error) = &result {
        // An in-flight consuming operation is never overwritten by a duplicate invocation.
        if matches!(
            error,
            AppError::StorageUnavailable {} | AppError::Stopped {}
        ) {
            return result;
        }
        if *error == (AppError::RecoveryRequired {}) {
            // A duplicate must not advance an active executor's consuming-step sequence.
            if !checkpoint
                .steps
                .values()
                .any(|s| matches!(s, StepState::InFlight { .. }))
                && !checkpoint.recovery_required
            {
                let mut next = checkpoint.clone();
                next.recovery_required = true;
                host.checkpoint(&next, &Disposition::RecoveryRequired {}, None, None)?;
                *checkpoint = next;
            }
            return result;
        }
        let attempt = checkpoint
            .last_failure
            .as_ref()
            .filter(|failure| failure.error == *error)
            .map(|failure| failure.attempt)
            .unwrap_or(MAX_ATTEMPTS);
        let disposition = if *error == (AppError::Continue {}) {
            Disposition::Retry { after_seconds: 1 }
        } else if transient(error) && attempt < MAX_ATTEMPTS {
            Disposition::Retry {
                after_seconds: (1u16 << attempt.min(5)).min(60),
            }
        } else {
            Disposition::Refused {
                error: error.clone(),
            }
        };
        host.checkpoint(checkpoint, &disposition, None, None)?;
    }
    result
}

fn execute_inner(
    provider: &Provider,
    input: &AppInput,
    checkpoint: &mut Checkpoint,
    host: &mut impl OperationHost,
) -> AppResult<Value> {
    if checkpoint.recovery_required {
        return Err(AppError::RecoveryRequired {});
    }
    if let Some(result) = &checkpoint.result {
        return serde_json::to_value(result).map_err(|_| AppError::Unavailable {});
    }
    if let Some(credentials) = &input.credentials {
        credentials.validate()?;
    }
    if let Some(configuration) = &input.configuration {
        configuration.validate()?;
    }
    let result = match &input.command {
        AppCommand::Manifest {
            identity,
            options,
            callback_url,
            events_url,
        } => {
            let manifest = manifest(identity, *options, callback_url, events_url.as_deref())?;
            let mut create = url::Url::parse("https://api.slack.com/apps").expect("fixed URL");
            create
                .query_pairs_mut()
                .append_pair("new_app", "1")
                .append_pair("manifest_json", &manifest.to_string());
            OperationResult::Manifest {
                manifest,
                requested_scopes: options.scopes(),
                create_url: create.into(),
                settings_url: input
                    .credentials
                    .as_ref()
                    .map(|c| format!("https://api.slack.com/apps/{}", c.app_id)),
                manifest_settings_url: input
                    .credentials
                    .as_ref()
                    .map(|c| format!("https://api.slack.com/apps/{}/app-manifest", c.app_id)),
            }
        }
        AppCommand::Register {} => OperationResult::Registered {
            credentials: credentials(input)?.clone(),
        },
        AppCommand::Authorize {
            callback_url,
            state,
            options,
        } => {
            super::manifest::validate_url(callback_url, "callback_url")?;
            if !(32..=256).contains(&state.len())
                || !state
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                return Err(AppError::invalid("state"));
            }
            let credentials = credentials(input)?;
            let scopes = options
                .scopes()
                .into_iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let mut authorize =
                url::Url::parse("https://slack.com/oauth/v2/authorize").expect("fixed URL");
            authorize
                .query_pairs_mut()
                .append_pair("client_id", &credentials.client_id)
                .append_pair("scope", &scopes)
                .append_pair("redirect_uri", callback_url)
                .append_pair("state", state);
            OperationResult::Authorization {
                authorize_url: authorize.into(),
                requested_scopes: options.scopes(),
            }
        }
        AppCommand::Apply {
            identity,
            options,
            callback_url,
            events_url,
            icon,
        } => {
            let manifest = manifest(identity, *options, callback_url, events_url.as_deref())?;
            if let Some(icon) = icon {
                icon.decode()?;
            }
            configured_step(
                provider,
                input,
                checkpoint,
                host,
                Step::Validate,
                Step::ValidateWithRotatedToken,
                |token| {
                    provider
                        .validate_manifest(token, &manifest)
                        .map(|_| Value::Null)
                },
            )?;
            let credentials = if let Some(credentials) = &input.credentials {
                credentials.clone()
            } else {
                let receipt = configured_step(
                    provider,
                    input,
                    checkpoint,
                    host,
                    Step::Create,
                    Step::CreateWithRotatedToken,
                    |token| provider.create_receipt(token, &manifest),
                )?;
                Provider::created_credentials(&receipt)
                    .map_err(|_| AppError::RecoveryRequired {})?
            };
            if input.credentials.is_some() {
                update(provider, input, checkpoint, host, &credentials, &manifest)?;
            }
            if let Some(icon) = icon {
                configured_step(
                    provider,
                    input,
                    checkpoint,
                    host,
                    Step::Icon,
                    Step::IconWithRotatedToken,
                    |token| {
                        provider
                            .icon(token, &credentials.app_id, icon)
                            .map(|_| Value::Null)
                    },
                )?;
            }
            OperationResult::Application {
                credentials,
                configuration: current_configuration(input, checkpoint)?,
                requested_scopes: options.scopes(),
            }
        }
        AppCommand::ConfigureEvents {
            identity,
            options,
            callback_url,
            events_url,
        } => {
            let credentials = credentials(input)?;
            bot_token(credentials)?;
            let manifest = manifest(identity, *options, callback_url, Some(events_url))?;
            update(provider, input, checkpoint, host, credentials, &manifest)?;
            OperationResult::EventsConfigured {
                configuration: current_configuration(input, checkpoint)?,
            }
        }
        AppCommand::Exchange {
            code,
            callback_url,
            options,
            expected_team_id,
        } => {
            super::manifest::validate_url(callback_url, "callback_url")?;
            if code.is_empty() || code.len() > 1024 {
                return Err(AppError::invalid("code"));
            }
            let credentials = credentials(input)?;
            let receipt = step(host, checkpoint, Step::Exchange, None, || {
                provider.exchange_receipt(credentials, code, callback_url)
            })?;
            let grant = checked_grant(receipt)?;
            if grant.is_enterprise_install {
                return Err(AppError::EnterpriseInstall {});
            }
            if grant.app_id.as_deref() != Some(&credentials.app_id) {
                return Err(AppError::AppMismatch {});
            }
            let team = grant.team.ok_or(AppError::RecoveryRequired {})?;
            let token = grant.access_token.ok_or(AppError::RecoveryRequired {})?;
            let bot = grant.bot_user_id.ok_or(AppError::RecoveryRequired {})?;
            if expected_team_id
                .as_deref()
                .or(credentials.team_id.as_deref())
                .is_some_and(|expected| expected != team.id)
            {
                return Err(AppError::WorkspaceMismatch {});
            }
            let granted: Vec<&str> = grant
                .scope
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if options
                .scopes()
                .iter()
                .any(|s| !granted.contains(&s.as_str()))
            {
                return Err(AppError::MissingRequiredScope {});
            }
            let identity = step(host, checkpoint, Step::Verify, None, || {
                let identity = provider.verify(&token)?;
                Ok(json!({"team_id": identity.team_id, "user_id": identity.user_id}))
            })?;
            if identity["team_id"] != team.id || identity["user_id"] != bot {
                return Err(AppError::RecoveryRequired {});
            }
            let installed = AppCredentials {
                bot_token: Some(token),
                team_id: Some(team.id),
                bot_user_id: Some(bot),
                ..credentials.clone()
            };
            OperationResult::Installation {
                credentials: installed,
                granted_scopes: granted.into_iter().map(str::to_owned).collect(),
                team_name: team.name.filter(|n| n.chars().count() <= 255),
                enterprise_id: grant.enterprise.map(|e| e.id),
            }
        }
        AppCommand::Verify {} => {
            let credentials = credentials(input)?;
            let token = bot_token(credentials)?;
            let identity = step(host, checkpoint, Step::Verify, None, || {
                let identity = provider.verify(token)?;
                Ok(json!({"team_id": identity.team_id, "user_id": identity.user_id}))
            })?;
            if identity["team_id"].as_str() != credentials.team_id.as_deref()
                || identity["user_id"].as_str() != credentials.bot_user_id.as_deref()
            {
                return Err(AppError::BindingMismatch {});
            }
            OperationResult::Verified {}
        }
        AppCommand::Revoke {} => {
            let token = bot_token(credentials(input)?)?;
            step(host, checkpoint, Step::Revoke, None, || {
                removal(provider.revoke(token))
            })?;
            OperationResult::CredentialsRevoked {}
        }
        AppCommand::Uninstall {} => {
            let credentials = credentials(input)?;
            let uninstalled = step(host, checkpoint, Step::Uninstall, None, || {
                removal(provider.uninstall(credentials))
            });
            match uninstalled {
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error,
                        AppError::StorageUnavailable {}
                            | AppError::Stopped {}
                            | AppError::RecoveryRequired {}
                    ) || transient(&error) =>
                {
                    return Err(error);
                }
                Err(error) if definitive_refusal(&error) => {
                    let token = bot_token(credentials)?;
                    step(host, checkpoint, Step::Revoke, None, || {
                        removal(provider.revoke(token))
                    })?;
                }
                Err(error) => return Err(error),
            }
            OperationResult::CredentialsRevoked {}
        }
        AppCommand::Channels {
            private_channels,
            cursor,
        } => {
            if cursor
                .as_ref()
                .is_some_and(|c| c.len() > 2048 || c.chars().any(char::is_control))
            {
                return Err(AppError::invalid("cursor"));
            }
            let token = bot_token(credentials(input)?)?;
            let page = step(host, checkpoint, Step::Channels, None, || {
                let (channels, cursor) =
                    provider.channels(token, *private_channels, cursor.as_deref())?;
                Ok(json!({"channels": channels, "next_cursor": cursor}))
            })?;
            #[derive(Deserialize)]
            struct Page {
                channels: Vec<Channel>,
                next_cursor: Option<String>,
            }
            let page: Page = serde_json::from_value(page).map_err(|_| AppError::Unavailable {})?;
            OperationResult::Channels {
                channels: page.channels,
                next_cursor: page.next_cursor,
            }
        }
        AppCommand::InspectChannels { channels } => {
            if channels.len() > 100
                || channels.iter().any(|c| !valid_id(c))
                || channels
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != channels.len()
            {
                return Err(AppError::invalid("channels"));
            }
            let token = bot_token(credentials(input)?)?;
            OperationResult::ChannelInspections {
                channels: inspect_channels(provider, token, channels, checkpoint, host)?,
            }
        }
    };
    let mut next = checkpoint.clone();
    next.result = Some(result.clone());
    let progress = if let OperationResult::Registered { credentials } = &result {
        let progress = LogicalProgress::Application {
            credentials: credentials.clone(),
        };
        progress.validate()?;
        Some(progress)
    } else {
        None
    };
    host.checkpoint(
        &next,
        &Disposition::Succeeded {},
        progress.as_ref(),
        Some(&result),
    )?;
    *checkpoint = next;
    serde_json::to_value(result).map_err(|_| AppError::Unavailable {})
}

fn credentials(input: &AppInput) -> AppResult<&AppCredentials> {
    input
        .credentials
        .as_ref()
        .ok_or_else(|| AppError::invalid("credentials"))
}
fn bot_token(credentials: &AppCredentials) -> AppResult<&str> {
    credentials
        .bot_token
        .as_deref()
        .ok_or_else(|| AppError::invalid("bot_token"))
}

fn current_configuration(
    input: &AppInput,
    checkpoint: &Checkpoint,
) -> AppResult<ConfigurationToken> {
    if let Some(StepState::Succeeded { receipt }) = checkpoint.steps.get(&Step::Rotate) {
        return Provider::rotated_configuration(receipt).map_err(|_| AppError::RecoveryRequired {});
    }
    input
        .configuration
        .clone()
        .ok_or_else(|| AppError::invalid("configuration_token"))
}

fn update(
    provider: &Provider,
    input: &AppInput,
    checkpoint: &mut Checkpoint,
    host: &mut impl OperationHost,
    credentials: &AppCredentials,
    manifest: &Value,
) -> AppResult<()> {
    configured_step(
        provider,
        input,
        checkpoint,
        host,
        Step::Update,
        Step::UpdateWithRotatedToken,
        |token| {
            provider
                .update(token, &credentials.app_id, manifest)
                .map(|_| Value::Null)
        },
    )
    .map(|_| ())
}

fn configured_step(
    provider: &Provider,
    input: &AppInput,
    checkpoint: &mut Checkpoint,
    host: &mut impl OperationHost,
    original: Step,
    rotated: Step,
    call: impl Fn(&str) -> AppResult<Value>,
) -> AppResult<Value> {
    // Token rotation cannot reopen a completed logical operation or hide an unresolved effect.
    if !original.repeatable()
        && [original, rotated]
            .iter()
            .any(|name| matches!(checkpoint.steps.get(name), Some(StepState::InFlight { .. })))
    {
        return Err(AppError::RecoveryRequired {});
    }
    let receipts = [original, rotated]
        .iter()
        .filter_map(|name| match checkpoint.steps.get(name) {
            Some(StepState::Succeeded { receipt }) => Some(receipt.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !original.repeatable() && receipts.len() > 1 {
        return Err(AppError::RecoveryRequired {});
    }
    if let Some(receipt) = receipts.into_iter().next() {
        return Ok(receipt);
    }
    let configuration = current_configuration(input, checkpoint)?;
    // Only an existing Apply adopts desired profile/options; event setup is a separate logical operation.
    let application =
        if original == Step::Update && matches!(&input.command, AppCommand::Apply { .. }) {
            input.credentials.as_ref()
        } else {
            None
        };
    // A rejected expired token may be rotated once; its raw receipt is saved before reuse.
    if !checkpoint.steps.contains_key(&Step::Rotate) {
        let result = step(host, checkpoint, original, application, || {
            call(&configuration.configuration_token)
        });
        match result {
            Ok(value) => return Ok(value),
            Err(error)
                if configuration_expired(&error) && configuration.refresh_token.is_some() =>
            {
                let refresh = configuration.refresh_token.as_deref().unwrap();
                let receipt = step(host, checkpoint, Step::Rotate, None, || {
                    provider.rotate_receipt(refresh)
                })?;
                Provider::rotated_configuration(&receipt)
                    .map_err(|_| AppError::RecoveryRequired {})?;
            }
            Err(error) => return Err(error),
        }
    }
    if !matches!(
        checkpoint.steps.get(&Step::Rotate),
        Some(StepState::Succeeded { .. })
    ) {
        let refresh = input
            .configuration
            .as_ref()
            .and_then(|c| c.refresh_token.as_deref())
            .ok_or_else(|| AppError::invalid("refresh_token"))?;
        let receipt = step(host, checkpoint, Step::Rotate, None, || {
            provider.rotate_receipt(refresh)
        })?;
        Provider::rotated_configuration(&receipt).map_err(|_| AppError::RecoveryRequired {})?;
    }
    let configuration = current_configuration(input, checkpoint)?;
    step(host, checkpoint, rotated, application, || {
        call(&configuration.configuration_token)
    })
}

fn configuration_expired(error: &AppError) -> bool {
    matches!(error, AppError::Provider { code } if code == "token_expired")
}

fn checked_grant(receipt: Value) -> AppResult<OAuthGrant> {
    let grant: OAuthGrant =
        serde_json::from_value(receipt).map_err(|_| AppError::RecoveryRequired {})?;
    if grant.app_id.as_deref().is_none_or(|id| !valid_id(id))
        || grant
            .access_token
            .as_deref()
            .is_none_or(|token| !valid_bot_token(token))
        || grant.bot_user_id.as_deref().is_none_or(|id| !valid_id(id))
        || grant.scope.len() > 4096
        || grant.team.as_ref().is_some_and(|team| !valid_id(&team.id))
        || grant
            .enterprise
            .as_ref()
            .is_some_and(|enterprise| !valid_id(&enterprise.id))
        || (!grant.is_enterprise_install && grant.team.is_none())
        || (grant.is_enterprise_install && grant.enterprise.is_none())
    {
        return Err(AppError::RecoveryRequired {});
    }
    Ok(grant)
}

fn inspect_channels(
    provider: &Provider,
    token: &str,
    channels: &[String],
    checkpoint: &mut Checkpoint,
    host: &mut impl OperationHost,
) -> AppResult<Vec<Channel>> {
    let mut inspected = 0;
    for id in channels {
        if checkpoint.inspected_channels.contains_key(id) {
            continue;
        }
        if inspected == 20 {
            return Err(AppError::Continue {});
        }
        let receipt = step(host, checkpoint, Step::InspectChannel, None, || {
            serde_json::to_value(provider.channel(token, id)?).map_err(|_| AppError::Unavailable {})
        })?;
        let channel: Channel =
            serde_json::from_value(receipt).map_err(|_| AppError::Unavailable {})?;
        if channel.id != *id {
            return Err(AppError::BindingMismatch {});
        }
        let mut next = checkpoint.clone();
        next.inspected_channels.insert(id.clone(), channel);
        next.steps.remove(&Step::InspectChannel);
        if serde_json::to_vec(&next.inspected_channels)
            .map_err(|_| AppError::Unavailable {})?
            .len()
            > 128 * 1024
        {
            return Err(AppError::invalid("channel_result_size"));
        }
        host.checkpoint(&next, &Disposition::Running {}, None, None)?;
        *checkpoint = next;
        inspected += 1;
    }
    Ok(channels
        .iter()
        .map(|id| checkpoint.inspected_channels[id].clone())
        .collect())
}

fn removal(result: AppResult<()>) -> AppResult<Value> {
    match result {
        Ok(()) => Ok(Value::Null),
        Err(error) if error.revoked() => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct Store {
        saved: Checkpoint,
        writes: usize,
        fail_at: Option<usize>,
        dispositions: Vec<Disposition>,
        lose_receipt_ack: Option<Step>,
        stop_on_receipt: Option<Step>,
        progress: Vec<LogicalProgress>,
        refuse_application_adoption: bool,
        completed: Option<OperationResult>,
    }

    impl OperationHost for Store {
        fn checkpoint(
            &mut self,
            checkpoint: &Checkpoint,
            disposition: &Disposition,
            progress: Option<&LogicalProgress>,
            result: Option<&OperationResult>,
        ) -> AppResult<()> {
            self.writes += 1;
            if self.fail_at == Some(self.writes) {
                return Err(AppError::StorageUnavailable {});
            }
            if self.refuse_application_adoption
                && matches!(progress, Some(LogicalProgress::Application { .. }))
            {
                return Err(AppError::StorageUnavailable {});
            }
            let loses_ack = self.lose_receipt_ack.is_some_and(|name| {
                matches!(
                    checkpoint.steps.get(&name),
                    Some(StepState::Succeeded { .. })
                ) && !matches!(
                    self.saved.steps.get(&name),
                    Some(StepState::Succeeded { .. })
                )
            });
            let stops = self.stop_on_receipt.is_some_and(|name| {
                matches!(
                    checkpoint.steps.get(&name),
                    Some(StepState::Succeeded { .. })
                )
            });
            self.saved = checkpoint.clone();
            self.dispositions.push(disposition.clone());
            if let Some(progress) = progress {
                progress.validate()?;
                self.progress.push(progress.clone());
            }
            if let Some(result) = result {
                self.completed = Some(result.clone());
            }
            if loses_ack {
                self.lose_receipt_ack = None;
                return Err(AppError::StorageUnavailable {});
            }
            if stops {
                self.stop_on_receipt = None;
                return Err(AppError::Stopped {});
            }
            Ok(())
        }
    }

    #[test]
    fn tagged_commands_errors_and_outcomes_refuse_extra_fields() {
        for kind in ["register", "verify", "revoke", "uninstall"] {
            let valid = json!({"kind":kind});
            let command: AppCommand = serde_json::from_value(valid.clone()).unwrap();
            assert_eq!(serde_json::to_value(command).unwrap(), valid);
            assert!(
                serde_json::from_value::<AppCommand>(
                    json!({"kind":kind,"url":"https://other.example"})
                )
                .is_err()
            );
        }
        for kind in ["running", "succeeded", "recovery_required", "stopped"] {
            let valid = json!({"kind":kind});
            let outcome: Disposition = serde_json::from_value(valid.clone()).unwrap();
            assert_eq!(serde_json::to_value(outcome).unwrap(), valid);
            assert!(
                serde_json::from_value::<Disposition>(json!({"kind":kind,"retry":true})).is_err()
            );
        }
        for kind in [
            "unavailable",
            "recovery_required",
            "storage_unavailable",
            "binding_mismatch",
            "enterprise_install",
            "app_mismatch",
            "workspace_mismatch",
            "missing_required_scope",
            "continue",
            "stopped",
            "retry_exhausted",
        ] {
            let valid = json!({"kind":kind});
            let error: AppError = serde_json::from_value(valid.clone()).unwrap();
            assert_eq!(serde_json::to_value(error).unwrap(), valid);
            assert!(
                serde_json::from_value::<AppError>(
                    json!({"kind":kind,"detail":"unapproved-payload"})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn rejected_intent_checkpoint_prevents_the_provider_call() {
        let calls = Cell::new(0);
        let mut store = Store {
            fail_at: Some(1),
            ..Store::default()
        };
        let mut checkpoint = Checkpoint::default();
        let result = step(&mut store, &mut checkpoint, Step::Create, None, || {
            calls.set(calls.get() + 1);
            Ok(json!({}))
        });
        assert_eq!(result, Err(AppError::StorageUnavailable {}));
        assert_eq!(calls.get(), 0);
        assert!(checkpoint.steps.is_empty());
    }

    #[test]
    fn consuming_steps_with_unknown_outcomes_are_never_repeated_after_restart() {
        for name in [Step::Create, Step::Rotate, Step::Exchange] {
            let calls = Cell::new(0);
            let mut store = Store::default();
            let mut checkpoint = Checkpoint::default();
            assert_eq!(
                step(&mut store, &mut checkpoint, name, None, || {
                    calls.set(calls.get() + 1);
                    Err(AppError::Unavailable {})
                }),
                Err(AppError::RecoveryRequired {})
            );
            let encoded = serde_json::to_vec(&store.saved).unwrap();
            let mut restored = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(
                step(&mut store, &mut restored, name, None, || {
                    calls.set(calls.get() + 1);
                    Ok(json!({}))
                }),
                Err(AppError::RecoveryRequired {})
            );
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn created_credentials_survive_a_later_icon_failure_and_retry() {
        let mut store = Store::default();
        let mut checkpoint = Checkpoint::default();
        let creates = Cell::new(0);
        let icons = Cell::new(0);
        let receipt = json!({"app_id": "A1", "credentials": {"client_id": "1.2", "client_secret": "0123456789abcdef", "signing_secret": "abcdef0123456789"}});
        step(&mut store, &mut checkpoint, Step::Create, None, || {
            creates.set(creates.get() + 1);
            Ok(receipt.clone())
        })
        .unwrap();
        assert_eq!(
            store
                .saved
                .steps
                .get(&Step::Create)
                .map(|s| matches!(s, StepState::Succeeded { receipt: saved } if saved == &receipt)),
            Some(true)
        );
        assert_eq!(
            step(&mut store, &mut checkpoint, Step::Icon, None, || {
                icons.set(icons.get() + 1);
                Err(AppError::Unavailable {})
            }),
            Err(AppError::Unavailable {})
        );
        let mut restored = store.saved.clone();
        assert_eq!(
            step(&mut store, &mut restored, Step::Create, None, || {
                creates.set(creates.get() + 1);
                Ok(Value::Null)
            })
            .unwrap(),
            receipt
        );
        step(&mut store, &mut restored, Step::Icon, None, || {
            icons.set(icons.get() + 1);
            Ok(Value::Null)
        })
        .unwrap();
        assert_eq!(creates.get(), 1);
        assert_eq!(icons.get(), 2);
    }

    #[test]
    fn lost_receipt_acknowledgement_keeps_the_consuming_step_fenced() {
        let mut store = Store {
            fail_at: Some(2),
            ..Store::default()
        };
        let mut checkpoint = Checkpoint::default();
        let calls = Cell::new(0);
        assert_eq!(
            step(&mut store, &mut checkpoint, Step::Create, None, || {
                calls.set(calls.get() + 1);
                Ok(json!({"app_id":"A1"}))
            }),
            Err(AppError::StorageUnavailable {})
        );
        let mut restored = store.saved.clone();
        assert_eq!(
            step(&mut store, &mut restored, Step::Create, None, || {
                calls.set(calls.get() + 1);
                Ok(Value::Null)
            }),
            Err(AppError::RecoveryRequired {})
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn safe_provider_retries_are_bounded() {
        let mut store = Store::default();
        let mut checkpoint = Checkpoint::default();
        let calls = Cell::new(0);
        for _ in 0..MAX_ATTEMPTS {
            assert_eq!(
                step(&mut store, &mut checkpoint, Step::Update, None, || {
                    calls.set(calls.get() + 1);
                    Err(AppError::Unavailable {})
                }),
                Err(AppError::Unavailable {})
            );
        }
        assert_eq!(
            step(&mut store, &mut checkpoint, Step::Update, None, || {
                calls.set(calls.get() + 1);
                Ok(Value::Null)
            }),
            Err(AppError::RetryExhausted {})
        );
        assert_eq!(calls.get(), MAX_ATTEMPTS);
    }

    fn installed() -> AppCredentials {
        AppCredentials {
            app_id: "A1".into(),
            client_id: "1.2".into(),
            client_secret: "0123456789abcdef".into(),
            signing_secret: "abcdef0123456789".into(),
            bot_token: Some("xoxb-local-test".into()),
            team_id: Some("T1".into()),
            bot_user_id: Some("U1".into()),
        }
    }

    fn configuration() -> ConfigurationToken {
        ConfigurationToken {
            configuration_token: "xoxe-old-token".into(),
            refresh_token: Some("xoxe-refresh-token".into()),
        }
    }

    fn identity() -> AppIdentity {
        AppIdentity {
            name: "Employee".into(),
            display_name: "employee".into(),
            description: "Team assistance".into(),
            background_color: None,
        }
    }

    fn events_input() -> AppInput {
        AppInput {
            credentials: Some(installed()),
            configuration: Some(configuration()),
            command: AppCommand::ConfigureEvents {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: "https://core.example/webhooks/slack/apps/R1".into(),
            },
        }
    }

    #[test]
    fn expired_configuration_is_rotated_once_before_event_setup_resumes() {
        let (provider, requests, task) = super::super::test_support::script(vec![
            (
                "apps.manifest.update",
                200,
                json!({"ok":false,"error":"token_expired"}),
            ),
            (
                "tooling.tokens.rotate",
                200,
                json!({"ok":true,"token":"xoxe-new-token","refresh_token":"xoxe-new-refresh"}),
            ),
            ("apps.manifest.update", 200, json!({"ok":true})),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store::default();
        let result = execute(&provider, &events_input(), &mut checkpoint, &mut store).unwrap();
        assert_eq!(result["kind"], "events_configured");
        assert_eq!(
            result["configuration"]["configuration_token"],
            "xoxe-new-token"
        );
        assert_eq!(store.dispositions.last(), Some(&Disposition::Succeeded {}));
        assert!(
            matches!(store.progress.as_slice(),[LogicalProgress::Configuration { credentials }]
            if credentials.configuration_token=="xoxe-new-token" && credentials.refresh_token.as_deref()==Some("xoxe-new-refresh"))
        );
        task.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].contains("Bearer xoxe-new-token"));
        let mut restored = store.saved.clone();
        assert_eq!(
            execute(
                &super::super::test_support::offline(),
                &events_input(),
                &mut restored,
                &mut store
            )
            .unwrap(),
            result
        );
    }

    #[test]
    fn malformed_consuming_receipts_remain_recovery_only_across_restarts() {
        let create = AppInput {
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: None,
            },
            credentials: None,
            configuration: Some(configuration()),
        };
        let cases = [
            (
                create,
                vec![
                    ("apps.manifest.validate", 200, json!({"ok":true})),
                    (
                        "apps.manifest.create",
                        200,
                        json!({"ok":true,"app_id":"A1","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef"}}),
                    ),
                ],
            ),
            (
                events_input(),
                vec![
                    (
                        "apps.manifest.update",
                        200,
                        json!({"ok":false,"error":"token_expired"}),
                    ),
                    (
                        "tooling.tokens.rotate",
                        200,
                        json!({"ok":true,"token":"xoxe-new-token"}),
                    ),
                ],
            ),
        ];
        for (input, responses) in cases {
            let (provider, requests, task) = super::super::test_support::script(responses);
            let mut checkpoint = Checkpoint::default();
            let mut store = Store::default();
            assert_eq!(
                execute(&provider, &input, &mut checkpoint, &mut store),
                Err(AppError::RecoveryRequired {})
            );
            assert_eq!(
                store.dispositions.last(),
                Some(&Disposition::RecoveryRequired {})
            );
            task.join().unwrap();
            assert_eq!(requests.try_iter().count(), 2);
            let writes = store.writes;
            for _ in 0..10 {
                let mut restored =
                    serde_json::from_slice(&serde_json::to_vec(&store.saved).unwrap()).unwrap();
                assert_eq!(
                    execute(
                        &super::super::test_support::offline(),
                        &input,
                        &mut restored,
                        &mut store
                    ),
                    Err(AppError::RecoveryRequired {})
                );
            }
            assert_eq!(store.writes, writes);
            assert!(
                store
                    .saved
                    .steps
                    .values()
                    .any(|s| matches!(s, StepState::Succeeded { receipt } if !receipt.is_null()))
            );
        }
    }

    #[test]
    fn actual_unknown_create_result_saves_recovery_before_the_final_response() {
        let input = AppInput {
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: None,
            },
            credentials: None,
            configuration: Some(configuration()),
        };
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            ("apps.manifest.create", 503, json!({"error":"unknown"})),
        ]);
        let mut store = Store::default();
        let mut checkpoint = Checkpoint::default();
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::RecoveryRequired {})
        );
        assert_eq!(
            store.dispositions.last(),
            Some(&Disposition::RecoveryRequired {})
        );
        assert!(matches!(
            store.saved.steps.get(&Step::Create),
            Some(StepState::InFlight { .. })
        ));
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 2);
    }

    #[test]
    fn active_duplicate_preserves_the_original_consuming_receipt_sequence() {
        let mut store = Store::default();
        let mut checkpoint = Checkpoint::default();
        checkpoint
            .steps
            .insert(Step::Create, StepState::InFlight { attempt: 1 });
        checkpoint.steps.insert(
            Step::Validate,
            StepState::Succeeded {
                receipt: Value::Null,
            },
        );
        store.saved = checkpoint.clone();
        let writes = store.writes;
        let input = AppInput {
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: None,
            },
            credentials: None,
            configuration: Some(configuration()),
        };
        assert_eq!(
            execute(
                &super::super::test_support::offline(),
                &input,
                &mut checkpoint,
                &mut store
            ),
            Err(AppError::RecoveryRequired {})
        );
        assert_eq!(store.writes, writes);
        assert!(!store.saved.recovery_required);
    }

    #[test]
    fn channel_inspection_resumes_after_its_bounded_batch() {
        let channels = (1..=21).map(|n| format!("C{n}")).collect::<Vec<_>>();
        let input = AppInput {
            credentials: Some(installed()),
            configuration: None,
            command: AppCommand::InspectChannels {
                channels: channels.clone(),
            },
        };
        let replies = channels[..20].iter().map(|id| ("conversations.info",200,json!({"ok":true,"channel":{"id":id,"name":"help","created":123,"is_archived":false}}))).collect();
        let (provider, requests, task) = super::super::test_support::script(replies);
        let mut store = Store::default();
        let mut checkpoint = Checkpoint::default();
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::Continue {})
        );
        assert_eq!(checkpoint.inspected_channels.len(), 20);
        assert_eq!(
            store.dispositions.last(),
            Some(&Disposition::Retry { after_seconds: 1 })
        );
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 20);
        let (provider, requests, task) = super::super::test_support::script(vec![(
            "conversations.info",
            200,
            json!({"ok":true,"channel":{"id":"C21","name":"help"}}),
        )]);
        let mut restored =
            serde_json::from_slice(&serde_json::to_vec(&store.saved).unwrap()).unwrap();
        let result = execute(&provider, &input, &mut restored, &mut store).unwrap();
        assert_eq!(result["channels"].as_array().unwrap().len(), 21);
        task.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("channel=C21"));
    }

    #[test]
    fn icon_retry_can_rotate_an_expired_token_without_creating_again() {
        use base64::Engine as _;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let icon = AppIcon {
            content_type: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        };
        let input = AppInput {
            credentials: None,
            configuration: Some(configuration()),
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: Some(icon),
            },
        };
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            (
                "apps.manifest.create",
                200,
                json!({"ok":true,"app_id":"A1","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
            ),
            ("apps.icon.set", 503, json!({"error":"unavailable"})),
        ]);
        let mut store = Store::default();
        let mut checkpoint = Checkpoint::default();
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::Unavailable {})
        );
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 3);
        let (provider, requests, task) = super::super::test_support::script(vec![
            (
                "apps.icon.set",
                200,
                json!({"ok":false,"error":"token_expired"}),
            ),
            (
                "tooling.tokens.rotate",
                200,
                json!({"ok":true,"token":"xoxe-new-token","refresh_token":"xoxe-new-refresh"}),
            ),
            ("apps.icon.set", 200, json!({"ok":true})),
        ]);
        let mut restored =
            serde_json::from_slice(&serde_json::to_vec(&store.saved).unwrap()).unwrap();
        let result = execute(&provider, &input, &mut restored, &mut store).unwrap();
        assert_eq!(result["credentials"]["app_id"], "A1");
        assert_eq!(
            result["configuration"]["configuration_token"],
            "xoxe-new-token"
        );
        task.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].contains("Bearer xoxe-new-token"));
    }

    #[test]
    fn successful_but_incomplete_oauth_material_is_recovery_only() {
        let complete = json!({"ok":true,"app_id":"A1","access_token":"xoxb-new-token","bot_user_id":"U1",
            "team":{"id":"T1","name":"Team"},"scope":"app_mentions:read,chat:write,channels:read,reactions:write"});
        let mut receipts = Vec::new();
        for field in ["app_id", "access_token", "bot_user_id", "team"] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(field);
            receipts.push(missing);
            let mut null = complete.clone();
            null[field] = Value::Null;
            receipts.push(null);
        }
        let mut invalid_token = complete.clone();
        invalid_token["access_token"] = json!("invalid-returned-token");
        receipts.push(invalid_token);
        let mut invalid_team = complete;
        invalid_team["team"]["id"] = json!("not-a-team-id");
        receipts.push(invalid_team);
        let input = AppInput {
            credentials: Some(installed()),
            configuration: None,
            command: AppCommand::Exchange {
                code: "one-use-code".into(),
                callback_url: "https://core.example/callback".into(),
                options: AppOptions::default(),
                expected_team_id: Some("T1".into()),
            },
        };
        for receipt in receipts {
            let (provider, requests, task) =
                super::super::test_support::script(vec![("oauth.v2.access", 200, receipt.clone())]);
            let mut checkpoint = Checkpoint::default();
            let mut store = Store::default();
            assert_eq!(
                execute(&provider, &input, &mut checkpoint, &mut store),
                Err(AppError::RecoveryRequired {})
            );
            assert_eq!(
                store.dispositions.last(),
                Some(&Disposition::RecoveryRequired {})
            );
            assert!(
                matches!(store.saved.steps.get(&Step::Exchange), Some(StepState::Succeeded { receipt: saved }) if saved == &receipt)
            );
            task.join().unwrap();
            assert_eq!(requests.try_iter().count(), 1);
            let writes = store.writes;
            for _ in 0..3 {
                let mut restored =
                    serde_json::from_slice(&serde_json::to_vec(&store.saved).unwrap()).unwrap();
                assert_eq!(
                    execute(
                        &super::super::test_support::offline(),
                        &input,
                        &mut restored,
                        &mut store
                    ),
                    Err(AppError::RecoveryRequired {})
                );
            }
            assert_eq!(store.writes, writes);
        }
    }

    #[test]
    fn definitive_expired_create_refusal_rotates_without_repeating_an_unknown_effect() {
        let input = AppInput {
            credentials: None,
            configuration: Some(configuration()),
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: None,
            },
        };
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            (
                "apps.manifest.create",
                200,
                json!({"ok":false,"error":"token_expired"}),
            ),
            (
                "tooling.tokens.rotate",
                200,
                json!({"ok":true,"token":"xoxe-new-token","refresh_token":"xoxe-new-refresh"}),
            ),
            (
                "apps.manifest.create",
                200,
                json!({"ok":true,"app_id":"A1","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
            ),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store::default();
        let result = execute(&provider, &input, &mut checkpoint, &mut store).unwrap();
        assert_eq!(result["credentials"]["app_id"], "A1");
        assert_eq!(store.dispositions.last(), Some(&Disposition::Succeeded {}));
        task.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        assert!(requests[3].contains("Bearer xoxe-new-token"));
        assert!(
            matches!(store.saved.steps.get(&Step::Create), Some(StepState::Failed { error:AppError::Provider { code }, .. }) if code == "token_expired")
        );
        assert!(matches!(
            store.saved.steps.get(&Step::CreateWithRotatedToken),
            Some(StepState::Succeeded { .. })
        ));
        let mut restored = store.saved.clone();
        assert_eq!(
            execute(
                &super::super::test_support::offline(),
                &input,
                &mut restored,
                &mut store
            )
            .unwrap(),
            result
        );
    }

    #[test]
    fn saved_rotation_does_not_reopen_completed_creation_on_icon_restart() {
        use base64::Engine as _;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let icon = AppIcon {
            content_type: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        };
        let input = AppInput {
            credentials: None,
            configuration: Some(configuration()),
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: Some(icon),
            },
        };
        for lose_ack in [false, true] {
            let mut replies = vec![
                ("apps.manifest.validate", 200, json!({"ok":true})),
                (
                    "apps.manifest.create",
                    200,
                    json!({"ok":true,"app_id":"AORIGINAL","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
                ),
                (
                    "apps.icon.set",
                    200,
                    json!({"ok":false,"error":"token_expired"}),
                ),
                (
                    "tooling.tokens.rotate",
                    200,
                    json!({"ok":true,"token":"xoxe-new-token","refresh_token":"xoxe-new-refresh"}),
                ),
            ];
            if !lose_ack {
                replies.push(("apps.icon.set", 503, json!({"error":"unavailable"})));
            }
            let (provider, requests, task) = super::super::test_support::script(replies);
            let mut checkpoint = Checkpoint::default();
            let mut store = Store {
                lose_receipt_ack: lose_ack.then_some(Step::Rotate),
                ..Store::default()
            };
            assert_eq!(
                execute(&provider, &input, &mut checkpoint, &mut store),
                Err(if lose_ack {
                    AppError::StorageUnavailable {}
                } else {
                    AppError::Unavailable {}
                })
            );
            assert!(
                matches!(store.progress.first(), Some(LogicalProgress::Application { credentials }) if credentials.app_id == "AORIGINAL")
            );
            assert!(
                matches!(store.progress.last(), Some(LogicalProgress::Configuration { credentials }) if credentials.configuration_token == "xoxe-new-token")
            );
            assert!(store.saved.result.is_none());
            task.join().unwrap();
            assert_eq!(
                requests
                    .try_iter()
                    .filter(|r| r.starts_with("POST /apps.manifest.create "))
                    .count(),
                1
            );
            let (provider, requests, task) = super::super::test_support::script(vec![(
                "apps.icon.set",
                200,
                json!({"ok":true}),
            )]);
            let mut restored =
                serde_json::from_slice(&serde_json::to_vec(&store.saved).unwrap()).unwrap();
            let result = execute(&provider, &input, &mut restored, &mut store).unwrap();
            task.join().unwrap();
            assert_eq!(requests.try_iter().count(), 1);
            assert_eq!(result["credentials"]["app_id"], "AORIGINAL");
            assert_eq!(result["credentials"]["client_secret"], "0123456789abcdef");
            assert!(!restored.steps.contains_key(&Step::CreateWithRotatedToken));
        }
    }

    #[test]
    fn stored_stop_keeps_returned_credentials_and_prevents_icon_or_failure_rewrites() {
        use base64::Engine as _;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let icon = AppIcon {
            content_type: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        };
        let input = AppInput {
            credentials: None,
            configuration: Some(configuration()),
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: Some(icon),
            },
        };
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            (
                "apps.manifest.create",
                200,
                json!({"ok":true,"app_id":"ASTORED","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
            ),
        ]);
        let mut store = Store {
            stop_on_receipt: Some(Step::Create),
            ..Store::default()
        };
        let mut checkpoint = Checkpoint::default();
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::Stopped {})
        );
        assert!(
            matches!(store.saved.steps.get(&Step::Create), Some(StepState::Succeeded { receipt }) if receipt["app_id"] == "ASTORED")
        );
        assert_eq!(store.dispositions.last(), Some(&Disposition::Running {}));
        assert!(
            !store
                .dispositions
                .iter()
                .any(|d| matches!(d, Disposition::Refused { .. }))
        );
        assert!(store.saved.result.is_none());
        assert!(
            matches!(store.progress.as_slice(), [LogicalProgress::Application { credentials }] if credentials.app_id == "ASTORED")
        );
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 2);
    }

    #[test]
    fn uninstall_uses_token_revocation_only_after_a_definitive_provider_refusal() {
        let input = AppInput {
            credentials: Some(installed()),
            configuration: None,
            command: AppCommand::Uninstall {},
        };
        let (provider, requests, task) = super::super::test_support::script(vec![
            (
                "apps.uninstall",
                200,
                json!({"ok":false,"error":"no_permission"}),
            ),
            ("auth.revoke", 200, json!({"ok":true})),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store::default();
        let result = execute(&provider, &input, &mut checkpoint, &mut store).unwrap();
        assert_eq!(result["kind"], "credentials_revoked");
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 2);
        let (provider, requests, task) = super::super::test_support::script(vec![(
            "apps.uninstall",
            503,
            json!({"error":"unknown"}),
        )]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store::default();
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::Unavailable {})
        );
        assert_eq!(
            store.dispositions.last(),
            Some(&Disposition::Retry { after_seconds: 2 })
        );
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 1);
    }

    #[test]
    fn exhausted_unknown_uninstall_results_do_not_authorize_token_revocation_after_restart() {
        let mut store = Store::default();
        let mut checkpoint = Checkpoint::default();
        for _ in 0..MAX_ATTEMPTS {
            assert_eq!(
                step(&mut store, &mut checkpoint, Step::Uninstall, None, || Err(
                    AppError::Unavailable {}
                )),
                Err(AppError::Unavailable {})
            );
        }
        assert_eq!(store.dispositions.last(), Some(&Disposition::Running {}));
        let mut restored =
            serde_json::from_slice(&serde_json::to_vec(&store.saved).unwrap()).unwrap();
        let (provider, requests, task) = super::super::test_support::script(Vec::new());
        let input = AppInput {
            credentials: Some(installed()),
            configuration: None,
            command: AppCommand::Uninstall {},
        };
        assert_eq!(
            execute(&provider, &input, &mut restored, &mut store),
            Err(AppError::RetryExhausted {})
        );
        assert_eq!(
            store.dispositions.last(),
            Some(&Disposition::Refused {
                error: AppError::RetryExhausted {}
            })
        );
        assert!(store.saved.result.is_none());
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 0);
    }

    #[test]
    fn lost_application_ack_resumes_only_the_unfinished_icon() {
        use base64::Engine as _;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let input = AppInput {
            credentials: None,
            configuration: Some(configuration()),
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: Some(AppIcon {
                    content_type: "image/png".into(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
                }),
            },
        };
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            (
                "apps.manifest.create",
                200,
                json!({"ok":true,"app_id":"ALOSTACK","credentials":{
                "client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
            ),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store {
            lose_receipt_ack: Some(Step::Create),
            ..Store::default()
        };
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::StorageUnavailable {})
        );
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 2);
        assert!(store.saved.result.is_none());
        assert!(
            matches!(store.progress.as_slice(), [LogicalProgress::Application { credentials }] if credentials.app_id == "ALOSTACK")
        );
        let bytes = serde_json::to_vec(&store.saved).unwrap();
        let mut restored = serde_json::from_slice(&bytes).unwrap();
        let (provider, requests, task) =
            super::super::test_support::script(vec![("apps.icon.set", 200, json!({"ok":true}))]);
        let result = execute(&provider, &input, &mut restored, &mut store).unwrap();
        task.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /apps.icon.set HTTP/1.1\r\n"));
        assert_eq!(result["kind"], "application");
        assert_eq!(result["credentials"]["app_id"], "ALOSTACK");
        assert_eq!(store.progress.len(), 1);
        assert!(matches!(
            store.completed,
            Some(OperationResult::Application { .. })
        ));
    }

    #[test]
    fn application_adoption_must_be_acknowledged_before_icon_or_success() {
        use base64::Engine as _;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let icon = AppIcon {
            content_type: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        };
        let input = AppInput {
            credentials: None,
            configuration: Some(configuration()),
            command: AppCommand::Apply {
                identity: identity(),
                options: AppOptions::default(),
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: Some(icon),
            },
        };
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            (
                "apps.manifest.create",
                200,
                json!({"ok":true,"app_id":"AADOPT","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
            ),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store {
            refuse_application_adoption: true,
            ..Store::default()
        };
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::StorageUnavailable {})
        );
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 2);
        assert!(store.progress.is_empty());
        assert!(store.saved.result.is_none());
        assert!(matches!(
            store.saved.steps.get(&Step::Create),
            Some(StepState::InFlight { .. })
        ));
        let mut restored = store.saved.clone();
        assert_eq!(
            execute(
                &super::super::test_support::offline(),
                &input,
                &mut restored,
                &mut store
            ),
            Err(AppError::RecoveryRequired {})
        );
    }

    fn existing_apply_input() -> AppInput {
        use base64::Engine as _;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let mut desired = identity();
        desired.description = "Updated team assistance".into();
        AppInput {
            credentials: Some(installed()),
            configuration: Some(configuration()),
            command: AppCommand::Apply {
                identity: desired,
                options: AppOptions {
                    direct_messages: true,
                    private_channels: false,
                },
                callback_url: "https://core.example/callback".into(),
                events_url: None,
                icon: Some(AppIcon {
                    content_type: "image/png".into(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
                }),
            },
        }
    }

    fn expected_existing_application() -> LogicalProgress {
        LogicalProgress::Application {
            credentials: AppCredentials {
                app_id: "A1".into(),
                client_id: "1.2".into(),
                client_secret: "0123456789abcdef".into(),
                signing_secret: "abcdef0123456789".into(),
                bot_token: None,
                team_id: None,
                bot_user_id: None,
            },
        }
    }

    fn assert_existing_update_requests(requests: Vec<String>, icon_called: bool) {
        let paths: &[&str] = if icon_called {
            &[
                "apps.manifest.validate",
                "apps.manifest.update",
                "apps.icon.set",
            ]
        } else {
            &["apps.manifest.validate", "apps.manifest.update"]
        };
        assert_eq!(requests.len(), paths.len());
        for (request, path) in requests.iter().zip(paths) {
            assert!(request.starts_with(&format!("POST /{path} HTTP/1.1\r\n")));
        }
        let body = requests[1].split_once("\r\n\r\n").unwrap().1;
        let form = url::form_urlencoded::parse(body.as_bytes()).collect::<BTreeMap<_, _>>();
        assert_eq!(form.get("app_id").map(|value| value.as_ref()), Some("A1"));
    }

    #[test]
    fn existing_application_update_is_adopted_before_a_later_icon_failure() {
        let input = existing_apply_input();
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            ("apps.manifest.update", 200, json!({"ok":true})),
            ("apps.icon.set", 503, json!({"error":"unavailable"})),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store::default();
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::Unavailable {})
        );
        task.join().unwrap();
        assert_existing_update_requests(requests.try_iter().collect(), true);
        assert_eq!(store.progress, vec![expected_existing_application()]);
        assert!(matches!(
            store.saved.steps.get(&Step::Update),
            Some(StepState::Succeeded { .. })
        ));
        assert!(store.saved.result.is_none());
        assert!(store.completed.is_none());
    }

    #[test]
    fn stored_stop_at_existing_update_adopts_the_application_without_icon_or_success() {
        let input = existing_apply_input();
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            ("apps.manifest.update", 200, json!({"ok":true})),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store {
            stop_on_receipt: Some(Step::Update),
            ..Store::default()
        };
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::Stopped {})
        );
        task.join().unwrap();
        assert_existing_update_requests(requests.try_iter().collect(), false);
        assert_eq!(store.progress, vec![expected_existing_application()]);
        assert!(matches!(
            store.saved.steps.get(&Step::Update),
            Some(StepState::Succeeded { .. })
        ));
        assert!(!store.saved.steps.contains_key(&Step::Icon));
        assert!(store.saved.result.is_none());
        assert_eq!(store.dispositions.last(), Some(&Disposition::Running {}));
    }

    #[test]
    fn lost_existing_update_ack_resumes_only_icon_and_preserves_installed_credentials() {
        let input = existing_apply_input();
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            ("apps.manifest.update", 200, json!({"ok":true})),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store {
            lose_receipt_ack: Some(Step::Update),
            ..Store::default()
        };
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::StorageUnavailable {})
        );
        task.join().unwrap();
        assert_existing_update_requests(requests.try_iter().collect(), false);
        assert_eq!(store.progress, vec![expected_existing_application()]);
        assert!(store.saved.result.is_none());
        let mut restored =
            serde_json::from_slice(&serde_json::to_vec(&store.saved).unwrap()).unwrap();
        let (provider, requests, task) =
            super::super::test_support::script(vec![("apps.icon.set", 200, json!({"ok":true}))]);
        let result = execute(&provider, &input, &mut restored, &mut store).unwrap();
        task.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /apps.icon.set HTTP/1.1\r\n"));
        assert_eq!(
            result["credentials"],
            serde_json::to_value(installed()).unwrap()
        );
        assert_eq!(store.progress, vec![expected_existing_application()]);
        assert!(matches!(
            store.completed,
            Some(OperationResult::Application { .. })
        ));
    }

    #[test]
    fn rotated_existing_update_adopts_configuration_then_application_before_stop() {
        let input = existing_apply_input();
        let (provider, requests, task) = super::super::test_support::script(vec![
            ("apps.manifest.validate", 200, json!({"ok":true})),
            (
                "apps.manifest.update",
                200,
                json!({"ok":false,"error":"token_expired"}),
            ),
            (
                "tooling.tokens.rotate",
                200,
                json!({"ok":true,"token":"xoxe-new-token","refresh_token":"xoxe-new-refresh"}),
            ),
            ("apps.manifest.update", 200, json!({"ok":true})),
        ]);
        let mut checkpoint = Checkpoint::default();
        let mut store = Store {
            stop_on_receipt: Some(Step::UpdateWithRotatedToken),
            ..Store::default()
        };
        assert_eq!(
            execute(&provider, &input, &mut checkpoint, &mut store),
            Err(AppError::Stopped {})
        );
        task.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        for (request, path) in requests.iter().zip([
            "apps.manifest.validate",
            "apps.manifest.update",
            "tooling.tokens.rotate",
            "apps.manifest.update",
        ]) {
            assert!(request.starts_with(&format!("POST /{path} HTTP/1.1\r\n")));
        }
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer xoxe-old-token")
        );
        assert!(
            requests[3]
                .to_ascii_lowercase()
                .contains("authorization: bearer xoxe-new-token")
        );
        assert!(
            matches!(store.progress.first(),Some(LogicalProgress::Configuration { credentials })
            if credentials.configuration_token=="xoxe-new-token" && credentials.refresh_token.as_deref()==Some("xoxe-new-refresh"))
        );
        assert_eq!(store.progress.len(), 2);
        assert_eq!(store.progress[1], expected_existing_application());
        assert!(matches!(
            store.saved.steps.get(&Step::Update),
            Some(StepState::Failed { .. })
        ));
        assert!(matches!(
            store.saved.steps.get(&Step::UpdateWithRotatedToken),
            Some(StepState::Succeeded { .. })
        ));
        assert!(!store.saved.steps.contains_key(&Step::Create));
        assert!(!store.saved.steps.contains_key(&Step::Icon));
        assert!(store.saved.result.is_none());
    }
}
