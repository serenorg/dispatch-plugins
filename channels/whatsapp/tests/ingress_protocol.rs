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
        std::env::var("CARGO_BIN_EXE_channel-whatsapp").expect("channel-whatsapp binary path");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn channel-whatsapp");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{}", wrap_request(request)).expect("write request");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for child");
    assert!(
        output.status.success(),
        "channel-whatsapp failed:\nstdout:\n{}\nstderr:\n{}",
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

#[test]
fn ingress_event_round_trips_whatsapp_message_webhook() {
    let body = json!({
        "entry": [
            {
                "id": "WABA123",
                "changes": [
                    {
                        "field": "messages",
                        "value": {
                            "metadata": {
                                "display_phone_number": "15551234567",
                                "phone_number_id": "PN123"
                            },
                            "contacts": [
                                {
                                    "wa_id": "15550001111",
                                    "profile": {"name": "Alice"}
                                }
                            ],
                            "messages": [
                                {
                                    "from": "15550001111",
                                    "id": "wamid.HBgN",
                                    "timestamp": "1712860000",
                                    "type": "text",
                                    "text": {"body": "hello from whatsapp"},
                                    "context": {"id": "wamid.parent"}
                                }
                            ]
                        }
                    }
                ]
            }
        ]
    })
    .to_string();

    let response = run_request(json!({
        "protocol_version": 1,
        "request": {
            "kind": "ingress_event",
            "config": {},
            "payload": {
                "endpoint_id": "whatsapp:webhook",
                "method": "POST",
                "path": "/whatsapp/webhook",
                "headers": {},
                "query": {},
                "body": body,
                "trust_verified": true,
                "received_at": "2026-04-11T20:00:00Z"
            }
        }
    }));

    assert_eq!(response["kind"], "ingress_events_received");
    assert!(response["callback_reply"].is_null());
    let events = response["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);

    let event = &events[0];
    assert_eq!(event["event_id"], "wamid.HBgN");
    assert_eq!(event["platform"], "whatsapp");
    assert_eq!(event["conversation"]["id"], "15550001111");
    assert_eq!(event["conversation"]["kind"], "phone_number");
    assert_eq!(event["actor"]["display_name"], "Alice");
    assert_eq!(event["message"]["content"], "hello from whatsapp");
    assert_eq!(event["message"]["reply_to_message_id"], "wamid.parent");
    assert_eq!(event["account_id"], "PN123");
    assert_eq!(event["metadata"]["endpoint_id"], "whatsapp:webhook");
}
