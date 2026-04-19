use channel_email_core::EmailPreset;

pub struct Preset;

impl EmailPreset for Preset {
    const PLUGIN_ID: &'static str = "outlook";
    const PLATFORM: &'static str = "outlook";
    const DEFAULT_IMAP_HOST: Option<&'static str> = Some("outlook.office365.com");
    const DEFAULT_SMTP_HOST: Option<&'static str> = Some("smtp.office365.com");
    const DEFAULT_IMAP_PASSWORD_ENV: &'static str = "OUTLOOK_EMAIL_PASSWORD";
    const DEFAULT_SMTP_PASSWORD_ENV: &'static str = "OUTLOOK_EMAIL_PASSWORD";
}

#[cfg(test)]
mod tests {
    use super::*;
    use channel_email_core::{ChannelCapabilities, ThreadingModel};
    use channel_schema::{ChannelCapabilityManifest, ExtensionManifest};

    #[test]
    fn manifest_channel_capabilities_match_runtime_capabilities() {
        let manifest = manifest_channel_capability();
        let runtime = channel_email_core::capabilities::<Preset>();

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
