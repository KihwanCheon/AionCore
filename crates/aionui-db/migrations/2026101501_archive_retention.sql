-- User-owned archive retention schedules; disabled until explicitly configured.
CREATE TABLE IF NOT EXISTS archive_retention (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    retention_days INTEGER NOT NULL CHECK (retention_days BETWEEN 1 AND 3650),
    run_at TEXT NOT NULL,
    timezone TEXT NOT NULL,
    next_run_at INTEGER,
    last_run_at INTEGER,
    last_deleted INTEGER NOT NULL DEFAULT 0,
    last_failed INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_archive_retention_due
    ON archive_retention(next_run_at) WHERE enabled = 1;
