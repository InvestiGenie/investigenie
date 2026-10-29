# Connector contracts

Concrete data connectors live in `data_connectors/<source_id>/`; concrete AI transports live in `ai_connectors/<transport_id>/`. Shared contracts and registration live in the backend: [data_connectors.rs](../backend/src/data_connectors.rs) and [ai_connectors.rs](../backend/src/ai_connectors.rs). Data connector IDs, public APIs, and saved configurations remain unchanged by this layout.

For the complete implementation workflow, security restrictions, paging contract, AI compatibility requirements, and test checklist, use the [build-integration skill](../.github/skills/build-integration/SKILL.md).

`logging.level = "debug"` in `investigenie.toml` (or `LOG_LEVEL=debug`) enables shared operation details for every datasource and the default AI transport; failures are visible at `error`, client rejections at `warn`. `logging.mode` selects `console`, `file`, or `both`; `logging.path = "logs"` resolves beside the file. No per-connector setup is needed. Logs contain safe status/timing/count metadata. Elasticsearch decode failures also include status, content type, byte count, serde category, and schema path, never request or response bodies. See [logging configuration](../docs/backend.md#application-logs).

Your data connector converts native datasource responses into InvestiGenie's shared JSON shape. Start from [ClickHouse](data_connectors/clickhouse/mod.rs) and its [connector](data_connectors/clickhouse/connector.rs), the [Elasticsearch connector](data_connectors/elasticsearch/connector.rs), or the [VictoriaLogs connector](data_connectors/victorialogs/mod.rs) for a LogsQL API.

## 1. Make it visible

Create `integrations/data_connectors/<source_id>/mod.rs`. Use a unique, non-keyword Rust identifier, such as `my_source`; the descriptor ID must match the folder name. Export these functions with real implementations:

```rust
pub fn descriptor() -> crate::data_connectors::IntegrationDescriptor;
pub fn connect(
    config: &crate::data_connectors::ConnectionConfig,
) -> Result<std::sync::Arc<dyn crate::data_connectors::Connector>, crate::data_connectors::ConnectorError>;
```

Keep code and tests in your folder. `backend/build.rs` discovers immediate folders under `data_connectors/` containing `mod.rs`; rebuild and restart the API to register them. AI connector folders are not datasources. No manual datasource registry edit or runtime plugin installation is needed.

## 2. Describe the UI

The descriptor must contain all these properties with the correct types:

| Property | Contents |
| --- | --- |
| Identity | `id`, `display_name`, `query_language`, `description`, `icon`: strings. `icon` identifies a bundled connector brand asset: `elasticsearch`, `clickhouse`, `bigquery`, `loki`, `prometheus`, `victorialogs`, or `victoriametrics`. Missing or unknown keys use the generic database icon. |
| `connection_fields` | Each field: `name`, `label`, `input_type`, `placeholder` strings; `required`, `secret`, `editable` booleans. Inputs: `text`, `url`, `password`, `checkbox`, `select`. Include a required, editable, non-secret `url`. |
| Optional field controls | Select `options`: `[{"value":"basic","label":"Basic"}]` (first is default). Conditional visibility: `visible_when: {"field":"auth_type","value":"basic"}`. Optional `group` string: `"authentication"` pairs options in a separate grid before `"credentials"`, which pairs credential inputs. Noneditable, non-secret authentication options remain visible but disabled during edits. |
| `query` strings | `placeholder`, `default_query`, `field_query`, `identifier_quote`, `help`, `schema_label`. Templates accept `{index}`, `{field}`, `{query}`; inserted identifiers use `identifier_quote` (empty means unquoted). An empty query removes a leading `{query} AND `. |
| `query` capabilities | `document_mode`: `lookup` or `inline`; `sort_requires_projection`, `histogram_total_from_buckets`: booleans. |
| `ai_investigation` | Boolean, derived by the generated registry; connectors leave it to `..Default::default()`. It is `true` only when the connector folder has an `investigation.rs` dialect. When `false`, the AI page disables the source's Data Views and the backend rejects them before planning. |

The app supplies the connection name; settings arrive as a flat `ConnectionConfig` map. Validate them in `connect`. Mark credentials `secret: true`, `editable: true` to allow explicit replacements; the API excludes secrets from summaries and retains omitted credentials during edits. Use `editable: false` when replacement is not supported. Only editable fields are submitted during edits; handle null when clearing optional non-secret settings.

## 3. Implement the connector

Implement the [shared Connector](../backend/src/data_connectors.rs): `Send + Sync`, with boxed `Send` futures through `ConnectorFuture`.

| Method | Required behavior |
| --- | --- |
| `test_connection` | Verify endpoint/access before the API saves the connection. |
| `learned_configuration` | Optional. Settings `test_connection` learned from the source, such as Loki's `max_query_length`, which the API saves with the connection so later requests can respect them. |
| `indices` | Return `IndexSummary`: dataset `name`, optional `document_count` string and `health` string. List the datasets users can select. |
| `fields` | Return `FieldSummary`: `name`, `field_type`, `is_time_field`. Expose leaf fields, not object containers; flag actual dates/timestamps. A valid time field is required to create a usable Data View. Elasticsearch discovers wildcard-pattern fields through `_field_caps` and reports incompatible type families as `conflict`. |
| `search` | Translate the [SearchRequest](../backend/src/search.rs) and return the JSON below. |
| `document` | For `lookup`: return `index`, `id`, `found`, `source`; missing documents use `found: false`, `source: null`. For `inline`: the UI inspects the returned hit; this method may reject lookup. |

Honor `index`, `query`, `from`, `size`, optional `sort_by`/`sort_order` (`asc`/`desc`), `histogram_field`, and `time_range`. Time bounds are inclusive UTC epoch milliseconds. Filter and sort **before pagination**; aggregate the histogram across all matching records with the same filters, not just the page.

The public search API applies a shared 50-record response limit and a 100-record result-window ceiling to every connector. It wraps native cursors and removes continuation at the ceiling; native connector calls retain their existing maximum page size of 100. Do not add a 100-record SQL/query limit to the dataset used for totals or histograms. Explore shows at most two pages and starts a fresh window when a graph bucket or range is selected. Missing continuation does not imply exhaustive coverage; aggregate and sort work can still exceed the displayed record count. Internal AI evidence limits are unchanged.

For the current built-in adapters, see [integration behavior](../docs/integrations.md): Elasticsearch accepts wildcard/comma-separated index patterns with exact totals; field capabilities scope mixed-mapping patterns to indices whose time field is a date and add sort fallbacks, and shard failures are explicit errors. ClickHouse's optional execution limit must be omitted when blank, without fixed memory/result/overflow URL settings. Histogram-only failures preserve valid ClickHouse rows. Both adapters share `HistogramInterval::automatic` for All time breakdown intervals; keep the same interval for totals and top-value series. VictoriaLogs provides the virtual `logs` dataset, `_time` field, bounded JSONL pages, exact hit totals, and hits-API histograms. Its offset paging is not snapshot-consistent, IDs are presentation-only, non-UTC histograms are explicitly unavailable, and AI filter translation is not implemented. VictoriaMetrics exposes raw samples from its virtual `metrics` dataset using series selectors only; it does not evaluate arbitrary MetricsQL expressions. Totals and histograms are exact server-side counts, time-sorted pages export only the window holding the requested rows (each export capped at 50,000 samples and 16 MiB), label/value sorts are limited to 50,000 matches, offset paging is not snapshot-consistent, label discovery reflects the server's default recent time range, non-UTC histograms are unavailable, and AI translation is not implemented. Prometheus exposes one virtual `metrics` dataset and evaluates PromQL range queries at the selected histogram step; rows are evaluated values and may include lookback-filled samples, not raw stored samples. Totals and histograms use `count` at the same step, time-sorted pages fetch only the steps holding the requested rows (50,000 samples and 16 MiB per fetch), label/value sorts are limited to 50,000 evaluated samples, and offset paging re-runs the queries and is not snapshot-consistent. AI translation is not implemented. Loki exposes a virtual `logs` dataset with `_time`, `_msg`, and stream labels; it accepts Basic, bearer, or OAuth 2.0 client-credentials authentication (shared with Mimir in `backend/src/connector_auth.rs`) for an external auth proxy and an optional `X-Scope-OrgID` tenant header. Loki itself does not provide API authorization; the tenant header is not an authorization boundary. Label discovery defaults to a recent time window. Query bounds map inclusive milliseconds to Loki's nanosecond `[start,end)` API. Empty `{}` selectors resolve to `{service_name=~".+"}` on Loki 3. Each search fetches at most 5,000 entries and 16 MiB; larger time-sorted results keep a bounded page and get exact totals and histograms from `count_over_time`, while other sorts still fail explicitly. Local offset paging re-runs the query, and is not snapshot-consistent. Histograms currently support UTC only. Detected `| json`, `| logfmt`, and structured-metadata fields are listed with typed field types. AI investigations translate label, `_msg`, and parsed-field filters to LogQL, including numeric, duration, and bytes comparisons. The connection test saves Loki's `max_query_length` from `/config` when readable; searches stay within it, or learn it from Loki's rejection and retry once. Mimir reuses the Prometheus client with the shared authentication modes and a tenant header, and translates label and `_value` filters to PromQL.

BigQuery exposes tables from one configured project and dataset as fully qualified indices. Connections use an encrypted service-account JSON key, standard SQL is parsed as one query statement, and IAM remains the access boundary for custom SQL. Each query job is limited to 10 GiB billed bytes; pages use BigQuery job page tokens, exact totals, and typed result decoding. First-page histograms run as separate bounded jobs, and All time needs an additional bounded extent query. Row IDs are job-relative presentation IDs; document lookup and AI SQL translation are unavailable. See [BigQuery setup and limits](../docs/integrations.md#bigquery).

## 4. Return the Discover shape

```json
{
    "took": 12,
    "timed_out": false,
    "total": 1,
    "total_relation": "eq",
    "hits": [{
        "index": "events", "id": "1", "score": null,
        "source": { "timestamp": "2026-10-08T12:00:00Z", "message": "Example", "status": 200 }
    }],
    "histogram": [{ "timestamp": 1791460800000, "label": null, "count": 1 }]
}
```

- Always include those keys; zero matches use `total: 0`, `hits: []`, `histogram: []`. `took` is milliseconds or null. `total` counts matches before pagination; `total_relation` is `eq` (exact) or `gte` (lower bound).
- Each hit needs a string `index` and unique string `id` within the batch; `score` is a number or null. `source` is an object (or null if unavailable). Preserve JSON value types, arrays, and nesting; do not stringify rows. Date strings should include `Z` or an explicit offset.
- Histogram timestamps are UTC epoch milliseconds; counts are nonnegative. If unavailable, return `histogram: []` plus `histogram_unavailable_reason`. Enable `histogram_total_from_buckets` only when bucket sums represent the matching count.
- Projected results can include `columns: ["timestamp", "message", "status"]`. Supply it when `sort_requires_projection` is true: it determines sortable fields. Implement every advertised sort.
- Return sanitized `ConnectorError { status, code, message }` on failure; the API wraps it as `{ "error": { "code": "...", "message": "..." } }`. Never expose credentials. Returning arbitrary JSON does not enforce this schema; test it.

## 5. Test and try it

Add unit/mock-upstream tests for settings, credentials, fields, response shape, empty results, sorting, time filters, histogram counts, and errors. Enforce read-only access, page/query limits (existing adapters cap pages at 100), timeouts, and cancellation; shared API concurrency limits are not enough.

From `investigenie/`:

```sh
cargo test -p investigenie-api
npm run test:web
npm run build:web
```

Follow the [app startup instructions](../README.md). Check `GET /api/integrations` and **Connections > Add connection**; add your source, create a Data View with a valid time field, then test Discover's table, fields, sorting, time window, graph, and JSON inspector. Verify edits retain credentials and saved objects survive restart in both persistence modes. Configuration changes use revision-based `If-Match` headers; test stale edits as well.

Keep the relevant user docs, integration skill, agent instructions, and context references synchronized with every behavior or contract change. Prefer existing shared functions for repeated behavior; do not duplicate interval policies or UI result renderers in new adapters.

**Limits:** folder-only contributions use existing dependencies and UI capabilities. New Rust libraries require `backend/Cargo.toml` and lockfile changes; integration-local Cargo manifests are not loaded. New UI semantics require extending the shared contract.

## AI connector contract

[backend/src/ai_connectors.rs](../backend/src/ai_connectors.rs) owns `AiConnector::complete(settings, messages, json_mode, max_tokens)`, its boxed Send future, `AiSettings`, `ProviderReply`, `ModelUsage`, `AiConnectorError`, and `default_connector()`. [openai/mod.rs](ai_connectors/openai/mod.rs) implements the default OpenAI-compatible Chat Completions transport. New transports are explicitly declared/registered in the backend AI contract, not discovered by the datasource build script.

Transport code owns authentication, HTTP timeouts, payload-size caps, protocol parsing, actual usage, and protocol-specific fallback. It must not depend on backend investigation/storage modules or log provider secrets. Orchestration owns prompts, schema repair, evidence checks, source scans, pause/stop, and saved chat context. Preserve current errors, timeouts, body limits, cancellation, and OpenAI model/profile compatibility. No runtime transport selector or claim of full OpenAI API coverage is introduced.
