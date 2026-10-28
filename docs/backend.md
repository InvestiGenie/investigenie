# Backend

Rust crate `investigenie-api` in `backend/` (Axum + Tokio). One binary: `investigenie-api [migrate | healthcheck] [--config <path>]`.

## Modules (`backend/src`)

| File | What it does |
|---|---|
| `main.rs` | Startup, the `migrate` and `healthcheck` commands, serving the UI, graceful shutdown; declares backend-owned data/AI contract modules. |
| `config.rs`, `config/file.rs` | Strict typed TOML loading, command parsing, environment overrides, path resolution, and validated settings/quickstart storage. |
| `logging.rs` | Log-level and destination settings, console/daily file output, safe HTTP metadata, and operation timing/cancellation. |
| `api.rs` | All HTTP routes, error mapping, concurrency limits, background AI jobs, static UI serving. |
| `search.rs` | `SearchRequest` and validation of time ranges, histogram intervals, breakdown fields, and timezones. |
| `data_connectors.rs` | The datasource `Connector` trait, descriptors, secret-handling helpers, and generated registry. Implementations import `crate::data_connectors`. |
| `storage.rs`, `storage/filesystem.rs` | Encrypted saved objects with revisions (PostgreSQL or a filesystem snapshot). |
| `storage/knowledge.rs`, `api/knowledge.rs` | Bounded encrypted knowledge entries, revision-checked CRUD, and strict API input types. |
| `ai.rs` | AI orchestration: metadata planning, bounded evidence gathering, hypothesis generation, JSON parsing/repair, job control, follow-up context, and model probe semantics. Uses the shared AI completion interface. |
| `build.rs` | Finds `integrations/data_connectors/*/mod.rs` and registers datasources at compile time; AI connectors are excluded from this registry. |
| `ai_connectors.rs` | Provider-neutral `AiConnector` trait, settings, reply/usage/error types, explicit transport modules, and default registration. |
| `integrations/ai_connectors/openai/mod.rs` (repo root) | OpenAI-compatible HTTP transport, bearer auth, bounded payloads, response/usage decoding, and unsupported JSON-mode fallback. |
| `migrations/` | PostgreSQL schema. |

## API

Responses are JSON except for no-content deletes such as knowledge deletion (204); errors are `{"error":{"code","message"}}`. Updates and deletes need `If-Match: "<revision>"`; a missing header returns 428 and a stale one returns 409.

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
| `GET, POST /api/knowledge` | List `{entries}` or create `{entry}` (201). |
| `GET, PUT, DELETE /api/knowledge/{id}` | Read/update `{entry}` or delete (204); updates/deletes require quoted `If-Match`. |
| `/api/ai/models[/{id}]` | AI model profiles; tested before save; the key is never returned. |
| `POST /api/ai/investigations` | Start a background investigation (202). |
| `GET /api/ai/investigations/{id}` | Job status, progress, partial or final result. |
| `POST /api/ai/investigations/{id}/decision` | `{"action":"continue"\|"terminate"}`. |
| `POST /api/ai/investigate` | Synchronous investigation (legacy). |

## Search and histogram

For Elasticsearch Explore, the first-page document request fetches only the requested page (50 rows) with exact hit tracking disabled. A separate `size: 0` request uses the same query, time range, and PIT to compute the full-scope histogram and exact total. Later cursor pages omit that aggregation and reuse the first-page chart/total. If the histogram request fails, document rows are still returned with a lower-bound total and a histogram-unavailable reason. AI evidence searches keep their bounded row and aggregation work within each source query to preserve the investigation query budget. All time breakdowns resolve a common bucket interval before calculating top-value series; Elasticsearch uses a min/max timestamp preflight on the same PIT when paging. The time scope remains unbounded.

Elasticsearch and ClickHouse use `HistogramInterval::automatic(span_millis)` in `search.rs` for automatic breakdown intervals. Keep the candidate intervals and target bucket count in that helper, not separate connector-specific lists. Elasticsearch tolerates unmapped time fields when sorting multi-index patterns; ClickHouse histogram failures preserve valid rows and report an unavailable reason. SQL `LIMIT` bounds returned rows, while ClickHouse server profiles control memory limits; fixed memory/result/overflow URL overrides are not sent.

`SearchRequest` fields: `index`, `query`, `size` (positive; public API clamps to 50, default 50), `cursor` (`""` starts a result window), `sort_by`/`sort_order`, `histogram_field`, `time_range` (inclusive epoch ms), `histogram_interval` (`30s`, `5m`, `1d`, `1w`, `1M`, `1y`; requires a time range, ≤1000 buckets), `histogram_breakdown_field` (top 3 values + `__other__`), `time_zone`.

