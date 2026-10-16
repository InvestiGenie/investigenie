# Connector contracts

Concrete data connectors live in `data_connectors/<source_id>/`; concrete AI transports live in `ai_connectors/<transport_id>/`. Shared contracts and registration live in the backend: [data_connectors.rs](../backend/src/data_connectors.rs) and [ai_connectors.rs](../backend/src/ai_connectors.rs). Data connector IDs, public APIs, and saved configurations remain unchanged by this layout.

For the complete implementation workflow, security restrictions, paging contract, AI compatibility requirements, and test checklist, use the [build-integration skill](../.github/skills/build-integration/SKILL.md).

Your data connector converts native datasource responses into InvestiGenie's shared JSON shape. Start from [ClickHouse](data_connectors/clickhouse/mod.rs) and its [connector](data_connectors/clickhouse/connector.rs), or the [Elasticsearch connector](data_connectors/elasticsearch/connector.rs).

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
| Identity | `id`, `display_name`, `query_language`, `description`: strings. |
| `connection_fields` | Each field: `name`, `label`, `input_type`, `placeholder` strings; `required`, `secret`, `editable` booleans. Inputs: `text`, `url`, `password`, `checkbox`, `select`. Include a required, editable, non-secret `url`. |
| Optional field controls | Select `options`: `[{"value":"basic","label":"Basic"}]` (first is default). Conditional visibility: `visible_when: {"field":"auth_type","value":"basic"}`. Optional `group` string: `"authentication"` pairs options in a separate grid before `"credentials"`, which pairs credential inputs. Noneditable, non-secret authentication options remain visible but disabled during edits. |
| `query` strings | `placeholder`, `default_query`, `field_query`, `identifier_quote`, `help`, `schema_label`. Templates accept `{index}`, `{field}`, `{query}`; inserted identifiers use `identifier_quote` (empty means unquoted). An empty query removes a leading `{query} AND `. |
| `query` capabilities | `document_mode`: `lookup` or `inline`; `sort_requires_projection`, `histogram_total_from_buckets`: booleans. |

The app supplies the connection name; settings arrive as a flat `ConnectionConfig` map. Validate them in `connect`. Mark credentials `secret: true`, `editable: true` to allow explicit replacements; the API excludes secrets from summaries and retains omitted credentials during edits. Use `editable: false` when replacement is not supported. Only editable fields are submitted during edits; handle null when clearing optional non-secret settings.

## 3. Implement the connector

Implement the [shared Connector](../backend/src/data_connectors.rs): `Send + Sync`, with boxed `Send` futures through `ConnectorFuture`.

| Method | Required behavior |
| --- | --- |
| `test_connection` | Verify endpoint/access before the API saves the connection. |
| `indices` | Return `IndexSummary`: dataset `name`, optional `document_count` string and `health` string. List the datasets users can select. |
| `fields` | Return `FieldSummary`: `name`, `field_type`, `is_time_field`. Expose leaf fields, not object containers; flag actual dates/timestamps. A valid time field is required to create a usable Data View. |
| `search` | Translate the [SearchRequest](../backend/src/search.rs) and return the JSON below. |
| `document` | For `lookup`: return `index`, `id`, `found`, `source`; missing documents use `found: false`, `source: null`. For `inline`: the UI inspects the returned hit; this method may reject lookup. |

Honor `index`, `query`, `from`, `size`, optional `sort_by`/`sort_order` (`asc`/`desc`), `histogram_field`, and `time_range`. Time bounds are inclusive UTC epoch milliseconds. Filter and sort **before pagination**; aggregate the histogram across all matching records with the same filters, not just the page.

For the current built-in adapters, see [integration behavior](../docs/integrations.md): Elasticsearch accepts wildcard/comma-separated index patterns with exact totals and unmapped time-field sorting. ClickHouse's optional execution limit must be omitted when blank, without fixed memory/result/overflow URL settings. Histogram-only failures preserve valid ClickHouse rows. Both adapters share `HistogramInterval::automatic` for All time breakdown intervals; keep the same interval for totals and top-value series.

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