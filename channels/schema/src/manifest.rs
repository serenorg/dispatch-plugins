use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    Channel,
    Courier,
    Connector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub kind: ManifestKind,
    pub name: String,
    pub version: String,
    pub protocol: String,
    pub protocol_version: u32,
    pub description: String,
    pub entrypoint: ExtensionEntrypoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuthManifest>,
    #[serde(default)]
    pub capabilities: ExtensionCapabilitiesManifest,
    #[serde(default)]
    pub requirements: ExtensionRequirements,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionEntrypoint {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionCapabilitiesManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelCapabilityManifest>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapManifest {
    #[serde(default)]
    pub credentials: Vec<BootstrapCredential>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapCredential {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<BootstrapGenerator>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapGenerator {
    pub strategy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderAuthManifest {
    pub mode: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_url: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub shared_credentials: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelCapabilityManifest {
    pub platform: String,
    #[serde(default)]
    pub ingress_modes: Vec<String>,
    #[serde(default)]
    pub outbound_message_types: Vec<String>,
    pub threading_model: String,
    pub attachment_support: bool,
    pub reply_verification_support: bool,
    pub account_scoped_config: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_polling: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret_support: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_edits: Option<bool>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<ChannelIngressManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ChannelDeliveryManifest>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionRequirements {
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub optional_secrets: Vec<String>,
    #[serde(default)]
    pub network_domains: Vec<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelIngressManifest {
    #[serde(default)]
    pub endpoints: Vec<IngressEndpointManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polling: Option<PollingManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<IngressTrustManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressEndpointManifest {
    pub path: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub host_managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollingManifest {
    pub min_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_interval_ms: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressTrustManifest {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_key_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_secret_name: Option<String>,
    #[serde(default)]
    pub host_managed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelDeliveryManifest {
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub status_frames: bool,
    #[serde(default)]
    pub attachment_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attachment_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_manifest_supports_bootstrap_auth_and_ingress_metadata() {
        let manifest = ExtensionManifest {
            schema: Some(
                "https://serenorg.github.io/dispatch/schemas/extension.v1.json".to_string(),
            ),
            kind: ManifestKind::Channel,
            name: "channel-telegram".to_string(),
            version: "0.1.0".to_string(),
            protocol: "jsonl".to_string(),
            protocol_version: 1,
            description: "Telegram channel plugin for Dispatch.".to_string(),
            entrypoint: ExtensionEntrypoint {
                command: "../../target/release/channel-telegram".to_string(),
                args: vec![],
            },
            bootstrap: Some(BootstrapManifest {
                credentials: vec![BootstrapCredential {
                    name: "TELEGRAM_WEBHOOK_SECRET".to_string(),
                    prompt: "Optional webhook secret".to_string(),
                    optional: true,
                    format_hint: None,
                    generator: Some(BootstrapGenerator {
                        strategy: "hex".to_string(),
                        bytes: Some(64),
                    }),
                }],
                setup_url: Some("https://t.me/BotFather".to_string()),
                verification_endpoint: Some(
                    "https://api.telegram.org/bot{TELEGRAM_BOT_TOKEN}/getMe".to_string(),
                ),
            }),
            auth: Some(ProviderAuthManifest {
                mode: "manual".to_string(),
                provider: "Telegram".to_string(),
                setup_url: Some("https://t.me/BotFather".to_string()),
                scopes: vec![],
                shared_credentials: vec![],
            }),
            capabilities: ExtensionCapabilitiesManifest {
                channel: Some(ChannelCapabilityManifest {
                    platform: "telegram".to_string(),
                    ingress_modes: vec!["webhook".to_string()],
                    outbound_message_types: vec!["text".to_string()],
                    threading_model: "chat_or_topic".to_string(),
                    attachment_support: false,
                    reply_verification_support: true,
                    account_scoped_config: true,
                    allow_polling: Some(false),
                    webhook_secret_support: Some(true),
                    status_updates: Some(false),
                    message_edits: None,
                    allowed_paths: vec!["/telegram/updates".to_string()],
                    callback_timeout_secs: Some(30),
                    ingress: Some(ChannelIngressManifest {
                        endpoints: vec![IngressEndpointManifest {
                            path: "/telegram/updates".to_string(),
                            methods: vec!["POST".to_string()],
                            host_managed: true,
                        }],
                        polling: None,
                        trust: Some(IngressTrustManifest {
                            mode: "shared_secret_header".to_string(),
                            header_name: Some("X-Telegram-Bot-Api-Secret-Token".to_string()),
                            secret_name: Some("TELEGRAM_WEBHOOK_SECRET".to_string()),
                            signature_key_name: None,
                            hmac_secret_name: None,
                            host_managed: true,
                        }),
                    }),
                    delivery: Some(ChannelDeliveryManifest {
                        push: true,
                        status_frames: true,
                        attachment_sources: vec![
                            "data_base64".to_string(),
                            "url".to_string(),
                            "storage_key".to_string(),
                        ],
                        max_attachment_bytes: None,
                    }),
                }),
            },
            requirements: ExtensionRequirements {
                secrets: vec!["TELEGRAM_BOT_TOKEN".to_string()],
                optional_secrets: vec!["TELEGRAM_WEBHOOK_SECRET".to_string()],
                network_domains: vec!["api.telegram.org".to_string()],
                platforms: vec!["telegram".to_string()],
            },
        };

        let value = serde_json::to_value(&manifest).expect("serialize manifest");
        let parsed: ExtensionManifest =
            serde_json::from_value(value).expect("deserialize manifest");

        assert_eq!(parsed, manifest);
    }
}
