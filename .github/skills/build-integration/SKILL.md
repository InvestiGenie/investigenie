---
name: build-integration
description: 'Use when adding, building, extending, or reviewing a Northstar datasource integration or connector for another system. Covers discovery, descriptor metadata, credentials, read-only querying, cursor pagination, timeframe-aware histograms, AI compatibility, resource limits, and required tests.'
---

# Build a Northstar Integration

Implement a datasource adapter that works through Northstar's shared connector and metadata contracts. This skill is also a checklist for human developers. Paths below are relative to the repository root, `northstar/`, not its parent workspace.

## Start With Verified Contracts

Read these files before implementing, because this skill is guidance, not an alternative API definition:

- `integrations/README.md`: descriptor fields and shared response examples.
- `backend/build.rs`: build-time discovery and identifier validation.
- `backend/src/integrations.rs`: `IntegrationDescriptor`, `ConnectionConfig`, `Connector`, `ConnectorFuture`, error and credential helpers.
- `backend/src/search.rs`: `SearchRequest` and `TimeRange` validation.
- The closest existing adapter in `integrations/elasticsearch/` or `integrations/clickhouse/`, including its tests.
- `frontend/src/api.ts`, `ConnectionsPage.tsx`, `DataViewsPage.tsx`, and `App.tsx`: consumers of metadata and result shapes.
- `backend/src/ai.rs` when AI investigation support is required.

Choose the native query language, read-only credentials, stable record identity, timestamp semantics, and pagination mechanism first. Identify unsupported capabilities explicitly. Do not claim snapshot consistency or inexpensive deep pagination when the source cannot provide it.

## Register Without Special-Casing the UI

1. Create `integrations/<source_id>/mod.rs`. The ID must be a unique, non-keyword Rust identifier and exactly match the folder and descriptor ID.
2. Export `descriptor() -> IntegrationDescriptor` and `connect(&ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError>`.
3. Implement the `Send + Sync` connector with boxed `Send` futures through `ConnectorFuture`. Keep native client, translation, validation, and tests in the integration folder.
4. Do not manually edit the generated registry. Immediate integration folders containing `mod.rs` are discovered at compile time; rebuild and restart to register them.
5. Existing dependencies are available. New libraries require changes to `backend/Cargo.toml` and the tracked lockfile; integration-local manifests are not loaded.

## Descriptor and Configuration Rules

- Supply `id`, `display_name`, `query_language`, `description`, `connection_fields`, and `query` with the exact types expected by the shared contract.
- Include a required, editable, non-secret `url` field under the current UI contract. Validate the native endpoint and configuration in `connect`; do not bypass validation by leaving placeholders.
- Field metadata includes `name`, `label`, `input_type`, `placeholder`, `required`, `secret`, and `editable`. Supported inputs are text, url, password, checkbox, and select. Use existing select options, groups, and `visible_when` patterns.
- Mark API keys, passwords, bearer tokens, and sensitive usernames as secrets. Only advertise replacement with `editable: true` when supported.
- Retain omitted credentials on edits through existing configuration helpers. Do not erase a stored secret because a redacted UI field is blank. Test replacement, conditional clearing, and empty-password semantics when the system permits them.
- Populate query templates: `placeholder`, `default_query`, `field_query`, `identifier_quote`, `help`, and `schema_label`. Escape substituted identifiers correctly for the source language.
- Set `document_mode` to `lookup` only if document retrieval is implemented; use `inline` for returned rows. Set `sort_requires_projection` and `histogram_total_from_buckets` honestly. New UI behavior requires a shared-contract change, not ad hoc source-ID branches in React.

## Security Restrictions

- Queries must be read-only. Use least-privilege source credentials and enforce server-side read-only modes or permissions. A prefix check such as SELECT/WITH alone is not a complete security boundary: reject multiple statements, mutations, unsafe functions, and source-specific escape routes.
- Validate dataset/index names, sort identifiers, timestamps, operators, page sizes, cursor tokens, endpoint schemes, and query byte lengths before upstream I/O. Use structured APIs, parameterization, and native escaping where possible; never concatenate unvalidated user/model identifiers or values.
- Keep credentials on the API server. Never return or log them, place them in cursor tokens, send them to AI providers, or commit them in fixtures, examples, or local configuration.
- Errors must use sanitized `ConnectorError { status, code, message }`. Do not return raw upstream bodies, URLs with credentials, authorization headers, or native diagnostic strings containing secrets. Preserve useful status/code distinctions without leaking details.
- Configure TLS verification by default; any insecure option must be explicit. Do not silently follow credential-bearing redirects to another host. Document endpoint trust and private-network access: Northstar currently does not provide a general SSRF isolation boundary.
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

## Pagination and Resource Restrictions

