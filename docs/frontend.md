# Frontend

React 19 + TypeScript + Vite in `frontend/`. No router and no state library: `App.tsx` switches pages, and each page owns its state.

## Pages and components (`frontend/src`)

| File | What it does |
|---|---|
| `App.tsx` | Shell and navigation. **Explore**: Data View picker, query, time window, histogram controls, results table, paging, fields sidebar. |
| `InvestiGenieAIPage.tsx` | Ask a question over 1–5 Data Views; saved conversation history; follow-ups; timeout/check-in settings; live progress and findings. |
| `AIModelsPage.tsx` | Add, edit, and remove AI provider profiles. |
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

## Develop and test

```sh
npm install
npm run dev:web          # Vite on http://127.0.0.1:5173 (API must run on :3000)
npm run test:web         # Vitest + Testing Library
npm run build:web        # Type-check and build to frontend/dist
```

Tests mock `api.*` with `vi.spyOn` or stub `fetch`, and query elements by accessible role or label. Keep labels stable when you change markup.
