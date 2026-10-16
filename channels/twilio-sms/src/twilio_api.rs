use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::{fmt, io::Read, num::NonZeroU32};
use urlencoding::encode;

const DEFAULT_API_BASE: &str = "https://api.twilio.com/2010-04-01";

#[derive(Debug)]
pub struct TwilioRefusal {
    pub code: NonZeroU32,
}

impl fmt::Display for TwilioRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Twilio rejected the request")
    }
}

impl std::error::Error for TwilioRefusal {}

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
            .config()
            .http_status_as_error(false)
            .build()
            .header(
                "Authorization",
                &basic_auth_header(&self.auth_username, &self.auth_password),
            )
            .call()
            .map_err(|_| anyhow!("{context}: transport failure"))?;
        read_json_body(&mut response, context)
    }

    fn post_form(&self, url: &str, fields: &[(&str, String)], context: &str) -> Result<Value> {
        let body = fields
            .iter()
            .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut response = ureq::post(url)
            .config()
            .http_status_as_error(false)
            .build()
            .header(
                "Authorization",
                &basic_auth_header(&self.auth_username, &self.auth_password),
            )
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body)
            .map_err(|_| anyhow!("{context}: transport failure"))?;
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
        .map_err(|_| anyhow!("{context}: failed to read response body"))?;
    if !status.is_success() {
        // Only a client refusal carries a provider code; outages stay retryable.
        if status.is_client_error()
            && let Some(code) = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|body| body.get("code").and_then(Value::as_u64))
                .and_then(|code| u32::try_from(code).ok())
                .and_then(NonZeroU32::new)
        {
            return Err(TwilioRefusal { code }.into());
        }
        bail!("{context}: HTTP {}", status.as_u16());
    }
    serde_json::from_str(&text)
        .map_err(|_| anyhow!("{context}: failed to parse response body as JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch_channel_protocol::{PluginRequestId, parse_jsonrpc_response, response_to_jsonrpc};
    use std::{io::Write, net::TcpListener, time::Duration};

    fn mocked_send(status: Option<u16>, response_body: &str) -> Result<TwilioMessage> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let response_body = response_body.to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "request ended before its body");
                request.extend_from_slice(&buffer[..count]);
                if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                    let length: usize = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .unwrap()
                        .1
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            if let Some(status) = status {
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len(),
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let client = TwilioClient {
            account_sid: "ACprivate".to_string(),
            auth_username: "ACprivate".to_string(),
            auth_password: "private-token".to_string(),
            auth_mode: TwilioAuthMode::AccountToken,
            base_url,
        };
        let result = client.send_message(
            "+15550001111",
            "private message body",
            Some("+15550002222"),
            None,
            &[],
        );
        server.join().unwrap();
        result
    }

    fn assert_wire_error(error: &anyhow::Error, code: &str) {
        let response = crate::request_error_response(error);
        let crate::protocol::PluginResponse::Error { error: payload } = &response else {
            panic!("expected typed plugin error");
        };
        assert_eq!(payload.code, code);
        assert_eq!(payload.message, error.to_string());
        let id = PluginRequestId::integer(17);
        let wire = response_to_jsonrpc(&id, &response).unwrap();
        let json: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(json["error"]["data"]["dispatch_error"]["code"], code);
        assert_eq!(
            json["error"]["data"]["dispatch_error"]["message"],
            payload.message
        );
        let (decoded_id, decoded) = parse_jsonrpc_response(&wire).unwrap();
        assert_eq!(decoded_id, id);
        assert_eq!(decoded, response);
        for private in [
            "private message body",
            "+15550001111",
            "ACprivate",
            "SMprivate",
            "private-token",
            "http://",
            "https://",
        ] {
            assert!(!wire.contains(private), "private data reached the wire");
            assert!(!format!("{error:#}").contains(private));
        }
    }

    #[test]
    fn client_refusals_preserve_numeric_codes_in_typed_and_jsonrpc_errors() {
        for status in [400, 422] {
            for code in [21610, 21617, 21211, 21614, 99999, u32::MAX] {
                let body = serde_json::json!({
                    "code": code,
                    "message": "private message body +15550001111 ACprivate SMprivate",
                    "more_info": "https://private.example.test",
                });
                let error = mocked_send(Some(status), &body.to_string()).unwrap_err();
                assert_eq!(
                    error.downcast_ref::<TwilioRefusal>().unwrap().code.get(),
                    code
                );
                assert_eq!(error.to_string(), "Twilio rejected the request");
                assert_wire_error(&error, &format!("twilio_{code}"));
            }
        }
    }

    #[test]
    fn malformed_or_nonpositive_refusal_codes_remain_generic_errors() {
        for code in [
            Value::Null,
            serde_json::json!("21610"),
            serde_json::json!(0),
            serde_json::json!(-21610),
            serde_json::json!(21610.5),
            serde_json::json!(u64::MAX),
        ] {
            let body = serde_json::json!({"code": code, "message": "private message body"});
            let error = mocked_send(Some(400), &body.to_string()).unwrap_err();
            assert!(error.downcast_ref::<TwilioRefusal>().is_none());
            assert_wire_error(&error, "internal_error");
        }
        let error = mocked_send(Some(400), "private message body").unwrap_err();
        assert_wire_error(&error, "internal_error");
    }

    #[test]
    fn outages_and_transport_failures_never_carry_a_twilio_refusal_code() {
        for (status, body) in [
            (
                Some(500),
                r#"{"code":21610,"message":"private message body"}"#,
            ),
            (None, ""),
        ] {
            let error = mocked_send(status, body).unwrap_err();
            assert!(error.downcast_ref::<TwilioRefusal>().is_none());
            assert_wire_error(&error, "internal_error");
        }
    }

    #[test]
    fn successful_sends_still_return_the_provider_receipt() {
        let sent = mocked_send(
            Some(201),
            r#"{"sid":"SMsent","to":"+15550001111","from":"+15550002222","status":"queued"}"#,
        )
        .unwrap();
        assert_eq!(sent.sid, "SMsent");
        assert_eq!(sent.to, "+15550001111");
        assert_eq!(sent.from.as_deref(), Some("+15550002222"));
        assert_eq!(sent.status.as_deref(), Some("queued"));
    }

    #[test]
    fn invalid_json_in_a_success_response_does_not_echo_its_body() {
        let error = mocked_send(Some(201), "private message body").unwrap_err();
        assert_wire_error(&error, "internal_error");
    }

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