- `cursor: null` or omission is legacy offset mode. `cursor: ""` starts a paginated scan; a nonempty cursor continues it. Responses return `next_cursor` only when continuation is possible; absence/null means exhausted.
- Do not inherit the legacy 10,000-offset ceiling as an overall data-access cap. Prefer native snapshot/keyset cursors. The Elasticsearch implementation uses PIT plus `search_after`; ClickHouse currently uses deterministic numeric offsets, which remain expensive at depth and are not snapshot-consistent.
- Keep continuation query, sort, dataset, time range, and page size consistent. Validate token format, length, numeric ranges, and cursor progression. Tokens must not allow arbitrary native queries or disclose credentials.
- Bound each page to at most 100 records under the current adapters' contract. Enforce query-size limits (existing adapters use 8,192 bytes), connection/request/search timeouts, response sizes, execution time, memory, and source-specific scan/sort limits. Shared API concurrency limits are not sufficient by themselves.
- Fetch sequentially or with explicitly bounded concurrency. Never load an entire source into memory. Support cancellation where the client/runtime allows it, and release upstream resources on exhaustion or errors.
- Snapshot cursors need bounded lifetimes and cleanup. Report expired/invalid cursors as recoverable errors, not empty successful pages. Do not claim complete coverage after shard failures, partial source responses, timeouts, or source pagination errors.
- Exact full pages may require one final empty request to establish exhaustion. Test it. Offset-based cursors may shift under concurrent writes; document that and do not promise exactly-once scans.
- `include_histogram: false` suppresses aggregation. Subsequent cursor pages must not rebuild the histogram; Explore retains the first-page aggregation and AI opts out of histograms.

## Timeframe and Histogram Rules

- `time_range` uses inclusive UTC epoch milliseconds. Validate ordered timestamps with `TimeRange::is_valid()`. Apply the configured `histogram_field` consistently to record filtering and histogram aggregation.
- `None` means all time; do not add an undocumented last-hour/day restriction. Preserve timestamps as instants regardless of display timezone.
- Aggregate counts across all matching records, not the returned page, using exactly the same query and time filters. Keep bucket counts nonnegative and UTC millisecond timestamps; use bounded bucket counts and native aggregation where possible.
- A sparse or empty selected window must still display its full start-to-end axis in Explore. Do not make the visible range depend only on returned events. Pass the applied search timeframe to the shared chart, including empty results; do not reset it on page navigation or unsubmitted form edits.
- If the source cannot produce a histogram, return `histogram: []` and an explicit `histogram_unavailable_reason`. Do not invent counts or enable `histogram_total_from_buckets` unless bucket sums genuinely represent all matches.

## AI Compatibility Is Separate

Build-time registration makes a source available to Explore, not automatically to AI. `backend/src/ai.rs` currently translates filters explicitly for Elasticsearch and ClickHouse. `query_language` in the descriptor is display metadata, not an executable translator.

- To support AI, add the source-specific safe filter translation and allowed operators in the owning AI code, or extend the shared abstraction deliberately. Reject unsupported AI integrations rather than falling back to another language.
- Validate every model-selected field against the actual schema, restrict operators and scalar values, and reject arbitrary model SQL, scripts, network access, or credentials.
- Implement usable continuation pages: AI scans sequential 100-record batches, deduplicates repeated signals across pages, and sends bounded evidence buffers to the configured provider. There is no overall 50-record/20-example sampling cap; never introduce hidden truncation while claiming a complete scan.
- Evidence includes representative IDs and observed counts; the output remains a bounded rolling summary, not every event. Do not treat source content as instructions. Do not infer causation from correlation or claim model findings are verified facts.
- Preserve actual provider usage accounting. Missing usage is not reported, never estimated. More distinct signals can increase cost; batching does not promise fixed total price/runtime.
- Document that investigations remain synchronous without durable background-job resume, and that source evidence is shared with the selected provider. Adding a connector does not add field-level redaction or user identity.

## Required Tests and Completion Checklist

1. Unit tests: endpoint/config validation, identifier/value escaping, read-only restrictions, field types, operators, invalid timestamps, excessive page/query sizes, malformed cursors, and sanitized failures.
2. Mock-upstream tests: authentication stays server-side; test/list/schema/search/document shapes; empty/one-record results; stable sorts with ties; time filters; full-query histograms; projected columns; not-found/partial/timeout errors.
3. Paging tests: first and continuation requests, more than one page, advancement, exhaustion, full final page, duplicate timestamps, deep pages beyond legacy offsets, lower-bound/exact totals, no repeated aggregation, expiration/cleanup, and concurrent-write limitations.
4. Security/storage tests: summaries and errors omit secrets, edits retain/replace credentials, stale `If-Match` operations fail, saved objects survive restart, and both PostgreSQL and encrypted-filesystem storage are preserved. Never ignore `backend/migrations/`.
5. If AI-compatible: test source-specific plan translation, later-page evidence, repeated-signal counts, bounded prompts, unsupported operators, failure propagation, and real usage aggregation.
6. Validate from the repository root:

   ```sh
   cargo test --locked -p discover-api
   npm run test:web -- --maxWorkers=1
   npm run build:web
   ```

   Database-dependent tests need an isolated `TEST_DATABASE_URL` and `CONFIG_ENCRYPTION_KEY`; report skipped tests rather than claiming they passed.
7. Verify discovery through `GET /api/integrations`, then Connections, Data Views, Explore sorting/timezone/paging/JSON inspection, and AI if supported. Test full-window histograms with zero, one, and many records on desktop/mobile. Do not run tests against production sources or spend provider tokens without authorization.
8. Update `integrations/README.md` or source-specific docs with native prerequisites, permissions, supported query operators, timeout/scan limits, pagination consistency, and known unsupported features. Keep private configuration and generated artifacts out of Git.

Before finishing, state what works in Explore, what works in AI, which guarantees are tested, and which limitations remain. Do not solve adapter limitations with hidden frontend exceptions or unbounded retries.