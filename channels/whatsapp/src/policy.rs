use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};

use whatsapp_rust::Jid;

use crate::protocol::{ChannelConfig, ChannelPolicy};
use crate::session::{RegistrationSummary, SessionState, load_session};

const DM_POLICY_DENY: &str = "deny";
const DM_POLICY_ALLOWLIST: &str = "allowlist";
const DM_POLICY_OPEN: &str = "open";
const REPLY_DELIVERY_RUNTIME_OWNED: &str = "runtime_owned";
const REPLY_DELIVERY_TOOL_OWNED: &str = "tool_owned";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmPolicy {
    Deny,
    Allowlist,
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyDelivery {
    RuntimeOwned,
    ToolOwned,
}

/// The normalized direct-message surface for every WhatsApp operation.
///
/// WhatsApp group conversations are deliberately absent until the plugin has a
/// separately reviewed group policy. An authenticated WhatsApp session is not
/// itself an authorization grant for every direct message it can receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsAppPolicy {
    self_chat_jids: Vec<Jid>,
    dm_policy: DmPolicy,
    allowed_dm_sender_ids: Vec<String>,
    outbound_recipient_ids: Vec<String>,
    reply_delivery: ReplyDelivery,
}

impl WhatsAppPolicy {
    pub fn from_config(config: &ChannelConfig) -> Result<Self> {
        if config.self_chat_only {
            let SessionState::Registered { summary, .. } = load_session(config)? else {
                bail!("WhatsApp self-chat mode requires a linked session");
            };
            return Self::for_self_chat(config, &summary);
        }
        let dm_policy = match config.dm_policy.as_deref() {
            None | Some(DM_POLICY_DENY) => DmPolicy::Deny,
            Some(DM_POLICY_ALLOWLIST) => DmPolicy::Allowlist,
            Some(DM_POLICY_OPEN) => DmPolicy::Open,
            Some(other) => bail!(
                "unknown WhatsApp dm_policy `{other}`; expected `{DM_POLICY_DENY}`, `{DM_POLICY_ALLOWLIST}`, or `{DM_POLICY_OPEN}`"
            ),
        };
        let reply_delivery = match config.reply_delivery.as_deref() {
            None | Some(REPLY_DELIVERY_RUNTIME_OWNED) => ReplyDelivery::RuntimeOwned,
            Some(REPLY_DELIVERY_TOOL_OWNED) => ReplyDelivery::ToolOwned,
            Some(other) => bail!(
                "unknown WhatsApp reply_delivery `{other}`; expected `{REPLY_DELIVERY_RUNTIME_OWNED}` or `{REPLY_DELIVERY_TOOL_OWNED}`"
            ),
        };
        let allowed_dm_sender_ids =
            normalize_direct_jids(&config.allowed_dm_sender_ids, "allowed_dm_sender_ids")?;
        let explicit_outbound =
            normalize_direct_jids(&config.outbound_recipient_ids, "outbound_recipient_ids")?;

        if dm_policy == DmPolicy::Allowlist && allowed_dm_sender_ids.is_empty() {
            bail!(
                "WhatsApp dm_policy `{DM_POLICY_ALLOWLIST}` requires at least one allowed_dm_sender_ids entry"
            );
        }
        if dm_policy != DmPolicy::Allowlist && !allowed_dm_sender_ids.is_empty() {
            bail!(
                "WhatsApp allowed_dm_sender_ids is only meaningful with dm_policy `{DM_POLICY_ALLOWLIST}`; `{DM_POLICY_DENY}` and `{DM_POLICY_OPEN}` require it to be empty"
            );
        }

        let outbound_recipient_ids = if config.outbound_recipient_ids.is_empty() {
            allowed_dm_sender_ids.clone()
        } else {
            explicit_outbound
        };
        if dm_policy == DmPolicy::Deny && !outbound_recipient_ids.is_empty() {
            bail!("WhatsApp outbound_recipient_ids requires an authorized direct-message surface");
        }
        if dm_policy == DmPolicy::Allowlist
            && outbound_recipient_ids
                .iter()
                .any(|recipient| !allowed_dm_sender_ids.contains(recipient))
        {
            bail!("WhatsApp outbound_recipient_ids must be a subset of allowed_dm_sender_ids");
        }
        if reply_delivery == ReplyDelivery::RuntimeOwned {
            match dm_policy {
                DmPolicy::Allowlist
                    if allowed_dm_sender_ids
                        .iter()
                        .any(|sender| !outbound_recipient_ids.contains(sender)) =>
                {
                    bail!(
                        "WhatsApp runtime_owned reply delivery requires outbound_recipient_ids to include every allowed_dm_sender_ids entry"
                    );
                }
                DmPolicy::Open if !outbound_recipient_ids.is_empty() => {
                    bail!(
                        "WhatsApp runtime_owned reply delivery requires an empty outbound_recipient_ids fallback when dm_policy is `open`"
                    );
                }
                DmPolicy::Deny | DmPolicy::Allowlist | DmPolicy::Open => {}
            }
        }

        Ok(Self {
            self_chat_jids: Vec::new(),
            dm_policy,
            allowed_dm_sender_ids,
            outbound_recipient_ids,
            reply_delivery,
        })
    }

