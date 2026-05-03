use rebuilderd_common::errors::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct SyncConfigFile {
    #[serde(rename = "profile")]
    pub profiles: HashMap<String, SyncProfile>,
}

impl SyncConfigFile {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<SyncConfigFile> {
        let buf = fs::read_to_string(path).context("Failed to read config file")?;
        let config = toml::from_str(&buf).context("Failed to load config")?;
        Ok(config)
    }
}

#[derive(Debug, Deserialize)]
pub struct SyncProfile {
    pub distro: String,

    pub sync_method: Option<String>,

    #[deprecated]
    pub suite: Option<String>,

    #[serde(default)]
    pub components: Vec<String>,

    #[serde(default)]
    pub releases: Vec<String>,

    #[deprecated]
    pub architecture: Option<String>,

    #[serde(default)]
    pub architectures: Vec<String>,

    pub source: String,

    #[serde(default)]
    pub maintainers: Vec<String>,

    #[serde(default)]
    pub pkgs: Vec<String>,

    #[serde(default)]
    pub excludes: Vec<String>,

    pub github_token: Option<String>,

    /// Full versioned API base URLs of peer rebuilder instances to cross-check
    /// against, e.g. `["https://reproducible.archlinux.org/api/v1"]`.
    #[serde(default)]
    pub peer_rebuilders: Vec<String>,

    /// Per-position release name aliases for peer queries, parallel to `releases`.
    /// `peer_release_aliases[i]` replaces `releases[i]` when querying peers.
    /// Use an empty string at a position to keep the original release name.
    /// Shorter than `releases` → remaining positions use original names.
    /// E.g. `releases = ["sid"]`, `peer_release_aliases = ["unstable"]`.
    #[serde(default)]
    pub peer_release_aliases: Vec<String>,
}
