---
name: build-integration
description: 'Use when adding, building, extending, or reviewing InvestiGenie data connectors or AI transports. Covers data_connectors discovery, descriptors, read-only paging/histograms, ai_connectors completion contracts, OpenAI-compatible transport, credentials, resource limits, and tests.'
---

# Build an InvestiGenie Connector

There are two extension points: auto-discovered datasources in `integrations/data_connectors/<id>/` and explicitly registered AI transports in `integrations/ai_connectors/<id>/`. Their shared contracts/registration are backend-owned (`backend/src/data_connectors.rs`, `backend/src/ai_connectors.rs`); integration folders hold concrete implementations only. Do not mix their registries or change saved datasource IDs when moving files. The AI transport workflow is below; datasource rules remain applicable only to data connectors.

Implement a datasource adapter that works through InvestiGenie's shared connector and metadata contracts. This skill is also a checklist for human developers. Paths below are relative to the repository root, `investigenie/`, not its parent workspace.

## Start With Verified Contracts

Contributor setup and repository conventions are in the root [CONTRIBUTING.md](../../../CONTRIBUTING.md), GitHub's contributing overview entry point. Keep it canonical rather than duplicating it under docs/.

Read these files before implementing, because this skill is guidance, not an alternative API definition:

- `integrations/README.md`: descriptor fields and shared response examples.
- `backend/build.rs`: build-time discovery and identifier validation.
- `backend/src/data_connectors.rs`: `IntegrationDescriptor`, `ConnectionConfig`, `Connector`, `ConnectorFuture`, error and credential helpers; use `crate::data_connectors` imports in implementations.
- `backend/src/search.rs`: `SearchRequest` and `TimeRange` validation.
- The closest existing adapter in `integrations/data_connectors/elasticsearch/` or `integrations/data_connectors/clickhouse/`, including its tests.
- `frontend/src/api.ts`, `ConnectionsPage.tsx`, `DataViewsPage.tsx`, and `App.tsx`: consumers of metadata and result shapes.
- `backend/src/ai.rs` when AI investigation support is required.

Choose the native query language, read-only credentials, stable record identity, timestamp semantics, and pagination mechanism first. Identify unsupported capabilities explicitly. Do not claim snapshot consistency or inexpensive deep pagination when the source cannot provide it.

## Register Without Special-Casing the UI

1. Create `integrations/data_connectors/<source_id>/mod.rs`. The ID must be a unique, non-keyword Rust identifier and exactly match the folder and descriptor ID.
2. Export `descriptor() -> IntegrationDescriptor` and `connect(&ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError>`.
3. Implement the `Send + Sync` connector with boxed `Send` futures through `ConnectorFuture`. Keep native client, translation, validation, and tests in the integration folder.
4. Do not manually edit the generated registry. Immediate folders under `data_connectors/` containing `mod.rs` are discovered at compile time; rebuild and restart to register them. `ai_connectors/` is excluded.
5. Existing dependencies are available. New libraries require changes to `backend/Cargo.toml` and the tracked lockfile; integration-local manifests are not loaded.

## Descriptor and Configuration Rules

- Supply `id`, `display_name`, `icon`, `query_language`, `description`, `connection_fields`, and `query` with the exact types expected by the shared contract. `icon` selects a bundled connector brand asset (`elasticsearch`, `clickhouse`, `bigquery`, `loki`, `prometheus`, `victorialogs`, or `victoriametrics`); missing or unsupported values fall back to the generic database icon.
- Include a required, editable, non-secret `url` field under the current UI contract. Validate the native endpoint and configuration in `connect`; do not bypass validation by leaving placeholders.
- Field metadata includes `name`, `label`, `input_type`, `placeholder`, `required`, `secret`, and `editable`. Supported inputs are text, url, password, checkbox, and select. Use existing select options, groups, and `visible_when` patterns.
- Mark API keys, passwords, bearer tokens, and sensitive usernames as secrets. Only advertise replacement with `editable: true` when supported.
- Retain omitted credentials on edits through existing configuration helpers. Do not erase a stored secret because a redacted UI field is blank. Test replacement, conditional clearing, and empty-password semantics when the system permits them.
- Read optional credentials with `credential_setting` / `basic_credentials` from `backend/src/data_connectors.rs`: forms submit untouched secret inputs as empty strings, so blank must mean unset rather than an invalid connection.
- Populate query templates: `placeholder`, `default_query`, `field_query`, `identifier_quote`, `help`, and `schema_label`. Escape substituted identifiers correctly for the source language.
- Set `document_mode` to `lookup` only if document retrieval is implemented; use `inline` for returned rows. Set `sort_requires_projection` and `histogram_total_from_buckets` honestly. New UI behavior requires a shared-contract change, not ad hoc source-ID branches in React.

