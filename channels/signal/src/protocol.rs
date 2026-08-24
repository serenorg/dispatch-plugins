use anyhow::{Result, anyhow};
use dispatch_channel_protocol as proto;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

// The migration to presage is being landed incrementally. Each feature
// commit wires up another family of plugin operations and starts using
// more of the re-exports below. Until the migration finishes some of
// these are only touched by the channel-plugin.json manifest conformance
// test, so silence `unused_imports` at the `pub use` boundary rather
// than churning the re-export list on every commit.
#[allow(unused_imports)]
pub use proto::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities, ChannelPolicy,
    ConfiguredChannel, DeliveryReceipt, HealthReport, InboundActivation, InboundActor,
    InboundAttachment, InboundConversationRef, InboundEventEnvelope, InboundMessage, IngressMode,
    IngressState, OutboundAttachment, OutboundMessageEnvelope, PluginResponse, StatusAcceptance,
    StatusFrame, StatusKind, ThreadingModel, parse_jsonrpc_request, plugin_error,
    response_to_jsonrpc,
};

/// Configuration for the native Rust Signal channel plugin.
///
/// The native plugin owns its own Signal session state in a local SQLite
/// store via `presage-store-sqlite`. There is no external daemon, REST
/// endpoint, or Docker container involved: linking the session, receiving,
/// and sending all happen inside this plugin process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChannelConfig {
    /// Absolute (or `~`-expanded) path to the SQLite store backing the
    /// Signal session. When unset, the plugin uses
    /// `$XDG_CONFIG_HOME/dispatch/channels/signal/<account>/store.db`
    /// (or `$HOME/.config/...`), where `<account>` defaults to `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_store_path: Option<String>,
    /// Optional logical account name. When the store path is not
    /// explicitly set this selects the subdirectory under the default
    /// store root, allowing a single host to link multiple Signal
    /// accounts side-by-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Optional env var name holding a passphrase that encrypts the
    /// SQLite store at rest (SQLCipher / SQLite `PRAGMA key`). When
    /// unset the store is written unencrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase_env: Option<String>,
    /// Fallback recipient for operator-driven `push`, `deliver`, and
    /// `status` requests that do not carry routing metadata of their
    /// own. Accepts a bare UUID (treated as ACI), `ACI:<uuid>`, or
    /// `PNI:<uuid>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_recipient: Option<String>,
    /// Receive timeout in seconds for a single `poll_ingress` cycle.
    /// Zero falls back to the default timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_timeout_secs: Option<u16>,
    /// Who may start a direct-message turn. The default is deny so a
    /// configuration without an explicit ingress policy cannot receive messages.
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Signal ServiceIds permitted to start direct-message turns when
    /// `dm_policy` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_dm_sender_ids: Vec<String>,
    /// Signal ServiceIds permitted for outbound delivery. When empty, the
    /// allowlist used for inbound turns is also the outbound scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbound_recipient_ids: Vec<String>,
    /// Which runtime path owns the visible reply to an inbound turn.
    #[serde(default)]
    pub reply_delivery: ReplyDelivery,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DmPolicy {
    #[default]
    Deny,
    Allowlist,
    Open,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReplyDelivery {
    #[default]
    RuntimeOwned,
    ToolOwned,
}

impl ReplyDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeOwned => "runtime_owned",
            Self::ToolOwned => "tool_owned",
        }
    }
}

/// A validated, canonical Signal authorization policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalChannelPolicy {
    dm_policy: DmPolicy,
    allowed_dm_sender_ids: BTreeSet<String>,
    outbound_recipient_ids: BTreeSet<String>,
    reply_delivery: ReplyDelivery,
}

