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

Data connectors provide connection metadata, schema, read-only search, paging, and histograms. Their forms and Explore/Data Views behavior are metadata-driven. Investigation query translation must also support a source before AI evidence gathering can use it; adding a datasource does not automatically implement that translation.

Built in: `elasticsearch` (Query DSL / Lucene), `clickhouse` (SQL `WHERE` clauses, read-only), `bigquery` (Standard SQL, read-only), `victorialogs` (LogsQL, read-only), `victoriametrics` (raw metrics selectors, read-only), `prometheus` (PromQL range queries, read-only), and `loki` (LogQL, read-only).

ClickHouse connections have an optional **Max execution time (seconds)** setting (1–600). Leave it blank to omit ClickHouse's server-side `max_execution_time`; the HTTP client still bounds each request to 15 seconds when unset. A configured limit extends the client timeout by five seconds. SQL `LIMIT` clauses bound returned rows; configure server-side memory limits in the ClickHouse user profile, since the connector does not override them per request.

VictoriaLogs exposes one virtual `logs` dataset and the native `_time` timestamp field. The adapter supports HTTP Basic authentication and `AccountID` / `ProjectID` tenant headers. It returns bounded JSON Lines pages and uses the native hits API for exact totals and histograms. Row IDs are presentation-only, and offset paging is not snapshot-consistent under concurrent ingestion. Histograms currently support UTC only; a non-UTC histogram returns an explicit unavailable reason without discarding log rows. The connector does not yet add VictoriaLogs filter translation to AI investigations.

VictoriaMetrics exposes one virtual `metrics` dataset. Data Views use synthetic `_time` and `_value` fields plus discovered metric labels. Search accepts raw MetricsQL series selectors and returns raw JSONL-export samples; it does not evaluate MetricsQL functions or interpolated range-query points. Exports are capped at 50,000 samples and 16 MiB and fail explicitly if a result exceeds either cap. Pagination is local over the bounded export, so each page re-fetches the result and is not snapshot-consistent while new samples arrive. Label discovery uses VictoriaMetrics' default recent time range and may omit older-only labels. Histograms are derived from raw samples and currently support UTC only. AI filter translation is not implemented.

Prometheus exposes one virtual `metrics` dataset with synthetic `_time` and `_value` fields plus labels discovered from `/api/v1/labels`. Explore runs a PromQL range query at the selected histogram step; the rows are evaluated values, not raw samples, and may include lookback-filled points. The adapter caps results at 500 series, 50,000 sample rows, and 16 MiB, failing explicitly rather than presenting truncated data as complete. Pagination is local and re-runs the range query, so it is not snapshot-consistent. Histograms count the evaluated sample rows. Prometheus warnings are treated as incomplete results. AI filter translation is not implemented.

Grafana Loki exposes one virtual `logs` dataset with synthetic `_time` and `_msg` fields plus stream labels discovered from `/loki/api/v1/labels`. Label discovery uses Loki's default recent time window and may omit older-only labels. Basic and bearer credentials are sent for an external authentication proxy; Loki itself does not provide API authorization, and `X-Scope-OrgID` is a tenant selector, not an authorization boundary. Explore queries use Loki's nanosecond `[start,end)` bounds, translating the app's inclusive millisecond end. Results are capped at 5,000 entries and 16 MiB; a saturated result is rejected rather than treated as complete. Pagination is local and re-runs the bounded query, so it is not snapshot-consistent. Histograms count returned log lines and currently support UTC only. AI LogQL translation is not implemented.

### BigQuery

BigQuery connections require the exact `https://bigquery.googleapis.com/` API URL, a billing project ID, one dataset ID, and a service-account JSON key. The key is stored through InvestiGenie's encrypted connection storage and exchanged for a BigQuery-scoped OAuth token; user-managed service-account keys are high-risk, so prefer workload identity or other keyless credentials where available. Grant the service account BigQuery Job User on the billing project and read-only data access only on the intended dataset. The dataset setting scopes table discovery and schema, but custom SQL can reference any resource that the service account is allowed to read; IAM is the access boundary.

BigQuery accepts one parsed Standard SQL query statement, returns exact job totals, and uses Google-issued query-result page tokens for scan cursors. Pages are limited to 100 rows, SQL to 8 KiB, each response to 16 MiB, table discovery to 5,000 names, and each query job to 10 GiB billed bytes. A first-page histogram uses a separate bounded aggregation job; All time also uses a bounded min/max job, so the billed-bytes ceiling applies to each job, not to the total Explore operation. Job cursors refer to the same query result and may expire with BigQuery's stored job results. Hit IDs use job ID plus row offset and are presentation-only; document lookup and AI SQL translation are not implemented. Date, datetime, and timestamp fields are supported for time filtering and histograms; fixed intervals are UTC-aligned, while calendar buckets use the selected timezone.

## Create one with the AI skill (recommended)

The repo includes the agent skill [`build-integration`](../.github/skills/build-integration/SKILL.md). It is a step-by-step workflow and checklist covering the contract, credentials, read-only access, paging, histograms, AI support, and tests.

1. Open the `investigenie/` folder in VS Code with GitHub Copilot (or another agent that reads `.github/skills`).
2. In agent mode, ask for the integration with the details the agent cannot guess:
   > Use the build-integration skill to add a **Loki** integration. Endpoint is HTTP with optional bearer token. Queries are LogQL. Time field is the log timestamp.
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

The full field-by-field contract and response examples are in [integrations/README.md](../integrations/README.md). Start from the closest existing data connector ([ClickHouse](../integrations/data_connectors/clickhouse/mod.rs) is the smallest).

## AI transports

The backend-owned [AI connector contract](../backend/src/ai_connectors.rs) defines `AiConnector::complete`, settings, completion content/usage/truncation, and sanitized errors. [OpenAI-compatible transport](../integrations/ai_connectors/openai/mod.rs) implements Chat Completions for OpenAI and compatible endpoints, including bounded HTTP requests and one fallback when a provider explicitly rejects JSON mode. This is not full coverage of every OpenAI API endpoint.

To add a future transport or harness, implement the completion trait in `integrations/ai_connectors/<id>/mod.rs` and explicitly declare/register it in `backend/src/ai_connectors.rs`; datasource auto-discovery does not scan this directory. Keep provider wire formats inside the transport. Keep metadata planning, bounded evidence queries, hypothesis generation, JSON repair, evidence validation, jobs, and conversation history in `backend/src/ai.rs` and storage. The current default remains OpenAI-compatible; no new provider-selection field or saved-object migration is required by this move.

## Check

Elasticsearch Data Views accept comma-separated index names and wildcard patterns such as `logs-*`; the API validates fields against the pattern's mappings. ClickHouse Data Views use an exact table name.

Elasticsearch requests exact match totals across the pattern and supports unmapped time fields in sort requests. All time breakdowns use a shared automatic interval (`HistogramInterval::automatic`), not independent per-series intervals; bounded custom ranges still support manual interval selection. ClickHouse keeps valid document rows if its separate histogram query fails and returns a sanitized unavailable reason. Test these paths without exposing raw upstream errors, credentials, or URLs.

Reuse `search::safe_field_name` for Elasticsearch sort/time-field validation. ClickHouse automatic and fixed histograms share `decode_histogram_rows`; malformed timestamps/counts must return the same sanitized invalid-response error in either path. Keep source-specific SQL/HTTP behavior separate rather than extracting cross-connector abstractions solely to reduce repeated syntax.

```sh
cargo test -p investigenie-api
npm run test:web
npm run build:web
```

Then start the app, add a connection on **Connections**, create a Data View, and try Explore and InvestiGenie AI against it.