## Security Restrictions

- Queries must be read-only. Use least-privilege source credentials and enforce server-side read-only modes or permissions. A prefix check such as SELECT/WITH alone is not a complete security boundary: reject multiple statements, mutations, unsafe functions, and source-specific escape routes.
- Validate dataset/index names, sort identifiers, timestamps, operators, page sizes, cursor tokens, endpoint schemes, and query byte lengths before upstream I/O. Use structured APIs, parameterization, and native escaping where possible; never concatenate unvalidated user/model identifiers or values.
- Keep credentials on the API server. Never return or log them, place them in cursor tokens, send them to AI providers, or commit them in fixtures, examples, or local configuration.
- Errors must use sanitized `ConnectorError { status, code, message }`. Do not return raw upstream bodies, URLs with credentials, authorization headers, or native diagnostic strings containing secrets. Preserve useful status/code distinctions without leaking details.
- Logging is shared: generated factories wrap every `Connector` method and default AI calls are observed. TOML `logging.level` controls error/warn/info/debug detail; `logging.mode` selects console/file/both; `logging.path` is relative to the config file unless absolute. Environment overrides are LOG_LEVEL/LOG_MODE/LOG_PATH. Failures must remain visible below debug with safe correlation. Keep fields scalar/allowlisted; never log queries, index/document names, settings, credentials, raw errors, prompts, replies, or wire dumps. Extend redaction regressions when adding diagnostics.
- Configure TLS verification by default; any insecure option must be explicit. Do not silently follow credential-bearing redirects to another host. Document endpoint trust and private-network access: InvestiGenie currently does not provide a general SSRF isolation boundary.
- Do not implement custom encryption or persistence for secrets. Use the existing encrypted saved-object storage, including revision checks and both persistence modes. Preserve `If-Match` conflict semantics.
- The current API has no built-in user authentication or datasource authorization. New adapters must not be described as adding those guarantees; deployments still need a trusted authenticated gateway.

## Connector Behavior and Response Shape

- `test_connection`: test actual endpoint/auth/access before save, using a minimal non-mutating request. Respect the API's bounded probe timeout.
- `indices`: list datasets as `IndexSummary`, with optional document-count and health strings. Avoid an unbounded scan of every record to enumerate datasets.
- `fields`: expose selectable leaf fields with native type strings and accurate `is_time_field`. Do not label arbitrary strings/numbers as timestamps without defined conversion semantics. Data Views require a valid time field.
- `search`: honor query, dataset, sorting, pagination, time filtering, and histogram options. Filtering and sorting happen before pagination.
- `document`: for lookup return `{index,id,found,source}`; missing documents use `found:false` and `source:null`. Inline adapters may reject lookup consistently.
- Search results contain `took`, `timed_out`, `total`, `total_relation`, `hits`, and `histogram`. Hits have string `index`/`id`, nullable numeric `score`, and object/null `source`. Preserve JSON numbers, booleans, arrays, nesting, and explicit-offset date strings.
- IDs must identify records consistently across pages and support every advertised lookup capability. Position-based SQL IDs are only presentation IDs, not persistent event identities; document that limitation.
- `total_relation: eq` means exact count; `gte` means a lower bound. Never report page length as an exact total of a larger query. Prefer an honest lower bound over an expensive count on every page.
- Supply `columns` when projected columns determine valid sorting. Implement every advertised sort and deterministic tie-breaking, including equal timestamps.

### BigQuery datasource notes

