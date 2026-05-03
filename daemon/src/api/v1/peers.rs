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

// ---------------------------------------------------------------------------
// Attestation JSON structures (shared by local parsing and peer HTTP response)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AttestationJson {
    signed: AttestationSigned,
}

#[derive(Deserialize)]
struct AttestationSigned {
    products: HashMap<String, AttestationProduct>,
}

#[derive(Deserialize)]
struct AttestationProduct {
    sha256: Option<String>,
}

// Used with QueryableByName for raw SQL attestation fetch.
#[derive(QueryableByName)]
struct LocalAttestationRow {
    #[diesel(sql_type = diesel::sql_types::Binary)]
    attestation_log: Vec<u8>,
}

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
    /// ID of the most recent local rebuild (None if never rebuilt).
    local_rebuild_id: Option<i32>,
    status: BuildStatus,
}

struct PeerPkgInfo {
    status: BuildStatus,
    peer_build_id: Option<i32>,
    /// v1 artifact ID (for attestation endpoint); None for v0 peers.
    artifact_id: Option<i32>,
    /// Whether attestation may be available for this package on the peer.
    has_attestation: bool,
}

#[derive(Clone)]
struct PendingCheck {
    peer_rebuilder_id: i32,
    peer_url: String,
    binary_name: String,
    binary_version: String,
    peer_build_id: i32,
    artifact_id: Option<i32>,
    local_rebuild_id: i32,
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
    let rate_limit = cfg.peer_attestation_rate_limit;
    tokio::spawn(async move {
        if let Err(e) = run_peer_check(pool, distribution, architecture, rate_limit).await {
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

    // Load local sha256 (only exists when local status is GOOD with an attestation).
    let local_sha256: Option<String> = {
        let mut conn = pool.get().map_err(Error::from)?;
        load_local_sha256(
            conn.as_mut(),
            &query.distribution,
            &query.architecture,
            &query.name,
            &query.version,
        )?
    };

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
        let local_sha = local_sha256.clone();
        join_set.spawn(async move {
            let result =
                fetch_peer_package(&client, &resolved_url, &n, &v, &a, local_sha.as_deref())
                    .await;
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
                    sha256_match: None,
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
                // If peer_status is missing (pre-migration row), infer GOOD from
                // sha256_match being set — sha256 checks only run when both sides are GOOD.
                let status = c
                    .peer_status
                    .as_deref()
                    .and_then(str_to_build_status)
                    .or_else(|| c.sha256_match.map(|_| BuildStatus::Good));
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
                    sha256_match: c.sha256_match,
                }
            }
            None => PeerStatusEntry {
                url: resolved_url,
                status: None,
                build_id: None,
                log_url: None,
                diffoscope_url: None,
                sha256_match: None,
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
    rate_limit: u32,
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

    // Fetch local packages (with rebuild_id for cache staleness detection).
    let mut connection = pool.get().map_err(Error::from)?;
    let local_packages =
        fetch_local_packages(connection.as_mut(), &distribution, &architecture)?;

    // First pass: collect pending sha256 checks and upserts for status disagreements.
    let mut all_pending: Vec<PendingCheck> = Vec::new();
    // Upserts for status disagreements (peer_status populated; sha256_match = None).
    let mut disagreement_upserts: Vec<UpsertPeerSha256Check> = Vec::new();

    for (peer, resolved_url, peer_map) in &peer_maps {
        // Load sha256 check cache for this peer.
        let cache = load_peer_sha256_cache(connection.as_mut(), peer.id)?;

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
                disagreement_upserts.push(UpsertPeerSha256Check {
                    peer_rebuilder_id: peer.id,
                    binary_name: bin_name.clone(),
                    binary_version: bin_version.clone(),
                    peer_build_id: peer_info.peer_build_id,
                    local_rebuild_id: local_info.local_rebuild_id,
                    sha256_match: None,
                    checked_at: chrono::Utc::now().naive_utc(),
                    peer_status: Some(build_status_to_str(&peer_info.status)),
                });
                continue;
            }

            // Both GOOD — check sha256 cache or schedule attestation fetch.
            if peer_info.status == BuildStatus::Good && local_info.status == BuildStatus::Good {
                let cache_key = (bin_name.clone(), bin_version.clone());
                let cached = cache.get(&cache_key);

                let is_fresh = cached.map_or(false, |c| {
                    c.peer_build_id == peer_info.peer_build_id
                        && c.local_rebuild_id == local_info.local_rebuild_id
                });

                if is_fresh {
                    // sha256_match is already stored in peer_sha256_checks; nothing to do.
                } else if peer_info.has_attestation {
                    if let (Some(peer_build_id), Some(local_rebuild_id)) =
                        (peer_info.peer_build_id, local_info.local_rebuild_id)
                    {
                        all_pending.push(PendingCheck {
                            peer_rebuilder_id: peer.id,
                            peer_url: resolved_url.clone(),
                            binary_name: bin_name.clone(),
                            binary_version: bin_version.clone(),
                            peer_build_id,
                            artifact_id: peer_info.artifact_id,
                            local_rebuild_id,
                        });
                    }
                }
            }
        }
    }
    drop(connection);

    log::info!(
        "Peer check: {distribution}/{architecture} — {} status disagreements, {} attestation checks pending",
        disagreement_upserts.len(),
        all_pending.len(),
    );

    // Group pending checks by peer so each peer gets its own independent rate-limited stream.
    let mut pending_by_peer: HashMap<i32, Vec<PendingCheck>> = HashMap::new();
    for item in all_pending {
        pending_by_peer.entry(item.peer_rebuilder_id).or_default().push(item);
    }

    // Pre-load all local sha256s from DB before spawning async peer tasks.
    let local_sha256_cache = {
        let mut cache: HashMap<(i32, String), Option<String>> = HashMap::new();
        if !pending_by_peer.is_empty() {
            let mut conn = pool.get().map_err(Error::from)?;
            for items in pending_by_peer.values() {
                for item in items {
                    let key = (item.local_rebuild_id, item.binary_name.clone());
                    if !cache.contains_key(&key) {
                        let sha256 = load_local_sha256_by_rebuild_id(
                            conn.as_mut(),
                            item.local_rebuild_id,
                            &item.binary_name,
                        )?;
                        cache.insert(key, sha256);
                    }
                }
            }
        }
        std::sync::Arc::new(cache)
    };

    // One rate-limited task per peer, all running concurrently.
    // Each peer gets rate_limit requests/min independently (they go to different hosts).
    let mut attest_set = tokio::task::JoinSet::new();
    for (_, items) in pending_by_peer {
        let client = http_client.clone();
        let sha256_cache = local_sha256_cache.clone();
        let dist = distribution.clone();
        let arch = architecture.clone();
        attest_set.spawn(async move {
            let peer_url = items.first().map(|i| i.peer_url.as_str()).unwrap_or("").to_owned();
            let interval_ms = 60_000u64 / rate_limit.max(1) as u64;
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            let total = items.len();
            let mut upserts: Vec<UpsertPeerSha256Check> = Vec::new();

            for (i, item) in items.iter().enumerate() {
                interval.tick().await;
                if i % 100 == 0 || i + 1 == total {
                    log::info!(
                        "Peer check [{peer_url}]: attestation {}/{total} for {dist}/{arch}",
                        i + 1,
                    );
                }

                let peer_sha256 = fetch_peer_sha256(
                    &client,
                    &item.peer_url,
                    &item.binary_name,
                    item.peer_build_id,
                    item.artifact_id,
                )
                .await;

                // Skip caching on connection failure so it's retried next run.
                let Some(peer_sha256) = peer_sha256 else { continue };

                let local_sha256 = sha256_cache
                    .get(&(item.local_rebuild_id, item.binary_name.clone()))
                    .and_then(|opt| opt.as_deref().map(str::to_owned));

                let sha256_match = local_sha256.as_deref().map(|l| l == peer_sha256);

                upserts.push(UpsertPeerSha256Check {
                    peer_rebuilder_id: item.peer_rebuilder_id,
                    binary_name: item.binary_name.clone(),
                    binary_version: item.binary_version.clone(),
                    peer_build_id: Some(item.peer_build_id),
                    local_rebuild_id: Some(item.local_rebuild_id),
                    sha256_match,
                    checked_at: chrono::Utc::now().naive_utc(),
                    peer_status: Some("GOOD".to_owned()),
                });
            }

            upserts
        });
    }

    let mut upserts: Vec<UpsertPeerSha256Check> = disagreement_upserts;
    while let Some(task_result) = attest_set.join_next().await {
        match task_result {
            Ok(peer_upserts) => upserts.extend(peer_upserts),
            Err(e) => log::warn!("Peer attestation task panicked: {e}"),
        }
    }

    // Upsert sha256 check results into the cache table.
    if !upserts.is_empty() {
        let mut conn = pool.get().map_err(Error::from)?;
        conn.transaction(|conn| {
            for upsert in &upserts {
                diesel::insert_into(peer_sha256_checks::table)
                    .values(upsert)
                    .on_conflict((
                        peer_sha256_checks::peer_rebuilder_id,
                        peer_sha256_checks::binary_name,
                        peer_sha256_checks::binary_version,
                    ))
                    .do_update()
                    .set(upsert)
                    .execute(conn)?;
            }
            Ok::<_, diesel::result::Error>(())
        })
        .map_err(Error::from)?;
        log::info!(
            "Peer check: cached {} sha256 results for {distribution}/{architecture}",
            upserts.len(),
        );
    }

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
            r1.field(rebuilds::id).nullable(),
        ))
        .get_results::<(
            String,
            String,
            Option<BuildStatus>,
            Option<BuildStatus>,
            Option<i32>,
        )>(conn)
        .map_err(Error::from)?;