Every public connector search exposes at most 100 records per result window: two 50-record pages in Explore. Shared `SearchWindow` logic wraps native continuation cursors, bounds legacy offsets to 0-99, and removes `next_cursor` at the ceiling without changing totals or histogram buckets. An invalid/exhausted cursor or offset returns 400 `invalid_search_window`; restart older cursor searches after upgrading. No continuation can mean the display cap, not source exhaustion. Narrowing the query or clicking/dragging the graph starts a fresh window for the selected time range. Native connector paging remains bounded to 100 records per call; internal AI evidence limits are unchanged. Aggregate queries and some source sorts may still examine more than 100 records upstream.

## AI conversation history

AI history routes: `GET /api/ai/conversations` lists summaries; `GET /api/ai/conversations/{id}` reads a conversation; `POST /api/ai/conversations/{id}/messages` adds a follow-up; `DELETE /api/ai/conversations/{id}` deletes it. Follow-ups and deletes require the current quoted `If-Match` revision.

Conversation messages and investigation results are persisted in the encrypted filesystem snapshot or as encrypted PostgreSQL payloads. They are bounded to 64 conversations, 200 messages per conversation, and 128 KiB per conversation to fit the filesystem snapshot limit. When the history is full, starting a new investigation evicts the least recently updated conversation (both storage modes), so the history behaves like a rolling window instead of rejecting new work. Follow-ups use the stored transcript and result; they do not rescan sources. The app has no user authentication, so history is shared by anyone who can access the instance.

## AI investigations (`ai.rs`)

If a provider explicitly rejects JSON response mode, the request is retried without that option; the model is still instructed to return the required JSON shape. Elasticsearch query-string searches tolerate incompatible field mappings across matched indices, and missing fields in individual records are treated as normal.

The provider-specific HTTP implementation lives only in `integrations/ai_connectors/openai/`. Its prompt and completion limits remain 65,536 and 32,768 bytes, with a bounded 4,096-byte error preview used privately for fallback detection. `AiConnectorError` maps to existing investigation error codes. Backend orchestration keeps repair retries, time-budget pauses/cancellation, evidence checks, and history ownership; it does not depend on a concrete provider implementation. Existing saved AI models continue using the default OpenAI-compatible transport with no migration.

Both shared connector contracts and registrations live in the backend, while `integrations/data_connectors/<id>/` and `integrations/ai_connectors/<id>/` contain concrete implementations. `backend/build.rs` scans only immediate datasource directories and generates `OUT_DIR/data_connectors.rs`, included by the backend data contract. There are no old-module forwarding wrappers or shared contract files under `integrations/`.

API helpers `model_settings`, `prepare_ai_call`, and `probe_ai_model` share model settings, capacity acquisition, and provider probes across synchronous/background investigations, follow-ups, and model saves. Keep validation/revision checks before those operations; model saves still probe before persistence. `ai_model_save_error` preserves the existing duplicate-model response. Both conversation creation and updates reuse `conversation_timestamp`; no storage schema changes are involved.

1. **Plan evidence:** the AI receives the question, selected Data View metadata (field names and types), and time scope. It proposes validated filters, up to four sample fields, and an optional aggregation field; invalid choices are dropped with a caveat.
2. **Gather evidence:** for each selected Data View, the backend performs at most three bounded sample queries: latest records, earliest records only when an exact/lower-bound total indicates more matches may exist, and an unfiltered query within the same time scope if proposed filters return no rows. Each returns at most four rows; at most eight unique rows are retained per Data View. An exact total equal to the first sample size avoids the extra comparison query. The first query may also request a source-side histogram/top-values aggregation, which can count more records than are sampled. Only four selected fields per row are retained, each capped at 256 bytes; the hypothesis payload keeps at most 16 aggregate buckets. Query timeouts fail the investigation rather than being treated as complete evidence. The engine does not page through every matching record.
3. **Form hypotheses:** the AI receives per-query scope, aggregate summaries, and compact samples, then proposes hypotheses. Findings without validated sample citations are removed; each finding count is derived from its distinct cited sample IDs. Source aggregation totals are reported separately and cannot be copied into a finding count.
4. **Clean the output:** model replies are parsed leniently, retried once if unusable, and unsupported evidence is removed.
5. **Knowledge retrieval (future):** manual knowledge storage is implemented. The UI can explicitly copy a completed investigation into an unverified draft through the existing knowledge create route; investigations do not automatically create entries or retrieve knowledge. Future retrieval must treat published entries as candidate guidance, recheck current bounded evidence, and retain cited revision/excerpt snapshots. See the [investigation pipeline plan](investigation-pipeline.md).

## Knowledge management

