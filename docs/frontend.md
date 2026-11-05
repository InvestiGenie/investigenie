# Frontend

React 19 + TypeScript + Vite in `frontend/`. No router and no state library: `App.tsx` switches pages, and each page owns its state.

## Pages and components (`frontend/src`)

| File | What it does |
|---|---|
| `App.tsx` | Shell and navigation. **Explore** mode selector and Logs view: Data View picker, query, time window, histogram controls, results table, paging, fields sidebar. |
| `MetricsExplore.tsx` | Prometheus/VictoriaMetrics metric-name discovery, PromQL/MetricsQL range queries, bounded series chart with compact legend and expandable full labels; exposes the current Tempo limitation. |
| `InvestiGenieAIPage.tsx` | Ask a question over 1–5 Data Views; playbook aggregation evidence; saved conversation history and retention; follow-ups; timeout/check-in settings; live progress and findings. |
| `AIModelsPage.tsx` | Add, edit, and remove AI provider profiles. |
| `KnowledgePage.tsx` | Search/filter, read, create, and manage knowledge entries, including editable Investigation Playbook steps. |
| `ConnectionsPage.tsx`, `ConnectionStatus.tsx` | Connection forms built from integration metadata; live status badges. |
| `DataViewsPage.tsx` | Create and edit Data Views (searchable index/table picker, time field, persisted Explore mode). |
| `HistogramChart.tsx`, `histogram.ts` | SVG histogram: intervals, ticks, breakdown stacks, tooltips, drag/click zoom. |
| `SearchableSelect.tsx` | Accessible searchable dropdown for long lists. |
| `TimeRangePicker.tsx`, `DateTimeInput.tsx`, `time.ts` | Time window editor and timezone conversions. |
| `api.ts` | Every API call and type. The only place that talks HTTP. |
| `format.ts` | Shared error-message fallbacks and normal/compact count formatting. |
| `PageFeedback.tsx` | Shared dismissible error alerts and management-page status notices. |
| `query.ts` | Default and field query templates per integration. |
| `preferences.ts` | Browser preferences (theme, timezone, AI settings, chart visibility). |
| `styles.css` | All styles, including the dark theme (`:root[data-theme="dark"]`). |

## Key behaviours

