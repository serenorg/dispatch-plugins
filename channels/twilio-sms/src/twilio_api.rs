use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::io::Read;
use urlencoding::encode;

const DEFAULT_API_BASE: &str = "https://api.twilio.com/2010-04-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwilioAuthMode {
    AccountToken,
    ApiKey,
}

#[derive(Debug)]
pub struct TwilioClient {
    account_sid: String,
    auth_username: String,
    auth_password: String,
    auth_mode: TwilioAuthMode,
    base_url: String,
}

#[derive(Debug, Clone)]
pub struct TwilioIdentity {
    pub sid: String,
    pub friendly_name: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TwilioMessage {
    pub sid: String,
    pub to: String,
    pub from: Option<String>,
    pub status: Option<String>,
}

impl TwilioClient {
    pub fn from_env(
        account_sid_env: &str,
        auth_token_env: &str,
        api_key_sid_env: &str,
        api_key_secret_env: &str,
    ) -> Result<Self> {
        let account_sid = std::env::var(account_sid_env)
            .with_context(|| format!("{account_sid_env} is required for the twilio sms channel"))?;
        let api_key_sid = std::env::var(api_key_sid_env).ok();
        let api_key_secret = std::env::var(api_key_secret_env).ok();

        let (auth_username, auth_password, auth_mode) = auth_credentials_from_parts(
            &account_sid,
            std::env::var(auth_token_env).ok(),
            api_key_sid,
            api_key_secret,
            auth_token_env,
            api_key_sid_env,
            api_key_secret_env,
        )?;

        Ok(Self {
            account_sid,
            auth_username,
            auth_password,
            auth_mode,
            base_url: DEFAULT_API_BASE.to_string(),
        })
    }

    pub fn auth_mode(&self) -> TwilioAuthMode {
        self.auth_mode
    }

    pub fn account_sid(&self) -> &str {
        &self.account_sid
    }