    pub(crate) fn for_self_chat(
        config: &ChannelConfig,
        summary: &RegistrationSummary,
    ) -> Result<Self> {
        let mut regular_config = config.clone();
        regular_config.self_chat_only = false;
        let mut policy = Self::from_config(&regular_config)?;
        policy.validate_for_ingress()?;
        let own_jids = [summary.phone_jid.as_deref(), summary.lid_jid.as_deref()]
            .into_iter()
            .flatten()
            .map(|raw| {
                normalize_direct_jid(raw, "linked account")?
                    .parse::<Jid>()
                    .map_err(|_| anyhow!("WhatsApp linked account JID is invalid"))
            })
            .collect::<Result<Vec<_>>>()?;
        if own_jids.is_empty() || own_jids.iter().any(|jid| jid.device == 0) {
            bail!("WhatsApp self-chat mode requires a linked companion identity");
        }
        policy.self_chat_jids = own_jids;
        for raw in policy
            .allowed_dm_sender_ids
            .iter()
            .chain(policy.outbound_recipient_ids.iter())
            .chain(config.default_recipient.iter())
        {
            let jid = normalize_direct_jid(raw, "self-chat recipient")?
                .parse::<Jid>()
                .map_err(|_| anyhow!("WhatsApp self-chat recipient is invalid"))?;
            if !policy.is_own_jid(&jid) {
                bail!(
                    "WhatsApp self-chat mode permits only the linked account in sender, outbound, and default recipient fields"
                );
            }
        }
        let ids: BTreeSet<_> = policy
            .self_chat_jids
            .iter()
            .map(|jid| jid.to_non_ad().to_string())
            .collect();
        policy.dm_policy = DmPolicy::Allowlist;
        policy.allowed_dm_sender_ids = ids.into_iter().collect();
        policy.outbound_recipient_ids = policy.allowed_dm_sender_ids.clone();
        Ok(policy)
    }

    pub fn self_chat_account_id(&self) -> Option<String> {
        self.self_chat_jids
            .first()
            .map(|jid| jid.to_non_ad().to_string())
    }

    fn is_own_jid(&self, jid: &Jid) -> bool {
        let Some(primary) = self.self_chat_jids.first() else {
            return false;
        };
        // matches_user_or_lid ignores the namespace as well as the device.
        // Require the matching PN/LID namespace so unrelated equal numbers
        // in the other namespace cannot enter the account's scope.
        jid.matches_user_or_lid(primary, self.self_chat_jids.get(1))
            && self
                .self_chat_jids
                .iter()
                .any(|own| own.server == jid.server && own.user == jid.user)
    }

    pub fn accepts_self_chat(&self, chat: &Jid, sender: &Jid) -> bool {
        self.is_own_jid(chat)
            && self.is_own_jid(sender)
            && !self
                .self_chat_jids
                .iter()
                .any(|own| own.device == sender.device)
    }

