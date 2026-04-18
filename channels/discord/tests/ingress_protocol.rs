use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn run_request(request: Value) -> Value {
    let binary =
        std::env::var("CARGO_BIN_EXE_channel-discord").expect("channel-discord binary path");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-discord");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{request}").expect("write request");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for child");
    assert!(
        output.status.success(),
        "channel-discord failed:\nstdout:\n{}\nstderr:\n{}",
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
fn ingress_event_round_trips_discord_command_interaction() {
    let body = json!({
        "id": "interaction-1",
        "application_id": "app-1",
        "type": 2,
        "guild_id": "guild-1",
        "channel_id": "channel-1",
        "locale": "en-US",
        "guild_locale": "en-US",
        "member": {
            "nick": "Dispatch User",
            "user": {
                "id": "user-1",
                "username": "dispatch-user",
                "global_name": "Dispatch User"
            }
        },
        "data": {
            "name": "ask",
            "type": 1,
            "options": [
                {
                    "name": "query",
                    "value": "hello world"
                }
            ]
        }
    })
    .to_string();

    let response = run_request(json!({
        "protocol_version": 1,
        "request": {
            "kind": "ingress_event",
            "config": {},
            "payload": {
                "endpoint_id": "discord:/discord/interactions",
                "method": "POST",
                "path": "/discord/interactions",
                "headers": {},
                "query": {},
                "body": body,
                "trust_verified": true,
                "received_at": "2026-04-11T21:00:00Z"
            }
        }
    }));

    assert_eq!(response["kind"], "ingress_events_received");
    let reply = &response["callback_reply"];
    assert_eq!(reply["status"], 200);
    assert_eq!(reply["content_type"], "application/json");

    let events = response["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["event_id"], "interaction-1");
    assert_eq!(event["platform"], "discord");
    assert_eq!(event["event_type"], "application_command");
    assert_eq!(event["conversation"]["id"], "channel-1");
    assert_eq!(event["actor"]["id"], "user-1");
    assert_eq!(event["message"]["content"], "/ask query=hello world");
    assert_eq!(event["metadata"]["transport"], "interaction_webhook");
    assert_eq!(
        event["metadata"]["endpoint_id"],
        "discord:/discord/interactions"
    );
}
