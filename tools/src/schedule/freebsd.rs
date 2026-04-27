use crate::args::PkgsSync;
use crate::schedule::{fetch_url_or_path, Pkg};
use rebuilderd_common::api::v1::{BinaryPackageReport, PackageReport, SourcePackageReport};
use rebuilderd_common::errors::*;
use rebuilderd_common::http;
use serde::Deserialize;
use serde_json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use tar;
use zstd::stream::read::Decoder as ZstdDecoder;

#[derive(Debug, Clone, serde::Deserialize)]
struct FreeBSDPkgAnnotations {
    ports_top_git_hash: Option<String>,
    build_timestamp: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct FreeBSDPkg {
    name: String,
    origin: String,
    version: String,
    arch: String,
    #[serde(default)]
    annotations: Option<FreeBSDPkgAnnotations>,
}

#[derive(Debug, Deserialize)]
struct GitHubCommit {
    commit: GitHubCommitDetails,
}

#[derive(Debug, Deserialize)]
struct GitHubCommitDetails {
    committer: GitHubCommitter,
}

#[derive(Debug, Deserialize)]
struct GitHubCommitter {
    date: String,
}

impl Pkg for FreeBSDPkg {
    fn pkg_name(&self) -> &str {
        &self.name
    }

    fn by_maintainer(&self, _maintainers: &[String]) -> bool {
        // packagesite.yaml does not contain maintainer info.
        false
    }
}

#[derive(Debug, Clone)]
struct PackageInfo {
    pkg: FreeBSDPkg,
    pkg_url: String,
    build_timestamp: Option<chrono::NaiveDateTime>,
    git_hash: Option<String>,
}

/// Fetch commit timestamp from GitHub API for a given commit hash
/// Returns None if the API call fails or if no token is provided
async fn fetch_github_commit_timestamp(
    http: &http::Client,
    git_hash: &str,
    github_token: Option<&str>,
) -> Option<chrono::NaiveDateTime> {
    let token = github_token?;

    let url = format!(
        "https://api.github.com/repos/freebsd/freebsd-ports/commits/{}",
        git_hash
    );

    let mut request = http.get(&url);
    request = request.header("Authorization", format!("Bearer {}", token));
    request = request.header("User-Agent", "rebuilderd");
    request = request.header("Accept", "application/vnd.github+json");
    request = request.header("X-GitHub-Api-Version", "2022-11-28");

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to fetch GitHub commit {}: {}", git_hash, e);
            return None;
        }
    };

    if !response.status().is_success() {
        warn!(
            "GitHub API returned status {} for commit {}",
            response.status(),
            git_hash
        );
        return None;
    }

    let commit: GitHubCommit = match response.json().await {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to parse GitHub response for commit {}: {}", git_hash, e);
            return None;
        }
    };

    // Parse the ISO 8601 timestamp from GitHub
    match chrono::DateTime::parse_from_rfc3339(&commit.commit.committer.date) {
        Ok(dt) => Some(dt.naive_utc()),
        Err(e) => {
            warn!(
                "Failed to parse commit timestamp for {}: {}",
                git_hash, e
            );
            None
        }
    }
}

