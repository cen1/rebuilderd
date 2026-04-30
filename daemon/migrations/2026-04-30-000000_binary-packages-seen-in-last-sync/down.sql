DROP INDEX binary_packages_seen_in_last_sync_idx;
-- SQLite does not support DROP COLUMN in older versions; recreate table to remove column.
