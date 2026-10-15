use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

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

fn private_journal_directory() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "channel-telegram-ingress-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    dispatch_channel_runtime::create_event_journal_directory(&path)
        .expect("create private journal directory");
    path
}

/// Exact durable receipt for one `channel.event` request.
fn receipt_for(message: &Value) -> Value {
    let params = &message["params"];
    let event_ids: Vec<Value> = params["batch"]["events"]
        .as_array()
        .expect("batch events")
        .iter()
        .map(|event| event["event_id"].clone())
        .collect();
    json!({
        "jsonrpc": "2.0",
        "id": message["id"].clone(),
        "result": {
            "kind": "accepted",
            "batch_digest": params["batch_digest"].clone(),
            "event_ids": event_ids,
            "sequence": params["previous_acceptance"]["sequence"]
                .as_u64()
                .map_or(1, |sequence| sequence + 1)
        }
    })
}

fn run_ingress(request: Value) -> (Value, Vec<Value>) {
    run_ingress_with_env(request, &[])
}

/// Play the host for one ingress request: supply private journal storage,
/// answer each batch with its exact receipt, and return the control result
/// together with every confirmed batch.
fn run_ingress_with_env(request: Value, envs: &[(&str, &str)]) -> (Value, Vec<Value>) {
    let binary =
        std::env::var("CARGO_BIN_EXE_channel-telegram").expect("channel-telegram binary path");
    let directory = private_journal_directory();
    let mut wrapped = wrap_request(request);
    wrapped["params"]["storage"] = json!({
        "journal_path": directory.join("producer.journal").to_string_lossy()
    });
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .envs(envs.iter().copied())
        .spawn()
        .expect("spawn channel-telegram");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{wrapped}").expect("write request");
    let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut batches = Vec::new();
    let mut result = None;
    for line in stdout.lines() {
        let line = line.expect("read plugin output");
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line).expect("parse plugin output");
        if message["method"] == "channel.event" {
            writeln!(stdin, "{}", receipt_for(&message)).expect("write receipt");
            batches.push(message["params"]["batch"].clone());
        } else if message["id"] == 1 {
            result = Some(message["result"].clone());
            break;
        }
    }
    drop(stdin);
    let status = child.wait().expect("wait for child");
    assert!(status.success(), "channel-telegram failed: {status}");
    std::fs::remove_dir_all(directory).expect("remove journal directory");
    (result.expect("control response"), batches)
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

    let (response, batches) = run_ingress(json!({
        "protocol_version": 2,
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
    assert!(
        response.get("events").is_none(),
        "control responses carry no content"
    );
    assert_eq!(
        batches.len(),
        1,
        "the callback answers only after one confirmed batch"
    );
    let events = batches[0]["events"].as_array().expect("events array");
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
fn ingress_event_rejects_secret_mismatch_with_an_empty_confirmed_batch() {
    let (response, batches) = run_ingress_with_env(
        json!({
            "protocol_version": 2,
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
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["events"], json!([]));
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

    let (response, batches) = run_ingress(json!({
        "protocol_version": 2,
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
    assert_eq!(batches.len(), 1);
    let events = batches[0]["events"].as_array().expect("events array");
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
