use anyhow::{Context, Result, anyhow};
use futures::channel::oneshot;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use wacore::store::DevicePropsOverride;
use whatsapp_rust::TokioRuntime;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::types::events::Event;
use whatsapp_rust::waproto::whatsapp::device_props::PlatformType;
use whatsapp_rust_sqlite_storage::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::protocol::ChannelConfig;
use crate::session::{RegistrationSummary, SessionState, load_session, tokio_runtime};
use crate::store::{resolve_store_path, to_sqlite_url};

#[derive(Debug)]
pub enum ParsedLinkCommand {
    Run(Box<LinkOptions>),
    Help,
}

#[derive(Debug)]
pub struct LinkOptions {
    pub json: bool,
    pub device_name: String,
    pub config: ChannelConfig,
}

impl LinkOptions {
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<ParsedLinkCommand> {
        let mut json = false;
        let mut device_name: Option<String> = None;
        let mut sqlite_store_path: Option<String> = None;
        let mut account: Option<String> = None;

        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--json" => json = true,
                "--device-name" | "-n" => {
                    device_name = Some(iter.next().context("--device-name requires a value")?);
                }
                "--account" => {
                    account = Some(iter.next().context("--account requires a value")?);
                }
                "--sqlite-store-path" => {
                    sqlite_store_path = Some(
                        iter.next()
                            .context("--sqlite-store-path requires a value")?,
                    );
                }
                "-h" | "--help" => return Ok(ParsedLinkCommand::Help),
                other => return Err(anyhow!("unknown argument `{other}`\n\n{HELP_TEXT}")),
            }
        }

        Ok(ParsedLinkCommand::Run(Box::new(Self {
            json,
            device_name: device_name.unwrap_or_else(|| "Dispatch".to_string()),
            config: ChannelConfig {
                sqlite_store_path,
                account,
                ..ChannelConfig::default()
            },
        })))
    }
}

pub const HELP_TEXT: &str = "\
channel-whatsapp --link [OPTIONS]

Link this Dispatch host as a WhatsApp Web companion device. You only need
to run this once per account; subsequent plugin invocations reuse the stored
session.

Options:
--json
    Stream pairing codes as JSON lines on stdout; QR instructions stay on stderr.
-n, --device-name <NAME>
    Device label to advertise during pairing. Defaults to `Dispatch`.
--account <NAME>
    Logical account name; selects the per-account subdirectory under
    the default store root. Defaults to `default`.
--sqlite-store-path <PATH>
    Absolute path to the SQLite store. Overrides --account.
-h, --help
    Print this help.
";

type LinkCompletionTx = oneshot::Sender<Result<(), String>>;
type SharedLinkCompletion = Arc<Mutex<Option<LinkCompletionTx>>>;

pub fn run(options: LinkOptions) -> Result<()> {
    let store_path = resolve_store_path(&options.config)?;
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create WhatsApp store directory `{}`",
                parent.display()
            )
        })?;
    }

    let runtime = tokio_runtime();
    let sqlite_url = to_sqlite_url(&store_path);
    let backend = runtime
        .block_on(SqliteStore::new(&sqlite_url))
        .with_context(|| {
            format!(
                "failed to open WhatsApp session store at `{}`",
                store_path.display()
            )
        })?;

    let device_name = options.device_name.clone();
    let json_output = options.json;
    let link_result = runtime.block_on(async {
        let (completion_tx, completion_rx) = oneshot::channel::<Result<(), String>>();
        let completion: SharedLinkCompletion = Arc::new(Mutex::new(Some(completion_tx)));
        let completion_events = Arc::clone(&completion);

        let bot = Bot::builder()
            .with_backend(backend)
            .with_transport_factory(TokioWebSocketTransportFactory::new())
            .with_http_client(UreqHttpClient::new())
            .with_runtime(TokioRuntime)
            .with_device_props(
                DevicePropsOverride::new()
                    .with_os(device_name.clone())
                    .with_platform_type(PlatformType::Desktop),
            )
            .on_event(move |event, _client| {
                let completion = Arc::clone(&completion_events);
                async move {
                    match &*event {
                        Event::PairingQrCode(qr) => {
                            if json_output && write_json_line(&pairing_json(&qr.code)).is_err() {
                                complete_link(
                                    &completion,
                                    Err("failed to write pairing event".to_string()),
                                );
                                return;
                            }
                            if let Err(error) = render_qr_to_stderr(&qr.code) {
                                eprintln!("warning: failed to render WhatsApp QR code: {error}");
                                eprintln!("Pairing code payload: {}", qr.code);
                            }
                        }
                        Event::PairingQrCodesExhausted(_) => {
                            complete_link(&completion, Err("pairing_expired".to_string()));
                        }
                        Event::PairSuccess(_) => {
                            complete_link(&completion, Ok(()));
                        }
                        Event::PairError(_) => {
                            complete_link(&completion, Err("WhatsApp pairing failed".to_string()));
                        }
                        Event::LoggedOut(_) => {
                            complete_link(
                                &completion,
                                Err("logged out during WhatsApp pairing".to_string()),
                            );
                        }
                        Event::ClientOutdated(_) => {
                            complete_link(
                                &completion,
                                Err("WhatsApp rejected this client as outdated".to_string()),
                            );
                        }
                        _ => {}
                    }
                }
            })
            .build()
            .await
            .context("failed to build WhatsApp bot for linking")?;

        let bot_handle = bot.spawn();
        let result = completion_rx
            .await
            .map_err(|_| anyhow!("pairing flow ended before producing a result"))
            .and_then(|completion| completion.map_err(anyhow::Error::msg));

        bot_handle.shutdown().await;
        result
    });
    if let Err(error) = link_result {
        if error.to_string() == "pairing_expired" {
            write_json_line(&pairing_expired_json())?;
        }
        return Err(error);
    }

    match load_session(&options.config)? {
        SessionState::Registered {
            store_path,
            summary,
        } => {
            let summary = linked_summary_json(&store_path, &options.device_name, &summary);
            write_json_line(&summary)?;
            Ok(())
        }
        SessionState::NotYetLinked { store_path } | SessionState::StoreEmpty { store_path } => {
            Err(anyhow!(
                "pairing reported success but no registered WhatsApp session was found at `{}`",
                store_path.display()
            ))
        }
    }
}

