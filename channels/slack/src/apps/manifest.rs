use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AppError, AppResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppIdentity {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub background_color: Option<String>,
}

impl AppIdentity {
    pub fn validate(&self) -> AppResult<()> {
        if self.name.trim() != self.name
            || self.name.is_empty()
            || self.name.chars().count() > 35
            || self.name.chars().any(char::is_control)
        {
            return Err(AppError::invalid("name"));
        }
        if self.display_name.is_empty()
            || self.display_name.len() > 80
            || !self.display_name.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
            })
        {
            return Err(AppError::invalid("display_name"));
        }
        if self.description.trim().is_empty()
            || self.description.chars().count() > 140
            || self.description.chars().any(char::is_control)
        {
            return Err(AppError::invalid("description"));
        }
        if let Some(color) = &self.background_color {
            let hex = color.strip_prefix('#').unwrap_or_default();
            if !matches!(hex.len(), 3 | 6) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(AppError::invalid("background_color"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppOptions {
    pub direct_messages: bool,
    pub private_channels: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppScope {
    #[serde(rename = "app_mentions:read")]
    AppMentionsRead,
    #[serde(rename = "chat:write")]
    ChatWrite,
    #[serde(rename = "channels:read")]
    ChannelsRead,
    #[serde(rename = "reactions:write")]
    ReactionsWrite,
    #[serde(rename = "groups:read")]
    GroupsRead,
    #[serde(rename = "im:history")]
    ImHistory,
}

impl AppScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AppMentionsRead => "app_mentions:read",
            Self::ChatWrite => "chat:write",
            Self::ChannelsRead => "channels:read",
            Self::ReactionsWrite => "reactions:write",
            Self::GroupsRead => "groups:read",
            Self::ImHistory => "im:history",
        }
    }
}

impl AppOptions {
    pub fn scopes(self) -> Vec<AppScope> {
        let mut scopes = vec![
            AppScope::AppMentionsRead,
            AppScope::ChatWrite,
            AppScope::ChannelsRead,
            AppScope::ReactionsWrite,
        ];
        if self.private_channels {
            scopes.push(AppScope::GroupsRead);
        }
        if self.direct_messages {
            scopes.push(AppScope::ImHistory);
        }
        scopes
    }
}

/// URLs are admitted by the owning host, then checked again before provider use.
pub fn manifest(
    identity: &AppIdentity,
    options: AppOptions,
    callback: &str,
    events: Option<&str>,
) -> AppResult<Value> {
    identity.validate()?;
    validate_url(callback, "callback_url")?;
    if let Some(url) = events {
        validate_url(url, "events_url")?;
    }
    let mut display = json!({"name": identity.name, "description": identity.description});
    if let Some(color) = &identity.background_color {
        display["background_color"] = json!(color);
    }
    let mut settings = json!({"org_deploy_enabled": false, "socket_mode_enabled": false, "token_rotation_enabled": false});
    if let Some(url) = events {
        let mut events = vec!["app_mention", "app_uninstalled", "tokens_revoked"];
        if options.direct_messages {
            events.push("message.im");
        }
        settings["event_subscriptions"] = json!({"request_url": url, "bot_events": events});
    }
    Ok(json!({
        "display_information": display,
        "features": {"bot_user": {"display_name": identity.display_name, "always_online": true}},
        "oauth_config": {"redirect_urls": [callback], "scopes": {"bot": options.scopes()}},
        "settings": settings,
    }))
}

pub(super) fn validate_url(value: &str, field: &str) -> AppResult<()> {
    let uri = url::Url::parse(value).map_err(|_| AppError::invalid(field))?;
    if value.len() > 2048
        || uri.scheme() != "https"
        || uri.host_str().is_none()
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.fragment().is_some()
        || value.chars().any(char::is_control)
    {
        return Err(AppError::invalid(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> AppIdentity {
        AppIdentity {
            name: "Employee".into(),
            display_name: "employee".into(),
            description: "Help the team".into(),
            background_color: Some("#123abc".into()),
        }
    }

    #[test]
    fn manifest_scopes_and_events_follow_checked_options() {
        for private_channels in [false, true] {
            for direct_messages in [false, true] {
                let options = AppOptions {
                    private_channels,
                    direct_messages,
                };
                let m = manifest(
                    &identity(),
                    options,
                    "https://core.example/integrations/slack/callback",
                    Some("https://core.example/webhooks/slack/apps/R1"),
                )
                .unwrap();
                let scopes = m["oauth_config"]["scopes"]["bot"].as_array().unwrap();
                assert_eq!(scopes.contains(&json!("groups:read")), private_channels);
                assert_eq!(scopes.contains(&json!("im:history")), direct_messages);
                let events = m["settings"]["event_subscriptions"]["bot_events"]
                    .as_array()
                    .unwrap();
                assert_eq!(events.contains(&json!("message.im")), direct_messages);
                assert!(events.contains(&json!("app_uninstalled")));
                assert!(events.contains(&json!("tokens_revoked")));
                assert_eq!(m["settings"]["token_rotation_enabled"], false);
            }
        }
        assert!(
            manifest(
                &identity(),
                AppOptions::default(),
                "https://core.example/callback",
                None
            )
            .unwrap()["settings"]
                .get("event_subscriptions")
                .is_none()
        );
    }

    #[test]
    fn invalid_identity_and_callback_are_refused_before_provider_use() {
        let mut value = identity();
        value.display_name = "Invalid Name".into();
        assert!(value.validate().is_err());
        for url in [
            "http://core.example/callback",
            "https://user:pass@core.example/callback",
            "https://core.example/callback#fragment",
        ] {
            assert!(manifest(&identity(), AppOptions::default(), url, None).is_err());
        }
    }
}
