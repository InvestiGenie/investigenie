# InvestiGenie

**Connect every data source once. Investigate all your logs from one place.**

InvestiGenie is a self-hosted log explorer with an AI investigator. Browse supported log, metric, and SQL sources from one screen. AI investigations currently support Elasticsearch and ClickHouse: they plan from metadata, examine bounded samples and source aggregations, and report findings with evidence references rather than scanning every matching record.

Website: [investigenie.com](https://investigenie.com) · Releases: [GitHub Releases](https://github.com/InvestiGenie/investigenie/releases)

## Features

- **All sources, one UI:** connect as many datasources as you need and save Data Views over any index or table, including Elasticsearch wildcard and comma-separated index patterns.
- **Supported datasources:** Elasticsearch, ClickHouse, BigQuery, Loki, Prometheus, VictoriaLogs, and VictoriaMetrics. Explore capabilities vary by connector; build-time registration alone does not enable AI investigations for a datasource.
- **Explore:** query in each source's own language. You get time windows (last 15 minutes by default) and timezones, sorting, field columns, JSON inspection, and paging past 10,000 results. Elasticsearch counts matches across the entire selected pattern.
- **Histogram:** auto or fixed intervals, breakdown by field (including All time), and drag-to-zoom.
- **InvestiGenie AI:** ask a question across 1–5 Data Views. The AI plans from field metadata, gathers bounded samples and aggregations, forms cited hypotheses, reports token usage, and checks in with you on long runs.
- **AI history:** reopen or delete encrypted conversation history and ask follow-ups using saved findings. Follow-ups do not rescan sources; active scan jobs are not resumed after a restart.
- **Knowledge:** manually create, edit, publish, archive, restore, and delete encrypted service context, runbooks, and resolved incidents. Completed investigations can be explicitly saved as unverified knowledge drafts for review. Search by content and filter by status, type, service, and environment. Automatic investigation retrieval is a later milestone.
- **Bring your own model:** works with any OpenAI-compatible provider, including local ones.
- **Secure by default:** credentials are encrypted at rest, never sent to the browser or the AI, and datasources are only read.
- **Extensible:** datasource adapters live in `integrations/data_connectors/`; AI transports live in `integrations/ai_connectors/`, starting with OpenAI-compatible Chat Completions. An AI skill guides datasource additions.

Shared connector interfaces and registration stay in `backend/src/data_connectors.rs` and `backend/src/ai_connectors.rs`; `integrations/` contains concrete implementations rather than shared contracts.

## Quick start

**Release (one step):** download the zip for your platform from [Releases](https://github.com/InvestiGenie/investigenie/releases), extract it, and run `investigenie-api` (`investigenie-api.exe` on Windows). Then open http://127.0.0.1:3000.

**Docker:**

```sh
docker run -d --name investigenie -p 127.0.0.1:3000:3000 -v investigenie-data:/data ghcr.io/investigenie/investigenie:<version>
```

GHCR pull access depends on package visibility and permissions. If anonymous pulls return 401, authenticate with a pull-authorized account or build locally with `docker compose up --build`; publishing a GitHub release does not itself make a container package public.

**From source:** see [Contributing](CONTRIBUTING.md#setup).

Then:
1. **Connections:** add a supported datasource.
2. **Data Views:** pick a connection, index or table, and time field.
3. **Explore** your data, or add a provider under **AI Models** and ask **InvestiGenie AI** over supported Elasticsearch or ClickHouse Data Views.
4. **Knowledge:** create entries manually, or reopen a completed investigation in AI history and choose **Save as knowledge draft**, then **Review knowledge draft**. Review and publish entries yourself; completed investigations do not automatically add knowledge, and investigations do not retrieve knowledge yet. See the [knowledge workflow](docs/frontend.md#knowledge-workflow).

> Release binaries are unsigned. On macOS, run `xattr -d com.apple.quarantine investigenie-api` once. On Windows, choose **More info → Run anyway**.

## Configuration

With no settings, InvestiGenie runs in **quickstart mode**: encrypted data and a generated key are stored in `investigenie_data/` (or `/data` in Docker). That is fine for one user on one machine.

For shared use, set these as environment variables or in a `.env` file next to the binary:

| Variable | Use |
|---|---|
| `PERSISTENCE_MODE` | `filesystem` (one instance) or `database` (PostgreSQL, multiple replicas). |
| `CONFIG_ENCRYPTION_KEY` | Base64 of 32 random bytes. Store it safely, because losing it means losing saved data. |
| `FILESYSTEM_PATH` / `DATABASE_URL` | Where to store data. In database mode, run `investigenie-api migrate` once per release. |
| `RUST_API_BIND` | Listen address, default `127.0.0.1:3000`. |

Full list: [Backend → Configuration](docs/backend.md#configuration). Datasource and AI credentials are entered only in the UI, never in configuration files.

## Security notes

- **No built-in login yet.** Keep the app on localhost or behind an authenticated gateway. Anyone who can reach it can query every saved Data View.
- **AI data sharing.** Investigations send your question and compacted records to the chosen provider. Credentials and URLs are never sent. Review the provider's data policies and redact sensitive data at the source.
- **Hypotheses, not verdicts.** Findings come with evidence; confirm them before you act.

## Documentation

| Doc | Covers |
|---|---|
| [Architecture](docs/architecture.md) | How the parts fit, request flows, deployment shapes. |
| [Backend](docs/backend.md) | Rust modules, API routes, AI pipeline, configuration. |
| [Frontend](docs/frontend.md) | React pages, components, behaviours, tests. |
| [Integrations](docs/integrations.md) | Add a datasource, with the AI skill or by hand. |
| [Contributing](CONTRIBUTING.md) | Setup, checks, conventions, releases. |

## License

[Apache License 2.0](LICENSE).
