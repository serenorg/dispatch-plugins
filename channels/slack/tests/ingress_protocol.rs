use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn run_request(request: Value) -> Value {
    let binary = std::env::var("CARGO_BIN_EXE_channel-slack").expect("channel-slack binary path");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-slack");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{request}").expect("write request");
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
    serde_json::from_str(line).expect("parse response")
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
            "config": {},
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
    assert_eq!(event["conversation"]["id"], "C123");
    assert_eq!(event["conversation"]["thread_id"], "1712860000.000001");
    assert_eq!(event["actor"]["id"], "U123");
    assert_eq!(event["message"]["content"], "hello from slack");
    assert_eq!(event["metadata"]["transport"], "events_webhook");
    assert_eq!(event["metadata"]["endpoint_id"], "slack-events");
}