    let mut map = HashMap::with_capacity(rows.len());
    for (bin_name, bin_version, artifact_status, rebuild_status, local_rebuild_id) in rows {
        // Artifact-level status (GOOD/BAD) takes priority; fall back to
        // build-level status (FAIL) if no artifact row.
        let effective = artifact_status.or(rebuild_status).unwrap_or(BuildStatus::Unknown);
        map.insert(
            (bin_name, bin_version),
            LocalPkgInfo {
                local_rebuild_id,
                status: effective,
            },
        );
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Peer sha256 cache helpers
// ---------------------------------------------------------------------------

/// Loads the sha256 check cache for a single peer rebuilder.
fn load_peer_sha256_cache(
    conn: &mut SqliteConnection,
    peer_rebuilder_id: i32,
) -> Result<HashMap<(String, String), PeerSha256Check>> {
    let rows = peer_sha256_checks::table
        .filter(peer_sha256_checks::peer_rebuilder_id.eq(peer_rebuilder_id))
        .get_results::<PeerSha256Check>(conn)
        .map_err(Error::from)?;

    Ok(rows
        .into_iter()
        .map(|r| ((r.binary_name.clone(), r.binary_version.clone()), r))
        .collect())
}

// ---------------------------------------------------------------------------
// Local attestation sha256 loaders
// ---------------------------------------------------------------------------

/// Returns the sha256 of the latest GOOD local rebuild of the named artifact,
/// or `None` if no GOOD rebuild with an attestation exists.
fn load_local_sha256(
    conn: &mut SqliteConnection,
    distribution: &str,
    architecture: &str,
    name: &str,
    version: &str,
) -> Result<Option<String>> {
    let row = diesel::sql_query(
        "SELECT al.attestation_log
         FROM binary_packages bp
         JOIN build_inputs bi ON bi.id = bp.build_input_id
         JOIN source_packages sp ON sp.id = bi.source_package_id
         JOIN rebuilds r ON r.build_input_id = bi.id
         JOIN rebuild_artifacts ra ON ra.rebuild_id = r.id AND ra.name = bp.name
         JOIN attestation_logs al ON al.id = ra.attestation_log_id
         WHERE bp.name = ? AND bp.version = ? AND bp.architecture = ?
           AND sp.distribution = ?
           AND ra.status = 'GOOD'
         ORDER BY r.built_at DESC
         LIMIT 1",
    )
    .bind::<diesel::sql_types::Text, _>(name)
    .bind::<diesel::sql_types::Text, _>(version)
    .bind::<diesel::sql_types::Text, _>(architecture)
    .bind::<diesel::sql_types::Text, _>(distribution)
    .get_result::<LocalAttestationRow>(conn)
    .optional()
    .map_err(Error::from)?;

    let Some(row) = row else {
        return Ok(None);
    };

    extract_sha256_from_attestation_bytes(&row.attestation_log, name)
}

/// Loads the sha256 for a specific local rebuild ID (for the bulk check cache path).
fn load_local_sha256_by_rebuild_id(
    conn: &mut SqliteConnection,
    rebuild_id: i32,
    artifact_name: &str,
) -> Result<Option<String>> {
    let row = diesel::sql_query(
        "SELECT al.attestation_log
         FROM rebuild_artifacts ra
         JOIN attestation_logs al ON al.id = ra.attestation_log_id
         WHERE ra.rebuild_id = ? AND ra.name = ? AND ra.status = 'GOOD'
         LIMIT 1",
    )
    .bind::<diesel::sql_types::Integer, _>(rebuild_id)
    .bind::<diesel::sql_types::Text, _>(artifact_name)
    .get_result::<LocalAttestationRow>(conn)
    .optional()
    .map_err(Error::from)?;

    let Some(row) = row else {
        return Ok(None);
    };

    extract_sha256_from_attestation_bytes(&row.attestation_log, artifact_name)
}

/// Decompress (if zstd) and extract the sha256 for `artifact_name` from an
/// in-toto attestation blob. Returns the first product sha256 whose key
/// contains the artifact name, or any product sha256 if there is only one.
fn extract_sha256_from_attestation_bytes(
    bytes: &[u8],
    artifact_name: &str,
) -> Result<Option<String>> {
    let json_bytes: Vec<u8> = if bytes.first().copied() == Some(0x28)
        && bytes.get(1).copied() == Some(0xb5)
    {
        // zstd magic: 0xFD2FB528 (little-endian first bytes are 0x28, 0xB5)
        zstd::stream::decode_all(bytes)?
    } else {
        bytes.to_vec()
    };

    let attestation: AttestationJson = match serde_json::from_slice(&json_bytes) {
        Ok(a) => a,
        Err(e) => {
            log::warn!("Failed to parse attestation JSON: {e}");
            return Ok(None);
        }
    };

    // Prefer the product whose filename matches; fall back to first product.
    let sha256 = attestation
        .signed
        .products
        .iter()
        .find(|(k, _)| k.contains(artifact_name))
        .or_else(|| attestation.signed.products.iter().next())
        .and_then(|(_, p)| p.sha256.clone());

    Ok(sha256)
}

// ---------------------------------------------------------------------------
// Peer attestation fetcher
// ---------------------------------------------------------------------------

/// Fetch the sha256 from a peer's attestation for a GOOD package.
/// `build_id` — peer's rebuild ID; `artifact_id` — only for v1 peers.
async fn fetch_peer_sha256(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    artifact_name: &str,
    build_id: i32,
    artifact_id: Option<i32>,
) -> Option<String> {
    let url = if is_v0_url(peer_url) {
        format!("{}/builds/{}/attestation", peer_url.trim_end_matches('/'), build_id)
    } else if let Some(aid) = artifact_id {
        format!(
            "{}/builds/{}/artifacts/{}/attestation",
            peer_url.trim_end_matches('/'),
            build_id,
            aid
        )
    } else {
        return None;
    };

    let bytes = match client.get(&url).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(r) => match r.bytes().await {
                Ok(b) => b.to_vec(),
                Err(e) => {
                    log::warn!("Failed to read peer attestation body from {url}: {e}");
                    return None;
                }
            },
            Err(e) => {
                log::warn!("Peer attestation request failed {url}: {e}");
                return None;
            }
        },
        Err(e) => {
            log::warn!("Peer attestation request error {url}: {e}");
            return None;
        }
    };