- Connections target one explicit billing project and dataset, use the Google BigQuery API root, and accept a service-account JSON key only after validating its OAuth token URI against Google's token endpoints. The key is secret configuration; warn users that user-managed service-account keys are high-risk and recommend keyless identity where available.
- Grant BigQuery Job User on the billing project and dataset-level read-only access. The dataset setting scopes discovery/schema, not arbitrary custom SQL; the service account's IAM grants determine every resource a query may read.
- Accept only one parsed BigQuery Standard SQL query statement. Set `maximumBytesBilled` to 10 GiB on every query job, including histogram jobs. Treat it as a per-job cap, not a total request budget.
- Use BigQuery's `pageToken` for continuation, return exact job totals, cap pages at 100 rows and responses at 16 MiB, and keep cursors bound to the same job/query. IDs are job-relative presentation IDs; do not advertise document lookup.
- Time filters and histograms require a schema-confirmed DATE, DATETIME, or TIMESTAMP field. Histogram failure must not discard successful query rows; All time uses automatic interval selection from bounded extents.

## Pagination and Resource Restrictions

- At the native connector boundary, `cursor: null` or omission is legacy offset mode, `cursor: ""` starts paging, and a nonempty native cursor continues it. Native `next_cursor` describes source continuation.
- The public API wraps native cursors with shared `SearchWindow` handling: maximum 50 records per response, 100 per window, offsets 0-99 only. It stops exposing continuation at the cap without modifying total counts, `total_relation`, or histograms. An absent public cursor can mean the cap rather than source exhaustion. Old public cursors require restarting the search; graph clicks/brushes start a fresh capped window. Internal AI evidence calls bypass this public wrapper and keep their smaller bounds.
- Prefer native snapshot/keyset cursors. Elasticsearch uses PIT plus `search_after`; ClickHouse uses offsets, which are not snapshot-consistent. Native deep-page capability is not a promise of public deep-page access. Never limit the aggregation dataset to 100 just because the public result window is capped.
- Keep continuation query, sort, dataset, time range, and page size consistent. Validate token format, length, numeric ranges, and cursor progression. Tokens must not allow arbitrary native queries or disclose credentials.
- Bound each page to at most 100 records under the current adapters' contract. Enforce query-size limits (existing adapters use 8,192 bytes), connection/request/search timeouts, response sizes, execution time, memory, and source-specific scan/sort limits. Shared API concurrency limits are not sufficient by themselves.
- Fetch sequentially or with explicitly bounded concurrency. Never load an entire source into memory. Support cancellation where the client/runtime allows it, and release upstream resources on exhaustion or errors.
- Snapshot cursors need bounded lifetimes and cleanup. Report expired/invalid cursors as recoverable errors, not empty successful pages. Do not claim complete coverage after shard failures, partial source responses, timeouts, or source pagination errors.
- Exact full pages may require one final empty request to establish exhaustion. Test it. Offset-based cursors may shift under concurrent writes; document that and do not promise exactly-once scans.
- `include_histogram: false` suppresses aggregation. Subsequent cursor pages must not rebuild the histogram; Explore retains the first-page aggregation and AI opts out of histograms.
- Elasticsearch currently requests exact totals (`track_total_hits: true`); 10,000 is not a multi-index total or scan cap. Preserve `total_relation`, and label lower bounds with `+` in every UI count.
- Built-in ClickHouse connections omit `max_execution_time` when blank/null/absent; otherwise validate 1-600 seconds. HTTP timeout is 15s when unset or configured seconds + 5s. Do not reintroduce fixed memory/result/overflow URL settings that read-only profiles reject; server profiles own memory limits and SQL LIMITs bound returned rows.

## Timeframe and Histogram Rules

