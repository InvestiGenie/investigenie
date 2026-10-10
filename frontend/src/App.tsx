import { Fragment, lazy, Suspense, useEffect, useRef, useState, type FormEvent } from 'react';
import { ArrowDown, ArrowUp, ArrowUpDown, BrainCircuit, ChevronLeft, ChevronRight } from 'lucide-react';
import { api, makeSearchRequest, type ConnectionSummary, type DataViewSummary, type DocumentResult, type FieldSummary, type SearchResult } from './api';
import ConnectionsPage from './ConnectionsPage';
import DataViewsPage from './DataViewsPage';
import TimeRangePicker from './TimeRangePicker';
import { formatTimestamp, initialTimeZone, isDateField } from './time';
import { applyQueryTemplate, resolveSearchQuery } from './query';

const HistogramChart = lazy(() => import('./HistogramChart'));
const NorthstarAIPage = lazy(() => import('./NorthstarAIPage'));
const AIModelsPage = lazy(() => import('./AIModelsPage'));

type Theme = 'light' | 'dark';

function initialTheme(): Theme {
  try {
    return window.localStorage.getItem('northstar-theme') === 'dark' ? 'dark' : 'light';
  } catch {
    return 'light';
  }
}

function displayValue(value: unknown): string {
  if (value === null || value === undefined) return '—';
  if (typeof value === 'string') return value;
  if (typeof value === 'object') return JSON.stringify(value);
  return String(value);
}

function formatCount(value: string | null | undefined): string {
  if (!value) return '—';
  const count = Number(value);
  return Number.isFinite(count) ? new Intl.NumberFormat().format(count) : value;
}

function flattenSource(value: unknown, prefix = ''): Array<[string, unknown]> {
  if (Array.isArray(value)) return prefix ? [[prefix, value]] : [];
  if (typeof value === 'object' && value !== null) {
    return Object.entries(value).flatMap(([key, child]) =>
      flattenSource(child, prefix ? `${prefix}.${key}` : key),
    );
  }
  return prefix ? [[prefix, value]] : [];
}

function sourceField(source: Record<string, unknown> | null, name: string): unknown {
  if (!source) return undefined;
  if (Object.hasOwn(source, name)) return source[name];
  let value: unknown = source;
  for (const segment of name.split('.')) {
    if (typeof value !== 'object' || value === null || !Object.hasOwn(value, segment)) return undefined;
    value = (value as Record<string, unknown>)[segment];
  }
  return value;
}

