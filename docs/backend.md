# Backend

Rust crate `investigenie-api` in `backend/` (Axum + Tokio). One binary: `investigenie-api [migrate | healthcheck]`.

## Modules (`backend/src`)

| File | What it does |
|---|---|
| `main.rs` | Startup, the `migrate` and `healthcheck` commands, serving the UI, graceful shutdown; declares backend-owned data/AI contract modules. |
| `config.rs` | Reads environment settings; picks quickstart storage when nothing is configured. |
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
| `GET, POST /api/knowledge` | List `{entries}` or create `{entry}` (201). |
| `GET, PUT, DELETE /api/knowledge/{id}` | Read/update `{entry}` or delete (204); updates/deletes require quoted `If-Match`. |
| `/api/ai/models[/{id}]` | AI model profiles; tested before save; the key is never returned. |
| `POST /api/ai/investigations` | Start a background investigation (202). |
| `GET /api/ai/investigations/{id}` | Job status, progress, partial or final result. |
| `POST /api/ai/investigations/{id}/decision` | `{"action":"continue"\|"terminate"}`. |
| `POST /api/ai/investigate` | Synchronous investigation (legacy). |

## Search and histogram

Explore Elasticsearch searches request exact matching totals across all selected indices rather than stopping the count at 10,000. Exact counting may cost more on large datasets. AI evidence searches disable exact hit counting and use histogram/top-value aggregates instead. All time breakdowns resolve a common bucket interval before calculating top-value series; Elasticsearch uses a min/max timestamp preflight on the same PIT when paging. The time scope remains unbounded.

Both connectors use `HistogramInterval::automatic(span_millis)` in `search.rs` for automatic breakdown intervals. Keep the candidate intervals and target bucket count in that helper, not separate connector-specific lists. Elasticsearch tolerates unmapped time fields when sorting multi-index patterns; ClickHouse histogram failures preserve valid rows and report an unavailable reason. SQL `LIMIT` bounds returned rows, while ClickHouse server profiles control memory limits; fixed memory/result/overflow URL overrides are not sent.

`SearchRequest` fields: `index`, `query`, `size` (≤100), `cursor` (`""` starts a paged scan), `sort_by`/`sort_order`, `histogram_field`, `time_range` (inclusive epoch ms), `histogram_interval` (`30s`, `5m`, `1d`, `1w`, `1M`, `1y`; requires a time range, ≤1000 buckets), `histogram_breakdown_field` (top 3 values + `__other__`), `time_zone`.

## AI conversation history

AI history routes: `GET /api/ai/conversations` lists summaries; `GET /api/ai/conversations/{id}` reads a conversation; `POST /api/ai/conversations/{id}/messages` adds a follow-up; `DELETE /api/ai/conversations/{id}` deletes it. Follow-ups and deletes require the current quoted `If-Match` revision.

Conversation messages and investigation results are persisted in the encrypted filesystem snapshot or as encrypted PostgreSQL payloads. They are bounded to 64 conversations, 200 messages per conversation, and 128 KiB per conversation to fit the filesystem snapshot limit. When the history is full, starting a new investigation evicts the least recently updated conversation (both storage modes), so the history behaves like a rolling window instead of rejecting new work. Follow-ups use the stored transcript and result; they do not rescan sources. The app has no user authentication, so history is shared by anyone who can access the instance.

## AI investigations (`ai.rs`)

If a provider explicitly rejects JSON response mode, the request is retried without that option; the model is still instructed to return the required JSON shape. Elasticsearch query-string searches tolerate incompatible field mappings across matched indices, and missing fields in individual records are treated as normal.

The provider-specific HTTP implementation lives only in `integrations/ai_connectors/openai/`. Its prompt and completion limits remain 65,536 and 32,768 bytes, with a bounded 4,096-byte error preview used privately for fallback detection. `AiConnectorError` maps to existing investigation error codes. Backend orchestration keeps repair retries, time-budget pauses/cancellation, evidence checks, and history ownership; it does not depend on a concrete provider implementation. Existing saved AI models continue using the default OpenAI-compatible transport with no migration.

