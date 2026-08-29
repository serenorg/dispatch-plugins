use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::{
    Message, accept_hdr,
    handshake::server::{Request, Response},
};

fn request_method(kind: &str) -> &'static str {
    match kind {
        "capabilities" => "channel.capabilities",
        "configure" => "channel.configure",
        "health" => "channel.health",
        "start_ingress" => "channel.start_ingress",
        "stop_ingress" => "channel.stop_ingress",
        "ingress_event" => "channel.ingress_event",
        "deliver" => "channel.deliver",
        "push" => "channel.push",
        "status" => "channel.status",
        "shutdown" => "channel.shutdown",
        other => panic!("unsupported request kind `{other}`"),
    }
}

fn wrap_request(request: Value) -> Value {
    let protocol_version = request["protocol_version"].clone();
    let mut params = request["request"]
        .as_object()
        .expect("request object")
        .clone();
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .expect("request kind")
        .to_string();
    params.insert("protocol_version".to_string(), protocol_version);
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": request_method(&kind),
        "params": Value::Object(params),
    })
}

fn run_request(request: Value) -> Value {
    run_request_with_env(request, BTreeMap::new())
}

fn authenticated_ingress_state() -> Value {
    json!({
        "mode": "events_webhook",
        "status": "running",
        "endpoint": null,
        "metadata": {
            "bot_user_id": "UBOT123"
        }
    })
}

fn run_request_with_env(request: Value, envs: BTreeMap<String, String>) -> Value {
    run_request_with_env_and_stderr(request, envs).0
}

fn run_request_with_env_and_stderr(
    request: Value,
    envs: BTreeMap<String, String>,
) -> (Value, String) {
    let binary = std::env::var("CARGO_BIN_EXE_channel-slack").expect("channel-slack binary path");
    let mut child = Command::new(binary)
        .env_remove("SLACK_BOT_TOKEN")
        .env_remove("SLACK_API_BASE_URL")
        .envs(envs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn channel-slack");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{}", wrap_request(request)).expect("write request");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for child");
    assert!(
        output.status.success(),
        "channel-slack failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("response line");
    let response: Value = serde_json::from_str(line).expect("parse response");
    let payload = response
        .get("result")
        .cloned()
        .unwrap_or_else(|| response["error"]["data"]["dispatch_error"].clone());
    (
        payload,
        String::from_utf8(output.stderr).expect("stderr utf-8"),
    )
}

#[derive(Debug)]
struct CapturedApiRequest {
    request_line: String,
    authorization: Option<String>,
    body: Value,
}

fn serve_slack_api(
    responses: Vec<Value>,
) -> (String, Receiver<CapturedApiRequest>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Slack API test listener");
    let address = listener.local_addr().expect("Slack API listener address");
    let (request_tx, request_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept Slack API request");
            let request = read_api_request(&mut stream);
            request_tx.send(request).expect("capture Slack API request");

            let body = response.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write Slack API response");
        }
    });

    (format!("http://{address}/api"), request_rx, server)
}

fn read_api_request(stream: &mut std::net::TcpStream) -> CapturedApiRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set Slack API request timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("read Slack API request");
        assert!(read > 0, "Slack API connection closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };

    let header_text = String::from_utf8(buffer[..header_end].to_vec()).expect("header utf-8");
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().expect("request line").to_string();
    let mut authorization = None;
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().expect("content length");
        }
    }

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).expect("read Slack API body");
        assert!(read > 0, "Slack API connection closed before request body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&buffer[body_start..body_start + content_length])
        .expect("Slack API request JSON");

    CapturedApiRequest {
        request_line,
        authorization,
        body,
    }
}

fn read_message(reader: &mut BufReader<std::process::ChildStdout>) -> Value {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).expect("read child stdout");
        assert!(
            bytes > 0,
            "slack child exited before emitting expected output"
        );
        if !line.trim().is_empty() {
            return serde_json::from_str(line.trim()).expect("parse plugin json");
        }
    }
}