export default function App() {
  const [theme, setTheme] = useState<Theme>(initialTheme);
  const [timeZone, setTimeZone] = useState(initialTimeZone);
  const [navExpanded, setNavExpanded] = useState(() => {
    try {
      return window.localStorage.getItem('northstar-nav-expanded') === 'true';
    } catch {
      return false;
    }
  });
  const [connections, setConnections] = useState<ConnectionSummary[]>([]);
  const [dataViews, setDataViews] = useState<DataViewSummary[]>([]);
  const [selectedDataViewId, setSelectedDataViewId] = useState('');
  const [activePage, setActivePage] = useState<'discover' | 'northstar-ai' | 'connections' | 'data-views' | 'ai-models'>('northstar-ai');
  const [fields, setFields] = useState<FieldSummary[]>([]);
  const [fieldFilter, setFieldFilter] = useState('');
  const [selectedFieldsByView, setSelectedFieldsByView] = useState<Record<string, string[]>>({});
  const [query, setQuery] = useState('');
  const [startDate, setStartDate] = useState('');
  const [endDate, setEndDate] = useState('');
  const [sortBy, setSortBy] = useState('');
  const [sortOrder, setSortOrder] = useState<'asc' | 'desc'>('desc');
  const [results, setResults] = useState<SearchResult | null>(null);
  const [selectedDocument, setSelectedDocument] = useState<DocumentResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [expandedHit, setExpandedHit] = useState<string | null>(null);
  const requestController = useRef<AbortController | null>(null);
  const [pageNumber, setPageNumber] = useState(1);
  const [previousPages, setPreviousPages] = useState<Array<{ number: number; result: SearchResult }>>([]);
  const [forwardPages, setForwardPages] = useState<Array<{ number: number; result: SearchResult }>>([]);
  const [histogramTimeRange, setHistogramTimeRange] = useState<{ start: number; end: number } | undefined>();
  const appliedSearch = useRef<ReturnType<typeof makeSearchRequest> | null>(null);

  function firstPageRequest(body: ReturnType<typeof makeSearchRequest>) {
    return { ...body, cursor: '' };
  }

  function acceptFirstPage(response: SearchResult, body: ReturnType<typeof makeSearchRequest>) {
    appliedSearch.current = body;
    setHistogramTimeRange(body.time_range);
    setResults(response);
    setPageNumber(1);
    setPreviousPages([]);
    setForwardPages([]);
    setExpandedHit(null);
  }

  useEffect(() => {
    try { window.localStorage.setItem('northstar-timezone', timeZone); } catch {}
  }, [timeZone]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try {
      window.localStorage.setItem('northstar-theme', theme);
    } catch {}
  }, [theme]);

  useEffect(() => {
    try {
      window.localStorage.setItem('northstar-nav-expanded', String(navExpanded));
    } catch {}
  }, [navExpanded]);

  useEffect(() => {
    const controller = new AbortController();
    api.connections(controller.signal)
      .then((available) => {
        if (controller.signal.aborted) return;
        setConnections(available);
      })
      .catch(() => {
        if (!controller.signal.aborted) {
          setConnections([]);
        }
      });
    return () => controller.abort();
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    api.dataViews(controller.signal)
      .then((available) => {
        if (controller.signal.aborted) return;
        setDataViews(available);
        if (available.length > 0) setSelectedDataViewId((current) => current || available[0].id);
      })
      .catch(() => {
        if (!controller.signal.aborted) {
          setDataViews([]);
        }
      });
    return () => controller.abort();
  }, []);

  const selectedDataView = dataViews.find((view) => view.id === selectedDataViewId);
  const selectedConnectionId = selectedDataView?.connection_id ?? '';
  const selectedIndex = selectedDataView?.index ?? '';
  const selectedConnection = connections.find((connection) => connection.id === selectedConnectionId);
  const integration = selectedConnection?.integration;
  const queryMetadata = integration?.query;

  useEffect(() => {
    if (!selectedDataView || !selectedConnection) {
      setFields([]);
      setResults(null);
      setLoading(false);
      setError('');
      return;
    }
    setFields([]);
    const fieldsController = new AbortController();
    const searchController = new AbortController();
    requestController.current?.abort();
    requestController.current = searchController;
    setLoading(true);
    setError('');

    async function loadDataView() {
      try {
        const availableFields = await api.fields(selectedConnectionId, selectedIndex, fieldsController.signal);
        if (fieldsController.signal.aborted) return;
        setFields(availableFields);
        const searchQuery = resolveSearchQuery(selectedConnection!.integration.query, selectedIndex, query);
        const body = firstPageRequest(makeSearchRequest(selectedIndex, searchQuery, 0, sortBy, selectedDataView?.time_field, sortOrder, startDate, endDate));
        const response = await api.search(
          selectedConnectionId,
          body,
          searchController.signal,
        );
        if (searchController.signal.aborted) return;
        acceptFirstPage(response, body);
      } catch (reason) {
        if (!fieldsController.signal.aborted && !searchController.signal.aborted) setError(errorMessage(reason));
      } finally {
        if (!searchController.signal.aborted) setLoading(false);
      }
    }

    void loadDataView();
    return () => {
      fieldsController.abort();
      searchController.abort();
    };
  }, [selectedDataView, connections]);

  useEffect(() => () => requestController.current?.abort(), []);

  async function runSearch(
    event?: FormEvent<HTMLFormElement>,
    nextOffset = 0,
    overrides: { sortBy?: string; sortOrder?: 'asc' | 'desc' } = {},
  ) {
    event?.preventDefault();
    if (!selectedIndex || !selectedConnectionId || !queryMetadata) return;
    requestController.current?.abort();
    const controller = new AbortController();
    requestController.current = controller;
    setLoading(true);
    setError('');
    try {
      const body = firstPageRequest(makeSearchRequest(
          selectedIndex,
          resolveSearchQuery(queryMetadata, selectedIndex, query),
          nextOffset,
          overrides.sortBy ?? sortBy,
          selectedDataView?.time_field,
          overrides.sortOrder ?? sortOrder,
          startDate,
          endDate,
        ));
      const response = await api.search(
        selectedConnectionId,
        body,
        controller.signal,
      );
      if (controller.signal.aborted) return;
      acceptFirstPage(response, body);
    } catch (reason) {
      if (!controller.signal.aborted) setError(errorMessage(reason));
    } finally {
      if (!controller.signal.aborted) setLoading(false);
    }
  }

  async function openDocument(index: string, id: string) {
    setError('');
    try {
      if (selectedConnectionId) setSelectedDocument(await api.document(selectedConnectionId, { index, id }));
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function nextPage() {
    if (!results || !appliedSearch.current || loading) return;
    const cached = forwardPages.at(-1);
    if (cached) {
      setPreviousPages((pages) => [...pages, { number: pageNumber, result: results }].slice(-5));
      setResults(cached.result);
      setPageNumber(cached.number);
      setForwardPages((pages) => pages.slice(0, -1));
      setExpandedHit(null);
      return;
    }
    if (!results.next_cursor) return;
    const current = results;
    const controller = new AbortController();
    requestController.current?.abort();
    requestController.current = controller;
    setLoading(true);
    setError('');
    try {
      const response = await api.search(selectedConnectionId, { ...appliedSearch.current, cursor: current.next_cursor!, include_histogram: false }, controller.signal);
      if (controller.signal.aborted) return;
      if (!response.hits.length) {
        setResults({ ...current, next_cursor: null, ...(current.total_relation === 'gte' ? { total: pageNumber * appliedSearch.current!.size, total_relation: 'eq' } : {}) });
        return;
      }
      setPreviousPages((pages) => [...pages, { number: pageNumber, result: current }].slice(-5));
      const total = current.total_relation === 'eq' ? current.total : Math.max(current.total, response.total, pageNumber * appliedSearch.current!.size + response.hits.length);
      const relation = current.total_relation === 'eq' || !response.next_cursor ? 'eq' : 'gte';
      setResults({ ...response, histogram: current.histogram, total, total_relation: relation, histogram_unavailable_reason: current.histogram_unavailable_reason });
      setPageNumber((number) => number + 1);
      setExpandedHit(null);
    } catch (reason) {
      if (!controller.signal.aborted) setError(errorMessage(reason));
    } finally {
      if (!controller.signal.aborted) setLoading(false);
    }
  }

  function previousPage() {
    const page = previousPages.at(-1);
    if (!page || loading) return;
    setResults(page.result);
    if (results) setForwardPages((pages) => [...pages, { number: pageNumber, result: results }].slice(-5));
    setPageNumber(page.number);
    setPreviousPages((pages) => pages.slice(0, -1));
    setExpandedHit(null);
  }

  const histogramField = selectedDataView?.time_field;
  const selectableFields = fields.filter((field) => !['object', 'nested'].includes(field.field_type.toLowerCase()));
  const selectedFields = (selectedFieldsByView[selectedDataViewId] ?? [])
    .filter((name) => name !== histogramField && selectableFields.some((field) => field.name === name));
  const sidebarFields = histogramField && !selectableFields.some((field) => field.name === histogramField)
    ? [{ name: histogramField, field_type: 'date/time' }, ...selectableFields] : selectableFields;
  const selectedSidebarFields = histogramField ? [histogramField, ...selectedFields] : selectedFields;
  const matchingFields = sidebarFields.filter((field) => `${field.name} ${field.field_type}`.toLowerCase().includes(fieldFilter.toLowerCase()));

  function displayField(name: string, value: unknown): string {
    if (name === histogramField || fields.some((field) => field.name === name && isDateField(field.field_type))) {
      if (Array.isArray(value)) return displayValue(value.map((item) => formatTimestamp(item, timeZone) ?? item));
      return formatTimestamp(value, timeZone) ?? displayValue(value);
    }
    return displayValue(value);
  }

  function toggleField(name: string) {
    if (!selectedDataViewId || name === histogramField) return;
    setSelectedFieldsByView((current) => {
      const selected = current[selectedDataViewId] ?? [];
      return { ...current, [selectedDataViewId]: selected.includes(name) ? selected.filter((field) => field !== name) : [...selected, name] };
    });
    if (selectedFields.includes(name) && sortBy === name) {
      setSortBy('');
      if (results) void runSearch(undefined, 0, { sortBy: '' });
    }
  }

  function sortColumn(name: string) {
    const direction = sortBy === name && sortOrder === 'asc' ? 'desc' : 'asc';
    setSortBy(name);
    setSortOrder(direction);
    void runSearch(undefined, 0, { sortBy: name, sortOrder: direction });
  }

  function columnSortButton(name: string) {
    const missingProjectedColumn = queryMetadata?.sort_requires_projection && !results?.columns?.includes(name);
    const direction = sortBy === name && sortOrder === 'asc' ? 'descending' : 'ascending';
    const label = missingProjectedColumn ? `${name} is not in the ${integration?.query_language} result` : `Sort ${name} ${direction}`;
    return <button className="column-sort-button" disabled={loading || missingProjectedColumn} aria-label={label} title={label} onClick={() => sortColumn(name)}>
      {sortBy === name ? sortOrder === 'asc' ? <ArrowUp size={13} aria-hidden="true" /> : <ArrowDown size={13} aria-hidden="true" /> : <ArrowUpDown size={13} aria-hidden="true" />}
    </button>;
  }

  async function refreshConnections() {
    const available = await api.connections();
    setConnections(available);
    await refreshDataViews();
  }

  async function refreshDataViews() {
    const available = await api.dataViews();
    setDataViews(available);
    if (!available.some((view) => view.id === selectedDataViewId)) {
      setSelectedDataViewId(available[0]?.id ?? '');
      setResults(null);
      setFields([]);
    }
  }

  return (
    <div className={`app-shell${navExpanded ? ' nav-expanded' : ''}`}>
      <aside className="rail" aria-label="Primary navigation">
        <div className="rail-brand"><div className="brand-mark" aria-label="Northstar">N</div><span className="nav-label">Northstar</span></div>
        <nav className="rail-nav" aria-label="Main menu">
          <button className={`rail-button${activePage === 'northstar-ai' ? ' active' : ''}`} aria-label="Northstar AI" title="Northstar AI" onClick={() => setActivePage('northstar-ai')}><span className="rail-icon" aria-hidden="true"><BrainCircuit size={19} /></span><span className="nav-label">Northstar AI</span></button>
          <button className={`rail-button${activePage === 'discover' ? ' active' : ''}`} aria-label="Explore" title="Explore" onClick={() => setActivePage('discover')}><span className="rail-icon" aria-hidden="true">⌕</span><span className="nav-label">Explore</span></button>
          <button className="rail-button" aria-label="Dashboard" title="Dashboard" disabled><span className="rail-icon" aria-hidden="true">▦</span><span className="nav-label">Dashboard</span></button>
          <div className="rail-nav-group" role="group" aria-label="Management">
            <span className="rail-group-label">Management</span>
            <button className={`rail-button${activePage === 'ai-models' ? ' active' : ''}`} aria-label="AI Models" title="AI Models" onClick={() => setActivePage('ai-models')}><span className="rail-icon" aria-hidden="true"><BrainCircuit size={18} /></span><span className="nav-label">AI Models</span></button>
            <button className={`rail-button${activePage === 'connections' ? ' active' : ''}`} aria-label="Connections" title="Connections" onClick={() => setActivePage('connections')}><span className="rail-icon" aria-hidden="true">⌘</span><span className="nav-label">Connections</span></button>
            <button className={`rail-button${activePage === 'data-views' ? ' active' : ''}`} aria-label="Data Views" title="Data Views" onClick={() => setActivePage('data-views')}><span className="rail-icon" aria-hidden="true">◫</span><span className="nav-label">Data Views</span></button>
          </div>
        </nav>
        <div className="rail-bottom"><button className="rail-toggle" aria-label={navExpanded ? 'Collapse menu' : 'Expand menu'} title={navExpanded ? 'Collapse menu' : 'Expand menu'} aria-expanded={navExpanded} onClick={() => setNavExpanded((expanded) => !expanded)}><span aria-hidden="true">{navExpanded ? '‹' : '›'}</span><span className="nav-label">{navExpanded ? 'Collapse' : 'Expand menu'}</span></button></div>
      </aside>

      <div className="workspace">
        <header className="topbar">
          <div className="breadcrumb"><h1 className="workspace-title">{activePage === 'connections' ? 'Connections' : activePage === 'data-views' ? 'Data Views' : activePage === 'ai-models' ? 'AI Models' : activePage === 'northstar-ai' ? 'Northstar AI' : 'Explore'}</h1></div>
          <div className="topbar-right"><button className="theme-toggle" type="button" aria-label={`Switch to ${theme === 'light' ? 'dark' : 'light'} mode`} aria-pressed={theme === 'dark'} title={`Switch to ${theme === 'light' ? 'dark' : 'light'} mode`} onClick={() => setTheme((current) => current === 'light' ? 'dark' : 'light')}><span aria-hidden="true">{theme === 'light' ? '☾' : '☀'}</span></button></div>
        </header>

        {activePage === 'northstar-ai'
          ? <Suspense fallback={<div className="loading-state"><span className="spinner dark" /> Loading Northstar AI…</div>}><NorthstarAIPage dataViews={dataViews} onManageModels={() => setActivePage('ai-models')} /></Suspense>
          : activePage === 'ai-models'
            ? <Suspense fallback={<div className="loading-state"><span className="spinner dark" /> Loading AI models…</div>}><AIModelsPage /></Suspense>
          : activePage === 'connections'
          ? <ConnectionsPage connections={connections} onConnectionsChange={refreshConnections} />
          : activePage === 'data-views'
            ? <DataViewsPage connections={connections} dataViews={dataViews} onDataViewsChange={refreshDataViews} />
            : <main className={`discover-layout${histogramField && results ? ' has-histogram' : ''}`}>
          <section className="main-column">
            <section className="query-panel" aria-label="Search controls">
              <div className="query-topline">
                <label className="select-label" htmlFor="data-view-select">DATA VIEW</label>
                <select id="data-view-select" value={selectedDataViewId} onChange={(event) => { setSelectedDataViewId(event.target.value); setQuery(''); setSortBy(''); setSortOrder('desc'); setResults(null); }}>
                  {dataViews.length === 0 && <option value="">No data views available</option>}
                  {dataViews.map((view) => <option key={view.id} value={view.id}>{view.name}</option>)}
                </select>
                <TimeRangePicker start={startDate} end={endDate} timeZone={timeZone} onTimeZoneChange={setTimeZone} onChange={(start, end) => { setStartDate(start); setEndDate(end); setError(''); }} />
              </div>
              <form id="discover-search-form" className="query-form" onSubmit={(event) => void runSearch(event)}>
                <span className="query-prefix" aria-hidden="true">⌕</span>
                <input
                  aria-label={integration ? `${integration.query_language} query` : 'Query'}
                  value={query}
                  onChange={(event) => setQuery(event.target.value)}
                  placeholder={queryMetadata ? applyQueryTemplate(queryMetadata.placeholder, queryMetadata, { index: selectedIndex }) : ''}
                  spellCheck={false}
                />
                <kbd>↵</kbd>
                <button className="search-button" type="submit" disabled={!selectedIndex || !selectedConnection || loading}>
                  {loading ? <span className="spinner" /> : <span aria-hidden="true">⌕</span>}
                  {loading ? 'Searching' : 'Search'}
                </button>
              </form>
              <div className="query-footer"><span className="lucene-badge">{integration?.query_language.toUpperCase()}</span><span>{queryMetadata ? applyQueryTemplate(queryMetadata.help, queryMetadata, { index: selectedIndex, query }) : ''}</span></div>
            </section>

            {error && <div className="alert" role="alert"><span className="alert-symbol">!</span><span>{error}</span><button aria-label="Dismiss error" onClick={() => setError('')}>×</button></div>}

            {histogramField && results && <section className="histogram-panel" aria-labelledby="histogram-title">
              <div className="histogram-heading"><div><h2 id="histogram-title">Documents over time</h2><p>{histogramField} · {formatCount(String(queryMetadata?.histogram_total_from_buckets ? results.histogram.reduce((count, bucket) => count + bucket.count, 0) : results.total))} matching documents</p></div><span className="histogram-interval">{timeZone} · AUTO INTERVAL</span></div>
              {results.histogram.length > 0 || (histogramTimeRange && !results.histogram_unavailable_reason)
                ? <Suspense fallback={<div className="histogram-chart" aria-hidden="true" />}><HistogramChart data={results.histogram} timeZone={timeZone} timeRange={histogramTimeRange} /></Suspense>
                : <div className="histogram-empty">{results.histogram_unavailable_reason ?? 'No timestamped documents match this query.'}</div>}
            </section>}

            <section className="results-section" aria-label="Search results">
              <div className="results-heading">
                <p>{results ? `${formatCount(String(results.total))}${results.total_relation === 'gte' ? '+' : ''} matching documents` : 'Run a search to explore documents'}</p>
                {results && <div className="results-timing"><span className="query-duration">Took {results.took ?? '—'} ms</span>{results.timed_out && <span className="timeout-label">Timed out; results may be partial</span>}</div>}
              </div>

              {!results && !loading && !error && <div className="empty-state"><div className="empty-glyph">⌕</div>{!selectedDataView ? <><h3>No data views available</h3><button className="sample-query" onClick={() => setActivePage('data-views')}>Create data view</button></> : <><h3>Start with a search</h3><p>{queryMetadata ? applyQueryTemplate(queryMetadata.help, queryMetadata, { index: selectedIndex, query }) : ''}</p></>}</div>}
              {loading && <div className="loading-state"><span className="spinner dark" /> Querying {integration?.display_name}…</div>}
              {results && !loading && results.hits.length === 0 && <div className="empty-state compact"><h3>No matching documents</h3><p>Try changing the query and search again.</p></div>}

              {results && results.hits.length > 0 && <>
                <div className="table-wrap" role="region" aria-label="Documents" tabIndex={0}>
                  <table>
                    <thead><tr>
                      <th className="col-time" aria-sort={sortBy === histogramField ? sortOrder === 'asc' ? 'ascending' : 'descending' : 'none'}><span className="column-heading"><span>{histogramField ?? 'Timestamp'}</span>{histogramField && columnSortButton(histogramField)}</span></th>
                      <th className="col-document"><span className="column-heading"><span>Document</span><button className="column-sort-button" disabled aria-label="Document preview has no single sortable field" title="Document preview has no single sortable field"><ArrowUpDown size={13} aria-hidden="true" /></button></span></th>
                      {selectedFields.map((name) => <th className="selected-field-column" key={name} aria-sort={sortBy === name ? sortOrder === 'asc' ? 'ascending' : 'descending' : 'none'}><span className="column-heading"><span>{name}</span>{columnSortButton(name)}</span></th>)}
                    </tr></thead>
                    <tbody>
                      {results.hits.map((hit) => {
                        const hitKey = `${hit.index}:${hit.id}`;
                        const isExpanded = expandedHit === hitKey;
                        const sourceFields = flattenSource(hit.source);
                        const timestamp = histogramField ? sourceField(hit.source, histogramField) : undefined;
                        const message = hit.source?.message ?? hit.source?.['event.original'] ?? hit.source?.['log.original'] ?? hit.source;
                        return <Fragment key={hitKey}>
                          <tr>
                            <td className="time-cell" title={timeZone}>{displayField(histogramField ?? '', timestamp)}</td>
                            <td><button className="doc-toggle" aria-expanded={isExpanded} aria-label={`${isExpanded ? 'Collapse' : 'Expand'} document ${hit.id}`} onClick={() => setExpandedHit(isExpanded ? null : hitKey)}><span className={`disclosure${isExpanded ? ' open' : ''}`} aria-hidden="true">›</span><span className="doc-id">{hit.id}</span></button><span className="doc-message">{displayValue(message)}</span></td>
                            {selectedFields.map((name) => <td className="selected-field-cell" key={name}>{displayField(name, sourceField(hit.source, name))}</td>)}
                          </tr>
                          {isExpanded && <tr className="expanded-row"><td colSpan={2 + selectedFields.length}><div className="source-fields">{sourceFields.map(([name, value]) => <div className="source-field" key={name}><span className="source-field-name">{name}</span><span className="source-field-value">{displayField(name, value)}</span></div>)}</div><button className="open-document" onClick={() => queryMetadata?.document_mode === 'inline' ? setSelectedDocument({ index: hit.index, id: hit.id, found: true, source: hit.source }) : void openDocument(hit.index, hit.id)}>Open full document <span aria-hidden="true">↗</span></button></td></tr>}
                        </Fragment>;
                      })}
                    </tbody>
                  </table>
                </div>
                <nav className="pagination" aria-label="Document pages">
                  <button className="icon-button" aria-label="Previous page" title="Previous page" disabled={loading || !previousPages.length} onClick={previousPage}><ChevronLeft size={16} aria-hidden="true" /></button>
                  <span>Page {pageNumber} · {results.hits.length} documents</span>
                  <button className="icon-button" aria-label="Next page" title="Next page" disabled={loading || (!results.next_cursor && !forwardPages.length)} onClick={() => void nextPage()}><ChevronRight size={16} aria-hidden="true" /></button>
                </nav>
              </>}
            </section>
          </section>

          <aside className="fields-panel" aria-label="Data View Fields">
            <div className="fields-header"><div><p className="eyebrow">DATA VIEW</p><h2>Data View Fields</h2></div><span className="field-count">{sidebarFields.length}</span></div>
            <div className="field-search"><span aria-hidden="true">⌕</span><input aria-label="Filter fields" placeholder="Filter fields" value={fieldFilter} onChange={(event) => setFieldFilter(event.target.value)} /></div>
            {!selectedDataView && <p className="panel-hint">Create a data view to explore its fields.</p>}
            {selectedIndex && fields.length === 0 && <p className="panel-hint">No fields found for this data view.</p>}
            <div className="field-list">{[true, false].map((selected) => <section className="field-section" key={String(selected)} aria-label={selected ? 'Selected Fields' : 'Available Fields'}>
              <div className="field-section-heading"><h3>{selected ? 'Selected Fields' : 'Available Fields'}</h3><span className="field-count">{selected ? selectedSidebarFields.length : sidebarFields.length - selectedSidebarFields.length}</span></div>
              {matchingFields.filter((field) => selectedSidebarFields.includes(field.name) === selected).map((field) => <div className="field-row" key={field.name}>
                <button className="field-item" title={field.name} disabled={!queryMetadata} onClick={() => { if (queryMetadata) setQuery((current) => applyQueryTemplate(queryMetadata.field_query, queryMetadata, { index: selectedIndex, field: field.name, query: current })); }}><span className="field-type">{field.field_type === 'date' ? '◷' : field.field_type === 'text' ? 'T' : '·'}</span><span className="field-name">{field.name}</span><span className="field-kind">{field.field_type}</span></button>
                {field.name === histogramField
                  ? <span className="field-selection-button" role="img" aria-label={`${field.name} is the required time field`} title="Configured time field; cannot be removed">◷</span>
                  : <button className="field-selection-button" aria-label={`${selected ? 'Remove' : 'Add'} ${field.name} ${selected ? 'from' : 'to'} selected fields`} title={`${selected ? 'Remove' : 'Add'} ${field.name} ${selected ? 'from' : 'to'} selected fields`} onClick={() => toggleField(field.name)}>{selected ? '-' : '+'}</button>}
              </div>)}
              {selected && selectedSidebarFields.length === 0 && <p className="panel-hint">No selected fields.</p>}
            </section>)}</div>
            <div className="fields-footnote">{queryMetadata && `Fields loaded from ${applyQueryTemplate(queryMetadata.schema_label, queryMetadata, { index: selectedIndex })}`}</div>
          </aside>
        </main>}
      </div>

      {selectedDocument && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setSelectedDocument(null); }}>
        <section className="document-modal" role="dialog" aria-modal="true" aria-labelledby="document-title">
          <div className="modal-header"><div><p className="eyebrow">DOCUMENT</p><h2 id="document-title">{selectedDocument.id}</h2></div><button className="icon-button close-button" aria-label="Close document" onClick={() => setSelectedDocument(null)}>×</button></div>
          <div className="modal-meta"><span>{selectedDocument.found ? queryMetadata?.document_mode === 'inline' ? `Returned by ${integration?.display_name} query` : `Retrieved from ${integration?.display_name}` : 'Document not found'}</span></div>
          <pre className="json-view">{JSON.stringify(selectedDocument.source, null, 2)}</pre>
        </section>
      </div>}
    </div>
  );
}

function errorMessage(reason: unknown): string {
  return reason instanceof Error ? reason.message : 'An unexpected request error occurred';
}