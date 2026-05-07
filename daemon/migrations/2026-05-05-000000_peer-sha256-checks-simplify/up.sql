-- SQLite does not support DROP COLUMN before version 3.35, so we recreate the table
-- without the sha256_match and local_rebuild_id columns, which are no longer used.
CREATE TABLE IF NOT EXISTS peer_sha256_checks_new (
    id                INTEGER   NOT NULL PRIMARY KEY AUTOINCREMENT,
    peer_rebuilder_id INTEGER   NOT NULL REFERENCES peer_rebuilders(id) ON DELETE CASCADE,
    binary_name       TEXT      NOT NULL,
    binary_version    TEXT      NOT NULL,
    peer_build_id     INTEGER,
    checked_at        TIMESTAMP NOT NULL,
    peer_status       TEXT,
    UNIQUE(peer_rebuilder_id, binary_name, binary_version)
);

INSERT INTO peer_sha256_checks_new
    (id, peer_rebuilder_id, binary_name, binary_version, peer_build_id, checked_at, peer_status)
SELECT id, peer_rebuilder_id, binary_name, binary_version, peer_build_id, checked_at, peer_status
FROM peer_sha256_checks;

DROP TABLE peer_sha256_checks;
ALTER TABLE peer_sha256_checks_new RENAME TO peer_sha256_checks;

CREATE INDEX IF NOT EXISTS peer_sha256_checks_peer_idx ON peer_sha256_checks (peer_rebuilder_id);
CREATE INDEX IF NOT EXISTS peer_sha256_checks_name_version_idx
    ON peer_sha256_checks (binary_name, binary_version);
