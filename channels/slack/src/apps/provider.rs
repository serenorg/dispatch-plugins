use std::{
    io::Read,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    AppError, AppResult, valid_bot_token, valid_client_id, valid_configuration_token, valid_id,
    valid_secret,
};

/// Provider material is serialized only into the owning organization's protected record.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppCredentials {
    pub app_id: String,
    pub client_id: String,
    pub client_secret: String,
    pub signing_secret: String,
    pub bot_token: Option<String>,
    pub team_id: Option<String>,
    pub bot_user_id: Option<String>,
}

impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials").finish_non_exhaustive()
    }
}

impl AppCredentials {
    pub fn validate(&self) -> AppResult<()> {
        if !valid_id(&self.app_id)
            || !valid_client_id(&self.client_id)
            || !valid_secret(&self.client_secret)
            || !valid_secret(&self.signing_secret)
        {
            return Err(AppError::invalid("credentials"));
        }
        match (&self.bot_token, &self.team_id, &self.bot_user_id) {
            (None, None, None) => Ok(()),
            (Some(token), Some(team), Some(bot))
                if valid_bot_token(token) && valid_id(team) && valid_id(bot) =>
            {
                Ok(())
            }
            _ => Err(AppError::invalid("bot_identity")),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationToken {
    pub configuration_token: String,
    pub refresh_token: Option<String>,
}

impl ConfigurationToken {
    pub fn validate(&self) -> AppResult<()> {
        if !valid_configuration_token(&self.configuration_token)
            || self
                .refresh_token
                .as_deref()
                .is_some_and(|t| !valid_configuration_token(t))
        {
            return Err(AppError::invalid("configuration_token"));
        }
        Ok(())
    }
}

impl std::fmt::Debug for ConfigurationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigurationToken").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub is_member: bool,
    #[serde(default)]
    pub is_im: bool,
}

#[derive(Deserialize)]
struct ProviderChannel {
    id: String,
    name: Option<String>,
    #[serde(default)]
    is_private: bool,
    #[serde(default)]
    is_member: bool,
    #[serde(default)]
    is_im: bool,
}

impl TryFrom<ProviderChannel> for Channel {
    type Error = AppError;

    fn try_from(channel: ProviderChannel) -> AppResult<Self> {
        if !valid_id(&channel.id) || channel.name.as_ref().is_some_and(|n| n.len() > 1024) {
            return Err(AppError::Unavailable {});
        }
        Ok(Self {
            id: channel.id,
            name: channel.name,
            is_private: channel.is_private,
            is_member: channel.is_member,
            is_im: channel.is_im,
        })
    }
}

#[derive(Deserialize)]
pub struct BotIdentity {
    pub team_id: String,
    pub user_id: String,
}

#[derive(Deserialize)]
pub struct OAuthGrant {
    pub access_token: Option<String>,
    #[serde(default)]
    pub scope: String,
    pub bot_user_id: Option<String>,
    pub app_id: Option<String>,
    pub team: Option<Workspace>,
    pub enterprise: Option<Workspace>,
    #[serde(default)]
    pub is_enterprise_install: bool,
}

#[derive(Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppIcon {
    pub content_type: String,
    pub data_base64: String,
}

impl AppIcon {
    pub fn decode(&self) -> AppResult<Vec<u8>> {
        const MAX_BYTES: usize = 2 * 1024 * 1024;
        let format = match self.content_type.as_str() {
            "image/png" => image::ImageFormat::Png,
            "image/jpeg" => image::ImageFormat::Jpeg,
            _ => return Err(AppError::invalid("icon")),
        };
        if self.data_base64.len() > MAX_BYTES * 4 / 3 + 4 {
            return Err(AppError::invalid("icon"));
        }
        let bytes = STANDARD
            .decode(self.data_base64.trim())
            .map_err(|_| AppError::invalid("icon"))?;
        if bytes.len() > MAX_BYTES {
            return Err(AppError::invalid("icon"));
        }
        let (width, height) = image::ImageReader::with_format(std::io::Cursor::new(&bytes), format)
            .into_dimensions()
            .map_err(|_| AppError::invalid("icon"))?;
        if !(512..=2000).contains(&width) || !(512..=2000).contains(&height) {
            return Err(AppError::invalid("icon"));
        }
        Ok(bytes)
    }
}

/// One bounded provider attempt. Retry ownership belongs to the durable operation runner.
pub struct Provider {
    agent: ureq::Agent,
    base: String,
    deadline: Instant,
}

impl Default for Provider {
    fn default() -> Self {
        Self::new(&crate::api_base_url())
    }
}

impl Provider {
    #[cfg(test)]
    pub(super) fn for_tests(base: &str, budget: Duration) -> Self {
        let mut provider = Self::new(base);
        provider.deadline = Instant::now() + budget;
        provider.agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(None)
            .build()
            .new_agent();
        provider
    }

