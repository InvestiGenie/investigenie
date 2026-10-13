# InvestiGenie

A multi-source document explorer with a React/TypeScript browser UI and a Rust/Axum API. Datasource endpoints and credentials are configured in the UI and remain on the server.

Project website: [investigenie.com](https://investigenie.com)

## License

InvestiGenie is licensed under the [Apache License 2.0](LICENSE). This license permits commercial use, redistribution, and derivative works subject to its terms.

## Repository layout

The publishable application lives in `investigenie/`. Run the commands below from that directory. If you opened the parent workspace, first run `cd investigenie`.

- `backend/` contains the Axum API, application configuration, and connection registry.
- `frontend/` contains the React/TypeScript/Vite UI.
- `integrations/elasticsearch/` and `integrations/clickhouse/` contain the source-specific Rust adapters.

Integration folders are discovered and registered automatically during Cargo builds. Each integration owns its connector implementation, connection-form metadata, and query capabilities. The API and UI use the shared contract rather than source-specific branches. See [Adding integrations](integrations/README.md). This is build-time integration, not runtime loading of arbitrary source files.

## Initial scope

- Multiple named Elasticsearch and ClickHouse connections can be active at once. Add and remove them from the Connections page; each endpoint is tested before it is registered.
- Create named data views on the Data Views page by selecting a connection, index/table, and date/time field. The API validates the configuration before saving; Discover's dropdown lists saved data views only and uses their configured source and time field.
- Data Views opens on the saved-view list, with an Add data view button and edit/delete actions matching Connections. Editing revalidates the configuration and keeps the view ID stable; rejected edits do not replace the saved view.
- Data views persist across API restarts. Renaming a connection updates its view references; deleting a connection also removes its data views. Stale edits are rejected rather than overwriting another user's changes.
- Elasticsearch uses Lucene-style query-string input translated to Elasticsearch's `query_string` query. This is not a promise of full Lucene parser compatibility.
- ClickHouse uses read-only SQL through its HTTP interface. Queries are single `SELECT` or `WITH ... SELECT` statements and run with ClickHouse's `readonly=1` setting.
- ClickHouse accepts usernames with empty passwords, including the public playground. Leave the password input blank, rather than entering quote characters.
- Discover shows ClickHouse time histograms using the saved view's time field, aggregated across matching SQL rows rather than just the current result page. Custom SQL must include that time field in its output; otherwise rows remain available with a graph-unavailable message.
- Connections and their encrypted configuration persist in the selected storage backend. Connections are created through the Connections UI.
- Saved connection summaries from `GET /api/connections` describe stored configuration; their `status` is not a live availability check, and listing them does not query every datasource. The Connections page checks each row independently through `GET /api/connections/{id}/status`, which calls the registered connector's connection test and returns `connected` or `not_connected`. Elasticsearch checks its indices endpoint; ClickHouse runs `SELECT 1`. API readiness only checks configuration storage, not datasource availability.
- Live badges begin with Checking and retain the previous result during polling. Each check is followed by a five-second polling delay; edits restart the check for the new revision. The backend bounds the connector probe to five seconds, and the browser bounds the entire status request to seven seconds to allow for storage and transport. An outage can therefore take about 12 seconds to appear (five seconds until the next poll plus seven seconds for a stalled request); most failures return sooner. Upstream errors and timeouts show Not connected, as do API, missing-record, storage, capacity, or malformed-response failures. Offline checks never delete saved connections, and later successful checks restore Connected without reloading the list.
- Search results are bounded to 50 rows per request, with a hard API maximum of 100. Both integrations use the same expandable Documents view and JSON inspection modal. ClickHouse query rows are represented as JSON objects, retaining the SQL result's column names and value types; its modal inspects the returned row without a separate document lookup. Elasticsearch supports retrieval by document ID and source-specific sorting.
- Data View Fields separates available and selected fields. Its + / - controls add/remove Documents columns without changing the query; selections are retained per data view until the page reloads. Fields absent from a returned record display a dash.
- Timestamp and selected-field headers have ascending/descending sort controls for both integrations. Sorting runs at the source before pagination; ClickHouse preserves the entered SQL and applies ordering around its result. The combined Document preview and fields absent from a SQL projection are not sortable.
- Discover's time-range editor has a shared timezone preference, defaulting to UTC and retained across reloads. Changing it updates date inputs, graph labels/tooltips, document timestamps, and other date fields without shifting the selected instants. Search requests always use UTC epoch milliseconds. JSON inspection retains the original source values. Ambiguous or nonexistent daylight-saving input times are rejected.
- The compact time-range button opens an editor with a timezone menu, custom dates, and presets. Apply sets an inclusive window, then Search runs it; Cancel discards date edits. Presets capture a fixed range when selected. All time removes the window. The API validates bounds and filters documents and histograms before pagination; ClickHouse custom SQL must project the saved time field when a window is active.
- Authentication, ACLs, and tenants are not implemented. Do not expose the API to untrusted users or networks.

## Quick start from a release

1. Download the zip for your platform from [Releases](https://github.com/InvestiGenie/investigenie/releases) and extract it.
2. Run `investigenie-api` (`investigenie-api.exe` on Windows) from the extracted `investigenie` folder.
3. Open the address it prints, `http://127.0.0.1:3000`.

The API serves the bundled `web/` UI from the same address. Without storage settings, it starts in **quickstart mode**: configuration is saved in `investigenie_data/` next to the binary, encrypted with a generated key in `investigenie_data/encryption.key`. Keep that folder to keep your saved connections. For shared or production use, set `PERSISTENCE_MODE`, `CONFIG_ENCRYPTION_KEY`, and the related settings below (a `.env` file next to the binary works), so the key is not stored beside the data.

Release binaries are not signed. On macOS run `xattr -d com.apple.quarantine investigenie-api` once; on Windows choose **More info → Run anyway** if SmartScreen warns. To listen on another address, set `RUST_API_BIND` (for example `0.0.0.0:3000`) and keep the API behind an authenticated gateway.

## Requirements

- Node.js 20.19+ or 22.12+, with npm.
- Stable Rust toolchain with Cargo.
- A reachable Elasticsearch and/or ClickHouse deployment and credentials with the required read-only access for the data you want to explore. Datasource details are entered in the Connections UI after the app starts.

## Configure the application

Copy `.env.example` to `.env` to configure the application process and persistence. Datasource URLs, databases, and credentials are configured through the Connections UI. The configuration-storage database URL and encryption key are private bootstrap settings; never commit them. The `.env` file is ignored by Git.

AI providers are managed at runtime in the InvestiGenie AI page. Add a named OpenAI-compatible profile with its API base URL (for example, `https://api.example.com/v1`), model, and API key. InvestiGenie tests the provider with a small JSON-mode request before saving the profile. The browser receives only the profile name, URL, model, revision, and `has_api_key`; the key is never returned by an API. Investigations explicitly select a saved profile. No AI provider environment variables are used.

InvestiGenie AI scans matching records from planned Data Views in sequential 100-record pages until the source is exhausted. The selected timeframe is a query filter, not a sampling restriction. There is no overall 50-record or 20-example cap. Repeated signals are grouped across pages with observed counts and representative event IDs; distinct evidence is split into small prompts rather than discarded. One planning call is followed by as many analysis cycles as needed, maintaining a rolling summary of important findings. Actual provider usage is aggregated across those cycles. More distinct evidence means more provider calls; batching controls memory and prompt size but cannot guarantee a fixed total price. Each inquiry is independent; multi-turn memory and dashboard generation are not implemented.

**AI data handling:** the question and compacted examples from selected sources are sent to the configured provider. Credentials and source URLs are not. Treat provider processing as external data sharing: review retention/training and regional processing terms, and avoid sensitive data until InvestiGenie has user authentication, authorization, and field-level redaction. The API currently has no built-in user auth; keep it behind a trusted authenticated gateway and do not expose it to untrusted networks. Findings are hypotheses grounded in returned evidence, not a verified root-cause guarantee.

Add Elasticsearch connections with an HTTP(S) endpoint and optional Basic Auth or API key. Add ClickHouse connections with its HTTP endpoint (commonly port `8123`), an optional database, and optional username/password. TLS verification remains enabled for Elasticsearch unless explicitly disabled for that connection in the UI; this development-only bypass must not be used in production or on untrusted networks. Stored connection configuration is encrypted with AES-GCM and is never returned with credentials to the browser.

### Persistence

Both modes save connections, Data Views, AI model profiles, and server application settings (`theme` and `default_timezone`, through `/api/settings`). Query results and browser-local field selections are not saved in this store. Browser theme/timezone preferences are not yet synchronized with server settings. Changing modes does not transfer existing records.

| Setting | Database mode | Filesystem mode |
| --- | --- | --- |
| `PERSISTENCE_MODE` | `database` (default) | `filesystem` |
| `DATABASE_URL` | Required PostgreSQL URL | Not used |
| `DB_MAX_CONNECTIONS` | Pool size per replica, default `10` | Not used |
| `FILESYSTEM_PATH` | Not used | Required directory; absolute or relative to the API working directory |
| `CONFIG_ENCRYPTION_KEY` | Required base64-encoded 32 random bytes | Same requirement |
| `CONFIG_ENCRYPTION_KEY_ID` | Key identifier, default `v1` | Same requirement |

Generate a key locally without printing it, for example in PowerShell:

```powershell
$bytes = New-Object byte[] 32
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$rng.GetBytes($bytes)
$rng.Dispose()
$env:CONFIG_ENCRYPTION_KEY = [Convert]::ToBase64String($bytes)
```

Store that key securely before ending the session; this command only sets the current process environment. Do not regenerate it for an existing store. All database replicas must share the same key and key ID; keep secure backups separately from stored objects. Wrong keys fail closed. Automatic key rotation is not implemented.

**Database:** use PostgreSQL for multi-replica deployments. The URL must explicitly use `sslmode=verify-full`; `sslmode=disable` is allowed only for loopback development endpoints. Before starting replicas, run the migration command on a fresh database and after each release that adds schema changes:

```powershell
cargo run -p investigenie-api -- migrate
```

Startup verifies the schema instead of automatically migrating it. Use a migration role for schema changes and a restricted runtime role for normal access. Provision database replication/failover, backups, and restore tests separately; database mode alone does not make the deployment highly available.

**Filesystem:** set `PERSISTENCE_MODE=filesystem` and `FILESYSTEM_PATH`, for example `F:/InvestiGenie/objects` or `./data`. Startup creates an encrypted `configuration.snapshot` in that directory. It uses atomic replacement and a writer lock; only one API instance can open that directory. This is not a shared-filesystem HA backend. The snapshot is limited to 16 MiB. Restrict directory access (Windows ACLs must be set by the operator), keep backups of the snapshot and its separate key, and place custom storage paths outside the source tree. Do not manually edit encrypted objects. Filesystem mode does not need PostgreSQL or an explicit migration.

Updates and deletes require `If-Match: "<revision>"` from the last read. Missing headers return `428`; stale revisions return `409`. The UI handles these headers. Failed or cancelled responses can be ambiguous around a completed commit: reload state before retrying.

### InvestiGenie AI (early investigation MVP)

Add one or more OpenAI-compatible providers from the AI page. Each profile is tested against `{base_url}/chat/completions` in JSON mode with a bounded 15-second timeout before it can be saved. A test sends only a fixed probe prompt and does not include a question, data-view schema, or source data. The profile must return JSON containing `{"ok":true}`. Failed provider responses are bounded and reported without exposing provider response bodies or keys. The default UI remains usable with no profiles saved.

In database mode, provider API keys are encrypted with AES-GCM using the configured encryption key and model-specific associated data. In filesystem mode, profiles and keys are part of the existing authenticated-encrypted snapshot. Model metadata and keys are never saved through public `/api/settings`; model list and mutation responses omit keys. Replacing a key is optional during edits: omitting `api_key` keeps the existing key. Model updates and deletes require the current quoted `If-Match` revision. The endpoint URL must be HTTP(S), contain no embedded credentials, query, or fragment, and fit the configured length limit. Provider hosts are resolved and contacted by the API server; URLs resolving to private network addresses are not blocked, as with the existing operator-configured datasource endpoints. Only configure provider hosts you trust.

In **InvestiGenie AI**, select 1–5 saved Data Views, choose a timeframe, and ask a question. The server, not the model, resolves the selected views, inspects their schemas, builds source-specific structured filters, and executes searches through InvestiGenie connectors. Raw model SQL is not accepted. The default window is the last 30 minutes; clearing it searches All time. Each source is paged sequentially and evidence is compacted into approximately 9 KB cycle buffers. Completed responses report records scanned and source pages processed. Findings remain a bounded rolling summary, not an exhaustive event listing. Source or provider failures fail the request rather than silently claiming complete coverage. Investigations are still synchronous HTTP requests: very large scopes may run for a long time or exceed gateway timeouts, and do not persist resumable background jobs.

Explore loads 50 documents per page, with Next/Previous navigation and a bounded cache of five recent pages in each direction. Pagination keeps the applied query, sort, and time window unchanged until a new search, and reuses the initial histogram instead of recomputing it on every page. Elasticsearch uses a point-in-time snapshot and `search_after` to move beyond the 10,000-result offset window. Abandoned snapshots expire after one minute; expired snapshots require a new search. ClickHouse uses deterministic, bounded LIMIT/OFFSET pages without the old 10,000-offset ceiling, skips repeated histogram queries, and applies per-query execution/resource limits. Deep ClickHouse offsets can still require repeated scans and sorting; they are not snapshot-consistent under concurrent writes. Efficient keyset paging for arbitrary SQL requires a source-specific stable unique key and is not implemented here.

When the provider returns usage metadata, each investigation displays prompt, completion, and total tokens separately for the planning and analysis calls. Providers that omit usage metadata are shown as not reporting counts; InvestiGenie does not estimate them. Counts are currently response-only and are not persisted or attributed to users. A future audit view requires built-in authentication and user identity before per-user accounting is meaningful.

**Provider privacy:** matching event examples and the investigation question are sent to the configured AI provider. Connection credentials and datasource URLs are not sent. Review the provider's retention, training, and regional processing policies before enabling this for sensitive data. Redact secrets and personal data at ingestion/source mapping; truncation alone is not a privacy filter. Configure only a provider you trust. InvestiGenie currently has no built-in user authentication/authorization, so do not expose the AI endpoint to untrusted users; it can query every saved Data View available to this API instance.

The investigator makes at most two model requests per run (plan and findings), is limited to four concurrent runs, a 15-second provider timeout per call, and bounded request/response sizes. Source access still uses InvestiGenie's shared upstream concurrency limit. These limits are initial safeguards, not production capacity guarantees. Provider API keys are runtime model-profile secrets, not public application settings.

`RUST_API_BIND` defaults to `127.0.0.1:3000`, and `WEB_ORIGIN` defaults to `http://127.0.0.1:5173`. Configure `WEB_ORIGIN` to the exact browser origin used by the deployed UI. The browser defaults to the local API URL; configure `VITE_API_BASE_URL` in the Vite environment if the API uses another URL. To bind the API to a network interface for a trusted deployment, set `RUST_API_BIND` explicitly and review network access controls. Authentication and user authorization are intentionally deferred, so do not expose this initial app to untrusted users or networks.

The Vite development server binds to `127.0.0.1:5173`; the API allows browser requests from localhost on that origin only.

## Run locally

In one terminal, start the backend:

```powershell
cargo run -p investigenie-api
```

In another terminal, install web dependencies and start the UI:

```powershell
npm install
npm run dev:web
```

Open the URL printed by Vite. `/api/health` and `/api/health/live` are liveness probes; `/api/health/ready` checks storage readiness. Authentication/authorization are still not implemented: restrict access through an authenticated gateway before any production exposure.

## Checks

```powershell
cargo test -p investigenie-api
npm run test:web
npm run build:web
```

The backend uses bounded upstream concurrency (64 in-flight calls), a 15-second HTTP client timeout, an Elasticsearch per-search timeout, bounded page sizes, and request cancellation when the client disconnects. These are initial guardrails, not production capacity guarantees; measure and tune against an agreed workload before production use.