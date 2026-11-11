CREATE TABLE IF NOT EXISTS conversations (
    id           TEXT PRIMARY KEY,
    target       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    bytes        INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS conversations_last_used_at ON conversations (last_used_at);

CREATE TABLE IF NOT EXISTS settings (
    hash  TEXT PRIMARY KEY,
    json  TEXT NOT NULL,
    bytes INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS responses (
    id                TEXT PRIMARY KEY,
    conversation_id   TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    parent_id         TEXT REFERENCES responses (id) ON DELETE CASCADE,
    status            TEXT NOT NULL,
    settings_hash     TEXT NOT NULL REFERENCES settings (hash),
    input_items       TEXT NOT NULL,
    output_items      TEXT NOT NULL DEFAULT '[]',
    usage             TEXT,
    incomplete_reason TEXT,
    metadata          TEXT,
    created_at        INTEGER NOT NULL,
    completed_at      INTEGER,
    bytes             INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS responses_conversation_id ON responses (conversation_id);
CREATE INDEX IF NOT EXISTS responses_parent_id ON responses (parent_id);
