CREATE TABLE ai_models (
    id TEXT PRIMARY KEY CHECK (id ~ '^[0-9a-f-]{36}$'),
    name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 128),
    api_url TEXT NOT NULL CHECK (length(api_url) <= 2048),
    model TEXT NOT NULL CHECK (length(trim(model)) BETWEEN 1 AND 256),
    api_key_encrypted BYTEA NOT NULL,
    revision TEXT NOT NULL
);
CREATE UNIQUE INDEX ai_models_name ON ai_models (lower(name));