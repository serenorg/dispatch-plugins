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
        "channel-discord-ingress-{}-{}-{}",
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

/// Play the host for one ingress request: supply private journal storage,
/// answer each batch with its exact receipt, and return the control result
/// together with every confirmed batch.
fn run_ingress(request: Value) -> (Value, Vec<Value>) {
    let binary =
        std::env::var("CARGO_BIN_EXE_channel-discord").expect("channel-discord binary path");
    let directory = private_journal_directory();
    let mut wrapped = wrap_request(request);
    wrapped["params"]["storage"] = json!({
        "journal_path": directory.join("producer.journal").to_string_lossy()
    });
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-discord");

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
    assert!(status.success(), "channel-discord failed: {status}");
    std::fs::remove_dir_all(directory).expect("remove journal directory");
    (result.expect("control response"), batches)
}

/// Scope a binding to one guild and one channel, as an operator would.
fn scoped_config() -> Value {
    json!({
        "allowed_guild_ids": ["guild-1"],
        "allowed_channel_ids": ["channel-1"]
    })
}

fn interaction_body(guild_id: &str, channel_id: &str) -> String {
    json!({
        "id": "interaction-1",
        "application_id": "app-1",
        "type": 2,
        "guild_id": guild_id,
        "channel_id": channel_id,
        "channel": {
            "id": channel_id,
            "type": 0
        },
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
    .to_string()
}

fn ingress_request(config: Value, body: String) -> Value {
    json!({
        "protocol_version": 2,
        "request": {
            "kind": "ingress_event",
            "config": config,
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
    })
}

#[test]
fn ingress_event_round_trips_discord_command_interaction() {
    let (response, batches) = run_ingress(ingress_request(
        scoped_config(),
        interaction_body("guild-1", "channel-1"),
    ));

    assert_eq!(response["kind"], "ingress_events_received");
    assert!(
        response.get("events").is_none(),
        "control responses carry no content"
    );
    let reply = &response["callback_reply"];
    assert_eq!(reply["status"], 200);
    assert_eq!(reply["content_type"], "application/json");

    assert_eq!(
        batches.len(),
        1,
        "the callback answers only after one confirmed batch"
    );
    let events = batches[0]["events"].as_array().expect("events array");
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

    // Provenance a host revalidates on its own before it creates a run.
    assert_eq!(event["conversation"]["kind"], "channel");
    assert_eq!(event["conversation"]["workspace_id"], "guild-1");
    assert_eq!(event["activation"]["reason"], "slash_command");
    assert_eq!(event["activation"]["agent_account_id"], "app-1");
}

#[test]
fn ingress_event_rejects_an_interaction_outside_the_configured_channel() {
    let (response, batches) = run_ingress(ingress_request(
        scoped_config(),
        interaction_body("guild-1", "channel-other"),
    ));

    assert_eq!(response["kind"], "ingress_events_received");
    assert_eq!(batches.len(), 1);
    assert!(
        batches[0]["events"]
            .as_array()
            .expect("events array")
            .is_empty()
    );
}

#[test]
fn ingress_event_rejects_an_interaction_when_the_binding_has_no_scope() {
    let (response, batches) = run_ingress(ingress_request(
        json!({}),
        interaction_body("guild-1", "channel-1"),
    ));

    assert_eq!(response["kind"], "ingress_events_received");
    assert_eq!(batches.len(), 1);
    assert!(
        batches[0]["events"]
            .as_array()
            .expect("events array")
            .is_empty()
    );
}
