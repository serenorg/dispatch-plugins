use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::Message;

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

// -----------------------------------------------------------------------------
// Gateway websocket ingress against the packaged binary.
//
// These tests drive the built `channel-discord` through a protocol-faithful fake
// gateway (HELLO -> IDENTIFY -> READY -> MESSAGE_CREATE) and a REST double, using
// the production-inert `DISCORD_GATEWAY_URL` / `DISCORD_API_BASE_URL` seams. They
// prove a notification crosses the stdio boundary for an authorized direct
// mention, and that every rejection is counted without a debug flag - the exact
// space the synthetic unit fixtures could not reach.
// -----------------------------------------------------------------------------

const GATEWAY_BOT_ID: &str = "bot-9";
const GATEWAY_APP_ID: &str = "app-1";
const GATEWAY_GUILD: &str = "guild-1";
const GATEWAY_CHANNEL: &str = "channel-1";

fn websocket_config() -> Value {
    json!({
        "allowed_guild_ids": [GATEWAY_GUILD],
        "allowed_channel_ids": [GATEWAY_CHANNEL],
        "application_id": GATEWAY_APP_ID
    })
}

/// One provider `MESSAGE_CREATE` `d` payload, shaped like the real gateway event
/// including the partial `member` object Discord nests inside each mention.
fn message_create(
    id: &str,
    channel_id: &str,
    author_id: &str,
    content: &str,
    mention_ids: &[&str],
) -> Value {
    let mentions: Vec<Value> = mention_ids
        .iter()
        .map(|mid| {
            json!({
                "id": mid,
                "username": "testbot",
                "global_name": "Testbot",
                "member": { "roles": [] }
            })
        })
        .collect();
    json!({
        "id": id,
        "channel_id": channel_id,
        "guild_id": GATEWAY_GUILD,
        "type": 0,
        "content": content,
        "timestamp": "2026-08-26T00:00:00.000000+00:00",
        "author": { "id": author_id, "username": "human", "global_name": "Human" },
        "mentions": mentions
    })
}

/// Minimal Discord REST double. Always answers the identity call the plugin makes
/// at ingress start. `channel_lookup_ok` decides whether `GET /channels/{id}`
/// resolves; a 500 reproduces the production condition where the per-message
/// channel classification is unavailable, which the fix must survive.
fn spawn_rest_double(channel_lookup_ok: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind rest listener");
    let addr = listener.local_addr().expect("rest addr");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            thread::spawn(move || serve_rest_request(stream, channel_lookup_ok));
        }
    });
    format!("http://{addr}")
}

