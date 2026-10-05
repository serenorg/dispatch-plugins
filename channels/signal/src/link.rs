//! `channel-signal --link` subcommand implementation.
//!
//! Native Signal sessions are bootstrapped by linking this process as
//! a secondary device under an existing primary Signal installation
//! (your phone). The linking flow:
//!
//!   1. Opens (or creates) the SQLite store at the configured path.
//!   2. Calls `Manager::link_secondary_device` and waits for the
//!      provisioning URL to arrive on a oneshot channel.
//!   3. Renders the URL as a Unicode QR code on stderr for the operator
//!      to scan with the Signal mobile app (Settings -> Linked Devices
//!      -> Link a new device).
//!   4. Awaits the remote scan and saves registration data to the
//!      store. After this the normal JSON-RPC plugin path can load
//!      the session via `session::load_session`.
//!
//! The command is non-interactive over stdin; the terminal user just
//! scans the QR code on their phone. Successful linking prints a JSON
//! summary to stdout so it can be piped into operator tooling.

use anyhow::{Context, Result, anyhow};
use futures::channel::oneshot;
use presage::Manager;
use presage::libsignal_service::configuration::SignalServers;
use presage::model::identity::OnNewIdentity;
use presage_store_sqlite::SqliteStore;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use serde_json::json;
use std::io::Write;

use crate::protocol::ChannelConfig;
use crate::session::{RegistrationSummary, tokio_runtime};
use crate::store::{resolve_passphrase, resolve_store_path, to_sqlite_url};

#[derive(Debug)]
pub enum ParsedLinkCommand {
    Run(Box<LinkOptions>),
    Help,
}

/// Parsed CLI options for the `--link` subcommand.
#[derive(Debug)]
pub struct LinkOptions {
    pub json: bool,
    pub device_name: String,
    pub servers: SignalServers,
    pub config: ChannelConfig,
}

impl LinkOptions {
    /// Parse `channel-signal --link [flags]` arguments. Returns an
    /// error with a `--help`-style message on unknown flags or bad
    /// values.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<ParsedLinkCommand> {
        let mut json = false;
        let mut device_name: Option<String> = None;
        let mut servers = SignalServers::Production;
        let mut sqlite_store_path: Option<String> = None;
        let mut account: Option<String> = None;
        let mut passphrase_env: Option<String> = None;

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
                "--passphrase-env" => {
                    passphrase_env =
                        Some(iter.next().context("--passphrase-env requires a value")?);
                }
                "--servers" | "-s" => {
                    let value = iter.next().context("--servers requires a value")?;
                    servers = match value.as_str() {
                        "production" => SignalServers::Production,
                        "staging" => SignalServers::Staging,
                        other => {
                            return Err(anyhow!(
                                "unknown --servers value `{other}`; expected `production` or `staging`"
                            ));
                        }
                    };
                }
                "-h" | "--help" => return Ok(ParsedLinkCommand::Help),
                other => return Err(anyhow!("unknown argument `{other}`\n\n{HELP_TEXT}")),
            }
        }

        Ok(ParsedLinkCommand::Run(Box::new(Self {
            json,
            device_name: device_name.unwrap_or_else(|| "Dispatch".to_string()),
            servers,
            config: ChannelConfig {
                sqlite_store_path,
                account,
                passphrase_env,
                ..ChannelConfig::default()
            },
        })))
    }
}

pub const HELP_TEXT: &str = "\
channel-signal --link [OPTIONS]

Link this Dispatch host as a secondary Signal device against an existing
primary Signal account on your phone. You only need to run this once per
account; subsequent plugin invocations reuse the stored session.

Options:
--json
    Stream pairing codes as JSON lines on stdout; QR instructions stay on stderr.
-n, --device-name <NAME>
    Device name shown in Signal's `Linked devices` list.
    Defaults to `Dispatch`.
--account <NAME>
    Logical account name; selects the per-account subdirectory under
    the default store root. Defaults to `default`.
