use crate::api::v1::util::auth;
use crate::config::Config;
use crate::db::Pool;
use crate::models::{NewPeerRebuilder, PeerRebuilder, PeerSha256Check, UpsertPeerSha256Check};
use crate::schema::{
    binary_packages, build_inputs, peer_rebuilders, peer_sha256_checks, rebuild_artifacts,
    rebuilds, source_packages,
};
use crate::web;
use actix_web::{HttpRequest, HttpResponse, Responder, get, post};
use diesel::prelude::*;
use diesel::upsert::excluded;
use rebuilderd_common::api::v1::{BuildStatus, CheckPeersRequest, PeerStatusEntry};
use rebuilderd_common::errors::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

mod aliases {
    diesel::alias!(
        crate::schema::rebuilds as r1: PeersRebuildsAlias1,
        crate::schema::rebuilds as r2: PeersRebuildsAlias2
    );
}
use aliases::*;

// ---------------------------------------------------------------------------
// Internal types for bulk peer check
// ---------------------------------------------------------------------------

struct LocalPkgInfo {
    status: BuildStatus,
}

struct PeerPkgInfo {
    status: BuildStatus,
    peer_build_id: Option<i32>,
}

// ---------------------------------------------------------------------------
// POST /api/v1/peers/check
// ---------------------------------------------------------------------------

#[post("/check")]
pub async fn check_peers(
    req: HttpRequest,
    cfg: web::Data<Config>,
    pool: web::Data<Pool>,
    body: web::Json<CheckPeersRequest>,
) -> web::Result<impl Responder> {
    if auth::admin(&cfg, &req).is_err() {
        return Ok(HttpResponse::Forbidden().finish());
    }

    let mut connection = pool.get().map_err(Error::from)?;
    let request = body.into_inner();

    log::info!(
        "check_peers: distribution={}, architecture={}, release={:?}, urls={:?}",
        request.distribution,
        request.architecture,
        request.release,
        request.urls,
    );

    // Normalize: None → "" (no release filter, e.g. Arch Linux).
    let effective_release = request.release.clone().unwrap_or_default();
    // release_alias is what the peer calls this release (e.g. "unstable" when local = "sid").
    // Falls back to effective_release when not specified or empty.
    let effective_alias = request
        .release_alias
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&effective_release)
        .to_owned();

    // Upsert peer_rebuilders: unique key is (url, dist, arch) — one row per peer per
    // distro/arch regardless of release alias differences.  Update release/release_alias
    // on conflict so that re-running with corrected aliases takes effect.
    for url in &request.urls {
        let new_peer = NewPeerRebuilder {
            url: url.clone(),
            distribution: request.distribution.clone(),
            architecture: request.architecture.clone(),
            release: effective_release.clone(),
            release_alias: effective_alias.clone(),
        };
        diesel::insert_into(peer_rebuilders::table)
            .values(&new_peer)
            .on_conflict((
                peer_rebuilders::url,
                peer_rebuilders::distribution,
                peer_rebuilders::architecture,
            ))
            .do_update()
            .set((
                peer_rebuilders::release.eq(excluded(peer_rebuilders::release)),
                peer_rebuilders::release_alias.eq(excluded(peer_rebuilders::release_alias)),
            ))
            .execute(connection.as_mut())
            .map_err(Error::from)?;
    }
    // Delete stale peer URLs for this (dist, arch) that are no longer configured.
    diesel::delete(peer_rebuilders::table)
        .filter(peer_rebuilders::distribution.eq(&request.distribution))
        .filter(peer_rebuilders::architecture.eq(&request.architecture))
        .filter(peer_rebuilders::url.ne_all(&request.urls))
        .execute(connection.as_mut())
        .map_err(Error::from)?;
    drop(connection);

    // Spawn a background task to perform the bulk cross-check.
    let pool = pool.into_inner();
    let distribution = request.distribution.clone();
    let architecture = request.architecture.clone();
    tokio::spawn(async move {
        if let Err(e) = run_peer_check(pool, distribution, architecture).await {
            log::error!("Peer cross-check failed: {e:#}");
        }
    });

    Ok(HttpResponse::Accepted().finish())
}

// ---------------------------------------------------------------------------
// GET /api/v1/peers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PeerListQuery {
    pub distribution: Option<String>,
    pub architecture: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PeerInfo {
    pub id: i32,
    pub url: String,
    pub distribution: String,
    pub architecture: String,
    pub release: String,
}

