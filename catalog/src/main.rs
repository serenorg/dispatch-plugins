use anyhow::{Context, Result, bail};
use channel_schema::{CatalogEntry, CatalogInstallSource, ExtensionCatalog, ManifestKind};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (catalog_path, command_index) = parse_catalog_flag(&args)?;
    let catalog = load_catalog(&catalog_path)?;

    if command_index >= args.len() {
        print_entries(&catalog.entries);
        return Ok(());
    }

    match args[command_index].as_str() {
        "list" => {
            let kind = args
                .get(command_index + 1)
                .map(|value| parse_kind(value))
                .transpose()?;
            let entries: Vec<&CatalogEntry> = catalog
                .entries
                .iter()
                .filter(|entry| kind.as_ref().is_none_or(|kind| &entry.kind == kind))
                .collect();
            print_entry_refs(&entries);
        }
        "search" => {
            let query = args
                .get(command_index + 1)
                .ok_or_else(|| anyhow::anyhow!("search requires a query"))?;
            let query = query.to_ascii_lowercase();
            let entries: Vec<&CatalogEntry> = catalog
                .entries
                .iter()
                .filter(|entry| matches_query(entry, &query))
                .collect();
            print_entry_refs(&entries);
        }
        "inspect" => {
            let name = args
                .get(command_index + 1)
                .ok_or_else(|| anyhow::anyhow!("inspect requires an extension name"))?;
            let entry = find_entry(&catalog, name)?;
            println!("{}", serde_json::to_string_pretty(entry)?);
        }
        "install-hint" => {
            let name = args
                .get(command_index + 1)
                .ok_or_else(|| anyhow::anyhow!("install-hint requires an extension name"))?;
            let entry = find_entry(&catalog, name)?;
            if let Some(hint) = &entry.install_hint {
                println!("{hint}");
            } else {
                bail!("no install hint recorded for {}", entry.name);
            }
        }
        "catalog-path" => {
            println!("{}", catalog_path.display());
        }
        "release-assets" => {
            let config = parse_release_assets_args(&args[command_index + 1..])?;
            let assets = release_assets(&catalog, &config)?;
            println!("{}", serde_json::to_string_pretty(&assets)?);
        }
        "stage-release-assets" => {
            let config = parse_stage_release_assets_args(&args[command_index + 1..])?;
            stage_release_assets(&catalog, &config)?;
        }
        "help" | "--help" | "-h" => print_help(),
        other => bail!("unknown command `{other}`"),
    }

    Ok(())
}

fn parse_catalog_flag(args: &[String]) -> Result<(PathBuf, usize)> {
    if args.len() >= 2 && args[0] == "--catalog" {
        return Ok((PathBuf::from(&args[1]), 2));
    }
    Ok((resolve_default_catalog_path()?, 0))
}

fn resolve_default_catalog_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("DISPATCH_EXTENSION_CATALOG") {
        return Ok(PathBuf::from(path));
    }

    let candidates = [
        PathBuf::from("extensions.json"),
        PathBuf::from("catalog/extensions.json"),
        PathBuf::from("../catalog/extensions.json"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!("could not resolve catalog/extensions.json; pass --catalog <path>");
}

fn load_catalog(path: &Path) -> Result<ExtensionCatalog> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("failed to read catalog file {}", path.display()))?;
    let catalog: ExtensionCatalog = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse catalog file {}", path.display()))?;
    validate_catalog_identity(&catalog).with_context(|| {
        format!(
            "catalog file {} has invalid identity metadata",
            path.display()
        )
    })?;
    Ok(catalog)
}

fn validate_catalog_identity(catalog: &ExtensionCatalog) -> Result<()> {
    validate_catalog_identifier("catalog_id", &catalog.catalog_id)?;
    let entry_id_prefix = format!("{}.", catalog.catalog_id);
    let mut seen_entry_ids = std::collections::BTreeSet::new();
    let mut seen_entry_names = std::collections::BTreeSet::new();

    for entry in &catalog.entries {
        validate_catalog_identifier("entry id", &entry.id)
            .with_context(|| format!("catalog entry `{}` has invalid id", entry.name))?;
        if !entry.id.starts_with(&entry_id_prefix) {
            bail!(
                "catalog entry `{}` id `{}` must start with catalog_id prefix `{entry_id_prefix}`",
                entry.name,
                entry.id
            );
        }
        if !seen_entry_ids.insert(entry.id.clone()) {
            bail!("duplicate catalog entry id `{}`", entry.id);
        }
        if !seen_entry_names.insert(entry.name.clone()) {
            bail!("duplicate catalog entry name `{}`", entry.name);
        }
    }
    Ok(())
}

