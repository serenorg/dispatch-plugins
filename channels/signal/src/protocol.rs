use dispatch_channel_protocol as proto;
use serde::{Deserialize, Serialize};

// The migration to presage is being landed incrementally. Each feature
// commit wires up another family of plugin operations and starts using
// more of the re-exports below. Until the migration finishes some of
// these are only touched by the channel-plugin.json manifest conformance
// test, so silence `unused_imports` at the `pub use` boundary rather
// than churning the re-export list on every commit.
#[allow(unused_imports)]
pub use proto::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressMode, IngressState, OutboundAttachment,
    OutboundMessageEnvelope, PluginResponse, StatusAcceptance, StatusFrame, StatusKind,
    ThreadingModel, parse_jsonrpc_request, plugin_error, response_to_jsonrpc,
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
    /// Clamped to at least 1 second when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_timeout_secs: Option<u16>,
}

pub type OutboundMessage = OutboundMessageEnvelope;
pub type PluginRequest = proto::PluginRequest<ChannelConfig, OutboundMessage>;
pub type PluginRequestEnvelope = proto::PluginRequestEnvelope<PluginRequest>;

pub fn capabilities() -> ChannelCapabilities {
    ChannelCapabilities {
        plugin_id: "signal".to_string(),
        platform: "signal".to_string(),
        ingress_modes: vec![IngressMode::Polling],
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
            delivery["max_attachment_bytes"]
                .as_u64()
                .map(|value| value as u64),
            runtime.max_attachment_bytes
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