impl SignalChannelPolicy {
    pub fn from_config(config: &ChannelConfig) -> Result<Self> {
        let allowed_dm_sender_ids =
            normalize_service_ids("allowed_dm_sender_ids", &config.allowed_dm_sender_ids)?;
        let outbound_recipient_ids =
            normalize_service_ids("outbound_recipient_ids", &config.outbound_recipient_ids)?;

        match config.dm_policy {
            DmPolicy::Deny | DmPolicy::Open if !allowed_dm_sender_ids.is_empty() => {
                return Err(anyhow!(
                    "dm_policy `{}` requires allowed_dm_sender_ids to be empty",
                    dm_policy_name(config.dm_policy)
                ));
            }
            DmPolicy::Deny if !outbound_recipient_ids.is_empty() => {
                return Err(anyhow!(
                    "dm_policy `deny` requires outbound_recipient_ids to be empty"
                ));
            }
            DmPolicy::Allowlist
                if outbound_recipient_ids
                    .iter()
                    .any(|recipient| !allowed_dm_sender_ids.contains(recipient)) =>
            {
                return Err(anyhow!(
                    "outbound_recipient_ids must be a subset of allowed_dm_sender_ids"
                ));
            }
            _ => {}
        }

        if config.reply_delivery == ReplyDelivery::RuntimeOwned {
            match config.dm_policy {
                DmPolicy::Allowlist
                    if allowed_dm_sender_ids.iter().any(|sender| {
                        !outbound_recipient_ids.is_empty()
                            && !outbound_recipient_ids.contains(sender)
                    }) =>
                {
                    return Err(anyhow!(
                        "runtime_owned reply delivery requires outbound_recipient_ids to include every allowed_dm_sender_ids entry"
                    ));
                }
                DmPolicy::Open if !outbound_recipient_ids.is_empty() => {
                    return Err(anyhow!(
                        "runtime_owned reply delivery requires an empty outbound_recipient_ids fallback when dm_policy is `open`"
                    ));
                }
                DmPolicy::Deny | DmPolicy::Allowlist | DmPolicy::Open => {}
            }
        }

        Ok(Self {
            dm_policy: config.dm_policy,
            allowed_dm_sender_ids,
            outbound_recipient_ids,
            reply_delivery: config.reply_delivery,
        })
    }

    pub fn require_persistent_ingress(&self) -> Result<()> {
        match self.dm_policy {
            DmPolicy::Open => Ok(()),
            DmPolicy::Allowlist if !self.allowed_dm_sender_ids.is_empty() => Ok(()),
            DmPolicy::Allowlist => Err(anyhow!(
                "Signal persistent ingress requires at least one allowed_dm_sender_ids entry or dm_policy `open`"
            )),
            DmPolicy::Deny => Err(anyhow!(
                "Signal persistent ingress requires dm_policy `allowlist` or `open`"
            )),
        }
    }

    pub fn allows_inbound_sender(&self, sender: &str) -> bool {
        let Ok(sender) = normalize_service_id(sender) else {
            return false;
        };
        match self.dm_policy {
            DmPolicy::Deny => false,
            DmPolicy::Allowlist => self.allowed_dm_sender_ids.contains(&sender),
            DmPolicy::Open => true,
        }
    }

    pub fn authorize_outbound_recipient(&self, recipient: &str) -> Result<String> {
        let recipient = normalize_service_id(recipient)?;
        let allowed = match self.dm_policy {
            DmPolicy::Deny => false,
            DmPolicy::Open if self.outbound_recipient_ids.is_empty() => true,
            DmPolicy::Open => self.outbound_recipient_ids.contains(&recipient),
            DmPolicy::Allowlist if self.outbound_recipient_ids.is_empty() => {
                self.allowed_dm_sender_ids.contains(&recipient)
            }
            DmPolicy::Allowlist => self.outbound_recipient_ids.contains(&recipient),
        };
        if !allowed {
            return Err(anyhow!(
                "Signal outbound recipient is outside the configured channel policy"
            ));
        }
        Ok(recipient)
    }

    pub fn project(&self, account_id: Option<String>) -> ChannelPolicy {
        let allowed_dm_sender_ids = self.allowed_dm_sender_ids.iter().cloned().collect();
        let allowed_outbound_conversation_ids =
            self.outbound_recipient_ids.iter().cloned().collect();
        ChannelPolicy {
            owner_id: account_id,
            allowed_sender_ids: self.allowed_dm_sender_ids.iter().cloned().collect(),
            allowed_conversation_ids: self.allowed_dm_sender_ids.iter().cloned().collect(),
            allowed_workspace_ids: Vec::new(),
            allowed_outbound_conversation_ids,
            activation: Some(InboundActivation::REASON_DIRECT_MESSAGE.to_string()),
            thread_policy: None,
            allowed_thread_ids: Vec::new(),
            dm_policy: Some(dm_policy_name(self.dm_policy).to_string()),
            allowed_dm_sender_ids,
            reply_delivery: Some(self.reply_delivery.as_str().to_string()),
            require_signature_validation: None,
            allow_group_messages: Some(false),
            max_attachment_bytes: None,
            metadata: Default::default(),
        }
    }
}

pub fn normalize_service_id(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value == "*" {
        return Err(anyhow!("Signal ServiceId must be a non-empty exact value"));
    }
    if let Some(service_id) = ServiceId::parse_from_service_id_string(value) {
        return Ok(service_id.service_id_string());
    }
    if let Some(rest) = value.strip_prefix("ACI:")
        && let Ok(uuid) = Uuid::parse_str(rest)
    {
        let service_id: ServiceId = Aci::from(uuid).into();
        return Ok(service_id.service_id_string());
    }
    let uuid = Uuid::parse_str(value)
        .map_err(|_| anyhow!("Signal ServiceId must be an ACI or PNI identifier"))?;
    let service_id: ServiceId = Aci::from(uuid).into();
    Ok(service_id.service_id_string())
}

