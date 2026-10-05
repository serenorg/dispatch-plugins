use crate::protocol::{ChannelConfig, OutboundMessage};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use ureq::http::Request;
use url::{Host, Url};
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const RESPONSE_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImessageError {
    BlueBubblesUrlMissing,
    BlueBubblesPasswordMissing,
    BlueBubblesInvalidUrl,
    BlueBubblesPingFailed,
    BlueBubblesChatVerificationFailed,
    BlueBubblesSendFailed,
    BlueBubblesInvalidResponse,
    BlueBubblesMissingMessageId,
    BlueBubblesInvalidRequest,
    DmRequired,
    OutboundAttachmentsUnsupported,
    InvalidWebhookUrl,
    UnsupportedProtocolVersion,
    PollingNotSupported,
    UnsupportedRequest,
}

impl ImessageError {
    // Provider errors can include credential-bearing URLs or message content.
    pub fn code(self) -> &'static str {
        match self {
            Self::BlueBubblesUrlMissing => "bluebubbles_url_missing",
            Self::BlueBubblesPasswordMissing => "bluebubbles_password_missing",
            Self::BlueBubblesInvalidUrl => "bluebubbles_invalid_url",
            Self::BlueBubblesPingFailed => "bluebubbles_ping_failed",
            Self::BlueBubblesChatVerificationFailed => "bluebubbles_chat_verification_failed",
            Self::BlueBubblesSendFailed => "bluebubbles_send_failed",
            Self::BlueBubblesInvalidResponse => "bluebubbles_invalid_response",
            Self::BlueBubblesMissingMessageId => "bluebubbles_missing_message_id",
            Self::BlueBubblesInvalidRequest => "bluebubbles_invalid_request",
            Self::DmRequired => "imessage_dm_required",
            Self::OutboundAttachmentsUnsupported => "imessage_outbound_attachments_unsupported",
            Self::InvalidWebhookUrl => "imessage_invalid_webhook_url",
            Self::UnsupportedProtocolVersion => "unsupported_protocol_version",
            Self::PollingNotSupported => "polling_not_supported",
            Self::UnsupportedRequest => "unsupported_request",
        }
    }
}

pub type Result<T> = std::result::Result<T, ImessageError>;

pub struct BlueBubblesClient {
    base_url: Url,
    password: String,
    agent: ureq::Agent,
}

impl BlueBubblesClient {
    pub fn from_env(config: &ChannelConfig) -> Result<Self> {
        let base_url =
            std::env::var(config.url_env()).map_err(|_| ImessageError::BlueBubblesUrlMissing)?;
        Self::new(validate_base_url(&base_url)?, password_from_env(config)?)
    }

    fn new(base_url: Url, password: String) -> Result<Self> {
        Self::with_proxy(base_url, password, ureq::Proxy::try_from_env())
    }

    fn with_proxy(base_url: Url, password: String, proxy: Option<ureq::Proxy>) -> Result<Self> {
        let proxy = match TransportSecurity::for_url(&base_url)? {
            TransportSecurity::Https => proxy,
            // Loopback HTTP must stay on this host, including with an env proxy.
            TransportSecurity::LoopbackHttp => None,
        };
        Ok(Self {
            base_url,
            password,
            agent: ureq::Agent::config_builder()
                .proxy(proxy)
                .max_redirects(0)
                .timeout_global(Some(REQUEST_TIMEOUT))
                .build()
                .into(),
        })
    }

