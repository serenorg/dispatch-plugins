use serde::{Deserialize, Serialize};

use crate::{ExtensionRequirements, ManifestKind};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionCatalog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub entries: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogEntry {
    pub name: String,
    pub display_name: String,
    pub kind: ManifestKind,
    pub version: String,
    pub description: String,
    pub protocol: String,
    pub protocol_version: u32,
    pub source_dir: String,
    pub manifest_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_url: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<CatalogInstallSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<CatalogAuthSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<CatalogArtifactRef>,
    #[serde(default)]
    pub requirements: ExtensionRequirements,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CatalogInstallSource {
    GithubRelease {
        repo: String,
        tag: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checksum_asset: Option<String>,
        binaries: Vec<GithubReleaseBinary>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GithubReleaseBinary {
    pub target: String,
    pub asset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub binary_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogAuthSummary {
    pub method: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogArtifactRef {
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}