fn normalize_service_ids(field: &str, values: &[String]) -> Result<BTreeSet<String>> {
    values
        .iter()
        .map(|value| normalize_service_id(value).map_err(|error| anyhow!("{field}: {error}")))
        .collect()
}

fn dm_policy_name(policy: DmPolicy) -> &'static str {
    match policy {
        DmPolicy::Deny => "deny",
        DmPolicy::Allowlist => "allowlist",
        DmPolicy::Open => "open",
    }
}

pub type OutboundMessage = OutboundMessageEnvelope;
pub type PluginRequest = proto::PluginRequest<ChannelConfig, OutboundMessage>;
pub type PluginRequestEnvelope = proto::PluginRequestEnvelope<PluginRequest>;

pub fn capabilities() -> ChannelCapabilities {
    ChannelCapabilities {
        plugin_id: "signal".to_string(),
        platform: "signal".to_string(),
        ingress_modes: vec![IngressMode::Polling, IngressMode::Websocket],
        outbound_message_types: vec!["text".to_string()],
        threading_model: ThreadingModel::ChatOrThread,
        attachment_support: true,
        reply_verification_support: false,
        account_scoped_config: true,
        accepts_push: true,
        accepts_status_frames: true,
        attachment_sources: vec![AttachmentSource::DataBase64],
        max_attachment_bytes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn service_id(value: u8) -> String {
        format!("00000000-0000-0000-0000-0000000000{value:02}")
    }

    fn config(dm_policy: DmPolicy, allowed: Vec<String>, outbound: Vec<String>) -> ChannelConfig {
        ChannelConfig {
            dm_policy,
            allowed_dm_sender_ids: allowed,
            outbound_recipient_ids: outbound,
            ..ChannelConfig::default()
        }
    }

    #[test]
    fn manifest_channel_capabilities_match_runtime_capabilities() {
        let manifest = manifest_channel_capability();
        let runtime = capabilities();

        assert_eq!(
            manifest["platform"].as_str().expect("platform"),
            runtime.platform
        );
        assert_eq!(
            manifest["ingress_modes"]
                .as_array()
                .expect("ingress_modes")
                .iter()
                .map(value_as_string)
                .collect::<Vec<_>>(),
            ingress_mode_names(&runtime)
        );
        assert_eq!(
            manifest["outbound_message_types"]
                .as_array()
                .expect("outbound_message_types")
                .iter()
                .map(value_as_string)
                .collect::<Vec<_>>(),
            runtime.outbound_message_types
        );
        assert_eq!(
            manifest["threading_model"]
                .as_str()
                .expect("threading_model"),
            threading_model_name(&runtime.threading_model)
        );
        assert_eq!(
            manifest["attachment_support"]
                .as_bool()
                .expect("attachment_support"),
            runtime.attachment_support
        );
        assert_eq!(
            manifest["reply_verification_support"]
                .as_bool()
                .expect("reply_verification_support"),
            runtime.reply_verification_support
        );
        assert_eq!(
            manifest["account_scoped_config"]
                .as_bool()
                .expect("account_scoped_config"),
            runtime.account_scoped_config
        );

        let delivery = &manifest["delivery"];
        assert_eq!(
            delivery["push"].as_bool().expect("delivery.push"),
            runtime.accepts_push
        );
        assert_eq!(
            delivery["status_frames"]
                .as_bool()
                .expect("delivery.status_frames"),
            runtime.accepts_status_frames
        );
        assert_eq!(
            delivery["attachment_sources"]
                .as_array()
                .expect("delivery.attachment_sources")
                .iter()
                .map(value_as_string)
                .collect::<Vec<_>>(),
            attachment_source_names(&runtime)
        );
        assert_eq!(
            delivery["max_attachment_bytes"].as_u64(),
            runtime.max_attachment_bytes
        );
    }

    #[test]
    fn absent_policy_and_empty_allowlist_do_not_start_ingress() {
        let absent = SignalChannelPolicy::from_config(&ChannelConfig::default()).unwrap();
        assert!(absent.require_persistent_ingress().is_err());
        assert!(!absent.allows_inbound_sender(&service_id(1)));
        assert!(absent.authorize_outbound_recipient(&service_id(1)).is_err());

        let empty =
            SignalChannelPolicy::from_config(&config(DmPolicy::Allowlist, vec![], vec![])).unwrap();
        assert!(empty.require_persistent_ingress().is_err());
    }

    #[test]
    fn allowlist_is_canonical_exact_and_bounds_outbound_fallback() {
        let allowed = service_id(1);
        let policy = SignalChannelPolicy::from_config(&config(
            DmPolicy::Allowlist,
            vec![format!(" ACI:{allowed} "), allowed.clone()],
            vec![],
        ))
        .unwrap();

        assert!(policy.require_persistent_ingress().is_ok());
        assert!(policy.allows_inbound_sender(&allowed));
        assert!(!policy.allows_inbound_sender(&service_id(2)));
        assert_eq!(
            policy.authorize_outbound_recipient(&allowed).unwrap(),
            allowed
        );
        assert!(policy.authorize_outbound_recipient(&service_id(2)).is_err());

        let projected = policy.project(Some(service_id(9)));
        assert_eq!(projected.dm_policy.as_deref(), Some("allowlist"));
        assert_eq!(projected.reply_delivery.as_deref(), Some("runtime_owned"));
        assert_eq!(projected.allowed_dm_sender_ids, vec![service_id(1)]);
        assert_eq!(projected.owner_id.as_deref(), Some(service_id(9).as_str()));
    }

    #[test]
    fn open_policy_allows_direct_messages_and_valid_outbound_recipients() {
        let policy =
            SignalChannelPolicy::from_config(&config(DmPolicy::Open, vec![], vec![])).unwrap();
        assert!(policy.require_persistent_ingress().is_ok());
        assert!(policy.allows_inbound_sender(&service_id(1)));
        assert_eq!(
            policy.authorize_outbound_recipient(&service_id(2)).unwrap(),
            service_id(2)
        );

        let mut narrowed_config = config(DmPolicy::Open, vec![], vec![service_id(1)]);
        narrowed_config.reply_delivery = ReplyDelivery::ToolOwned;
        let narrowed = SignalChannelPolicy::from_config(&narrowed_config).unwrap();
        assert!(
            narrowed
                .authorize_outbound_recipient(&service_id(1))
                .is_ok()
        );
        assert!(
            narrowed
                .authorize_outbound_recipient(&service_id(2))
                .is_err()
        );
    }

    #[test]
    fn policy_rejects_wildcards_invalid_values_and_wider_outbound() {
        assert!(
            SignalChannelPolicy::from_config(&config(
                DmPolicy::Allowlist,
                vec!["*".to_string()],
                vec![],
            ))
            .is_err()
        );
        assert!(
            SignalChannelPolicy::from_config(&config(
                DmPolicy::Allowlist,
                vec![service_id(1)],
                vec![service_id(2)],
            ))
            .is_err()
        );
        assert!(
            SignalChannelPolicy::from_config(&config(
                DmPolicy::Allowlist,
                vec![service_id(1), service_id(2)],
                vec![service_id(1)],
            ))
            .is_err()
        );
        assert!(
            SignalChannelPolicy::from_config(&config(DmPolicy::Open, vec![], vec![service_id(1)],))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ChannelConfig>(serde_json::json!({
                "dm_policy": "pairing"
            }))
            .is_err()
        );
    }

    #[test]
    fn reply_ownership_is_explicit_and_defaults_to_runtime() {
        let default_policy = SignalChannelPolicy::from_config(&ChannelConfig::default()).unwrap();
        assert_eq!(
            default_policy.project(None).reply_delivery.as_deref(),
            Some("runtime_owned")
        );
        let tool_owned = SignalChannelPolicy::from_config(&ChannelConfig {
            dm_policy: DmPolicy::Open,
            reply_delivery: ReplyDelivery::ToolOwned,
            ..ChannelConfig::default()
        })
        .unwrap();
        assert_eq!(
            tool_owned.project(None).reply_delivery.as_deref(),
            Some("tool_owned")
        );
    }

    fn manifest_channel_capability() -> Value {
        let manifest: Value =
            serde_json::from_str(include_str!("../channel-plugin.json")).expect("parse manifest");
        manifest["capabilities"]["channel"].clone()
    }

    fn ingress_mode_names(capabilities: &ChannelCapabilities) -> Vec<String> {
        capabilities.ingress_modes.iter().map(enum_name).collect()
    }

    fn threading_model_name(model: &ThreadingModel) -> String {
        enum_name(model)
    }

    fn attachment_source_names(capabilities: &ChannelCapabilities) -> Vec<String> {
        capabilities
            .attachment_sources
            .iter()
            .map(enum_name)
            .collect()
    }

    fn enum_name<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_value(value)
            .expect("serialize enum")
            .as_str()
            .expect("enum wire name")
            .to_string()
    }

    fn value_as_string(value: &Value) -> String {
        value.as_str().expect("string value").to_string()
    }
}
