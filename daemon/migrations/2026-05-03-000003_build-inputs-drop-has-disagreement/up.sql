DROP INDEX IF EXISTS build_inputs_has_disagreement_idx;
ALTER TABLE build_inputs DROP COLUMN has_disagreement;
