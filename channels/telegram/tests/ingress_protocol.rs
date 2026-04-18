use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn run_request(request: Value) -> Value {
    run_request_with_env(request, &[])
}

fn run_request_with_env(request: Value, envs: &[(&str, &str)]) -> Value {
    let binary =
        std::env::var("CARGO_BIN_EXE_channel-telegram").expect("channel-telegram binary path");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .envs(envs.iter().copied())
        .spawn()
        .expect("spawn channel-telegram");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{request}").expect("write request");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for child");
    assert!(
        output.status.success(),
        "channel-telegram failed:\nstdout:\n{}\nstderr:\n{}",
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
fn ingress_event_round_trips_telegram_message_webhook() {
    let body = json!({
        "update_id": 812345678,
        "message": {
            "message_id": 321,
            "date": 1_710_000_000,
            "message_thread_id": 44,
            "chat": {
                "id": -1001234567890_i64,
                "type": "supergroup",
                "title": "Dispatch Ops"
            },
            "from": {
                "id": 9001,
                "is_bot": false,
                "username": "chris",
                "first_name": "Chris"
            },
            "text": "Status report",
            "reply_to_message": {
                "message_id": 111
            }
        }
    })
    .to_string();

    let response = run_request(json!({
        "protocol_version": 1,
        "request": {
            "kind": "ingress_event",
            "config": {},
            "payload": {
                "endpoint_id": "telegram-main",
                "method": "POST",
                "path": "/telegram/updates",
                "headers": {},
                "query": {},
                "body": body,
                "trust_verified": true,
                "received_at": "2026-04-10T21:12:13Z"
            }
        }
    }));

    assert_eq!(response["kind"], "ingress_events_received");
    assert!(response["callback_reply"].is_null());
    let events = response["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);

    let event = &events[0];
    assert_eq!(event["event_id"], "telegram:812345678:321");
    assert_eq!(event["platform"], "telegram");
    assert_eq!(event["event_type"], "message.received");
    assert_eq!(event["conversation"]["id"], "-1001234567890");
    assert_eq!(event["conversation"]["thread_id"], "44");
    assert_eq!(event["conversation"]["parent_message_id"], "111");
    assert_eq!(event["actor"]["id"], "9001");
    assert_eq!(event["actor"]["username"], "chris");
    assert_eq!(event["message"]["id"], "321");
    assert_eq!(event["message"]["content"], "Status report");
    assert_eq!(event["metadata"]["transport"], "webhook");
    assert_eq!(event["metadata"]["endpoint_id"], "telegram-main");
}

#[test]
fn ingress_event_rejects_secret_mismatch_without_emitting_events() {
    let response = run_request_with_env(
        json!({
            "protocol_version": 1,
            "request": {
                "kind": "ingress_event",
                "config": {
                    "webhook_secret_env": "DISPATCH_TELEGRAM_SECRET"
                },
                "payload": {
                    "method": "POST",
                    "path": "/telegram/updates",
                    "headers": {
                        "X-Telegram-Bot-Api-Secret-Token": "wrong-secret"
                    },
                    "query": {},
                    "body": "{}",
                    "trust_verified": false
                }
            }
        }),
        &[("DISPATCH_TELEGRAM_SECRET", "expected-secret")],
    );

    assert_eq!(response["kind"], "ingress_events_received");
    assert_eq!(response["events"], json!([]));
    assert_eq!(response["callback_reply"]["status"], 403);
    assert!(
        response["callback_reply"]["body"]
            .as_str()
            .expect("callback body")
            .contains("mismatch")
    );
}

#[test]
fn ingress_event_round_trips_telegram_media_attachment_webhook() {
    let body = json!({
        "update_id": 812345679,
        "message": {
            "message_id": 322,
            "date": 1_710_000_001,
            "chat": {
                "id": -1001234567890_i64,
                "type": "supergroup",
                "title": "Dispatch Ops"
            },
            "from": {
                "id": 9001,
                "is_bot": false,
                "username": "chris",
                "first_name": "Chris"
            },
            "caption": "see attached",
            "photo": [
                {
                    "file_id": "small-photo",
                    "file_unique_id": "photo-1",
                    "width": 90,
                    "height": 90,
                    "file_size": 1000
                },
                {
                    "file_id": "large-photo",
                    "file_unique_id": "photo-2",
                    "width": 1280,
                    "height": 720,
                    "file_size": 4096
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
                "endpoint_id": "telegram-main",
                "method": "POST",
                "path": "/telegram/updates",
                "headers": {},
                "query": {},
                "body": body,
                "trust_verified": true,
                "received_at": "2026-04-10T21:12:14Z"
            }
        }
    }));

    assert_eq!(response["kind"], "ingress_events_received");
    let events = response["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);

    let event = &events[0];
    assert_eq!(event["message"]["content"], "see attached");
    assert_eq!(
        event["message"]["attachments"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(event["message"]["attachments"][0]["id"], "large-photo");
    assert_eq!(event["message"]["attachments"][0]["kind"], "image");
    assert!(event["message"]["attachments"][0]["url"].is_null());
    assert_eq!(
        event["message"]["attachments"][0]["storage_key"],
        "telegram:file:large-photo"
    );
    assert_eq!(
        event["message"]["attachments"][0]["extras"]["file_unique_id"],
        "photo-2"
    );
    assert_eq!(
        event["message"]["metadata"]["telegram_content_origin"],
        "caption"
    );
    assert_eq!(event["message"]["metadata"]["attachment_count"], "1");
}