Entries contain `title`, `kind` (`service_context`, `runbook`, `resolved_incident`), `status` (`draft`, `published`, `archived`), Markdown `content`, `services`, `environments`, `connection_ids`, `data_view_ids`, `tags`, and an optional `source_url`. Empty scope arrays mean global applicability. Scope identifiers are retained when their source is deleted; the UI labels unavailable references rather than silently broadening the scope. IDs, revisions, and created/updated timestamps are server-managed and are not accepted in mutation bodies.

Both modes encrypt the complete entry, not just its content. PostgreSQL stores each encrypted payload in `knowledge_entries`; filesystem mode adds a default-empty map to existing snapshots. Updates and deletes use atomic revision checks; stale writes return 409 without retry. There is no stored revision-history browser in this milestone.

Limits: 128 entries, nonblank titles up to 256 bytes, nonblank content up to 32,768 bytes, up to 20 items per scope/tag array (labels up to 128 bytes), source links up to 2,048 bytes, and API mutation bodies up to 65,536 bytes. Source links permit only HTTP/HTTPS without embedded credentials. Search/filtering is local to the bounded UI collection; no search index or embeddings are persisted.

Publishing indicates editorial availability, not independent verification. Completed investigation results can be explicitly saved as drafts and reviewed through the Knowledge editor; this uses existing CRUD and does not change the storage schema. Automatic creation/publishing, automatic retrieval, immutable investigation knowledge citations, and feedback promotion are not implemented yet. The instance has no login or tenancy; anyone with access can read and modify knowledge, so protect it with the same authenticated gateway as the rest of the application.

### Storage, upgrades, and backups

