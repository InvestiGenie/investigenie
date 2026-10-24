CREATE TABLE knowledge_entries (
    id TEXT PRIMARY KEY CHECK (id ~ '^[0-9a-f-]{36}$'),
    revision TEXT NOT NULL,
    entry_encrypted BYTEA NOT NULL
);