Both shared connector contracts and registrations live in the backend, while `integrations/data_connectors/<id>/` and `integrations/ai_connectors/<id>/` contain concrete implementations. `backend/build.rs` scans only immediate datasource directories and generates `OUT_DIR/data_connectors.rs`, included by the backend data contract. There are no old-module forwarding wrappers or shared contract files under `integrations/`.

API helpers `model_settings`, `prepare_ai_call`, and `probe_ai_model` share model settings, capacity acquisition, and provider probes across synchronous/background investigations, follow-ups, and model saves. Keep validation/revision checks before those operations; model saves still probe before persistence. `ai_model_save_error` preserves the existing duplicate-model response. Both conversation creation and updates reuse `conversation_timestamp`; no storage schema changes are involved.

1. **Plan evidence:** the AI receives the question, selected Data View metadata (field names and types), and time scope. It proposes validated filters, up to four sample fields, and an optional aggregation field; invalid choices are dropped with a caveat.
2. **Gather evidence:** the backend sends one bounded search request per selected Data View for at most four records plus a time histogram; adapters may issue additional database-side aggregation/preflight queries. Results can include a top-three-plus-`__other__` breakdown. Only four selected fields per record are retained, each capped at 256 bytes; the hypothesis payload keeps at most 16 aggregate buckets. The engine does not page through every matching record.
3. **Form hypotheses:** the AI receives aggregate summaries and bounded sample evidence, then returns supported findings. Evidence IDs are validated against the supplied samples, and reported counts are capped by available aggregates or sample counts.
4. **Clean the output:** model replies are parsed leniently, retried once if unusable, and unsupported evidence is removed.
5. **Knowledge retrieval (future):** manual knowledge storage is implemented. The UI can explicitly copy a completed investigation into an unverified draft through the existing knowledge create route; investigations do not automatically create entries or retrieve knowledge. Future retrieval must treat published entries as candidate guidance, recheck current bounded evidence, and retain cited revision/excerpt snapshots. See the [investigation pipeline plan](investigation-pipeline.md).

## Knowledge management

Entries contain `title`, `kind` (`service_context`, `runbook`, `resolved_incident`), `status` (`draft`, `published`, `archived`), Markdown `content`, `services`, `environments`, `connection_ids`, `data_view_ids`, `tags`, and an optional `source_url`. Empty scope arrays mean global applicability. Scope identifiers are retained when their source is deleted; the UI labels unavailable references rather than silently broadening the scope. IDs, revisions, and created/updated timestamps are server-managed and are not accepted in mutation bodies.

Both modes encrypt the complete entry, not just its content. PostgreSQL stores each encrypted payload in `knowledge_entries`; filesystem mode adds a default-empty map to existing snapshots. Updates and deletes use atomic revision checks; stale writes return 409 without retry. There is no stored revision-history browser in this milestone.

Limits: 128 entries, nonblank titles up to 256 bytes, nonblank content up to 32,768 bytes, up to 20 items per scope/tag array (labels up to 128 bytes), source links up to 2,048 bytes, and API mutation bodies up to 65,536 bytes. Source links permit only HTTP/HTTPS without embedded credentials. Search/filtering is local to the bounded UI collection; no search index or embeddings are persisted.

Publishing indicates editorial availability, not independent verification. Completed investigation results can be explicitly saved as drafts and reviewed through the Knowledge editor; this uses existing CRUD and does not change the storage schema. Automatic creation/publishing, automatic retrieval, immutable investigation knowledge citations, and feedback promotion are not implemented yet. The instance has no login or tenancy; anyone with access can read and modify knowledge, so protect it with the same authenticated gateway as the rest of the application.

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
| `AI_PROBE_TIMEOUT_SECONDS` | `15` | Timeout (5-600) for the test call made when an AI model is saved. Raise it for slow local models. |

Quickstart (none of `PERSISTENCE_MODE`, `CONFIG_ENCRYPTION_KEY`, `DATABASE_URL` set) stores a generated key beside the data. That suits local use only.

Run `investigenie-api migrate` after upgrading a PostgreSQL-backed deployment, including the `202610230001_knowledge.sql` migration for knowledge entries. Filesystem snapshots add optional history and knowledge maps compatibly when next written.

## Tests

```sh
cargo test -p investigenie-api
```

Upstreams are mocked with `wiremock`. PostgreSQL tests are `#[ignore]` and need a database.