--sqlite-store-path <PATH>
    Absolute path to the SQLite store. Overrides --account.
--passphrase-env <NAME>
    Env var holding an optional passphrase used to encrypt the store
    at rest.
-s, --servers <production|staging>
    Signal server pool. Defaults to production.
-h, --help
    Print this help.
";

/// Run the QR-code pairing flow to completion.
pub fn run(options: LinkOptions) -> Result<()> {
    let store_path = resolve_store_path(&options.config)?;
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create Signal store directory `{}`",
                parent.display()
            )
        })?;
    }
    let url = to_sqlite_url(&store_path);
    let passphrase = resolve_passphrase(&options.config)?;

    let runtime = tokio_runtime();
    let store = runtime
        .block_on(SqliteStore::open_with_passphrase(
            &url,
            passphrase.as_deref(),
            OnNewIdentity::Trust,
        ))
        .with_context(|| {
            format!(
                "failed to open Signal session store at `{}`",
                store_path.display()
            )
        })?;

    let device_name = options.device_name.clone();
    let json_output = options.json;
    let servers = options.servers;
    let (url_tx, url_rx) = oneshot::channel::<url::Url>();

    // Run the linking future together with the URL receiver so we
    // render the QR code as soon as the URL is available while the
    // link future continues waiting for the remote scan.
    let (provisioning, link_result) = runtime.block_on(async move {
        let url_renderer = async move {
            match url_rx.await {
                Ok(url) => {
                    let provisioning_url = ensure_backup_capability(url);
                    if json_output {
                        write_json_line(&pairing_json(provisioning_url.as_str()))?;
                    }
                    if let Err(err) = render_qr_to_stderr(&provisioning_url) {
                        eprintln!("warning: failed to render QR code to stderr: {err}");
                        eprintln!("Provisioning URL (scan this manually): {provisioning_url}");
                    }
                    Ok(provisioning_url)
                }
                Err(_) => Err(anyhow!("linking flow closed the URL channel early")),
            }
        };
        let link_future = Manager::link_secondary_device(store, servers, device_name, url_tx);
        tokio::join!(url_renderer, link_future)
    });

    let _provisioning_url = provisioning?;
    let manager = link_result.map_err(|_| anyhow!("Signal linking failed"))?;

    let data = manager.registration_data();
    let summary = RegistrationSummary {
        aci: data.service_ids.aci.to_string(),
        phone_number: data.phone_number.to_string(),
        device_id: data.device_id.unwrap_or(1),
        device_name: data.device_name.clone(),
        servers: match data.signal_servers {
            SignalServers::Production => "production",
            SignalServers::Staging => "staging",
        },
    };
    write_json_line(&linked_summary_json(&store_path, &summary))?;
    Ok(())
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
    summary: &RegistrationSummary,
) -> serde_json::Value {
    json!({
        "status": "linked",
        "linked": true,
        "aci": summary.aci,
        "phone_number": summary.phone_number,
        "device_id": summary.device_id,
        "device_name": summary.device_name,
        "sqlite_store_path": store_path.display().to_string(),
        "signal_servers": summary.servers,
    })
}

fn ensure_backup_capability(mut url: url::Url) -> url::Url {
    let has_backup5 = url
        .query_pairs()
        .any(|(key, value)| key == "capabilities" && value == "backup5");
    if !has_backup5 {
        url.query_pairs_mut().append_pair("capabilities", "backup5");
    }
    url
}

