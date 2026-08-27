use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use dispatch_channel_protocol::{
    ChannelEventNotification, PluginNotificationEnvelope, notification_to_jsonrpc,
};
use dispatch_channel_runtime::write_stdout_line;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead},
    net::TcpStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket,
    protocol::{CloseFrame, frame::coding::CloseCode},
    stream::MaybeTlsStream,
};

mod discord_api;
mod protocol;

use discord_api::{DiscordChannelInfo, DiscordClient, DiscordUpload};
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ChannelPolicy, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActivation, InboundActor, InboundAttachment,
    InboundConversationRef, InboundEventEnvelope, InboundMessage, IngressCallbackReply,
    IngressMode, IngressPayload, IngressState, OutboundMessage, PluginRequest,
    PluginRequestEnvelope, PluginResponse, StatusAcceptance, StatusFrame, StatusKind, capabilities,
    parse_jsonrpc_request, plugin_error, response_to_jsonrpc,
};

const META_REASON: &str = "reason";
const META_PLATFORM: &str = "platform";
const META_BOT_TOKEN_ENV: &str = "bot_token_env";
const META_APPLICATION_ID: &str = "application_id";
const META_DEFAULT_CHANNEL_ID: &str = "default_channel_id";
const META_ALLOWED_GUILD_COUNT: &str = "allowed_guild_count";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_INTERACTION_ENDPOINT: &str = "interaction_endpoint";
const META_INTERACTION_PUBLIC_KEY_ENV: &str = "interaction_public_key_env";
const META_VERIFICATION_KEY_ENV: &str = "verification_key_env";
const META_HOST_ACTION: &str = "host_action";
const META_CHANNEL_ID: &str = "channel_id";
const META_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const META_THREAD_ID: &str = "thread_id";
const META_TRANSPORT: &str = "transport";
const META_ENDPOINT_ID: &str = "endpoint_id";
const META_PATH: &str = "path";
const META_INTERACTION_TYPE: &str = "interaction_type";
const META_GUILD_ID: &str = "guild_id";
const META_COMMAND_NAME: &str = "command_name";
const META_COMMAND_KIND: &str = "command_kind";
const META_CUSTOM_ID: &str = "custom_id";
const META_COMPONENT_TYPE: &str = "component_type";
const META_SOURCE_MESSAGE_ID: &str = "source_message_id";
const META_ATTACHMENT_COUNT: &str = "attachment_count";
const META_LOCALE: &str = "locale";
const META_GUILD_LOCALE: &str = "guild_locale";
const META_ACTOR_KIND: &str = "actor_kind";
const META_NICK: &str = "nick";
const META_RESOLVED_DESTINATION: &str = "resolved_destination";
const META_STATUS_KIND: &str = "status_kind";
const META_REASON_CODE: &str = "reason_code";
const META_ALLOWED_CHANNEL_COUNT: &str = "allowed_channel_count";
const META_ALLOWED_THREAD_COUNT: &str = "allowed_thread_count";
const META_ALLOWED_DM_SENDER_COUNT: &str = "allowed_dm_sender_count";
const META_ALLOWED_SENDER_COUNT: &str = "allowed_sender_count";
const META_OUTBOUND_CHANNEL_COUNT: &str = "outbound_channel_count";
const META_ACTIVATION: &str = "activation";
const META_ACTIVATION_REASON: &str = "activation_reason";
const META_THREAD_POLICY: &str = "thread_policy";
const META_DM_POLICY: &str = "dm_policy";
const META_REPLY_DELIVERY: &str = "reply_delivery";
const META_CHANNEL_WILDCARD: &str = "channel_wildcard";
const META_BOT_USER_ID: &str = "bot_user_id";
const META_PARENT_CHANNEL_ID: &str = "parent_channel_id";

const PLATFORM_DISCORD: &str = "discord";
const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const ROUTE_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const TRANSPORT_INTERACTION_WEBHOOK: &str = "interaction_webhook";
const TRANSPORT_WEBSOCKET: &str = "websocket";
const DISCORD_STATUS_TEXT: &str = "discord_status_text";
const DISCORD_GATEWAY_BASE_URL: &str = "wss://gateway.discord.gg";

const HEADER_X_SIGNATURE_ED25519: &str = "X-Signature-Ed25519";
const HEADER_X_SIGNATURE_TIMESTAMP: &str = "X-Signature-Timestamp";

/// Accept Discord interaction signatures whose timestamp is within this many
/// seconds of the host clock. The ED25519 signature covers the timestamp, but
/// the timestamp itself is not bounded by the signature - without a freshness
/// window, an attacker who captured a valid interaction could replay it
/// indefinitely. Discord's own documentation recommends a small window.
const DISCORD_MAX_SIGNATURE_AGE_SECS: i64 = 300;

const INTERACTION_TYPE_PING: u8 = 1;
const INTERACTION_TYPE_APPLICATION_COMMAND: u8 = 2;
const INTERACTION_TYPE_MESSAGE_COMPONENT: u8 = 3;
const INTERACTION_TYPE_APPLICATION_COMMAND_AUTOCOMPLETE: u8 = 4;
const INTERACTION_TYPE_MODAL_SUBMIT: u8 = 5;

const COMMAND_KIND_CHAT_INPUT: u8 = 1;
const COMMAND_KIND_USER: u8 = 2;
const COMMAND_KIND_MESSAGE: u8 = 3;

const COMPONENT_KIND_BUTTON: u8 = 2;
const COMPONENT_KIND_STRING_SELECT: u8 = 3;
const COMPONENT_KIND_TEXT_INPUT: u8 = 4;
const COMPONENT_KIND_USER_SELECT: u8 = 5;
const COMPONENT_KIND_ROLE_SELECT: u8 = 6;
const COMPONENT_KIND_MENTIONABLE_SELECT: u8 = 7;
const COMPONENT_KIND_CHANNEL_SELECT: u8 = 8;

// -----------------------------------------------------------------------------
// Binding policy vocabulary
// -----------------------------------------------------------------------------

const ACTIVATION_MENTION_OR_REPLY: &str = "mention_or_reply";
const ACTIVATION_SLASH_COMMAND: &str = "slash_command";
const ACTIVATION_ALL_MESSAGES: &str = "all_messages";

const THREAD_POLICY_DENY: &str = "deny";
const THREAD_POLICY_INHERIT_PARENT: &str = "inherit_parent";
const THREAD_POLICY_ALLOWLIST: &str = "allowlist";

const DM_POLICY_DENY: &str = "deny";
const DM_POLICY_ALLOWLIST: &str = "allowlist";
const DM_POLICY_OPEN: &str = "open";

const REPLY_DELIVERY_RUNTIME_OWNED: &str = "runtime_owned";
const REPLY_DELIVERY_TOOL_OWNED: &str = "tool_owned";

/// The only wildcard this plugin honours, and only inside `allowed_channel_ids`.
const CHANNEL_WILDCARD: &str = "*";

const CONVERSATION_KIND_CHANNEL: &str = "channel";
const CONVERSATION_KIND_THREAD: &str = "thread";
const CONVERSATION_KIND_DM: &str = "dm";

// Stable rejection codes. A host records these without storing message content.
const REJECT_BOT_EVENT: &str = "bot_event";
const REJECT_UNSUPPORTED_MESSAGE_TYPE: &str = "unsupported_message_type";
const REJECT_UNAUTHORIZED_WORKSPACE: &str = "unauthorized_workspace";
const REJECT_UNAUTHORIZED_CHANNEL: &str = "unauthorized_channel";
const REJECT_UNAUTHORIZED_THREAD: &str = "unauthorized_thread";
const REJECT_UNRESOLVED_CONVERSATION: &str = "unresolved_conversation";
const REJECT_DM_DENIED: &str = "dm_denied";
const REJECT_SENDER_DENIED: &str = "sender_denied";
const REJECT_NOT_ADDRESSED: &str = "not_addressed";
const REJECT_EMPTY_MESSAGE: &str = "empty_message";
const REJECT_APPLICATION_MISMATCH: &str = "application_mismatch";
const REJECT_INVALID_POLICY: &str = "invalid_policy";
const REJECT_MISSING_DESTINATION: &str = "missing_destination";
const REJECT_UNAUTHORIZED_DESTINATION: &str = "unauthorized_destination";

// Discord channel type discriminants used to separate channels from threads.
const CHANNEL_TYPE_ANNOUNCEMENT_THREAD: u8 = 10;
const CHANNEL_TYPE_PUBLIC_THREAD: u8 = 11;
const CHANNEL_TYPE_PRIVATE_THREAD: u8 = 12;
const CHANNEL_TYPE_DM: u8 = 1;

// Discord message type discriminants. Only ordinary and reply messages carry a
// user request; the rest are provider system notices about the channel itself.
const MESSAGE_TYPE_DEFAULT: u8 = 0;
const MESSAGE_TYPE_REPLY: u8 = 19;

/// Interaction responses that Discord shows to a single member. They are not
/// inbound requests to this binding.
const MESSAGE_FLAG_EPHEMERAL: u64 = 1 << 6;

/// `message_reference.type` for a reply. Other values, such as a forward, point
/// at a message that was not addressed to the referenced author.
const MESSAGE_REFERENCE_TYPE_DEFAULT: u8 = 0;

/// Bound on how long a resolved channel shape is reused. Thread parentage is
/// stable in practice, but a bounded lifetime keeps a stale answer from
/// authorizing a conversation after the provider moved it.
const DISCORD_CHANNEL_CACHE_TTL: Duration = Duration::from_secs(300);
/// Bound on cached channels so an unbounded stream of conversation IDs cannot
/// grow the process footprint without limit.
const DISCORD_CHANNEL_CACHE_CAPACITY: usize = 256;

const DISCORD_GATEWAY_VERSION: u8 = 10;
const DISCORD_GATEWAY_BASE_INTENTS: u64 = 1 | (1 << 9) | (1 << 12);
const DISCORD_GATEWAY_MESSAGE_CONTENT_INTENT: u64 = 1 << 15;
const DISCORD_GATEWAY_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Minimum healthy session duration required to clear reconnect history.
const DISCORD_SESSION_STABILITY_WINDOW: Duration = Duration::from_secs(60);
const DISCORD_MAX_CONSECUTIVE_FAILURES: u32 = 5;

fn main() -> Result<()> {
    let stdin = io::stdin().lock();
    let stdout_lock = Arc::new(Mutex::new(()));
    let mut ingress_worker: Option<DiscordIngressWorker> = None;

    for line in stdin.lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let (request_id, envelope) = parse_jsonrpc_request(&line)
            .map_err(|error| anyhow!("failed to parse channel request: {error}"))?;
        let should_exit = matches!(envelope.request, PluginRequest::Shutdown);

        let response = match handle_request(&envelope, &stdout_lock, &mut ingress_worker) {
            Ok(response) => response,
            Err(error) => plugin_error("internal_error", error.to_string()),
        };

        let json = response_to_jsonrpc(&request_id, &response).map_err(|error| anyhow!(error))?;
        write_stdout_line(&stdout_lock, &json)?;
        if should_exit {
            break;
        }
    }

    let _ = stop_ingress_worker(&mut ingress_worker);
    Ok(())
}

fn handle_request(
    envelope: &PluginRequestEnvelope,
    stdout_lock: &Arc<Mutex<()>>,
    ingress_worker: &mut Option<DiscordIngressWorker>,
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
            capabilities: capabilities(),
        }),
        PluginRequest::Configure { config } => Ok(PluginResponse::Configured {
            configuration: Box::new(configure(config)?),
        }),
        PluginRequest::Health { config } => Ok(PluginResponse::Health {
            health: health(config)?,
        }),
        PluginRequest::PollIngress { config, state } => {
            handle_websocket_receive(config, state.as_ref())
        }
        PluginRequest::StartIngress { config, state } => {
            let started = start_ingress(config)?;
            let started = match (&started.mode, state.clone()) {
                (IngressMode::Websocket, Some(state)) if state.mode == IngressMode::Websocket => {
                    state
                }
                _ => started,
            };
            if matches!(started.mode, IngressMode::Websocket) {
                restart_ingress_worker(
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
            state: stop_ingress(
                config,
                stop_ingress_worker(ingress_worker).or(state.clone()),
            )?,
        }),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::GetMessage { .. } | PluginRequest::GetPermalink { .. } => Ok(plugin_error(
            "unsupported_request",
            "discord does not support message read-back",
        )),
        PluginRequest::IngressEvent {
            config, payload, ..
        } => handle_ingress_event(config, payload),
        PluginRequest::Status { config, update } => Ok(PluginResponse::StatusAccepted {
            status: send_status(config, update)?,
        }),
        PluginRequest::Shutdown => {
            let _ = stop_ingress_worker(ingress_worker);
            Ok(PluginResponse::Ok)
        }
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let bot_token_env = bot_token_env(config);
    read_required_env(bot_token_env)?;
    let policy = DiscordPolicy::from_config(config)?;
    policy.validate_for_ingress(uses_interaction_webhook(config))?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_BOT_TOKEN_ENV.to_string(), bot_token_env.to_string());
    if let Some(application_id) = &config.application_id {
        metadata.insert(META_APPLICATION_ID.to_string(), application_id.clone());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        metadata.insert(
            META_DEFAULT_CHANNEL_ID.to_string(),
            default_channel_id.clone(),
        );
    }
    metadata.extend(policy.diagnostics());
    if let Some(endpoint) = resolved_endpoint(config) {
        let public_key_env = interaction_public_key_env(config);
        read_required_env(public_key_env)?;
        metadata.insert(
            META_INGRESS_MODE.to_string(),
            ingress_mode_name(IngressMode::InteractionWebhook),
        );
        metadata.insert(META_INTERACTION_ENDPOINT.to_string(), endpoint);
        metadata.insert(
            META_INTERACTION_PUBLIC_KEY_ENV.to_string(),
            public_key_env.to_string(),
        );
    }

    Ok(ConfiguredChannel {
        metadata,
        policy: Some(policy.to_channel_policy(config)),
        runtime: None,
    })
}

