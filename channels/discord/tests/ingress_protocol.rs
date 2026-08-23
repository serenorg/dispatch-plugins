use std::io::Write;
use std::process::{Command, Stdio};

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

fn run_request(request: Value) -> Value {
    let binary =
        std::env::var("CARGO_BIN_EXE_channel-discord").expect("channel-discord binary path");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-discord");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{}", wrap_request(request)).expect("write request");
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
    let response: Value = serde_json::from_str(line).expect("parse response");
    response["result"].clone()
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
        "protocol_version": 1,
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
    let response = run_request(ingress_request(
        scoped_config(),
        interaction_body("guild-1", "channel-1"),
    ));

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

    // Provenance a host revalidates on its own before it creates a run.
    assert_eq!(event["conversation"]["kind"], "channel");
    assert_eq!(event["conversation"]["workspace_id"], "guild-1");
    assert_eq!(event["activation"]["reason"], "slash_command");
    assert_eq!(event["activation"]["agent_account_id"], "app-1");
}

#[test]
fn ingress_event_rejects_an_interaction_outside_the_configured_channel() {
    let response = run_request(ingress_request(
        scoped_config(),
        interaction_body("guild-1", "channel-other"),
    ));

    assert_eq!(response["kind"], "ingress_events_received");
    assert!(
        response["events"]
            .as_array()
            .expect("events array")
            .is_empty()
    );
}

#[test]
fn ingress_event_rejects_an_interaction_when_the_binding_has_no_scope() {
    let response = run_request(ingress_request(
        json!({}),
        interaction_body("guild-1", "channel-1"),
    ));

    assert_eq!(response["kind"], "ingress_events_received");
    assert!(
        response["events"]
            .as_array()
            .expect("events array")
            .is_empty()
    );
}
