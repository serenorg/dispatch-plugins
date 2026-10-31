use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    AppError, AppInput, AppResult, Checkpoint, Disposition, LogicalProgress, OperationHost,
    OperationResult, Provider, execute,
};

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Authenticated by the hosting Gateway and resolved against Core's admitted operation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    owner_id: Uuid,
    organization_id: Uuid,
    deployment_id: Uuid,
    installation_id: Uuid,
    operation_id: Uuid,
}

impl Binding {
    fn validate(&self) -> AppResult<()> {
        if [
            self.owner_id,
            self.organization_id,
            self.deployment_id,
            self.installation_id,
            self.operation_id,
        ]
        .iter()
        .any(Uuid::is_nil)
        {
            return Err(AppError::BindingMismatch {});
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Work,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Work {
    binding: Binding,
    execution_id: Uuid,
    sequence: u64,
    input: Value,
    checkpoint: Option<SavedCheckpoint>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedCheckpoint {
    binding: Binding,
    input_sha256: String,
    state: Checkpoint,
}

struct Host<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
    binding: Binding,
    execution_id: Uuid,
    input_sha256: String,
    sequence: u64,
    message_id: u64,
    disposition: Disposition,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointControl {
    Continue,
    Stop,
}

impl<R: BufRead, W: Write> OperationHost for Host<'_, R, W> {
    fn checkpoint(
        &mut self,
        state: &Checkpoint,
        disposition: &Disposition,
        progress: Option<&LogicalProgress>,
        result: Option<&OperationResult>,
    ) -> AppResult<()> {
        if let Some(progress) = progress {
            progress.validate()?;
        }
        self.message_id = self
            .message_id
            .checked_add(1)
            .ok_or(AppError::StorageUnavailable {})?;
        let saved = SavedCheckpoint {
            binding: self.binding.clone(),
            input_sha256: self.input_sha256.clone(),
            state: state.clone(),
        };
        write_frame(
            self.writer,
            &json!({"jsonrpc":"2.0", "id":self.message_id, "method":"slack.apps.checkpoint",
            "params":{"binding":self.binding,"execution_id":self.execution_id,"input_sha256":self.input_sha256,
                "expected_sequence":self.sequence,"checkpoint":saved,"disposition":disposition,"logical_progress":progress,"completed_result":result}}),
        )?;
        let reply: Value = serde_json::from_slice(&read_frame(self.reader)?)
            .map_err(|_| AppError::StorageUnavailable {})?;
        let next = self
            .sequence
            .checked_add(1)
            .ok_or(AppError::StorageUnavailable {})?;
        if reply["jsonrpc"] != "2.0"
            || reply["id"].as_u64() != Some(self.message_id)
            || reply["result"]["sequence"].as_u64() != Some(next)
            || reply["result"]["execution_id"] != self.execution_id.to_string()
            || reply["result"]["input_sha256"] != self.input_sha256
            || reply["result"].get("logical_progress")
                != Some(
                    &serde_json::to_value(progress).map_err(|_| AppError::StorageUnavailable {})?,
                )
            || reply["result"].get("completed_result")
                != Some(&serde_json::to_value(result).map_err(|_| AppError::StorageUnavailable {})?)
            || reply.get("error").is_some()
        {
            return Err(AppError::StorageUnavailable {});
        }
        let control: CheckpointControl = serde_json::from_value(reply["result"]["control"].clone())
            .map_err(|_| AppError::StorageUnavailable {})?;
        self.sequence = next;
        self.disposition = disposition.clone();
        if matches!(control, CheckpointControl::Stop) {
            self.disposition = Disposition::Stopped {};
            return Err(AppError::Stopped {});
        }
        Ok(())
    }
}

fn read_frame(reader: &mut impl BufRead) -> AppResult<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|_| AppError::StorageUnavailable {})?;
        if chunk.is_empty() {
            return Err(AppError::StorageUnavailable {});
        }
        let count = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or(chunk.len(), |i| i + 1);
        if line.len() + count > MAX_FRAME_BYTES {
            return Err(AppError::invalid("frame"));
        }
        line.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            return Ok(line);
        }
    }
}

fn write_frame(writer: &mut impl Write, value: &Value) -> AppResult<()> {
    let line = serde_json::to_vec(value).map_err(|_| AppError::StorageUnavailable {})?;
    if line.len() >= MAX_FRAME_BYTES {
        return Err(AppError::invalid("frame"));
    }
    writer
        .write_all(&line)
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|_| AppError::StorageUnavailable {})
}

