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

Data connectors provide connection metadata, schema, read-only search, paging, and histograms. Their forms and Explore/Data Views behavior are metadata-driven. AI evidence gathering also needs an investigation dialect: an `investigation.rs` file beside the connector's `mod.rs` that implements `InvestigationDialect` from `backend/src/investigation_dialect/` (currently Elasticsearch, ClickHouse, Loki, Prometheus, and Mimir). Adding a datasource does not add one automatically. `backend/build.rs` discovers the file and derives the descriptor's `ai_investigation` flag from it. The AI page disables Data Views from other sources, and investigations that include them fail with `unsupported_integration` before any source or model call.

Built in: `elasticsearch` (Query DSL / Lucene), `clickhouse` (SQL `WHERE` clauses, read-only), `bigquery` (Standard SQL, read-only), `victorialogs` (LogsQL, read-only), `victoriametrics` (raw metrics selectors, read-only), `prometheus` (PromQL range queries, read-only), `mimir` (PromQL range queries through Mimir's Prometheus API, read-only), and `loki` (LogQL, read-only).

Optional credential inputs left empty mean "not configured": an empty username means no authentication, so never type `""` into a field. Prometheus, VictoriaMetrics, VictoriaLogs, and ClickHouse accept a username with an empty password; a password without a username is rejected. Loki ignores credential inputs that belong to an unselected authentication mode.

Loki and Mimir share one authentication module (`backend/src/connector_auth.rs`) with four modes: none, Basic, bearer token, and OAuth 2.0 client credentials. Basic accepts a username with an empty password; inputs for unselected modes are ignored. OAuth 2.0 needs a token URL, client ID, and client secret. The token URL must use HTTPS, because token requests carry the client secret (RFC 6749 §3.2); plain HTTP is accepted only for `localhost` and loopback addresses, such as a local identity provider. It also takes with optional space-separated scopes and an audience (needed by some providers, such as Auth0). The client authenticates to the token endpoint with an HTTP Basic header (`client_secret_basic`, the default) or in the request body (`client_secret_post`). Access tokens are cached in server memory until 30 seconds before `expires_in` (60 seconds when the provider omits it), keyed by every OAuth setting including the secret, so connections that share a client share a token and a changed secret is verified. A 401 from Loki or Mimir drops the cached token so the next request fetches a new one. Token-endpoint failures return a fixed message (`loki_authentication_failed` / `mimir_authentication_failed`) without upstream text. Client IDs and secrets are stored as encrypted secrets; the token URL, scopes, audience, and client authentication method are shown again when editing.

ClickHouse connections have an optional **Max execution time (seconds)** setting (1–600). Leave it blank to omit ClickHouse's server-side `max_execution_time`; the HTTP client still bounds each request to 15 seconds when unset. A configured limit extends the client timeout by five seconds. SQL `LIMIT` clauses bound returned rows; configure server-side memory limits in the ClickHouse user profile, since the connector does not override them per request.

VictoriaLogs exposes one virtual `logs` dataset and the native `_time` timestamp field. The adapter supports HTTP Basic authentication and `AccountID` / `ProjectID` tenant headers. It returns bounded JSON Lines pages and uses the native hits API for exact totals and histograms. Row IDs are presentation-only, and offset paging is not snapshot-consistent under concurrent ingestion. Histograms currently support UTC only; a non-UTC histogram returns an explicit unavailable reason without discarding log rows. The connector does not yet add VictoriaLogs filter translation to AI investigations.

VictoriaMetrics exposes one virtual `metrics` dataset. Data Views use synthetic `_time` and `_value` fields plus discovered metric labels. Search accepts raw MetricsQL series selectors and returns raw stored samples; it does not evaluate MetricsQL functions or interpolated range-query points. Totals and histograms are exact server-side `count_over_time` counts over the selected range, so they are not limited by row caps. Time-sorted pages export only the time window that holds the requested rows and re-count dense windows on a finer grid; each export is capped at 50,000 samples and 16 MiB. Sorting by a label or `_value` needs the full result and is limited to 50,000 matching samples. Selectors that VictoriaMetrics cannot count fall back to one bounded export. All time first resolves the first and last sample timestamps. Offset paging is not snapshot-consistent while new samples arrive. Label discovery uses VictoriaMetrics' default recent time range and may omit older-only labels. Histograms currently support UTC only. AI filter translation is not implemented.

Prometheus exposes one virtual `metrics` dataset with synthetic `_time` and `_value` fields plus labels discovered from `/api/v1/labels`. Explore evaluates a PromQL range query at the selected histogram step; the rows are evaluated values, not raw samples, and may include lookback-filled points. Totals and histograms come from `count(...)` (or `count by (label)` for breakdowns) at the same step, so they stay exact for any number of series. Time-sorted pages fetch only the steps that hold the requested rows; each fetch is capped at 50,000 samples and 16 MiB. Sorting by a label or `_value` needs every evaluated sample and is limited to 50,000. Expressions that cannot be counted, such as scalars, fall back to one bounded range query. The connection test evaluates `vector(1)`. Pagination re-runs the queries, so it is not snapshot-consistent. Prometheus warnings are treated as incomplete results, and error responses map to curated parse, limit, and timeout messages. AI investigations and cross-language queries use the PromQL dialect in `integrations/data_connectors/prometheus/investigation.rs`, described under [cross-language Explore queries](#cross-language-explore-queries).

Grafana Loki exposes one virtual `logs` dataset with synthetic `_time` and `_msg` fields plus stream labels discovered from `/loki/api/v1/labels`. Label discovery uses Loki's default recent time window and may omit older-only labels. Basic and bearer credentials are sent for an external authentication proxy; Loki itself does not provide API authorization, and `X-Scope-OrgID` is a tenant selector, not an authorization boundary. Explore queries use Loki's nanosecond `[start,end)` bounds, translating the app's inclusive millisecond end. An empty query or an empty `{}` selector, which Loki itself rejects, resolves to `{service_name=~".+"}`: Loki 3 gives every stream a `service_name` label. If labels exist in the range but none is `service_name` (older Loki), the search fails with `loki_selector_required`. Each search fetches at most 5,000 entries (16 MiB). When more lines match a time-sorted search, rows come from the newest entries (or a forward query for ascending pages), and Loki's metric engine counts the full result with `count_over_time`, so totals stay exact. Histogram buckets come from a step-aligned range query, so a line timestamped exactly on an interior bucket boundary counts in the earlier bucket; buckets still sum to the exact total. Large-result breakdowns need a stream label, and All time cannot be counted; in those cases, or when a count query fails, rows are kept with a lower-bound total and a histogram-unavailable reason. Sorting a large result by any other field still fails with `loki_result_limit_exceeded`. Pagination re-runs the bounded query, so it is not snapshot-consistent. Histograms currently support UTC only. Data View fields also include fields Loki 3 detects in the last 24 hours of logs (`/loki/api/v1/detected_fields`), typed by source and value, for example `json int`, `logfmt duration`, or `metadata string` for structured metadata. A field that both parsers report is treated as JSON, because chaining `| json | logfmt` lets non-matching lines through. Older Loki versions without the endpoint simply list no parsed fields. Parsed fields appear in rows only when the query parses lines, such as `| json`. Metric queries (`sum by (service) (count_over_time({…} [5m]))`) run as range queries evaluated at multiples of the step since the epoch and return one row per series and step with `_time`, `_value`, and the series labels; `{}` selectors inside them resolve like a log query's, and Loki's limit of 11,000 points per series applies.

AI investigations and cross-language queries use the LogQL dialect in `integrations/data_connectors/loki/investigation.rs`. Stream conditions that select streams on their own anchor the selector; line conditions become line filters; parsed fields add their parser once (`| json` or `| logfmt`; structured metadata needs none) and a label filter. Numeric, duration, and byte comparisons are guarded with a pattern of the value type, so lines whose value does not convert neither match nor fail, and metric queries drop lines a parser could not read with `| __error__=""`. Line conditions inside OR or NOT test the line copied into a temporary `ig_line` label, which is dropped again. For AI plans, a value that does not fit the field type is dropped from the plan instead of failing the investigation. Stream labels are the only breakdown fields.

Loki rejects range queries longer than its `max_query_length` (default `30d1h`). When a connection is added or edited, the connection test reads `limits_config.max_query_length` from Loki's `/config` and saves it with the connection as `max_query_length_ms` (0 means unlimited). Searches then narrow longer ranges, including All time, to the newest window Loki accepts and mark the total as a lower bound (`+`). `/config` is an admin endpoint that gateways and hosted Loki often block, and it shows the global default rather than per-tenant overrides, so when the limit is unknown or different, Loki's own rejection (`the query time range exceeds the limit (…, limit: 30d1h)`) supplies it and the search retries once within it. A search that still exceeds it fails with `loki_query_too_long`. Instant count queries are not subject to this limit.

Grafana Mimir reuses the Prometheus connector's PromQL engine (same virtual `metrics` dataset, synthetic `_time` and `_value` fields, server-side counts, and step-bounded pages) against Mimir's Prometheus-compatible API. Use the API URL including its prefix, normally `http://mimir:9009/prometheus`. Mimir adds the shared authentication modes for a gateway in front of it, and an optional tenant sent as `X-Scope-OrgID`; as with Loki, the tenant header selects data and is not an authorization boundary. Errors use Mimir codes and messages (`mimir_*`). AI investigations and cross-language queries use the PromQL dialect shared with Prometheus (`mimir/investigation.rs` re-exports it). Labels are the only breakdown fields. Mimir's own query-length limits are disabled by default and are not discovered.

### Cross-language Explore queries

Explore accepts the Data View's own language unchanged. For sources with an investigation dialect (Elasticsearch, ClickHouse, Loki, Prometheus, and Mimir), the query bar also offers the other languages: Lucene, SQL, LogQL, and PromQL. A query in another language is read into a language-neutral query (`backend/src/investigation_dialect/`), either a filter tree or a computation. Its field names are mapped onto the target Data View, each condition is checked against the target's field types, and the query is written in the target's language. The API returns the query it sent as `translated_query`, and Explore shows it under the query bar. The descriptor's `query_languages` lists the choices, native first, each with an example. Anything the target cannot express exactly is refused with `query_translation_failed` and a reason, never approximated. An empty query uses the native default query and is not translated.

**Filters** combine conditions with AND, OR, NOT, and parentheses. Conditions are equality and not-equal, value lists, contains, prefix, suffix, regex, whole-word text search, exists and missing, numeric comparisons, case-insensitive variants of the string conditions, and free text. Every language has the same meaning:

- A missing field never matches a condition, and NOT is the full complement, so it includes records without the field. Writers compensate per engine. SQL negations add `OR x IS NULL`. PromQL and LogQL read a missing label as `""`, so a positive pattern that matches the empty string also gets `l!=""`, and a negated one gets an `l=""` alternative. LogQL numeric comparisons are guarded, as `(f=~"<number>" and f >= 5)` or, negated, `(f!~"<number>" or f < 5)`.
- String conditions are case-sensitive unless marked case-insensitive: `(?i)` in RE2, `ILIKE` or `lower(x) = 'v'` in SQL.
- Regexes match the whole value, and `.` matches newlines. Only a portable subset of RE2 and Lucene regex syntax translates: literals, `.`, classes, `\d`, `\w`, `\s`, groups, alternation, and bounded repetition, plus the flags `(?i)` and `(?s)`. Anchors are allowed only at the ends. LogQL line filters and SQL `match` search within a value, so they are written with explicit anchors.
- Text search matches the words in order, case-insensitively, as `"two words"` in Lucene, `match(message, '(?i)\btwo\W+words\b')` in SQL, and `|~ "(?i)\btwo\W+words\b"` in LogQL.

| Language | Reads and writes |
|---|---|
| Lucene | `field:value`, `field:"phrase"`, `field:(a OR b)`, `field:prefix*`, `field:/regex/`, ranges such as `field:[low TO high]`, `field:{low TO *]`, and `field:>=v`, `_exists_:field`, bare free text, and `AND`, `OR`, `NOT`, `+`, and `-` with parentheses, read with Lucene's own clause semantics. Phrases and bare words on message fields (`message`, `msg`, `log`, `body`) are word searches. Fuzzy, proximity, boost, and date math are refused. |
| SQL | `SELECT * FROM table WHERE …` with `=`, `!=`, `<>`, `IN`, `LIKE`, `ILIKE`, `BETWEEN`, `IS [NOT] NULL`, `match()`, `lower()`, map access such as `LogAttributes['http.route']`, AND, OR, NOT, and computations. |
| LogQL | Stream selectors, line filters (`\|=`, `!=`, `\|~`, `!~`, with `or`), `\| json`, `\| logfmt`, label filters with `and`, `or`, parentheses, and number, duration, and byte comparisons, and computations. Other stages, such as `line_format`, `pattern`, and `regexp`, are refused. |
| PromQL | Selectors with `=`, `!=`, `=~`, and `!~` (double-quoted, single-quoted, and backtick strings, and Prometheus 3 quoted names), `_value` comparisons such as `> 5`, `or`, parentheses, and computations. `and`, `unless`, `bool`, functions, range selectors, `offset`, and `@` are refused. |

PromQL has no NOT, and a selector is a single AND of matchers. Filters are therefore written in disjunctive normal form: up to 32 selectors joined with `or`, each followed by its `_value` comparisons. Selectors whose matchers all accept the empty string get the match-all `__name__!=""` anchor.

**Field names** are matched exactly first, then across the sources' conventions:

- OpenTelemetry attribute keys, so `service.name`, `resource.attributes.service.name`, `attributes.http.route`, and ClickHouse's `ResourceAttributes['service.name']` all name the same attribute.
- The OpenTelemetry ClickHouse exporter's columns: `ServiceName`, `SeverityText`, `TraceId`, `SpanId`, and `Body`.
- Label spellings, such as `service_name`.
- Message fields: Loki's `_msg` line maps to `message`, `Body`, and similar fields.

Elasticsearch analyzed `text` fields use their `.keyword` sub-field for exact conditions when it exists. Without one, only word searches on them are exact, so an AI `eq` on a text field is a phrase search. ClickHouse `Nullable` columns, numbers stored as strings (compared with `toFloat64OrNull`), and maps are handled. Elasticsearch has no contains, suffix, or regex that starts with `.*`, because each needs a leading wildcard and the connector disables leading wildcards. It also has no case-insensitive exact matching.

**Computations** are `count`, `rate` (records per second), and `sum`, `avg`, `min`, or `max` of a numeric field. Each covers time buckets of a fixed range, from one second to one week, grouped by up to eight fields. A bucket `(t - range, t]` is labelled by its end `t`, which falls on a multiple of the range since the epoch. The translated search steps by exactly the range, over the bucket ends inside the selected time range. `avg` is the exact mean (`sum_over_time / count_over_time`), not a mean of per-series means.

| Language | Form |
|---|---|
| LogQL | `sum by (g) (count_over_time({…} [5m]))`, `rate`, and `sum`, `min`, or `max` of the matching `*_over_time({…} \| unwrap f [5m])`; avg is `sum_over_time` divided by `count_over_time` |
| PromQL | The same forms over raw samples. Rate is `sum(count_over_time(…[5m])) / 300`, because PromQL `rate()` has counter semantics and is refused. The filter must be one selector without `_value` comparisons. |
| SQL | A nested `SELECT` that buckets the Data View's time field and aggregates. The simple `toStartOfInterval` form is also read. |

Lucene has no aggregations, so computations cannot target Elasticsearch.

**Verification.** Unit tests check every dialect against a reference evaluator of these semantics. They cover the read-back of every condition with hostile values, translation between every pair of languages, and computations. The live test `live_translations_return_the_same_records_in_every_engine` seeds one dataset into Elasticsearch 8.17, ClickHouse 24.8, Loki 3.4.2, Prometheus 3.2.1, and Mimir 2.15. It runs filters and computations written in every language against every engine, and requires each engine to return exactly the records, or values, that the evaluator selects. It needs the engines, so CI skips it; run it locally before changing a dialect:

```sh
export $(scripts/translation-engines/start.sh)
cargo test -p investigenie-api live_translations -- --include-ignored --nocapture
scripts/translation-engines/stop.sh
```

Writers are checked against these engines. ClickHouse processes backslash escapes inside string literals, so the SQL writer escapes both `\` and `'` (doubling `'` alone let a trailing `\` end the string); `LIKE` patterns escape `\`, `%`, and `_` before string-literal escaping. The Lucene writer escapes whitespace in unquoted terms and quotes non-numeric range bounds, since an unquoted bound cannot hold whitespace.

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
