mod catalog;
mod manifest;

pub use catalog::{
    CatalogArtifactRef, CatalogAuthSummary, CatalogEntry, CatalogInstallSource, ExtensionCatalog,
    GithubReleaseBinary,
};
pub use manifest::{
    BootstrapCredential, BootstrapGenerator, BootstrapManifest, ChannelCapabilityManifest,
    ChannelDeliveryManifest, ChannelIngressManifest, ExtensionCapabilitiesManifest,
    ExtensionEntrypoint, ExtensionManifest, ExtensionRequirements, IngressEndpointManifest,
    IngressTrustManifest, ManifestKind, PollingManifest, ProviderAuthManifest,
};
