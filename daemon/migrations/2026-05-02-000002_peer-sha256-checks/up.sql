CREATE TABLE peer_sha256_checks (
    id                INTEGER   NOT NULL PRIMARY KEY AUTOINCREMENT,
    peer_rebuilder_id INTEGER   NOT NULL REFERENCES peer_rebuilders(id) ON DELETE CASCADE,
    binary_name       TEXT      NOT NULL,
    binary_version    TEXT      NOT NULL,
    peer_build_id     INTEGER,
    local_rebuild_id  INTEGER,
    sha256_match      BOOLEAN,
    checked_at        TIMESTAMP NOT NULL,
    UNIQUE(peer_rebuilder_id, binary_name, binary_version)
);
CREATE INDEX peer_sha256_checks_peer_idx ON peer_sha256_checks (peer_rebuilder_id);