    pub fn validate_for_ingress(&self) -> Result<()> {
        if self.dm_policy == DmPolicy::Deny {
            bail!("WhatsApp persistent ingress has no authorized direct-message surface");
        }
        Ok(())
    }

    pub fn allows_inbound_sender(&self, sender_id: &str) -> bool {
        if !self.self_chat_jids.is_empty() {
            return sender_id
                .parse::<Jid>()
                .is_ok_and(|jid| self.is_own_jid(&jid));
        }
        matches!(self.dm_policy, DmPolicy::Open)
            || self
                .allowed_dm_sender_ids
                .iter()
                .any(|allowed| allowed == sender_id)
    }

    pub fn allows_outbound_recipient(&self, recipient_id: &str) -> bool {
        if !self.self_chat_jids.is_empty() {
            return recipient_id
                .parse::<Jid>()
                .is_ok_and(|jid| self.is_own_jid(&jid));
        }
        (matches!(self.dm_policy, DmPolicy::Open) && self.outbound_recipient_ids.is_empty())
            || self
                .outbound_recipient_ids
                .iter()
                .any(|allowed| allowed == recipient_id)
    }

    pub fn bridge_delivers(&self) -> bool {
        self.reply_delivery == ReplyDelivery::RuntimeOwned
    }

    pub fn to_channel_policy(&self) -> ChannelPolicy {
        ChannelPolicy {
            owner_id: self.self_chat_account_id(),
            allowed_sender_ids: if self.self_chat_jids.is_empty() {
                Vec::new()
            } else {
                self.allowed_dm_sender_ids.clone()
            },
            allowed_conversation_ids: self.allowed_dm_sender_ids.clone(),
            allowed_workspace_ids: Vec::new(),
            allowed_outbound_conversation_ids: self.outbound_recipient_ids.clone(),
            activation: Some("direct_message".to_string()),
            thread_policy: None,
            allowed_thread_ids: Vec::new(),
            dm_policy: Some(self.dm_policy_name().to_string()),
            allowed_dm_sender_ids: self.allowed_dm_sender_ids.clone(),
            reply_delivery: Some(
                if self.bridge_delivers() {
                    REPLY_DELIVERY_RUNTIME_OWNED
                } else {
                    REPLY_DELIVERY_TOOL_OWNED
                }
                .to_string(),
            ),
            require_signature_validation: Some(true),
            allow_group_messages: Some(false),
            max_attachment_bytes: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn diagnostics(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "allowed_dm_sender_count".to_string(),
                self.allowed_dm_sender_ids.len().to_string(),
            ),
            (
                "outbound_recipient_count".to_string(),
                self.outbound_recipient_ids.len().to_string(),
            ),
            ("dm_policy".to_string(), self.dm_policy_name().to_string()),
            (
                "reply_delivery".to_string(),
                self.reply_delivery_name().to_string(),
            ),
        ])
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
}

pub fn normalize_direct_jid(raw: &str, field: &str) -> Result<String> {
    let normalized = raw.trim();
    if normalized.is_empty() {
        bail!("WhatsApp {field} does not accept an empty recipient");
    }
    if normalized == "*" {
        bail!("WhatsApp {field} does not accept wildcard recipients");
    }
    if !normalized.ends_with("@s.whatsapp.net") && !normalized.ends_with("@lid") {
        bail!(
            "WhatsApp {field} only accepts direct-message JIDs ending in `@s.whatsapp.net` or `@lid`"
        );
    }
    normalized
        .parse::<whatsapp_rust::Jid>()
        .map_err(|_| anyhow!("WhatsApp {field} contains an invalid direct-message JID"))?;
    Ok(normalized.to_string())
}

