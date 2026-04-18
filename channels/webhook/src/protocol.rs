use dispatch_channel_protocol as proto;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use proto::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressCallbackReply, IngressMode, IngressPayload,
    IngressState, OutboundAttachment, PluginResponse, StatusAcceptance, StatusFrame, StatusKind,
    ThreadingModel, plugin_error,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_public_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_secret_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound_bearer_token_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck_url: Option<String>,
    #[serde(default)]
    pub static_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OutboundMessage {
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<OutboundAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

pub type PluginRequest = proto::PluginRequest<ChannelConfig, OutboundMessage>;
pub type PluginRequestEnvelope = proto::PluginRequestEnvelope<PluginRequest>;

pub fn capabilities() -> ChannelCapabilities {
    ChannelCapabilities {
        plugin_id: "webhook".to_string(),
        platform: "webhook".to_string(),
        ingress_modes: vec![IngressMode::Webhook],
        outbound_message_types: vec!["json".to_string(), "text".to_string()],
        threading_model: ThreadingModel::CallerDefined,
        attachment_support: true,
        reply_verification_support: true,
        account_scoped_config: true,
        accepts_push: true,
        accepts_status_frames: true,
        attachment_sources: vec![
            AttachmentSource::DataBase64,
            AttachmentSource::Url,
            AttachmentSource::StorageKey,
        ],
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