// -----------------------------------------------------------------------------
// Normalized binding policy
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationMode {
    MentionOrReply,
    SlashCommand,
    AllMessages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadPolicy {
    Deny,
    InheritParent,
    Allowlist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmPolicy {
    Deny,
    Allowlist,
    /// Any sender may direct-message this binding. A direct message is
    /// addressed to the bot account by construction, so the sender is the only
    /// open question, but the mode is still unbounded ingress: it is never
    /// inferred from an absent field and carries no sender allowlist.
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyDelivery {
    RuntimeOwned,
    ToolOwned,
}

/// The binding's authorized communication surface, normalized once so every
/// ingress and egress path decides from the same values.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscordPolicy {
    allowed_guild_ids: Vec<String>,
    allowed_channel_ids: Vec<String>,
    channel_wildcard: bool,
    thread_policy: ThreadPolicy,
    allowed_thread_ids: Vec<String>,
    activation: ActivationMode,
    owner_id: Option<String>,
    allowed_sender_ids: Vec<String>,
    dm_policy: DmPolicy,
    allowed_dm_sender_ids: Vec<String>,
    outbound_channel_ids: Vec<String>,
    outbound_wildcard: bool,
    reply_delivery: ReplyDelivery,
    application_id: Option<String>,
}

impl DiscordPolicy {
    /// Normalize the declared configuration.
    ///
    /// An unrecognized mode is a configuration error rather than a default,
    /// because silently picking a mode for an operator who asked for another one
    /// grants a surface nobody authorized.
    fn from_config(config: &ChannelConfig) -> Result<Self> {
        let activation = match config.activation.as_deref() {
            None | Some(ACTIVATION_MENTION_OR_REPLY) => ActivationMode::MentionOrReply,
            Some(ACTIVATION_SLASH_COMMAND) => ActivationMode::SlashCommand,
            Some(ACTIVATION_ALL_MESSAGES) => ActivationMode::AllMessages,
            Some(other) => bail!(
                "unknown discord activation `{other}`; expected `{ACTIVATION_MENTION_OR_REPLY}`, `{ACTIVATION_SLASH_COMMAND}`, or `{ACTIVATION_ALL_MESSAGES}`"
            ),
        };
        let thread_policy = match config.thread_policy.as_deref() {
            None | Some(THREAD_POLICY_DENY) => ThreadPolicy::Deny,
            Some(THREAD_POLICY_INHERIT_PARENT) => ThreadPolicy::InheritParent,
            Some(THREAD_POLICY_ALLOWLIST) => ThreadPolicy::Allowlist,
            Some(other) => bail!(
                "unknown discord thread_policy `{other}`; expected `{THREAD_POLICY_DENY}`, `{THREAD_POLICY_INHERIT_PARENT}`, or `{THREAD_POLICY_ALLOWLIST}`"
            ),
        };
        let dm_policy = match config.dm_policy.as_deref() {
            None | Some(DM_POLICY_DENY) => DmPolicy::Deny,
            Some(DM_POLICY_ALLOWLIST) => DmPolicy::Allowlist,
            Some(DM_POLICY_OPEN) => DmPolicy::Open,
            Some(other) => bail!(
                "unknown discord dm_policy `{other}`; expected `{DM_POLICY_DENY}`, `{DM_POLICY_ALLOWLIST}`, or `{DM_POLICY_OPEN}`"
            ),
        };
        let reply_delivery = match config.reply_delivery.as_deref() {
            None | Some(REPLY_DELIVERY_RUNTIME_OWNED) => ReplyDelivery::RuntimeOwned,
            Some(REPLY_DELIVERY_TOOL_OWNED) => ReplyDelivery::ToolOwned,
            Some(other) => bail!(
                "unknown discord reply_delivery `{other}`; expected `{REPLY_DELIVERY_RUNTIME_OWNED}` or `{REPLY_DELIVERY_TOOL_OWNED}`"
            ),
        };

        let channel_wildcard = config
            .allowed_channel_ids
            .iter()
            .any(|entry| entry == CHANNEL_WILDCARD);
        let allowed_channel_ids: Vec<String> = config
            .allowed_channel_ids
            .iter()
            .filter(|entry| *entry != CHANNEL_WILDCARD)
            .cloned()
            .collect();

        // Outbound falls back to the inbound channel set so an unset outbound
        // list can never reach a destination inbound scope excludes. The only
        // accepted wildcard is the inbound `*`; an explicit outbound `*` is
        // allowed only when inbound already is that wildcard.
        let (outbound_channel_ids, outbound_wildcard) = if config.outbound_channel_ids.is_empty() {
            (allowed_channel_ids.clone(), channel_wildcard)
        } else {
            let outbound_wildcard = config
                .outbound_channel_ids
                .iter()
                .any(|entry| entry == CHANNEL_WILDCARD);
            let outbound_channel_ids: Vec<String> = config
                .outbound_channel_ids
                .iter()
                .filter(|entry| *entry != CHANNEL_WILDCARD)
                .cloned()
                .collect();
            (outbound_channel_ids, outbound_wildcard)
        };

        Ok(Self {
            allowed_guild_ids: config.allowed_guild_ids.clone(),
            allowed_channel_ids,
            channel_wildcard,
            thread_policy,
            allowed_thread_ids: config.allowed_thread_ids.clone(),
            activation,
            owner_id: config.owner_id.clone(),
            allowed_sender_ids: config.allowed_sender_ids.clone(),
            dm_policy,
            allowed_dm_sender_ids: config.allowed_dm_sender_ids.clone(),
            outbound_channel_ids,
            outbound_wildcard,
            reply_delivery,
            application_id: config.application_id.clone(),
        })
    }

    /// Reject a binding that cannot state the surface it is authorized for.
    ///
    /// Persistent gateway ingress observes every guild message the bot account
    /// can see, so a binding without guild and channel scope would inherit the
    /// bot account's whole visibility as its routing policy.
    fn validate_for_ingress(&self, interaction_webhook: bool) -> Result<()> {
        let has_guild_ids = !self.allowed_guild_ids.is_empty();
        let has_channel_ids = self.channel_wildcard || !self.allowed_channel_ids.is_empty();
        if has_guild_ids != has_channel_ids {
            bail!(
                "discord guild ingress requires both allowed_guild_ids and allowed_channel_ids; omit both to deny every guild"
            );
        }
        let guild_surface = has_guild_ids && has_channel_ids;
        let dm_surface = self.dm_policy != DmPolicy::Deny;
        if !guild_surface && !dm_surface {
            bail!("discord persistent ingress has no authorized guild or direct-message surface");
        }
        if self
            .allowed_guild_ids
            .iter()
            .any(|guild_id| guild_id == CHANNEL_WILDCARD)
        {
            bail!(
                "discord allowed_guild_ids does not accept a wildcard guild; list every authorized guild id"
            );
        }
        if self.channel_wildcard && !self.allowed_channel_ids.is_empty() {
            bail!(
                "discord allowed_channel_ids must be either the explicit `{CHANNEL_WILDCARD}` wildcard on its own or a list of channel ids"
            );
        }
        if self.activation == ActivationMode::AllMessages && self.channel_wildcard {
            bail!(
                "discord activation `{ACTIVATION_ALL_MESSAGES}` requires explicitly listed allowed_channel_ids and rejects the `{CHANNEL_WILDCARD}` wildcard"
            );
        }
        if self.outbound_wildcard && !self.outbound_channel_ids.is_empty() {
            bail!(
                "discord outbound_channel_ids must be either the explicit `{CHANNEL_WILDCARD}` wildcard on its own or a list of channel ids"
            );
        }
        if self.outbound_wildcard && !self.channel_wildcard {
            bail!(
                "discord outbound_channel_ids does not accept a wildcard unless allowed_channel_ids is the channel wildcard"
            );
        }
        if !guild_surface && !self.outbound_channel_ids.is_empty() {
            bail!("discord outbound_channel_ids requires an authorized guild channel surface");
        }
        // A thread allowlist enumerates threads and has no wildcard form.
        // `thread_policy` `inherit_parent` is the written-down mode for every
        // thread beneath an authorized channel, and it resolves a thread through
        // that parent. A wildcard entry here would instead authorize any thread
        // anywhere in an authorized guild regardless of the channel it descends
        // from, bypassing `allowed_channel_ids`.
        if self
            .allowed_thread_ids
            .iter()
            .any(|thread_id| thread_id == CHANNEL_WILDCARD)
        {
            bail!(
                "discord allowed_thread_ids does not accept a wildcard thread; list every authorized thread id, or authorize every thread beneath an allowed channel with thread_policy `{THREAD_POLICY_INHERIT_PARENT}`"
            );
        }
        if self.thread_policy == ThreadPolicy::Allowlist && self.allowed_thread_ids.is_empty() {
            bail!(
                "discord thread_policy `{THREAD_POLICY_ALLOWLIST}` requires at least one allowed_thread_ids entry"
            );
        }
        if self.thread_policy != ThreadPolicy::Allowlist && !self.allowed_thread_ids.is_empty() {
            bail!(
                "discord allowed_thread_ids is only meaningful with thread_policy `{THREAD_POLICY_ALLOWLIST}`"
            );
        }
        if !guild_surface && self.thread_policy != ThreadPolicy::Deny {
            bail!("discord thread_policy requires an authorized guild channel surface");
        }
        // A sender allowlist enumerates senders and has no wildcard form.
        // `dm_policy` `open` is the written-down mode for unbounded
        // direct-message ingress, so a wildcard entry here would be a second and
        // inferred way to state it.
        if self
            .allowed_dm_sender_ids
            .iter()
            .any(|sender_id| sender_id == CHANNEL_WILDCARD)
        {
            bail!(
                "discord allowed_dm_sender_ids does not accept a wildcard sender; list every authorized sender id, or state unbounded direct-message ingress with dm_policy `{DM_POLICY_OPEN}`"
            );
        }
        if self.dm_policy == DmPolicy::Allowlist && self.allowed_dm_sender_ids.is_empty() {
            bail!(
                "discord dm_policy `{DM_POLICY_ALLOWLIST}` requires at least one allowed_dm_sender_ids entry"
            );
        }
        if self.dm_policy != DmPolicy::Allowlist && !self.allowed_dm_sender_ids.is_empty() {
            bail!(
                "discord allowed_dm_sender_ids is only meaningful with dm_policy `{DM_POLICY_ALLOWLIST}`; `{DM_POLICY_DENY}` and `{DM_POLICY_OPEN}` require it to be empty"
            );
        }
        if !self.channel_wildcard && !self.allowed_channel_ids.is_empty() {
            for channel_id in &self.outbound_channel_ids {
                if !self.channel_is_allowed(channel_id) {
                    bail!(
                        "discord outbound_channel_ids entry `{channel_id}` is not in allowed_channel_ids; outbound scope must never be wider than inbound"
                    );
                }
            }
        }
        if interaction_webhook {
            return Ok(());
        }
        Ok(())
    }

    fn guild_is_allowed(&self, guild_id: &str) -> bool {
        self.allowed_guild_ids
            .iter()
            .any(|allowed| allowed == guild_id)
    }

    fn channel_is_allowed(&self, channel_id: &str) -> bool {
        self.channel_wildcard
            || self
                .allowed_channel_ids
                .iter()
                .any(|allowed| allowed == channel_id)
    }

    fn thread_is_listed(&self, thread_id: &str) -> bool {
        self.allowed_thread_ids
            .iter()
            .any(|allowed| allowed == thread_id)
    }

    fn outbound_channel_is_allowed(&self, channel_id: &str) -> bool {
        self.outbound_wildcard
            || self
                .outbound_channel_ids
                .iter()
                .any(|allowed| allowed == channel_id)
    }

    /// Redacted effective policy for `configure` and health diagnostics. Counts
    /// and modes only: raw IDs stay out of host telemetry.
    fn diagnostics(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                META_ALLOWED_GUILD_COUNT.to_string(),
                self.allowed_guild_ids.len().to_string(),
            ),
            (
                META_ALLOWED_CHANNEL_COUNT.to_string(),
                self.allowed_channel_ids.len().to_string(),
            ),
            (
                META_ALLOWED_THREAD_COUNT.to_string(),
                self.allowed_thread_ids.len().to_string(),
            ),
            (
                META_ALLOWED_SENDER_COUNT.to_string(),
                self.allowed_sender_ids.len().to_string(),
            ),
            (
                META_ALLOWED_DM_SENDER_COUNT.to_string(),
                self.allowed_dm_sender_ids.len().to_string(),
            ),
            (
                META_OUTBOUND_CHANNEL_COUNT.to_string(),
                self.outbound_channel_ids.len().to_string(),
            ),
            (
                META_CHANNEL_WILDCARD.to_string(),
                self.channel_wildcard.to_string(),
            ),
            (
                META_ACTIVATION.to_string(),
                self.activation_name().to_string(),
            ),
            (
                META_THREAD_POLICY.to_string(),
                self.thread_policy_name().to_string(),
            ),
            (
                META_DM_POLICY.to_string(),
                self.dm_policy_name().to_string(),
            ),
            (
                META_REPLY_DELIVERY.to_string(),
                self.reply_delivery_name().to_string(),
            ),
        ])
    }

    fn activation_name(&self) -> &'static str {
        match self.activation {
            ActivationMode::MentionOrReply => ACTIVATION_MENTION_OR_REPLY,
            ActivationMode::SlashCommand => ACTIVATION_SLASH_COMMAND,
            ActivationMode::AllMessages => ACTIVATION_ALL_MESSAGES,
        }
    }

    fn thread_policy_name(&self) -> &'static str {
        match self.thread_policy {
            ThreadPolicy::Deny => THREAD_POLICY_DENY,
            ThreadPolicy::InheritParent => THREAD_POLICY_INHERIT_PARENT,
            ThreadPolicy::Allowlist => THREAD_POLICY_ALLOWLIST,
        }
    }

    fn dm_policy_name(&self) -> &'static str {
        match self.dm_policy {
            DmPolicy::Deny => DM_POLICY_DENY,
            DmPolicy::Allowlist => DM_POLICY_ALLOWLIST,
            DmPolicy::Open => DM_POLICY_OPEN,
        }
    }

    fn reply_delivery_name(&self) -> &'static str {
        match self.reply_delivery {
            ReplyDelivery::RuntimeOwned => REPLY_DELIVERY_RUNTIME_OWNED,
            ReplyDelivery::ToolOwned => REPLY_DELIVERY_TOOL_OWNED,
        }
    }

    /// Project the binding onto the generic host policy envelope.
    ///
    /// A guild is a workspace, one level above a conversation, so guild IDs go
    /// to `allowed_workspace_ids`. Placing them in `allowed_conversation_ids`
    /// would leave the host with no conversation allowlist to enforce.
    fn to_channel_policy(&self, config: &ChannelConfig) -> ChannelPolicy {
        let mut allowed_conversation_ids = config.allowed_channel_ids.clone();
        // Under an explicit thread allowlist each listed thread is its own
        // authorized conversation, so the host sees it alongside the channels.
        if self.thread_policy == ThreadPolicy::Allowlist {
            allowed_conversation_ids.extend(self.allowed_thread_ids.iter().cloned());
        }
        let mut allowed_outbound_conversation_ids = self.outbound_channel_ids.clone();
        if self.outbound_wildcard {
            allowed_outbound_conversation_ids.push(CHANNEL_WILDCARD.to_string());
        }

        ChannelPolicy {
            owner_id: self.owner_id.clone(),
            allowed_sender_ids: self.allowed_sender_ids.clone(),
            allowed_conversation_ids,
            allowed_workspace_ids: self.allowed_guild_ids.clone(),
            allowed_outbound_conversation_ids,
            activation: Some(self.activation_name().to_string()),
            thread_policy: Some(self.thread_policy_name().to_string()),
            allowed_thread_ids: self.allowed_thread_ids.clone(),
            dm_policy: Some(self.dm_policy_name().to_string()),
            allowed_dm_sender_ids: self.allowed_dm_sender_ids.clone(),
            reply_delivery: Some(self.reply_delivery_name().to_string()),
            require_signature_validation: Some(true),
            allow_group_messages: None,
            max_attachment_bytes: None,
            metadata: BTreeMap::new(),
        }
    }
}

fn uses_interaction_webhook(config: &ChannelConfig) -> bool {
    resolved_endpoint(config).is_some()
}

// -----------------------------------------------------------------------------
// Conversation scope resolution
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationKind {
    Channel,
    Thread,
    Dm,
}

impl ConversationKind {
    fn name(self) -> &'static str {
        match self {
            Self::Channel => CONVERSATION_KIND_CHANNEL,
            Self::Thread => CONVERSATION_KIND_THREAD,
            Self::Dm => CONVERSATION_KIND_DM,
        }
    }
}

/// The conversation an event belongs to, after thread parentage is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConversationScope {
    id: String,
    kind: ConversationKind,
    workspace_id: Option<String>,
    parent_conversation_id: Option<String>,
}

/// Channel shape the provider stated in the event payload.
///
/// Gateway message events carry no channel object, so this is empty there and
/// parentage must be resolved through the API instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ChannelHint {
    kind: Option<u8>,
    parent_id: Option<String>,
}

impl ChannelHint {
    fn is_thread(&self) -> Option<bool> {
        self.kind.map(is_thread_channel_type)
    }
}

fn is_thread_channel_type(kind: u8) -> bool {
    matches!(
        kind,
        CHANNEL_TYPE_ANNOUNCEMENT_THREAD | CHANNEL_TYPE_PUBLIC_THREAD | CHANNEL_TYPE_PRIVATE_THREAD
    )
}

/// Reads channel shape for conversations the event payload did not describe.
trait DiscordChannelLookup {
    fn channel(&self, channel_id: &str) -> Result<DiscordChannelInfo>;
}

/// Lookup that answers nothing, used where the provider is expected to describe
/// the conversation in the payload itself. Every unresolved conversation then
/// fails closed instead of reaching for the network on a rejected event.
struct NoChannelLookup;

impl DiscordChannelLookup for NoChannelLookup {
    fn channel(&self, channel_id: &str) -> Result<DiscordChannelInfo> {
        Err(anyhow!(
            "discord channel {channel_id} has no provider-supplied shape and no lookup is available"
        ))
    }
}

/// Lookup backed by the Discord API with a bounded, expiring cache.
struct RestChannelLookup<'a> {
    client: &'a DiscordClient,
}

impl DiscordChannelLookup for RestChannelLookup<'_> {
    fn channel(&self, channel_id: &str) -> Result<DiscordChannelInfo> {
        if let Some(cached) = cached_channel(channel_id) {
            return Ok(cached);
        }
        let info = self.client.channel(channel_id)?;
        store_cached_channel(channel_id, &info);
        Ok(info)
    }
}

struct CachedChannel {
    info: DiscordChannelInfo,
    fetched_at: Instant,
}

static DISCORD_CHANNEL_CACHE: Mutex<BTreeMap<String, CachedChannel>> = Mutex::new(BTreeMap::new());

fn cached_channel(channel_id: &str) -> Option<DiscordChannelInfo> {
    let mut cache = DISCORD_CHANNEL_CACHE.lock().ok()?;
    let entry = cache.get(channel_id)?;
    if entry.fetched_at.elapsed() >= DISCORD_CHANNEL_CACHE_TTL {
        cache.remove(channel_id);
        return None;
    }
    Some(entry.info.clone())
}

fn store_cached_channel(channel_id: &str, info: &DiscordChannelInfo) {
    let Ok(mut cache) = DISCORD_CHANNEL_CACHE.lock() else {
        return;
    };
    cache.retain(|_, entry| entry.fetched_at.elapsed() < DISCORD_CHANNEL_CACHE_TTL);
    if cache.len() >= DISCORD_CHANNEL_CACHE_CAPACITY && !cache.contains_key(channel_id) {
        // Drop the whole generation rather than evicting by guesswork: the cache
        // is a latency aid, and every miss still resolves through the API.
        cache.clear();
    }
    cache.insert(
        channel_id.to_string(),
        CachedChannel {
            info: info.clone(),
            fetched_at: Instant::now(),
        },
    );
}

/// Resolve which conversation an event belongs to and prove thread parentage.
///
/// Returns a rejection code when the conversation cannot be placed, because an
/// unplaced conversation cannot be matched against any allowlist.
fn resolve_conversation_scope(
    channel_id: &str,
    guild_id: Option<&str>,
    hint: &ChannelHint,
    lookup: &dyn DiscordChannelLookup,
) -> Result<ConversationScope, &'static str> {
    let Some(guild_id) = guild_id else {
        return Ok(ConversationScope {
            id: channel_id.to_string(),
            kind: ConversationKind::Dm,
            workspace_id: None,
            parent_conversation_id: None,
        });
    };

    let channel_scope = |parent: Option<String>, kind: ConversationKind| ConversationScope {
        id: channel_id.to_string(),
        kind,
        workspace_id: Some(guild_id.to_string()),
        parent_conversation_id: parent,
    };

    if hint.kind.is_none() && hint.parent_id.is_some() {
        return Err(REJECT_UNRESOLVED_CONVERSATION);
    }

    if let Some(is_thread) = hint.is_thread() {
        if !is_thread {
            return Ok(channel_scope(None, ConversationKind::Channel));
        }
        let Some(parent_id) = hint.parent_id.clone() else {
            return Err(REJECT_UNRESOLVED_CONVERSATION);
        };
        return Ok(channel_scope(Some(parent_id), ConversationKind::Thread));
    }

    // No provider-supplied shape. Resolve it before applying either channel or
    // thread policy; the wildcard names channels only and cannot erase a deny
    // or allowlist decision for child threads.
    let Ok(info) = lookup.channel(channel_id) else {
        return Err(REJECT_UNRESOLVED_CONVERSATION);
    };
    if info.guild_id.as_deref() != Some(guild_id) {
        return Err(REJECT_UNAUTHORIZED_WORKSPACE);
    }
    if !is_thread_channel_type(info.kind) {
        return Ok(channel_scope(None, ConversationKind::Channel));
    }
    let Some(parent_id) = info.parent_id else {
        return Err(REJECT_UNRESOLVED_CONVERSATION);
    };
    Ok(channel_scope(Some(parent_id), ConversationKind::Thread))
}

// -----------------------------------------------------------------------------
// Inbound policy evaluation
// -----------------------------------------------------------------------------

/// Provider identity this binding proved before it accepts any event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscordBotIdentity {
    /// Bot user ID resolved from `GET /users/@me` for gateway ingress, or the
    /// verified application ID for interaction ingress.
    account_id: String,
    /// Application ID the binding declares, when it declares one.
    application_id: Option<String>,
}

/// What the provider said about one inbound Discord event.
///
/// Every field is a provider-supplied value read from the payload. None of it
/// is caller metadata, and none of it is display text.
#[derive(Debug, Clone)]
struct InboundCandidate<'a> {
    channel_id: &'a str,
    guild_id: Option<&'a str>,
    hint: ChannelHint,
    author_id: &'a str,
    author_is_bot: bool,
    is_webhook: bool,
    surface: CandidateSurface<'a>,
}

#[derive(Debug, Clone)]
enum CandidateSurface<'a> {
    /// A gateway message the provider delivered because the bot account can see
    /// the channel. Visibility is not addressing, so activation is proven here.
    Message {
        mentions_account: bool,
        /// Author of the referenced message, only when the provider resolved
        /// it. A bare reference leaves this unset and cannot prove a reply.
        referenced_message_author_id: Option<&'a str>,
        has_content: bool,
    },
    /// An interaction the provider routed to this application by ID.
    Interaction,
}

/// An event that satisfied the binding policy, with the evidence that proved it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorizedEvent {
    scope: ConversationScope,
    activation: InboundActivation,
}

/// The single inbound policy evaluator.
///
/// Gateway messages, interaction webhooks, DMs, and threads all decide here so
/// one surface cannot be authorized by rules another surface never applied.
/// Order is fixed: loop source, workspace, conversation, sender, activation.
fn evaluate_inbound(
    policy: &DiscordPolicy,
    identity: &DiscordBotIdentity,
    candidate: &InboundCandidate<'_>,
    lookup: &dyn DiscordChannelLookup,
) -> Result<AuthorizedEvent, &'static str> {
    // 1. Loop sources. A message this account or any other automation authored
    // must never wake the binding, or a reply becomes its own next request.
    if candidate.author_is_bot || candidate.is_webhook || candidate.author_id == identity.account_id
    {
        return Err(REJECT_BOT_EVENT);
    }

    // 2. Workspace scope. A DM has no guild, so the DM policy decides instead.
    match candidate.guild_id {
        Some(guild_id) => {
            if !policy.guild_is_allowed(guild_id) {
                return Err(REJECT_UNAUTHORIZED_WORKSPACE);
            }
        }
        None => {
            if policy.dm_policy == DmPolicy::Deny {
                return Err(REJECT_DM_DENIED);
            }
        }
    }

    // 3. Conversation scope, including which allowlist a thread answers to.
    let scope = resolve_conversation_scope(
        candidate.channel_id,
        candidate.guild_id,
        &candidate.hint,
        lookup,
    )?;
    match scope.kind {
        ConversationKind::Channel => {
            if !policy.channel_is_allowed(&scope.id) {
                return Err(REJECT_UNAUTHORIZED_CHANNEL);
            }
        }
        ConversationKind::Thread => {
            let parent_id = scope
                .parent_conversation_id
                .as_deref()
                .ok_or(REJECT_UNRESOLVED_CONVERSATION)?;
            match policy.thread_policy {
                ThreadPolicy::Deny => return Err(REJECT_UNAUTHORIZED_THREAD),
                ThreadPolicy::Allowlist => {
                    if !policy.thread_is_listed(&scope.id) || !policy.channel_is_allowed(parent_id)
                    {
                        return Err(REJECT_UNAUTHORIZED_THREAD);
                    }
                }
                ThreadPolicy::InheritParent => {
                    if !policy.channel_is_allowed(parent_id) {
                        return Err(REJECT_UNAUTHORIZED_THREAD);
                    }
                }
            }
        }
        ConversationKind::Dm => {}
    }

    // 4. Sender policy. Guild and DM rules are independent: a DM sender is never
    // authorized by guild membership, and a guild sender is never authorized by
    // the DM allowlist.
    if let Some(owner_id) = policy.owner_id.as_deref()
        && owner_id != candidate.author_id
    {
        return Err(REJECT_SENDER_DENIED);
    }
    match scope.kind {
        ConversationKind::Dm => match policy.dm_policy {
            DmPolicy::Deny => return Err(REJECT_DM_DENIED),
            DmPolicy::Allowlist => {
                if !policy
                    .allowed_dm_sender_ids
                    .iter()
                    .any(|allowed| allowed == candidate.author_id)
                {
                    return Err(REJECT_SENDER_DENIED);
                }
            }
            // The written-down open mode authorizes every sender, so no further
            // sender condition applies. `owner_id` was already checked above and
            // still narrows this surface when it is set.
            DmPolicy::Open => {}
        },
        ConversationKind::Channel | ConversationKind::Thread => {
            // An empty guild sender allowlist accepts any sender already inside
            // an allowed conversation; channel scope and activation are the
            // boundary. It never widens which conversations are in scope.
            if !policy.allowed_sender_ids.is_empty()
                && !policy
                    .allowed_sender_ids
                    .iter()
                    .any(|allowed| allowed == candidate.author_id)
            {
                return Err(REJECT_SENDER_DENIED);
            }
        }
    }

    // 5. Activation. Being visible in an allowed conversation is not a request.
    let activation = match &candidate.surface {
        CandidateSurface::Interaction => InboundActivation {
            // TODO: The protocol has no distinct activation reason for message
            // components or modal submits. All three interaction kinds report
            // `slash_command` because the provider routed each of them to this
            // application by ID; revisit if the protocol gains finer reasons.
            reason: InboundActivation::REASON_SLASH_COMMAND.to_string(),
            agent_account_id: Some(identity.account_id.clone()),
            referenced_message_author_id: None,
        },
        CandidateSurface::Message {
            mentions_account,
            referenced_message_author_id,
            has_content,
        } => {
            let activation = message_activation(
                policy,
                identity,
                scope.kind,
                *mentions_account,
                *referenced_message_author_id,
            )?;
            // Addressing evidence proves who the message targeted, but it does
            // not turn an unreadable provider event into a user request. Until
            // attachments have an explicit readable-payload policy, message
            // content is required before this binding may wake a workload.
            if !has_content {
                return Err(REJECT_EMPTY_MESSAGE);
            }
            activation
        }
    };

    Ok(AuthorizedEvent { scope, activation })
}

/// Decide why, if at all, a plain message addressed this binding.
fn message_activation(
    policy: &DiscordPolicy,
    identity: &DiscordBotIdentity,
    kind: ConversationKind,
    mentions_account: bool,
    referenced_message_author_id: Option<&str>,
) -> Result<InboundActivation, &'static str> {
    // A direct message is addressed to this account by construction; the DM
    // policy and sender allowlist already decided whether it is authorized.
    if kind == ConversationKind::Dm {
        return Ok(InboundActivation {
            reason: InboundActivation::REASON_DIRECT_MESSAGE.to_string(),
            agent_account_id: Some(identity.account_id.clone()),
            referenced_message_author_id: referenced_message_author_id.map(str::to_owned),
        });
    }

    match policy.activation {
        ActivationMode::AllMessages => Ok(InboundActivation {
            reason: InboundActivation::REASON_ALL_MESSAGES.to_string(),
            agent_account_id: Some(identity.account_id.clone()),
            referenced_message_author_id: referenced_message_author_id.map(str::to_owned),
        }),
        // Only a provider-routed interaction activates this mode, so a plain
        // message in the conversation is not a request.
        ActivationMode::SlashCommand => Err(REJECT_NOT_ADDRESSED),
        ActivationMode::MentionOrReply => {
            if mentions_account {
                return Ok(InboundActivation {
                    reason: InboundActivation::REASON_DIRECT_MENTION.to_string(),
                    agent_account_id: Some(identity.account_id.clone()),
                    referenced_message_author_id: referenced_message_author_id.map(str::to_owned),
                });
            }
            match referenced_message_author_id {
                Some(author_id) if author_id == identity.account_id => Ok(InboundActivation {
                    reason: InboundActivation::REASON_REPLY_TO_AGENT.to_string(),
                    agent_account_id: Some(identity.account_id.clone()),
                    referenced_message_author_id: Some(author_id.to_string()),
                }),
                // A reply to someone else, and a reference whose target author
                // the provider did not resolve, both prove nothing about who
                // the message addressed.
                _ => Err(REJECT_NOT_ADDRESSED),
            }
        }
    }
}