fn normalize_direct_jids(raw_ids: &[String], field: &str) -> Result<Vec<String>> {
    let mut normalized = BTreeSet::new();
    for raw_id in raw_ids {
        normalized.insert(normalize_direct_jid(raw_id, field)?);
    }
    Ok(normalized.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: &str = "15551234567@s.whatsapp.net";
    const OTHER: &str = "15557654321@s.whatsapp.net";

    fn config() -> ChannelConfig {
        ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec![format!("  {SENDER}  "), SENDER.to_string()],
            ..ChannelConfig::default()
        }
    }

    #[test]
    fn self_chat_policy_derives_only_account_aliases_and_rejects_conflicts() {
        let summary = RegistrationSummary {
            phone_jid: Some(format!(
                "{}:7@s.whatsapp.net",
                SENDER.split('@').next().unwrap()
            )),
            lid_jid: Some("987654321:7@lid".to_string()),
            device_id: 1,
            push_name: None,
        };
        let config = ChannelConfig {
            self_chat_only: true,
            dm_policy: Some("open".to_string()),
            ..ChannelConfig::default()
        };
        let policy = WhatsAppPolicy::for_self_chat(&config, &summary).unwrap();
        for own in [SENDER, "987654321@lid", "987654321:3@lid"] {
            assert!(policy.allows_inbound_sender(own));
            assert!(policy.allows_outbound_recipient(own));
        }
        for other in [
            OTHER,
            "15551234567@lid",
            "987654321@s.whatsapp.net",
            "987654321@g.us",
        ] {
            assert!(!policy.allows_inbound_sender(other));
            assert!(!policy.allows_outbound_recipient(other));
        }
        let projected = policy.to_channel_policy();
        assert_eq!(projected.dm_policy.as_deref(), Some("allowlist"));
        assert_eq!(
            projected.allowed_dm_sender_ids,
            vec![SENDER, "987654321@lid"]
        );
        assert_eq!(
            projected.allowed_conversation_ids,
            projected.allowed_dm_sender_ids
        );
        assert_eq!(
            projected.allowed_outbound_conversation_ids,
            projected.allowed_dm_sender_ids
        );
        for conflict in [
            ChannelConfig {
                dm_policy: None,
                ..config.clone()
            },
            ChannelConfig {
                dm_policy: Some("deny".to_string()),
                ..config.clone()
            },
            ChannelConfig {
                dm_policy: Some("allowlist".to_string()),
                allowed_dm_sender_ids: vec![OTHER.to_string()],
                ..config.clone()
            },
            ChannelConfig {
                outbound_recipient_ids: vec![OTHER.to_string()],
                reply_delivery: Some("tool_owned".to_string()),
                ..config.clone()
            },
            ChannelConfig {
                default_recipient: Some(OTHER.to_string()),
                ..config.clone()
            },
        ] {
            assert!(WhatsAppPolicy::for_self_chat(&conflict, &summary).is_err());
        }
        assert!(
            WhatsAppPolicy::for_self_chat(
                &ChannelConfig {
                    dm_policy: Some("allowlist".to_string()),
                    allowed_dm_sender_ids: vec![SENDER.to_string()],
                    ..config
                },
                &summary
            )
            .is_ok()
        );
        assert!(
            !serde_json::from_str::<ChannelConfig>("{}")
                .unwrap()
                .self_chat_only
        );
    }

    #[test]
    fn allowlist_normalizes_and_falls_back_for_outbound() {
        let policy = WhatsAppPolicy::from_config(&config()).unwrap();
        policy.validate_for_ingress().unwrap();
        assert!(policy.allows_inbound_sender(SENDER));
        assert!(!policy.allows_inbound_sender(OTHER));
        assert!(policy.allows_outbound_recipient(SENDER));
        assert!(!policy.allows_outbound_recipient(OTHER));
        assert!(policy.bridge_delivers());
    }

    #[test]
    fn absent_or_deny_policy_cannot_start_ingress() {
        let absent = WhatsAppPolicy::from_config(&ChannelConfig::default()).unwrap();
        assert!(absent.validate_for_ingress().is_err());

        let denied = WhatsAppPolicy::from_config(&ChannelConfig {
            dm_policy: Some(DM_POLICY_DENY.to_string()),
            ..ChannelConfig::default()
        })
        .unwrap();
        assert!(denied.validate_for_ingress().is_err());
    }

    #[test]
    fn open_is_explicit_unbounded_direct_message_surface() {
        let policy = WhatsAppPolicy::from_config(&ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            reply_delivery: Some(REPLY_DELIVERY_TOOL_OWNED.to_string()),
            ..ChannelConfig::default()
        })
        .unwrap();
        policy.validate_for_ingress().unwrap();
        assert!(policy.allows_inbound_sender(OTHER));
        assert!(policy.allows_outbound_recipient(OTHER));
        assert!(!policy.bridge_delivers());

        let narrowed = WhatsAppPolicy::from_config(&ChannelConfig {
            dm_policy: Some(DM_POLICY_OPEN.to_string()),
            outbound_recipient_ids: vec![SENDER.to_string()],
            reply_delivery: Some(REPLY_DELIVERY_TOOL_OWNED.to_string()),
            ..ChannelConfig::default()
        })
        .unwrap();
        assert!(narrowed.allows_outbound_recipient(SENDER));
        assert!(!narrowed.allows_outbound_recipient(OTHER));
    }

    #[test]
    fn rejects_empty_allowlist_wildcards_groups_and_unknown_modes() {
        assert!(
            WhatsAppPolicy::from_config(&ChannelConfig {
                dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
                ..ChannelConfig::default()
            })
            .is_err()
        );
        assert!(
            WhatsAppPolicy::from_config(&ChannelConfig {
                dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
                allowed_dm_sender_ids: vec!["*".to_string()],
                ..ChannelConfig::default()
            })
            .is_err()
        );
        assert!(
            WhatsAppPolicy::from_config(&ChannelConfig {
                dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
                allowed_dm_sender_ids: vec!["120363161500776365@g.us".to_string()],
                ..ChannelConfig::default()
            })
            .is_err()
        );
        assert!(
            WhatsAppPolicy::from_config(&ChannelConfig {
                dm_policy: Some("pairing".to_string()),
                ..ChannelConfig::default()
            })
            .is_err()
        );
    }

    #[test]
    fn outbound_scope_cannot_widen_an_allowlist() {
        assert!(
            WhatsAppPolicy::from_config(&ChannelConfig {
                dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
                allowed_dm_sender_ids: vec![SENDER.to_string()],
                outbound_recipient_ids: vec![OTHER.to_string()],
                ..ChannelConfig::default()
            })
            .is_err()
        );
        assert!(
            WhatsAppPolicy::from_config(&ChannelConfig {
                dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
                allowed_dm_sender_ids: vec![SENDER.to_string(), OTHER.to_string()],
                outbound_recipient_ids: vec![SENDER.to_string()],
                reply_delivery: Some(REPLY_DELIVERY_RUNTIME_OWNED.to_string()),
                ..ChannelConfig::default()
            })
            .is_err()
        );
        assert!(
            WhatsAppPolicy::from_config(&ChannelConfig {
                dm_policy: Some(DM_POLICY_OPEN.to_string()),
                outbound_recipient_ids: vec![SENDER.to_string()],
                reply_delivery: Some(REPLY_DELIVERY_RUNTIME_OWNED.to_string()),
                ..ChannelConfig::default()
            })
            .is_err()
        );
    }

    #[test]
    fn policy_projection_preserves_direct_message_scope_and_delivery_owner() {
        let policy = WhatsAppPolicy::from_config(&ChannelConfig {
            dm_policy: Some(DM_POLICY_ALLOWLIST.to_string()),
            allowed_dm_sender_ids: vec![SENDER.to_string()],
            reply_delivery: Some(REPLY_DELIVERY_TOOL_OWNED.to_string()),
            ..ChannelConfig::default()
        })
        .unwrap();
        let projected = policy.to_channel_policy();
        assert_eq!(projected.activation.as_deref(), Some("direct_message"));
        assert_eq!(projected.dm_policy.as_deref(), Some(DM_POLICY_ALLOWLIST));
        assert_eq!(projected.allowed_dm_sender_ids, vec![SENDER]);
        assert_eq!(projected.allowed_conversation_ids, vec![SENDER]);
        assert_eq!(projected.allowed_outbound_conversation_ids, vec![SENDER]);
        assert_eq!(
            projected.reply_delivery.as_deref(),
            Some(REPLY_DELIVERY_TOOL_OWNED)
        );
        assert_eq!(projected.allow_group_messages, Some(false));
    }
}