fn validate_catalog_identifier(field: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field} must not be empty");
    }
    if trimmed != value {
        bail!("{field} must not contain leading or trailing whitespace");
    }
    if value.len() > 80 {
        bail!("{field} must be at most 80 characters");
    }
    if value.starts_with('.') || value.ends_with('.') {
        bail!("{field} must not start or end with `.`");
    }
    if value.split('.').any(str::is_empty) {
        bail!("{field} must not contain empty `.`-separated segments");
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
    }) {
        bail!("{field} must use lowercase ASCII letters, digits, `.`, `-`, or `_`");
    }
    Ok(())
}

fn parse_kind(value: &str) -> Result<ManifestKind> {
    match value {
        "channel" => Ok(ManifestKind::Channel),
        "courier" => Ok(ManifestKind::Courier),
        "connector" => Ok(ManifestKind::Connector),
        other => bail!("unsupported kind `{other}`"),
    }
}

fn matches_query(entry: &CatalogEntry, query: &str) -> bool {
    entry.name.to_ascii_lowercase().contains(query)
        || entry.display_name.to_ascii_lowercase().contains(query)
        || entry.description.to_ascii_lowercase().contains(query)
        || entry
            .keywords
            .iter()
            .any(|keyword| keyword.to_ascii_lowercase().contains(query))
        || entry
            .tags
            .iter()
            .any(|tag| tag.to_ascii_lowercase().contains(query))
}

fn find_entry<'a>(catalog: &'a ExtensionCatalog, name: &str) -> Result<&'a CatalogEntry> {
    catalog
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| anyhow::anyhow!("extension `{name}` not found"))
}

fn print_entries(entries: &[CatalogEntry]) {
    let refs: Vec<&CatalogEntry> = entries.iter().collect();
    print_entry_refs(&refs);
}

fn print_entry_refs(entries: &[&CatalogEntry]) {
    for entry in entries {
        println!(
            "{:<9} {:<32} {:<8} {}",
            kind_label(&entry.kind),
            entry.name,
            entry.version,
            entry.display_name
        );
    }
}

fn kind_label(kind: &ManifestKind) -> &'static str {
    match kind {
        ManifestKind::Channel => "channel",
        ManifestKind::Courier => "courier",
        ManifestKind::Connector => "connector",
    }
}

fn print_help() {
    println!("dispatch-extension-catalog [--catalog PATH] <command>");
    println!();
    println!("Commands:");
    println!("  list [channel|courier|connector]");
    println!("  search <query>");
    println!("  inspect <name>");
    println!("  install-hint <name>");
    println!("  catalog-path");
    println!("  release-assets --target <triple> [--extra <binary>...]");
    println!(
        "  stage-release-assets --target <triple> --release-dir <path> --dist-dir <path> [--extra <binary>...]"
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseAssetsConfig {
    target: String,
    extras: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct ReleaseAsset {
    name: String,
    binary_name: String,
    asset: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StageReleaseAssetsConfig {
    assets: ReleaseAssetsConfig,
    release_dir: PathBuf,
    dist_dir: PathBuf,
}

fn parse_release_assets_args(args: &[String]) -> Result<ReleaseAssetsConfig> {
    let mut target = None;
    let mut extras = Vec::new();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--target" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    anyhow::anyhow!("release-assets requires a value after --target")
                })?;
                target = Some(value.clone());
                index += 2;
            }
            "--extra" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    anyhow::anyhow!("release-assets requires a value after --extra")
                })?;
                extras.push(value.clone());
                index += 2;
            }
            other => bail!("unknown release-assets argument `{other}`"),
        }
    }

    let target =
        target.ok_or_else(|| anyhow::anyhow!("release-assets requires --target <triple>"))?;

    Ok(ReleaseAssetsConfig { target, extras })
}

