use dispatch_channel_protocol as proto;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use proto::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities, ChannelPolicy,
    ConfiguredChannel, DeliveryReceipt, HealthReport, InboundActivation, InboundActor,
    InboundAttachment, InboundConversationRef, InboundEventEnvelope, InboundMessage,
    IngressCallbackReply, IngressMode, IngressPayload, IngressState, OutboundAttachment,
    PluginResponse, StatusAcceptance, StatusFrame, StatusKind, ThreadingModel,
    parse_jsonrpc_request, plugin_error, response_to_jsonrpc,
};

/// Discord binding configuration.
///
/// Every scope field is an allowlist that is empty by default, and an empty
/// allowlist denies. Provider permission to read or post in a channel is the
/// outer ceiling of what the bot account can reach; it is not authorization for
/// this binding, so the binding states its own surface explicitly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_public_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_public_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,
    /// Outbound fallback destination. Never grants ingress on its own, and is
    /// still checked against the outbound allowlist before delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_content_intent: Option<bool>,
    /// Guilds (Discord servers) this binding may act within.
    #[serde(default)]
    pub allowed_guild_ids: Vec<String>,
    /// Channels this binding may receive from. A literal `*` entry is the only
    /// wildcard, and it widens channel scope only inside `allowed_guild_ids`.
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
    /// `deny`, `inherit_parent`, or `allowlist`. Absent means `deny`, because a
    /// Discord thread is a separate conversation from the channel that holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_policy: Option<String>,
    /// Threads this binding may receive from under the `allowlist` thread policy.
    #[serde(default)]
    pub allowed_thread_ids: Vec<String>,
    /// `mention_or_reply`, `slash_command`, or `all_messages`. Absent means
    /// `mention_or_reply`, so a shared channel wakes the agent only when a
    /// message addresses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<String>,
    /// Senders permitted to open a direct message under the `allowlist` DM
    /// policy. Must stay empty under `deny` and `open`.
    #[serde(default)]
    pub allowed_dm_sender_ids: Vec<String>,
    /// Destinations this binding may publish to. Empty falls back to
    /// `allowed_channel_ids`, so outbound scope is never wider than inbound.
    #[serde(default)]
    pub outbound_channel_ids: Vec<String>,
    /// `runtime_owned` or `tool_owned`. Absent means `runtime_owned`, so exactly
    /// one component owns reply delivery for an inbound turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_delivery: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    /// Senders permitted in allowed guild channels. Empty accepts any sender
    /// inside an allowed channel, where channel scope and activation are the
    /// boundary; it does not widen channel scope.
    #[serde(default)]
    pub allowed_sender_ids: Vec<String>,
    /// `deny`, `allowlist`, or `open`. Absent means `deny`. An unrecognized
    /// value is a configuration error, never a fallback to allow. `open`
    /// authorizes any sender and requires an empty `allowed_dm_sender_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dm_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OutboundMessage {
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<OutboundAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

pub type PluginRequest = proto::PluginRequest<ChannelConfig, OutboundMessage>;
pub type PluginRequestEnvelope = proto::PluginRequestEnvelope<PluginRequest>;

pub fn capabilities() -> ChannelCapabilities {
    ChannelCapabilities {
        plugin_id: "discord".to_string(),
        platform: "discord".to_string(),
        ingress_modes: vec![IngressMode::InteractionWebhook, IngressMode::Websocket],
        outbound_message_types: vec!["text".to_string()],
        threading_model: ThreadingModel::ChannelOrThread,
        attachment_support: true,
        reply_verification_support: true,
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
    use channel_schema::{ChannelCapabilityManifest, ExtensionManifest};

    #[test]
    fn manifest_channel_capabilities_match_runtime_capabilities() {
        let manifest = manifest_channel_capability();
        let runtime = capabilities();

        assert_eq!(manifest.platform, runtime.platform);
        assert_eq!(manifest.ingress_modes, ingress_mode_names(&runtime));
        assert_eq!(
            manifest.outbound_message_types,
            runtime.outbound_message_types
        );
        assert_eq!(
            manifest.threading_model,
            threading_model_name(&runtime.threading_model)
        );
        assert_eq!(manifest.attachment_support, runtime.attachment_support);
        assert_eq!(
            manifest.reply_verification_support,
            runtime.reply_verification_support
        );
        assert_eq!(
            manifest.account_scoped_config,
            runtime.account_scoped_config
        );

        let delivery = manifest.delivery.expect("manifest delivery settings");
        assert_eq!(delivery.push, runtime.accepts_push);
        assert_eq!(delivery.status_frames, runtime.accepts_status_frames);
        assert_eq!(
            delivery.attachment_sources,
            attachment_source_names(&runtime)
        );
        assert_eq!(delivery.max_attachment_bytes, runtime.max_attachment_bytes);
    }

    fn manifest_channel_capability() -> ChannelCapabilityManifest {
        let manifest: ExtensionManifest =
            serde_json::from_str(include_str!("../channel-plugin.json")).expect("parse manifest");
        manifest
            .capabilities
            .channel
            .expect("channel capability manifest")
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
}
