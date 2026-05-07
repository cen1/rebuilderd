use crate::api::v1::BuildStatus;
use serde::{Deserialize, Serialize};

/// Sent by `rebuildctl peers check` to register peer rebuilders and trigger a
/// background bulk cross-check on the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckPeersRequest {
    pub distribution: String,
    pub architecture: String,
    /// Local release name (e.g. `"sid"`). `None` = no release filter (Arch Linux).
    #[serde(default)]
    pub release: Option<String>,
    /// Release name the peer uses for this suite (e.g. `"unstable"` when local is `"sid"`).
    /// `None` or missing = same as `release`.
    #[serde(default)]
    pub release_alias: Option<String>,
    /// Full versioned API base URLs of peer rebuilders, e.g.
    /// `https://reproducible.archlinux.org/api/v1`
    pub urls: Vec<String>,
}

/// Status of a specific package as reported by a single peer rebuilder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatusEntry {
    /// Base URL of the peer rebuilder.
    pub url: String,
    /// The peer's status for this package (`GOOD`/`BAD`), or `None` if the
    /// peer has not built the package.
    pub status: Option<BuildStatus>,
    /// The peer's build ID for this package.
    pub build_id: Option<i32>,
    /// Direct URL to the peer's build log (API endpoint).
    pub log_url: Option<String>,
    /// Direct URL to the peer's diffoscope output, if the peer reported BAD
    /// and has diffoscope available.
    pub diffoscope_url: Option<String>,
}
