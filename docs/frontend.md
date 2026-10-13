# Frontend

React 19 + TypeScript + Vite in `frontend/`. No router and no state library: `App.tsx` switches pages, and each page owns its state.

## Pages and components (`frontend/src`)

| File | What it does |
|---|---|
| `App.tsx` | Shell and navigation. **Explore**: Data View picker, query, time window, histogram controls, results table, paging, fields sidebar. |
| `InvestiGenieAIPage.tsx` | Ask a question over 1–5 Data Views; timeout and check-in settings; live progress; continue/stop; findings. |
| `AIModelsPage.tsx` | Add, edit, and remove AI provider profiles. |
| `ConnectionsPage.tsx`, `ConnectionStatus.tsx` | Connection forms built from integration metadata; live status badges. |
| `DataViewsPage.tsx` | Create and edit Data Views (searchable index/table picker, time field). |
| `HistogramChart.tsx`, `histogram.ts` | SVG histogram: intervals, ticks, breakdown stacks, tooltips, drag/click zoom. |
| `SearchableSelect.tsx` | Accessible searchable dropdown for long lists. |
| `TimeRangePicker.tsx`, `DateTimeInput.tsx`, `time.ts` | Time window editor and timezone conversions. |
| `api.ts` | Every API call and type. The only place that talks HTTP. |
| `query.ts` | Default and field query templates per integration. |
| `preferences.ts` | Browser preferences (theme, timezone, AI settings, chart visibility). |
| `styles.css` | All styles, including the dark theme (`:root[data-theme="dark"]`). |

## Key behaviours

- **Metadata-driven:** connection forms, query placeholders, sorting rules, and document lookup come from each integration's descriptor. Avoid `if (integration === "x")` in UI code.
- **Time:** the UI keeps UTC values and sends epoch milliseconds. The chosen timezone only changes how times are displayed; it never re-queries.
- **Explore paging:** 50 documents per page. The histogram comes from the first page and is reused while paging.
- **Histogram:** Auto targets about 50 bars; a fixed interval that would exceed 100 bars is scaled up. Drag or click to zoom (this updates the time window and searches). Interval and breakdown need a time window.
- **AI page:** starts a job, polls every second (every 5 s while paused), and shows partial findings at check-ins.
- **API address:** production builds call the same origin. The dev server calls `http://127.0.0.1:3000` (override with `VITE_API_BASE_URL`).

## Develop and test

```sh
npm install
npm run dev:web          # Vite on http://127.0.0.1:5173 (API must run on :3000)
npm run test:web         # Vitest + Testing Library
npm run build:web        # Type-check and build to frontend/dist
```

Tests mock `api.*` with `vi.spyOn` or stub `fetch`, and query elements by accessible role or label. Keep labels stable when you change markup.