fn serve(reader: &mut impl BufRead, writer: &mut impl Write, provider: &Provider) -> AppResult<()> {
    let request: Request =
        serde_json::from_slice(&read_frame(reader)?).map_err(|_| AppError::invalid("request"))?;
    if request.jsonrpc != "2.0" || request.method != "slack.apps.execute" || request.id != 1 {
        return Err(AppError::invalid("request"));
    }
    let Work {
        binding,
        execution_id,
        sequence,
        input,
        checkpoint,
    } = request.params;
    binding.validate()?;
    if execution_id.is_nil() {
        return Err(AppError::BindingMismatch {});
    }
    let input_sha256 = input_digest(&input)?;
    let input: AppInput = serde_json::from_value(input).map_err(|_| AppError::invalid("input"))?;
    let mut checkpoint = match checkpoint {
        Some(saved) if saved.binding == binding && saved.input_sha256 == input_sha256 => {
            saved.state
        }
        Some(_) => return Err(AppError::BindingMismatch {}),
        None if sequence == 0 => Checkpoint::default(),
        None => return Err(AppError::BindingMismatch {}),
    };
    let mut host = Host {
        reader,
        writer,
        binding,
        execution_id,
        input_sha256,
        sequence,
        message_id: 1,
        disposition: Disposition::Running {},
    };
    let disposition = match execute(provider, &input, &mut checkpoint, &mut host) {
        Ok(_) => Disposition::Succeeded {},
        Err(AppError::RecoveryRequired {}) => Disposition::RecoveryRequired {},
        Err(AppError::StorageUnavailable {}) => return Err(AppError::StorageUnavailable {}),
        Err(AppError::Stopped {}) => Disposition::Stopped {},
        Err(_) => host.disposition.clone(),
    };
    // Provider results exist only in the acknowledged protected checkpoint.
    write_frame(
        host.writer,
        &json!({"jsonrpc":"2.0","id":1,"result":{"disposition":disposition,"sequence":host.sequence,"execution_id":host.execution_id}}),
    )
}

fn input_digest(value: &Value) -> AppResult<String> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys = map.keys().collect::<Vec<_>>();
                keys.sort();
                let mut result = serde_json::Map::new();
                for key in keys {
                    result.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(result)
            }
            Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&sorted(value)).map_err(|_| AppError::invalid("input"))?,
    )))
}