struct DiscordIngressWorker {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Option<IngressState>>>,
    handle: JoinHandle<()>,
}

fn restart_ingress_worker(
    worker: &mut Option<DiscordIngressWorker>,
    config: ChannelConfig,
    state: IngressState,
    stdout_lock: Arc<Mutex<()>>,
) {
    let _ = stop_ingress_worker(worker);
    let stop = Arc::new(AtomicBool::new(false));
    let shared_state = Arc::new(Mutex::new(Some(state.clone())));
    let worker_stop = Arc::clone(&stop);
    let worker_state = Arc::clone(&shared_state);
    let handle = thread::spawn(move || {
        run_discord_websocket_worker(config, state, worker_stop, worker_state, stdout_lock);
    });
    *worker = Some(DiscordIngressWorker {
        stop,
        state: shared_state,
        handle,
    });
}

fn stop_ingress_worker(worker: &mut Option<DiscordIngressWorker>) -> Option<IngressState> {
    let worker = worker.take()?;
    worker.stop.store(true, Ordering::Relaxed);
    let _ = worker.handle.join();
    worker.state.lock().ok().and_then(|state| (*state).clone())
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let identity = client.identity()?;
    let policy = DiscordPolicy::from_config(config)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    if let Some(default_channel_id) = &config.default_channel_id {
        metadata.insert(
            META_DEFAULT_CHANNEL_ID.to_string(),
            default_channel_id.clone(),
        );
    }
    metadata.extend(policy.diagnostics());

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: Some(identity.id),
        display_name: Some(identity.global_name.unwrap_or(identity.username)),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    read_required_env(bot_token_env(config))?;
    let policy = DiscordPolicy::from_config(config)?;
    policy.validate_for_ingress(uses_interaction_webhook(config))?;

    if let Some(endpoint) = resolved_endpoint(config) {
        read_required_env(interaction_public_key_env(config))?;
        let mut metadata = BTreeMap::new();
        metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
        metadata.insert(
            META_VERIFICATION_KEY_ENV.to_string(),
            interaction_public_key_env(config).to_string(),
        );
        metadata.insert(
            META_HOST_ACTION.to_string(),
            "route Discord interaction POSTs to the reported endpoint and verify signatures with the configured public key".to_string(),
        );
        metadata.extend(policy.diagnostics());

        return Ok(IngressState {
            mode: IngressMode::InteractionWebhook,
            status: "configured".to_string(),
            endpoint: Some(endpoint),
            metadata,
        });
    }

    // Prove the account identity before the gateway is opened. Without it the
    // plugin cannot tell a mention of this bot from a mention of anyone else,
    // and cannot recognize its own messages to break reply loops.
    let identity = resolve_bot_identity(config)?;
    Ok(websocket_ingress_state(&policy, &identity))
}

fn websocket_ingress_state(policy: &DiscordPolicy, identity: &DiscordBotIdentity) -> IngressState {
    let mut metadata = websocket_state_metadata();
    metadata.insert(
        META_HOST_ACTION.to_string(),
        "keep a Discord websocket connection open and emit message events as channel.event notifications".to_string(),
    );
    metadata.insert(META_BOT_USER_ID.to_string(), identity.account_id.clone());
    metadata.extend(policy.diagnostics());

    IngressState {
        mode: IngressMode::Websocket,
        status: "running".to_string(),
        endpoint: None,
        metadata,
    }
}

/// Resolve the authenticated bot account from the provider.
///
/// The identity comes from the credential the binding holds, never from display
/// names or message text, which any member can copy.
fn resolve_bot_identity(config: &ChannelConfig) -> Result<DiscordBotIdentity> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let identity = client.identity()?;
    Ok(DiscordBotIdentity {
        account_id: identity.id,
        application_id: config.application_id.clone(),
    })
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    read_required_env(bot_token_env(config))?;

    let mut stopped = state.unwrap_or_else(|| {
        if let Some(endpoint) = resolved_endpoint(config) {
            IngressState {
                mode: IngressMode::InteractionWebhook,
                status: "configured".to_string(),
                endpoint: Some(endpoint),
                metadata: BTreeMap::new(),
            }
        } else {
            websocket_state("running", None, None, None, None)
        }
    });
    stopped.status = "stopped".to_string();
    stopped
        .metadata
        .insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    Ok(stopped)
}

fn handle_ingress_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if !payload.method.eq_ignore_ascii_case("POST") {
        return Ok(ingress_rejection(
            405,
            "discord interactions expect POST requests",
        ));
    }

    if let Some(reply) = validate_ingress_signature(config, payload)? {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(reply),
            state: None,
            poll_after_ms: None,
        });
    }

    let interaction: DiscordInteraction = match serde_json::from_str(&payload.body) {
        Ok(interaction) => interaction,
        Err(_) => {
            return Ok(ingress_rejection(
                400,
                "invalid Discord interaction payload",
            ));
        }
    };

    if interaction.interaction_type == INTERACTION_TYPE_PING {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(discord_json_reply(json!({ "type": INTERACTION_TYPE_PING }))),
            state: None,
            poll_after_ms: None,
        });
    }

    let policy = match DiscordPolicy::from_config(config).and_then(|policy| {
        policy.validate_for_ingress(true)?;
        Ok(policy)
    }) {
        Ok(policy) => policy,
        Err(_) => {
            // An unresolvable policy authorizes nothing, including an
            // interaction the provider addressed to this application.
            debug_discord_reject(REJECT_INVALID_POLICY, &interaction.id);
            return Ok(PluginResponse::IngressEventsReceived {
                events: Vec::new(),
                callback_reply: Some(discord_ephemeral_message(
                    "This Discord binding is not configured for this request.",
                )),
                state: None,
                poll_after_ms: None,
            });
        }
    };

    match build_inbound_event(&policy, &interaction, payload) {
        InteractionOutcome::Event(event) => Ok(PluginResponse::IngressEventsReceived {
            events: vec![*event],
            callback_reply: Some(discord_ephemeral_message(
                "Dispatch is processing your request.",
            )),
            state: None,
            poll_after_ms: None,
        }),
        InteractionOutcome::Rejected(reason) => {
            debug_discord_reject(reason, &interaction.id);
            Ok(PluginResponse::IngressEventsReceived {
                events: Vec::new(),
                callback_reply: Some(discord_ephemeral_message(
                    "This Discord conversation is not allowed for this binding.",
                )),
                state: None,
                poll_after_ms: None,
            })
        }
        InteractionOutcome::Unsupported => {
            let message = if interaction.interaction_type
                == INTERACTION_TYPE_APPLICATION_COMMAND_AUTOCOMPLETE
            {
                "Discord autocomplete interactions are not implemented by this plugin."
            } else {
                "This Discord interaction type is not implemented by this plugin."
            };
            Ok(PluginResponse::IngressEventsReceived {
                events: Vec::new(),
                callback_reply: Some(discord_ephemeral_message(message)),
                state: None,
                poll_after_ms: None,
            })
        }
    }
}

fn handle_websocket_receive(
    config: &ChannelConfig,
    state: Option<&IngressState>,
) -> Result<PluginResponse> {
    discord_websocket_receive(config, state.cloned())
}

fn discord_websocket_receive(
    config: &ChannelConfig,
    state: Option<IngressState>,
) -> Result<PluginResponse> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let mut session = DiscordGatewaySession::from_state(state.as_ref());
    let policy = DiscordPolicy::from_config(config)?;
    policy.validate_for_ingress(false)?;
    let identity = session_bot_identity(&mut session, config, &client)?;
    let lookup = RestChannelLookup { client: &client };
    let context = DiscordIngressContext {
        config,
        policy: &policy,
        identity: &identity,
        lookup: &lookup,
    };
    let gateway_url =
        discord_gateway_base_url(&session, || Ok(DISCORD_GATEWAY_BASE_URL.to_string()))?;
    let websocket_url = discord_gateway_websocket_url(&gateway_url);
    let (mut socket, _) = tungstenite::connect(websocket_url.as_str())
        .with_context(|| format!("failed to connect Discord websocket: {websocket_url}"))?;
    let deadline = Instant::now() + discord_websocket_timeout_window();

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(PluginResponse::IngressEventsReceived {
                events: Vec::new(),
                callback_reply: None,
                state: Some(session.to_state("running")),
                poll_after_ms: Some(1000),
            });
        }
        if now >= session.next_heartbeat {
            if session.awaiting_heartbeat_ack {
                close_discord_websocket_for_reconnect(&mut socket);
                return Ok(discord_gateway_action_response(
                    DiscordGatewayAction::Reconnect,
                    &session,
                ));
            }
            send_discord_session_heartbeat(&mut socket, &mut session)?;
        }
        configure_websocket_read_timeout(
            socket.get_mut(),
            std::cmp::min(
                DISCORD_GATEWAY_READ_TIMEOUT,
                deadline.saturating_duration_since(now),
            ),
        )?;
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(action) = handle_discord_gateway_text(
                    &context,
                    &mut socket,
                    text.as_str(),
                    &mut session,
                    client.bot_token(),
                )? {
                    return Ok(discord_gateway_action_response(action, &session));
                }
            }
            Ok(Message::Binary(bytes)) => {
                let text = std::str::from_utf8(bytes.as_ref())
                    .context("Discord websocket binary frame was not valid UTF-8")?;
                if let Some(action) = handle_discord_gateway_text(
                    &context,
                    &mut socket,
                    text,
                    &mut session,
                    client.bot_token(),
                )? {
                    return Ok(discord_gateway_action_response(action, &session));
                }
            }
            Ok(Message::Ping(payload)) => {
                socket.send(Message::Pong(payload))?;
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(frame)) => {
                let close_action = handle_discord_close_frame(&mut session, frame.as_ref());
                return Ok(PluginResponse::IngressEventsReceived {
                    events: Vec::new(),
                    callback_reply: None,
                    state: Some(session.to_state(discord_close_status(close_action))),
                    poll_after_ms: discord_close_poll_after(close_action),
                });
            }
            Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(PluginResponse::IngressEventsReceived {
                    events: Vec::new(),
                    callback_reply: None,
                    state: Some(session.to_state("closed")),
                    poll_after_ms: Some(1000),
                });
            }
            Err(error) => return Err(error).context("failed to read Discord websocket frame"),
        }
    }
}

fn run_discord_websocket_worker(
    config: ChannelConfig,
    initial_state: IngressState,
    stop: Arc<AtomicBool>,
    shared_state: Arc<Mutex<Option<IngressState>>>,
    stdout_lock: Arc<Mutex<()>>,
) {
    let mut state = Some(initial_state);
    let mut failure_backoff = Duration::from_secs(1);
    let mut consecutive_failures = 0_u32;
    while !stop.load(Ordering::Relaxed) {
        let mut healthy_since = None;
        let session = run_discord_websocket_session(
            &config,
            state.clone(),
            &stop,
            &shared_state,
            &stdout_lock,
            &mut healthy_since,
        );
        // Clear retry history only after a sustained gateway session.
        if discord_session_was_stable(healthy_since) {
            consecutive_failures = 0;
            failure_backoff = Duration::from_secs(1);
        }
        // Do not classify host-requested shutdown as a gateway failure.
        if stop.load(Ordering::Relaxed) {
            if let Ok(next_state) = session {
                set_worker_state(&shared_state, next_state);
            }
            break;
        }
        match session {
            Ok(next_state) => {
                let should_stop = next_state.status == "stopped";
                let should_reconnect = matches!(next_state.status.as_str(), "closed" | "reconnect");
                state = Some(next_state);
                if should_stop {
                    eprintln!(
                        "discord websocket worker terminated after an unrecoverable gateway close"
                    );
                    std::process::exit(1);
                }
                if should_reconnect {
                    consecutive_failures += 1;
                    if consecutive_failures >= DISCORD_MAX_CONSECUTIVE_FAILURES {
                        eprintln!(
                            "discord websocket worker terminated after {consecutive_failures} consecutive disconnects"
                        );
                        std::process::exit(1);
                    }
                    sleep_until_stopped(&stop, failure_backoff);
                    failure_backoff = std::cmp::min(failure_backoff * 2, Duration::from_secs(60));
                    continue;
                }
                consecutive_failures = 0;
                failure_backoff = Duration::from_secs(1);
            }
            Err(error) => {
                consecutive_failures += 1;
                if consecutive_failures >= DISCORD_MAX_CONSECUTIVE_FAILURES {
                    eprintln!(
                        "discord websocket worker terminated after {consecutive_failures} receive failures: {error:#}"
                    );
                    std::process::exit(1);
                }
                eprintln!("discord websocket worker reconnecting after receive failure: {error:#}");
                sleep_until_stopped(&stop, failure_backoff);
                failure_backoff = std::cmp::min(failure_backoff * 2, Duration::from_secs(60));
                continue;
            }
        }
        sleep_until_stopped(&stop, Duration::from_secs(1));
    }
}

fn run_discord_websocket_session(
    config: &ChannelConfig,
    state: Option<IngressState>,
    stop: &AtomicBool,
    shared_state: &Arc<Mutex<Option<IngressState>>>,
    stdout_lock: &Arc<Mutex<()>>,
    healthy_since: &mut Option<Instant>,
) -> Result<IngressState> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let mut session = DiscordGatewaySession::from_state(state.as_ref());
    let policy = DiscordPolicy::from_config(config)?;
    policy.validate_for_ingress(false)?;
    let identity = session_bot_identity(&mut session, config, &client)?;
    let lookup = RestChannelLookup { client: &client };
    let context = DiscordIngressContext {
        config,
        policy: &policy,
        identity: &identity,
        lookup: &lookup,
    };
    let gateway_url =
        discord_gateway_base_url(&session, || Ok(DISCORD_GATEWAY_BASE_URL.to_string()))?;
    let websocket_url = discord_gateway_websocket_url(&gateway_url);
    let (mut socket, _) = tungstenite::connect(websocket_url.as_str())
        .with_context(|| format!("failed to connect Discord websocket: {websocket_url}"))?;

    loop {
        if stop.load(Ordering::Relaxed) {
            close_discord_websocket_for_stop(&mut socket);
            return Ok(session.to_state("running"));
        }
        let now = Instant::now();
        if now >= session.next_heartbeat {
            if session.awaiting_heartbeat_ack {
                let state = session.to_state("reconnect");
                set_worker_state(shared_state, state.clone());
                close_discord_websocket_for_reconnect(&mut socket);
                return Ok(state);
            }
            send_discord_session_heartbeat(&mut socket, &mut session)?;
        }
        configure_websocket_read_timeout(socket.get_mut(), DISCORD_GATEWAY_READ_TIMEOUT)?;
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(action) = handle_discord_gateway_text(
                    &context,
                    &mut socket,
                    text.as_str(),
                    &mut session,
                    client.bot_token(),
                )? {
                    match action {
                        DiscordGatewayAction::Event(event) => {
                            healthy_since.get_or_insert_with(Instant::now);
                            let state = session.to_state("running");
                            set_worker_state(shared_state, state.clone());
                            emit_channel_event_notification(
                                stdout_lock,
                                vec![*event],
                                Some(state),
                                Some(0),
                            )?;
                        }
                        DiscordGatewayAction::Heartbeat => {
                            healthy_since.get_or_insert_with(Instant::now);
                            let state = session.to_state("running");
                            set_worker_state(shared_state, state.clone());
                            emit_channel_event_notification(
                                stdout_lock,
                                Vec::new(),
                                Some(state),
                                Some(0),
                            )?;
                        }
                        DiscordGatewayAction::Reconnect => {
                            let state = session.to_state("reconnect");
                            set_worker_state(shared_state, state.clone());
                            return Ok(state);
                        }
                    }
                }
            }
            Ok(Message::Binary(bytes)) => {
                let text = std::str::from_utf8(bytes.as_ref())
                    .context("Discord websocket binary frame was not valid UTF-8")?;
                if let Some(action) = handle_discord_gateway_text(
                    &context,
                    &mut socket,
                    text,
                    &mut session,
                    client.bot_token(),
                )? {
                    match action {
                        DiscordGatewayAction::Event(event) => {
                            healthy_since.get_or_insert_with(Instant::now);
                            let state = session.to_state("running");
                            set_worker_state(shared_state, state.clone());
                            emit_channel_event_notification(
                                stdout_lock,
                                vec![*event],
                                Some(state),
                                Some(0),
                            )?;
                        }
                        DiscordGatewayAction::Heartbeat => {
                            healthy_since.get_or_insert_with(Instant::now);
                            let state = session.to_state("running");
                            set_worker_state(shared_state, state.clone());
                            emit_channel_event_notification(
                                stdout_lock,
                                Vec::new(),
                                Some(state),
                                Some(0),
                            )?;
                        }
                        DiscordGatewayAction::Reconnect => {
                            let state = session.to_state("reconnect");
                            set_worker_state(shared_state, state.clone());
                            return Ok(state);
                        }
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                socket.send(Message::Pong(payload))?;
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(frame)) => {
                let close_action = handle_discord_close_frame(&mut session, frame.as_ref());
                let state = session.to_state(discord_close_status(close_action));
                set_worker_state(shared_state, state.clone());
                return Ok(state);
            }
            Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                let state = session.to_state("closed");
                set_worker_state(shared_state, state.clone());
                return Ok(state);
            }
            Err(error) => return Err(error).context("failed to read Discord websocket frame"),
        }
    }
}

fn discord_session_was_stable(healthy_since: Option<Instant>) -> bool {
    healthy_since
        .is_some_and(|healthy_since| healthy_since.elapsed() >= DISCORD_SESSION_STABILITY_WINDOW)
}

fn set_worker_state(shared_state: &Arc<Mutex<Option<IngressState>>>, state: IngressState) {
    if let Ok(mut guard) = shared_state.lock() {
        *guard = Some(state);
    }
}

fn sleep_until_stopped(stop: &AtomicBool, duration: Duration) {
    let mut slept = Duration::ZERO;
    while slept < duration && !stop.load(Ordering::Relaxed) {
        let step = std::cmp::min(Duration::from_millis(250), duration - slept);
        thread::sleep(step);
        slept += step;
    }
}

fn emit_channel_event_notification(
    stdout_lock: &Arc<Mutex<()>>,
    events: Vec<InboundEventEnvelope>,
    state: Option<IngressState>,
    poll_after_ms: Option<u64>,
) -> Result<()> {
    let envelope = PluginNotificationEnvelope {
        protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
        notification: ChannelEventNotification {
            events,
            state,
            poll_after_ms,
        },
    };
    let json = notification_to_jsonrpc(&envelope)
        .map_err(|error| anyhow!("failed to encode channel event notification: {error}"))?;
    write_stdout_line(stdout_lock, &json).context("failed to write channel event notification")
}

struct DiscordGatewaySession {
    last_sequence: Option<u64>,
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    /// Bot account proven at ingress start, carried across reconnect and resume
    /// so a resumed session enforces the same identity the first one did.
    bot_user_id: Option<String>,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    awaiting_heartbeat_ack: bool,
    identified: bool,
}

impl DiscordGatewaySession {
    fn from_state(state: Option<&IngressState>) -> Self {
        Self {
            last_sequence: state
                .and_then(|state| state.metadata.get("sequence"))
                .and_then(|value| value.parse::<u64>().ok()),
            session_id: state
                .and_then(|state| state.metadata.get("session_id"))
                .cloned(),
            resume_gateway_url: state
                .and_then(|state| state.metadata.get("resume_gateway_url"))
                .cloned(),
            bot_user_id: state
                .and_then(|state| state.metadata.get(META_BOT_USER_ID))
                .cloned(),
            heartbeat_interval: Duration::from_secs(45),
            next_heartbeat: Instant::now() + Duration::from_secs(45),
            awaiting_heartbeat_ack: false,
            identified: false,
        }
    }

    fn can_resume(&self) -> bool {
        self.session_id.is_some()
            && self.last_sequence.is_some()
            && self.resume_gateway_url.is_some()
    }

    fn reset_resume(&mut self) {
        self.session_id = None;
        self.last_sequence = None;
        self.resume_gateway_url = None;
    }

    fn to_state(&self, status: &str) -> IngressState {
        websocket_state(
            status,
            self.last_sequence,
            self.session_id.as_deref(),
            self.resume_gateway_url.as_deref(),
            self.bot_user_id.as_deref(),
        )
    }
}

enum DiscordGatewayAction {
    Event(Box<InboundEventEnvelope>),
    Heartbeat,
    Reconnect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscordGatewayCloseAction {
    Resume,
    Reidentify,
    Stop,
}

fn discord_gateway_action_response(
    action: DiscordGatewayAction,
    session: &DiscordGatewaySession,
) -> PluginResponse {
    match action {
        DiscordGatewayAction::Event(event) => PluginResponse::IngressEventsReceived {
            events: vec![*event],
            callback_reply: None,
            state: Some(session.to_state("running")),
            poll_after_ms: Some(0),
        },
        DiscordGatewayAction::Heartbeat => PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: Some(session.to_state("running")),
            poll_after_ms: Some(0),
        },
        DiscordGatewayAction::Reconnect => PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: Some(session.to_state("reconnect")),
            poll_after_ms: Some(1000),
        },
    }
}

/// Everything an ingress path needs to authorize an event, resolved once per
/// session so every frame is judged against the same policy and identity.
struct DiscordIngressContext<'a> {
    config: &'a ChannelConfig,
    policy: &'a DiscordPolicy,
    identity: &'a DiscordBotIdentity,
    lookup: &'a dyn DiscordChannelLookup,
}

