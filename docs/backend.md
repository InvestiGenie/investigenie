# Backend

Rust crate `investigenie-api` in `backend/` (Axum + Tokio). One binary: `investigenie-api [migrate | healthcheck]`.

## Modules (`backend/src`)

| File | What it does |
|---|---|
| `main.rs` | Startup, the `migrate` and `healthcheck` commands, serving the UI, graceful shutdown. |
| `config.rs` | Reads environment settings; picks quickstart storage when nothing is configured. |
| `api.rs` | All HTTP routes, error mapping, concurrency limits, background AI jobs, static UI serving. |
| `search.rs` | `SearchRequest` and validation of time ranges, histogram intervals, breakdown fields, and timezones. |
| `integrations.rs` | The `Connector` trait, integration descriptors, and secret-handling helpers. |
| `storage.rs`, `storage/filesystem.rs` | Encrypted saved objects with revisions (PostgreSQL or a filesystem snapshot). |
| `ai.rs` | AI investigations: planning, scanning, evidence compaction, analysis cycles, job control. |
| `build.rs` | Finds `integrations/*/mod.rs` and registers them at compile time. |
| `migrations/` | PostgreSQL schema. |

## API

All responses are JSON; errors are `{"error":{"code","message"}}`. Updates and deletes need `If-Match: "<revision>"`; a missing header returns 428 and a stale one returns 409.

| Route | Purpose |
|---|---|
| `GET /api/health`, `/health/live`, `/health/ready` | Liveness and readiness (storage only). |
| `GET, PUT /api/settings` | Server settings (`theme`, `default_timezone`). |
| `GET /api/integrations` | Available datasource types and their UI metadata. |
| `/api/connections[/{id}]` | Create, list, edit, and delete connections. Secrets are never returned. |
| `GET /api/connections/{id}/status` | Live connection check. |
| `GET /api/connections/{id}/indices[/{index}/fields]` | Datasets and fields. |
| `GET /api/connections/{id}/indices/{index}/documents/{doc}` | One document (lookup integrations only). |
| `POST /api/connections/{id}/search` | Documents + histogram. |
| `/api/data-views[/{id}]` | Data View CRUD; validated against the source. |
| `/api/ai/models[/{id}]` | AI model profiles; tested before save; the key is never returned. |
| `POST /api/ai/investigations` | Start a background investigation (202). |
| `GET /api/ai/investigations/{id}` | Job status, progress, partial or final result. |
| `POST /api/ai/investigations/{id}/decision` | `{"action":"continue"\|"terminate"}`. |
| `POST /api/ai/investigate` | Synchronous investigation (legacy). |

## Search and histogram

`SearchRequest` fields: `index`, `query`, `size` (≤100), `cursor` (`""` starts a paged scan), `sort_by`/`sort_order`, `histogram_field`, `time_range` (inclusive epoch ms), `histogram_interval` (`30s`, `5m`, `1d`, `1w`, `1M`, `1y`; requires a time range, ≤1000 buckets), `histogram_breakdown_field` (top 3 values + `__other__`), `time_zone`.

## AI investigations (`ai.rs`)

1. **Plan:** the AI receives the question and the field names (no data) and proposes filters. Invalid filters are dropped, with a caveat explaining it.
2. **Scan:** every matching record is read in 100-record pages (Elasticsearch point-in-time snapshots; ClickHouse offsets).
3. **Compact:** values are trimmed and similar records are grouped by error/message/status fields, keeping a few examples plus counts.
4. **Analyse:** about 9 KB batches go to the AI with the previous findings; it returns a rolling summary.
5. **Clean the output:** model replies are parsed leniently, retried once if unusable, and evidence the model wasn't given is removed.

Jobs (`InvestigationJob`, `Control`) pause at the user's check-in interval or on an AI timeout and wait for continue or stop. They are kept in memory: at most 64, with finished jobs kept for 30 minutes.

## Configuration

| Variable | Default | Notes |
|---|---|---|
| `RUST_API_BIND` | `127.0.0.1:3000` | `0.0.0.0:3000` in Docker. |
| `WEB_ORIGIN` | `http://127.0.0.1:5173` | Only for the separate Vite dev server (CORS). |
| `WEB_DIR` | `web/` next to the binary | Built UI to serve. |
| `PERSISTENCE_MODE` | quickstart | `filesystem` or `database`. |
| `FILESYSTEM_PATH` | `investigenie_data/` next to the binary | One API instance per directory. |
| `DATABASE_URL`, `DB_MAX_CONNECTIONS` | none, `10` | PostgreSQL; use `sslmode=verify-full` outside loopback. |
| `CONFIG_ENCRYPTION_KEY`, `CONFIG_ENCRYPTION_KEY_ID` | generated in quickstart, `v1` | Base64 of 32 random bytes. Keep it; a wrong key fails closed. |

Quickstart (none of `PERSISTENCE_MODE`, `CONFIG_ENCRYPTION_KEY`, `DATABASE_URL` set) stores a generated key beside the data. That suits local use only.

## Tests

```sh
cargo test -p investigenie-api
```

Upstreams are mocked with `wiremock`. PostgreSQL tests are `#[ignore]` and need a database.