pub fn run() -> AppResult<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(&mut stdin.lock(), &mut stdout.lock(), &Provider::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> Binding {
        let id = Uuid::from_u128(1);
        Binding {
            owner_id: id,
            organization_id: id,
            deployment_id: id,
            installation_id: id,
            operation_id: id,
        }
    }

    #[test]
    fn persisted_checkpoint_cannot_be_rebound_to_other_input_or_business_identity() {
        let input = json!({"command":{"kind":"verify"},"credentials":null,"configuration":null});
        let mut cases = vec![(binding(), "wrong".into())];
        for field in [
            "owner_id",
            "organization_id",
            "deployment_id",
            "installation_id",
            "operation_id",
        ] {
            let mut changed = serde_json::to_value(binding()).unwrap();
            changed[field] = json!(Uuid::from_u128(2));
            cases.push((
                serde_json::from_value(changed).unwrap(),
                input_digest(&input).unwrap(),
            ));
        }
        for (saved_binding, digest) in cases {
            let saved = SavedCheckpoint {
                binding: saved_binding,
                input_sha256: digest,
                state: Checkpoint::default(),
            };
            let request = json!({"jsonrpc":"2.0","id":1,"method":"slack.apps.execute","params":{"binding":binding(),"execution_id":Uuid::from_u128(3),"sequence":1,"input":input,"checkpoint":saved}});
            let mut reader = std::io::Cursor::new(format!("{request}\n"));
            let mut output = Vec::new();
            assert_eq!(
                serve(
                    &mut reader,
                    &mut output,
                    &super::super::test_support::offline()
                ),
                Err(AppError::BindingMismatch {})
            );
            assert!(output.is_empty());
        }
    }

    #[test]
    fn business_binding_requires_every_identity_without_runtime_authority_fields() {
        for field in [
            "owner_id",
            "organization_id",
            "deployment_id",
            "installation_id",
            "operation_id",
        ] {
            let mut changed = serde_json::to_value(binding()).unwrap();
            changed[field] = json!(Uuid::nil());
            let changed: Binding = serde_json::from_value(changed).unwrap();
            assert_eq!(changed.validate(), Err(AppError::BindingMismatch {}));
        }
        for (field, value) in [
            ("agent_identity_id", json!(Uuid::from_u128(1))),
            ("deployment_revision_id", json!(Uuid::from_u128(1))),
            ("context_id", json!(Uuid::from_u128(1))),
            ("assignment_generation", json!(1)),
        ] {
            let mut changed = serde_json::to_value(binding()).unwrap();
            changed[field] = value;
            assert!(serde_json::from_value::<Binding>(changed).is_err());
        }
        binding().validate().unwrap();
    }

    #[test]
    fn checkpoint_acknowledgements_require_exact_sequence_and_request_identity() {
        for reply in [
            json!({"jsonrpc":"2.0","id":2,"result":{"execution_id":Uuid::from_u128(3),"input_sha256":"digest","logical_progress":null,"completed_result":null,"sequence":9,"control":"continue"}}),
            json!({"jsonrpc":"2.0","id":3,"result":{"execution_id":Uuid::from_u128(3),"input_sha256":"digest","logical_progress":null,"completed_result":null,"sequence":1,"control":"continue"}}),
        ] {
            let mut reader = std::io::Cursor::new(format!("{reply}\n"));
            let mut output = Vec::new();
            let mut host = Host {
                reader: &mut reader,
                writer: &mut output,
                binding: binding(),
                execution_id: Uuid::from_u128(3),
                input_sha256: "digest".into(),
                sequence: 0,
                message_id: 1,
                disposition: Disposition::Running {},
            };
            assert_eq!(
                host.checkpoint(&Checkpoint::default(), &Disposition::Running {}, None, None),
                Err(AppError::StorageUnavailable {})
            );
            assert_eq!(host.sequence, 0);
        }
    }

    #[test]
    fn stored_stop_records_the_exact_execution_sequence_without_permitting_continuation() {
        let reply = json!({"jsonrpc":"2.0","id":2,"result":{"execution_id":Uuid::from_u128(3),"input_sha256":"digest","logical_progress":null,"completed_result":null,"sequence":1,"control":"stop"}});
        let mut reader = std::io::Cursor::new(format!("{reply}\n"));
        let mut output = Vec::new();
        let mut host = Host {
            reader: &mut reader,
            writer: &mut output,
            binding: binding(),
            execution_id: Uuid::from_u128(3),
            input_sha256: "digest".into(),
            sequence: 0,
            message_id: 1,
            disposition: Disposition::Running {},
        };
        assert_eq!(
            host.checkpoint(&Checkpoint::default(), &Disposition::Running {}, None, None),
            Err(AppError::Stopped {})
        );
        assert_eq!(host.sequence, 1);
        assert_eq!(host.disposition, Disposition::Stopped {});
        let frame: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            frame["params"]["execution_id"],
            Uuid::from_u128(3).to_string()
        );
    }

    #[test]
    fn malformed_control_or_execution_identity_never_advances_acknowledged_state() {
        let valid = json!({"jsonrpc":"2.0","id":2,"result":{"execution_id":Uuid::from_u128(3),"input_sha256":"digest","logical_progress":null,"completed_result":null,"sequence":1,"control":"continue"}});
        let mut replies = Vec::new();
        for control in [Value::Null, json!("unknown")] {
            let mut reply = valid.clone();
            reply["result"]["control"] = control;
            replies.push(reply);
        }
        let mut missing_control = valid.clone();
        missing_control["result"]
            .as_object_mut()
            .unwrap()
            .remove("control");
        replies.push(missing_control);
        let mut wrong_execution = valid.clone();
        wrong_execution["result"]["execution_id"] = json!(Uuid::from_u128(4));
        replies.push(wrong_execution);
        let mut missing_hash = valid.clone();
        missing_hash["result"]
            .as_object_mut()
            .unwrap()
            .remove("input_sha256");
        replies.push(missing_hash);
        let mut wrong_hash = valid.clone();
        wrong_hash["result"]["input_sha256"] = json!("other-input");
        replies.push(wrong_hash);
        let mut missing_progress = valid.clone();
        missing_progress["result"]
            .as_object_mut()
            .unwrap()
            .remove("logical_progress");
        replies.push(missing_progress);
        let mut wrong_progress = valid.clone();
        wrong_progress["result"]["logical_progress"] = json!({"kind":"configuration","credentials":{"configuration_token":"xoxe-other","refresh_token":"xoxe-other-refresh"}});
        replies.push(wrong_progress);
        let mut missing_result = valid.clone();
        missing_result["result"]
            .as_object_mut()
            .unwrap()
            .remove("completed_result");
        replies.push(missing_result);
        let mut wrong_result = valid.clone();
        wrong_result["result"]["completed_result"] = json!({"kind":"verified"});
        replies.push(wrong_result);
        let mut missing_execution = valid;
        missing_execution["result"]
            .as_object_mut()
            .unwrap()
            .remove("execution_id");
        replies.push(missing_execution);
        for reply in replies {
            let mut reader = std::io::Cursor::new(format!("{reply}\n"));
            let mut output = Vec::new();
            let previous = Disposition::Retry { after_seconds: 7 };
            let mut host = Host {
                reader: &mut reader,
                writer: &mut output,
                binding: binding(),
                execution_id: Uuid::from_u128(3),
                input_sha256: "digest".into(),
                sequence: 0,
                message_id: 1,
                disposition: previous.clone(),
            };
            assert_eq!(
                host.checkpoint(&Checkpoint::default(), &Disposition::Running {}, None, None),
                Err(AppError::StorageUnavailable {})
            );
            assert_eq!(host.sequence, 0);
            assert_eq!(host.disposition, previous);
        }
    }

    #[test]
    fn immutable_input_digests_match_shared_contract_vectors() {
        #[derive(Deserialize)]
        struct Vector {
            name: String,
            input: Value,
            canonical_json: String,
            input_sha256: String,
        }
        let vectors: Vec<Vector> =
            serde_json::from_str(include_str!("../../tests/fixtures/apps_input_digests.json"))
                .unwrap();
        for vector in vectors {
            let _: AppInput = serde_json::from_value(vector.input.clone()).unwrap();
            assert_eq!(
                input_digest(&vector.input).unwrap(),
                vector.input_sha256,
                "{}",
                vector.name
            );
            assert_eq!(
                hex::encode(Sha256::digest(vector.canonical_json.as_bytes())),
                vector.input_sha256,
                "{}",
                vector.name
            );
            let reordered: Value = serde_json::from_str(&vector.canonical_json).unwrap();
            assert_eq!(
                input_digest(&reordered).unwrap(),
                vector.input_sha256,
                "{}",
                vector.name
            );
        }
    }

    fn ack(id: u64, sequence: u64, control: &str) -> Value {
        json!({"jsonrpc":"2.0","id":id,"result":{"execution_id":Uuid::from_u128(3),"input_sha256":"digest","logical_progress":null,"completed_result":null,"sequence":sequence,"control":control}})
    }

    fn serve_input(input: Value, replies: Vec<Value>, provider: &Provider) -> Vec<Value> {
        let digest = input_digest(&input).unwrap();
        let request = json!({"jsonrpc":"2.0","id":1,"method":"slack.apps.execute","params":{"binding":binding(),
            "execution_id":Uuid::from_u128(3),"sequence":0,"input":input,"checkpoint":null}});
        let mut bytes = format!("{request}\n");
        for mut reply in replies {
            reply["result"]["input_sha256"] = json!(digest);
            bytes.push_str(&format!("{reply}\n"));
        }
        let mut reader = std::io::Cursor::new(bytes);
        let mut output = Vec::new();
        serve(&mut reader, &mut output, provider).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn credentials() -> Value {
        json!({"app_id":"A1","client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789",
            "bot_token":"xoxb-local-test","team_id":"T1","bot_user_id":"U1"})
    }

    fn configuration() -> Value {
        json!({"configuration_token":"xoxe-old-token","refresh_token":"xoxe-refresh-token"})
    }

    fn identity() -> Value {
        json!({"name":"Employee","display_name":"employee","description":"Team assistance","background_color":null})
    }

    #[test]
    fn stop_at_consuming_intent_prevents_the_create_and_emits_only_the_stopped_receipt() {
        let input = json!({"credentials":null,"configuration":configuration(),"command":{"kind":"apply","identity":identity(),
            "options":{"direct_messages":false,"private_channels":false},"callback_url":"https://core.example/callback","events_url":null,"icon":null}});
        let (provider, requests, task) = super::super::test_support::script(vec![(
            "apps.manifest.validate",
            200,
            json!({"ok":true}),
        )]);
        let output = serve_input(
            input,
            vec![
                ack(2, 1, "continue"),
                ack(3, 2, "continue"),
                ack(4, 3, "stop"),
            ],
            &provider,
        );
        task.join().unwrap();
        assert_eq!(requests.try_iter().count(), 1);
        assert_eq!(output.len(), 4);
        assert_eq!(
            output.last().unwrap()["result"],
            json!({"execution_id":Uuid::from_u128(3),"sequence":3,"disposition":{"kind":"stopped"}})
        );
    }

    #[test]
    fn stop_after_each_consuming_receipt_retains_material_without_further_effects() {
        use base64::Engine as _;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(512, 512)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let icon = json!({"content_type":"image/png","data_base64":base64::engine::general_purpose::STANDARD.encode(png.into_inner())});
        let options = json!({"direct_messages":false,"private_channels":false});
        let apply = json!({"credentials":null,"configuration":configuration(),"command":{"kind":"apply","identity":identity(),
            "options":options,"callback_url":"https://core.example/callback","events_url":null,"icon":icon}});
        let events = json!({"credentials":credentials(),"configuration":configuration(),"command":{"kind":"configure_events","identity":identity(),
            "options":options,"callback_url":"https://core.example/callback","events_url":"https://core.example/events"}});
        let exchange = json!({"credentials":credentials(),"configuration":null,"command":{"kind":"exchange","code":"one-use-code",
            "options":options,"callback_url":"https://core.example/callback","expected_team_id":"T1"}});
        let cases = [
            (
                apply,
                vec![
                    ("apps.manifest.validate", 200, json!({"ok":true})),
                    (
                        "apps.manifest.create",
                        200,
                        json!({"ok":true,"app_id":"ASTORED","credentials":{"client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789"}}),
                    ),
                ],
                4,
                "create",
                "app_id",
                "ASTORED",
            ),
            (
                events,
                vec![
                    (
                        "apps.manifest.update",
                        200,
                        json!({"ok":false,"error":"token_expired"}),
                    ),
                    (
                        "tooling.tokens.rotate",
                        200,
                        json!({"ok":true,"token":"xoxe-new-token","refresh_token":"xoxe-new-refresh"}),
                    ),
                ],
                4,
                "rotate",
                "token",
                "xoxe-new-token",
            ),
            (
                exchange,
                vec![(
                    "oauth.v2.access",
                    200,
                    json!({"ok":true,"app_id":"A1","access_token":"xoxb-new-token","bot_user_id":"U1","team":{"id":"T1"},
                "scope":"app_mentions:read,chat:write,channels:read,reactions:write"}),
                )],
                2,
                "exchange",
                "access_token",
                "xoxb-new-token",
            ),
        ];
        for (input, responses, sequence, step, field, expected) in cases {
            let expected_calls = responses.len();
            let (provider, requests, task) = super::super::test_support::script(responses);
            let projection = match step {
                "create" => {
                    json!({"kind":"application","credentials":{"app_id":"ASTORED","client_id":"1.2","client_secret":"0123456789abcdef","signing_secret":"abcdef0123456789","bot_token":null,"team_id":null,"bot_user_id":null}})
                }
                "rotate" => {
                    json!({"kind":"configuration","credentials":{"configuration_token":"xoxe-new-token","refresh_token":"xoxe-new-refresh"}})
                }
                _ => Value::Null,
            };
            let replies = (1..=sequence)
                .map(|n| {
                    let mut reply = ack(n + 1, n, if n == sequence { "stop" } else { "continue" });
                    if n == sequence {
                        reply["result"]["logical_progress"] = projection.clone();
                    }
                    reply
                })
                .collect();
            let output = serve_input(input, replies, &provider);
            task.join().unwrap();
            assert_eq!(requests.try_iter().count(), expected_calls);
            assert_eq!(output.len(), sequence as usize + 1);
            assert_eq!(
                output.last().unwrap()["result"],
                json!({"execution_id":Uuid::from_u128(3),"sequence":sequence,"disposition":{"kind":"stopped"}})
            );
            let stored = &output[output.len() - 2]["params"]["checkpoint"]["state"];
            assert_eq!(stored["steps"][step]["receipt"][field], expected);
            assert!(stored["result"].is_null());
            assert_eq!(
                output[output.len() - 2]["params"]["logical_progress"],
                projection
            );
        }
    }
}
