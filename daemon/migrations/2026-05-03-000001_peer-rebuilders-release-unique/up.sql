-- Add release to the peer_rebuilders unique key so that a profile with
-- multiple releases can have separate rows per (url, distribution,
-- architecture, release).  SQLite cannot ALTER constraints, so recreate.
-- release is stored as '' (empty string) when there is no release filter
-- (e.g. Arch Linux).  Existing NULL values are coerced to ''.

CREATE TABLE peer_rebuilders_new (
    id           INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    url          TEXT    NOT NULL,
    distribution TEXT    NOT NULL,
    architecture TEXT    NOT NULL,
    release      TEXT    NOT NULL DEFAULT '',
    UNIQUE(url, distribution, architecture, release)
);

INSERT INTO peer_rebuilders_new (id, url, distribution, architecture, release)
    SELECT id, url, distribution, architecture, COALESCE(release, '')
    FROM peer_rebuilders;

DROP TABLE peer_rebuilders;
ALTER TABLE peer_rebuilders_new RENAME TO peer_rebuilders;
