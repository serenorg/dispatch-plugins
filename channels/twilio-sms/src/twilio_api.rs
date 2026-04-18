use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::io::Read;
use urlencoding::encode;

const DEFAULT_API_BASE: &str = "https://api.twilio.com/2010-04-01";

#[derive(Debug)]
pub struct TwilioClient {
    account_sid: String,
    auth_token: String,
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
    pub fn from_env(account_sid_env: &str, auth_token_env: &str) -> Result<Self> {
        let account_sid = std::env::var(account_sid_env)
            .with_context(|| format!("{account_sid_env} is required for the twilio sms channel"))?;
        let auth_token = std::env::var(auth_token_env)
            .with_context(|| format!("{auth_token_env} is required for the twilio sms channel"))?;
        Ok(Self {
            account_sid,
            auth_token,
            base_url: DEFAULT_API_BASE.to_string(),
        })
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
                &basic_auth_header(&self.account_sid, &self.auth_token),
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
                &basic_auth_header(&self.account_sid, &self.auth_token),
            )
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body)
            .map_err(|error| anyhow!("{context}: {error}"))?;
        read_json_body(&mut response, context)
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
