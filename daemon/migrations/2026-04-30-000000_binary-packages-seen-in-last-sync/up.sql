ALTER TABLE binary_packages ADD COLUMN seen_in_last_sync BOOLEAN NOT NULL DEFAULT 1;

CREATE INDEX binary_packages_seen_in_last_sync_idx ON binary_packages (seen_in_last_sync);
