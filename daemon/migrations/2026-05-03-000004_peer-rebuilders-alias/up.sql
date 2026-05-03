-- Add release_alias (what the peer calls the release, e.g. "unstable" when
-- our local release is "sid") and change the unique key from
-- (url, distribution, architecture, release) to (url, distribution, architecture).
--
-- One row per peer per distro/arch.  release = local name, release_alias = peer name.
-- For existing rows, set release_alias = release (no separate alias was tracked before).
-- Duplicate rows for the same (url, dist, arch) are reduced to the one with MIN(id);
-- their peer_sha256_checks rows are deleted first to avoid orphans.

DELETE FROM peer_sha256_checks
WHERE peer_rebuilder_id NOT IN (
    SELECT MIN(id) FROM peer_rebuilders GROUP BY url, distribution, architecture
);

CREATE TABLE peer_rebuilders_new (
    id              INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    url             TEXT    NOT NULL,
    distribution    TEXT    NOT NULL,
    architecture    TEXT    NOT NULL,
    release         TEXT    NOT NULL DEFAULT '',
    release_alias   TEXT    NOT NULL DEFAULT '',
    UNIQUE(url, distribution, architecture)
);

INSERT INTO peer_rebuilders_new (id, url, distribution, architecture, release, release_alias)
    SELECT MIN(id), url, distribution, architecture, MIN(release), MIN(release)
    FROM peer_rebuilders
    GROUP BY url, distribution, architecture;

DROP TABLE peer_rebuilders;
ALTER TABLE peer_rebuilders_new RENAME TO peer_rebuilders;