fn pairing_expired_json() -> serde_json::Value {
    json!({"error": "pairing_expired"})
}

fn pairing_json(code: &str) -> serde_json::Value {
    json!({"pairing": code})
}

fn write_json_line(value: &serde_json::Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").context("failed to write linking event")?;
    stdout.flush().context("failed to flush linking event")
}

fn linked_summary_json(
    store_path: &std::path::Path,
    device_name: &str,
    summary: &RegistrationSummary,
) -> serde_json::Value {
    json!({
        "status": "linked",
        "linked": true,
        "phone_jid": summary.phone_jid,
        "lid_jid": summary.lid_jid,
        "device_id": summary.device_id,
        "push_name": summary.push_name,
        "device_name": device_name,
        "sqlite_store_path": store_path.display().to_string(),
    })
}

fn complete_link(completion: &SharedLinkCompletion, result: Result<(), String>) {
    if let Ok(mut guard) = completion.lock()
        && let Some(sender) = guard.take()
    {
        let _ = sender.send(result);
    }
}

fn render_qr_to_stderr(code: &str) -> Result<()> {
    let rendered = render_qr_text(code)?;
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "\nScan this QR code from the WhatsApp mobile app:")?;
    writeln!(stderr, "(WhatsApp -> Linked Devices -> Link a device)")?;
    writeln!(stderr)?;
    writeln!(stderr, "{rendered}")?;
    writeln!(stderr, "Waiting for phone to scan...")?;
    stderr.flush()?;
    Ok(())
}

fn render_qr_text(code: &str) -> Result<String> {
    let qr = QrCode::new(code.as_bytes()).context("failed to encode WhatsApp pairing QR code")?;
    Ok(qr
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_linking_flags_and_lines_match_the_stream_contract() {
        let ParsedLinkCommand::Run(options) =
            LinkOptions::parse(vec!["--json".to_string()]).unwrap()
        else {
            panic!("expected link options");
        };
        assert!(options.json);
        assert_eq!(
            pairing_json("rotating,code\nnext").to_string(),
            r#"{"pairing":"rotating,code\nnext"}"#
        );
        assert_eq!(
            pairing_expired_json().to_string(),
            r#"{"error":"pairing_expired"}"#
        );
        let summary = RegistrationSummary {
            phone_jid: Some("15551234567:7@s.whatsapp.net".to_string()),
            lid_jid: Some("987654321:7@lid".to_string()),
            device_id: 1,
            push_name: Some("Owner".to_string()),
        };
        let linked = linked_summary_json(std::path::Path::new("store.db"), "Dispatch", &summary);
        assert_eq!(
            linked,
            json!({"status":"linked", "linked":true, "phone_jid":"15551234567:7@s.whatsapp.net", "lid_jid":"987654321:7@lid", "device_id":1, "push_name":"Owner", "device_name":"Dispatch", "sqlite_store_path":"store.db"})
        );
        assert!(!linked.to_string().contains('\n'));
    }

    #[test]
    fn parse_defaults_when_no_flags() {
        let ParsedLinkCommand::Run(options) = LinkOptions::parse(Vec::<String>::new()).unwrap()
        else {
            panic!("expected parsed link options");
        };

        assert_eq!(options.device_name, "Dispatch");
        assert!(options.config.account.is_none());
        assert!(options.config.sqlite_store_path.is_none());
    }

    #[test]
    fn parse_link_options_accepts_flags() {
        let ParsedLinkCommand::Run(options) = LinkOptions::parse(vec![
            "--device-name".to_string(),
            "Dispatch QA".to_string(),
            "--account".to_string(),
            "work".to_string(),
            "--sqlite-store-path".to_string(),
            "/tmp/dispatch-whatsapp/store.db".to_string(),
        ])
        .unwrap() else {
            panic!("expected parsed link options");
        };

        assert_eq!(options.device_name, "Dispatch QA");
        assert_eq!(options.config.account.as_deref(), Some("work"));
        assert_eq!(
            options.config.sqlite_store_path.as_deref(),
            Some("/tmp/dispatch-whatsapp/store.db")
        );
    }

    #[test]
    fn parse_help_flag_returns_help_command() {
        assert!(matches!(
            LinkOptions::parse(vec!["--help".to_string()]).unwrap(),
            ParsedLinkCommand::Help
        ));
    }

    #[test]
    fn parse_unknown_flag_returns_helpful_error() {
        let error = LinkOptions::parse(vec!["--wat".to_string()]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("unknown argument `--wat`"));
        assert!(text.contains("channel-whatsapp --link [OPTIONS]"));
    }

    #[test]
    fn render_qr_text_produces_visible_output() {
        let rendered = render_qr_text("hello-whatsapp").unwrap();
        assert!(!rendered.trim().is_empty());
        assert!(rendered.contains('\n'));
    }
}
