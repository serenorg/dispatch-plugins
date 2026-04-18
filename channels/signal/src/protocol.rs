use dispatch_channel_protocol as proto;
use serde::{Deserialize, Serialize};

pub use proto::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities,
    ChannelEventNotification, ConfiguredChannel, DeliveryReceipt, HealthReport, InboundActor,
    InboundAttachment, InboundConversationRef, InboundEventEnvelope, InboundMessage, IngressMode,
    IngressState, OutboundMessageEnvelope, PluginNotificationEnvelope, PluginResponse,
    StatusAcceptance, StatusFrame, StatusKind, ThreadingModel, notification_to_jsonrpc,
    parse_jsonrpc_request, plugin_error, response_to_jsonrpc,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_recipient: Option<String>,
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
