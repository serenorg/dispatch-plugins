use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;

use serde_json::{Value, json};

fn run_request(request: Value) -> Value {
    let binary = std::env::var("CARGO_BIN_EXE_channel-signal").expect("channel-signal binary path");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-signal");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{request}").expect("write request");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for child");
    assert!(
        output.status.success(),
        "channel-signal failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("response line");
    serde_json::from_str(line).expect("parse response")
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

#[test]
fn poll_ingress_round_trips_signal_receive_messages() {
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

    let response = run_request(json!({
        "protocol_version": 1,
        "request": {
            "kind": "poll_ingress",
            "config": {
                "base_url": base_url,
                "account": "+15550001111"
            }
        }
    }));

    assert_eq!(response["kind"], "ingress_events_received");
    assert!(response["callback_reply"].is_null());
    assert_eq!(response["poll_after_ms"], 1000);
    assert_eq!(response["state"]["mode"], "polling");
    assert_eq!(response["state"]["status"], "running");
    let events = response["events"].as_array().expect("events array");
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