    pub fn ping(&self) -> Result<()> {
        let url = api_url(&self.base_url, "/api/v1/ping", &self.password);
        let mut response = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|_| ImessageError::BlueBubblesPingFailed)?;
        if response.status().as_u16() != 200 {
            return Err(ImessageError::BlueBubblesPingFailed);
        }
        let body: Value = response
            .body_mut()
            .with_config()
            .limit(RESPONSE_LIMIT)
            .read_json()
            .map_err(|_| ImessageError::BlueBubblesPingFailed)?;
        if !successful_response(&body) || body.get("data").and_then(Value::as_str) != Some("pong") {
            return Err(ImessageError::BlueBubblesPingFailed);
        }
        Ok(())
    }

    pub fn verify_dm_chat(&self, chat: ImessageDmChat<'_>) -> Result<()> {
        self.verify_dm_chat_before(chat, Instant::now() + REQUEST_TIMEOUT)
    }

    fn verify_dm_chat_before(&self, chat: ImessageDmChat<'_>, deadline: Instant) -> Result<()> {
        if !chat.requires_provider_binding() {
            return Ok(());
        }
        // The provider applies these predicates to the exact joined chat row.
        let body = json!({
            "chatGuid": chat.guid(),
            "with": ["chat"],
            "limit": 1,
            "offset": 0,
            "where": [
                {"statement": "chat.service_name = :service", "args": {"service": "iMessage"}},
                {"statement": "chat.style = :style", "args": {"style": 45}},
            ],
        });
        let request = Request::post(
            api_url(&self.base_url, "/api/v1/message/query", &self.password).as_str(),
        )
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .map_err(|_| ImessageError::BlueBubblesInvalidRequest)?;
        let body = self.request_json(
            request,
            deadline,
            ImessageError::BlueBubblesChatVerificationFailed,
        )?;
        verified_chat_response(&body, chat)
    }

    pub fn send_message(&self, message: &OutboundMessage) -> Result<String> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        self.verify_dm_chat_before(validate_outbound_message(message)?, deadline)?;
        let request = build_send_request(&self.base_url, &self.password, message, Uuid::new_v4())?;
        let body = self.request_json(request, deadline, ImessageError::BlueBubblesSendFailed)?;
        message_id(&body).map(ToOwned::to_owned)
    }

    fn request_json(
        &self,
        request: Request<String>,
        deadline: Instant,
        failure: ImessageError,
    ) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(failure);
        }
        let request = self
            .agent
            .configure_request(request)
            .timeout_global(Some(remaining))
            .build();
        let mut response = self.agent.run(request).map_err(|_| failure)?;
        if response.status().as_u16() != 200 {
            return Err(failure);
        }
        let body: Value = response
            .body_mut()
            .with_config()
            .limit(RESPONSE_LIMIT)
            .read_json()
            .map_err(|_| ImessageError::BlueBubblesInvalidResponse)?;
        if !successful_response(&body) {
            return Err(ImessageError::BlueBubblesInvalidResponse);
        }
        Ok(body)
    }
}

pub fn password_from_env(config: &ChannelConfig) -> Result<String> {
    std::env::var(config.password_env())
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(ImessageError::BlueBubblesPasswordMissing)
}

pub fn validate_base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| ImessageError::BlueBubblesInvalidUrl)?;
    TransportSecurity::for_url(&url)?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ImessageError::BlueBubblesInvalidUrl);
    }
    Ok(url)
}

enum TransportSecurity {
    Https,
    LoopbackHttp,
}

impl TransportSecurity {
    fn for_url(url: &Url) -> Result<Self> {
        match url.scheme() {
            "https" => Ok(Self::Https),
            "http" => match url.host() {
                Some(Host::Ipv4(address)) if address.is_loopback() => Ok(Self::LoopbackHttp),
                Some(Host::Ipv6(address)) if address.is_loopback() => Ok(Self::LoopbackHttp),
                _ => Err(ImessageError::BlueBubblesInvalidUrl),
            },
            _ => Err(ImessageError::BlueBubblesInvalidUrl),
        }
    }
}

fn api_url(base_url: &Url, path: &str, password: &str) -> Url {
    let mut url = base_url.clone();
    url.set_path(&format!("{}{path}", base_url.path().trim_end_matches('/')));
    url.query_pairs_mut().append_pair("password", password);
    url
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatService {
    Imessage,
    Unspecified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImessageDmChat<'a> {
    guid: &'a str,
    handle: &'a str,
    service: ChatService,
}

impl<'a> ImessageDmChat<'a> {
    pub fn parse(guid: &'a str) -> Option<Self> {
        let (service, handle) = if let Some(handle) = guid.strip_prefix("iMessage;-;") {
            (ChatService::Imessage, handle)
        } else {
            (ChatService::Unspecified, guid.strip_prefix("any;-;")?)
        };
        if handle.is_empty()
            || handle
                .chars()
                .any(|ch| ch == ';' || ch.is_whitespace() || ch.is_control())
        {
            return None;
        }
        Some(Self {
            guid,
            handle,
            service,
        })
    }

    pub fn guid(&self) -> &'a str {
        self.guid
    }

    pub fn requires_provider_binding(&self) -> bool {
        match self.service {
            ChatService::Imessage => false,
            ChatService::Unspecified => true,
        }
    }

    pub fn matches_message_service(&self, service: Option<&Value>) -> bool {
        match service {
            Some(value) => value.as_str() == Some("iMessage"),
            None => true,
        }
    }

    pub fn matches_sender(&self, sender: &str) -> bool {
        if self.handle.contains('@') && sender.contains('@') {
            self.handle.eq_ignore_ascii_case(sender)
        } else {
            self.handle == sender
        }
    }
}

