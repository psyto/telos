-- Append-only event log keyed by intent_id.
--
-- Each row records one stage transition for one intent. Stages run
-- observed → quoted → simulated → decided → settled, but the schema
-- doesn't enforce ordering — a real-world intent can stop at any
-- stage (rejected at decide, no quote because no price, etc).
--
-- payload_json carries the stage-specific fields as serde-serialised
-- JSON. Going through JSON keeps the schema stable while domain types
-- evolve; the trade-off is no SQL-level filtering on payload contents.

CREATE TABLE intent_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    intent_id    TEXT    NOT NULL,
    stage        TEXT    NOT NULL,
    payload_json TEXT    NOT NULL,
    created_at   INTEGER NOT NULL
);

CREATE INDEX idx_intent_events_intent_id ON intent_events(intent_id);
CREATE INDEX idx_intent_events_stage     ON intent_events(stage);
