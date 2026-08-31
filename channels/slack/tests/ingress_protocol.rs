use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, ExitStatus, Stdio};
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
    let body = if content_length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&buffer[body_start..body_start + content_length])
            .expect("Slack API request JSON")
    };

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

fn write_api_json_response(stream: &mut std::net::TcpStream, body: Value) {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write Slack API response");
}

fn open_test_socket(
    listener: &TcpListener,
    address: std::net::SocketAddr,
    path: &str,
    expected_app_token: &str,
) {
    let (mut stream, _) = listener.accept().expect("accept open request");
    let request = read_api_request(&mut stream);
    assert_eq!(
        request.request_line,
        "POST /api/apps.connections.open HTTP/1.1"
    );
    let expected_authorization = format!("Bearer {expected_app_token}");
    assert_eq!(
        request.authorization.as_deref(),
        Some(expected_authorization.as_str())
    );
    write_api_json_response(
        &mut stream,
        json!({ "ok": true, "url": format!("ws://{address}{path}") }),
    );
}

fn accept_test_socket(
    listener: &TcpListener,
    expected_path: &str,
) -> tungstenite::WebSocket<std::net::TcpStream> {
    let (stream, _) = listener.accept().expect("accept websocket request");
    accept_hdr(stream, |request: &Request, response: Response| {
        assert_eq!(request.uri().path(), expected_path);
        Ok(response)
    })
    .expect("accept websocket")
}

fn accept_test_reaction(listener: &TcpListener, expected_bot_token: &str, response: Value) {
    let (mut stream, _) = listener.accept().expect("accept reaction request");
    let request = read_api_request(&mut stream);
    assert_eq!(request.request_line, "POST /api/reactions.add HTTP/1.1");
    let expected_authorization = format!("Bearer {expected_bot_token}");
    assert_eq!(
        request.authorization.as_deref(),
        Some(expected_authorization.as_str())
    );
    assert_eq!(request.body["channel"], "C123");
    assert_eq!(request.body["timestamp"], "1712860000.100200");
    assert_eq!(request.body["name"], "eyes");
    write_api_json_response(&mut stream, response);
}

fn serve_slack_socket_mode_recovery(
    event_payload: Value,
    expected_app_token: &str,
    expected_bot_token: &str,
) -> (String, Receiver<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Slack socket test listener");
    let address = listener.local_addr().expect("Slack socket test address");
    let expected_app_token = expected_app_token.to_string();
    let expected_bot_token = expected_bot_token.to_string();
    let (recovered_tx, recovered_rx) = mpsc::channel();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept auth request");
        let request = read_api_request(&mut stream);
        assert_eq!(request.request_line, "POST /api/auth.test HTTP/1.1");
        let expected_authorization = format!("Bearer {expected_bot_token}");
        assert_eq!(
            request.authorization.as_deref(),
            Some(expected_authorization.as_str())
        );
        write_api_json_response(
            &mut stream,
            json!({ "ok": true, "user_id": "UBOT123", "team_id": "T123" }),
        );

        let (mut stream, _) = listener.accept().expect("accept malformed open request");
        let request = read_api_request(&mut stream);
        assert_eq!(
            request.request_line,
            "POST /api/apps.connections.open HTTP/1.1"
        );
        let expected_authorization = format!("Bearer {expected_app_token}");
        assert_eq!(
            request.authorization.as_deref(),
            Some(expected_authorization.as_str())
        );
        write_api_json_response(&mut stream, json!({ "url": "missing-ok" }));

        open_test_socket(&listener, address, "/disconnect", &expected_app_token);
        let mut socket = accept_test_socket(&listener, "/disconnect");
        socket
            .send(Message::Text(
                json!({ "type": "disconnect" }).to_string().into(),
            ))
            .expect("send disconnect envelope");

        open_test_socket(&listener, address, "/close", &expected_app_token);
        let mut socket = accept_test_socket(&listener, "/close");
        socket.close(None).expect("send close frame");

        open_test_socket(&listener, address, "/transport", &expected_app_token);
        let (stream, _) = listener
            .accept()
            .expect("accept failed websocket handshake");
        drop(stream);

        open_test_socket(&listener, address, "/event", &expected_app_token);
        let mut socket = accept_test_socket(&listener, "/event");
        socket
            .send(Message::Text(
                json!({
                    "type": "events_api",
                    "envelope_id": "socket-env-first",
                    "payload": event_payload,
                    "accepts_response_payload": false
                })
                .to_string()
                .into(),
            ))
            .expect("send recovered event");
        accept_test_reaction(&listener, &expected_bot_token, json!({ "ok": true }));
        let ack = socket.read().expect("read recovered event acknowledgement");
        let Message::Text(ack) = ack else {
            panic!("unexpected recovered event acknowledgement: {ack:?}");
        };
        let ack: Value = serde_json::from_str(ack.as_str()).expect("parse recovered event ack");
        assert_eq!(ack["envelope_id"], "socket-env-first");

        open_test_socket(&listener, address, "/redelivery", &expected_app_token);
        let mut socket = accept_test_socket(&listener, "/redelivery");
        socket
            .send(Message::Text(
                json!({
                    "type": "events_api",
                    "envelope_id": "socket-env-redelivery",
                    "payload": event_payload,
                    "accepts_response_payload": false
                })
                .to_string()
                .into(),
            ))
            .expect("send redelivered event");
        accept_test_reaction(
            &listener,
            &expected_bot_token,
            json!({ "ok": false, "error": "already_reacted" }),
        );
        let ack = socket
            .read()
            .expect("read redelivered event acknowledgement");
        let Message::Text(ack) = ack else {
            panic!("unexpected redelivered event acknowledgement: {ack:?}");
        };
        let ack: Value = serde_json::from_str(ack.as_str()).expect("parse redelivered event ack");
        assert_eq!(ack["envelope_id"], "socket-env-redelivery");
        recovered_tx
            .send(())
            .expect("report recovered socket session");
        let _ = socket.read();
    });

    (format!("http://{address}/api"), recovered_rx, server)
}

