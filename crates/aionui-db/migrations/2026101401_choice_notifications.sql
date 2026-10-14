-- Server-owned choice notification state shared by every client for one user.
CREATE TABLE IF NOT EXISTS choice_notifications (
    user_id        TEXT    NOT NULL,
    key            TEXT    NOT NULL,
    conversation_id TEXT  NOT NULL,
    message_id     TEXT    NOT NULL,
    marker_index   INTEGER NOT NULL CHECK (marker_index >= 0),
    preview        TEXT    NOT NULL,
    status         TEXT    NOT NULL CHECK (status IN ('pending', 'dismissed', 'resolved', 'superseded')),
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (user_id, key),
    UNIQUE (user_id, conversation_id, message_id, marker_index),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_choice_notifications_pending
    ON choice_notifications(user_id, status, created_at, key);

CREATE INDEX IF NOT EXISTS idx_choice_notifications_conversation
    ON choice_notifications(user_id, conversation_id, status);