/// Recover the proven bot account for a session.
///
/// Ingress start records the account in the state the host hands back, so a
/// resumed session reuses it. A session that starts without one resolves it
/// from the credential rather than running with an unknown identity.
fn session_bot_identity(
    session: &mut DiscordGatewaySession,
    config: &ChannelConfig,
    client: &DiscordClient,
) -> Result<DiscordBotIdentity> {
    let account_id = match session.bot_user_id.clone() {
        Some(account_id) => account_id,
        None => {
            let resolved = client.identity()?.id;
            session.bot_user_id = Some(resolved.clone());
            resolved
        }
    };
    Ok(DiscordBotIdentity {
        account_id,
        application_id: config.application_id.clone(),
    })
}

fn handle_discord_gateway_text(
    context: &DiscordIngressContext<'_>,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    text: &str,
    session: &mut DiscordGatewaySession,
    bot_token: &str,
) -> Result<Option<DiscordGatewayAction>> {
    let payload: DiscordGatewayPayload =
        serde_json::from_str(text).context("failed to parse Discord websocket payload")?;
    if let Some(sequence) = payload.sequence {
        session.last_sequence = Some(sequence);
    }

    match payload.op {
        0 => {
            match payload.event_type.as_deref() {
                Some("READY") => {
                    debug_discord_gateway(format_args!("received READY"));
                    verify_ready_identity(context.identity, &payload.data)?;
                    if let Some(session_id) = payload.data.get("session_id").and_then(Value::as_str)
                    {
                        session.session_id = Some(session_id.to_string());
                    }
                    if let Some(resume_gateway_url) = payload
                        .data
                        .get("resume_gateway_url")
                        .and_then(Value::as_str)
                    {
                        session.resume_gateway_url = Some(resume_gateway_url.to_string());
                    }
                    return Ok(Some(DiscordGatewayAction::Heartbeat));
                }
                Some("RESUMED") => return Ok(Some(DiscordGatewayAction::Heartbeat)),
                Some("MESSAGE_CREATE") => {}
                other => {
                    debug_discord_gateway(format_args!("ignoring gateway event {:?}", other));
                    return Ok(None);
                }
            }
            let message: DiscordGatewayMessage = serde_json::from_value(payload.data)
                .context("failed to parse Discord MESSAGE_CREATE payload")?;
            let Some(event) = build_websocket_inbound_event(context, &message) else {
                return Ok(None);
            };
            Ok(Some(DiscordGatewayAction::Event(Box::new(event))))
        }
        1 => {
            send_discord_session_heartbeat(socket, session)?;
            Ok(None)
        }
        7 => Ok(Some(DiscordGatewayAction::Reconnect)),
        9 => {
            if !payload.data.as_bool().unwrap_or(false) {
                session.reset_resume();
            }
            Ok(Some(DiscordGatewayAction::Reconnect))
        }
        10 => {
            if let Some(interval) = payload
                .data
                .get("heartbeat_interval")
                .and_then(Value::as_u64)
            {
                session.heartbeat_interval = Duration::from_millis(interval);
                session.next_heartbeat = Instant::now() + session.heartbeat_interval;
            }
            if !session.identified {
                if session.can_resume() {
                    send_discord_resume(socket, bot_token, session)?;
                } else {
                    send_discord_identify(socket, bot_token, context.config)?;
                }
                session.identified = true;
            }
            Ok(None)
        }
        11 => {
            session.awaiting_heartbeat_ack = false;
            Ok(Some(DiscordGatewayAction::Heartbeat))
        }
        _ => Ok(None),
    }
}

/// Cross-check the account the gateway says this session belongs to.
///
/// The credential and the gateway must name the same account; a mismatch means
/// the session is not the one whose scope this binding validated.
fn verify_ready_identity(identity: &DiscordBotIdentity, ready: &Value) -> Result<()> {
    let ready_user_id = ready
        .get("user")
        .and_then(|user| user.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Discord READY payload did not report a bot user id"))?;
    if ready_user_id != identity.account_id {
        bail!(
            "Discord READY reported bot user {ready_user_id}, which is not the account this binding authenticated as"
        );
    }
    if let Some(expected) = identity.application_id.as_deref() {
        let ready_application_id = ready
            .get("application")
            .and_then(|application| application.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Discord READY payload did not report an application id"))?;
        if ready_application_id != expected {
            bail!(
                "Discord READY reported application {ready_application_id}, which is not the configured application_id"
            );
        }
    }
    Ok(())
}

fn send_discord_identify(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    token: &str,
    config: &ChannelConfig,
) -> Result<()> {
    let payload = json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": discord_gateway_intents(config),
            "properties": {
                "os": "linux",
                "browser": "dispatch",
                "device": "dispatch"
            }
        }
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .context("failed to identify Discord websocket")
}

fn discord_gateway_intents(config: &ChannelConfig) -> u64 {
    let mut intents = DISCORD_GATEWAY_BASE_INTENTS;
    if config.message_content_intent.unwrap_or(false) {
        intents |= DISCORD_GATEWAY_MESSAGE_CONTENT_INTENT;
    }
    intents
}

fn handle_discord_close_frame(
    session: &mut DiscordGatewaySession,
    frame: Option<&CloseFrame>,
) -> DiscordGatewayCloseAction {
    let action = discord_gateway_close_action(frame);
    if matches!(
        action,
        DiscordGatewayCloseAction::Reidentify | DiscordGatewayCloseAction::Stop
    ) {
        session.reset_resume();
    }
    log_discord_close_frame(frame, action);
    action
}

fn discord_gateway_close_action(frame: Option<&CloseFrame>) -> DiscordGatewayCloseAction {
    let Some(frame) = frame else {
        return DiscordGatewayCloseAction::Resume;
    };
    match u16::from(frame.code) {
        4003 | 4007 | 4009 => DiscordGatewayCloseAction::Reidentify,
        4004 | 4010 | 4011 | 4012 | 4013 | 4014 => DiscordGatewayCloseAction::Stop,
        _ => DiscordGatewayCloseAction::Resume,
    }
}

fn discord_close_status(action: DiscordGatewayCloseAction) -> &'static str {
    match action {
        DiscordGatewayCloseAction::Resume | DiscordGatewayCloseAction::Reidentify => "closed",
        DiscordGatewayCloseAction::Stop => "stopped",
    }
}

fn discord_close_poll_after(action: DiscordGatewayCloseAction) -> Option<u64> {
    match action {
        DiscordGatewayCloseAction::Resume | DiscordGatewayCloseAction::Reidentify => Some(1000),
        DiscordGatewayCloseAction::Stop => None,
    }
}

fn log_discord_close_frame(frame: Option<&CloseFrame>, action: DiscordGatewayCloseAction) {
    if let Some(frame) = frame {
        eprintln!(
            "discord websocket closed: code={} reason={} action={:?}",
            u16::from(frame.code),
            frame.reason,
            action
        );
    } else {
        eprintln!("discord websocket closed without a close frame action={action:?}");
    }
}

fn debug_discord_gateway(args: std::fmt::Arguments<'_>) {
    let enabled = std::env::var("DISCORD_GATEWAY_DEBUG")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if enabled {
        eprintln!("discord gateway debug: {args}");
    }
}

fn send_discord_resume(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    token: &str,
    session: &DiscordGatewaySession,
) -> Result<()> {
    let payload = json!({
        "op": 6,
        "d": {
            "token": token,
            "session_id": session.session_id.as_deref().unwrap_or_default(),
            "seq": session.last_sequence,
        }
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .context("failed to resume Discord websocket")
}

fn send_discord_heartbeat(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    last_sequence: Option<u64>,
) -> Result<()> {
    socket
        .send(Message::Text(
            json!({ "op": 1, "d": last_sequence }).to_string().into(),
        ))
        .context("failed to send Discord websocket heartbeat")
}

fn send_discord_session_heartbeat(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    session: &mut DiscordGatewaySession,
) -> Result<()> {
    send_discord_heartbeat(socket, session.last_sequence)?;
    session.awaiting_heartbeat_ack = true;
    session.next_heartbeat = Instant::now() + session.heartbeat_interval;
    Ok(())
}

fn close_discord_websocket_for_stop(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    let close = CloseFrame {
        code: CloseCode::Normal,
        reason: "stopping".into(),
    };
    let _ = socket.close(Some(close));
}

fn close_discord_websocket_for_reconnect(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    let _ = socket.close(None);
}

fn build_websocket_inbound_event(
    context: &DiscordIngressContext<'_>,
    message: &DiscordGatewayMessage,
) -> Option<InboundEventEnvelope> {
    // Only ordinary and reply messages carry a member request. The remaining
    // types are provider notices about the conversation itself, and an
    // interaction response is addressed to one member rather than to this
    // binding.
    let message_type = message.message_type.unwrap_or(MESSAGE_TYPE_DEFAULT);
    if !matches!(message_type, MESSAGE_TYPE_DEFAULT | MESSAGE_TYPE_REPLY)
        || message.flags.unwrap_or(0) & MESSAGE_FLAG_EPHEMERAL != 0
    {
        debug_discord_reject(REJECT_UNSUPPORTED_MESSAGE_TYPE, &message.id);
        return None;
    }

    // A reply points at a message; a forward points at a message that was never
    // addressed to this binding, so only a reply can carry reply activation.
    let reference = message.message_reference.as_ref().filter(|reference| {
        reference
            .reference_type
            .unwrap_or(MESSAGE_REFERENCE_TYPE_DEFAULT)
            == MESSAGE_REFERENCE_TYPE_DEFAULT
    });
    let referenced_author = reference.and_then(|_| {
        message
            .referenced_message
            .as_ref()
            .and_then(|referenced| referenced.author.as_ref())
            .map(|author| author.id.as_str())
    });

    // Mentions are matched by account ID. Display text such as `@Piper` is
    // member-controlled and proves nothing, and role mentions are a separate
    // provider field that never names this account.
    let mentions_account = message
        .mentions
        .iter()
        .any(|mention| mention.id == context.identity.account_id);

    let candidate = InboundCandidate {
        channel_id: &message.channel_id,
        guild_id: message.guild_id.as_deref(),
        hint: ChannelHint::default(),
        author_id: &message.author.id,
        author_is_bot: message.author.bot.unwrap_or(false),
        is_webhook: message.webhook_id.is_some(),
        surface: CandidateSurface::Message {
            mentions_account,
            referenced_message_author_id: referenced_author,
            has_content: !message.content.trim().is_empty(),
        },
    };

    let authorized =
        match evaluate_inbound(context.policy, context.identity, &candidate, context.lookup) {
            Ok(authorized) => authorized,
            Err(reason) => {
                debug_discord_reject(reason, &message.id);
                return None;
            }
        };

    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(META_TRANSPORT.to_string(), TRANSPORT_WEBSOCKET.to_string());
    event_metadata.insert(
        META_ACTIVATION_REASON.to_string(),
        authorized.activation.reason.clone(),
    );
    if let Some(guild_id) = &authorized.scope.workspace_id {
        event_metadata.insert(META_GUILD_ID.to_string(), guild_id.clone());
    }
    if let Some(parent_id) = &authorized.scope.parent_conversation_id {
        event_metadata.insert(META_PARENT_CHANNEL_ID.to_string(), parent_id.clone());
    }
    let mut actor_metadata = BTreeMap::new();
    actor_metadata.insert(META_ACTOR_KIND.to_string(), "user".to_string());

    let reply_to_message_id = reference.and_then(|reference| reference.message_id.clone());

    Some(InboundEventEnvelope {
        event_id: message.id.clone(),
        platform: PLATFORM_DISCORD.to_string(),
        event_type: "message.created".to_string(),
        received_at: message
            .timestamp
            .clone()
            .unwrap_or_else(|| Timestamp::now().to_string()),
        conversation: InboundConversationRef {
            id: authorized.scope.id.clone(),
            kind: authorized.scope.kind.name().to_string(),
            thread_id: match authorized.scope.kind {
                ConversationKind::Thread => Some(authorized.scope.id.clone()),
                _ => None,
            },
            parent_message_id: reply_to_message_id.clone(),
            workspace_id: authorized.scope.workspace_id.clone(),
            parent_conversation_id: authorized.scope.parent_conversation_id.clone(),
        },
        actor: InboundActor {
            id: message.author.id.clone(),
            display_name: message
                .author
                .global_name
                .clone()
                .or_else(|| Some(message.author.username.clone())),
            username: Some(message.author.username.clone()),
            is_bot: false,
            metadata: actor_metadata,
        },
        message: InboundMessage {
            id: message.id.clone(),
            content: message.content.clone(),
            content_type: "text/plain".to_string(),
            reply_to_message_id,
            attachments: message.attachments.iter().map(gateway_attachment).collect(),
            metadata: BTreeMap::new(),
        },
        account_id: Some(context.identity.account_id.clone()),
        activation: Some(authorized.activation),
        metadata: event_metadata,
    })
}

/// Record a content-free rejection so operators can see why nothing was emitted.
fn debug_discord_reject(reason: &str, message_id: &str) {
    debug_discord_gateway(format_args!(
        "dropping gateway event id={message_id} reason={reason}"
    ));
}

fn gateway_attachment(attachment: &DiscordGatewayAttachment) -> InboundAttachment {
    InboundAttachment {
        id: Some(attachment.id.clone()),
        kind: "file".to_string(),
        url: Some(attachment.url.clone()),
        mime_type: attachment.content_type.clone(),
        size_bytes: attachment.size,
        name: Some(attachment.filename.clone()),
        storage_key: None,
        extracted_text: None,
        extras: BTreeMap::new(),
    }
}

fn websocket_state(
    status: &str,
    sequence: Option<u64>,
    session_id: Option<&str>,
    resume_gateway_url: Option<&str>,
    bot_user_id: Option<&str>,
) -> IngressState {
    let mut state = IngressState {
        mode: IngressMode::Websocket,
        status: status.to_string(),
        endpoint: None,
        metadata: websocket_state_metadata(),
    };
    if let Some(bot_user_id) = bot_user_id {
        state
            .metadata
            .insert(META_BOT_USER_ID.to_string(), bot_user_id.to_string());
    }
    if let Some(sequence) = sequence {
        state
            .metadata
            .insert("sequence".to_string(), sequence.to_string());
    }
    if let Some(session_id) = session_id {
        state
            .metadata
            .insert("session_id".to_string(), session_id.to_string());
    }
    if let Some(resume_gateway_url) = resume_gateway_url {
        state.metadata.insert(
            "resume_gateway_url".to_string(),
            resume_gateway_url.to_string(),
        );
    }
    state
}

fn websocket_state_metadata() -> BTreeMap<String, String> {
    BTreeMap::from([
        (META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string()),
        (META_TRANSPORT.to_string(), TRANSPORT_WEBSOCKET.to_string()),
    ])
}

fn discord_websocket_timeout_window() -> Duration {
    std::env::var("DISCORD_GATEWAY_RECEIVE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300))
}

fn configure_websocket_read_timeout(
    stream: &mut MaybeTlsStream<TcpStream>,
    timeout: Duration,
) -> Result<()> {
    let tcp = match stream {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Ok(()),
    };
    tcp.set_read_timeout(Some(timeout))
        .context("failed to configure Discord websocket read timeout")
}

fn discord_gateway_base_url<F>(session: &DiscordGatewaySession, fallback: F) -> Result<String>
where
    F: FnOnce() -> Result<String>,
{
    session
        .resume_gateway_url
        .clone()
        .filter(|_| session.can_resume())
        .map(Ok)
        .unwrap_or_else(fallback)
}

fn discord_gateway_websocket_url(gateway_url: &str) -> String {
    let mut base = gateway_url.to_string();
    if !base.contains('?') && !base.ends_with('/') {
        base.push('/');
    }
    format!(
        "{}{}v={}&encoding=json",
        base,
        if base.contains('?') { "&" } else { "?" },
        DISCORD_GATEWAY_VERSION
    )
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let content = render_status_message(update);
    if content.trim().is_empty() {
        return Ok(rejected_status(
            "missing_message",
            "discord status frames require a message or discord_status_text override",
        ));
    }

    let message = OutboundMessage {
        content,
        attachments: Vec::new(),
        channel_id: update
            .conversation_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_CONVERSATION_ID).cloned()),
        thread_id: update
            .thread_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_THREAD_ID).cloned()),
        reply_to_message_id: update.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned(),
        metadata: BTreeMap::new(),
    };

    let delivery = match deliver(config, &message) {
        Ok(delivery) => delivery,
        Err(error) => {
            // Status frames answer to the same outbound scope as replies, so a
            // rejected destination keeps its own code instead of reading as a
            // provider failure.
            let reason_code = error
                .downcast_ref::<OutboundRejected>()
                .map(|rejected| rejected.code)
                .unwrap_or("delivery_failed");
            return Ok(rejected_status(reason_code, error.to_string()));
        }
    };

    let mut metadata = delivery.metadata.clone();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    metadata.insert(META_STATUS_KIND.to_string(), status_kind_name(&update.kind));
    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let policy = DiscordPolicy::from_config(config)?;
    policy.validate_for_ingress(uses_interaction_webhook(config))?;
    let destination = resolve_destination(config, message)?;
    let client = DiscordClient::from_env(bot_token_env(config))?;
    // Resolve provider shape and authorize before the mutating provider call.
    // Permission to post is the bot account's ceiling, not this binding's grant.
    authorize_outbound_destination(
        &policy,
        &destination,
        &RestChannelLookup { client: &client },
    )?;
    let reply_to_message_id = resolve_reply_to_message_id(message);
    let upload = discord_upload(message)?;
    let posted = client.send_message(
        &destination,
        &message.content,
        reply_to_message_id.as_deref(),
        upload.as_ref(),
    )?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    metadata.insert(META_CHANNEL_ID.to_string(), posted.channel_id.clone());
    metadata.insert(META_RESOLVED_DESTINATION.to_string(), destination.clone());
    if upload.is_some() {
        metadata.insert(META_ATTACHMENT_COUNT.to_string(), "1".to_string());
    }
    if let Some(reply_to_message_id) = &reply_to_message_id {
        metadata.insert(
            META_REPLY_TO_MESSAGE_ID.to_string(),
            reply_to_message_id.clone(),
        );
    }
    if let Some(thread_id) = resolve_thread_id(message) {
        metadata.insert(META_THREAD_ID.to_string(), thread_id.clone());
    }

    Ok(DeliveryReceipt {
        message_ref: None,
        message_id: posted.id,
        conversation_id: posted.channel_id,
        metadata,
    })
}

fn discord_upload(message: &OutboundMessage) -> Result<Option<DiscordUpload>> {
    match message.attachments.as_slice() {
        [] => Ok(None),
        [attachment] => {
            if attachment.url.is_some() {
                bail!(
                    "discord outbound attachments require data_base64; url attachments are not supported"
                );
            }
            if attachment.storage_key.is_some() {
                bail!(
                    "discord outbound attachments require data_base64; storage_key attachments are not supported"
                );
            }
            let Some(data_base64) = attachment.data_base64.as_deref() else {
                bail!("discord outbound attachments require data_base64");
            };
            let data = BASE64_STANDARD.decode(data_base64).with_context(|| {
                format!(
                    "invalid base64 attachment payload for `{}`",
                    attachment.name
                )
            })?;
            Ok(Some(DiscordUpload {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                data,
            }))
        }
        _ => bail!("discord delivery supports at most one attachment"),
    }
}

/// Outcome of normalizing one interaction webhook.
enum InteractionOutcome {
    Event(Box<InboundEventEnvelope>),
    /// Policy denied the interaction. Carries a stable rejection code.
    Rejected(&'static str),
    /// The plugin does not implement this interaction type.
    Unsupported,
}

fn build_inbound_event(
    policy: &DiscordPolicy,
    interaction: &DiscordInteraction,
    payload: &IngressPayload,
) -> InteractionOutcome {
    let Some(event_type) = interaction_type_name(interaction.interaction_type) else {
        return InteractionOutcome::Unsupported;
    };

    // The provider routes an interaction to an application by ID. If that ID is
    // not the application this binding declares, the interaction belongs to a
    // different application even though the signature checked out.
    if let Some(expected) = policy.application_id.as_deref()
        && expected != interaction.application_id
    {
        return InteractionOutcome::Rejected(REJECT_APPLICATION_MISMATCH);
    }

    let Some(actor) = inbound_actor(interaction) else {
        return InteractionOutcome::Rejected(REJECT_SENDER_DENIED);
    };
    let Some(conversation_id) = interaction.channel_id.clone() else {
        return InteractionOutcome::Rejected(REJECT_UNRESOLVED_CONVERSATION);
    };

    // An interaction is explicitly addressed to this application, and it is
    // still confined to the conversations the binding was granted.
    let identity = DiscordBotIdentity {
        account_id: interaction.application_id.clone(),
        application_id: policy.application_id.clone(),
    };
    let candidate = InboundCandidate {
        channel_id: &conversation_id,
        guild_id: interaction.guild_id.as_deref(),
        // Interaction payloads describe their own channel, so parentage never
        // needs a lookup here and an undescribed thread fails closed.
        hint: interaction
            .channel
            .as_ref()
            .map(|channel| ChannelHint {
                kind: channel.channel_type,
                parent_id: channel.parent_id.clone(),
            })
            .unwrap_or_default(),
        author_id: &actor.id,
        author_is_bot: actor.is_bot,
        is_webhook: false,
        surface: CandidateSurface::Interaction,
    };
    let authorized = match evaluate_inbound(policy, &identity, &candidate, &NoChannelLookup) {
        Ok(authorized) => authorized,
        Err(reason) => return InteractionOutcome::Rejected(reason),
    };

    let received_at = received_at(payload.received_at.as_deref());
    let (content, mut message_metadata) = interaction_message(interaction);
    let attachments = extract_source_attachments(interaction, &mut message_metadata);
    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(
        META_TRANSPORT.to_string(),
        TRANSPORT_INTERACTION_WEBHOOK.to_string(),
    );
    event_metadata.insert(META_INTERACTION_TYPE.to_string(), event_type.to_string());
    event_metadata.insert(
        META_APPLICATION_ID.to_string(),
        interaction.application_id.clone(),
    );
    event_metadata.insert(
        META_ACTIVATION_REASON.to_string(),
        authorized.activation.reason.clone(),
    );
    if let Some(parent_id) = &authorized.scope.parent_conversation_id {
        event_metadata.insert(META_PARENT_CHANNEL_ID.to_string(), parent_id.clone());
    }
    if let Some(endpoint_id) = &payload.endpoint_id {
        event_metadata.insert(META_ENDPOINT_ID.to_string(), endpoint_id.clone());
    }
    if !payload.path.is_empty() {
        event_metadata.insert(META_PATH.to_string(), payload.path.clone());
    }
    if let Some(guild_id) = &interaction.guild_id {
        event_metadata.insert(META_GUILD_ID.to_string(), guild_id.clone());
    }
    if let Some(locale) = &interaction.locale {
        event_metadata.insert(META_LOCALE.to_string(), locale.clone());
    }
    if let Some(guild_locale) = &interaction.guild_locale {
        event_metadata.insert(META_GUILD_LOCALE.to_string(), guild_locale.clone());
    }

    let parent_message_id = interaction
        .message
        .as_ref()
        .map(|message| message.id.clone());
    if let Some(source_message_id) = &parent_message_id {
        message_metadata.insert(
            META_SOURCE_MESSAGE_ID.to_string(),
            source_message_id.clone(),
        );
    }

    InteractionOutcome::Event(Box::new(InboundEventEnvelope {
        event_id: interaction.id.clone(),
        platform: PLATFORM_DISCORD.to_string(),
        event_type: event_type.to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: authorized.scope.id.clone(),
            kind: authorized.scope.kind.name().to_string(),
            thread_id: match authorized.scope.kind {
                ConversationKind::Thread => Some(authorized.scope.id.clone()),
                _ => None,
            },
            parent_message_id: parent_message_id.clone(),
            workspace_id: authorized.scope.workspace_id.clone(),
            parent_conversation_id: authorized.scope.parent_conversation_id.clone(),
        },
        actor,
        message: InboundMessage {
            id: source_message_id_or_interaction_id(interaction),
            content,
            content_type: "text/plain".to_string(),
            reply_to_message_id: parent_message_id,
            attachments,
            metadata: message_metadata,
        },
        account_id: Some(interaction.application_id.clone()),
        activation: Some(authorized.activation),
        metadata: event_metadata,
    }))
}

