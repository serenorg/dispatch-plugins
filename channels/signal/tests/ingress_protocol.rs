use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;

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

fn read_message(reader: &mut BufReader<std::process::ChildStdout>) -> Value {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).expect("read child stdout");
        assert!(
            bytes > 0,
            "signal child exited before emitting expected output"
        );
        if !line.trim().is_empty() {
            return serde_json::from_str(line.trim()).expect("parse plugin json");
        }
    }
}

fn run_start_ingress_cycle(config: Value) -> (Value, Value) {
    let binary = std::env::var("CARGO_BIN_EXE_channel-signal").expect("channel-signal binary path");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-signal");

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
        "channel-signal exited unsuccessfully: {status}"
    );

    (
        response.expect("start_ingress response"),
        notification.expect("channel.event notification"),
    )
}

fn serve_signal_receive_once(response_body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind signal test listener");
    let addr = listener.local_addr().expect("listener addr");

    thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buffer = Vec::new();
            let header_end;
            loop {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).expect("read request");
                assert!(read > 0, "signal test server saw EOF before headers");
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = position + 4;
                    break;
                }
            }

            let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
            let (expected_fragment, body) = if index == 0 {
                (
                    "/v1/about",
                    json!({"mode": "native", "version": "0.98"}).to_string(),
                )
            } else {
                ("/v1/receive/", response_body.clone())
            };
            assert!(headers.contains(expected_fragment));
            if index == 1 {
                assert!(headers.contains("+15550001111") || headers.contains("%2B15550001111"));
            }

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
    });

    format!("http://{}", addr)
}

fn serve_signal_websocket_receive_once(response_body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind signal websocket test listener");
    let addr = listener.local_addr().expect("listener addr");

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept about request");
        let mut buffer = Vec::new();
        let header_end;
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).expect("read about request");
            assert!(
                read > 0,
                "signal websocket test server saw EOF before headers"
            );
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = position + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        assert!(headers.contains("/v1/about"));
        let about = json!({"mode": "json-rpc", "version": "0.98"}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            about.len(),
            about
        );
        stream
            .write_all(response.as_bytes())
            .expect("write about response");

        let (stream, _) = listener.accept().expect("accept websocket request");
        let mut websocket = accept_hdr(stream, |request: &Request, response: Response| {
            assert!(request.uri().path().contains("/v1/receive/"));
            let uri = request.uri().to_string();
            assert!(uri.contains("+15550001111") || uri.contains("%2B15550001111"));
            Ok(response)
        })
        .expect("accept websocket");
        websocket
            .send(Message::Text(response_body.into()))
            .expect("send websocket frame");
        websocket.close(None).expect("close websocket");
    });

    format!("http://{}", addr)
}

#[test]
fn start_ingress_emits_signal_receive_messages() {
    let response_body = json!([{
        "envelope": {
            "source": "+15552223333",
            "sourceNumber": "+15552223333",
            "sourceName": "Alice",
            "sourceUuid": "4c5d6e7f",
            "sourceDevice": 2,
            "timestamp": 1712860000123_i64,
            "dataMessage": {
                "message": "hello from signal",
                "attachments": [{
                    "contentType": "image/jpeg",
                    "id": "att-1",
                    "size": 4096,
                    "filename": "photo.jpg"
                }]
            }
        }
    }])
    .to_string();
    let base_url = serve_signal_receive_once(response_body);

    let (response, notification) = run_start_ingress_cycle(json!({
        "base_url": base_url,
        "account": "+15550001111",
        "poll_timeout_secs": 1
    }));

    assert_eq!(response["kind"], "ingress_started");
    assert_eq!(response["state"]["mode"], "polling");
    assert_eq!(response["state"]["status"], "running");
    assert_eq!(notification["protocol_version"], 1);
    assert_eq!(notification["poll_after_ms"], 1000);
    let events = notification["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);

    let event = &events[0];
    assert_eq!(event["platform"], "signal");
    assert_eq!(event["event_type"], "message.received");
    assert_eq!(event["conversation"]["id"], "+15552223333");
    assert_eq!(event["actor"]["id"], "+15552223333");
    assert_eq!(event["actor"]["display_name"], "Alice");
    assert_eq!(event["message"]["content"], "hello from signal");
    assert_eq!(event["message"]["attachments"][0]["id"], "att-1");
    assert_eq!(
        event["message"]["attachments"][0]["mime_type"],
        "image/jpeg"
    );
    assert_eq!(event["metadata"]["transport"], "polling");
}

#[test]
fn start_ingress_emits_signal_websocket_messages() {
    let response_body = json!([{
        "envelope": {
            "source": "+15553334444",
            "sourceNumber": "+15553334444",
            "sourceName": "Bob",
            "sourceUuid": "7f6e5d4c",
            "sourceDevice": 1,
            "timestamp": 1712860000999_i64,
            "dataMessage": {
                "message": "hello from websocket"
            }
        }
    }])
    .to_string();
    let base_url = serve_signal_websocket_receive_once(response_body);

    let (_response, notification) = run_start_ingress_cycle(json!({
        "base_url": base_url,
        "account": "+15550001111",
        "poll_timeout_secs": 1
    }));

    let events = notification["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["platform"], "signal");
    assert_eq!(event["actor"]["id"], "+15553334444");
    assert_eq!(event["message"]["content"], "hello from websocket");
    assert_eq!(event["metadata"]["transport"], "websocket");
}