fn run_start_ingress_recovery_cycle(
    config: Value,
    envs: BTreeMap<String, String>,
    recovered: Receiver<()>,
) -> (Value, Vec<Value>, String) {
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
    let mut notifications = Vec::new();
    while response.is_none()
        || !notifications.iter().any(|notification: &Value| {
            notification["events"]
                .as_array()
                .is_some_and(|events| !events.is_empty())
        })
    {
        let message = read_message(&mut reader);
        if let Some(result) = message.get("result") {
            response = Some(result.clone());
        } else if message["method"] == "channel.event" {
            notifications.push(message["params"].clone());
        }
    }

    recovered
        .recv_timeout(Duration::from_secs(20))
        .expect("Slack worker must process the redelivery without exiting");
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

    let mut remaining_stdout = String::new();
    reader
        .read_to_string(&mut remaining_stdout)
        .expect("read remaining child stdout");
    for line in remaining_stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let message: Value = serde_json::from_str(line).expect("parse remaining plugin json");
        if message["method"] == "channel.event" {
            notifications.push(message["params"].clone());
        }
    }

    let status = child.wait().expect("wait for child");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_string(&mut stderr)
        .expect("read child stderr");
    assert!(
        status.success(),
        "channel-slack exited unsuccessfully: {status}\nstderr:\n{stderr}"
    );

    (
        response.expect("start_ingress response"),
        notifications,
        stderr,
    )
}

fn serve_slack_socket_mode_open_error(
    slack_error: &str,
    expected_app_token: &str,
    expected_bot_token: &str,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Slack socket test listener");
    let address = listener.local_addr().expect("Slack socket test address");
    let expected_app_token = expected_app_token.to_string();
    let expected_bot_token = expected_bot_token.to_string();
    let slack_error = slack_error.to_string();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept auth request");
        let request = read_api_request(&mut stream);
        assert_eq!(request.request_line, "POST /api/auth.test HTTP/1.1");
        let expected_authorization = format!("Bearer {expected_bot_token}");
        assert_eq!(
            request.authorization.as_deref(),
            Some(expected_authorization.as_str())
        );
        write_api_json_response(
            &mut stream,
            json!({ "ok": true, "user_id": "UBOT123", "team_id": "T123" }),
        );

        let (mut stream, _) = listener.accept().expect("accept open request");
        let request = read_api_request(&mut stream);
        assert_eq!(
            request.request_line,
            "POST /api/apps.connections.open HTTP/1.1"
        );
        let expected_authorization = format!("Bearer {expected_app_token}");
        assert_eq!(
            request.authorization.as_deref(),
            Some(expected_authorization.as_str())
        );
        write_api_json_response(&mut stream, json!({ "ok": false, "error": slack_error }));
    });

    (format!("http://{address}/api"), server)
}

fn run_start_ingress_expect_terminal(
    config: Value,
    envs: BTreeMap<String, String>,
) -> (String, ExitStatus) {
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

    let status = child.wait().expect("wait for child");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_string(&mut stderr)
        .expect("read child stderr");
    drop(stdin);

    (stderr, status)
}

