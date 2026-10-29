# Integrations

Connectors are Rust modules compiled into the server, separated into datasource adapters and AI transports:

```text
integrations/
   data_connectors/
      elasticsearch/
      clickhouse/
      bigquery/
      victorialogs/
      victoriametrics/
      prometheus/
      loki/
   ai_connectors/
      openai/
```

Shared contracts and registration live in `backend/src/data_connectors.rs` and `backend/src/ai_connectors.rs`. The integration folders contain concrete implementations, each with its own `mod.rs` entry point.

Data connectors provide connection metadata, schema, read-only search, paging, and histograms. Their forms and Explore/Data Views behavior are metadata-driven. AI evidence gathering also needs an investigation dialect: an `investigation.rs` file beside the connector's `mod.rs` that implements `InvestigationDialect` from `backend/src/investigation_dialect.rs` (currently Elasticsearch, ClickHouse, Loki, and Mimir). Adding a datasource does not add one automatically. `backend/build.rs` discovers the file and derives the descriptor's `ai_investigation` flag from it. The AI page disables Data Views from other sources, and investigations that include them fail with `unsupported_integration` before any source or model call.

Built in: `elasticsearch` (Query DSL / Lucene), `clickhouse` (SQL `WHERE` clauses, read-only), `bigquery` (Standard SQL, read-only), `victorialogs` (LogsQL, read-only), `victoriametrics` (raw metrics selectors, read-only), `prometheus` (PromQL range queries, read-only), `mimir` (PromQL range queries through Mimir's Prometheus API, read-only), and `loki` (LogQL, read-only).

Optional credential inputs left empty mean "not configured": an empty username means no authentication, so never type `""` into a field. Prometheus, VictoriaMetrics, VictoriaLogs, and ClickHouse accept a username with an empty password; a password without a username is rejected. Loki ignores credential inputs that belong to an unselected authentication mode.

Loki and Mimir share one authentication module (`backend/src/connector_auth.rs`) with four modes: none, Basic, bearer token, and OAuth 2.0 client credentials. Basic accepts a username with an empty password; inputs for unselected modes are ignored. OAuth 2.0 needs a token URL, client ID, and client secret, with optional space-separated scopes and an audience (needed by some providers, such as Auth0). The client authenticates to the token endpoint with an HTTP Basic header (`client_secret_basic`, the default) or in the request body (`client_secret_post`). Access tokens are cached in server memory until 30 seconds before `expires_in` (60 seconds when the provider omits it), keyed by every OAuth setting including the secret, so connections that share a client share a token and a changed secret is verified. A 401 from Loki or Mimir drops the cached token so the next request fetches a new one. Token-endpoint failures return a fixed message (`loki_authentication_failed` / `mimir_authentication_failed`) without upstream text. Client IDs and secrets are stored as encrypted secrets; the token URL, scopes, audience, and client authentication method are shown again when editing.

ClickHouse connections have an optional **Max execution time (seconds)** setting (1–600). Leave it blank to omit ClickHouse's server-side `max_execution_time`; the HTTP client still bounds each request to 15 seconds when unset. A configured limit extends the client timeout by five seconds. SQL `LIMIT` clauses bound returned rows; configure server-side memory limits in the ClickHouse user profile, since the connector does not override them per request.

VictoriaLogs exposes one virtual `logs` dataset and the native `_time` timestamp field. The adapter supports HTTP Basic authentication and `AccountID` / `ProjectID` tenant headers. It returns bounded JSON Lines pages and uses the native hits API for exact totals and histograms. Row IDs are presentation-only, and offset paging is not snapshot-consistent under concurrent ingestion. Histograms currently support UTC only; a non-UTC histogram returns an explicit unavailable reason without discarding log rows. The connector does not yet add VictoriaLogs filter translation to AI investigations.

VictoriaMetrics exposes one virtual `metrics` dataset. Data Views use synthetic `_time` and `_value` fields plus discovered metric labels. Search accepts raw MetricsQL series selectors and returns raw stored samples; it does not evaluate MetricsQL functions or interpolated range-query points. Totals and histograms are exact server-side `count_over_time` counts over the selected range, so they are not limited by row caps. Time-sorted pages export only the time window that holds the requested rows and re-count dense windows on a finer grid; each export is capped at 50,000 samples and 16 MiB. Sorting by a label or `_value` needs the full result and is limited to 50,000 matching samples. Selectors that VictoriaMetrics cannot count fall back to one bounded export. All time first resolves the first and last sample timestamps. Offset paging is not snapshot-consistent while new samples arrive. Label discovery uses VictoriaMetrics' default recent time range and may omit older-only labels. Histograms currently support UTC only. AI filter translation is not implemented.

Prometheus exposes one virtual `metrics` dataset with synthetic `_time` and `_value` fields plus labels discovered from `/api/v1/labels`. Explore evaluates a PromQL range query at the selected histogram step; the rows are evaluated values, not raw samples, and may include lookback-filled points. Totals and histograms come from `count(...)` (or `count by (label)` for breakdowns) at the same step, so they stay exact for any number of series. Time-sorted pages fetch only the steps that hold the requested rows; each fetch is capped at 50,000 samples and 16 MiB. Sorting by a label or `_value` needs every evaluated sample and is limited to 50,000. Expressions that cannot be counted, such as scalars, fall back to one bounded range query. The connection test evaluates `vector(1)`. Pagination re-runs the queries, so it is not snapshot-consistent. Prometheus warnings are treated as incomplete results, and error responses map to curated parse, limit, and timeout messages. AI filter translation is not implemented.

Grafana Loki exposes one virtual `logs` dataset with synthetic `_time` and `_msg` fields plus stream labels discovered from `/loki/api/v1/labels`. Label discovery uses Loki's default recent time window and may omit older-only labels. Basic and bearer credentials are sent for an external authentication proxy; Loki itself does not provide API authorization, and `X-Scope-OrgID` is a tenant selector, not an authorization boundary. Explore queries use Loki's nanosecond `[start,end)` bounds, translating the app's inclusive millisecond end. An empty query or an empty `{}` selector, which Loki itself rejects, resolves to `{service_name=~".+"}`: Loki 3 gives every stream a `service_name` label. If labels exist in the range but none is `service_name` (older Loki), the search fails with `loki_selector_required`. Each search fetches at most 5,000 entries (16 MiB). When more lines match a time-sorted search, rows come from the newest entries (or a forward query for ascending pages), and Loki's metric engine counts the full result with `count_over_time`, so totals stay exact. Histogram buckets come from a step-aligned range query, so a line timestamped exactly on an interior bucket boundary counts in the earlier bucket; buckets still sum to the exact total. Large-result breakdowns need a stream label, and All time cannot be counted; in those cases, or when a count query fails, rows are kept with a lower-bound total and a histogram-unavailable reason. Sorting a large result by any other field still fails with `loki_result_limit_exceeded`. Pagination re-runs the bounded query, so it is not snapshot-consistent. Histograms currently support UTC only. Data View fields also include fields Loki 3 detects in the last 24 hours of logs (`/loki/api/v1/detected_fields`), typed by source and value, for example `json int`, `logfmt duration`, or `metadata string` for structured metadata. A field that both parsers report is treated as JSON, because chaining `| json | logfmt` lets non-matching lines through. Older Loki versions without the endpoint simply list no parsed fields. Parsed fields appear in rows only when the query parses lines, such as `| json`.

AI investigations translate filters to LogQL (`integrations/data_connectors/loki/investigation.rs`): `eq` and `contains` on stream labels become selector matchers (`contains` as an escaped regex), `contains` on `_msg` becomes a `|=` line filter, and `_time` is filtered only by the investigation time range. Parsed fields add their parser stage once (`| json` or `| logfmt`; structured metadata needs none) and a label filter: strings support `eq`/`contains`, `int`/`float` fields support `eq`/`gt`/`gte`/`lt`/`lte` with numeric values, `duration` and `bytes` fields support comparisons with literals such as `1.5s` or `10MiB`, and booleans support `eq`. Loki keeps lines whose value fails a numeric conversion (marked with `__error__`), so any numeric comparison ends with `| __error__=""`. A value that does not fit the field type is dropped from the plan instead of failing the investigation. Empty-value label filters become label-filter stages, because they cannot anchor a selector. Stream labels are the only breakdown fields.

Loki rejects range queries longer than its `max_query_length` (default `30d1h`). When a connection is added or edited, the connection test reads `limits_config.max_query_length` from Loki's `/config` and saves it with the connection as `max_query_length_ms` (0 means unlimited). Searches then narrow longer ranges, including All time, to the newest window Loki accepts and mark the total as a lower bound (`+`). `/config` is an admin endpoint that gateways and hosted Loki often block, and it shows the global default rather than per-tenant overrides, so when the limit is unknown or different, Loki's own rejection (`the query time range exceeds the limit (…, limit: 30d1h)`) supplies it and the search retries once within it. A search that still exceeds it fails with `loki_query_too_long`. Instant count queries are not subject to this limit.

Grafana Mimir reuses the Prometheus connector's PromQL engine (same virtual `metrics` dataset, synthetic `_time` and `_value` fields, server-side counts, and step-bounded pages) against Mimir's Prometheus-compatible API. Use the API URL including its prefix, normally `http://mimir:9009/prometheus`. Mimir adds the shared authentication modes for a gateway in front of it, and an optional tenant sent as `X-Scope-OrgID`; as with Loki, the tenant header selects data and is not an authorization boundary. Errors use Mimir codes and messages (`mimir_*`). AI investigations translate filters to PromQL (`integrations/data_connectors/mimir/investigation.rs`): label `eq`/`contains` become selector matchers, `_value` comparisons (`eq`, `gt`, `gte`, `lt`, `lte`) wrap the selector, such as `({__name__="up"}) == 1`, and a selector whose matchers all accept empty values gets the match-all `__name__!=""` anchor. Labels are the only breakdown fields. Mimir's own query-length limits are disabled by default and are not discovered.

### BigQuery

BigQuery connections require the exact `https://bigquery.googleapis.com/` API URL, a billing project ID, one dataset ID, and a service-account JSON key. The key is stored through InvestiGenie's encrypted connection storage and exchanged for a BigQuery-scoped OAuth token; user-managed service-account keys are high-risk, so prefer workload identity or other keyless credentials where available. Grant the service account BigQuery Job User on the billing project and read-only data access only on the intended dataset. The dataset setting scopes table discovery and schema, but custom SQL can reference any resource that the service account is allowed to read; IAM is the access boundary.

BigQuery accepts one parsed Standard SQL query statement, returns exact job totals, and uses Google-issued query-result page tokens for scan cursors. Pages are limited to 100 rows, SQL to 8 KiB, each response to 16 MiB, table discovery to 5,000 names, and each query job to 10 GiB billed bytes. A first-page histogram uses a separate bounded aggregation job; All time also uses a bounded min/max job, so the billed-bytes ceiling applies to each job, not to the total Explore operation. Job cursors refer to the same query result and may expire with BigQuery's stored job results. Hit IDs use job ID plus row offset and are presentation-only; document lookup and AI SQL translation are not implemented. Date, datetime, and timestamp fields are supported for time filtering and histograms; fixed intervals are UTC-aligned, while calendar buckets use the selected timezone.

## Create one with the AI skill (recommended)

The repo includes the agent skill [`build-integration`](../.github/skills/build-integration/SKILL.md). It is a step-by-step workflow and checklist covering the contract, credentials, read-only access, paging, histograms, AI support, and tests.

1. Open the `investigenie/` folder in VS Code with GitHub Copilot (or another agent that reads `.github/skills`).
2. In agent mode, ask for the integration with the details the agent cannot guess:
   > Use the build-integration skill to add a new datasource. API documentation: [provide URL]. Authentication: [provide method]. Query syntax, timestamp field, paging, and aggregation endpoints: [provide details]. Check existing connectors before creating a new ID.
3. The agent reads the contracts, scaffolds `integrations/data_connectors/<id>/`, and writes the tests.
4. Review the result against the checklist at the end of the skill, then run the checks below.

The skill also works as a checklist when you write an integration by hand.

## Create one by hand

1. **Folder:** `integrations/data_connectors/<id>/mod.rs`. The `<id>` must be a Rust identifier (`my_source`) that matches the descriptor ID. `backend/build.rs` discovers it automatically, so no registry edit is needed. Moving a connector must not change its ID or invalidate saved configurations.
2. **Exports:**
   ```rust
      pub fn descriptor() -> crate::data_connectors::IntegrationDescriptor;
      pub fn connect(config: &crate::data_connectors::ConnectionConfig)
         -> Result<std::sync::Arc<dyn crate::data_connectors::Connector>, crate::data_connectors::ConnectorError>;
   ```
3. **Descriptor:** the display name, the connection fields (types, `secret`, `editable`, `visible_when`, groups), and query metadata (placeholder, templates, quoting, `document_mode`, sorting and histogram capabilities). The UI is built from this.
4. **Connector:** implement `test_connection`, `indices`, `fields`, `search`, and `document`.
5. **Search must honour:** query, inclusive UTC epoch-ms time range, sort, page size (≤100), cursor paging, and the histogram interval, breakdown field, and timezone. Filter and sort before paging. Compute the histogram over all matches, not just the page.
6. **Return the shared shape:** `took`, `timed_out`, `total`, `total_relation`, `hits[{index,id,score,source}]`, and `histogram[{timestamp,label,count}]`.
7. **Stay safe:** read-only credentials and queries, timeouts, size limits, and errors that never include secrets.
8. **Test it:** mock the upstream with `wiremock`. Cover settings, credentials, fields, empty results, sorting, time filters, histogram counts, paging, and errors.

The full field-by-field contract and response examples are in [integrations/README.md](../integrations/README.md). Start from the closest existing data connector, such as [ClickHouse](../integrations/data_connectors/clickhouse/mod.rs) for a read-only SQL source.

## AI transports

The backend-owned [AI connector contract](../backend/src/ai_connectors.rs) defines `AiConnector::complete`, settings, completion content/usage/truncation, and sanitized errors. [OpenAI-compatible transport](../integrations/ai_connectors/openai/mod.rs) implements Chat Completions for OpenAI and compatible endpoints, including bounded HTTP requests and one fallback when a provider explicitly rejects JSON mode. This is not full coverage of every OpenAI API endpoint.

To add a future transport or harness, implement the completion trait in `integrations/ai_connectors/<id>/mod.rs` and explicitly declare/register it in `backend/src/ai_connectors.rs`; datasource auto-discovery does not scan this directory. Keep provider wire formats inside the transport. Keep metadata planning, bounded evidence queries, hypothesis generation, JSON repair, evidence validation, jobs, and conversation history in `backend/src/ai.rs` and storage. The current default remains OpenAI-compatible; no new provider-selection field or saved-object migration is required by this move.

## Check

Elasticsearch Data Views accept comma-separated index names and wildcard patterns such as `logs-*`. Field discovery uses `_field_caps` across the full pattern, excludes metadata and object/nested containers, and marks fields as `conflict` when their Elasticsearch types map to different Kibana type families. Compatible types such as `date`/`date_nanos` remain one usable field. ClickHouse Data Views use an exact table name.

On an Elasticsearch Explore first page, the connector sends a bounded document request with exact hit tracking disabled and a separate `size: 0` request for the full-scope histogram and exact total. Both use the same query, time range, and PIT. Cursor pages fetch only rows and reuse the first-page histogram/total. AI evidence queries keep bounded rows and aggregations within each source query to preserve their query budget. If an Explore histogram request fails, rows are still returned with a lower-bound total and an unavailable reason. Before searching, the connector reads field capabilities for the time and sort fields. If some matched indices map the time field as a non-date type, the document search is scoped to the indices that map it as `date` or `date_nanos`. Sort fields get `unmapped_type`/`numeric_type` fallbacks; text, object, or incompatible sort fields fail with a specific message. Shard failures are reported as incomplete results instead of partial pages, and a timed-out document search returns its rows with `timed_out` and no continuation cursor. Rejections map Elasticsearch error types to curated messages (missing index, unparseable query, unsortable field, timeout, overload). All time breakdowns use a shared automatic interval (`HistogramInterval::automatic`), not independent per-series intervals; bounded custom ranges still support manual interval selection. ClickHouse keeps valid document rows if its separate histogram query fails and returns a sanitized unavailable reason. Test these paths without exposing raw upstream errors, credentials, or URLs.

Elasticsearch PIT IDs grow with the number of matched shards. Allow up to 256 KiB per PIT ID and 512 KiB per native cursor; the shared public cursor wrapper accepts up to 1 MiB. Larger inputs remain rejected, and an oversized newly opened PIT is closed. Debug diagnostics for invalid PIT/cursor responses include only static reasons and size/count metadata, never opaque PIT IDs or cursor contents. These byte limits do not change the 50-row page or 100-row result window.

Reuse `search::safe_field_name` for Elasticsearch sort/time-field validation. ClickHouse automatic and fixed histograms share `decode_histogram_rows`; malformed timestamps/counts must return the same sanitized invalid-response error in either path. Keep source-specific SQL/HTTP behavior separate rather than extracting cross-connector abstractions solely to reduce repeated syntax.

```sh
cargo test -p investigenie-api
npm run test:web
npm run build:web
```

Then start the app, add a connection on **Connections**, create a Data View, and try Explore. Test InvestiGenie AI only when the connector has an `investigation.rs` dialect; registering a datasource alone does not enable it.
