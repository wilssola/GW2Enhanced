-- Indexes for the paged text listing (GET /v1/texts): each sort reads one index of `current` in order, with the
-- text key as tie-break, so a page never needs a sort of the whole table.
DROP INDEX IF EXISTS current_score;
DROP INDEX IF EXISTS current_updated;
CREATE INDEX current_score ON current (lang, score, text_key);
CREATE INDEX current_updated ON current (lang, updated_at, text_key);
-- "Worst": down votes minus up votes (the query uses this exact expression).
CREATE INDEX current_net ON current (lang, (down - up), text_key);