fn socket_mode_config(app_token_env: &str, bot_token_env: &str) -> Value {
    json!({
        "app_token_env": app_token_env,
        "bot_token_env": bot_token_env,
        "allowed_team_ids": ["T123"],
        "allowed_channel_ids": ["C123"],
        "poll_timeout_secs": 2,
        "webhook_public_url": null,
        "default_channel_id": null,
        "signing_secret_env": null,
        "incoming_webhook_url_env": null
    })
}

#[test]
fn root_app_mention_round_trips_to_threaded_push() {
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
            "client_msg_id": "client-generated-id",
            "ts": "1712860000.100200",
            "event_ts": "1712860000.100200"
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
    assert_eq!(event["conversation"]["thread_id"], "1712860000.100200");
    assert!(event["conversation"]["parent_message_id"].is_null());
    assert_eq!(event["actor"]["id"], "U123");
    assert_eq!(event["message"]["id"], "1712860000.100200");
    assert_eq!(event["message"]["content"], "hello from slack");
    assert!(event["message"]["reply_to_message_id"].is_null());
    assert_eq!(event["activation"]["reason"], "direct_mention");
    assert_eq!(event["activation"]["agent_account_id"], "UBOT123");
    assert_eq!(event["metadata"]["transport"], "events_webhook");
    assert_eq!(event["metadata"]["endpoint_id"], "slack-events");
    assert_eq!(event["metadata"]["client_msg_id"], "client-generated-id");
    let conversation_id = event["conversation"]["id"]
        .as_str()
        .expect("conversation id");
    let thread_id = event["conversation"]["thread_id"]
        .as_str()
        .expect("thread id");

    let bot_token_env = "SLACK_TEST_BOT_TOKEN_THREADED_PUSH";
    let bot_token = "xoxb-test-threaded-push";
    let (base_url, request_rx, server) = serve_slack_api(vec![json!({
        "ok": true,
        "channel": "C123",
        "ts": "1712860001.100200"
    })]);
    let push = run_request_with_env(
        json!({
            "protocol_version": 1,
            "request": {
                "kind": "push",
                "config": {
                    "bot_token_env": bot_token_env,
                    "allowed_channel_ids": ["C123"]
                },
                "message": {
                    "content": "hello from the agent",
                    "metadata": {
                        "conversation_id": conversation_id,
                        "thread_id": thread_id
                    }
                }
            }
        }),
        BTreeMap::from([
            (bot_token_env.to_string(), bot_token.to_string()),
            ("SLACK_API_BASE_URL".to_string(), base_url),
        ]),
    );

    assert_eq!(push["kind"], "pushed");
    assert_eq!(push["delivery"]["conversation_id"], "C123");
    assert_eq!(push["delivery"]["message_id"], "1712860001.100200");
    assert_eq!(push["delivery"]["thread_id"], "1712860000.100200");
    let api_request = request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("chat.postMessage request");
    assert_eq!(
        api_request.request_line,
        "POST /api/chat.postMessage HTTP/1.1"
    );
    assert_eq!(
        api_request.authorization.as_deref(),
        Some("Bearer xoxb-test-threaded-push")
    );
    assert_eq!(api_request.body["channel"], "C123");
    assert_eq!(api_request.body["thread_ts"], "1712860000.100200");
    server.join().expect("Slack API server");
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
        assert_eq!(event["message"]["id"], "1712860000.100200");
        assert_eq!(event["conversation"]["thread_id"], "1712860000.100200");
        assert!(event["conversation"]["parent_message_id"].is_null());
        assert!(event["message"]["reply_to_message_id"].is_null());
        assert_eq!(event["metadata"]["message_ts"], "1712860000.100200");
        assert_eq!(event["metadata"]["client_msg_id"], "client-generated-id");
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
                "client_msg_id": "socket-client-generated-id",
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
    assert_eq!(event["conversation"]["thread_id"], "1712860000.100200");
    assert!(event["conversation"]["parent_message_id"].is_null());
    assert_eq!(event["actor"]["id"], "U123");
    assert_eq!(event["message"]["id"], "1712860000.100200");
    assert_eq!(event["message"]["content"], "hello from slack socket mode");
    assert!(event["message"]["reply_to_message_id"].is_null());
    assert_eq!(event["activation"]["reason"], "direct_mention");
    assert_eq!(event["activation"]["agent_account_id"], "UBOT123");
    assert_eq!(event["metadata"]["transport"], "socket_mode");
    assert_eq!(
        event["metadata"]["client_msg_id"],
        "socket-client-generated-id"
    );
}

