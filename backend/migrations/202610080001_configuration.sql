CREATE TABLE connections (
    id TEXT PRIMARY KEY CHECK (id ~ '^[a-z0-9_-]{1,64}$'),
    name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 256),
    integration_id TEXT NOT NULL,
    config_encrypted BYTEA NOT NULL,
    revision TEXT NOT NULL
);
CREATE TABLE data_views (
    id TEXT PRIMARY KEY CHECK (id ~ '^[a-z0-9_-]{1,64}$'),
    name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 256),
    connection_id TEXT NOT NULL REFERENCES connections(id) ON UPDATE CASCADE ON DELETE CASCADE,
    index_name TEXT NOT NULL CHECK (length(trim(index_name)) BETWEEN 1 AND 1024),
    time_field TEXT NOT NULL CHECK (length(trim(time_field)) BETWEEN 1 AND 1024),
    revision TEXT NOT NULL
);
CREATE UNIQUE INDEX data_views_name ON data_views (lower(name));
CREATE INDEX data_views_connection ON data_views (connection_id);
CREATE TABLE app_settings (
    singleton BOOLEAN PRIMARY KEY DEFAULT true CHECK (singleton),
    settings JSONB NOT NULL CHECK (jsonb_typeof(settings) = 'object'),
    revision TEXT NOT NULL,
    encryption_verifier BYTEA
);
INSERT INTO app_settings (settings, revision) VALUES ('{"theme":"system","default_timezone":"UTC"}', gen_random_uuid()::text);