CREATE TABLE ai_conversations (
    id TEXT PRIMARY KEY CHECK (id ~ '^[a-z0-9_-]{1,64}$'),
    revision TEXT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    content_encrypted BYTEA NOT NULL
);
CREATE INDEX ai_conversations_updated ON ai_conversations (updated_at_ms DESC);