#[test]
fn supervised_socket_mode_recovers_without_duplicate_delivery() {
    let app_token_env = "SLACK_TEST_APP_TOKEN_RECOVERY";
    let app_token = "xapp-recovery-token";
    let bot_token_env = "SLACK_TEST_BOT_TOKEN_RECOVERY";
    let bot_token = "xoxb-recovery-token";
    let event = json!({
        "type": "event_callback",
        "team_id": "T123",
        "api_app_id": "A123",
        "event_id": "EvSocketRecovery",
        "event_time": 1712860000,
        "event": {
            "type": "app_mention",
            "channel": "C123",
            "channel_type": "channel",
            "user": "U123",
            "text": "hello after reconnect",
            "client_msg_id": "recovery-client-generated-id",
            "ts": "1712860000.100200",
            "event_ts": "1712860000.100200"
        }
    });
    let (base_url, recovered, server) =
        serve_slack_socket_mode_recovery(event, app_token, bot_token);

    let (response, notifications, stderr) = run_start_ingress_recovery_cycle(
        socket_mode_config(app_token_env, bot_token_env),
        BTreeMap::from([
            (app_token_env.to_string(), app_token.to_string()),
            (bot_token_env.to_string(), bot_token.to_string()),
            ("SLACK_API_BASE_URL".to_string(), base_url),
        ]),
        recovered,
    );

    assert_eq!(response["kind"], "ingress_started");
    let reconnecting = notifications
        .iter()
        .filter(|notification| notification["state"]["status"] == "reconnecting")
        .count();
    assert_eq!(reconnecting, 4);
    let delivered: Vec<&Value> = notifications
        .iter()
        .flat_map(|notification| notification["events"].as_array().into_iter().flatten())
        .collect();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["event_id"], "EvSocketRecovery");
    assert_eq!(delivered[0]["message"]["id"], "1712860000.100200");
    assert_eq!(
        delivered[0]["conversation"]["thread_id"],
        "1712860000.100200"
    );
    assert_eq!(
        delivered[0]["metadata"]["client_msg_id"],
        "recovery-client-generated-id"
    );
    let delivery = notifications
        .iter()
        .find(|notification| {
            notification["events"]
                .as_array()
                .is_some_and(|events| !events.is_empty())
        })
        .expect("recovered delivery notification");
    assert_eq!(delivery["state"]["status"], "running");
    assert!(stderr.contains("missing_ok"));
    assert!(stderr.contains("retryable=true"));
    for sensitive in [app_token, bot_token, "hello after reconnect"] {
        assert!(!stderr.contains(sensitive));
    }
    server.join().expect("Slack recovery server");
}

#[test]
fn supervised_socket_mode_invalid_auth_remains_terminal() {
    let app_token_env = "SLACK_TEST_APP_TOKEN_AUTH";
    let app_token = "xapp-auth-token";
    let bot_token_env = "SLACK_TEST_BOT_TOKEN_AUTH";
    let bot_token = "xoxb-auth-token";
    let (base_url, server) =
        serve_slack_socket_mode_open_error("invalid_auth", app_token, bot_token);

    let (stderr, status) = run_start_ingress_expect_terminal(
        socket_mode_config(app_token_env, bot_token_env),
        BTreeMap::from([
            (app_token_env.to_string(), app_token.to_string()),
            (bot_token_env.to_string(), bot_token.to_string()),
            ("SLACK_API_BASE_URL".to_string(), base_url),
        ]),
    );

    assert!(
        !status.success(),
        "invalid credentials must exit the plugin process\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("slack_authentication_failed"));
    assert!(stderr.contains("invalid_auth"));
    assert!(stderr.contains("retryable=false"));
    for sensitive in [app_token, bot_token] {
        assert!(!stderr.contains(sensitive));
    }
    server.join().expect("Slack auth server");
}

#[test]
fn supervised_socket_mode_static_configuration_remains_terminal() {
    let app_token_env = "SLACK_TEST_APP_TOKEN_CONFIG";
    let app_token = "xapp-config-token";
    let bot_token_env = "SLACK_TEST_BOT_TOKEN_CONFIG";
    let bot_token = "xoxb-config-token";
    let (base_url, server) =
        serve_slack_socket_mode_open_error("no_permission", app_token, bot_token);

    let (stderr, status) = run_start_ingress_expect_terminal(
        socket_mode_config(app_token_env, bot_token_env),
        BTreeMap::from([
            (app_token_env.to_string(), app_token.to_string()),
            (bot_token_env.to_string(), bot_token.to_string()),
            ("SLACK_API_BASE_URL".to_string(), base_url),
        ]),
    );

    assert!(
        !status.success(),
        "static configuration errors must exit the plugin process\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("slack_socket_protocol_error"));
    assert!(stderr.contains("no_permission"));
    assert!(stderr.contains("retryable=false"));
    for sensitive in [app_token, bot_token] {
        assert!(!stderr.contains(sensitive));
    }
    server.join().expect("Slack configuration server");
}