fn parse_stage_release_assets_args(args: &[String]) -> Result<StageReleaseAssetsConfig> {
    let mut target = None;
    let mut extras = Vec::new();
    let mut release_dir = None;
    let mut dist_dir = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--target" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    anyhow::anyhow!("stage-release-assets requires a value after --target")
                })?;
                target = Some(value.clone());
                index += 2;
            }
            "--extra" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    anyhow::anyhow!("stage-release-assets requires a value after --extra")
                })?;
                extras.push(value.clone());
                index += 2;
            }
            "--release-dir" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    anyhow::anyhow!("stage-release-assets requires a value after --release-dir")
                })?;
                release_dir = Some(PathBuf::from(value));
                index += 2;
            }
            "--dist-dir" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    anyhow::anyhow!("stage-release-assets requires a value after --dist-dir")
                })?;
                dist_dir = Some(PathBuf::from(value));
                index += 2;
            }
            other => bail!("unknown stage-release-assets argument `{other}`"),
        }
    }

    let target =
        target.ok_or_else(|| anyhow::anyhow!("stage-release-assets requires --target <triple>"))?;
    let release_dir = release_dir
        .ok_or_else(|| anyhow::anyhow!("stage-release-assets requires --release-dir <path>"))?;
    let dist_dir = dist_dir
        .ok_or_else(|| anyhow::anyhow!("stage-release-assets requires --dist-dir <path>"))?;

    Ok(StageReleaseAssetsConfig {
        assets: ReleaseAssetsConfig { target, extras },
        release_dir,
        dist_dir,
    })
}

fn release_assets(
    catalog: &ExtensionCatalog,
    config: &ReleaseAssetsConfig,
) -> Result<Vec<ReleaseAsset>> {
    let mut assets = Vec::new();
    let mut seen_assets = std::collections::BTreeSet::new();
    let mut seen_binaries = std::collections::BTreeSet::new();

    let mut entries: Vec<&CatalogEntry> = catalog
        .entries
        .iter()
        .filter(|entry| entry.kind == ManifestKind::Channel)
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    for entry in entries {
        let asset = release_asset_for_entry(entry, &config.target)?;
        register_release_asset(&asset, &mut seen_assets, &mut seen_binaries)?;
        assets.push(asset);
    }

    for extra in &config.extras {
        let asset = release_asset_for_extra(extra, &config.target);
        register_release_asset(&asset, &mut seen_assets, &mut seen_binaries)?;
        assets.push(asset);
    }

    Ok(assets)
}

fn release_asset_for_entry(entry: &CatalogEntry, target: &str) -> Result<ReleaseAsset> {
    let source = entry.source.as_ref().ok_or_else(|| {
        anyhow::anyhow!("catalog entry {} is missing a release source", entry.name)
    })?;

    let binaries = match source {
        CatalogInstallSource::GithubRelease { binaries, .. } => binaries,
    };

    let matches: Vec<_> = binaries
        .iter()
        .filter(|binary| binary.target == target)
        .collect();
    if matches.len() != 1 {
        bail!(
            "catalog entry {} must declare exactly one binary for target {}",
            entry.name,
            target
        );
    }

    let binary = matches[0];
    Ok(ReleaseAsset {
        name: entry.name.clone(),
        binary_name: binary.binary_name.clone(),
        asset: binary.asset.clone(),
    })
}

fn release_asset_for_extra(name: &str, target: &str) -> ReleaseAsset {
    let suffix = if target.ends_with("-windows-msvc") {
        ".exe"
    } else {
        ""
    };
    ReleaseAsset {
        name: name.to_string(),
        binary_name: format!("{name}{suffix}"),
        asset: format!("{name}-{target}{suffix}"),
    }
}

fn register_release_asset(
    asset: &ReleaseAsset,
    seen_assets: &mut std::collections::BTreeSet<String>,
    seen_binaries: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    if !seen_assets.insert(asset.asset.clone()) {
        bail!("duplicate release asset declared: {}", asset.asset);
    }
    if !seen_binaries.insert(asset.binary_name.clone()) {
        bail!("duplicate binary name declared: {}", asset.binary_name);
    }
    Ok(())
}

fn stage_release_assets(
    catalog: &ExtensionCatalog,
    config: &StageReleaseAssetsConfig,
) -> Result<()> {
    let assets = release_assets(catalog, &config.assets)?;
    fs::create_dir_all(&config.dist_dir)
        .with_context(|| format!("failed to create {}", config.dist_dir.display()))?;

    for asset in assets {
        let source = config.release_dir.join(&asset.binary_name);
        let destination = config.dist_dir.join(&asset.asset);
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "failed to copy release asset {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        mark_executable_if_supported(&destination)?;
    }

    Ok(())
}