fn run_start_ingress_cycle(config: Value, envs: BTreeMap<String, String>) -> (Value, Value) {
    let binary = std::env::var("CARGO_BIN_EXE_channel-slack").expect("channel-slack binary path");
    let mut child = Command::new(binary)
        .env_remove("SLACK_BOT_TOKEN")
        .env_remove("SLACK_API_BASE_URL")
        .envs(envs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-slack");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(
        stdin,
        "{}",
        wrap_request(json!({
            "protocol_version": 1,
            "request": {
                "kind": "start_ingress",
                "config": config,
                "state": null
            }
        }))
    )
    .expect("write start_ingress request");

    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);
    let mut response = None;
    let mut notification = None;
    for _ in 0..4 {
        let message = read_message(&mut reader);
        if let Some(result) = message.get("result") {
            response = Some(result.clone());
        } else if message["method"] == "channel.event" {
            notification = Some(message["params"].clone());
        }
        if response.is_some() && notification.is_some() {
            break;
        }
    }

    writeln!(
        stdin,
        "{}",
        wrap_request(json!({
            "protocol_version": 1,
            "request": { "kind": "shutdown" }
        }))
    )
    .expect("write shutdown request");
    drop(stdin);
    let status = child.wait().expect("wait for child");
    assert!(
        status.success(),
        "channel-slack exited unsuccessfully: {status}"
    );

    (
        response.expect("start_ingress response"),
        notification.expect("channel.event notification"),
    )
}

#[allow(clippy::result_large_err)]
fn serve_slack_socket_mode_once(
    event_payload: Value,
    expected_app_token: &str,
    expected_bot_token: &str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind slack socket test listener");
    let addr = listener.local_addr().expect("listener addr");
    let expected_app_token = expected_app_token.to_string();
    let expected_bot_token = expected_bot_token.to_string();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept auth request");
        let auth_request = read_api_request(&mut stream);
        assert_eq!(auth_request.request_line, "POST /api/auth.test HTTP/1.1");
        let expected_authorization = format!("Bearer {expected_bot_token}");
        assert_eq!(
            auth_request.authorization.as_deref(),
            Some(expected_authorization.as_str())
        );
        let body = json!({
            "ok": true,
            "user_id": "UBOT123",
            "team_id": "T123"
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write auth response");

        let (mut stream, _) = listener.accept().expect("accept open request");
        let mut buffer = Vec::new();
        let header_end;
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).expect("read open request");
            assert!(read > 0, "slack socket test server saw EOF before headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = position + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let headers_lower = headers.to_ascii_lowercase();
        assert!(headers.contains("POST /api/apps.connections.open"));
        assert!(headers_lower.contains(&format!(
            "authorization: bearer {}",
            expected_app_token.to_ascii_lowercase()
        )));
        let websocket_url = format!("ws://{addr}/socket");
        let body = json!({ "ok": true, "url": websocket_url }).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write open response");

        let (stream, _) = listener.accept().expect("accept websocket request");
        let mut websocket = accept_hdr(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/socket");
            Ok(response)
        })
        .expect("accept websocket");

        websocket
            .send(Message::Text(
                json!({
                    "type": "hello",
                    "connection_info": { "app_id": "A123" }
                })
                .to_string()
                .into(),
            ))
            .expect("send hello");
        websocket
            .send(Message::Text(
                json!({
                    "type": "events_api",
                    "envelope_id": "socket-env-1",
                    "payload": event_payload,
                    "accepts_response_payload": false
                })
                .to_string()
                .into(),
            ))
            .expect("send event");

        let ack = websocket.read().expect("read socket ack");
        let Message::Text(ack_text) = ack else {
            panic!("unexpected socket ack frame: {ack:?}");
        };
        let ack_json: Value = serde_json::from_str(ack_text.as_str()).expect("parse socket ack");
        assert_eq!(ack_json["envelope_id"], "socket-env-1");

        websocket.close(None).expect("close websocket");
    });

    format!("http://{addr}/api")
}

