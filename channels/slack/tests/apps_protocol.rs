use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Plugin {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
    reader: Option<thread::JoinHandle<()>>,
}

impl Plugin {
    fn start(api: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_channel-slack"))
            .arg("--apps")
            .env_clear()
            .env("SLACK_API_BASE_URL", api)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sent, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("adapter stdout read failed");
                let value = serde_json::from_str(&line).expect("adapter emitted invalid JSON");
                if sent.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
            reader: Some(reader),
        }
    }

    fn send(&mut self, value: &Value) {
        writeln!(self.input, "{value}").unwrap();
        self.input.flush().unwrap();
    }

    fn next(&self) -> Value {
        self.output.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    fn acknowledge(&mut self, request: &Value, control: &str) {
        assert_eq!(request["method"], "slack.apps.checkpoint");
        self.send(&json!({"jsonrpc":"2.0","id":request["id"],"result":{
            "execution_id":request["params"]["execution_id"],
            "input_sha256":request["params"]["input_sha256"],"logical_progress":request["params"]["logical_progress"],
            "completed_result":request["params"]["completed_result"],
            "sequence":request["params"]["expected_sequence"].as_u64().unwrap()+1,"control":control}}));
    }

    fn assert_stopped(&mut self, sequence: u64) {
        let result = self.next();
        assert_eq!(result["id"], 1);
        assert_eq!(
            result["result"],
            json!({"execution_id":"33333333-3333-4333-8333-333333333333",
            "sequence":sequence,"disposition":{"kind":"stopped"}})
        );
        self.assert_exit(true);
    }

    fn assert_exit(&mut self, success: bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), success);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "adapter did not exit after Stored+Stop"
            );
            thread::sleep(Duration::from_millis(5));
        }
        self.reader.take().unwrap().join().unwrap();
        assert_eq!(self.output.try_iter().count(), 0);
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn provider(
    replies: Vec<(&'static str, Value)>,
) -> (String, Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let (sent, requests) = mpsc::channel();
    let task = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for (path, body) in replies {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("expected local provider request: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16384);
            }
            let headers = String::from_utf8(request).unwrap();
            assert!(headers.starts_with(&format!("POST /{path} HTTP/1.1\r\n")));
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(length <= 4 * 1024 * 1024);
            let mut payload = vec![0; length];
            stream.read_exact(&mut payload).unwrap();
            sent.send(path.to_string()).unwrap();
            let body = body.to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    (address, requests, task)
}

fn request() -> Value {
    use base64::Engine as _;
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(512, 512)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    json!({"jsonrpc":"2.0","id":1,"method":"slack.apps.execute","params":{
        "binding":{"owner_id":id,"organization_id":id,"deployment_id":id,
            "installation_id":id,"operation_id":id},
        "execution_id":"33333333-3333-4333-8333-333333333333","sequence":0,"checkpoint":null,
        "input":{"credentials":null,"configuration":{"configuration_token":"xoxe-local-token","refresh_token":"xoxe-local-refresh"},
            "command":{"kind":"apply","identity":{"name":"Employee","display_name":"employee","description":"Team assistance","background_color":null},
                "options":{"direct_messages":false,"private_channels":false},"callback_url":"https://core.example/callback","events_url":null,
                "icon":{"content_type":"image/png","data_base64":base64::engine::general_purpose::STANDARD.encode(png.into_inner())}}}}})
}

#[test]
fn stored_stop_at_create_intent_prevents_the_binary_from_calling_create() {
    let (api, requests, task) = provider(vec![("apps.manifest.validate", json!({"ok":true}))]);
    let mut plugin = Plugin::start(&api);
    plugin.send(&request());
    for _ in 0..2 {
        let checkpoint = plugin.next();
        plugin.acknowledge(&checkpoint, "continue");
    }
    let intent = plugin.next();
    assert_eq!(
        intent["params"]["checkpoint"]["state"]["steps"]["create"]["kind"],
        "in_flight"
    );
    plugin.acknowledge(&intent, "stop");
    plugin.assert_stopped(3);
    task.join().unwrap();
    assert_eq!(
        requests.try_iter().collect::<Vec<_>>(),
        vec!["apps.manifest.validate"]
    );
}

#[test]
fn stored_stop_at_create_receipt_preserves_credentials_and_prevents_binary_icon_upload() {
    let (api, requests, task) = provider(vec![
        ("apps.manifest.validate", json!({"ok":true})),
        (
            "apps.manifest.create",
            json!({"ok":true,"app_id":"ASTORED","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
        ),
    ]);
    let mut plugin = Plugin::start(&api);
    plugin.send(&request());
    for _ in 0..3 {
        let checkpoint = plugin.next();
        plugin.acknowledge(&checkpoint, "continue");
    }
    let receipt = plugin.next();
    assert_eq!(
        receipt["params"]["checkpoint"]["state"]["steps"]["create"]["receipt"]["app_id"],
        "ASTORED"
    );
    assert_eq!(
        receipt["params"]["checkpoint"]["state"]["steps"]["create"]["receipt"]["credentials"]["client_secret"],
        "0123456789abcdef"
    );
    assert_eq!(receipt["params"]["logical_progress"]["kind"], "application");
    assert_eq!(
        receipt["params"]["logical_progress"]["credentials"]["app_id"],
        "ASTORED"
    );
    plugin.acknowledge(&receipt, "stop");
    plugin.assert_stopped(4);
    task.join().unwrap();
    assert_eq!(
        requests.try_iter().collect::<Vec<_>>(),
        vec!["apps.manifest.validate", "apps.manifest.create"]
    );
}

#[test]
fn application_adoption_ack_is_required_before_binary_icon_or_completion() {
    let (api, requests, task) = provider(vec![
        ("apps.manifest.validate", json!({"ok":true})),
        (
            "apps.manifest.create",
            json!({"ok":true,"app_id":"AADOPT","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
        ),
    ]);
    let mut plugin = Plugin::start(&api);
    plugin.send(&request());
    for _ in 0..3 {
        let checkpoint = plugin.next();
        plugin.acknowledge(&checkpoint, "continue");
    }
    let receipt = plugin.next();
    assert_eq!(receipt["params"]["logical_progress"]["kind"], "application");
    plugin.send(&json!({"jsonrpc":"2.0","id":receipt["id"],"result":{
        "execution_id":receipt["params"]["execution_id"],"input_sha256":receipt["params"]["input_sha256"],
        "sequence":4,"control":"continue","logical_progress":null,"completed_result":null}}));
    plugin.assert_exit(false);
    task.join().unwrap();
    assert_eq!(
        requests.try_iter().collect::<Vec<_>>(),
        vec!["apps.manifest.validate", "apps.manifest.create"]
    );
}

#[test]
fn binary_publishes_a_checked_terminal_result_without_requiring_journal_parsing() {
    let (api, requests, task) = provider(vec![
        ("apps.manifest.validate", json!({"ok":true})),
        (
            "apps.manifest.create",
            json!({"ok":true,"app_id":"ARESULT","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"},"provider_extra":"protected-journal-only"}),
        ),
        ("apps.icon.set", json!({"ok":true})),
    ]);
    let mut plugin = Plugin::start(&api);
    plugin.send(&request());
    for sequence in 1..=7 {
        let checkpoint = plugin.next();
        assert_eq!(checkpoint["params"]["expected_sequence"], sequence - 1);
        if sequence == 4 {
            assert_eq!(
                checkpoint["params"]["logical_progress"]["credentials"]["app_id"],
                "ARESULT"
            );
        }
        if sequence == 7 {
            assert_eq!(
                checkpoint["params"]["completed_result"]["kind"],
                "application"
            );
            assert_eq!(
                checkpoint["params"]["completed_result"]["credentials"]["app_id"],
                "ARESULT"
            );
            assert!(
                checkpoint["params"]["completed_result"]
                    .get("provider_extra")
                    .is_none()
            );
        } else {
            assert!(checkpoint["params"]["completed_result"].is_null());
        }
        plugin.acknowledge(&checkpoint, "continue");
    }
    assert_eq!(
        plugin.next()["result"],
        json!({"execution_id":"33333333-3333-4333-8333-333333333333","sequence":7,"disposition":{"kind":"succeeded"}})
    );
    plugin.assert_exit(true);
    task.join().unwrap();
    assert_eq!(requests.try_iter().count(), 3);
}