fn render_qr_to_stderr(url: &url::Url) -> Result<()> {
    let code = QrCode::new(url.as_str().as_bytes())
        .context("failed to encode provisioning URL as QR code")?;
    let rendered = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build();

    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "\nScan this QR code from the Signal mobile app:")?;
    writeln!(
        stderr,
        "(Signal -> Settings -> Linked Devices -> Link a new device)"
    )?;
    writeln!(stderr)?;
    writeln!(stderr, "{rendered}")?;
    writeln!(stderr, "Provisioning URL: {url}")?;
    writeln!(stderr, "Waiting for phone to scan...")?;
    stderr.flush()?;
    Ok(())
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
        let url = ensure_backup_capability(
            url::Url::parse("sgnl://linkdevice?uuid=test&pub_key=abc").unwrap(),
        );
        assert_eq!(
            pairing_json(url.as_str()).to_string(),
            r#"{"pairing":"sgnl://linkdevice?uuid=test&pub_key=abc&capabilities=backup5"}"#
        );
        let summary = RegistrationSummary {
            aci: "00000000-0000-0000-0000-000000000009".to_string(),
            phone_number: "+15551234567".to_string(),
            device_id: 7,
            device_name: Some("Dispatch".to_string()),
            servers: "production",
        };
        let linked = linked_summary_json(std::path::Path::new("store.db"), &summary);
        assert_eq!(
            linked,
            json!({"status":"linked", "linked":true, "aci":"00000000-0000-0000-0000-000000000009", "phone_number":"+15551234567", "device_id":7, "device_name":"Dispatch", "sqlite_store_path":"store.db", "signal_servers":"production"})
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
        assert!(matches!(options.servers, SignalServers::Production));
        assert!(options.config.sqlite_store_path.is_none());
        assert!(options.config.account.is_none());
        assert!(options.config.passphrase_env.is_none());
    }

    #[test]
    fn ensure_backup_capability_appends_missing_value() {
        let url = url::Url::parse("sgnl://linkdevice?uuid=test&pub_key=abc").unwrap();

        let updated = ensure_backup_capability(url);

        assert_eq!(
            updated.as_str(),
            "sgnl://linkdevice?uuid=test&pub_key=abc&capabilities=backup5"
        );
    }

    #[test]
    fn ensure_backup_capability_does_not_duplicate_value() {
        let url = url::Url::parse("sgnl://linkdevice?uuid=test&pub_key=abc&capabilities=backup5")
            .unwrap();

        let updated = ensure_backup_capability(url);
        let capabilities: Vec<_> = updated
            .query_pairs()
            .filter(|(key, _)| key == "capabilities")
            .collect();

        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].1, "backup5");
    }

    #[test]
    fn parse_accepts_long_and_short_flags() {
        let args = vec![
            "-n".to_string(),
            "Dispatch-CI".to_string(),
            "--account".to_string(),
            "work".to_string(),
            "--sqlite-store-path".to_string(),
            "/tmp/store.db".to_string(),
            "--passphrase-env".to_string(),
            "DISPATCH_SIGNAL_PASS".to_string(),
            "-s".to_string(),
            "staging".to_string(),
        ];
        let ParsedLinkCommand::Run(options) = LinkOptions::parse(args).unwrap() else {
            panic!("expected parsed link options");
        };
        assert_eq!(options.device_name, "Dispatch-CI");
        assert!(matches!(options.servers, SignalServers::Staging));
        assert_eq!(
            options.config.sqlite_store_path.as_deref(),
            Some("/tmp/store.db")
        );
        assert_eq!(options.config.account.as_deref(), Some("work"));
        assert_eq!(
            options.config.passphrase_env.as_deref(),
            Some("DISPATCH_SIGNAL_PASS")
        );
    }

    #[test]
    fn parse_rejects_unknown_flags() {
        let args = vec!["--no-such".to_string()];
        let error = LinkOptions::parse(args).unwrap_err();
        assert!(error.to_string().contains("unknown argument"));
    }

    #[test]
    fn parse_rejects_unknown_server() {
        let args = vec!["--servers".to_string(), "localhost".to_string()];
        let error = LinkOptions::parse(args).unwrap_err();
        assert!(error.to_string().contains("localhost"));
    }

    #[test]
    fn parse_help_returns_help_command() {
        let args = vec!["--help".to_string()];
        assert!(matches!(
            LinkOptions::parse(args).unwrap(),
            ParsedLinkCommand::Help
        ));
    }
}