fn successful_response(body: &Value) -> bool {
    body.get("status").and_then(Value::as_u64) == Some(200) && body.get("error").is_none()
}

fn verified_chat_response(body: &Value, chat: ImessageDmChat<'_>) -> Result<()> {
    let messages = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or(ImessageError::BlueBubblesInvalidResponse)?;
    if messages.is_empty() {
        return Err(ImessageError::DmRequired);
    }
    if messages.len() != 1 {
        return Err(ImessageError::BlueBubblesInvalidResponse);
    }
    let chats = messages[0]
        .get("chats")
        .and_then(Value::as_array)
        .ok_or(ImessageError::BlueBubblesInvalidResponse)?;
    if chats.len() != 1
        || chats[0].get("guid").and_then(Value::as_str) != Some(chat.guid())
        || chats[0].get("style").and_then(Value::as_u64) != Some(45)
    {
        return Err(ImessageError::BlueBubblesInvalidResponse);
    }
    Ok(())
}

pub fn validate_outbound_message(message: &OutboundMessage) -> Result<ImessageDmChat<'_>> {
    let chat = message
        .metadata
        .get("conversation_id")
        .and_then(|guid| ImessageDmChat::parse(guid))
        .ok_or(ImessageError::DmRequired)?;
    if !message.attachments.is_empty() {
        return Err(ImessageError::OutboundAttachmentsUnsupported);
    }
    Ok(chat)
}

fn build_send_request(
    base_url: &Url,
    password: &str,
    message: &OutboundMessage,
    temp_guid: Uuid,
) -> Result<Request<String>> {
    let chat = validate_outbound_message(message)?;
    let body = json!({
        "chatGuid": chat.guid(),
        "tempGuid": temp_guid.to_string(),
        "message": message.content,
        "method": "apple-script",
    });
    Request::post(api_url(base_url, "/api/v1/message/text", password).as_str())
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .map_err(|_| ImessageError::BlueBubblesInvalidRequest)
}

