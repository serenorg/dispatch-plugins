use crate::protocol::{ChannelConfig, OutboundMessage};
use serde_json::{Value, json};
use std::time::Duration;
use ureq::http::Request;
use url::Url;
use uuid::Uuid;

// Only static errors cross the plugin boundary: HTTP errors can contain the
// password-bearing URL, and provider error bodies can echo message content.
pub type Result<T> = std::result::Result<T, &'static str>;

pub struct BlueBubblesClient {
    base_url: Url,
    password: String,
    agent: ureq::Agent,
}

impl BlueBubblesClient {
    pub fn from_env(config: &ChannelConfig) -> Result<Self> {
        let base_url = std::env::var(config.url_env()).map_err(|_| "bluebubbles_url_missing")?;
        Ok(Self {
            base_url: validate_base_url(&base_url)?,
            password: password_from_env(config)?,
            agent: ureq::Agent::config_builder()
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(20)))
                .build()
                .into(),
        })
    }

    pub fn ping(&self) -> Result<()> {
        let url = api_url(&self.base_url, "/api/v1/ping", &self.password);
        let response = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|_| "bluebubbles_ping_failed")?;
        if !response.status().is_success() {
            return Err("bluebubbles_ping_failed");
        }
        Ok(())
    }

    pub fn send_message(&self, message: &OutboundMessage) -> Result<String> {
        let request = build_send_request(&self.base_url, &self.password, message, Uuid::new_v4())?;
        let mut response = self
            .agent
            .run(request)
            .map_err(|_| "bluebubbles_send_failed")?;
        if !response.status().is_success() {
            return Err("bluebubbles_send_failed");
        }
        let body: Value = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_json()
            .map_err(|_| "bluebubbles_invalid_response")?;
        message_id(&body).map(ToOwned::to_owned)
    }
}

pub fn password_from_env(config: &ChannelConfig) -> Result<String> {
    std::env::var(config.password_env())
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or("bluebubbles_password_missing")
}

pub fn validate_base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| "bluebubbles_invalid_url")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("bluebubbles_invalid_url");
    }
    Ok(url)
}

fn api_url(base_url: &Url, path: &str, password: &str) -> Url {
    let mut url = base_url.clone();
    url.set_path(&format!("{}{path}", base_url.path().trim_end_matches('/')));
    url.query_pairs_mut().append_pair("password", password);
    url
}

pub fn is_dm_chat_guid(guid: &str) -> bool {
    guid.split_once(";-;").is_some_and(|(service, address)| {
        !service.is_empty()
            && !address.is_empty()
            && !service.contains(';')
            && !address.contains(';')
    })
}

pub fn destination(message: &OutboundMessage) -> Result<&str> {
    message
        .metadata
        .get("conversation_id")
        .map(String::as_str)
        .filter(|guid| is_dm_chat_guid(guid))
        .ok_or("imessage_dm_required")
}

fn build_send_request(
    base_url: &Url,
    password: &str,
    message: &OutboundMessage,
    temp_guid: Uuid,
) -> Result<Request<String>> {
    let chat_guid = destination(message)?;
    if !message.attachments.is_empty() {
        return Err("imessage_outbound_attachments_unsupported");
    }
    let body = json!({
        "chatGuid": chat_guid,
        "tempGuid": temp_guid.to_string(),
        "message": message.content,
    });
    Request::post(api_url(base_url, "/api/v1/message/text", password).as_str())
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .map_err(|_| "bluebubbles_invalid_request")
}

fn message_id(body: &Value) -> Result<&str> {
    body.get("data")
        .and_then(|data| data.get("guid"))
        .and_then(Value::as_str)
        .filter(|guid| !guid.trim().is_empty())
        .ok_or("bluebubbles_missing_message_id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn send_request_matches_bluebubbles_plain_text_contract() {
        let message = OutboundMessage {
            content: "Hello world!".to_string(),
            attachments: vec![],
            metadata: BTreeMap::from([(
                "conversation_id".to_string(),
                "iMessage;-;+15551234567".to_string(),
            )]),
        };
        let temp_guid = Uuid::new_v4();
        let base_url = validate_base_url("https://bluebubbles.example/").unwrap();
        let request = build_send_request(&base_url, "p+&= word", &message, temp_guid).unwrap();
        assert_eq!(request.method(), "POST");
        let url = Url::parse(&request.uri().to_string()).unwrap();
        assert_eq!(url.path(), "/api/v1/message/text");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("password".into(), "p+&= word".into())]
        );
        let body: Value = serde_json::from_str(request.body()).unwrap();
        assert_eq!(
            body,
            json!({
                "chatGuid": "iMessage;-;+15551234567", "tempGuid": temp_guid.to_string(), "message": "Hello world!",
            })
        );
        assert_eq!(
            Uuid::parse_str(body["tempGuid"].as_str().unwrap())
                .unwrap()
                .get_version_num(),
            4
        );
        assert_eq!(request.headers().len(), 1);
        assert_eq!(request.headers()["Content-Type"], "application/json");
        assert!(!request.body().contains("p+&= word"));
        assert_eq!(
            message_id(&json!({"data": {"guid": "sent-1"}})),
            Ok("sent-1")
        );
        assert_eq!(
            message_id(&json!({"guid": "wrong-level"})),
            Err("bluebubbles_missing_message_id")
        );
    }

    #[test]
    fn send_refuses_non_dm_destinations_and_outbound_attachments() {
        let base_url = validate_base_url("https://bluebubbles.example").unwrap();
        for guid in [
            None,
            Some("+15551234567"),
            Some("iMessage;+;group-1"),
            Some("iMessage;-;"),
            Some("iMessage;+;group;-;handle"),
        ] {
            let mut message = OutboundMessage {
                content: "private text".to_string(),
                ..Default::default()
            };
            if let Some(guid) = guid {
                message
                    .metadata
                    .insert("conversation_id".to_string(), guid.to_string());
            }
            assert_eq!(
                build_send_request(&base_url, "secret", &message, Uuid::new_v4()).unwrap_err(),
                "imessage_dm_required"
            );
        }
        let message: OutboundMessage = serde_json::from_value(json!({
            "content": "hello", "metadata": {"conversation_id": "iMessage;-;customer@example.com"},
            "attachments": [{"name": "photo.png", "mime_type": "image/png", "url": "https://example.com/photo.png"}],
        }))
        .unwrap();
        assert_eq!(
            build_send_request(&base_url, "secret", &message, Uuid::new_v4()).unwrap_err(),
            "imessage_outbound_attachments_unsupported"
        );
        for url in [
            "invalid secret",
            "https://host/?password=secret",
            "https://user:secret@host",
            "file:///secret",
        ] {
            assert_eq!(
                validate_base_url(url).unwrap_err(),
                "bluebubbles_invalid_url"
            );
        }
    }
}
