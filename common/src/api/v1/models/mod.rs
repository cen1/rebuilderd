mod build;
mod dashboard;
mod meta;
mod package;
mod peers;
mod queue;
mod stats;
mod worker;

pub use build::*;
pub use dashboard::*;
pub use meta::*;
pub use package::*;
pub use peers::*;
pub use queue::*;
use serde::{Deserialize, Serialize};
pub use stats::*;
pub use worker::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub limit: Option<i32>,
    pub before: Option<i32>,
    pub after: Option<i32>,
    pub sort: Option<String>,
    pub direction: Option<SortDirection>,
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResultPage<T> {
    pub total: i64,
    pub records: Vec<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginFilter {
    pub distribution: Option<String>,
    pub release: Option<String>,
    pub component: Option<String>,
    pub architecture: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceIdentityFilter {
    pub name: Option<String>,
    #[serde(default)]
    pub search_type: SearchType,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BinaryIdentityFilter {
    pub name: Option<String>,
    #[serde(default)]
    pub search_type: SearchType,
    pub version: Option<String>,
    pub source_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchType {
    Exact,
    Contains,
    StartsWith,
}

impl Default for SearchType {
    fn default() -> Self {
        Self::Exact
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreshnessFilter {
    pub seen_only: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusFilter {
    #[serde(default, deserialize_with = "deserialize_comma_separated")]
    pub status: Option<Vec<String>>,
}

/// Filter binary packages by whether they have a peer-disagreement flag set.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DisagreementFilter {
    /// Any disagreement with a peer: status mismatch OR sha256 mismatch.
    pub has_disagreement: Option<bool>,
    /// Restrict disagreement checks to peers whose URL contains any of these substrings.
    /// Comma-delimited: `peer=a,b`.
    #[serde(default, deserialize_with = "deserialize_peer_filter")]
    pub peer: Vec<String>,
}

fn deserialize_peer_filter<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    Ok(s.map(|s| {
        s.split(',')
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect()
    })
    .unwrap_or_default())
}

fn deserialize_comma_separated<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    Ok(s.map(|s| {
        s.split(',')
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect()
    }))
}