- `time_range` uses inclusive UTC epoch milliseconds. Validate ordered timestamps with `TimeRange::is_valid()`. Apply the configured `histogram_field` consistently to record filtering and histogram aggregation.
- `None` means all time; do not add an undocumented last-hour/day restriction. Preserve timestamps as instants regardless of display timezone.
- Explore's initial window is last 15 minutes; AI defaults to last 30 minutes. Both allow explicit All time. Defaults belong to the caller, not hidden connector filtering.
- Elasticsearch Data Views support wildcards and comma-separated indices; validate fields from the pattern mapping response rather than requiring exact index-list membership. Searches read `_field_caps` for the time and sort fields to scope mixed-mapping patterns to date-typed indices and to add `unmapped_type`/`numeric_type` sort fallbacks. PIT and offset searches reject partial shards (a timed-out page returns rows without a cursor) and release snapshots on errors/exhaustion.
- Metrics sources must not cap totals by fetched rows: count server-side (Prometheus `count` at the query step, VictoriaMetrics `count_over_time` on epoch-aligned cells) and fetch only the time window holding the requested page. Sample IDs must not depend on fetch position, so windowed pages sort identically.
- All time breakdowns work without an explicit client interval: reuse `HistogramInterval::automatic(span_millis)` from `backend/src/search.rs` for the shared bucket selection. Elasticsearch resolves min/max on the same PIT (including rotated IDs); ClickHouse resolves from its automatic histogram. Never duplicate the interval candidate list or resolve independent intervals per series.
- Aggregate counts across all matching records, not the returned page, using exactly the same query and time filters. Keep bucket counts nonnegative and UTC millisecond timestamps; use bounded bucket counts and native aggregation where possible.
- A sparse or empty selected window must still display its full start-to-end axis in Explore. Do not make the visible range depend only on returned events. Pass the applied search timeframe to the shared chart, including empty results; do not reset it on page navigation or unsubmitted form edits.
- If the source cannot produce a histogram, return `histogram: []` and an explicit `histogram_unavailable_reason`. Do not invent counts or enable `histogram_total_from_buckets` unless bucket sums genuinely represent all matches.
- Reuse shared field validation (`search::safe_field_name`) and automatic intervals rather than copying them. ClickHouse uses `decode_histogram_rows` for both automatic/fixed buckets; preserve its malformed-count/timestamp failures when extending it. Share real behavior, not merely source-specific SQL syntax.
- ClickHouse histogram failures preserve successfully fetched document rows with a sanitized unavailable reason; a row-query failure remains an error.

## AI Compatibility Is Separate

### AI transport workflow

- Read `backend/src/ai_connectors.rs` and `integrations/ai_connectors/openai/mod.rs` before modifying provider communication. OpenAI-compatible means the implemented Chat Completions protocol, not every OpenAI API endpoint.
- Implement `AiConnector::complete` in `integrations/ai_connectors/<id>/mod.rs` with the shared `AiSettings`, `ProviderReply`, `ModelUsage`, `AiConnectorError`, and boxed Send future. Explicitly declare/register the transport in `backend/src/ai_connectors.rs`, using a path to the concrete implementation. There is currently one `default_connector()` (OpenAI), not auto-discovery or a runtime UI selector.
- Keep bearer/provider authentication, wire formats, HTTP timeout, bounded request/response handling, actual usage decoding, and protocol fallback inside the transport. Keep it independent of backend `ai` and storage modules.
- Backend `ai.rs` retains metadata planning, bounded evidence queries, hypothesis generation, JSON extraction/repair, evidence validation, jobs/control, model probe semantics, and history-context composition. Call the shared default/interface rather than importing concrete provider modules there.
- Preserve OpenAI transport's 65,536-byte prompt and 32,768-byte completion caps, bounded private error preview (4,096 bytes), explicit unsupported-JSON-mode retry, refusal/truncation handling, and sanitized errors. Never print credentials/provider bodies or retry authentication failures as format failures.
- Add connector-local wiremock tests for limits/protocol failures and retain orchestration/API regressions for JSON repair, cancellation/time budgets, model probes, and follow-up context. Do not change saved model schema or add speculative harness fields as part of a folder move.

### Datasource investigation support

Build-time registration makes a source available to Explore, not automatically to AI. `backend/src/ai.rs` currently translates filters explicitly for Elasticsearch and ClickHouse. `query_language` in the descriptor is display metadata, not an executable translator.