#[cfg(unix)]
fn mark_executable_if_supported(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to set executable permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn mark_executable_if_supported(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use channel_schema::{
        CatalogAuthSummary, CatalogInstallSource, ExtensionManifest, ExtensionRequirements,
        GithubReleaseBinary,
    };

    fn sample_entry() -> CatalogEntry {
        CatalogEntry {
            id: "seren.channel.slack".to_string(),
            name: "channel-slack".to_string(),
            display_name: "Slack Channel".to_string(),
            kind: ManifestKind::Channel,
            version: "0.1.0".to_string(),
            description: "Slack messaging bridge".to_string(),
            protocol: "jsonl".to_string(),
            protocol_version: 1,
            source_dir: "channels/slack".to_string(),
            manifest_path: "channels/slack/channel-plugin.json".to_string(),
            manifest_url: Some(
                "https://raw.githubusercontent.com/serenorg/dispatch-plugins/v0.1.0/channels/slack/channel-plugin.json".to_string(),
            ),
            keywords: vec!["slack".to_string(), "workspace".to_string()],
            tags: vec!["messaging".to_string(), "webhook".to_string()],
            install_hint: Some("dispatch extension install channel-slack".to_string()),
            source: Some(CatalogInstallSource::GithubRelease {
                repo: "serenorg/dispatch-plugins".to_string(),
                tag: "v0.1.0".to_string(),
                base_url: None,
                checksum_asset: Some("SHA256SUMS.txt".to_string()),
                binaries: vec![GithubReleaseBinary {
                    target: "x86_64-unknown-linux-gnu".to_string(),
                    asset: "channel-slack-x86_64-unknown-linux-gnu".to_string(),
                    sha256: None,
                    binary_name: "channel-slack".to_string(),
                }],
            }),
            auth: None,
            artifact: None,
            requirements: ExtensionRequirements::default(),
        }
    }

    #[test]
    fn parse_kind_accepts_supported_values() {
        assert_eq!(
            parse_kind("channel").expect("channel kind"),
            ManifestKind::Channel
        );
        assert_eq!(
            parse_kind("courier").expect("courier kind"),
            ManifestKind::Courier
        );
        assert_eq!(
            parse_kind("connector").expect("connector kind"),
            ManifestKind::Connector
        );
    }

    #[test]
    fn matches_query_uses_name_description_keywords_and_tags() {
        let entry = sample_entry();

        assert!(matches_query(&entry, "slack"));
        assert!(matches_query(&entry, "bridge"));
        assert!(matches_query(&entry, "workspace"));
        assert!(matches_query(&entry, "webhook"));
        assert!(!matches_query(&entry, "telegram"));
    }

    #[test]
    fn find_entry_returns_matching_extension() {
        let entry = sample_entry();
        let catalog = ExtensionCatalog {
            schema: None,
            catalog_id: "seren".to_string(),
            repository: "serenorg/dispatch-plugins".to_string(),
            generated_at: None,
            entries: vec![entry.clone()],
        };

        let found = find_entry(&catalog, "channel-slack").expect("find entry");

        assert_eq!(found, &entry);
    }

    #[test]
    fn load_catalog_rejects_invalid_entry_id() {
        let root = unique_test_dir();
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("extensions.json");
        fs::write(
            &path,
            r#"{
                "catalog_id": "seren",
                "repository": "serenorg/dispatch-plugins",
                "entries": [
                    {
                        "id": "Seren/channel.slack",
                        "name": "channel-slack",
                        "display_name": "Slack Channel",
                        "kind": "channel",
                        "version": "0.1.0",
                        "description": "Slack messaging bridge",
                        "protocol": "jsonl",
                        "protocol_version": 1,
                        "source_dir": "channels/slack",
                        "manifest_path": "channels/slack/channel-plugin.json"
                    }
                ]
            }"#,
        )
        .expect("write catalog");

        let error = load_catalog(&path).expect_err("invalid id should fail");
        let chain = error
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(error.to_string().contains("invalid identity metadata"));
        assert!(chain.contains("catalog entry `channel-slack`"));
        fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn load_catalog_rejects_entry_id_from_another_catalog() {
        let root = unique_test_dir();
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("extensions.json");
        fs::write(
            &path,
            r#"{
                "catalog_id": "seren",
                "repository": "serenorg/dispatch-plugins",
                "entries": [
                    {
                        "id": "other.channel.slack",
                        "name": "channel-slack",
                        "display_name": "Slack Channel",
                        "kind": "channel",
                        "version": "0.1.0",
                        "description": "Slack messaging bridge",
                        "protocol": "jsonl",
                        "protocol_version": 1,
                        "source_dir": "channels/slack",
                        "manifest_path": "channels/slack/channel-plugin.json"
                    }
                ]
            }"#,
        )
        .expect("write catalog");

        let error = load_catalog(&path).expect_err("wrong catalog prefix should fail");

        assert!(error.to_string().contains("invalid identity metadata"));
        assert!(
            error
                .chain()
                .map(ToString::to_string)
                .any(|message| message.contains("must start with catalog_id prefix `seren.`"))
        );
        fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn load_catalog_rejects_duplicate_entry_ids() {
        let root = unique_test_dir();
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("extensions.json");
        fs::write(
            &path,
            r#"{
                "catalog_id": "seren",
                "repository": "serenorg/dispatch-plugins",
                "entries": [
                    {
                        "id": "seren.channel.slack",
                        "name": "channel-slack",
                        "display_name": "Slack Channel",
                        "kind": "channel",
                        "version": "0.1.0",
                        "description": "Slack messaging bridge",
                        "protocol": "jsonl",
                        "protocol_version": 1,
                        "source_dir": "channels/slack",
                        "manifest_path": "channels/slack/channel-plugin.json"
                    },
                    {
                        "id": "seren.channel.slack",
                        "name": "channel-slack-copy",
                        "display_name": "Slack Copy Channel",
                        "kind": "channel",
                        "version": "0.1.0",
                        "description": "Slack messaging bridge",
                        "protocol": "jsonl",
                        "protocol_version": 1,
                        "source_dir": "channels/slack",
                        "manifest_path": "channels/slack/channel-plugin.json"
                    }
                ]
            }"#,
        )
        .expect("write catalog");

        let error = load_catalog(&path).expect_err("duplicate id should fail");

        assert!(
            error
                .chain()
                .map(ToString::to_string)
                .any(|message| message.contains("duplicate catalog entry id"))
        );
        fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn catalog_entries_reference_existing_paths_and_expected_install_hints() {
        let catalog = load_repo_catalog();
        let repo_root = repo_root();

        for entry in &catalog.entries {
            let source_dir = repo_root.join(&entry.source_dir);
            assert!(
                source_dir.is_dir(),
                "source_dir does not exist for {}: {}",
                entry.name,
                source_dir.display()
            );

            let manifest_path = repo_root.join(&entry.manifest_path);
            assert!(
                manifest_path.is_file(),
                "manifest_path does not exist for {}: {}",
                entry.name,
                manifest_path.display()
            );

            assert_eq!(
                entry.manifest_path,
                format!("{}/channel-plugin.json", entry.source_dir)
            );
            let expected_install_hint = Some(format!("dispatch extension install {}", entry.name));
            assert_eq!(entry.install_hint, expected_install_hint);
        }
    }

    #[test]
    fn catalog_entries_match_manifest_metadata() {
        let catalog = load_repo_catalog();

        for entry in &catalog.entries {
            let manifest = load_manifest_for_entry(entry);

            assert_eq!(
                entry.kind, manifest.kind,
                "kind mismatch for {}",
                entry.name
            );
            assert_eq!(
                entry.name, manifest.name,
                "name mismatch for {}",
                entry.name
            );
            assert_eq!(
                entry.version, manifest.version,
                "version mismatch for {}",
                entry.name
            );
            assert_eq!(
                entry.description, manifest.description,
                "description mismatch for {}",
                entry.name
            );
            assert_eq!(
                entry.protocol, manifest.protocol,
                "protocol mismatch for {}",
                entry.name
            );
            assert_eq!(
                entry.protocol_version, manifest.protocol_version,
                "protocol_version mismatch for {}",
                entry.name
            );
            assert_eq!(
                entry.requirements, manifest.requirements,
                "requirements mismatch for {}",
                entry.name
            );

            let expected_auth = manifest.auth.map(|auth| CatalogAuthSummary {
                method: auth.mode,
                provider: auth.provider,
                setup_url: auth.setup_url,
            });
            assert_eq!(
                entry.auth, expected_auth,
                "auth mismatch for {}",
                entry.name
            );
        }
    }

    #[test]
    fn release_assets_emit_catalog_binaries_for_linux_and_extras() {
        let catalog = ExtensionCatalog {
            schema: None,
            catalog_id: "seren".to_string(),
            repository: "serenorg/dispatch-plugins".to_string(),
            generated_at: None,
            entries: vec![sample_entry()],
        };

        let assets = release_assets(
            &catalog,
            &ReleaseAssetsConfig {
                target: "x86_64-unknown-linux-gnu".to_string(),
                extras: vec!["dispatch-extension-catalog".to_string()],
            },
        )
        .expect("release assets");

        assert_eq!(
            assets,
            vec![
                ReleaseAsset {
                    name: "channel-slack".to_string(),
                    binary_name: "channel-slack".to_string(),
                    asset: "channel-slack-x86_64-unknown-linux-gnu".to_string(),
                },
                ReleaseAsset {
                    name: "dispatch-extension-catalog".to_string(),
                    binary_name: "dispatch-extension-catalog".to_string(),
                    asset: "dispatch-extension-catalog-x86_64-unknown-linux-gnu".to_string(),
                }
            ]
        );
    }

    #[test]
    fn release_assets_append_windows_extension_for_extras() {
        let asset = release_asset_for_extra("dispatch-extension-catalog", "x86_64-pc-windows-msvc");
        assert_eq!(
            asset,
            ReleaseAsset {
                name: "dispatch-extension-catalog".to_string(),
                binary_name: "dispatch-extension-catalog.exe".to_string(),
                asset: "dispatch-extension-catalog-x86_64-pc-windows-msvc.exe".to_string(),
            }
        );
    }

    #[test]
    fn release_assets_require_target_match() {
        let catalog = ExtensionCatalog {
            schema: None,
            catalog_id: "seren".to_string(),
            repository: "serenorg/dispatch-plugins".to_string(),
            generated_at: None,
            entries: vec![sample_entry()],
        };

        let err = release_assets(
            &catalog,
            &ReleaseAssetsConfig {
                target: "aarch64-apple-darwin".to_string(),
                extras: Vec::new(),
            },
        )
        .expect_err("missing target should fail");

        assert!(
            err.to_string()
                .contains("must declare exactly one binary for target aarch64-apple-darwin")
        );
    }

    #[test]
    fn stage_release_assets_copies_catalog_and_extra_binaries() {
        let root = unique_test_dir();
        let release_dir = root.join("release");
        let dist_dir = root.join("dist");
        fs::create_dir_all(&release_dir).expect("create release dir");

        fs::write(release_dir.join("channel-slack"), "slack").expect("write slack binary");
        fs::write(
            release_dir.join("dispatch-extension-catalog"),
            "catalog helper",
        )
        .expect("write helper binary");

        let catalog = ExtensionCatalog {
            schema: None,
            catalog_id: "seren".to_string(),
            repository: "serenorg/dispatch-plugins".to_string(),
            generated_at: None,
            entries: vec![sample_entry()],
        };

        stage_release_assets(
            &catalog,
            &StageReleaseAssetsConfig {
                assets: ReleaseAssetsConfig {
                    target: "x86_64-unknown-linux-gnu".to_string(),
                    extras: vec!["dispatch-extension-catalog".to_string()],
                },
                release_dir: release_dir.clone(),
                dist_dir: dist_dir.clone(),
            },
        )
        .expect("stage release assets");

        assert_eq!(
            fs::read_to_string(dist_dir.join("channel-slack-x86_64-unknown-linux-gnu"))
                .expect("read copied slack asset"),
            "slack"
        );
        assert_eq!(
            fs::read_to_string(
                dist_dir.join("dispatch-extension-catalog-x86_64-unknown-linux-gnu")
            )
            .expect("read copied helper asset"),
            "catalog helper"
        );

        fs::remove_dir_all(root).expect("remove temp test dir");
    }

    fn unique_test_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dispatch-extension-catalog-test-{}-{nanos}",
            std::process::id()
        ))
    }

    fn load_repo_catalog() -> ExtensionCatalog {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions.json");
        load_catalog(&path).expect("load repo catalog")
    }

    fn load_manifest_for_entry(entry: &CatalogEntry) -> ExtensionManifest {
        let path = repo_root().join(&entry.manifest_path);
        let body = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        serde_json::from_str(&body)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("catalog crate has repo root parent")
            .to_path_buf()
    }
}