fn extract_source_attachments(
    interaction: &DiscordInteraction,
    message_metadata: &mut BTreeMap<String, String>,
) -> Vec<InboundAttachment> {
    let Some(source_message) = interaction.message.as_ref() else {
        return Vec::new();
    };

    let mut attachments = Vec::new();
    for attachment in &source_message.attachments {
        attachments.push(InboundAttachment {
            id: Some(attachment.id.clone()),
            kind: attachment_kind(attachment.content_type.as_deref()),
            url: Some(attachment.url.clone()),
            mime_type: attachment.content_type.clone(),
            size_bytes: attachment.size,
            name: Some(attachment.filename.clone()),
            storage_key: None,
            extracted_text: attachment.description.clone(),
            extras: BTreeMap::new(),
        });
    }

    if !attachments.is_empty() {
        message_metadata.insert(
            META_ATTACHMENT_COUNT.to_string(),
            attachments.len().to_string(),
        );
    }

    attachments
}

fn attachment_kind(content_type: Option<&str>) -> String {
    match content_type.and_then(|value| value.split('/').next()) {
        Some("image") => "image".to_string(),
        Some("audio") => "audio".to_string(),
        Some("video") => "video".to_string(),
        _ => "file".to_string(),
    }
}

fn interaction_message(interaction: &DiscordInteraction) -> (String, BTreeMap<String, String>) {
    let mut metadata = BTreeMap::new();
    let Some(data) = interaction.data.as_ref() else {
        return (String::new(), metadata);
    };

    match interaction.interaction_type {
        INTERACTION_TYPE_APPLICATION_COMMAND => {
            if let Some(name) = &data.name {
                metadata.insert(META_COMMAND_NAME.to_string(), name.clone());
            }
            if let Some(command_type) = data.command_type {
                metadata.insert(
                    META_COMMAND_KIND.to_string(),
                    command_kind_name(command_type).to_string(),
                );
            }
            let content = render_command_content(data);
            (content, metadata)
        }
        INTERACTION_TYPE_MESSAGE_COMPONENT => {
            if let Some(custom_id) = &data.custom_id {
                metadata.insert(META_CUSTOM_ID.to_string(), custom_id.clone());
            }
            if let Some(component_type) = data.component_type {
                metadata.insert(
                    META_COMPONENT_TYPE.to_string(),
                    component_type_name(component_type).to_string(),
                );
            }
            let content = render_component_content(data);
            (content, metadata)
        }
        INTERACTION_TYPE_MODAL_SUBMIT => {
            if let Some(custom_id) = &data.custom_id {
                metadata.insert(META_CUSTOM_ID.to_string(), custom_id.clone());
            }
            let content = render_modal_content(data);
            (content, metadata)
        }
        _ => (String::new(), metadata),
    }
}

fn render_command_content(data: &DiscordInteractionData) -> String {
    let Some(name) = data.name.as_deref() else {
        return "/command".to_string();
    };
    let mut rendered = format!("/{name}");
    let options = render_command_options(&data.options);
    if !options.is_empty() {
        rendered.push(' ');
        rendered.push_str(&options.join(" "));
    }
    rendered
}

fn render_command_options(options: &[DiscordCommandOption]) -> Vec<String> {
    let mut rendered = Vec::new();
    for option in options {
        if option.options.is_empty() {
            if let Some(value) = &option.value {
                rendered.push(format!("{}={}", option.name, render_json_scalar(value)));
            } else {
                rendered.push(option.name.clone());
            }
            continue;
        }

        rendered.push(option.name.clone());
        rendered.extend(render_command_options(&option.options));
    }
    rendered
}

fn render_component_content(data: &DiscordInteractionData) -> String {
    let custom_id = data.custom_id.as_deref().unwrap_or("component");
    if !data.values.is_empty() {
        return format!("component {custom_id}: {}", data.values.join(", "));
    }
    format!("component {custom_id}")
}

fn render_modal_content(data: &DiscordInteractionData) -> String {
    let custom_id = data.custom_id.as_deref().unwrap_or("modal");
    let values = collect_modal_values(&data.components);
    if values.is_empty() {
        return format!("modal {custom_id}");
    }
    format!("modal {custom_id}: {}", values.join(", "))
}

fn collect_modal_values(rows: &[DiscordComponentRow]) -> Vec<String> {
    let mut values = Vec::new();
    for row in rows {
        values.extend(collect_component_values(&row.components));
    }
    values
}

fn collect_component_values(components: &[DiscordComponentValue]) -> Vec<String> {
    let mut values = Vec::new();
    for component in components {
        if let Some(value) = &component.value {
            if let Some(custom_id) = &component.custom_id {
                values.push(format!("{custom_id}={value}"));
            } else {
                values.push(value.clone());
            }
        }
        if !component.values.is_empty() {
            if let Some(custom_id) = &component.custom_id {
                values.push(format!("{custom_id}={}", component.values.join("|")));
            } else {
                values.extend(component.values.clone());
            }
        }
        if !component.components.is_empty() {
            values.extend(collect_component_values(&component.components));
        }
    }
    values
}

fn render_json_scalar(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn inbound_actor(interaction: &DiscordInteraction) -> Option<InboundActor> {
    if let Some(member) = &interaction.member
        && let Some(user) = &member.user
    {
        let mut metadata = BTreeMap::new();
        metadata.insert(META_ACTOR_KIND.to_string(), "member".to_string());
        if let Some(nick) = &member.nick {
            metadata.insert(META_NICK.to_string(), nick.clone());
        }
        return Some(InboundActor {
            id: user.id.clone(),
            display_name: member
                .nick
                .clone()
                .or_else(|| user.global_name.clone())
                .or_else(|| Some(user.username.clone())),
            username: Some(user.username.clone()),
            is_bot: user.bot.unwrap_or(false),
            metadata,
        });
    }

    interaction.user.as_ref().map(|user| InboundActor {
        id: user.id.clone(),
        display_name: user
            .global_name
            .clone()
            .or_else(|| Some(user.username.clone())),
        username: Some(user.username.clone()),
        is_bot: user.bot.unwrap_or(false),
        metadata: BTreeMap::from([(META_ACTOR_KIND.to_string(), "user".to_string())]),
    })
}

fn validate_ingress_signature(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let public_key = read_required_env(interaction_public_key_env(config))?;
    validate_discord_signature(&public_key, payload)
}

fn validate_discord_signature(
    public_key_hex: &str,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    let Some(signature_hex) = header_value(&payload.headers, HEADER_X_SIGNATURE_ED25519) else {
        return Ok(Some(callback_reply(
            401,
            "discord request signature header missing",
        )));
    };
    let Some(timestamp) = header_value(&payload.headers, HEADER_X_SIGNATURE_TIMESTAMP) else {
        return Ok(Some(callback_reply(
            401,
            "discord request timestamp header missing",
        )));
    };

    // Reject signatures whose timestamp is too far from now. Discord sends
    // Unix epoch seconds; treat unparseable values as untrusted.
    let Ok(signature_ts) = timestamp.parse::<i64>() else {
        return Ok(Some(callback_reply(
            401,
            "discord request timestamp is not a valid unix epoch",
        )));
    };
    let now = Timestamp::now().as_second();
    if now.abs_diff(signature_ts) > DISCORD_MAX_SIGNATURE_AGE_SECS as u64 {
        return Ok(Some(callback_reply(
            401,
            "discord request timestamp outside the accepted window",
        )));
    }

    let public_key_bytes =
        hex::decode(public_key_hex).context("invalid Discord interaction public key")?;
    let public_key_bytes: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| anyhow!("Discord interaction public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .context("invalid Discord interaction public key bytes")?;

    let signature_bytes =
        hex::decode(signature_hex).context("invalid X-Signature-Ed25519 header")?;
    let signature = Signature::try_from(signature_bytes.as_slice())
        .map_err(|_| anyhow!("invalid Discord interaction signature bytes"))?;

    let signed_message = format!("{timestamp}{}", payload.body);
    if verifying_key
        .verify(signed_message.as_bytes(), &signature)
        .is_err()
    {
        return Ok(Some(callback_reply(401, "invalid request signature")));
    }

    Ok(None)
}

/// Rejection of an outbound destination, carrying a stable code for telemetry.
#[derive(Debug)]
struct OutboundRejected {
    code: &'static str,
    reason: String,
}

impl std::fmt::Display for OutboundRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.reason)
    }
}

impl std::error::Error for OutboundRejected {}

fn outbound_rejected(code: &'static str, reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(OutboundRejected {
        code,
        reason: reason.into(),
    })
}

/// Resolve the destination a caller asked for.
///
/// A caller-supplied ID selects among destinations; it does not authorize one.
/// `default_channel_id` is the last fallback and is authorized like any other
/// destination.
fn resolve_destination(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
    if let Some(thread_id) = resolve_thread_id(message) {
        return Ok(thread_id.clone());
    }
    if let Some(channel_id) = &message.channel_id {
        return Ok(channel_id.clone());
    }
    if let Some(channel_id) = message.metadata.get(ROUTE_CONVERSATION_ID) {
        return Ok(channel_id.clone());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        return Ok(default_channel_id.clone());
    }
    Err(outbound_rejected(
        REJECT_MISSING_DESTINATION,
        "discord delivery requires message.thread_id, message.channel_id, or config.default_channel_id",
    ))
}

/// Confirm the resolved destination is inside the binding's outbound scope.
///
/// `Deliver`, `Push`, and `Status` all reach delivery through here, so one
/// destination rule covers replies, proactive sends, and status frames.
fn authorize_outbound_destination(
    policy: &DiscordPolicy,
    destination: &str,
    lookup: &dyn DiscordChannelLookup,
) -> Result<()> {
    let resolved = lookup.channel(destination).map_err(|error| {
        outbound_rejected(
            REJECT_UNAUTHORIZED_DESTINATION,
            format!("discord destination {destination} could not be resolved: {error}"),
        )
    })?;
    if resolved.kind == CHANNEL_TYPE_DM && resolved.guild_id.is_none() {
        let allowed = match policy.dm_policy {
            DmPolicy::Deny => false,
            DmPolicy::Open => true,
            DmPolicy::Allowlist => {
                !resolved.recipient_ids.is_empty()
                    && resolved.recipient_ids.iter().all(|recipient_id| {
                        policy
                            .allowed_dm_sender_ids
                            .iter()
                            .any(|allowed| allowed == recipient_id)
                    })
            }
        };
        return allowed.then_some(()).ok_or_else(|| {
            outbound_rejected(
                REJECT_UNAUTHORIZED_DESTINATION,
                format!("discord direct-message destination {destination} is outside this binding's dm policy"),
            )
        });
    }
    let resolved_guild_is_allowed = resolved
        .guild_id
        .as_deref()
        .is_some_and(|guild_id| policy.guild_is_allowed(guild_id));
    if policy.outbound_channel_is_allowed(destination)
        && !is_thread_channel_type(resolved.kind)
        && resolved_guild_is_allowed
    {
        return Ok(());
    }

    // A destination outside the channel list is reachable only as a child
    // thread, and only when the thread policy grants it.
    match policy.thread_policy {
        ThreadPolicy::Deny => Err(outbound_rejected(
            REJECT_UNAUTHORIZED_DESTINATION,
            format!("discord destination {destination} is outside this binding's outbound scope"),
        )),
        ThreadPolicy::Allowlist => {
            if !policy.thread_is_listed(destination) {
                return Err(outbound_rejected(
                    REJECT_UNAUTHORIZED_DESTINATION,
                    format!(
                        "discord thread {destination} is not in this binding's thread allowlist"
                    ),
                ));
            }
            let parent_is_allowed = is_thread_channel_type(resolved.kind)
                && resolved_guild_is_allowed
                && resolved
                    .parent_id
                    .as_deref()
                    .is_some_and(|parent_id| policy.outbound_channel_is_allowed(parent_id));
            if parent_is_allowed {
                Ok(())
            } else {
                Err(outbound_rejected(
                    REJECT_UNAUTHORIZED_DESTINATION,
                    format!(
                        "discord thread {destination} does not descend from an allowed outbound channel"
                    ),
                ))
            }
        }
        ThreadPolicy::InheritParent => {
            let parent_is_allowed = is_thread_channel_type(resolved.kind)
                && resolved_guild_is_allowed
                && resolved
                    .parent_id
                    .as_deref()
                    .is_some_and(|parent_id| policy.outbound_channel_is_allowed(parent_id));
            if parent_is_allowed {
                Ok(())
            } else {
                Err(outbound_rejected(
                    REJECT_UNAUTHORIZED_DESTINATION,
                    format!(
                        "discord destination {destination} does not descend from an allowed outbound channel"
                    ),
                ))
            }
        }
    }
}

fn resolve_thread_id(message: &OutboundMessage) -> Option<&String> {
    message
        .thread_id
        .as_ref()
        .or_else(|| message.metadata.get(ROUTE_THREAD_ID))
}

fn resolve_reply_to_message_id(message: &OutboundMessage) -> Option<String> {
    message
        .reply_to_message_id
        .clone()
        .or_else(|| message.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned())
}

fn resolved_endpoint(config: &ChannelConfig) -> Option<String> {
    let base = config.webhook_public_url.as_deref()?.trim_end_matches('/');
    let path = config
        .webhook_path
        .as_deref()
        .unwrap_or("/discord/interactions")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn bot_token_env(config: &ChannelConfig) -> &str {
    config
        .bot_token_env
        .as_deref()
        .unwrap_or("DISCORD_BOT_TOKEN")
}

fn interaction_public_key_env(config: &ChannelConfig) -> &str {
    config
        .interaction_public_key_env
        .as_deref()
        .unwrap_or("DISCORD_INTERACTION_PUBLIC_KEY")
}

fn source_message_id_or_interaction_id(interaction: &DiscordInteraction) -> String {
    interaction
        .message
        .as_ref()
        .map(|message| message.id.clone())
        .unwrap_or_else(|| interaction.id.clone())
}

fn received_at(host_received_at: Option<&str>) -> String {
    host_received_at
        .map(str::to_owned)
        .unwrap_or_else(|| Timestamp::now().to_string())
}

fn ingress_rejection(status: u16, message: &str) -> PluginResponse {
    PluginResponse::IngressEventsReceived {
        events: Vec::new(),
        callback_reply: Some(callback_reply(status, message)),
        state: None,
        poll_after_ms: None,
    }
}

fn callback_reply(status: u16, message: &str) -> IngressCallbackReply {
    IngressCallbackReply {
        status,
        content_type: Some("text/plain; charset=utf-8".to_string()),
        body: message.to_string(),
    }
}

fn discord_json_reply(body: Value) -> IngressCallbackReply {
    IngressCallbackReply {
        status: 200,
        content_type: Some("application/json".to_string()),
        body: body.to_string(),
    }
}

fn discord_ephemeral_message(message: &str) -> IngressCallbackReply {
    discord_json_reply(json!({
        "type": 4,
        "data": {
            "content": message,
            "flags": 64
        }
    }))
}

fn render_status_message(update: &StatusFrame) -> String {
    if let Some(status_text) = update.metadata.get(DISCORD_STATUS_TEXT) {
        return status_text.clone();
    }

    let prefix = match update.kind {
        StatusKind::Processing => "Processing",
        StatusKind::Completed => "Completed",
        StatusKind::Cancelled => "Cancelled",
        StatusKind::OperationStarted => "Started",
        StatusKind::OperationFinished => "Finished",
        StatusKind::ApprovalNeeded => "Approval needed",
        StatusKind::Info => "Info",
        StatusKind::Delivering => "Delivering",
        StatusKind::AuthRequired => "Authentication required",
        _ => "Status",
    };

    if update.message.trim().is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {}", update.message)
    }
}

fn interaction_type_name(interaction_type: u8) -> Option<&'static str> {
    match interaction_type {
        INTERACTION_TYPE_APPLICATION_COMMAND => Some("application_command"),
        INTERACTION_TYPE_MESSAGE_COMPONENT => Some("message_component"),
        INTERACTION_TYPE_MODAL_SUBMIT => Some("modal_submit"),
        _ => None,
    }
}

fn command_kind_name(command_type: u8) -> &'static str {
    match command_type {
        COMMAND_KIND_CHAT_INPUT => "chat_input",
        COMMAND_KIND_USER => "user",
        COMMAND_KIND_MESSAGE => "message",
        _ => "unknown",
    }
}

fn component_type_name(component_type: u8) -> &'static str {
    match component_type {
        COMPONENT_KIND_BUTTON => "button",
        COMPONENT_KIND_STRING_SELECT => "string_select",
        COMPONENT_KIND_TEXT_INPUT => "text_input",
        COMPONENT_KIND_USER_SELECT => "user_select",
        COMPONENT_KIND_ROLE_SELECT => "role_select",
        COMPONENT_KIND_MENTIONABLE_SELECT => "mentionable_select",
        COMPONENT_KIND_CHANNEL_SELECT => "channel_select",
        _ => "unknown",
    }
}

fn ingress_mode_name(mode: IngressMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "interaction_webhook".to_string())
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn rejected_status(reason_code: &str, reason: impl Into<String>) -> StatusAcceptance {
    StatusAcceptance {
        accepted: false,
        metadata: BTreeMap::from([
            (META_REASON_CODE.to_string(), reason_code.to_string()),
            (META_REASON.to_string(), reason.into()),
        ]),
    }
}

fn status_kind_name(kind: &StatusKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn read_required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required for the discord channel"))
}

