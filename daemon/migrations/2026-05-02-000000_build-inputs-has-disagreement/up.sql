ALTER TABLE build_inputs ADD COLUMN has_disagreement BOOLEAN NOT NULL DEFAULT 0;
CREATE INDEX build_inputs_has_disagreement_idx ON build_inputs (has_disagreement);
