-- Durable metadata for externally orchestrated conversation dispatches.
-- The instruction body is deliberately excluded: recovery must never replay
-- an interrupted command automatically after an AionCore restart.
CREATE TABLE external_conversation_dispatches (
    operation_id TEXT PRIMARY KEY,
    request_fingerprint TEXT NOT NULL,
    actor_conversation_id TEXT NOT NULL,
    target_conversation_id TEXT,
    state TEXT NOT NULL,
    response_json TEXT NOT NULL,
    workspace_lease_json TEXT,
    boot_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    terminal_at INTEGER
);

CREATE INDEX idx_external_conversation_dispatches_terminal_at
    ON external_conversation_dispatches(terminal_at);
