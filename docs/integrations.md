# Integrations

An integration adds a datasource type, such as Elasticsearch or ClickHouse. It is a folder of Rust code compiled into the server. Once it is built in, its connection form, Explore, Data Views, and AI investigations all work with no UI changes.

Built in: `elasticsearch` (Query DSL / Lucene), `clickhouse` (SQL `WHERE` clauses, read-only).

ClickHouse connections have an optional **Max execution time (seconds)** setting (1–600). Leave it blank to omit ClickHouse's server-side `max_execution_time`; the HTTP client still bounds each request to 15 seconds when unset. A configured limit extends the client timeout by five seconds. SQL `LIMIT` clauses bound returned rows; configure server-side memory limits in the ClickHouse user profile, since the connector does not override them per request.

## Create one with the AI skill (recommended)

The repo includes the agent skill [`build-integration`](../.github/skills/build-integration/SKILL.md). It is a step-by-step workflow and checklist covering the contract, credentials, read-only access, paging, histograms, AI support, and tests.

1. Open the `investigenie/` folder in VS Code with GitHub Copilot (or another agent that reads `.github/skills`).
2. In agent mode, ask for the integration with the details the agent cannot guess:
   > Use the build-integration skill to add a **Loki** integration. Endpoint is HTTP with optional bearer token. Queries are LogQL. Time field is the log timestamp.
3. The agent reads the contracts, scaffolds `integrations/<id>/`, and writes the tests.
4. Review the result against the checklist at the end of the skill, then run the checks below.

The skill also works as a checklist when you write an integration by hand.

## Create one by hand

1. **Folder:** `integrations/<id>/mod.rs`. The `<id>` must be a Rust identifier (`my_source`) that matches the descriptor ID. `backend/build.rs` discovers it automatically, so no registry edit is needed.
2. **Exports:**
   ```rust
   pub fn descriptor() -> crate::integrations::IntegrationDescriptor;
   pub fn connect(config: &crate::integrations::ConnectionConfig)
       -> Result<std::sync::Arc<dyn crate::integrations::Connector>, crate::integrations::ConnectorError>;
   ```
3. **Descriptor:** the display name, the connection fields (types, `secret`, `editable`, `visible_when`, groups), and query metadata (placeholder, templates, quoting, `document_mode`, sorting and histogram capabilities). The UI is built from this.
4. **Connector:** implement `test_connection`, `indices`, `fields`, `search`, and `document`.
5. **Search must honour:** query, inclusive UTC epoch-ms time range, sort, page size (≤100), cursor paging, and the histogram interval, breakdown field, and timezone. Filter and sort before paging. Compute the histogram over all matches, not just the page.
6. **Return the shared shape:** `took`, `timed_out`, `total`, `total_relation`, `hits[{index,id,score,source}]`, and `histogram[{timestamp,label,count}]`.
7. **Stay safe:** read-only credentials and queries, timeouts, size limits, and errors that never include secrets.
8. **Test it:** mock the upstream with `wiremock`. Cover settings, credentials, fields, empty results, sorting, time filters, histogram counts, paging, and errors.

The full field-by-field contract and response examples are in [integrations/README.md](../integrations/README.md). Start from the closest existing integration ([ClickHouse](../integrations/clickhouse/mod.rs) is the smallest).

## Check

Elasticsearch Data Views accept comma-separated index names and wildcard patterns such as `logs-*`; the API validates fields against the pattern's mappings. ClickHouse Data Views use an exact table name.

```sh
cargo test -p investigenie-api
npm run test:web
npm run build:web
```

Then start the app, add a connection on **Connections**, create a Data View, and try Explore and InvestiGenie AI against it.
