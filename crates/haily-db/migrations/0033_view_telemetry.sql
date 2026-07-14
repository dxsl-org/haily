-- View Engine Phase A telemetry (phase 3) — the Phase-B gate signal (design §14): whether an
-- LLM-projected entity view actually gets used, switched between projections, or provokes an
-- explicit edit-demand ask. `detail`'s meaning is keyed by `kind`:
--   'projection_switched' → the switched-TO projection kind ('table' | 'cards' | ...)
--   'usefulness'          → the raw thumb-up/thumb-down the GUI control sent
--   'edit_demand'         → the free-text intent (REQUIRED non-empty by the write path — see
--                            haily-db/src/queries/view_events.rs::insert_view_event; a bare
--                            click is not demand, per the funnel's anti-false-positive design)
--   'presented' / 'viewed' → NULL (no extra payload)
--
-- No FK to a `views` table: a `DataView` is an ephemeral, in-memory `ViewStore` snapshot with
-- no row of its own, so `view_id` here is an opaque UUID correlating rows from the SAME view
-- rather than a join key into another table.
CREATE TABLE view_events (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    view_id     TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN
                    ('presented', 'viewed', 'projection_switched', 'usefulness', 'edit_demand')),
    detail      TEXT,
    created_at  TEXT NOT NULL
);

-- The GO-ratio readbacks aggregate by kind across the whole table (small volume; funnel
-- signal only, see view_events.rs); view_id/session_id are the natural secondary lookups.
CREATE INDEX idx_view_events_kind ON view_events(kind);
CREATE INDEX idx_view_events_view ON view_events(view_id);
CREATE INDEX idx_view_events_session ON view_events(session_id);
