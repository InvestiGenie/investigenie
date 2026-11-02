ALTER TABLE data_views
    ADD COLUMN explore_mode TEXT NOT NULL DEFAULT 'logs'
    CHECK (explore_mode IN ('logs', 'metrics', 'traces'));

UPDATE data_views AS dv
SET explore_mode = 'metrics'
FROM connections AS conn
WHERE conn.id = dv.connection_id
  AND conn.integration_id IN ('prometheus', 'victoriametrics');