    pub fn identity(&self) -> Result<TwilioIdentity> {
        let url = format!("{}/Accounts/{}.json", self.base_url, self.account_sid);
        let body = self.get_json(&url, "failed to query Twilio account identity")?;
        Ok(TwilioIdentity {
            sid: body
                .get("sid")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("twilio account response missing sid"))?
                .to_string(),
            friendly_name: body
                .get("friendly_name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            status: body
                .get("status")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    pub fn validate_access(&self) -> Result<()> {
        let url = format!(
            "{}/Accounts/{}/Messages.json?PageSize=1",
            self.base_url, self.account_sid
        );
        self.get_json(&url, "failed to validate Twilio API access")
            .map(|_| ())
    }

    pub fn send_message(
        &self,
        to_number: &str,
        content: &str,
        from_number: Option<&str>,
        messaging_service_sid: Option<&str>,
        media_urls: &[String],
    ) -> Result<TwilioMessage> {
        if from_number.is_none() && messaging_service_sid.is_none() {
            bail!("twilio outbound sms requires either From or MessagingServiceSid");
        }

        let url = format!(
            "{}/Accounts/{}/Messages.json",
            self.base_url, self.account_sid
        );
        let mut fields = vec![("To", to_number.to_string()), ("Body", content.to_string())];
        if let Some(from_number) = from_number {
            fields.push(("From", from_number.to_string()));
        }
        if let Some(messaging_service_sid) = messaging_service_sid {
            fields.push(("MessagingServiceSid", messaging_service_sid.to_string()));
        }
        for media_url in media_urls {
            fields.push(("MediaUrl", media_url.clone()));
        }

        let body = self.post_form(&url, &fields, "failed to send Twilio SMS")?;
        Ok(TwilioMessage {
            sid: body
                .get("sid")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("twilio message response missing sid"))?
                .to_string(),
            to: body
                .get("to")
                .and_then(Value::as_str)
                .unwrap_or(to_number)
                .to_string(),
            from: body
                .get("from")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            status: body
                .get("status")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    fn get_json(&self, url: &str, context: &str) -> Result<Value> {
        let mut response = ureq::get(url)
            .header(
                "Authorization",
                &basic_auth_header(&self.auth_username, &self.auth_password),
            )
            .call()
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }

    fn post_form(&self, url: &str, fields: &[(&str, String)], context: &str) -> Result<Value> {
        let body = fields
            .iter()
            .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut response = ureq::post(url)
            .header(
                "Authorization",
                &basic_auth_header(&self.auth_username, &self.auth_password),
            )
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body)
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
    }
}

fn auth_credentials_from_parts(
    account_sid: &str,
    auth_token: Option<String>,
    api_key_sid: Option<String>,
    api_key_secret: Option<String>,
    auth_token_env: &str,
    api_key_sid_env: &str,
    api_key_secret_env: &str,
) -> Result<(String, String, TwilioAuthMode)> {
    match (api_key_sid, api_key_secret) {
        (Some(api_key_sid), Some(api_key_secret)) => {
            Ok((api_key_sid, api_key_secret, TwilioAuthMode::ApiKey))
        }
        (Some(_), None) => bail!(
            "{api_key_secret_env} is required when {api_key_sid_env} is set for the twilio sms channel"
        ),
        (None, Some(_)) => bail!(
            "{api_key_sid_env} is required when {api_key_secret_env} is set for the twilio sms channel"
        ),
        (None, None) => {
            let auth_token = auth_token.with_context(|| {
                format!(
                    "{auth_token_env} is required for the twilio sms channel when API key credentials are not set"
                )
            })?;
            Ok((
                account_sid.to_string(),
                auth_token,
                TwilioAuthMode::AccountToken,
            ))
        }
    }
}

fn basic_auth_header(username: &str, password: &str) -> String {
    let token = format!("{username}:{password}");
    format!("Basic {}", STANDARD.encode(token))
}

fn read_json_body(response: &mut ureq::http::Response<ureq::Body>, context: &str) -> Result<Value> {
    let status = response.status();
    let mut body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .reader();
    let mut text = String::new();
    body.read_to_string(&mut text)
        .with_context(|| format!("{context}: failed to read response body"))?;
    if !status.is_success() {
        bail!("{context}: HTTP {}: {}", status.as_u16(), text);
    }
    serde_json::from_str(&text)
        .with_context(|| format!("{context}: failed to parse response body as JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_auth_is_used_when_api_key_is_absent() {
        let (username, password, mode) = auth_credentials_from_parts(
            "AC123",
            Some("auth-token".to_string()),
            None,
            None,
            "TWILIO_AUTH_TOKEN",
            "TWILIO_API_KEY_SID",
            "TWILIO_API_KEY_SECRET",
        )
        .expect("account credentials");

        assert_eq!(username, "AC123");
        assert_eq!(password, "auth-token");
        assert_eq!(mode, TwilioAuthMode::AccountToken);
    }

    #[test]
    fn api_key_auth_is_preferred_when_full_key_pair_is_present() {
        let (username, password, mode) = auth_credentials_from_parts(
            "AC123",
            Some("auth-token".to_string()),
            Some("SK123".to_string()),
            Some("secret".to_string()),
            "TWILIO_AUTH_TOKEN",
            "TWILIO_API_KEY_SID",
            "TWILIO_API_KEY_SECRET",
        )
        .expect("api key credentials");

        assert_eq!(username, "SK123");
        assert_eq!(password, "secret");
        assert_eq!(mode, TwilioAuthMode::ApiKey);
    }

    #[test]
    fn partial_api_key_pair_is_rejected() {
        let error = auth_credentials_from_parts(
            "AC123",
            None,
            Some("SK123".to_string()),
            None,
            "TWILIO_AUTH_TOKEN",
            "TWILIO_API_KEY_SID",
            "TWILIO_API_KEY_SECRET",
        )
        .expect_err("partial api key pair should fail");

        assert!(
            error
                .to_string()
                .contains("TWILIO_API_KEY_SECRET is required"),
            "unexpected error: {error}"
        );
    }
}