- Filesystem mode keeps entries inside the encrypted `configuration.snapshot` under `FILESYSTEM_PATH`, alongside the other saved objects. Quickstart mode stores the generated `encryption.key` in that directory; explicitly configured deployments obtain their key from `CONFIG_ENCRYPTION_KEY` instead.
- PostgreSQL mode stores encrypted payloads in `knowledge_entries`. Run `investigenie-api migrate` when upgrading to 0.4.0-alpha before starting the updated server; older filesystem snapshots add an empty knowledge collection compatibly.
- Back up the snapshot or database together with the corresponding encryption key and key ID. A different or lost key makes saved objects unreadable. Preview/test instances may use separate directories and do not merge their knowledge into another instance automatically.
- Knowledge is server-side, not browser storage. Completing an investigation stores conversation history, not a knowledge entry; the explicit save action creates the separate draft. See the [knowledge lifecycle](investigation-pipeline.md#knowledge-lifecycle-implemented).

Jobs (`InvestigationJob`, `Control`) pause at the user's check-in interval or on an AI timeout and wait for continue or stop. They are kept in memory: at most 64, with finished jobs kept for 30 minutes.

## Configuration

Use `investigenie.toml`, starting from [investigenie.example.toml](../investigenie.example.toml). The API no longer loads `.env`. Selection is explicit `--config <path>` first; otherwise it looks in the working directory, then beside the executable. No discovered file means built-in defaults; a missing explicit file, unreadable/oversized file, malformed TOML, unknown key, or wrong type fails startup without echoing configuration values. Files are limited to 1 MiB. Environment variables override TOML, which overrides defaults. Restart to apply changes.

```toml
[server]
bind = "127.0.0.1:3000"

[logging]
level = "info"
mode = "both"
path = "logs"
max_files = 7

[storage]
path = "data"
```

`path = "logs"` and `path = "data"` create those folders beside the selected TOML file; no `./` prefix is required. Relative subpaths and relative environment path overrides use the same base. Absolute paths are unchanged. Without a file, relative overrides use the working directory. Windows absolute paths can use TOML literal strings, for example `path = 'C:\InvestiGenie\logs'`.

| TOML key | Environment override | Default / notes |
|---|---|---|
| `server.bind` | `RUST_API_BIND` | `127.0.0.1:3000`; Docker overrides to `0.0.0.0:3000`. |
| `server.web_dir` | `WEB_DIR` | Auto-detect `web/` beside the executable; helpers override with built `frontend/dist`. |
| `storage.mode` | `PERSISTENCE_MODE` | Omit for quickstart; explicit `filesystem` or `database` requires an encryption key. |
| `storage.path` | `FILESYSTEM_PATH` | Example: `data`; without any path, quickstart uses `investigenie_data/` beside the executable. Single instance per directory. |
| `storage.database_url` | `DATABASE_URL` | PostgreSQL only; prefer environment injection for credentials and `sslmode=verify-full` outside loopback. |
| `storage.max_connections` | `DB_MAX_CONNECTIONS` | `10`, range 1-1000. |
| `storage.encryption_key` | `CONFIG_ENCRYPTION_KEY` | Base64 of 32 random bytes. Prefer environment injection; preserve the existing key when migrating. |
| `storage.encryption_key_id` | `CONFIG_ENCRYPTION_KEY_ID` | `v1`. |
| `ai.probe_timeout_seconds` | `AI_PROBE_TIMEOUT_SECONDS` | `15`, range 5-600; invalid values fail startup. |
| `logging.level` | `LOG_LEVEL` | `error`, `warn`, `info` (default), or `debug`. |
| `logging.mode` | `LOG_MODE` | `console`, `file`, or `both` (default), independent of level. |
| `logging.path` | `LOG_PATH` | `logs` beside the TOML file; Docker uses writable `/data/logs`. |
| `logging.max_files` | `LOG_MAX_FILES` | `7`, range 1-365; validated when mode writes files. |

`WEB_ORIGIN` is an environment-only setting for split Vite/API development: it sets Vite's host and port and the API's allowed CORS origin. It is not part of the TOML schema. The unified runner serves the UI and API on `server.bind` and sets its own same-origin value.

Quickstart (no persistence mode, encryption key, or database URL) generates a key beside the data. The example uses `data/`; deployments without a file retain the executable-relative default. Keep private TOML files out of Git and Docker build contexts; datasource and AI credentials still belong in the UI/API, not TOML. `VITE_API_BASE_URL` remains an explicit process-environment build-time override, not a backend TOML field. Playground `.env` files configure Compose/seeding and are separate from application configuration.

Run `investigenie-api migrate` after upgrading a PostgreSQL-backed deployment, including the `202610230001_knowledge.sql` migration for knowledge entries. Filesystem snapshots add optional history and knowledge maps compatibly when next written.

## Tests

```sh
cargo test -p investigenie-api
```

Upstreams are mocked with `wiremock`. PostgreSQL tests are `#[ignore]` and need a database.

## Application logs

Set the logging section in `investigenie.toml`, then restart the process:

```toml
[logging]
level = "info"
mode = "both"
path = "logs"
max_files = 7
```

Modes are `console` (terminal only, no log directory), `file` (daily files only), or `both` (default). `logging.path` is a folder, not a filename. With the local project TOML and `path = "logs"`, files are under `investigenie/logs/`, named `investigenie.YYYY-MM-DD.log` (UTC dates). Uncreatable/unwritable file-log paths and invalid settings fail startup with sanitized errors. The `healthcheck` subcommand reads the same config/bind but never opens storage or initializes file logging.

Levels are cumulative: `error` records failures; `warn` adds warnings and rejected client requests; `info` adds startup/shutdown and investigation lifecycle events; `debug` adds detailed request/connector timing, counts, progress, and cancellation metadata. Server/connector failures remain visible at `error`, and request/provider/job context is retained at every level. User-requested stops are informational, not failures. To reproduce an issue, temporarily use `LOG_LEVEL=debug`, then return to `info`. There is one level control, not a separate debug toggle or dependency filter.

Coverage includes request IDs, HTTP method and matched route templates, status/duration, all datasource test/list/field/search/document methods, requested page metadata, returned record and histogram counts, timeout flags, stable error codes, AI provider timing and reported token counts, JSON fallback/repair attempts, investigation progress/decisions, storage startup, and shutdown. Elasticsearch successful-response decoding failures additionally log upstream status, content type, byte count, serde category, and schema path at debug level. Background investigations retain their request/job spans. Saved-object management activity is visible through its API route and result status, not through database wire dumps. Cancelled or interrupted operations are marked even when they do not return a result.

Logging uses an allowlist of application-owned metadata at every level: no authorization headers, cookies, credentials, raw URLs/query strings, datasource query text, document IDs/content, configuration payloads, AI questions/prompts/replies, response bodies, or raw upstream/database errors. Elasticsearch decode diagnostics include only parser metadata and the schema path, never request or response values. Third-party HTTP/SQL events are suppressed even at `debug`. Do not add blanket `Debug` dumps of requests/settings/results. Browser-only rendering/navigation errors are not sent to this server log; use browser developer tools when a request never reaches the API.

Files rotate daily, retaining at most `LOG_MAX_FILES` matching files. This is file-count retention, not a byte limit: a day's file can grow with traffic. A bounded 1,024-event non-blocking queue can drop events under sustained overload rather than stall searches. Graceful shutdown flushes queued output; force-killing the process can lose recent lines. Use debug verbosity temporarily, monitor disk space, protect the log directory, review logs before sharing, and mount it explicitly if container logs must survive replacement. Logs are diagnostic metadata, not an audit trail or encrypted saved-object storage.
