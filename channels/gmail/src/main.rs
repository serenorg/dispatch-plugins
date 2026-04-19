use anyhow::Result;

mod protocol;

fn main() -> Result<()> {
    Ok(channel_email_core::run::<protocol::Preset>()?)
}

#[cfg(test)]
mod tests {
    use super::protocol::Preset;
    use channel_email_core::ChannelConfig;

    #[test]
    fn configure_minimal_preset_uses_gmail_defaults() {
        // SAFETY: set_var mutates shared process state; these tests do not rely on env vars
        // concurrently.
        unsafe {
            std::env::set_var("GMAIL_APP_PASSWORD", "unit-test");
        }
        let config = ChannelConfig {
            imap_username: Some("you@gmail.com".to_string()),
            smtp_from_email: Some("you@gmail.com".to_string()),
            ..ChannelConfig::default()
        };

        let configured =
            channel_email_core::configure::<Preset>(&config).expect("minimal gmail config");

        assert_eq!(
            configured.metadata.get("imap_host").map(String::as_str),
            Some("imap.gmail.com")
        );
        assert_eq!(
            configured.metadata.get("imap_port").map(String::as_str),
            Some("993")
        );
        assert_eq!(
            configured.metadata.get("smtp_host").map(String::as_str),
            Some("smtp.gmail.com")
        );
        assert_eq!(
            configured.metadata.get("smtp_port").map(String::as_str),
            Some("465")
        );
        assert_eq!(
            configured
                .metadata
                .get("imap_password_env")
                .map(String::as_str),
            Some("GMAIL_APP_PASSWORD")
        );
        assert_eq!(
            configured
                .metadata
                .get("smtp_password_env")
                .map(String::as_str),
            Some("GMAIL_APP_PASSWORD")
        );
    }
}