fn message_id(body: &Value) -> Result<&str> {
    body.get("data")
        .and_then(|data| data.get("guid"))
        .and_then(Value::as_str)
        .filter(|guid| !guid.trim().is_empty())
        .ok_or(ImessageError::BlueBubblesMissingMessageId)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
    };

    #[derive(Debug)]
    pub struct HttpRequest {
        pub method: String,
        pub target: String,
        pub body: String,
    }

    pub struct HttpFixture {
        base_url: Url,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        requests: mpsc::Receiver<HttpRequest>,
    }

    impl HttpFixture {
        pub fn new(responses: Vec<(u16, String)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base_url =
                validate_base_url(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = Arc::clone(&stop);
            let (sender, requests) = mpsc::channel();
            let worker = thread::spawn(move || {
                let mut responses = VecDeque::from(responses);
                while !stopped.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(stream) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    };
                    // Accepted sockets can inherit the listener's nonblocking mode.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let mut request_line = line.split_whitespace();
                    let method = request_line.next().unwrap().to_string();
                    let target = request_line.next().unwrap().to_string();
                    let mut content_length = 0;
                    loop {
                        line.clear();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        let (name, value) = line.split_once(':').unwrap();
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut body = vec![0; content_length];
                    reader.read_exact(&mut body).unwrap();
                    sender
                        .send(HttpRequest {
                            method,
                            target,
                            body: String::from_utf8(body).unwrap(),
                        })
                        .unwrap();
                    drop(reader);
                    let (status, body) = responses
                        .pop_front()
                        .unwrap_or((500, "unexpected request".to_string()));
                    let response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).is_ok() {
                        let _ = stream.flush();
                    }
                }
            });
            Self {
                base_url,
                stop,
                worker: Some(worker),
                requests,
            }
        }

        pub fn client(&self) -> BlueBubblesClient {
            BlueBubblesClient::new(self.base_url.clone(), "fixture-secret".to_string()).unwrap()
        }

        pub fn client_with_proxy(&self, proxy: ureq::Proxy) -> BlueBubblesClient {
            BlueBubblesClient::with_proxy(
                self.base_url.clone(),
                "fixture-secret".to_string(),
                Some(proxy),
            )
            .unwrap()
        }

        pub fn proxy(&self) -> ureq::Proxy {
            ureq::Proxy::new(self.base_url.as_str()).unwrap()
        }

        pub fn recorded_requests(mut self) -> Vec<HttpRequest> {
            self.stop.store(true, Ordering::SeqCst);
            self.worker.take().unwrap().join().unwrap();
            self.requests.try_iter().collect()
        }
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    pub fn chat_response(guid: &str, style: u16) -> Value {
        json!({"status": 200, "data": [{"guid": "fixture-message", "chats": [{"guid": guid, "style": style}]}]})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use test_support::{HttpFixture, chat_response};

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
                "chatGuid": "iMessage;-;+15551234567", "tempGuid": temp_guid.to_string(),
                "message": "Hello world!", "method": "apple-script",
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
            Err(ImessageError::BlueBubblesMissingMessageId)
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
            Some("iMessage;-;   "),
            Some("iMessage;-;\t"),
            Some("SMS;-;+15551234567"),
            Some("other;-;+15551234567"),
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
                ImessageError::DmRequired
            );
        }
        let message: OutboundMessage = serde_json::from_value(json!({
            "content": "hello", "metadata": {"conversation_id": "iMessage;-;customer@example.com"},
            "attachments": [{"name": "photo.png", "mime_type": "image/png", "url": "https://example.com/photo.png"}],
        }))
        .unwrap();
        assert_eq!(
            build_send_request(&base_url, "secret", &message, Uuid::new_v4()).unwrap_err(),
            ImessageError::OutboundAttachmentsUnsupported
        );
        for url in [
            "invalid secret",
            "https://host/?password=secret",
            "https://user:secret@host",
            "file:///secret",
        ] {
            assert_eq!(
                validate_base_url(url).unwrap_err(),
                ImessageError::BlueBubblesInvalidUrl
            );
        }
    }

    #[test]
    fn urls_require_https_outside_literal_loopback_addresses() {
        for url in [
            "https://bluebubbles.example",
            "https://192.168.1.10",
            "http://127.0.0.1:1234",
            "http://127.0.0.2:1234",
            "http://[::1]:1234",
        ] {
            assert!(validate_base_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://bluebubbles.example",
            "http://192.168.1.10",
            "http://10.0.0.1",
            "http://localhost:1234",
            "http://[2001:db8::1]",
            "https://user:secret@host",
            "https://host/?password=secret",
            "https://host/#fragment",
        ] {
            assert_eq!(
                validate_base_url(url),
                Err(ImessageError::BlueBubblesInvalidUrl),
                "{url}"
            );
        }
    }

    #[test]
    fn loopback_http_requests_bypass_inherited_proxies() {
        let server = HttpFixture::new(vec![(
            200,
            json!({"status": 200, "data": "pong"}).to_string(),
        )]);
        let proxy = HttpFixture::new(vec![]);
        let inherited_proxy = proxy.proxy();
        let expected_uri = inherited_proxy.uri().clone();
        let secure = BlueBubblesClient::with_proxy(
            validate_base_url("https://bluebubbles.example").unwrap(),
            "fixture-secret".to_string(),
            Some(inherited_proxy),
        )
        .unwrap();
        assert_eq!(secure.agent.config().proxy().unwrap().uri(), &expected_uri);
        assert_eq!(server.client_with_proxy(proxy.proxy()).ping(), Ok(()));
        assert_eq!(server.recorded_requests().len(), 1);
        assert!(proxy.recorded_requests().is_empty());
    }

    #[test]
    fn ping_requires_the_provider_pong_envelope() {
        for (status, body, expected) in [
            (
                200,
                json!({"status": 200, "data": "pong"}).to_string(),
                Ok(()),
            ),
            (
                200,
                "<html>login</html>".to_string(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                200,
                String::new(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                200,
                "{invalid".to_string(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                200,
                " ".repeat(RESPONSE_LIMIT as usize + 1),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                200,
                json!({"status": 200, "data": "wrong"}).to_string(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                200,
                json!({"status": 500, "data": "pong"}).to_string(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                200,
                json!({"status": 200, "data": "pong", "error": {}}).to_string(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                401,
                "private provider error".to_string(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
            (
                302,
                "redirect".to_string(),
                Err(ImessageError::BlueBubblesPingFailed),
            ),
        ] {
            let server = HttpFixture::new(vec![(status, body)]);
            assert_eq!(server.client().ping(), expected);
            let requests = server.recorded_requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, "GET");
            assert_eq!(requests[0].target, "/api/v1/ping?password=fixture-secret");
        }
    }

    fn outbound(guid: &str) -> OutboundMessage {
        OutboundMessage {
            content: "private reply".to_string(),
            metadata: BTreeMap::from([("conversation_id".to_string(), guid.to_string())]),
            ..Default::default()
        }
    }

    #[test]
    fn unspecified_chat_is_verified_before_sending_to_the_original_guid() {
        let guid = "any;-;+15551234567";
        let server = HttpFixture::new(vec![
            (200, chat_response(guid, 45).to_string()),
            (
                200,
                json!({"status": 200, "data": {"guid": "sent-1"}}).to_string(),
            ),
        ]);
        assert_eq!(
            server.client().send_message(&outbound(guid)),
            Ok("sent-1".to_string())
        );
        let requests = server.recorded_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].target,
            "/api/v1/message/query?password=fixture-secret"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&requests[0].body).unwrap(),
            json!({
                "chatGuid": guid, "with": ["chat"], "limit": 1, "offset": 0,
                "where": [
                    {"statement": "chat.service_name = :service", "args": {"service": "iMessage"}},
                    {"statement": "chat.style = :style", "args": {"style": 45}},
                ],
            })
        );
        assert!(!requests[0].body.contains("private reply"));
        assert_eq!(
            requests[1].target,
            "/api/v1/message/text?password=fixture-secret"
        );
        let sent: Value = serde_json::from_str(&requests[1].body).unwrap();
        assert_eq!(sent["chatGuid"], guid);
        assert_eq!(sent["method"], "apple-script");
        assert_eq!(sent["message"], "private reply");
    }

    #[test]
    fn unverified_chat_and_provider_errors_never_send_a_message() {
        let guid = "any;-;+15551234567";
        for (status, body, expected) in [
            (200, json!({"status": 200, "data": []}).to_string(), ImessageError::DmRequired),
            (200, chat_response("any;-;+15557654321", 45).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, chat_response(guid, 43).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, json!({"status": 200, "data": [{}]}).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, json!({"status": 200, "data": {}}).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, json!({"status": 200, "data": [{"chats": [{"guid": guid, "style": 45}, {"guid": guid, "style": 45}]}]}).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, json!({"status": 200, "data": [{"chats": [{"guid": guid}]}]}).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, json!({"data": []}).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, json!({"status": 200, "error": "private provider error", "data": []}).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, json!({"status": 500, "data": []}).to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, "{invalid".to_string(), ImessageError::BlueBubblesInvalidResponse),
            (200, " ".repeat(RESPONSE_LIMIT as usize + 1), ImessageError::BlueBubblesInvalidResponse),
            (500, "private provider error".to_string(), ImessageError::BlueBubblesChatVerificationFailed),
            (302, "redirect".to_string(), ImessageError::BlueBubblesChatVerificationFailed),
        ] {
            let server = HttpFixture::new(vec![(status, body)]);
            assert_eq!(server.client().send_message(&outbound(guid)), Err(expected));
            let requests = server.recorded_requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].target, "/api/v1/message/query?password=fixture-secret");
            assert!(!requests[0].body.contains("private reply"));
        }
    }

    #[test]
    fn explicit_imessage_chats_keep_the_existing_send_contract() {
        let guid = "iMessage;-;customer@example.com";
        let server = HttpFixture::new(vec![(
            200,
            json!({"status": 200, "data": {"guid": "sent-2"}}).to_string(),
        )]);
        assert_eq!(
            server.client().send_message(&outbound(guid)),
            Ok("sent-2".to_string())
        );
        let requests = server.recorded_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].target,
            "/api/v1/message/text?password=fixture-secret"
        );
        let sent: Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(sent["chatGuid"], guid);
    }

    #[test]
    fn expired_deadlines_never_contact_the_provider() {
        let server = HttpFixture::new(vec![]);
        let client = server.client();
        let deadline = Instant::now() - Duration::from_secs(1);
        let chat = ImessageDmChat::parse("any;-;+15551234567").unwrap();
        assert_eq!(
            client.verify_dm_chat_before(chat, deadline),
            Err(ImessageError::BlueBubblesChatVerificationFailed)
        );
        let request = build_send_request(
            &client.base_url,
            &client.password,
            &outbound(chat.guid()),
            Uuid::new_v4(),
        )
        .unwrap();
        assert_eq!(
            client.request_json(request, deadline, ImessageError::BlueBubblesSendFailed),
            Err(ImessageError::BlueBubblesSendFailed)
        );
        assert!(server.recorded_requests().is_empty());
    }
}