#[derive(Debug, Deserialize)]
struct DiscordInteraction {
    id: String,
    application_id: String,
    #[serde(rename = "type")]
    interaction_type: u8,
    #[serde(default)]
    data: Option<DiscordInteractionData>,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    /// Partial channel object. Carries the type and thread parent, so an
    /// interaction proves its own conversation shape without a lookup.
    #[serde(default)]
    channel: Option<DiscordInteractionChannel>,
    #[serde(default)]
    member: Option<DiscordInteractionMember>,
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    message: Option<DiscordSourceMessage>,
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    guild_locale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionData {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    command_type: Option<u8>,
    #[serde(default)]
    custom_id: Option<String>,
    #[serde(default)]
    component_type: Option<u8>,
    #[serde(default)]
    options: Vec<DiscordCommandOption>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    components: Vec<DiscordComponentRow>,
}

#[derive(Debug, Deserialize)]
struct DiscordCommandOption {
    name: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    options: Vec<DiscordCommandOption>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionChannel {
    #[serde(default, rename = "type")]
    channel_type: Option<u8>,
    #[serde(default)]
    parent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionMember {
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    nick: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    bot: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DiscordSourceMessage {
    id: String,
    #[serde(default)]
    attachments: Vec<DiscordSourceAttachment>,
}

#[derive(Debug, Deserialize)]
struct DiscordSourceAttachment {
    id: String,
    filename: String,
    url: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayPayload {
    op: u64,
    #[serde(default, rename = "s")]
    sequence: Option<u64>,
    #[serde(default, rename = "t")]
    event_type: Option<String>,
    #[serde(default, rename = "d")]
    data: Value,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayMessage {
    id: String,
    channel_id: String,
    content: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    guild_id: Option<String>,
    author: DiscordUser,
    /// Set when the provider delivered the message through a webhook rather
    /// than a member account.
    #[serde(default)]
    webhook_id: Option<String>,
    /// Discord message type. Separates member messages from provider notices.
    #[serde(default, rename = "type")]
    message_type: Option<u8>,
    #[serde(default)]
    flags: Option<u64>,
    /// Accounts the provider resolved as mentioned. Role and everyone mentions
    /// are reported in separate fields and never place an account here.
    #[serde(default)]
    mentions: Vec<DiscordUser>,
    #[serde(default)]
    attachments: Vec<DiscordGatewayAttachment>,
    #[serde(default)]
    message_reference: Option<DiscordGatewayMessageReference>,
    /// Message this one references, resolved by the provider. Absent when the
    /// reference could not be resolved, which leaves the target author unknown.
    #[serde(default)]
    referenced_message: Option<DiscordReferencedMessage>,
}

#[derive(Debug, Deserialize)]
struct DiscordReferencedMessage {
    #[serde(default)]
    author: Option<DiscordUser>,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayAttachment {
    id: String,
    filename: String,
    url: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DiscordGatewayMessageReference {
    #[serde(default)]
    message_id: Option<String>,
    /// Reference kind. A forward points at a message that was not addressed to
    /// the referenced author, so it is not reply evidence.
    #[serde(default, rename = "type")]
    reference_type: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct DiscordComponentRow {
    #[serde(default)]
    components: Vec<DiscordComponentValue>,
}

#[derive(Debug, Deserialize)]
struct DiscordComponentValue {
    #[serde(default)]
    custom_id: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    components: Vec<DiscordComponentValue>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::OutboundAttachment;
    use ed25519_dalek::{Signer, SigningKey};

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    /// Account this binding authenticated as. Every mention and reply check
    /// compares against this ID rather than any display text.
    const BOT_ACCOUNT_ID: &str = "bot-account-1";
    const ALLOWED_GUILD: &str = "guild-marketing";
    const ALLOWED_CHANNEL: &str = "channel-marketing";
    const OTHER_CHANNEL: &str = "channel-team-chat";
    const ALLOWED_THREAD: &str = "thread-in-marketing";
    const OTHER_THREAD: &str = "thread-in-team-chat";
    const MEMBER_ID: &str = "member-1";

    /// One guild, one channel, mention-or-reply activation, DMs denied.
    fn scoped_config() -> ChannelConfig {
        ChannelConfig {
            allowed_guild_ids: vec![ALLOWED_GUILD.to_string()],
            allowed_channel_ids: vec![ALLOWED_CHANNEL.to_string()],
            ..ChannelConfig::default()
        }
    }

    fn bot_identity() -> DiscordBotIdentity {
        DiscordBotIdentity {
            account_id: BOT_ACCOUNT_ID.to_string(),
            application_id: None,
        }
    }

    fn discord_user(id: &str, bot: bool) -> DiscordUser {
        DiscordUser {
            id: id.to_string(),
            username: "member".to_string(),
            global_name: None,
            bot: Some(bot),
        }
    }

    /// Channel shapes a test provider would report, so thread parentage can be
    /// exercised without reaching the network.
    struct MapChannelLookup {
        channels: BTreeMap<String, DiscordChannelInfo>,
    }

    impl MapChannelLookup {
        fn channel(channel_id: &str, guild_id: &str) -> Self {
            Self {
                channels: BTreeMap::from([(
                    channel_id.to_string(),
                    DiscordChannelInfo {
                        kind: 0,
                        parent_id: None,
                        guild_id: Some(guild_id.to_string()),
                        recipient_ids: Vec::new(),
                    },
                )]),
            }
        }

        fn thread(thread_id: &str, parent_id: &str, guild_id: &str) -> Self {
            Self {
                channels: BTreeMap::from([(
                    thread_id.to_string(),
                    DiscordChannelInfo {
                        kind: CHANNEL_TYPE_PUBLIC_THREAD,
                        parent_id: Some(parent_id.to_string()),
                        guild_id: Some(guild_id.to_string()),
                        recipient_ids: Vec::new(),
                    },
                )]),
            }
        }

        fn direct_message(channel_id: &str, recipient_id: &str) -> Self {
            Self {
                channels: BTreeMap::from([(
                    channel_id.to_string(),
                    DiscordChannelInfo {
                        kind: 1,
                        parent_id: None,
                        guild_id: None,
                        recipient_ids: vec![recipient_id.to_string()],
                    },
                )]),
            }
        }
    }

    impl DiscordChannelLookup for MapChannelLookup {
        fn channel(&self, channel_id: &str) -> Result<DiscordChannelInfo> {
            self.channels
                .get(channel_id)
                .cloned()
                .ok_or_else(|| anyhow!("no test channel {channel_id}"))
        }
    }

    fn guild_message(channel_id: &str, content: &str) -> DiscordGatewayMessage {
        DiscordGatewayMessage {
            id: "msg-1".to_string(),
            channel_id: channel_id.to_string(),
            content: content.to_string(),
            timestamp: Some("2026-05-04T00:00:00Z".to_string()),
            guild_id: Some(ALLOWED_GUILD.to_string()),
            author: discord_user(MEMBER_ID, false),
            webhook_id: None,
            message_type: Some(MESSAGE_TYPE_DEFAULT),
            flags: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            message_reference: None,
            referenced_message: None,
        }
    }

    fn direct_message(content: &str) -> DiscordGatewayMessage {
        DiscordGatewayMessage {
            guild_id: None,
            channel_id: "dm-channel-1".to_string(),
            ..guild_message("dm-channel-1", content)
        }
    }

    /// Mark a message as a reply whose target the provider resolved.
    fn replying_to(mut message: DiscordGatewayMessage, author_id: &str) -> DiscordGatewayMessage {
        message.message_type = Some(MESSAGE_TYPE_REPLY);
        message.message_reference = Some(DiscordGatewayMessageReference {
            message_id: Some("parent-message-1".to_string()),
            reference_type: Some(MESSAGE_REFERENCE_TYPE_DEFAULT),
        });
        message.referenced_message = Some(DiscordReferencedMessage {
            author: Some(discord_user(author_id, author_id == BOT_ACCOUNT_ID)),
        });
        message
    }

    fn emit_with_lookup(
        config: &ChannelConfig,
        message: &DiscordGatewayMessage,
        lookup: &dyn DiscordChannelLookup,
    ) -> Option<InboundEventEnvelope> {
        let policy = DiscordPolicy::from_config(config).expect("policy");
        let identity = bot_identity();
        let context = DiscordIngressContext {
            config,
            policy: &policy,
            identity: &identity,
            lookup,
        };
        build_websocket_inbound_event(&context, message)
    }

    /// Emit with the provider shape that a normal guild message lookup returns.
    fn emit(
        config: &ChannelConfig,
        message: &DiscordGatewayMessage,
    ) -> Option<InboundEventEnvelope> {
        let lookup = message.guild_id.as_deref().map_or_else(
            || MapChannelLookup {
                channels: BTreeMap::new(),
            },
            |guild_id| MapChannelLookup::channel(&message.channel_id, guild_id),
        );
        emit_with_lookup(config, message, &lookup)
    }

    fn interaction_events(config: &ChannelConfig, body: &str) -> Vec<InboundEventEnvelope> {
        let payload = base_payload(body);
        match handle_ingress_event(config, &payload).expect("handle ingress") {
            PluginResponse::IngressEventsReceived { events, .. } => events,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn base_payload(body: &str) -> IngressPayload {
        IngressPayload {
            endpoint_id: Some("discord:/discord/interactions".to_string()),
            method: "POST".to_string(),
            path: "/discord/interactions".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: body.to_string(),
            trust_verified: true,
            received_at: Some("2026-04-11T21:00:00Z".to_string()),
        }
    }

    #[test]
    fn ping_returns_pong_callback() {
        let payload = base_payload(r#"{"id":"1","application_id":"app-1","type":1}"#);

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(events.is_empty());
                let reply = callback_reply.expect("pong reply");
                assert_eq!(reply.status, 200);
                assert_eq!(reply.content_type.as_deref(), Some("application/json"));
                assert_eq!(reply.body, r#"{"type":1}"#);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn gateway_heartbeat_action_emits_an_empty_liveness_notification() {
        let session = DiscordGatewaySession::from_state(None);

        let response = discord_gateway_action_response(DiscordGatewayAction::Heartbeat, &session);

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                state,
                poll_after_ms,
                ..
            } => {
                assert!(events.is_empty());
                assert_eq!(
                    state.as_ref().map(|state| state.status.as_str()),
                    Some("running")
                );
                assert_eq!(poll_after_ms, Some(0));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn reconnect_history_requires_time_in_a_healthy_gateway_session() {
        assert!(!discord_session_was_stable(None));
        assert!(!discord_session_was_stable(Some(Instant::now())));
        assert!(discord_session_was_stable(Some(
            Instant::now() - DISCORD_SESSION_STABILITY_WINDOW
        )));
    }

    #[test]
    fn command_interaction_maps_to_inbound_event() {
        let payload = base_payload(
            r#"{
                "id":"interaction-1",
                "application_id":"app-1",
                "type":2,
                "guild_id":"guild-marketing",
                "channel_id":"channel-marketing",
                "channel":{"id":"channel-marketing","type":0},
                "locale":"en-US",
                "guild_locale":"en-US",
                "member":{
                    "nick":"Dispatch User",
                    "user":{
                        "id":"user-1",
                        "username":"dispatch-user",
                        "global_name":"Dispatch User"
                    }
                },
                "data":{
                    "name":"ask",
                    "type":1,
                    "options":[
                        {
                            "name":"query",
                            "value":"hello world"
                        }
                    ]
                }
            }"#,
        );

        let response = handle_ingress_event(&scoped_config(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert_eq!(events.len(), 1);
                let reply = callback_reply.expect("interaction ack");
                assert_eq!(reply.status, 200);
                assert!(reply.body.contains("Dispatch is processing your request."));

                let event = &events[0];
                assert_eq!(event.event_id, "interaction-1");
                assert_eq!(event.platform, PLATFORM_DISCORD);
                assert_eq!(event.event_type, "application_command");
                assert_eq!(event.account_id.as_deref(), Some("app-1"));
                assert_eq!(event.conversation.id, ALLOWED_CHANNEL);
                assert_eq!(event.conversation.kind, CONVERSATION_KIND_CHANNEL);
                assert_eq!(
                    event.conversation.workspace_id.as_deref(),
                    Some(ALLOWED_GUILD)
                );
                let activation = event.activation.as_ref().expect("activation provenance");
                assert_eq!(activation.reason, InboundActivation::REASON_SLASH_COMMAND);
                assert_eq!(activation.agent_account_id.as_deref(), Some("app-1"));
                assert_eq!(event.actor.id, "user-1");
                assert_eq!(event.actor.display_name.as_deref(), Some("Dispatch User"));
                assert_eq!(event.message.id, "interaction-1");
                assert_eq!(event.message.content, "/ask query=hello world");
                assert_eq!(
                    event.metadata.get(META_TRANSPORT).map(String::as_str),
                    Some(TRANSPORT_INTERACTION_WEBHOOK)
                );
                assert_eq!(
                    event.metadata.get(META_ENDPOINT_ID).map(String::as_str),
                    Some("discord:/discord/interactions")
                );
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_COMMAND_NAME)
                        .map(String::as_str),
                    Some("ask")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Gateway ingress: activation
    // -------------------------------------------------------------------------

    #[test]
    fn websocket_message_maps_to_inbound_event_when_it_mentions_the_bot_in_an_allowed_channel() {
        let mut message = guild_message(ALLOWED_CHANNEL, "hello websocket");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        let event = emit(&scoped_config(), &message).expect("addressed message should map");

        assert_eq!(event.event_id, "msg-1");
        assert_eq!(event.event_type, "message.created");
        assert_eq!(event.message.content, "hello websocket");
        assert_eq!(event.conversation.id, ALLOWED_CHANNEL);
        assert_eq!(event.conversation.kind, CONVERSATION_KIND_CHANNEL);
        assert_eq!(
            event.conversation.workspace_id.as_deref(),
            Some(ALLOWED_GUILD)
        );
        assert_eq!(event.conversation.parent_conversation_id, None);
        assert_eq!(event.account_id.as_deref(), Some(BOT_ACCOUNT_ID));
        let activation = event.activation.as_ref().expect("activation provenance");
        assert_eq!(activation.reason, InboundActivation::REASON_DIRECT_MENTION);
        assert_eq!(activation.agent_account_id.as_deref(), Some(BOT_ACCOUNT_ID));
        assert_eq!(
            event.metadata.get(META_TRANSPORT).map(String::as_str),
            Some(TRANSPORT_WEBSOCKET)
        );
    }

    #[test]
    fn websocket_mention_in_another_channel_emits_no_event() {
        let mut message = guild_message(OTHER_CHANNEL, "hey");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        // Channel scope is checked before activation, so naming the bot in an
        // unauthorized channel does not reach the activation rules at all.
        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_mention_of_another_member_emits_no_event() {
        let mut message = guild_message(ALLOWED_CHANNEL, "please look at this");
        message.mentions = vec![discord_user("member-2", false)];

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_unaddressed_message_in_an_allowed_channel_emits_no_event() {
        let message = guild_message(ALLOWED_CHANNEL, "morning everyone");

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_display_name_text_without_a_mention_object_emits_no_event() {
        // Any member can type this text. Only the provider-resolved mention list
        // names an account.
        let message = guild_message(ALLOWED_CHANNEL, "@Piper Leeds can you help");

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_reply_to_a_bot_authored_message_emits_one_event() {
        let message = replying_to(guild_message(ALLOWED_CHANNEL, "thanks"), BOT_ACCOUNT_ID);

        let event = emit(&scoped_config(), &message).expect("verified reply should map");

        let activation = event.activation.as_ref().expect("activation provenance");
        assert_eq!(activation.reason, InboundActivation::REASON_REPLY_TO_AGENT);
        assert_eq!(
            activation.referenced_message_author_id.as_deref(),
            Some(BOT_ACCOUNT_ID)
        );
        assert_eq!(
            event.message.reply_to_message_id.as_deref(),
            Some("parent-message-1")
        );
    }

    #[test]
    fn websocket_reply_to_another_member_emits_no_event() {
        let message = replying_to(guild_message(ALLOWED_CHANNEL, "agreed"), "member-2");

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_bare_message_reference_without_a_resolved_author_emits_no_event() {
        let mut message = guild_message(ALLOWED_CHANNEL, "agreed");
        message.message_type = Some(MESSAGE_TYPE_REPLY);
        message.message_reference = Some(DiscordGatewayMessageReference {
            message_id: Some("parent-message-1".to_string()),
            reference_type: Some(MESSAGE_REFERENCE_TYPE_DEFAULT),
        });

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_forwarded_message_is_not_reply_activation() {
        let mut message = replying_to(guild_message(ALLOWED_CHANNEL, "look"), BOT_ACCOUNT_ID);
        message.message_reference = Some(DiscordGatewayMessageReference {
            message_id: Some("parent-message-1".to_string()),
            reference_type: Some(MESSAGE_REFERENCE_TYPE_DEFAULT + 1),
        });

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_empty_guild_message_emits_no_event_under_mention_or_reply() {
        // Absent message text is not addressing evidence, so a message that
        // names nobody stays unaddressed under mention-or-reply.
        let message = guild_message(ALLOWED_CHANNEL, "");

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_empty_message_with_a_verified_mention_emits_no_event() {
        let mut message = guild_message(ALLOWED_CHANNEL, "");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_authorization_is_identical_with_and_without_message_content() {
        // Addressing and readable payload are separate requirements. A mention
        // without readable content is not enough to start a workload.
        let mut with_content = guild_message(ALLOWED_CHANNEL, "hello there");
        with_content.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        let mut without_content = guild_message(ALLOWED_CHANNEL, "");
        without_content.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        let config = scoped_config();
        assert!(emit(&config, &with_content).is_some());
        assert!(emit(&config, &without_content).is_none());

        let unaddressed_with_content = guild_message(ALLOWED_CHANNEL, "hello there");
        let unaddressed_without_content = guild_message(ALLOWED_CHANNEL, "");
        assert!(emit(&config, &unaddressed_with_content).is_none());
        assert!(emit(&config, &unaddressed_without_content).is_none());
    }

    #[test]
    fn websocket_empty_message_with_an_attachment_emits_no_event() {
        // The configuration contract has no readable-attachment activation
        // policy, so an attachment cannot start a workload on its own.
        let mut message = guild_message(ALLOWED_CHANNEL, "");
        message.attachments = vec![DiscordGatewayAttachment {
            id: "attachment-1".to_string(),
            filename: "design.png".to_string(),
            url: "https://cdn.discordapp.com/attachments/design.png".to_string(),
            content_type: Some("image/png".to_string()),
            size: Some(1024),
        }];

        assert!(emit(&scoped_config(), &message).is_none());

        let dedicated = ChannelConfig {
            activation: Some(ACTIVATION_ALL_MESSAGES.to_string()),
            ..scoped_config()
        };
        assert!(emit(&dedicated, &message).is_none());
    }

    #[test]
    fn websocket_bot_webhook_and_self_authored_events_cannot_loop() {
        let config = scoped_config();

        let mut bot_authored = guild_message(ALLOWED_CHANNEL, "digest");
        bot_authored.author = discord_user("other-bot", true);
        bot_authored.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        assert!(emit(&config, &bot_authored).is_none());

        let mut webhook_authored = guild_message(ALLOWED_CHANNEL, "digest");
        webhook_authored.webhook_id = Some("webhook-1".to_string());
        webhook_authored.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        assert!(emit(&config, &webhook_authored).is_none());

        // The binding's own post, replayed as an event, must not wake it again.
        let mut self_authored = guild_message(ALLOWED_CHANNEL, "digest");
        self_authored.author = discord_user(BOT_ACCOUNT_ID, false);
        self_authored.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        assert!(emit(&config, &self_authored).is_none());
    }

    #[test]
    fn websocket_system_message_types_emit_no_event() {
        let mut message = guild_message(ALLOWED_CHANNEL, "");
        message.message_type = Some(7);
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_duplicate_event_ids_do_not_change_the_decision() {
        let mut message = guild_message(ALLOWED_CHANNEL, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        let config = scoped_config();

        let first = emit(&config, &message).expect("first delivery");
        let second = emit(&config, &message).expect("redelivery");

        assert_eq!(first.event_id, second.event_id);
        assert_eq!(first.activation, second.activation);
        assert_eq!(first.conversation, second.conversation);
    }

    // -------------------------------------------------------------------------
    // Gateway ingress: scope
    // -------------------------------------------------------------------------

    #[test]
    fn websocket_message_from_another_guild_emits_no_event() {
        let mut message = guild_message(ALLOWED_CHANNEL, "hello");
        message.guild_id = Some("guild-other".to_string());
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn guild_membership_and_roles_never_widen_the_channel_allowlist() {
        // The bot account can read every channel in the guild. Only the channel
        // allowlist decides which of them reach this binding.
        let config = scoped_config();
        for channel_id in [OTHER_CHANNEL, "channel-random", "channel-announcements"] {
            let mut message = guild_message(channel_id, "hello");
            message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
            assert!(
                emit(&config, &message).is_none(),
                "channel {channel_id} must stay out of scope"
            );
        }
    }

    #[test]
    fn empty_channel_allowlist_denies_every_channel() {
        let config = ChannelConfig {
            allowed_guild_ids: vec![ALLOWED_GUILD.to_string()],
            ..ChannelConfig::default()
        };
        let mut message = guild_message(ALLOWED_CHANNEL, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&config, &message).is_none());
    }

    #[test]
    fn empty_guild_allowlist_denies_every_guild() {
        let config = ChannelConfig {
            allowed_channel_ids: vec![ALLOWED_CHANNEL.to_string()],
            ..ChannelConfig::default()
        };
        let mut message = guild_message(ALLOWED_CHANNEL, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&config, &message).is_none());
    }

    #[test]
    fn channel_wildcard_applies_only_when_it_is_configured() {
        let mut message = guild_message(OTHER_CHANNEL, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&scoped_config(), &message).is_none());

        let wildcard = ChannelConfig {
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        };
        assert!(
            emit_with_lookup(
                &wildcard,
                &message,
                &MapChannelLookup::channel(OTHER_CHANNEL, ALLOWED_GUILD),
            )
            .is_some()
        );
    }

    #[test]
    fn all_messages_activation_applies_only_when_it_is_configured() {
        let message = guild_message(ALLOWED_CHANNEL, "status update for the team");

        assert!(emit(&scoped_config(), &message).is_none());

        let dedicated = ChannelConfig {
            activation: Some(ACTIVATION_ALL_MESSAGES.to_string()),
            ..scoped_config()
        };
        let event = emit(&dedicated, &message).expect("dedicated channel accepts every message");
        assert_eq!(
            event.activation.expect("activation").reason,
            InboundActivation::REASON_ALL_MESSAGES
        );
    }

    #[test]
    fn slash_command_activation_rejects_plain_gateway_messages() {
        let config = ChannelConfig {
            activation: Some(ACTIVATION_SLASH_COMMAND.to_string()),
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_CHANNEL, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&config, &message).is_none());
    }

    #[test]
    fn guild_sender_allowlist_narrows_an_allowed_channel() {
        let config = ChannelConfig {
            allowed_sender_ids: vec!["member-9".to_string()],
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_CHANNEL, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&config, &message).is_none());

        message.author = discord_user("member-9", false);
        assert!(emit(&config, &message).is_some());
    }

    // -------------------------------------------------------------------------
    // Threads
    // -------------------------------------------------------------------------

    #[test]
    fn child_thread_inherits_only_from_an_allowed_parent() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_INHERIT_PARENT.to_string()),
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_THREAD, "in the thread");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        let event = emit_with_lookup(
            &config,
            &message,
            &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
        )
        .expect("authorized parent");

        assert_eq!(event.conversation.id, ALLOWED_THREAD);
        assert_eq!(event.conversation.kind, CONVERSATION_KIND_THREAD);
        assert_eq!(
            event.conversation.thread_id.as_deref(),
            Some(ALLOWED_THREAD)
        );
        assert_eq!(
            event.conversation.parent_conversation_id.as_deref(),
            Some(ALLOWED_CHANNEL)
        );
        assert_eq!(
            event.conversation.workspace_id.as_deref(),
            Some(ALLOWED_GUILD)
        );
    }

    #[test]
    fn thread_under_an_unauthorized_parent_in_the_same_guild_is_rejected() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_INHERIT_PARENT.to_string()),
            ..scoped_config()
        };
        let mut message = guild_message(OTHER_THREAD, "in the other thread");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(
            emit_with_lookup(
                &config,
                &message,
                &MapChannelLookup::thread(OTHER_THREAD, OTHER_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );
    }

    #[test]
    fn listed_thread_under_an_unauthorized_parent_is_rejected() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_ALLOWLIST.to_string()),
            allowed_thread_ids: vec![ALLOWED_THREAD.to_string()],
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_THREAD, "in the wrong parent");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(
            emit_with_lookup(
                &config,
                &message,
                &MapChannelLookup::thread(ALLOWED_THREAD, OTHER_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );
    }

    #[test]
    fn channel_wildcard_does_not_bypass_thread_policy() {
        let config = ChannelConfig {
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_THREAD, "thread message");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(
            emit_with_lookup(
                &config,
                &message,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );
    }

    #[test]
    fn thread_id_in_channel_allowlist_does_not_bypass_thread_policy() {
        let config = ChannelConfig {
            allowed_channel_ids: vec![ALLOWED_THREAD.to_string()],
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_THREAD, "thread message");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(
            emit_with_lookup(
                &config,
                &message,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );
    }

    #[test]
    fn threads_are_denied_when_no_thread_policy_is_configured() {
        let mut message = guild_message(ALLOWED_THREAD, "in the thread");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(
            emit_with_lookup(
                &scoped_config(),
                &message,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );
    }

    #[test]
    fn explicit_thread_mode_accepts_only_listed_threads() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_ALLOWLIST.to_string()),
            allowed_thread_ids: vec![ALLOWED_THREAD.to_string()],
            ..scoped_config()
        };
        let mut listed = guild_message(ALLOWED_THREAD, "hello");
        listed.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        let mut unlisted = guild_message(OTHER_THREAD, "hello");
        unlisted.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(
            emit_with_lookup(
                &config,
                &listed,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_some()
        );
        assert!(
            emit_with_lookup(
                &config,
                &unlisted,
                &MapChannelLookup::thread(OTHER_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );
    }

    /// A thread allowlist enumerates threads and has no wildcard form, so a
    /// literal `*` entry is refused. `inherit_parent` is the audited way to
    /// authorize every thread beneath an allowed channel, and the one audited
    /// wildcard stays `allowed_channel_ids`.
    #[test]
    fn thread_allowlist_refuses_a_wildcard_thread() {
        for threads in [
            vec![CHANNEL_WILDCARD.to_string()],
            vec![CHANNEL_WILDCARD.to_string(), ALLOWED_THREAD.to_string()],
        ] {
            let policy = DiscordPolicy::from_config(&ChannelConfig {
                thread_policy: Some(THREAD_POLICY_ALLOWLIST.to_string()),
                allowed_thread_ids: threads.clone(),
                ..scoped_config()
            })
            .expect("policy");
            let error = policy
                .validate_for_ingress(false)
                .expect_err("a wildcard thread must be refused")
                .to_string();
            assert!(
                error.contains("allowed_thread_ids does not accept a wildcard thread"),
                "unexpected error for {threads:?}: {error}"
            );
        }

        // A named thread list still validates and still authorizes only the
        // threads it lists.
        let listed = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_ALLOWLIST.to_string()),
            allowed_thread_ids: vec![ALLOWED_THREAD.to_string()],
            ..scoped_config()
        };
        DiscordPolicy::from_config(&listed)
            .expect("policy")
            .validate_for_ingress(false)
            .expect("a named thread allowlist stays valid");
        let mut in_listed = guild_message(ALLOWED_THREAD, "hello");
        in_listed.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        assert!(
            emit_with_lookup(
                &listed,
                &in_listed,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_some()
        );
        let mut in_unlisted = guild_message(OTHER_THREAD, "hello");
        in_unlisted.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        assert!(
            emit_with_lookup(
                &listed,
                &in_unlisted,
                &MapChannelLookup::thread(OTHER_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );

        // `inherit_parent` remains the written-down mode for every thread
        // beneath an authorized channel, and it resolves a thread through that
        // parent rather than through the guild.
        let inherit = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_INHERIT_PARENT.to_string()),
            ..scoped_config()
        };
        DiscordPolicy::from_config(&inherit)
            .expect("policy")
            .validate_for_ingress(false)
            .expect("inherit_parent authorizes threads through their parent channel");
        assert!(
            emit_with_lookup(
                &inherit,
                &in_unlisted,
                &MapChannelLookup::thread(OTHER_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_some()
        );
        assert!(
            emit_with_lookup(
                &inherit,
                &in_unlisted,
                &MapChannelLookup::thread(OTHER_THREAD, OTHER_CHANNEL, ALLOWED_GUILD),
            )
            .is_none()
        );

        // The audited channel wildcard is untouched by the thread rule.
        DiscordPolicy::from_config(&ChannelConfig {
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        })
        .expect("policy")
        .validate_for_ingress(false)
        .expect("the audited channel wildcard is still accepted");
    }

    #[test]
    fn thread_parent_lookup_failure_fails_closed() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_INHERIT_PARENT.to_string()),
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_THREAD, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        // The provider described nothing and the lookup knows nothing, so
        // parentage cannot be proven.
        assert!(emit(&config, &message).is_none());
    }

    #[test]
    fn thread_reported_under_another_guild_is_rejected() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_INHERIT_PARENT.to_string()),
            ..scoped_config()
        };
        let mut message = guild_message(ALLOWED_THREAD, "hello");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(
            emit_with_lookup(
                &config,
                &message,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, "guild-other"),
            )
            .is_none()
        );
    }

    // -------------------------------------------------------------------------
    // Direct messages
    // -------------------------------------------------------------------------

    #[test]
    fn direct_messages_are_denied_by_default() {
        let message = direct_message("hello there");

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn allowlist_dm_policy_accepts_a_listed_sender_and_reports_dm_provenance() {
        let config = ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        };

        let event = emit(&config, &direct_message("hello there")).expect("allowlisted dm");

        assert_eq!(event.conversation.kind, CONVERSATION_KIND_DM);
        assert_eq!(event.conversation.workspace_id, None);
        assert_eq!(
            event.activation.expect("activation").reason,
            InboundActivation::REASON_DIRECT_MESSAGE
        );
    }

    #[test]
    fn allowlist_dm_policy_enforces_its_own_sender_list() {
        let config = ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec!["member-9".to_string()],
            // A guild sender allowlist must not authorize a DM sender.
            allowed_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        };

        assert!(emit(&config, &direct_message("hello")).is_none());

        let mut paired = direct_message("hello");
        paired.author = discord_user("member-9", false);
        assert!(emit(&config, &paired).is_some());
    }

    #[test]
    fn dm_policy_is_independent_of_guild_settings() {
        // An allowlisted DM policy does not widen guild scope, and guild scope
        // does not authorize a DM.
        let allowlisted_dms = ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        };
        let mut wrong_channel = guild_message(OTHER_CHANNEL, "hello");
        wrong_channel.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];
        assert!(emit(&allowlisted_dms, &wrong_channel).is_none());

        assert!(emit(&scoped_config(), &direct_message("hello")).is_none());
    }

    #[test]
    fn empty_direct_message_without_evidence_emits_no_event() {
        let config = ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        };

        assert!(emit(&config, &direct_message("")).is_none());
    }

    #[test]
    fn open_dm_policy_accepts_an_arbitrary_sender_and_reports_dm_provenance() {
        let config = ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            ..ChannelConfig::default()
        };
        DiscordPolicy::from_config(&config)
            .expect("policy")
            .validate_for_ingress(false)
            .expect("direct-message scope does not require a guild channel scope");
        let mut message = direct_message("hello there");
        message.author = discord_user("member-nobody-listed", false);

        let event = emit(&config, &message).expect("open dm policy authorizes any sender");

        assert_eq!(event.conversation.kind, CONVERSATION_KIND_DM);
        assert_eq!(event.conversation.workspace_id, None);
        assert_eq!(
            event.activation.expect("activation").reason,
            InboundActivation::REASON_DIRECT_MESSAGE
        );
    }

    #[test]
    fn open_dm_policy_rejects_a_sender_allowlist() {
        let policy = DiscordPolicy::from_config(&ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            allowed_dm_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        })
        .expect("policy");

        assert!(
            policy.validate_for_ingress(false).is_err(),
            "open names every sender and must not also carry a list"
        );

        let denied_with_senders = DiscordPolicy::from_config(&ChannelConfig {
            allowed_dm_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        })
        .expect("policy");
        assert_eq!(denied_with_senders.dm_policy, DmPolicy::Deny);
        assert!(denied_with_senders.validate_for_ingress(false).is_err());
    }

    /// A sender allowlist enumerates senders and has no wildcard form, so a
    /// literal `*` entry is refused. The one audited wildcard stays
    /// `allowed_channel_ids`.
    #[test]
    fn dm_sender_allowlist_refuses_a_wildcard_sender() {
        for senders in [
            vec![CHANNEL_WILDCARD.to_string()],
            vec![CHANNEL_WILDCARD.to_string(), MEMBER_ID.to_string()],
        ] {
            let policy = DiscordPolicy::from_config(&ChannelConfig {
                dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
                allowed_dm_sender_ids: senders.clone(),
                ..scoped_config()
            })
            .expect("policy");
            let error = policy
                .validate_for_ingress(false)
                .expect_err("a wildcard dm sender must be refused")
                .to_string();
            assert!(
                error.contains("allowed_dm_sender_ids does not accept a wildcard sender"),
                "unexpected error for {senders:?}: {error}"
            );
        }

        // A named sender list still validates and still authorizes only the
        // senders it lists.
        let listed = ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        };
        DiscordPolicy::from_config(&listed)
            .expect("policy")
            .validate_for_ingress(false)
            .expect("a named sender allowlist stays valid");
        assert!(emit(&listed, &direct_message("hello there")).is_some());
        let mut stranger = direct_message("hello there");
        stranger.author = discord_user("member-nobody-listed", false);
        assert!(emit(&listed, &stranger).is_none());

        // `open` remains the written-down mode for unbounded direct-message
        // ingress and still authorizes any sender.
        let open = ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            ..scoped_config()
        };
        DiscordPolicy::from_config(&open)
            .expect("policy")
            .validate_for_ingress(false)
            .expect("open states unbounded direct-message ingress on its own");
        assert!(emit(&open, &stranger).is_some());

        // The audited channel wildcard is untouched by the sender rule.
        DiscordPolicy::from_config(&ChannelConfig {
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        })
        .expect("policy")
        .validate_for_ingress(false)
        .expect("the audited channel wildcard is still accepted");
    }

    #[test]
    fn open_dm_policy_still_obeys_a_set_owner_id() {
        let config = ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            owner_id: Some(MEMBER_ID.to_string()),
            ..scoped_config()
        };

        let mut stranger = direct_message("hello there");
        stranger.author = discord_user("member-nobody-listed", false);
        assert!(
            emit(&config, &stranger).is_none(),
            "an owner-scoped binding must not accept a non-owner dm"
        );

        assert!(emit(&config, &direct_message("hello there")).is_some());
    }

    #[test]
    fn open_dm_policy_does_not_widen_guild_scope() {
        let config = ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            ..scoped_config()
        };
        let mut wrong_channel = guild_message(OTHER_CHANNEL, "hello");
        wrong_channel.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&config, &wrong_channel).is_none());
    }

    #[test]
    fn open_dm_policy_is_reported_in_diagnostics_and_the_projected_policy() {
        let config = ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            ..scoped_config()
        };
        let policy = DiscordPolicy::from_config(&config).expect("policy");

        assert_eq!(
            policy.diagnostics().get(META_DM_POLICY).map(String::as_str),
            Some(DM_POLICY_OPEN)
        );
        assert_eq!(
            policy.to_channel_policy(&config).dm_policy.as_deref(),
            Some(DM_POLICY_OPEN)
        );
    }

    #[test]
    fn unknown_dm_policy_fails_closed() {
        let config = ChannelConfig {
            dm_policy: Some("everyone".to_string()),
            ..scoped_config()
        };

        assert!(DiscordPolicy::from_config(&config).is_err());
    }

    // -------------------------------------------------------------------------
    // Interactions
    // -------------------------------------------------------------------------

    fn command_interaction_body(guild_id: &str, channel_id: &str) -> String {
        format!(
            r#"{{
                "id":"interaction-9",
                "application_id":"app-1",
                "type":2,
                "guild_id":"{guild_id}",
                "channel_id":"{channel_id}",
                "channel":{{"id":"{channel_id}","type":0}},
                "member":{{"user":{{"id":"member-1","username":"member"}}}},
                "data":{{"name":"ask","type":1}}
            }}"#
        )
    }