#[get("")]
pub async fn list_peers(
    pool: web::Data<Pool>,
    query: web::Query<PeerListQuery>,
) -> web::Result<impl Responder> {
    let mut connection = pool.get().map_err(Error::from)?;
    let mut q = peer_rebuilders::table.into_boxed();
    if let Some(ref dist) = query.distribution {
        q = q.filter(peer_rebuilders::distribution.eq(dist));
    }
    if let Some(ref arch) = query.architecture {
        q = q.filter(peer_rebuilders::architecture.eq(arch));
    }
    let rows = q
        .get_results::<PeerRebuilder>(connection.as_mut())
        .map_err(Error::from)?;
    let peers: Vec<PeerInfo> = rows
        .into_iter()
        .map(|p| PeerInfo {
            id: p.id,
            url: p.url.replace("{arch}", &p.architecture),
            distribution: p.distribution,
            architecture: p.architecture,
            release: p.release,
        })
        .collect();
    Ok(HttpResponse::Ok().json(peers))
}

// ---------------------------------------------------------------------------
// GET /api/v1/peers/package
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PeerPackageQuery {
    pub distribution: String,
    pub architecture: String,
    pub name: String,
    pub version: String,
    /// `true` (default): proxy requests to peers in real time.
    /// `false`: return cached results from the `peer_sha256_checks` table
    /// populated by `rebuildctl peers check`.
    #[serde(default = "default_true")]
    pub live: bool,
    /// If set, only return results for the peer whose stored URL contains this
    /// substring (e.g. `?peer=reproducible.archlinux.org`).
    pub peer: Option<String>,
}

fn default_true() -> bool {
    true
}

#[get("/package")]
pub async fn get_peer_package(
    pool: web::Data<Pool>,
    query: web::Query<PeerPackageQuery>,
) -> web::Result<impl Responder> {
    let mut connection = pool.get().map_err(Error::from)?;

    let peers: Vec<PeerRebuilder> = peer_rebuilders::table
        .filter(peer_rebuilders::distribution.eq(&query.distribution))
        .filter(peer_rebuilders::architecture.eq(&query.architecture))
        .get_results::<PeerRebuilder>(connection.as_mut())
        .map_err(Error::from)?;

    // Filter to a specific peer if requested.
    let peers: Vec<PeerRebuilder> = if let Some(ref filter) = query.peer {
        peers.into_iter().filter(|p| p.url.contains(filter.as_str())).collect()
    } else {
        peers
    };

    if peers.is_empty() {
        log::info!(
            "No peer rebuilders registered for {}/{} — run `rebuildctl peers check` to register peers",
            query.distribution,
            query.architecture,
        );
        return Ok(HttpResponse::Ok().json(Vec::<PeerStatusEntry>::new()));
    }

    // Cached path: read from peer_sha256_checks without contacting peers.
    if !query.live {
        let entries =
            cached_peer_entries(connection.as_mut(), &peers, &query.name, &query.version)?;
        return Ok(HttpResponse::Ok().json(entries));
    }
    drop(connection);

    // Live path: proxy to each peer concurrently.
    let http_client = rebuilderd_common::http::client().map_err(Error::from)?;
    let name = query.name.clone();
    let version = query.version.clone();
    let architecture = query.architecture.clone();

    let mut join_set = tokio::task::JoinSet::new();
    for peer in peers {
        // Resolve {arch} template before making HTTP requests.
        let resolved_url = peer.url.replace("{arch}", &architecture);
        let client = http_client.clone();
        let n = name.clone();
        let v = version.clone();
        let a = architecture.clone();
        join_set.spawn(async move {
            let result = fetch_peer_package(&client, &resolved_url, &n, &v, &a).await;
            (resolved_url, result)
        });
    }

    let mut entries: Vec<PeerStatusEntry> = Vec::new();
    while let Some(task_result) = join_set.join_next().await {
        match task_result {
            Ok((_url, Ok(entry))) => entries.push(entry),
            Ok((url, Err(e))) => {
                log::warn!("Failed to fetch package from peer {url}: {e:#}");
                entries.push(PeerStatusEntry {
                    url,
                    status: None,
                    build_id: None,
                    log_url: None,
                    diffoscope_url: None,
                });
            }
            Err(e) => log::warn!("Peer package fetch task panicked: {e}"),
        }
    }

    Ok(HttpResponse::Ok().json(entries))
}