fn serve_rest_request(mut stream: TcpStream, channel_lookup_ok: bool) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone rest stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header == "\r\n" || header == "\n" => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    let (status, body) = if path.ends_with("/users/@me") {
        (
            200,
            json!({ "id": GATEWAY_BOT_ID, "username": "testbot", "global_name": "Testbot" })
                .to_string(),
        )
    } else if let Some(channel_id) = path.strip_prefix("/channels/") {
        if channel_lookup_ok {
            (
                200,
                json!({ "id": channel_id, "type": 0, "guild_id": GATEWAY_GUILD }).to_string(),
            )
        } else {
            (500, json!({ "message": "unavailable" }).to_string())
        }
    } else {
        (404, "{}".to_string())
    };
    let response = format!(
        "HTTP/1.1 {status} STATUS\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Fake Discord gateway. Runs the HELLO -> IDENTIFY -> READY -> MESSAGE_CREATE
/// sequence for one connection, then idles until the plugin is torn down.
fn spawn_gateway_double(messages: Vec<Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind gateway listener");
    let addr = listener.local_addr().expect("gateway addr");
    let url = format!("ws://{addr}");
    let resume_url = url.clone();
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let Ok(mut ws) = tungstenite::accept(stream) else {
            return;
        };
        let hello = json!({ "op": 10, "d": { "heartbeat_interval": 45000 } });
        if ws.send(Message::Text(hello.to_string().into())).is_err() {
            return;
        }
        // Wait for IDENTIFY (op 2) or RESUME (op 6) before advertising the session.
        loop {
            match ws.read() {
                Ok(Message::Text(text)) => {
                    let parsed: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::Null);
                    if parsed["op"] == json!(2) || parsed["op"] == json!(6) {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
        let ready = json!({
            "op": 0,
            "s": 1,
            "t": "READY",
            "d": {
                "session_id": "sess-1",
                "resume_gateway_url": resume_url,
                "user": { "id": GATEWAY_BOT_ID },
                "application": { "id": GATEWAY_APP_ID }
            }
        });
        if ws.send(Message::Text(ready.to_string().into())).is_err() {
            return;
        }
        for (index, message) in messages.into_iter().enumerate() {
            let frame = json!({
                "op": 0,
                "s": 2 + index as u64,
                "t": "MESSAGE_CREATE",
                "d": message
            });
            if ws.send(Message::Text(frame.to_string().into())).is_err() {
                return;
            }
        }
        // A heartbeat ACK forces the plugin to flush its cumulative ingress
        // counters on a notification, exactly as its own 45s heartbeat would in
        // production - so a rejected or malformed message's reason count and the
        // degraded flag are observable without waiting a full heartbeat cycle.
        let ack = json!({ "op": 11 });
        let _ = ws.send(Message::Text(ack.to_string().into()));
        // Keep the socket open so the plugin stays connected until it is killed.
        while ws.read().is_ok() {}
    });
    url
}

/// One collected notification: its events array and the state metadata map.
struct CollectedNotification {
    events: Vec<Value>,
    metadata: Value,
}

/// Drive the packaged binary through one gateway session and return every
/// `channel.event` notification it wrote before teardown.
fn drive_gateway_session(
    config: Value,
    messages: Vec<Value>,
    channel_lookup_ok: bool,
) -> Vec<CollectedNotification> {
    let rest_base = spawn_rest_double(channel_lookup_ok);
    let gateway_url = spawn_gateway_double(messages);

    let binary =
        std::env::var("CARGO_BIN_EXE_channel-discord").expect("channel-discord binary path");
    let mut child = Command::new(binary)
        .env("DISCORD_BOT_TOKEN", "test-token")
        .env("DISCORD_GATEWAY_URL", &gateway_url)
        .env("DISCORD_API_BASE_URL", &rest_base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn channel-discord");

    let mut stdin = child.stdin.take().expect("child stdin");
    let request = wrap_request(json!({
        "protocol_version": 1,
        "request": { "kind": "start_ingress", "config": config, "state": Value::Null }
    }));
    writeln!(stdin, "{request}").expect("write start_ingress");
    // Hold stdin open so the plugin keeps its ingress worker running.

    let stdout = child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut notifications = Vec::new();
    let overall_deadline = Instant::now() + Duration::from_secs(15);
    // Once the first notification arrives the whole sequence has been sent, so a
    // short grace is enough to collect the message outcome and any counters.
    let mut grace_deadline: Option<Instant> = None;
    loop {
        if Instant::now() >= overall_deadline {
            break;
        }
        if let Some(grace) = grace_deadline
            && Instant::now() >= grace
        {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message["method"] == json!("channel.event") {
                    let params = &message["params"];
                    notifications.push(CollectedNotification {
                        events: params["events"].as_array().cloned().unwrap_or_default(),
                        metadata: params["state"]["metadata"].clone(),
                    });
                    grace_deadline
                        .get_or_insert_with(|| Instant::now() + Duration::from_millis(1500));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    if notifications.is_empty() {
        let mut stderr = String::new();
        if let Some(mut handle) = child.stderr.take() {
            let _ = handle.read_to_string(&mut stderr);
        }
        panic!("plugin emitted no channel.event notifications; stderr:\n{stderr}");
    }
    notifications
}

/// Every event across the collected notifications.
fn all_events(notifications: &[CollectedNotification]) -> Vec<Value> {
    notifications
        .iter()
        .flat_map(|notification| notification.events.iter().cloned())
        .collect()
}

/// The highest value a counter reached across the collected notifications.
fn max_counter(notifications: &[CollectedNotification], key: &str) -> u64 {
    notifications
        .iter()
        .filter_map(|notification| notification.metadata[key].as_str())
        .filter_map(|value| value.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

/// Whether any collected notification reported the actionable path degraded.
fn any_event_path_degraded(notifications: &[CollectedNotification]) -> bool {
    notifications
        .iter()
        .any(|notification| notification.metadata["event_path_ready"] == json!("false"))
}

#[test]
fn gateway_authorized_direct_mention_emits_one_event_without_channel_lookup() {
    // The REST channel classification is unavailable (500), reproducing the exact
    // production drop. Message Content intent is off by default, so this is also
    // the direct-mention exception case. Exactly one event must still cross stdio.
    let notifications = drive_gateway_session(
        websocket_config(),
        vec![message_create(
            "msg-1",
            GATEWAY_CHANNEL,
            "member-1",
            "hello <@bot-9>",
            &[GATEWAY_BOT_ID],
        )],
        false,
    );

    let events = all_events(&notifications);
    assert_eq!(events.len(), 1, "expected exactly one emitted event");
    assert_eq!(events[0]["conversation"]["id"], GATEWAY_CHANNEL);
    assert_eq!(events[0]["conversation"]["kind"], "channel");
    assert_eq!(events[0]["conversation"]["workspace_id"], GATEWAY_GUILD);
    assert_eq!(events[0]["activation"]["reason"], "direct_mention");
    assert_eq!(events[0]["account_id"], GATEWAY_BOT_ID);

    // The provider session announced its identity, and the actionable path is
    // healthy with the counters incremented - not just an empty heartbeat.
    assert_eq!(
        notifications
            .iter()
            .filter_map(|n| n.metadata["bot_user_id"].as_str())
            .next(),
        Some(GATEWAY_BOT_ID)
    );
    assert!(max_counter(&notifications, "frames_message_create") >= 1);
    assert!(max_counter(&notifications, "messages_accepted") >= 1);
    assert!(max_counter(&notifications, "notifications_emitted") >= 1);
    assert!(!any_event_path_degraded(&notifications));
}

#[test]
fn gateway_mention_in_another_channel_emits_zero_events_and_counts_rejection() {
    let notifications = drive_gateway_session(
        websocket_config(),
        vec![message_create(
            "msg-2",
            "channel-other",
            "member-1",
            "hello <@bot-9>",
            &[GATEWAY_BOT_ID],
        )],
        true,
    );

    assert!(
        all_events(&notifications).is_empty(),
        "no event may be emitted"
    );
    assert!(max_counter(&notifications, "reject_unauthorized_channel") >= 1);
}

#[test]
fn gateway_mention_of_another_member_emits_zero_events_and_counts_not_addressed() {
    let notifications = drive_gateway_session(
        websocket_config(),
        vec![message_create(
            "msg-3",
            GATEWAY_CHANNEL,
            "member-1",
            "look at this <@member-2>",
            &["member-2"],
        )],
        false,
    );

    assert!(all_events(&notifications).is_empty());
    assert!(max_counter(&notifications, "reject_not_addressed") >= 1);
}

#[test]
fn gateway_empty_content_emits_zero_events_and_counts_empty_message() {
    let notifications = drive_gateway_session(
        websocket_config(),
        vec![message_create(
            "msg-4",
            GATEWAY_CHANNEL,
            "member-1",
            "   ",
            &[GATEWAY_BOT_ID],
        )],
        false,
    );

    assert!(all_events(&notifications).is_empty());
    assert!(max_counter(&notifications, "reject_empty_message") >= 1);
}

#[test]
fn gateway_malformed_message_counts_decode_error_and_marks_path_degraded() {
    // A MESSAGE_CREATE whose payload cannot decode must be counted and surfaced
    // through the degraded flag, never silently dropped or tearing the session.
    let notifications = drive_gateway_session(
        websocket_config(),
        vec![json!({ "id": "msg-5", "channel_id": GATEWAY_CHANNEL })],
        false,
    );

    assert!(all_events(&notifications).is_empty());
    assert!(max_counter(&notifications, "message_decode_errors") >= 1);
    assert!(any_event_path_degraded(&notifications));
}