    fn new(base: &str) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .max_redirects(0)
                .http_status_as_error(false)
                .build()
                .new_agent(),
            base: base.trim_end_matches('/').into(),
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    fn read(mut response: ureq::http::Response<ureq::Body>) -> AppResult<Value> {
        if response.status().as_u16() == 429 {
            return Err(AppError::Provider {
                code: "ratelimited".into(),
            });
        }
        if !response.status().is_success() {
            return Err(AppError::Unavailable {});
        }
        let mut text = String::new();
        response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .reader()
            .read_to_string(&mut text)
            .map_err(|_| AppError::Unavailable {})?;
        let body: Value = serde_json::from_str(&text).map_err(|_| AppError::Unavailable {})?;
        if body.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(body);
        }
        let code = body
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| valid_code(s))
            .ok_or(AppError::Unavailable {})?;
        if code == "invalid_manifest" {
            let pointer = body
                .get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| e.get("pointer").and_then(Value::as_str))
                .find(|s| valid_pointer(s))
                .map(str::to_owned);
            return Err(AppError::InvalidManifest { pointer });
        }
        Err(AppError::Provider { code: code.into() })
    }

    fn form(&self, method: &str, token: Option<&str>, form: &[(&str, &str)]) -> AppResult<Value> {
        let timeout = self.timeout()?;
        let mut request = self
            .agent
            .post(format!("{}/{method}", self.base))
            .config()
            .timeout_global(Some(timeout))
            .build();
        if let Some(token) = token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        Self::read(
            request
                .send_form(form.iter().copied())
                .map_err(|_| AppError::Unavailable {})?,
        )
    }

    fn timeout(&self) -> AppResult<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AppError::Unavailable {});
        }
        Ok(remaining.min(Duration::from_secs(30)))
    }

    pub fn validate_manifest(&self, token: &str, manifest: &Value) -> AppResult<()> {
        self.form(
            "apps.manifest.validate",
            Some(token),
            &[("manifest", &manifest.to_string())],
        )
        .map(|_| ())
    }

    pub fn create_receipt(&self, token: &str, manifest: &Value) -> AppResult<Value> {
        self.form(
            "apps.manifest.create",
            Some(token),
            &[("manifest", &manifest.to_string())],
        )
    }

    pub fn created_credentials(body: &Value) -> AppResult<AppCredentials> {
        let credentials = AppCredentials {
            app_id: string(body, "app_id")?,
            client_id: string(&body["credentials"], "client_id")?,
            client_secret: string(&body["credentials"], "client_secret")?,
            signing_secret: string(&body["credentials"], "signing_secret")?,
            bot_token: None,
            team_id: None,
            bot_user_id: None,
        };
        credentials
            .validate()
            .map_err(|_| AppError::Unavailable {})?;
        Ok(credentials)
    }

    pub fn update(&self, token: &str, app: &str, manifest: &Value) -> AppResult<()> {
        self.form(
            "apps.manifest.update",
            Some(token),
            &[("app_id", app), ("manifest", &manifest.to_string())],
        )
        .map(|_| ())
    }

    pub fn icon(&self, token: &str, app: &str, icon: &AppIcon) -> AppResult<()> {
        let image = icon.decode()?;
        // Check the content-derived boundary before embedding the binary body.
        let boundary = format!(
            "slack{}",
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&image))
        );
        if image
            .windows(boundary.len())
            .any(|bytes| bytes == boundary.as_bytes())
        {
            return Err(AppError::invalid("icon"));
        }
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"app_id\"\r\n\r\n{app}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"icon\"\r\nContent-Type: {}\r\n\r\n", icon.content_type).into_bytes();
        body.extend_from_slice(&image);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Self::read(
            self.agent
                .post(format!("{}/apps.icon.set", self.base))
                .config()
                .timeout_global(Some(self.timeout()?))
                .build()
                .header("Authorization", &format!("Bearer {token}"))
                .header(
                    "Content-Type",
                    &format!("multipart/form-data; boundary={boundary}"),
                )
                .send(&body)
                .map_err(|_| AppError::Unavailable {})?,
        )
        .map(|_| ())
    }

    pub fn rotate_receipt(&self, refresh: &str) -> AppResult<Value> {
        self.form("tooling.tokens.rotate", None, &[("refresh_token", refresh)])
    }

    pub fn rotated_configuration(body: &Value) -> AppResult<ConfigurationToken> {
        let configuration = ConfigurationToken {
            configuration_token: string(body, "token")?,
            refresh_token: Some(string(body, "refresh_token")?),
        };
        configuration
            .validate()
            .map_err(|_| AppError::Unavailable {})?;
        Ok(configuration)
    }

    pub fn exchange_receipt(
        &self,
        credentials: &AppCredentials,
        code: &str,
        callback: &str,
    ) -> AppResult<Value> {
        let basic = STANDARD.encode(format!(
            "{}:{}",
            credentials.client_id, credentials.client_secret
        ));
        let response = self
            .agent
            .post(format!("{}/oauth.v2.access", self.base))
            .config()
            .timeout_global(Some(self.timeout()?))
            .build()
            .header("Authorization", &format!("Basic {basic}"))
            .send_form([("code", code), ("redirect_uri", callback)])
            .map_err(|_| AppError::Unavailable {})?;
        Self::read(response)
    }

    pub fn verify(&self, token: &str) -> AppResult<BotIdentity> {
        let identity: BotIdentity =
            serde_json::from_value(self.form("auth.test", Some(token), &[])?)
                .map_err(|_| AppError::Unavailable {})?;
        if !valid_id(&identity.team_id) || !valid_id(&identity.user_id) {
            return Err(AppError::Unavailable {});
        }
        Ok(identity)
    }

    pub fn revoke(&self, token: &str) -> AppResult<()> {
        self.form("auth.revoke", Some(token), &[]).map(|_| ())
    }

    pub fn uninstall(&self, credentials: &AppCredentials) -> AppResult<()> {
        let token = credentials
            .bot_token
            .as_deref()
            .ok_or_else(|| AppError::invalid("bot_token"))?;
        self.form(
            "apps.uninstall",
            Some(token),
            &[
                ("client_id", &credentials.client_id),
                ("client_secret", &credentials.client_secret),
            ],
        )
        .map(|_| ())
    }

    pub fn channels(
        &self,
        token: &str,
        private: bool,
        cursor: Option<&str>,
    ) -> AppResult<(Vec<Channel>, Option<String>)> {
        let mut form = vec![
            (
                "types",
                if private {
                    "public_channel,private_channel"
                } else {
                    "public_channel"
                },
            ),
            ("exclude_archived", "true"),
            ("limit", "200"),
        ];
        if let Some(cursor) = cursor {
            form.push(("cursor", cursor));
        }
        let body = self.form("conversations.list", Some(token), &form)?;
        let channels: Vec<ProviderChannel> =
            serde_json::from_value(body.get("channels").cloned().unwrap_or(json!([])))
                .map_err(|_| AppError::Unavailable {})?;
        if channels.len() > 200 {
            return Err(AppError::Unavailable {});
        }
        let channels = channels
            .into_iter()
            .map(Channel::try_from)
            .collect::<AppResult<Vec<_>>>()?;
        let cursor = body["response_metadata"]["next_cursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if cursor.as_ref().is_some_and(|c| c.len() > 2048) {
            return Err(AppError::Unavailable {});
        }
        Ok((channels, cursor))
    }

    pub fn channel(&self, token: &str, id: &str) -> AppResult<Channel> {
        let body = self.form("conversations.info", Some(token), &[("channel", id)])?;
        let channel = Channel::try_from(
            serde_json::from_value::<ProviderChannel>(body["channel"].clone())
                .map_err(|_| AppError::Unavailable {})?,
        )?;
        if channel.id != id || !valid_id(&channel.id) {
            return Err(AppError::BindingMismatch {});
        }
        Ok(channel)
    }
}