#[test]
fn ingress_event_round_trips_slack_message_event() {
    let body = json!({
        "type": "event_callback",
        "team_id": "T123",
        "api_app_id": "A123",
        "event_id": "Ev123",
        "event_time": 1712860000,
        "event_context": "4-message-T123-C123",
        "event": {
            "type": "app_mention",
            "channel": "C123",
            "channel_type": "channel",
            "user": "U123",
            "text": "hello from slack",
            "ts": "1712860000.100200",
            "event_ts": "1712860000.100200",
            "thread_ts": "1712860000.000001"
        }
    })
    .to_string();

    let response = run_request(json!({
        "protocol_version": 1,
        "request": {
            "kind": "ingress_event",
            "config": {
                "allowed_team_ids": ["T123"],
                "allowed_channel_ids": ["C123"]
            },
            "state": authenticated_ingress_state(),
            "payload": {
                "endpoint_id": "slack-events",
                "method": "POST",
                "path": "/slack/events",
                "headers": {},
                "query": {},
                "body": body,
                "trust_verified": true,
                "received_at": "2026-04-11T18:00:00Z"
            }
        }
    }));

    assert_eq!(response["kind"], "ingress_events_received");
    assert!(response["callback_reply"].is_null());
    let events = response["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);

    let event = &events[0];
    assert_eq!(event["event_id"], "Ev123");
    assert_eq!(event["platform"], "slack");
    assert_eq!(event["event_type"], "app_mention");
    assert_eq!(event["account_id"], "UBOT123");
    assert_eq!(event["conversation"]["id"], "C123");
    assert_eq!(event["conversation"]["thread_id"], "1712860000.000001");
    assert_eq!(event["actor"]["id"], "U123");
    assert_eq!(event["message"]["content"], "hello from slack");
    assert_eq!(event["activation"]["reason"], "direct_mention");
    assert_eq!(event["activation"]["agent_account_id"], "UBOT123");
    assert_eq!(event["metadata"]["transport"], "events_webhook");
    assert_eq!(event["metadata"]["endpoint_id"], "slack-events");
}

#[test]
fn denied_bot_token_push_returns_stable_code_before_network() {
    let response = run_request_with_env(
        json!({
            "protocol_version": 1,
            "request": {
                "kind": "push",
                "config": {
                    "bot_token_env": "SLACK_TEST_BOT_TOKEN_DENIED_PUSH",
                    "allowed_channel_ids": ["C123"]
                },
                "message": {
                    "content": "must not send",
                    "channel_id": "C999"
                }
            }
        }),
        BTreeMap::from([
            (
                "SLACK_TEST_BOT_TOKEN_DENIED_PUSH".to_string(),
                "xoxb-test-denied".to_string(),
            ),
            (
                "SLACK_API_BASE_URL".to_string(),
                "http://127.0.0.1:9/api".to_string(),
            ),
        ]),
    );

    assert_eq!(response["code"], "channel_target_denied");
}

#[test]
fn accepted_ingress_reacts_after_policy_checks_with_slack_timestamp() {
    let bot_token_env = "SLACK_TEST_BOT_TOKEN_REACTIONS";
    let bot_token = "xoxb-test-reaction-token";
    let (base_url, request_rx, server) = serve_slack_api(vec![
        json!({ "ok": true }),
        json!({ "ok": false, "error": "already_reacted" }),
    ]);
    let envs = BTreeMap::from([
        (bot_token_env.to_string(), bot_token.to_string()),
        ("SLACK_API_BASE_URL".to_string(), base_url),
    ]);
    let event_body = |channel: &str| {
        json!({
            "type": "event_callback",
            "team_id": "T123",
            "event_id": "EvReaction123",
            "event_time": 1712860000,
            "event": {
                "type": "app_mention",
                "channel": channel,
                "channel_type": "channel",
                "user": "U123",
                "text": "hello from slack",
                "client_msg_id": "client-generated-id",
                "ts": "1712860000.100200",
                "event_ts": "1712860000.100200"
            }
        })
        .to_string()
    };
    let request = |body: String| {
        json!({
            "protocol_version": 1,
            "request": {
                "kind": "ingress_event",
                "config": {
                    "bot_token_env": bot_token_env,
                    "allowed_team_ids": ["T123"],
                    "allowed_channel_ids": ["C123"]
                },
                "state": authenticated_ingress_state(),
                "payload": {
                    "endpoint_id": "slack-events",
                    "method": "POST",
                    "path": "/slack/events",
                    "headers": {},
                    "query": {},
                    "body": body,
                    "trust_verified": true,
                    "received_at": "2026-04-11T18:00:00Z"
                }
            }
        })
    };

    let rejected = run_request_with_env(request(event_body("C999")), envs.clone());
    assert!(
        rejected["events"]
            .as_array()
            .expect("events array")
            .is_empty()
    );
    assert!(request_rx.recv_timeout(Duration::from_millis(100)).is_err());

    for _ in 0..2 {
        let (accepted, stderr) =
            run_request_with_env_and_stderr(request(event_body("C123")), envs.clone());
        let event = &accepted["events"][0];
        assert_eq!(event["message"]["id"], "client-generated-id");
        assert_eq!(event["metadata"]["message_ts"], "1712860000.100200");
        assert!(!stderr.contains("slack inbound acknowledgement failed"));
    }

    for _ in 0..2 {
        let api_request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reactions.add request");
        assert_eq!(api_request.request_line, "POST /api/reactions.add HTTP/1.1");
        assert_eq!(
            api_request.authorization.as_deref(),
            Some("Bearer xoxb-test-reaction-token")
        );
        assert_eq!(api_request.body["channel"], "C123");
        assert_eq!(api_request.body["timestamp"], "1712860000.100200");
        assert_eq!(api_request.body["name"], "eyes");
        assert!(api_request.body.get("client_msg_id").is_none());
    }
    server.join().expect("Slack API server");
}

#[test]
fn reaction_failure_is_fail_open_and_diagnostic_is_content_free() {
    let bot_token_env = "SLACK_TEST_BOT_TOKEN_REACTION_FAILURE";
    let bot_token = "xoxb-sensitive-test-token";
    let (base_url, request_rx, server) =
        serve_slack_api(vec![json!({ "ok": false, "error": "missing_scope" })]);
    let body = json!({
        "type": "event_callback",
        "team_id": "T123",
        "event_id": "EvReactionFailure",
        "event_time": 1712860000,
        "event": {
            "type": "app_mention",
            "channel": "C-sensitive-test",
            "channel_type": "channel",
            "user": "U123",
            "text": "sensitive message body",
            "ts": "1712860000.100200"
        }
    })
    .to_string();
    let (response, stderr) = run_request_with_env_and_stderr(
        json!({
            "protocol_version": 1,
            "request": {
                "kind": "ingress_event",
                "config": {
                    "bot_token_env": bot_token_env,
                    "allowed_team_ids": ["T123"],
                    "allowed_channel_ids": ["C-sensitive-test"]
                },
                "state": authenticated_ingress_state(),
                "payload": {
                    "endpoint_id": "slack-events",
                    "method": "POST",
                    "path": "/slack/events",
                    "headers": {},
                    "query": {},
                    "body": body,
                    "trust_verified": true,
                    "received_at": "2026-04-11T18:00:00Z"
                }
            }
        }),
        BTreeMap::from([
            (bot_token_env.to_string(), bot_token.to_string()),
            ("SLACK_API_BASE_URL".to_string(), base_url),
        ]),
    );

    assert_eq!(
        response["events"].as_array().expect("events array").len(),
        1
    );
    request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reactions.add request");
    assert_eq!(stderr.trim(), "slack inbound acknowledgement failed");
    for sensitive in [
        bot_token,
        "C-sensitive-test",
        "sensitive message body",
        "missing_scope",
    ] {
        assert!(!stderr.contains(sensitive));
    }
    server.join().expect("Slack API server");
}

#[test]
fn start_ingress_emits_slack_socket_mode_event() {
    let app_token_env = "SLACK_TEST_APP_TOKEN_SOCKET";
    let app_token = "xapp-test-token";
    let bot_token_env = "SLACK_TEST_BOT_TOKEN_SOCKET";
    let bot_token = "xoxb-test-token";
    let base_url = serve_slack_socket_mode_once(
        json!({
            "type": "event_callback",
            "team_id": "T123",
            "api_app_id": "A123",
            "event_id": "EvSocket123",
            "event_time": 1712860000,
            "event_context": "4-message-T123-C123",
            "event": {
                "type": "app_mention",
                "channel": "C123",
                "channel_type": "channel",
                "user": "U123",
                "text": "hello from slack socket mode",
                "ts": "1712860000.100200",
                "event_ts": "1712860000.100200"
            }
        }),
        app_token,
        bot_token,
    );

    let (response, notification) = run_start_ingress_cycle(
        json!({
            "app_token_env": app_token_env,
            "bot_token_env": bot_token_env,
            "allowed_team_ids": ["T123"],
            "allowed_channel_ids": ["C123"],
            "poll_timeout_secs": 5,
            "webhook_public_url": null,
            "default_channel_id": null,
            "signing_secret_env": null,
            "incoming_webhook_url_env": null
        }),
        BTreeMap::from([
            (app_token_env.to_string(), app_token.to_string()),
            (bot_token_env.to_string(), bot_token.to_string()),
            ("SLACK_API_BASE_URL".to_string(), base_url),
        ]),
    );

    assert_eq!(response["kind"], "ingress_started");
    assert_eq!(response["state"]["mode"], "polling");
    assert_eq!(response["state"]["metadata"]["mode"], "socket_mode");
    assert_eq!(notification["protocol_version"], 1);
    let events = notification["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);

    let event = &events[0];
    assert_eq!(event["event_id"], "EvSocket123");
    assert_eq!(event["platform"], "slack");
    assert_eq!(event["event_type"], "app_mention");
    assert_eq!(event["account_id"], "UBOT123");
    assert_eq!(event["conversation"]["id"], "C123");
    assert_eq!(event["actor"]["id"], "U123");
    assert_eq!(event["message"]["content"], "hello from slack socket mode");
    assert_eq!(event["activation"]["reason"], "direct_mention");
    assert_eq!(event["activation"]["agent_account_id"], "UBOT123");
    assert_eq!(event["metadata"]["transport"], "socket_mode");
}
