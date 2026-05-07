CREATE TABLE IF NOT EXISTS peer_rebuilders (
    id              INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    url             TEXT    NOT NULL,
    distribution    TEXT    NOT NULL,
    architecture    TEXT    NOT NULL,
    -- Local release name (e.g. "sid"). Empty string = no release (Arch Linux).
    release         TEXT    NOT NULL DEFAULT '',
    -- Release name the peer uses (e.g. "unstable"). Empty string = same as release.
    release_alias   TEXT    NOT NULL DEFAULT '',
    UNIQUE(url, distribution, architecture)
);

CREATE TABLE IF NOT EXISTS peer_sha256_checks (
    id                INTEGER   NOT NULL PRIMARY KEY AUTOINCREMENT,
    peer_rebuilder_id INTEGER   NOT NULL REFERENCES peer_rebuilders(id) ON DELETE CASCADE,
    binary_name       TEXT      NOT NULL,
    binary_version    TEXT      NOT NULL,
    peer_build_id     INTEGER,
    checked_at        TIMESTAMP NOT NULL,
    -- "GOOD" or "BAD" as reported by the peer; NULL if peer does not have the package.
    peer_status       TEXT,
    UNIQUE(peer_rebuilder_id, binary_name, binary_version)
);

CREATE INDEX IF NOT EXISTS peer_sha256_checks_peer_idx ON peer_sha256_checks (peer_rebuilder_id);

-- Index for the correlated EXISTS subquery that filters by (binary_name, binary_version)
-- across all peers. Without this, the subquery does a full table scan per package row.
CREATE INDEX IF NOT EXISTS peer_sha256_checks_name_version_idx
    ON peer_sha256_checks (binary_name, binary_version);