- **Navigation:** internal page changes are recorded in browser history, so Back and Forward move between app pages instead of immediately leaving the tool.
- **Knowledge:** Management > Knowledge opens a searchable list/detail view. Create service context, runbooks, resolved incidents, or an **Investigation Playbook**. Playbooks have up to eight editable steps with target Data Views, connector-native query text, and optional breakdown fields; a default published playbook is seeded and can be edited or archived. An empty top-level scope means global applicability. Write/Preview tabs use `react-markdown` and GFM; raw HTML, external images, and non-HTTP(S) links are disabled. Save conflicts preserve editor text and expose an explicit reload action. Archive is reversible; restore returns to draft, and permanent deletion requires confirmation; the built-in default cannot be deleted. Unsaved changes prompt before switching entries or leaving through app navigation and on browser unload. Only published playbooks run during investigations; publication is not verification.
- **Save investigation knowledge:** a completed scoped conversation with at least one sample-supported turn offers **Save full investigation as Knowledge draft** at the top. It explicitly creates one `resolved_incident` entry with `status: draft`, carrying every user turn and investigation result, evidence IDs/counts, source query scopes, Data View IDs, and caveats. The draft is marked unverified and asks the reviewer to confirm cause and resolution. Oversized content is UTF-8-safe truncated with a notice; the serialized request also respects the API body limit. Saving is never automatic, no additional model call is made, and pending/successful saves disable duplicate clicks. Errors leave the conversation intact and allow retry. **Review draft** opens the saved entry in the Knowledge editor without publishing it. Partial turns are included and labeled; sessions with only explicitly insufficient evidence cannot be promoted. Older scope-less histories keep text-only replies.
- **Metadata-driven:** connection forms, query placeholders, sorting rules, document lookup, and connector brand marks come from each integration's descriptor. The optional `icon` key selects a locally bundled logo for the chooser, saved connections, and Data Views; missing or unknown keys use a generic database icon. Avoid `if (integration === "x")` in UI code.
- **Time:** Explore initially selects the last 15 minutes. The UI keeps UTC values and sends epoch milliseconds. The chosen timezone only changes how times are displayed; it never re-queries.
- **Explore paging:** 50 documents per page, at most two pages (100 documents) per search. The shared API enforces the record ceiling for every connector. Totals and histograms still describe the matching scope, not the displayed sample; lower bounds retain `+` at the cap. The first-page histogram is reused while paging. A new search or graph click/brush resets to page one with a fresh 100-record window in the selected range.
- **Explore modes:** Each Data View stores one mode; the Logs and Metrics pickers show only matching views. Logs retains the document-search workflow and its 50/100 limit. Metrics selects a metric Data View, discovers up to 500 metric names, and issues a bounded range query with a step of 1 second to 1 day. Each response contains at most 50 series × 500 points; truncation and non-finite gaps are shown separately from log totals. The chart legend summarizes each series with its metric name and up to two labels; expand a legend entry to inspect all labels. Table view also shows the full label set. Traces is a disabled placeholder until a Tempo connector and trace-specific API/UI exist. Legacy API responses without a saved mode fall back to the connector's advertised primary mode.
- **Histogram:** Auto targets about 50 bars; a fixed interval that would exceed 100 bars is scaled up. Drag or click to zoom (this updates the time window and searches). Manual intervals need a time window; breakdown works for bounded windows and All time, where the server resolves a shared interval. Elasticsearch fetches page rows separately from its full-scope histogram/exact-count request; later pages reuse that first-page result. If aggregation fails, rows remain available and a lower-bound total is marked with `+`.
- **AI page:** routes non-data conversation to a direct response before playbook or evidence searches. Explicit search/query/find requests cannot be routed to chat. Targeted record searches skip unrelated playbook/histogram work unless requested; if no safe field mapping is possible, the UI asks for clarification instead of scanning broadly. Other data requests run bounded published-playbook aggregations and fresh evidence searches over selected Data Views. Bounded windows compare against the immediately preceding equal-duration window; All time has no baseline. Each data turn in a scoped history conversation reuses the same Data Views/time range and uses prior turns as untrusted context; only new samples support new citations. The composer has an opt-in **Adaptive follow-up** checkbox, off by default; it permits one metadata-validated extra query per data turn. Jobs poll every second (every 5 s while paused), with partial findings at check-ins. Completed conversations persist; the history bar lets you set retention to 1–3650 days (default 30). A top-level **Save full investigation as Knowledge draft** action saves the whole transcript and all turn results as one unverified draft. Expired conversations are pruned on startup, history list/read, new investigations/follow-ups, and settings changes. Scope-less legacy conversations retain text-only follow-ups.
- **Investigation evidence display:** shows distinct sample rows collected, compact sample records supplied to the model, per-query sampling scopes, and source-side aggregate totals separately. Findings are labelled hypotheses; their count is the distinct validated sample citations, not an aggregate total. Old saved results without coverage metadata are labelled as not recorded.
- **Explore document investigations:** every document row can start an investigation. Lookup-mode sources fetch the full document; inline-mode sources use the search hit. The AI prompt includes the document ID, index, Data View and applied time range, with source content capped at 6,000 characters.
- **AI results:** the local `InvestigationResultView` component renders live and saved results. `response_mode: conversation` shows only the direct reply; `response_mode: data_query` shows the exact executed SQL and grouped rows without hypothesis/citation panels; investigation results show usage, confidence/evidence, source scope, caveats, and partial labels. A saved result replaces its live duplicate instead of showing both.
- **AI composer:** model selector, Timeout, and Ask after share one desktop row. The model can flex; duration selects are 76px wide. On small screens the model spans a row above two compact duration controls, without horizontal overflow.
- **Data Views:** choose Logs, Metrics, or a future Traces mode per view; the API accepts only modes advertised by that connection's descriptor. Metric-capable Prometheus and VictoriaMetrics views default to Metrics. The searchable index picker accepts wildcard/comma-separated Elasticsearch patterns and validates fields through the API. ClickHouse tables remain exact names. Connection controls come from integration metadata, including optional ClickHouse execution time; blank creation omits it, clearing on edit sends null.
- **API address:** production builds call the same origin. The dev server calls `http://127.0.0.1:3000` (override with `VITE_API_BASE_URL`).
- **Shared behavior:** `jsonMutation` in `api.ts` constructs JSON mutation bodies and quoted If-Match headers while routes remain explicit. Reuse `errorMessage`/`formatCount` from `format.ts` and `ErrorAlert`/`StatusNotice` from `PageFeedback.tsx`; keep specialized polling/decision flows separate.

## Knowledge workflow

1. Open **Management > Knowledge > New entry** to write service context, a runbook, an incident entry, or an Investigation Playbook. Each step can target Data Views and specify a query and optional breakdown field.
2. Investigation History is separate from Knowledge and follows the configured retention period. Continue a scoped investigation in the same conversation to add context and collect fresh bounded evidence. Click **Save full investigation as Knowledge draft** at the top to preserve every user turn, result, citation, query scope, and caveat beyond history expiry; this copies existing evidence without another model call and does not publish it. Partial turns are retained as partial; sessions with only explicitly insufficient evidence cannot be promoted.
3. Review the entire transcript, investigation reference, findings, evidence, query scopes, caveats, and any truncation notice. Fill in the confirmed cause, resolution, and recovery checks, and adjust the title, type, tags, and applicability before choosing **Published** and saving. Publishing is an editorial decision, not an independent verification result.
4. Use search and status/type/service/environment filters to find entries. **Archive** keeps an entry; **Restore to draft** makes it reviewable again. Permanent deletion requires confirmation. If a revision conflict occurs, preserve your notes and reload the current entry before editing again.

The Knowledge page, built-in playbook, and history-retention setting are shared by everyone who can access the instance; there is no per-user ownership or access control yet. Only published Investigation Playbooks are used by investigations. Storage locations and backups are documented in [Backend](backend.md#storage-upgrades-and-backups).

## Develop and test

```sh
npm install
npm run dev:web          # Vite on http://127.0.0.1:5173 (API must run on :3000)
npm run test:web         # Vitest + Testing Library
npm run build:web        # Type-check and build to frontend/dist
```

Tests mock `api.*` with `vi.spyOn` or stub `fetch`, and query elements by accessible role or label. Keep labels stable when you change markup.