- To support AI, add the source-specific safe filter translation and allowed operators in the owning AI code, or extend the shared abstraction deliberately. Reject unsupported AI integrations rather than falling back to another language.
- Validate every model-selected field against the actual schema, restrict operators and scalar values, and reject arbitrary model SQL, scripts, network access, or credentials.
- AI evidence gathering is not paginated: for each selected Data View the backend runs at most three bounded queries (latest samples, earliest samples when more matches are indicated, and same-time-scope unfiltered fallback if filters return no rows). Each query returns at most four documents and at most eight unique rows are retained per view. The first query may include a source-side histogram/top-value aggregation over a wider result set. Only at most four selected fields (256 bytes each) and 16 aggregate buckets are sent to hypothesis generation. Findings must cite retained sample IDs; counts derive from distinct citations, never aggregate totals. Do not add a full-record scan or claim bounded evidence is exhaustive or statistically representative.
- Evidence IDs must come from sampled documents; counts must be supported by aggregations or those samples. Treat source content as untrusted data, not instructions. Do not infer causation from correlation or claim model findings are verified facts.
- Preserve actual provider usage accounting. Missing usage is not reported, never estimated. More distinct signals can increase cost; batching does not promise fixed total price/runtime.
- Investigations run as in-memory background jobs (the legacy synchronous route remains). Encrypted conversation history persists in filesystem/PostgreSQL, but that is not durable scan-job resumption. Follow-ups use saved transcript/results without rescanning sources; writes/deletes require If-Match. See `docs/backend.md` for caps, routes, and migrations.
- OpenAI transport retries explicit JSON-mode rejection once without `response_format`; backend orchestration repairs unusable JSON once. Missing/null record fields are normal, not a reason to abort analysis. Keep provider diagnostics sanitized and evidence validation intact.
- `docs/investigation-pipeline.md` describes metadata/evidence/hypothesis boundaries and the implemented knowledge lifecycle. Manual encrypted CRUD and explicit save-as-draft from completed live/history results are implemented; publication is not verification. Automatic creation, retrieval, and immutable investigation knowledge citations remain future work; do not imply datasource registration enables any of them.

## Required Tests and Completion Checklist

1. Unit tests: endpoint/config validation, identifier/value escaping, read-only restrictions, field types, operators, invalid timestamps, excessive page/query sizes, malformed cursors, and sanitized failures.
2. Mock-upstream tests: authentication stays server-side; test/list/schema/search/document shapes; empty/one-record results; stable sorts with ties; time filters; full-query histograms; projected columns; not-found/partial/timeout errors.
3. Paging tests: native first/continuation requests, advancement, exhaustion, full final page, duplicate timestamps, supported deep offsets, expiration/cleanup, and concurrent-write limitations. Public API tests must preserve the shared 50-per-page/100-total window, reject out-of-window offsets/cursors, preserve lower-bound/exact totals and full-scope histograms, and reset on a graph-selected range.
4. Security/storage tests: summaries and errors omit secrets, edits retain/replace credentials, stale `If-Match` operations fail, saved objects survive restart, and both PostgreSQL and encrypted-filesystem storage are preserved. Never ignore `backend/migrations/`.
5. If AI-compatible: test source-specific plan translation, bounded sample/aggregate evidence, supported counts, bounded prompts, unsupported operators, failure propagation, and real usage aggregation. Do not imply the AI evidence path paginates through the dataset.
6. Validate from the repository root:

   ```sh
   cargo test --locked -p investigenie-api
   npm --workspace frontend run test -- --maxWorkers=1
   npm run build:web
   ```

   Database-dependent tests need an isolated `TEST_DATABASE_URL` and `CONFIG_ENCRYPTION_KEY`; report skipped tests rather than claiming they passed.
7. Verify discovery through `GET /api/integrations`, then Connections, Data Views, Explore sorting/timezone/paging/JSON inspection, and AI if supported. Test full-window histograms with zero, one, and many records on desktop/mobile. Do not run tests against production sources or spend provider tokens without authorization.
8. Update relevant docs, this skill, agent instructions, and context references whenever behavior/contracts/tooling change. Remove stale claims in the same task. Include native prerequisites, permissions, supported query operators, timeout/scan limits, pagination consistency, and limitations. Keep private configuration and generated artifacts out of Git; inspect directory junction targets before deleting local caches.

Before finishing, state what works in Explore, what works in AI, which guarantees are tested, and which limitations remain. Do not solve adapter limitations with hidden frontend exceptions or unbounded retries.