fn string(body: &Value, key: &str) -> AppResult<String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 4096)
        .map(str::to_owned)
        .ok_or(AppError::Unavailable {})
}

fn valid_code(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_pointer(s: &str) -> bool {
    s.starts_with('/')
        && s.len() <= 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, sync::mpsc, thread};

    fn server(
        status: &str,
        body: Value,
    ) -> (Provider, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = mpsc::channel();
        let status = status.to_string();
        let handle = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("local fake request missing: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break request.len();
                }
                assert!(request.len() < 16384);
            };
            let headers = String::from_utf8(request.clone()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            request.resize(header_end + length, 0);
            stream.read_exact(&mut request[header_end..]).unwrap();
            let _ = sent.send(String::from_utf8_lossy(&request).into_owned());
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        (
            Provider::for_tests(&format!("http://{address}"), Duration::from_secs(60)),
            received,
            handle,
        )
    }

    #[test]
    fn create_receipt_retains_credentials_before_any_optional_work() {
        let expected = json!({"ok":true,"app_id":"A1","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}});
        let (provider, request, handle) = server("200 OK", expected.clone());
        let receipt = provider
            .create_receipt(
                "xoxe-local-test",
                &json!({"display_information":{"name":"Employee"}}),
            )
            .unwrap();
        assert_eq!(receipt, expected);
        let credentials = Provider::created_credentials(&receipt).unwrap();
        assert_eq!(credentials.app_id, "A1");
        assert!(!format!("{credentials:?}").contains("0123456789abcdef"));
        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(request.starts_with("POST /apps.manifest.create HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer xoxe-local-test")
        );
        assert!(request.contains("manifest="));
        handle.join().unwrap();
    }

    #[test]
    fn untrusted_provider_text_and_headers_do_not_escape_errors() {
        for (status, body, expected) in [
            (
                "200 OK",
                json!({"ok":false,"error":"private token leaked"}),
                AppError::Unavailable {},
            ),
            (
                "200 OK",
                json!({"ok":false,"error":"invalid_manifest","errors":[{"pointer":"/display_information/name"}]}),
                AppError::InvalidManifest {
                    pointer: Some("/display_information/name".into()),
                },
            ),
            (
                "429 Too Many Requests",
                json!({"secret":"private"}),
                AppError::Provider {
                    code: "ratelimited".into(),
                },
            ),
            (
                "503 Service Unavailable",
                json!({"secret":"private"}),
                AppError::Unavailable {},
            ),
        ] {
            let (provider, _, handle) = server(status, body);
            assert_eq!(
                provider.validate_manifest("xoxe-local-test", &json!({})),
                Err(expected)
            );
            handle.join().unwrap();
        }
    }

    #[test]
    fn oauth_uses_basic_auth_and_preserves_the_grant_for_durable_recovery() {
        let (provider, request, handle) = server(
            "200 OK",
            json!({"ok":true,"access_token":"xoxb-private-token","app_id":"A1"}),
        );
        let credentials = AppCredentials {
            app_id: "A1".into(),
            client_id: "1.2".into(),
            client_secret: "0123456789abcdef".into(),
            signing_secret: "abcdef0123456789".into(),
            bot_token: None,
            team_id: None,
            bot_user_id: None,
        };
        let receipt = provider
            .exchange_receipt(&credentials, "once", "https://core.example/callback")
            .unwrap();
        assert_eq!(receipt["access_token"], "xoxb-private-token");
        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(request.starts_with("POST /oauth.v2.access HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: basic ")
        );
        assert!(!request.contains("client_secret="));
        handle.join().unwrap();
    }

    #[test]
    fn icon_format_size_and_dimensions_are_validated_before_upload() {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let icon = AppIcon {
            content_type: "image/png".into(),
            data_base64: STANDARD.encode(bytes.into_inner()),
        };
        assert!(!icon.decode().unwrap().is_empty());
        let (provider, request, handle) = server("200 OK", json!({"ok":true}));
        provider.icon("xoxe-local-test", "A1", &icon).unwrap();
        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(request.starts_with("POST /apps.icon.set HTTP/1.1"));
        assert!(request.contains("multipart/form-data; boundary="));
        let boundary = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-type")
                    .then_some(value.trim())?
                    .strip_prefix("multipart/form-data; boundary=")
            })
            .unwrap();
        assert!((1..=70).contains(&boundary.len()));
        assert!(request.contains(&format!("--{boundary}--\r\n")));
        assert!(request.contains("name=\"app_id\"\r\n\r\nA1"));
        handle.join().unwrap();
        for content_type in ["image/gif", "image/jpeg"] {
            let invalid = AppIcon {
                content_type: content_type.into(),
                data_base64: icon.data_base64.clone(),
            };
            assert_eq!(invalid.decode(), Err(AppError::invalid("icon")));
        }
    }

    #[test]
    fn normal_provider_channel_fields_are_accepted_then_normalized() {
        let channel = json!({"id":"C1","name":"help","is_private":false,"is_member":true,"is_im":false,
            "created":123,"is_archived":false,"topic":{"value":"hello"},"num_members":4});
        let (provider, _, handle) = server(
            "200 OK",
            json!({"ok":true,"channels":[channel],"response_metadata":{"next_cursor":""}}),
        );
        let (channels, cursor) = provider.channels("xoxb-local-test", false, None).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id, "C1");
        assert_eq!(cursor, None);
        assert!(
            serde_json::to_value(&channels[0])
                .unwrap()
                .get("topic")
                .is_none()
        );
        handle.join().unwrap();
        let (provider, _, handle) = server("200 OK", json!({"ok":true,"channel":channel}));
        assert_eq!(
            provider.channel("xoxb-local-test", "C1").unwrap(),
            channels[0]
        );
        handle.join().unwrap();
    }

    #[test]
    fn channel_info_and_list_have_the_same_field_bounds() {
        let channel = json!({"id":"C1","name":"n".repeat(1025),"created":123});
        let (provider, _, handle) = server("200 OK", json!({"ok":true,"channel":channel}));
        assert_eq!(
            provider.channel("xoxb-local-test", "C1"),
            Err(AppError::Unavailable {})
        );
        handle.join().unwrap();
        let (provider, _, handle) = server("200 OK", json!({"ok":true,"channels":[channel]}));
        assert_eq!(
            provider.channels("xoxb-local-test", false, None),
            Err(AppError::Unavailable {})
        );
        handle.join().unwrap();
    }
}
