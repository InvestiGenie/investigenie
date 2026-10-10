# Northstar Discover

A multi-source document explorer with a React/TypeScript browser UI and a Rust/Axum API. Datasource endpoints and credentials are configured in the UI and remain on the server.

## Repository layout

The publishable application lives in `northstar/`. Run the commands below from that directory. If you opened the parent workspace, first run `cd northstar`.

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

## Requirements

- Node.js 20.19+ or 22.12+, with npm.
- Stable Rust toolchain with Cargo.
- A reachable Elasticsearch and/or ClickHouse deployment and credentials with the required read-only access for the data you want to explore. Datasource details are entered in the Connections UI after the app starts.

## Configure the application

Copy `.env.example` to `.env` to configure the application process and persistence. Datasource URLs, databases, and credentials are configured through the Connections UI. The configuration-storage database URL and encryption key are private bootstrap settings; never commit them. The `.env` file is ignored by Git.

Add Elasticsearch connections with an HTTP(S) endpoint and optional Basic Auth or API key. Add ClickHouse connections with its HTTP endpoint (commonly port `8123`), an optional database, and optional username/password. TLS verification remains enabled for Elasticsearch unless explicitly disabled for that connection in the UI; this development-only bypass must not be used in production or on untrusted networks. Stored connection configuration is encrypted with AES-GCM and is never returned with credentials to the browser.

### Persistence

Both modes save connections, Data Views, and server application settings (`theme` and `default_timezone`, through `/api/settings`). Query results and browser-local field selections are not saved in this store. Browser theme/timezone preferences are not yet synchronized with server settings. Changing modes does not transfer existing records.

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

**Database:** use PostgreSQL for multi-replica deployments. The URL must explicitly use `sslmode=verify-full`; `sslmode=disable` is allowed only for loopback development endpoints. Run the migration once before starting replicas:

```powershell
cargo run -p discover-api -- migrate
```

Startup verifies the schema instead of automatically migrating it. Use a migration role for schema changes and a restricted runtime role for normal access. Provision database replication/failover, backups, and restore tests separately; database mode alone does not make the deployment highly available.

**Filesystem:** set `PERSISTENCE_MODE=filesystem` and `FILESYSTEM_PATH`, for example `F:/Northstar/objects` or `./data`. Startup creates an encrypted `configuration.snapshot` in that directory. It uses atomic replacement and a writer lock; only one API instance can open that directory. This is not a shared-filesystem HA backend. The snapshot is limited to 16 MiB. Restrict directory access (Windows ACLs must be set by the operator), keep backups of the snapshot and its separate key, and place custom storage paths outside the source tree. Do not manually edit encrypted objects. Filesystem mode does not need PostgreSQL or an explicit migration.

Updates and deletes require `If-Match: "<revision>"` from the last read. Missing headers return `428`; stale revisions return `409`. The UI handles these headers. Failed or cancelled responses can be ambiguous around a completed commit: reload state before retrying.

`RUST_API_BIND` defaults to `127.0.0.1:3000`, and `WEB_ORIGIN` defaults to `http://127.0.0.1:5173`. Configure `WEB_ORIGIN` to the exact browser origin used by the deployed UI. The browser defaults to the local API URL; configure `VITE_API_BASE_URL` in the Vite environment if the API uses another URL. To bind the API to a network interface for a trusted deployment, set `RUST_API_BIND` explicitly and review network access controls. Authentication and user authorization are intentionally deferred, so do not expose this initial app to untrusted users or networks.

The Vite development server binds to `127.0.0.1:5173`; the API allows browser requests from localhost on that origin only.

## Run locally

In one terminal, start the backend:

```powershell
cargo run -p discover-api
```

In another terminal, install web dependencies and start the UI:

```powershell
npm install
npm run dev:web
```

Open the URL printed by Vite. `/api/health` and `/api/health/live` are liveness probes; `/api/health/ready` checks storage readiness. Authentication/authorization are still not implemented: restrict access through an authenticated gateway before any production exposure.

## Checks

```powershell
cargo test -p discover-api
npm run test:web
npm run build:web
```

The backend uses bounded upstream concurrency (64 in-flight calls), a 15-second HTTP client timeout, an Elasticsearch per-search timeout, bounded page sizes, and request cancellation when the client disconnects. These are initial guardrails, not production capacity guarantees; measure and tune against an agreed workload before production use.