pub async fn sync(http: &http::Client, sync: &PkgsSync) -> Result<Vec<PackageReport>> {
    let mut reports = Vec::new();

    for release in &sync.releases {
        for arch in &sync.architectures {
            let url = format!(
                "{}/FreeBSD:{}:{}/latest/packagesite.pkg",
                sync.source, release, arch
            );

            let bytes = match fetch_url_or_path(http, &url).await {
                Ok(b) => {
                    info!("Successfully fetched {}", url);
                    b
                }
                Err(e) => {
                    warn!("Failed to fetch {}: {}. Skipping this release/arch.", url, e);
                    continue;
                }
            };

            let tar = ZstdDecoder::new(&bytes[..])?;
            let mut archive = tar::Archive::new(tar);

            let mut packagesite_json = Vec::new();
            let mut found_packagesite = false;
            for entry in archive.entries()? {
                let mut entry = entry?;
                if entry.path()?.to_str().unwrap_or_default() == "packagesite.yaml" {
                    entry.read_to_end(&mut packagesite_json)?;
                    found_packagesite = true;
                    break;
                }
            }

            if !found_packagesite {
                warn!("Could not find packagesite.yaml in {} for {}/{}. Skipping.", url, release, arch);
                continue;
            }

            let reader = BufReader::new(&packagesite_json[..]);
            let mut package_infos = Vec::new();

            // First pass: collect all packages with their metadata
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }

                let pkg: FreeBSDPkg = match serde_json::from_str(&line) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Failed to parse package line, skipping: {}. Error: {}", line, e);
                        continue;
                    }
                };

                if !pkg.matches(sync) {
                    continue;
                }

                let pkg_url = format!(
                    "{}/FreeBSD:{}:{}/latest/All/{}-{}.pkg",
                    sync.source, release, arch, pkg.name, pkg.version
                );

                // Parse build timestamp if available
                // FreeBSD uses format "2025-10-24T19:14:30+0000" (without colon in timezone)
                let build_timestamp = pkg.annotations.as_ref()
                    .and_then(|a| a.build_timestamp.as_ref())
                    .and_then(|ts| chrono::DateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%z").ok())
                    .map(|dt| dt.naive_utc());

                let git_hash = pkg.annotations.as_ref()
                    .and_then(|a| a.ports_top_git_hash.clone());

                package_infos.push(PackageInfo {
                    pkg,
                    pkg_url,
                    build_timestamp,
                    git_hash,
                });
            }

            // Calculate commit timestamps
            let mut commit_timestamps: HashMap<String, chrono::NaiveDateTime> = HashMap::new();

            // Log statistics about git hash coverage
            let packages_with_hash = package_infos.iter().filter(|info| info.git_hash.is_some()).count();
            let packages_without_hash = package_infos.len() - packages_with_hash;
            info!("Package git hash coverage: {}/{} packages have git hashes, {} missing",
                  packages_with_hash, package_infos.len(), packages_without_hash);

            // First, try to fetch actual commit timestamps from GitHub API
            if sync.github_token.is_some() {
                // Collect unique git hashes
                let unique_hashes: std::collections::HashSet<String> = package_infos
                    .iter()
                    .filter_map(|info| info.git_hash.clone())
                    .collect();

                info!("Fetching commit timestamps from GitHub for {} unique commits", unique_hashes.len());

                for git_hash in unique_hashes {
                    if let Some(timestamp) = fetch_github_commit_timestamp(http, &git_hash, sync.github_token.as_deref()).await {
                        commit_timestamps.insert(git_hash.clone(), timestamp);
                        debug!("Fetched GitHub timestamp for commit {}: {}", git_hash, timestamp);
                    }
                }

                info!("Successfully fetched {} commit timestamps from GitHub", commit_timestamps.len());
            }

            // Fall back to approximation for any commits we couldn't fetch from GitHub
            // Use the earliest build timestamp for each git hash
            for info in &package_infos {
                if let (Some(git_hash), Some(build_ts)) = (&info.git_hash, info.build_timestamp) {
                    commit_timestamps.entry(git_hash.clone())
                        .and_modify(|ts| {
                            // Only update if this build timestamp is earlier (shouldn't happen with GitHub data)
                            if build_ts < *ts {
                                *ts = build_ts;
                            }
                        })
                        .or_insert(build_ts);
                }
            }

            // Second pass: create reports with calculated commit timestamps
            let mut source_packages = Vec::new();
            for info in package_infos {
                let fbsd_ports_top_git_timestamp = info.git_hash.as_ref()
                    .and_then(|hash| commit_timestamps.get(hash).copied());

                let source_report = SourcePackageReport {
                    name: info.pkg.origin.clone(),
                    version: info.pkg.version.clone(),
                    url: info.pkg_url.clone(),
                    artifacts: vec![BinaryPackageReport {
                        name: info.pkg.name.clone(),
                        version: info.pkg.version.clone(),
                        architecture: info.pkg.arch.clone(),
                        url: info.pkg_url,
                    }],
                    fbsd_ports_top_git_hash: info.git_hash,
                    fbsd_build_timestamp: info.build_timestamp,
                    fbsd_ports_top_git_timestamp,
                };
                source_packages.push(source_report);
            }

            reports.push(PackageReport {
                distribution: "freebsd".to_string(),
                release: Some(release.clone()),
                component: None,
                architecture: arch.clone(),
                packages: source_packages,
            });
        }
    }

    Ok(reports)
}