# Frontend

React 19 + TypeScript + Vite in `frontend/`. No router and no state library: `App.tsx` switches pages, and each page owns its state.

## Pages and components (`frontend/src`)

| File | What it does |
|---|---|
| `App.tsx` | Shell and navigation. **Explore**: Data View picker, query, time window, histogram controls, results table, paging, fields sidebar. |
| `InvestiGenieAIPage.tsx` | Ask a question over 1–5 Data Views; saved conversation history; follow-ups; timeout/check-in settings; live progress and findings. |
| `AIModelsPage.tsx` | Add, edit, and remove AI provider profiles. |
| `KnowledgePage.tsx` | Search/filter, read, create, edit, publish, archive, restore, and permanently delete knowledge entries. |
| `ConnectionsPage.tsx`, `ConnectionStatus.tsx` | Connection forms built from integration metadata; live status badges. |
| `DataViewsPage.tsx` | Create and edit Data Views (searchable index/table picker, time field). |
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
- **Knowledge:** Management > Knowledge opens a searchable list/detail view. Create an entry with a title, type, status, Markdown content, optional comma-separated services/environments/tags, source link, and searchable connection/Data View checklists. An empty scope means global applicability. Write/Preview tabs use `react-markdown` and GFM; raw HTML, external images, and non-HTTP(S) links are disabled. Save conflicts preserve editor text and expose an explicit reload action. Archive is reversible; restore returns to draft, and permanent deletion requires confirmation. Unsaved changes prompt before switching entries or leaving through app navigation and on browser unload. Publishing is not verification, and entries are not yet sent to investigations.
- **Save investigation knowledge:** completed live results and completed results reopened from AI history offer **Save as knowledge draft**. This explicitly creates a `resolved_incident` entry with `status: draft`, carrying the original question, investigation/model reference, findings, evidence IDs/counts, source query scope, Data View IDs, and caveats. The text is marked unverified and asks the reviewer to confirm cause and resolution. Oversized content is UTF-8-safe truncated with a notice; the serialized request also respects the API body limit. Saving is never automatic, no additional model call is made, and pending/successful saves disable duplicate clicks. Errors leave the investigation intact and allow retry. **Review knowledge draft** navigates to the saved entry in the Knowledge editor without publishing it. Partial/running/failed results cannot be promoted; older history without a completeness flag remains compatible.
- **Metadata-driven:** connection forms, query placeholders, sorting rules, and document lookup come from each integration's descriptor. Avoid `if (integration === "x")` in UI code.
- **Time:** Explore initially selects the last 15 minutes. The UI keeps UTC values and sends epoch milliseconds. The chosen timezone only changes how times are displayed; it never re-queries.
- **Explore paging:** 50 documents per page. The histogram comes from the first page and is reused while paging.
- **Histogram:** Auto targets about 50 bars; a fixed interval that would exceed 100 bars is scaled up. Drag or click to zoom (this updates the time window and searches). Manual intervals need a time window; breakdown works for bounded windows and All time, where the server resolves a shared interval. Explore Elasticsearch searches request exact matching totals; any lower-bound response is marked with `+`.
- **AI page:** starts a job, polls every second (every 5 s while paused), and shows partial findings at check-ins. Completed conversations are persisted; selecting one restores its transcript, Data Views, and model, and follow-ups use saved findings without rescanning sources.
- **Explore document investigations:** every document row can start an investigation. Lookup-mode sources fetch the full document; inline-mode sources use the search hit. The AI prompt includes the document ID, index, Data View and applied time range, with source content capped at 6,000 characters.
- **AI results:** the local `InvestigationResultView` component renders both live and saved results with usage, confidence/evidence, source scope, caveats, and partial labels. A saved result replaces its live duplicate instead of showing both.
- **AI composer:** model selector, Timeout, and Ask after share one desktop row. The model can flex; duration selects are 76px wide. On small screens the model spans a row above two compact duration controls, without horizontal overflow.
- **Data Views:** the searchable picker accepts wildcard/comma-separated Elasticsearch patterns and validates fields through the API. ClickHouse tables remain exact names. Connection controls come from integration metadata, including optional ClickHouse execution time; blank creation omits it, clearing on edit sends null.
- **API address:** production builds call the same origin. The dev server calls `http://127.0.0.1:3000` (override with `VITE_API_BASE_URL`).
- **Shared behavior:** `jsonMutation` in `api.ts` constructs JSON mutation bodies and quoted If-Match headers while routes remain explicit. Reuse `errorMessage`/`formatCount` from `format.ts` and `ErrorAlert`/`StatusNotice` from `PageFeedback.tsx`; keep specialized polling/decision flows separate.

## Knowledge workflow

1. Open **Management > Knowledge > New entry** to write service context, a runbook, or an incident entry manually. Set applicability with services, environments, and optional connection/Data View checklists; empty scope arrays mean global applicability.
2. For an existing case, reopen it from **AI conversation history** or use its completed live result. Click **Save as knowledge draft** once, then **Review knowledge draft**. This copies existing evidence into an unverified draft; it does not call the model again or publish anything. Partial or failed results do not offer this action.
3. Review the question, investigation reference, findings, evidence, query scope, caveats, and any truncation notice. Fill in the confirmed cause, resolution, and recovery checks, and adjust the title, type, tags, and applicability before choosing **Published** and saving. Publishing is an editorial decision, not an independent verification result.
4. Use search and status/type/service/environment filters to find entries. **Archive** keeps an entry; **Restore to draft** makes it reviewable again. Permanent deletion requires confirmation. If a revision conflict occurs, preserve your notes and reload the current entry before editing again.

The knowledge page is shared by everyone who can access the instance; there is no per-user ownership or access control yet. Entries are not currently retrieved by investigations, so publishing one does not change later AI answers. Storage locations, backups, and the PostgreSQL upgrade requirement are documented in [Backend](backend.md#storage-upgrades-and-backups).

## Develop and test

```sh
npm install
npm run dev:web          # Vite on http://127.0.0.1:5173 (API must run on :3000)
npm run test:web         # Vitest + Testing Library
npm run build:web        # Type-check and build to frontend/dist
```

Tests mock `api.*` with `vi.spyOn` or stub `fetch`, and query elements by accessible role or label. Keep labels stable when you change markup.
