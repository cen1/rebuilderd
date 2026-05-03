CREATE TABLE peer_rebuilders (
    id           INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    url          TEXT    NOT NULL,
    distribution TEXT    NOT NULL,
    architecture TEXT    NOT NULL,
    UNIQUE(url, distribution, architecture)
);