    match extract_sha256_from_attestation_bytes(&bytes, artifact_name) {
        Ok(sha256) => sha256,
        Err(e) => {
            log::warn!("Failed to extract sha256 from peer attestation at {url}: {e}");
            None
        }
    }
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
    attestation_log_id: Option<i32>,
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
            let has_attestation = pkg.attestation_log_id.is_some();
            map.insert(
                (pkg.name, pkg.version),
                PeerPkgInfo {
                    status: effective,
                    peer_build_id: pkg.build_id,
                    artifact_id: pkg.artifact_id,
                    has_attestation,
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
    #[serde(default)]
    has_attestation: bool,
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
                    artifact_id: None, // v0 has no artifact IDs
                    has_attestation: pkg.has_attestation,
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
    local_sha256: Option<&str>,
) -> Result<PeerStatusEntry> {
    if is_v0_url(peer_url) {
        fetch_v0_peer_package(client, peer_url, name, version, architecture, local_sha256).await
    } else {
        fetch_v1_peer_package(client, peer_url, name, version, architecture, local_sha256).await
    }
}

async fn fetch_v1_peer_package(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    name: &str,
    version: &str,
    architecture: &str,
    local_sha256: Option<&str>,
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
        let sha256_match = if effective == Some(BuildStatus::Good) {
            if let (Some(local), Some(bid), Some(aid)) =
                (local_sha256, pkg.build_id, pkg.artifact_id)
            {
                let peer_sha256 =
                    fetch_peer_sha256(client, peer_url, name, bid, Some(aid)).await;
                peer_sha256.map(|p| p == local)
            } else {
                None
            }
        } else {
            None
        };
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
            sha256_match,
        })
    } else {
        Ok(PeerStatusEntry {
            url: peer_url.to_owned(),
            status: None,
            build_id: None,
            log_url: None,
            diffoscope_url: None,
            sha256_match: None,
        })
    }
}

async fn fetch_v0_peer_package(
    client: &rebuilderd_common::http::Client,
    peer_url: &str,
    name: &str,
    version: &str,
    architecture: &str,
    local_sha256: Option<&str>,
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
        let sha256_match = if peer_status == BuildStatus::Good && pkg.has_attestation {
            if let (Some(local), Some(bid)) = (local_sha256, pkg.build_id) {
                let peer_sha256 =
                    fetch_peer_sha256(client, peer_url, name, bid, None).await;
                peer_sha256.map(|p| p == local)
            } else {
                None
            }
        } else {
            None
        };
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
            sha256_match,
        })
    } else {
        Ok(PeerStatusEntry {
            url: peer_url.to_owned(),
            status: None,
            build_id: None,
            log_url: None,
            diffoscope_url: None,
            sha256_match: None,
        })
    }
}