fn build_status_to_str(s: &BuildStatus) -> String {
    // Serialize via serde to get the canonical renamed string ("GOOD", "BAD", …),
    // then strip the surrounding JSON quotes.
    serde_json::to_string(s)
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned()
}

fn str_to_build_status(s: &str) -> Option<BuildStatus> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

/// Build a `PeerStatusEntry` per peer from the `peer_sha256_checks` cache table.
fn cached_peer_entries(
    conn: &mut diesel::SqliteConnection,
    peers: &[PeerRebuilder],
    name: &str,
    version: &str,
) -> Result<Vec<PeerStatusEntry>> {
    use diesel::OptionalExtension;
    let mut entries: Vec<PeerStatusEntry> = Vec::new();
    for peer in peers {
        let resolved_url = peer.url.replace("{arch}", &peer.architecture);
        let cached: Option<PeerSha256Check> = peer_sha256_checks::table
            .filter(peer_sha256_checks::peer_rebuilder_id.eq(peer.id))
            .filter(peer_sha256_checks::binary_name.eq(name))
            .filter(peer_sha256_checks::binary_version.eq(version))
            .first::<PeerSha256Check>(conn)
            .optional()
            .map_err(Error::from)?;
        let entry = match cached {
            Some(c) => {
                let status = c.peer_status.as_deref().and_then(str_to_build_status);
                let is_bad = status == Some(BuildStatus::Bad);
                let log_url = c.peer_build_id.map(|id| {
                    format!("{}/builds/{id}/log", resolved_url.trim_end_matches('/'))
                });
                let diffoscope_url = if is_bad {
                    c.peer_build_id.map(|id| {
                        format!("{}/builds/{id}/diffoscope", resolved_url.trim_end_matches('/'))
                    })
                } else {
                    None
                };
                PeerStatusEntry {
                    url: resolved_url,
                    status,
                    build_id: c.peer_build_id,
                    log_url,
                    diffoscope_url,
                }
            }
            None => PeerStatusEntry {
                url: resolved_url,
                status: None,
                build_id: None,
                log_url: None,
                diffoscope_url: None,
            },
        };
        entries.push(entry);
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Background bulk cross-check
// ---------------------------------------------------------------------------

async fn run_peer_check(
    pool: Arc<Pool>,
    distribution: String,
    architecture: String,
) -> Result<()> {
    let mut connection = pool.get().map_err(Error::from)?;

    let peers: Vec<PeerRebuilder> = peer_rebuilders::table
        .filter(peer_rebuilders::distribution.eq(&distribution))
        .filter(peer_rebuilders::architecture.eq(&architecture))
        .get_results::<PeerRebuilder>(connection.as_mut())
        .map_err(Error::from)?;
    drop(connection);

    if peers.is_empty() {
        return Ok(());
    }

    let http_client = rebuilderd_common::http::client()?;

    // Fetch packages from all peers concurrently.
    // Resolve `{arch}` template in the stored URL before making HTTP requests.
    let mut join_set = tokio::task::JoinSet::new();
    for peer in peers.clone() {
        let client = http_client.clone();
        let dist = distribution.clone();
        let arch = architecture.clone();
        let resolved_url = peer.url.replace("{arch}", &arch);
        // Use release_alias when querying the peer (e.g. "unstable" when local is "sid").
        // Empty string means "no release filter"; convert to Option for HTTP query.
        let rel = if peer.release_alias.is_empty() {
            None
        } else {
            Some(peer.release_alias.clone())
        };
        join_set.spawn(async move {
            let result = fetch_all_peer_packages(
                &client,
                &resolved_url,
                &dist,
                &arch,
                rel.as_deref(),
            )
            .await;
            (peer, resolved_url, result)
        });
    }

    // peer_maps: (peer, resolved_url, package_map)
    let mut peer_maps: Vec<(PeerRebuilder, String, HashMap<(String, String), PeerPkgInfo>)> =
        Vec::new();
    while let Some(task_result) = join_set.join_next().await {
        match task_result {
            Ok((peer, resolved_url, Ok(map))) => {
                log::info!("Peer check: fetched {} packages from {}", map.len(), resolved_url);
                peer_maps.push((peer, resolved_url, map));
            }
            Ok((peer, resolved_url, Err(e))) => {
                log::warn!(
                    "Peer check: failed to fetch packages from {resolved_url} (peer {}): {e:#}",
                    peer.url
                );
            }
            Err(e) => log::warn!("Peer check: fetch task panicked: {e}"),
        }
    }

    if peer_maps.is_empty() {
        return Ok(());
    }

    // Fetch local packages.
    let mut connection = pool.get().map_err(Error::from)?;
    let local_packages =
        fetch_local_packages(connection.as_mut(), &distribution, &architecture)?;
    drop(connection);

    // Collect upserts for status disagreements.
    let mut upserts: Vec<UpsertPeerSha256Check> = Vec::new();

    for (peer, resolved_url, peer_map) in &peer_maps {
        let total = peer_map.len();
        let mut checked = 0usize;
        for ((bin_name, bin_version), peer_info) in peer_map {
            checked += 1;
            if checked % 10_000 == 0 || checked == total {
                log::info!(
                    "Peer check [{resolved_url}]: compared {checked}/{total} packages ({distribution}/{architecture})",
                );
            }

            let Some(local_info) =
                local_packages.get(&(bin_name.clone(), bin_version.clone()))
            else {
                continue;
            };

            // Status disagreement (GOOD vs BAD or BAD vs GOOD).
            if matches!(
                (&peer_info.status, &local_info.status),
                (BuildStatus::Good, BuildStatus::Bad) | (BuildStatus::Bad, BuildStatus::Good)
            ) {
                upserts.push(UpsertPeerSha256Check {
                    peer_rebuilder_id: peer.id,
                    binary_name: bin_name.clone(),
                    binary_version: bin_version.clone(),
                    peer_build_id: peer_info.peer_build_id,
                    checked_at: chrono::Utc::now().naive_utc(),
                    peer_status: Some(build_status_to_str(&peer_info.status)),
                });
            }
        }
    }

    log::info!(
        "Peer check: {distribution}/{architecture} — {} status disagreements",
        upserts.len(),
    );

    // Replace disagreement results atomically: delete stale entries for all peers that were
    // successfully compared, then insert the current set.  This ensures that previously-cached
    // disagreements which are now resolved (both sides agree) are removed rather than
    // accumulated indefinitely.
    let compared_peer_ids: Vec<i32> = peer_maps.iter().map(|(peer, _, _)| peer.id).collect();
    let mut conn = pool.get().map_err(Error::from)?;
    conn.transaction(|conn| {
        diesel::delete(peer_sha256_checks::table)
            .filter(peer_sha256_checks::peer_rebuilder_id.eq_any(&compared_peer_ids))
            .execute(conn)?;
        for upsert in &upserts {
            diesel::insert_into(peer_sha256_checks::table)
                .values(upsert)
                .execute(conn)?;
        }
        Ok::<_, diesel::result::Error>(())
    })
    .map_err(Error::from)?;
    log::info!(
        "Peer check: cached {} disagreement results for {distribution}/{architecture}",
        upserts.len(),
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Local package status helper
// ---------------------------------------------------------------------------

/// Returns `(binary_name, version) → LocalPkgInfo` for all binary packages
/// currently seen in the given distribution/architecture.
fn fetch_local_packages(
    conn: &mut SqliteConnection,
    distribution: &str,
    architecture: &str,
) -> Result<HashMap<(String, String), LocalPkgInfo>> {
    let rows = binary_packages::table
        .inner_join(source_packages::table)
        .inner_join(build_inputs::table)
        .left_join(r1.on(r1.field(rebuilds::build_input_id).is(build_inputs::id)))
        .left_join(
            r2.on(r2
                .field(rebuilds::build_input_id)
                .is(build_inputs::id)
                .and(
                    r1.field(rebuilds::built_at)
                        .lt(r2.field(rebuilds::built_at))
                        .or(r1.fields(
                            rebuilds::built_at
                                .eq(r2.field(rebuilds::built_at))
                                .and(r1.field(rebuilds::id).lt(r2.field(rebuilds::id))),
                        )),
                )),
        )
        .left_join(
            rebuild_artifacts::table.on(rebuild_artifacts::rebuild_id
                .is(r1.field(rebuilds::id))
                .and(rebuild_artifacts::name.is(binary_packages::name))),
        )
        .filter(r2.field(rebuilds::id).is_null())
        .filter(source_packages::distribution.eq(distribution))
        .filter(build_inputs::architecture.eq(architecture))
        .filter(binary_packages::seen_in_last_sync.is(true))
        .select((
            binary_packages::name,
            binary_packages::version,
            rebuild_artifacts::status.nullable(),
            r1.field(rebuilds::status).assume_not_null().nullable(),
        ))
        .get_results::<(String, String, Option<BuildStatus>, Option<BuildStatus>)>(conn)
        .map_err(Error::from)?;

    let mut map = HashMap::with_capacity(rows.len());
    for (bin_name, bin_version, artifact_status, rebuild_status) in rows {
        // Artifact-level status (GOOD/BAD) takes priority; fall back to
        // build-level status (FAIL) if no artifact row.
        let effective = artifact_status.or(rebuild_status).unwrap_or(BuildStatus::Unknown);
        map.insert(
            (bin_name, bin_version),
            LocalPkgInfo { status: effective },
        );
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Peer API fetchers (v0 and v1)
// ---------------------------------------------------------------------------

fn v0_urls(
    peer_url: &str,
    build_id: i32,
    has_diffoscope: bool,
    is_bad: bool,
) -> (Option<String>, Option<String>) {
    let base = peer_url.trim_end_matches('/');
    let log_url = Some(format!("{base}/builds/{build_id}/log"));
    let diffoscope_url = if is_bad && has_diffoscope {
        Some(format!("{base}/builds/{build_id}/diffoscope"))
    } else {
        None
    };
    (log_url, diffoscope_url)
}

fn v1_urls(
    peer_url: &str,
    build_id: i32,
    artifact_id: Option<i32>,
    diffoscope_log_id: Option<i32>,
    is_bad: bool,
) -> (Option<String>, Option<String>) {
    let base = peer_url.trim_end_matches('/');
    let log_url = Some(format!("{base}/builds/{build_id}/log"));
    let diffoscope_url = if is_bad {
        artifact_id.zip(diffoscope_log_id).map(|(aid, _)| {
            format!("{base}/builds/{build_id}/artifacts/{aid}/diffoscope")
        })
    } else {
        None
    };
    (log_url, diffoscope_url)
}

fn is_v0_url(url: &str) -> bool {
    let trimmed = url.trim_end_matches('/');
    trimmed.ends_with("/v0") || trimmed.contains("/v0/")
}

async fn fetch_all_peer_packages(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    distribution: &str,
    architecture: &str,
    release: Option<&str>,
) -> Result<HashMap<(String, String), PeerPkgInfo>> {
    if is_v0_url(peer_url) {
        fetch_v0_packages(client, peer_url, architecture).await
    } else {
        fetch_v1_packages(client, peer_url, distribution, architecture, release).await
    }
}

// Minimal struct used to deserialize only the fields we need from a v1
// BinaryPackage response (avoids issues if the peer has extra/missing fields).
#[derive(Debug, Deserialize)]
struct PeerBinaryPackage {
    id: i32,
    name: String,
    version: String,
    #[serde(default)]
    status: Option<BuildStatus>,
    #[serde(default)]
    build_id: Option<i32>,
    #[serde(default)]
    artifact_id: Option<i32>,
    #[serde(default)]
    diffoscope_log_id: Option<i32>,
    #[serde(default)]
    rebuild_status: Option<BuildStatus>,
}

#[derive(Debug, Deserialize)]
struct PeerResultPage {
    total: Option<i64>,
    records: Vec<PeerBinaryPackage>,
}

#[derive(Debug, Serialize)]
struct V1PageQuery<'a> {
    distribution: &'a str,
    architecture: &'a str,
    seen_only: bool,
    limit: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release: Option<&'a str>,
}

async fn fetch_v1_packages(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    distribution: &str,
    architecture: &str,
    release: Option<&str>,
) -> Result<HashMap<(String, String), PeerPkgInfo>> {
    let base = format!("{}/packages/binary", peer_url.trim_end_matches('/'));
    let mut map: HashMap<(String, String), PeerPkgInfo> = HashMap::new();
    let mut after: Option<i32> = None;
    let mut total: Option<i64> = None;

    loop {
        let page = V1PageQuery {
            distribution,
            architecture,
            seen_only: true,
            limit: 1000,
            after,
            release,
        };

        let resp: PeerResultPage = client
            .get(&base)
            .query(&page)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if total.is_none() {
            total = resp.total;
        }

        let count = resp.records.len();
        let last_pkg_id = resp.records.last().map(|p| p.id);

        for pkg in resp.records {
            let effective = pkg.status.or(pkg.rebuild_status).unwrap_or(BuildStatus::Unknown);
            map.insert(
                (pkg.name, pkg.version),
                PeerPkgInfo {
                    status: effective,
                    peer_build_id: pkg.build_id,
                },
            );
        }

        match total {
            Some(t) => log::info!(
                "Peer check [{peer_url}]: fetched {}/{t} packages ({distribution}/{architecture})",
                map.len(),
            ),
            None => log::info!(
                "Peer check [{peer_url}]: fetched {} packages ({distribution}/{architecture})",
                map.len(),
            ),
        }

        if count < 1000 {
            break;
        }
        after = last_pkg_id;
        if after.is_none() {
            break;
        }
    }

    Ok(map)
}

// Minimal v0 package struct.
#[derive(Debug, Deserialize)]
struct V0PkgRelease {
    name: String,
    version: String,
    status: V0Status,
    build_id: Option<i32>,
    #[serde(default)]
    has_diffoscope: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum V0Status {
    Good,
    Bad,
    #[serde(other)]
    Unknown,
}

async fn fetch_v0_packages(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    architecture: &str,
) -> Result<HashMap<(String, String), PeerPkgInfo>> {
    let url = format!("{}/pkgs/list", peer_url.trim_end_matches('/'));
    let packages: Vec<V0PkgRelease> = client
        .get(&url)
        .query(&[("architecture", architecture)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let map = packages
        .into_iter()
        .map(|pkg| {
            let status = v0_to_build_status(pkg.status);
            (
                (pkg.name, pkg.version),
                PeerPkgInfo {
                    status,
                    peer_build_id: pkg.build_id,
                },
            )
        })
        .collect();

    Ok(map)
}

fn v0_to_build_status(status: V0Status) -> BuildStatus {
    match status {
        V0Status::Good => BuildStatus::Good,
        V0Status::Bad => BuildStatus::Bad,
        V0Status::Unknown => BuildStatus::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Live per-package proxy to a single peer
// ---------------------------------------------------------------------------

async fn fetch_peer_package(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    name: &str,
    version: &str,
    architecture: &str,
) -> Result<PeerStatusEntry> {
    if is_v0_url(peer_url) {
        fetch_v0_peer_package(client, peer_url, name, version, architecture).await
    } else {
        fetch_v1_peer_package(client, peer_url, name, version, architecture).await
    }
}

async fn fetch_v1_peer_package(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    name: &str,
    version: &str,
    architecture: &str,
) -> Result<PeerStatusEntry> {
    let url = format!("{}/packages/binary", peer_url.trim_end_matches('/'));
    let resp: PeerResultPage = client
        .get(&url)
        .query(&[
            ("name", name),
            ("version", version),
            ("architecture", architecture),
            ("seen_only", "true"),
            ("limit", "1"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if let Some(pkg) = resp.records.into_iter().next() {
        let effective = pkg.status.or(pkg.rebuild_status);
        let is_bad = effective == Some(BuildStatus::Bad);
        let (log_url, diffoscope_url) = match pkg.build_id {
            Some(bid) => v1_urls(peer_url, bid, pkg.artifact_id, pkg.diffoscope_log_id, is_bad),
            None => (None, None),
        };
        Ok(PeerStatusEntry {
            url: peer_url.to_owned(),
            status: effective,
            build_id: pkg.build_id,
            log_url,
            diffoscope_url,
        })
    } else {
        Ok(PeerStatusEntry {
            url: peer_url.to_owned(),
            status: None,
            build_id: None,
            log_url: None,
            diffoscope_url: None,
        })
    }
}

async fn fetch_v0_peer_package(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    name: &str,
    version: &str,
    architecture: &str,
) -> Result<PeerStatusEntry> {
    let url = format!("{}/pkgs/list", peer_url.trim_end_matches('/'));
    let packages: Vec<V0PkgRelease> = client
        .get(&url)
        .query(&[("name", name), ("architecture", architecture)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // v0 can return multiple artifacts for a source; find the exact version match.
    if let Some(pkg) = packages.into_iter().find(|p| p.version == version) {
        let peer_status = v0_to_build_status(pkg.status);
        let is_bad = peer_status == BuildStatus::Bad;
        let (log_url, diffoscope_url) = match pkg.build_id {
            Some(bid) => v0_urls(peer_url, bid, pkg.has_diffoscope, is_bad),
            None => (None, None),
        };
        Ok(PeerStatusEntry {
            url: peer_url.to_owned(),
            status: Some(peer_status),
            build_id: pkg.build_id,
            log_url,
            diffoscope_url,
        })
    } else {
        Ok(PeerStatusEntry {
            url: peer_url.to_owned(),
            status: None,
            build_id: None,
            log_url: None,
            diffoscope_url: None,
        })
    }
}