    #[test]
    fn slash_command_in_an_allowed_channel_emits_one_event() {
        let events = interaction_events(
            &scoped_config(),
            &command_interaction_body(ALLOWED_GUILD, ALLOWED_CHANNEL),
        );

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].activation.as_ref().expect("activation").reason,
            InboundActivation::REASON_SLASH_COMMAND
        );
    }

    #[test]
    fn slash_command_in_an_unauthorized_channel_emits_no_event() {
        // The signature is valid and the interaction names this application. It
        // is still outside the conversations this binding was granted.
        let events = interaction_events(
            &scoped_config(),
            &command_interaction_body(ALLOWED_GUILD, OTHER_CHANNEL),
        );

        assert!(events.is_empty());
    }

    #[test]
    fn interaction_request_rejects_an_invalid_cross_field_policy() {
        let config = ChannelConfig {
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string(), ALLOWED_CHANNEL.to_string()],
            activation: Some(ACTIVATION_ALL_MESSAGES.to_string()),
            ..scoped_config()
        };

        assert!(
            interaction_events(
                &config,
                &command_interaction_body(ALLOWED_GUILD, ALLOWED_CHANNEL),
            )
            .is_empty()
        );
    }

    #[test]
    fn interaction_in_an_unauthorized_guild_emits_no_event() {
        let events = interaction_events(
            &scoped_config(),
            &command_interaction_body("guild-other", ALLOWED_CHANNEL),
        );

        assert!(events.is_empty());
    }

    #[test]
    fn component_interaction_in_an_unauthorized_channel_emits_no_event() {
        let body = format!(
            r#"{{
                "id":"interaction-10",
                "application_id":"app-1",
                "type":3,
                "guild_id":"{ALLOWED_GUILD}",
                "channel_id":"{OTHER_CHANNEL}",
                "member":{{"user":{{"id":"member-1","username":"member"}}}},
                "data":{{"custom_id":"approve","component_type":2}}
            }}"#
        );

        assert!(interaction_events(&scoped_config(), &body).is_empty());
    }

    #[test]
    fn interaction_from_another_application_is_rejected() {
        let config = ChannelConfig {
            application_id: Some("app-expected".to_string()),
            ..scoped_config()
        };

        let events = interaction_events(
            &config,
            &command_interaction_body(ALLOWED_GUILD, ALLOWED_CHANNEL),
        );

        assert!(events.is_empty());
    }

    #[test]
    fn interaction_in_a_thread_follows_the_thread_policy() {
        let body = |channel_id: &str, parent_id: &str| {
            format!(
                r#"{{
                    "id":"interaction-11",
                    "application_id":"app-1",
                    "type":2,
                    "guild_id":"{ALLOWED_GUILD}",
                    "channel_id":"{channel_id}",
                    "channel":{{"id":"{channel_id}","type":11,"parent_id":"{parent_id}"}},
                    "member":{{"user":{{"id":"member-1","username":"member"}}}},
                    "data":{{"name":"ask","type":1}}
                }}"#
            )
        };
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_INHERIT_PARENT.to_string()),
            ..scoped_config()
        };

        let allowed = interaction_events(&config, &body(ALLOWED_THREAD, ALLOWED_CHANNEL));
        assert_eq!(allowed.len(), 1);
        assert_eq!(
            allowed[0].conversation.parent_conversation_id.as_deref(),
            Some(ALLOWED_CHANNEL)
        );

        let rejected = interaction_events(&config, &body(OTHER_THREAD, OTHER_CHANNEL));
        assert!(rejected.is_empty());

        // Without a thread policy the same interaction is denied.
        assert!(
            interaction_events(&scoped_config(), &body(ALLOWED_THREAD, ALLOWED_CHANNEL)).is_empty()
        );
    }

    // -------------------------------------------------------------------------
    // Configuration and projection
    // -------------------------------------------------------------------------

    #[test]
    fn websocket_ingress_without_channel_scope_fails_validation() {
        let guild_only = DiscordPolicy::from_config(&ChannelConfig {
            allowed_guild_ids: vec![ALLOWED_GUILD.to_string()],
            ..ChannelConfig::default()
        })
        .expect("policy");
        let channel_only = DiscordPolicy::from_config(&ChannelConfig {
            allowed_channel_ids: vec![ALLOWED_CHANNEL.to_string()],
            ..ChannelConfig::default()
        })
        .expect("policy");

        assert!(guild_only.validate_for_ingress(false).is_err());
        assert!(channel_only.validate_for_ingress(false).is_err());
        assert!(
            DiscordPolicy::from_config(&scoped_config())
                .expect("policy")
                .validate_for_ingress(false)
                .is_ok()
        );
    }

    #[test]
    fn unknown_policy_values_fail_validation() {
        for config in [
            ChannelConfig {
                activation: Some("whenever".to_string()),
                ..scoped_config()
            },
            ChannelConfig {
                thread_policy: Some("sometimes".to_string()),
                ..scoped_config()
            },
            ChannelConfig {
                dm_policy: Some("everyone".to_string()),
                ..scoped_config()
            },
            ChannelConfig {
                reply_delivery: Some("both".to_string()),
                ..scoped_config()
            },
            ChannelConfig {
                dm_policy: Some("dm".to_string()),
                ..scoped_config()
            },
            ChannelConfig {
                dm_policy: Some("pairing".to_string()),
                ..scoped_config()
            },
        ] {
            assert!(
                DiscordPolicy::from_config(&config).is_err(),
                "unknown value must not fall through to a default"
            );
        }
    }

    #[test]
    fn all_messages_rejects_the_channel_wildcard() {
        let policy = DiscordPolicy::from_config(&ChannelConfig {
            activation: Some(ACTIVATION_ALL_MESSAGES.to_string()),
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        })
        .expect("policy");

        assert!(policy.validate_for_ingress(false).is_err());
    }

    #[test]
    fn thread_ids_require_the_allowlist_policy() {
        for thread_policy in [THREAD_POLICY_DENY, THREAD_POLICY_INHERIT_PARENT] {
            let policy = DiscordPolicy::from_config(&ChannelConfig {
                thread_policy: Some(thread_policy.to_string()),
                allowed_thread_ids: vec![ALLOWED_THREAD.to_string()],
                ..scoped_config()
            })
            .expect("policy");

            assert!(policy.validate_for_ingress(false).is_err());
        }
    }

    #[test]
    fn guild_wildcard_and_mixed_or_widened_channel_scope_fail_validation() {
        let guild_wildcard = DiscordPolicy::from_config(&ChannelConfig {
            allowed_guild_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        })
        .expect("policy");
        assert!(guild_wildcard.validate_for_ingress(false).is_err());

        let mixed = DiscordPolicy::from_config(&ChannelConfig {
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string(), ALLOWED_CHANNEL.to_string()],
            ..scoped_config()
        })
        .expect("policy");
        assert!(mixed.validate_for_ingress(false).is_err());

        let outbound_wider = DiscordPolicy::from_config(&ChannelConfig {
            outbound_channel_ids: vec![ALLOWED_CHANNEL.to_string(), OTHER_CHANNEL.to_string()],
            ..scoped_config()
        })
        .expect("policy");
        assert!(outbound_wider.validate_for_ingress(false).is_err());

        let outbound_wildcard = DiscordPolicy::from_config(&ChannelConfig {
            outbound_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        })
        .expect("policy");
        assert!(outbound_wildcard.validate_for_ingress(false).is_err());
    }

    #[test]
    fn channel_policy_separates_workspace_scope_from_conversation_scope() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_ALLOWLIST.to_string()),
            allowed_thread_ids: vec![ALLOWED_THREAD.to_string()],
            ..scoped_config()
        };
        let policy = DiscordPolicy::from_config(&config).expect("policy");

        let projected = policy.to_channel_policy(&config);

        assert_eq!(projected.allowed_workspace_ids, vec![ALLOWED_GUILD]);
        assert_eq!(
            projected.allowed_conversation_ids,
            vec![ALLOWED_CHANNEL, ALLOWED_THREAD]
        );
        assert!(
            !projected
                .allowed_conversation_ids
                .contains(&ALLOWED_GUILD.to_string())
        );
        assert_eq!(
            projected.allowed_outbound_conversation_ids,
            vec![ALLOWED_CHANNEL]
        );
        assert_eq!(
            projected.activation.as_deref(),
            Some(ACTIVATION_MENTION_OR_REPLY)
        );
        assert_eq!(
            projected.thread_policy.as_deref(),
            Some(THREAD_POLICY_ALLOWLIST)
        );
        assert_eq!(projected.dm_policy.as_deref(), Some(DM_POLICY_DENY));
    }

    #[test]
    fn diagnostics_report_modes_and_counts_without_identifiers() {
        let config = ChannelConfig {
            allowed_channel_ids: vec![ALLOWED_CHANNEL.to_string(), OTHER_CHANNEL.to_string()],
            ..scoped_config()
        };
        let policy = DiscordPolicy::from_config(&config).expect("policy");

        let diagnostics = policy.diagnostics();

        assert_eq!(
            diagnostics
                .get(META_ALLOWED_CHANNEL_COUNT)
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(
            diagnostics.get(META_ACTIVATION).map(String::as_str),
            Some(ACTIVATION_MENTION_OR_REPLY)
        );
        assert_eq!(
            diagnostics.get(META_REPLY_DELIVERY).map(String::as_str),
            Some(REPLY_DELIVERY_RUNTIME_OWNED)
        );
        for value in diagnostics.values() {
            assert!(
                !value.contains(ALLOWED_CHANNEL) && !value.contains(ALLOWED_GUILD),
                "diagnostics must not carry raw identifiers"
            );
        }
    }

    #[test]
    fn bot_identity_and_policy_survive_a_gateway_reconnect() {
        let policy = DiscordPolicy::from_config(&scoped_config()).expect("policy");
        let started = websocket_ingress_state(&policy, &bot_identity());

        let mut session = DiscordGatewaySession::from_state(Some(&started));
        session.session_id = Some("session-1".to_string());
        session.last_sequence = Some(42);
        session.resume_gateway_url = Some("wss://resume.discord.gg".to_string());
        let resumed = DiscordGatewaySession::from_state(Some(&session.to_state("reconnect")));

        assert_eq!(resumed.bot_user_id.as_deref(), Some(BOT_ACCOUNT_ID));
        assert!(resumed.can_resume());
    }

    #[test]
    fn ready_identity_must_match_the_authenticated_account() {
        let identity = DiscordBotIdentity {
            account_id: BOT_ACCOUNT_ID.to_string(),
            application_id: Some("app-1".to_string()),
        };

        assert!(
            verify_ready_identity(
                &identity,
                &json!({"user":{"id":BOT_ACCOUNT_ID},"application":{"id":"app-1"}}),
            )
            .is_ok()
        );
        assert!(
            verify_ready_identity(
                &identity,
                &json!({"user":{"id":"other-bot"},"application":{"id":"app-1"}}),
            )
            .is_err()
        );
        assert!(
            verify_ready_identity(
                &identity,
                &json!({"user":{"id":BOT_ACCOUNT_ID},"application":{"id":"app-other"}}),
            )
            .is_err()
        );
    }

    // -------------------------------------------------------------------------
    // Outbound scope
    // -------------------------------------------------------------------------

    fn outbound_to(channel_id: Option<&str>, thread_id: Option<&str>) -> OutboundMessage {
        OutboundMessage {
            content: "hello".to_string(),
            attachments: Vec::new(),
            channel_id: channel_id.map(str::to_owned),
            thread_id: thread_id.map(str::to_owned),
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        }
    }

    fn authorize(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
        let destination = resolve_destination(config, message)?;
        authorize_with_lookup(
            config,
            message,
            &MapChannelLookup::channel(&destination, ALLOWED_GUILD),
        )
    }

    fn authorize_with_lookup(
        config: &ChannelConfig,
        message: &OutboundMessage,
        lookup: &dyn DiscordChannelLookup,
    ) -> Result<String> {
        let policy = DiscordPolicy::from_config(config).expect("policy");
        let destination = resolve_destination(config, message)?;
        authorize_outbound_destination(&policy, &destination, lookup)?;
        Ok(destination)
    }

    #[test]
    fn outbound_accepts_the_configured_channel() {
        let destination = authorize(&scoped_config(), &outbound_to(Some(ALLOWED_CHANNEL), None))
            .expect("allowed destination");

        assert_eq!(destination, ALLOWED_CHANNEL);
    }

    #[test]
    fn outbound_rejects_a_caller_supplied_unauthorized_channel() {
        let error = authorize(&scoped_config(), &outbound_to(Some(OTHER_CHANNEL), None))
            .expect_err("out of scope");

        assert_eq!(
            error
                .downcast_ref::<OutboundRejected>()
                .map(|rejected| rejected.code),
            Some(REJECT_UNAUTHORIZED_DESTINATION)
        );
    }

    #[test]
    fn outbound_rejects_a_caller_supplied_unauthorized_thread() {
        let config = ChannelConfig {
            thread_policy: Some(THREAD_POLICY_ALLOWLIST.to_string()),
            allowed_thread_ids: vec![ALLOWED_THREAD.to_string()],
            ..scoped_config()
        };

        assert!(
            authorize_with_lookup(
                &config,
                &outbound_to(Some(ALLOWED_CHANNEL), Some(ALLOWED_THREAD)),
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_ok()
        );
        assert!(
            authorize_with_lookup(
                &config,
                &outbound_to(Some(ALLOWED_CHANNEL), Some(OTHER_THREAD)),
                &MapChannelLookup::thread(OTHER_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_err()
        );
    }

    #[test]
    fn outbound_wildcard_does_not_bypass_thread_deny() {
        let config = ChannelConfig {
            allowed_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        };
        let message = outbound_to(Some(ALLOWED_CHANNEL), Some(ALLOWED_THREAD));

        assert!(
            authorize_with_lookup(
                &config,
                &message,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_err()
        );
    }

    #[test]
    fn outbound_thread_id_in_channel_allowlist_does_not_bypass_thread_deny() {
        let config = ChannelConfig {
            allowed_channel_ids: vec![ALLOWED_THREAD.to_string()],
            ..scoped_config()
        };
        let message = outbound_to(Some(ALLOWED_THREAD), None);

        assert!(
            authorize_with_lookup(
                &config,
                &message,
                &MapChannelLookup::thread(ALLOWED_THREAD, ALLOWED_CHANNEL, ALLOWED_GUILD),
            )
            .is_err()
        );
    }

    #[test]
    fn outbound_direct_message_follows_the_declared_dm_policy() {
        let destination = "dm-channel-1";
        let message = outbound_to(Some(destination), None);

        assert!(
            authorize_with_lookup(
                &scoped_config(),
                &message,
                &MapChannelLookup::direct_message(destination, MEMBER_ID),
            )
            .is_err()
        );

        let allowlisted = ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec![MEMBER_ID.to_string()],
            ..scoped_config()
        };
        assert!(
            authorize_with_lookup(
                &allowlisted,
                &message,
                &MapChannelLookup::direct_message(destination, MEMBER_ID),
            )
            .is_ok()
        );
        assert!(
            authorize_with_lookup(
                &allowlisted,
                &message,
                &MapChannelLookup::direct_message(destination, "member-other"),
            )
            .is_err()
        );

        let open = ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            ..scoped_config()
        };
        assert!(
            authorize_with_lookup(
                &open,
                &message,
                &MapChannelLookup::direct_message(destination, "member-other"),
            )
            .is_ok()
        );
    }

    #[test]
    fn delivery_rejects_invalid_policy_before_provider_access() {
        let config = ChannelConfig {
            outbound_channel_ids: vec![CHANNEL_WILDCARD.to_string()],
            ..scoped_config()
        };
        let error = deliver(&config, &outbound_to(Some(OTHER_CHANNEL), None))
            .expect_err("outbound wildcard wider than inbound must fail locally");

        assert!(error.to_string().contains("outbound_channel_ids"));
    }

    #[test]
    fn outbound_rejects_unauthorized_route_metadata() {
        let mut message = outbound_to(None, None);
        message
            .metadata
            .insert(ROUTE_CONVERSATION_ID.to_string(), OTHER_CHANNEL.to_string());

        assert!(authorize(&scoped_config(), &message).is_err());
    }

    #[test]
    fn default_channel_id_cannot_bypass_the_outbound_allowlist() {
        let config = ChannelConfig {
            default_channel_id: Some(OTHER_CHANNEL.to_string()),
            ..scoped_config()
        };

        assert!(authorize(&config, &outbound_to(None, None)).is_err());
    }

    #[test]
    fn outbound_falls_back_to_the_inbound_channels_and_never_wider() {
        let narrowed = ChannelConfig {
            allowed_channel_ids: vec![ALLOWED_CHANNEL.to_string(), OTHER_CHANNEL.to_string()],
            outbound_channel_ids: vec![ALLOWED_CHANNEL.to_string()],
            ..scoped_config()
        };

        assert!(authorize(&narrowed, &outbound_to(Some(ALLOWED_CHANNEL), None)).is_ok());
        assert!(authorize(&narrowed, &outbound_to(Some(OTHER_CHANNEL), None)).is_err());

        let inherited = ChannelConfig {
            allowed_channel_ids: vec![ALLOWED_CHANNEL.to_string(), OTHER_CHANNEL.to_string()],
            ..scoped_config()
        };
        assert!(authorize(&inherited, &outbound_to(Some(OTHER_CHANNEL), None)).is_ok());
    }

    #[test]
    fn an_allowed_guild_does_not_imply_access_to_every_guild_channel() {
        let config = ChannelConfig {
            allowed_guild_ids: vec![ALLOWED_GUILD.to_string()],
            allowed_channel_ids: vec![ALLOWED_CHANNEL.to_string()],
            ..ChannelConfig::default()
        };

        assert!(authorize(&config, &outbound_to(Some("channel-anything-else"), None)).is_err());
    }

    #[test]
    fn deliver_rejects_an_unauthorized_destination_before_any_provider_request() {
        let guard = EnvGuard::set("DISCORD_BOT_TOKEN", "token");

        let error = deliver(&scoped_config(), &outbound_to(Some(OTHER_CHANNEL), None))
            .expect_err("out of scope");

        assert_eq!(
            error
                .downcast_ref::<OutboundRejected>()
                .map(|rejected| rejected.code),
            Some(REJECT_UNAUTHORIZED_DESTINATION)
        );
        drop(guard);
    }

    #[test]
    fn status_frames_answer_to_the_outbound_allowlist() {
        let guard = EnvGuard::set("DISCORD_BOT_TOKEN", "token");
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "working".to_string(),
            conversation_id: Some(OTHER_CHANNEL.to_string()),
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        let acceptance = send_status(&scoped_config(), &update).expect("send status");

        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some(REJECT_UNAUTHORIZED_DESTINATION)
        );
        drop(guard);
    }

    // -------------------------------------------------------------------------
    // Production control and failure pair
    // -------------------------------------------------------------------------

    #[test]
    fn production_control_direct_mention_in_the_configured_channel_emits_one_event() {
        let mut message = guild_message(ALLOWED_CHANNEL, "hey, are you there?");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        let event = emit(&scoped_config(), &message).expect("configured channel mention");

        assert_eq!(event.conversation.id, ALLOWED_CHANNEL);
        assert_eq!(
            event.activation.expect("activation").reason,
            InboundActivation::REASON_DIRECT_MENTION
        );
    }

    #[test]
    fn production_failure_unaddressed_message_in_another_channel_emits_no_event() {
        // Channel scope denies this before activation is reached, and naming
        // another member is not activation evidence in any case.
        let mut message = guild_message(OTHER_CHANNEL, "");
        message.mentions = vec![discord_user("member-christian", false)];

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn production_failure_mention_in_another_channel_still_emits_no_event() {
        let mut message = guild_message(OTHER_CHANNEL, "are you around?");
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn websocket_ingress_state_reports_transport_and_proven_identity() {
        let policy = DiscordPolicy::from_config(&scoped_config()).expect("policy");

        let state = websocket_ingress_state(&policy, &bot_identity());

        assert_eq!(state.mode, IngressMode::Websocket);
        assert_eq!(
            state.metadata.get(META_TRANSPORT).map(String::as_str),
            Some(TRANSPORT_WEBSOCKET)
        );
        assert_eq!(
            state.metadata.get(META_BOT_USER_ID).map(String::as_str),
            Some(BOT_ACCOUNT_ID)
        );
    }

    #[test]
    fn start_ingress_fails_closed_without_any_authorized_surface() {
        let guard = EnvGuard::set("DISCORD_BOT_TOKEN", "token");

        let error = start_ingress(&ChannelConfig::default()).expect_err("unscoped ingress");

        assert!(
            error
                .to_string()
                .contains("no authorized guild or direct-message surface"),
            "unexpected error: {error}"
        );
        drop(guard);
    }

    #[test]
    fn websocket_intents_do_not_request_message_content_by_default() {
        let config = ChannelConfig::default();

        assert_eq!(
            discord_gateway_intents(&config),
            DISCORD_GATEWAY_BASE_INTENTS
        );
    }

    #[test]
    fn websocket_intents_can_request_message_content() {
        let config = ChannelConfig {
            message_content_intent: Some(true),
            ..ChannelConfig::default()
        };

        assert_eq!(
            discord_gateway_intents(&config),
            DISCORD_GATEWAY_BASE_INTENTS | DISCORD_GATEWAY_MESSAGE_CONTENT_INTENT
        );
    }

    #[test]
    fn websocket_state_preserves_resume_metadata() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
            Some(BOT_ACCOUNT_ID),
        );
        let session = DiscordGatewaySession::from_state(Some(&state));

        assert_eq!(session.last_sequence, Some(42));
        assert_eq!(session.session_id.as_deref(), Some("session-1"));
        assert_eq!(
            session.resume_gateway_url.as_deref(),
            Some("wss://resume.discord.gg")
        );
        assert!(session.can_resume());
    }

    #[test]
    fn websocket_resume_url_is_preferred_over_gateway_lookup() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
            Some(BOT_ACCOUNT_ID),
        );
        let session = DiscordGatewaySession::from_state(Some(&state));

        let gateway_url = discord_gateway_base_url(&session, || {
            Err(anyhow!(
                "gateway lookup should not be used when resume URL exists"
            ))
        })
        .expect("resume URL");

        assert_eq!(gateway_url, "wss://resume.discord.gg");
        assert_eq!(
            discord_gateway_websocket_url(&gateway_url),
            "wss://resume.discord.gg/?v=10&encoding=json"
        );
    }

    #[test]
    fn websocket_resume_requires_resume_gateway_url() {
        let state = websocket_state("running", Some(42), Some("session-1"), None, None);
        let session = DiscordGatewaySession::from_state(Some(&state));

        let gateway_url =
            discord_gateway_base_url(&session, || Ok("wss://gateway.discord.gg".to_string()))
                .expect("fallback URL");

        assert!(!session.can_resume());
        assert_eq!(gateway_url, "wss://gateway.discord.gg");
    }

    #[test]
    fn websocket_non_reconnectable_close_stops_worker() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
            Some(BOT_ACCOUNT_ID),
        );
        let mut session = DiscordGatewaySession::from_state(Some(&state));
        let frame = CloseFrame {
            code: CloseCode::from(4014),
            reason: "disallowed intents".into(),
        };

        let action = handle_discord_close_frame(&mut session, Some(&frame));

        assert_eq!(action, DiscordGatewayCloseAction::Stop);
        assert!(!session.can_resume());
        assert_eq!(discord_close_status(action), "stopped");
        assert_eq!(discord_close_poll_after(action), None);
    }

    #[test]
    fn websocket_invalid_sequence_close_reidentifies() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
            Some(BOT_ACCOUNT_ID),
        );
        let mut session = DiscordGatewaySession::from_state(Some(&state));
        let frame = CloseFrame {
            code: CloseCode::from(4007),
            reason: "invalid seq".into(),
        };

        let action = handle_discord_close_frame(&mut session, Some(&frame));

        assert_eq!(action, DiscordGatewayCloseAction::Reidentify);
        assert!(!session.can_resume());
        assert_eq!(discord_close_status(action), "closed");
        assert_eq!(discord_close_poll_after(action), Some(1000));
    }

    #[test]
    fn websocket_missing_close_code_preserves_resume_metadata() {
        let state = websocket_state(
            "running",
            Some(42),
            Some("session-1"),
            Some("wss://resume.discord.gg"),
            Some(BOT_ACCOUNT_ID),
        );
        let mut session = DiscordGatewaySession::from_state(Some(&state));

        let action = handle_discord_close_frame(&mut session, None);

        assert_eq!(action, DiscordGatewayCloseAction::Resume);
        assert!(session.can_resume());
        assert_eq!(
            session.resume_gateway_url.as_deref(),
            Some("wss://resume.discord.gg")
        );
    }

    #[test]
    fn websocket_message_ignores_bot_author() {
        let mut message = guild_message(ALLOWED_CHANNEL, "ignore me");
        message.author = discord_user("bot-1", true);
        message.mentions = vec![discord_user(BOT_ACCOUNT_ID, true)];

        assert!(emit(&scoped_config(), &message).is_none());
    }

    #[test]
    fn component_interaction_preserves_source_message_attachments() {
        let payload = base_payload(
            r#"{
                "id":"interaction-2",
                "application_id":"app-1",
                "type":3,
                "guild_id":"guild-marketing",
                "channel_id":"channel-marketing",
                "channel":{"id":"channel-marketing","type":0},
                "member":{
                    "user":{
                        "id":"user-2",
                        "username":"dispatch-user"
                    }
                },
                "data":{
                    "custom_id":"approve",
                    "component_type":2
                },
                "message":{
                    "id":"message-99",
                    "attachments":[
                        {
                            "id":"attachment-1",
                            "filename":"design.png",
                            "url":"https://cdn.discordapp.com/attachments/design.png",
                            "content_type":"image/png",
                            "size":4096,
                            "description":"wireframe"
                        }
                    ]
                }
            }"#,
        );

        let response = handle_ingress_event(&scoped_config(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert_eq!(events.len(), 1);
                assert!(callback_reply.is_some());

                let event = &events[0];
                assert_eq!(event.event_type, "message_component");
                assert_eq!(event.message.id, "message-99");
                assert_eq!(
                    event.message.reply_to_message_id.as_deref(),
                    Some("message-99")
                );
                assert_eq!(event.message.attachments.len(), 1);

                let attachment = &event.message.attachments[0];
                assert_eq!(attachment.id.as_deref(), Some("attachment-1"));
                assert_eq!(attachment.kind, "image");
                assert_eq!(
                    attachment.url.as_deref(),
                    Some("https://cdn.discordapp.com/attachments/design.png")
                );
                assert_eq!(attachment.mime_type.as_deref(), Some("image/png"));
                assert_eq!(attachment.size_bytes, Some(4096));
                assert_eq!(attachment.name.as_deref(), Some("design.png"));
                assert_eq!(attachment.extracted_text.as_deref(), Some("wireframe"));
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_ATTACHMENT_COUNT)
                        .map(String::as_str),
                    Some("1")
                );
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_CUSTOM_ID)
                        .map(String::as_str),
                    Some("approve")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let mut payload = base_payload(r#"{"id":"1","application_id":"app-1","type":1}"#);
        payload.trust_verified = false;
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_ED25519.to_string(), "00".repeat(64));
        payload.headers.insert(
            HEADER_X_SIGNATURE_TIMESTAMP.to_string(),
            Timestamp::now().as_second().to_string(),
        );

        let reply = validate_discord_signature("11".repeat(32).as_str(), &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(reply.body, "invalid request signature");
    }

    #[test]
    fn valid_signature_is_accepted() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        let timestamp = Timestamp::now().as_second().to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), timestamp);

        let reply =
            validate_discord_signature(&verify_key_hex, &payload).expect("validate signature");

        assert!(reply.is_none());
    }

    #[test]
    fn stale_timestamp_is_rejected() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        // Sign with a timestamp well outside the freshness window so we cannot
        // be replayed even with a cryptographically valid signature.
        let stale_timestamp =
            (Timestamp::now().as_second() - DISCORD_MAX_SIGNATURE_AGE_SECS - 60).to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{stale_timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), stale_timestamp);

        let reply = validate_discord_signature(&verify_key_hex, &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(
            reply.body,
            "discord request timestamp outside the accepted window"
        );
    }

    #[test]
    fn future_timestamp_outside_window_is_rejected() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        let future_timestamp =
            (Timestamp::now().as_second() + DISCORD_MAX_SIGNATURE_AGE_SECS + 60).to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{future_timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), future_timestamp);

        let reply = validate_discord_signature(&verify_key_hex, &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(
            reply.body,
            "discord request timestamp outside the accepted window"
        );
    }

    #[test]
    fn absurdly_negative_timestamp_is_rejected_without_panicking() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        let timestamp = i64::MIN.to_string();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_TIMESTAMP.to_string(), timestamp);

        let reply = validate_discord_signature(&verify_key_hex, &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(
            reply.body,
            "discord request timestamp outside the accepted window"
        );
    }

    #[test]
    fn direct_message_is_dropped_when_allowlist_policy_has_no_sender_match() {
        let payload = base_payload(
            r#"{
                "id":"interaction-3",
                "application_id":"app-1",
                "type":2,
                "channel_id":"dm-channel-1",
                "member":{
                    "user":{
                        "id":"user-9",
                        "username":"dispatch-user"
                    }
                },
                "data":{
                    "name":"ask",
                    "type":1,
                    "options":[
                        {
                            "name":"query",
                            "value":"hello world"
                        }
                    ]
                }
            }"#,
        );

        let response = handle_ingress_event(
            &ChannelConfig {
                dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
                allowed_dm_sender_ids: vec!["member-1".to_string()],
                ..ChannelConfig::default()
            },
            &payload,
        )
        .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => assert!(events.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn render_status_message_uses_override() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "ignored".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::from([(DISCORD_STATUS_TEXT.to_string(), "custom".to_string())]),
        };

        assert_eq!(render_status_message(&update), "custom");
    }

    #[test]
    fn send_status_rejects_missing_destination() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "hello".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        let acceptance = send_status(&scoped_config(), &update).expect("send status");

        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some(REJECT_MISSING_DESTINATION)
        );
    }

    #[test]
    fn discord_upload_decodes_inline_attachment_data() {
        let message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: None,
                storage_key: None,
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };

        let upload = discord_upload(&message)
            .expect("inline attachment")
            .expect("upload present");
        assert_eq!(upload.name, "report.txt");
        assert_eq!(upload.mime_type, "text/plain");
        assert_eq!(upload.data, b"hello");
    }

    #[test]
    fn discord_upload_rejects_url_and_storage_key_sources() {
        let url_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: Some("https://example.com/report.txt".to_string()),
                storage_key: None,
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            discord_upload(&url_message)
                .expect_err("url attachments rejected")
                .to_string()
                .contains("url attachments are not supported")
        );

        let staged_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: None,
                storage_key: Some("cache://report.txt".to_string()),
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            discord_upload(&staged_message)
                .expect_err("storage_key attachments rejected")
                .to_string()
                .contains("storage_key attachments are not supported")
        );
    }
}
