import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from './App';
import { api } from './api';
import { clearPreference } from './preferences';
import { clickhouseIntegration, connectionFixture, elasticsearchIntegration, thirdIntegration } from './test-fixtures';

function jsonResponse(body: unknown): Response {
  return {
    ok: true,
    status: 200,
    json: async () => body,
  } as Response;
}

function enterDateTime(label: string, value: string) {
  const [date, time] = value.split('T');
  fireEvent.change(screen.getByLabelText(`${label} date`), { target: { value: date } });
  fireEvent.change(screen.getByLabelText(`${label} time`), { target: { value: time } });
}

function renderExplore() {
  const rendered = render(<App />);
  fireEvent.click(screen.getByRole('button', { name: 'Explore' }));
  return rendered;
}

describe('Discover', () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
    clearPreference('theme');
    clearPreference('nav-expanded');
    clearPreference('timezone');
    document.documentElement.dataset.theme = 'light';
    vi.spyOn(api, 'aiModels').mockResolvedValue([]);
    vi.spyOn(api, 'aiConversations').mockResolvedValue([]);
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      if (String(input).endsWith('/status')) return jsonResponse({ status: 'connected' });
      throw new Error(`Unexpected request: ${String(input)}`);
    }));
  });

  afterEach(() => vi.restoreAllMocks());

  it('renders an empty histogram for an applied week-long timeframe', async () => {
    vi.spyOn(api, 'connections').mockResolvedValue([connectionFixture(elasticsearchIntegration)]);
    vi.spyOn(api, 'dataViews').mockResolvedValue([{ id: 'view', revision: 'r1', name: 'Events', connection_id: 'source', index: 'events', time_field: '@timestamp' }]);
    vi.spyOn(api, 'fields').mockResolvedValue([{ name: '@timestamp', field_type: 'date' }]);
    const search = vi.spyOn(api, 'search').mockResolvedValue({ took: 1, timed_out: false, total: 0, total_relation: 'eq', hits: [], histogram: [] });
    renderExplore();
    await screen.findByText('No timestamped documents match this query.');
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    enterDateTime('Start', '2026-10-01T00:00:00');
    enterDateTime('End', '2026-10-08T00:00:00');
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByRole('img', { name: 'Document counts over time' })).toBeInTheDocument();
    expect(screen.queryByText('No timestamped documents match this query.')).not.toBeInTheDocument();
    expect(search.mock.calls.at(-1)?.[1].time_range).toEqual({ start: Date.parse('2026-10-01T00:00:00Z'), end: Date.parse('2026-10-08T00:00:00Z') });
  });

  it('sends Kibana-style interval and breakdown controls, zooms by clicking a bucket, and hides the chart', async () => {
    if (!window.PointerEvent) Object.defineProperty(window, 'PointerEvent', { value: MouseEvent, configurable: true });
    clearPreference('histogram-visible');
    vi.spyOn(api, 'connections').mockResolvedValue([connectionFixture(elasticsearchIntegration)]);
    vi.spyOn(api, 'dataViews').mockResolvedValue([{ id: 'view', revision: 'r1', name: 'Events', connection_id: 'source', index: 'events', time_field: '@timestamp' }]);
    vi.spyOn(api, 'fields').mockResolvedValue([
      { name: '@timestamp', field_type: 'date' }, { name: 'service', field_type: 'keyword' }, { name: 'message', field_type: 'text' },
    ]);
    const start = Date.parse('2026-10-06T08:00:00Z');
    const search = vi.spyOn(api, 'search').mockImplementation(async (_connection, body) => ({
      took: 1, timed_out: false, total: 3, total_relation: 'eq', hits: [],
      histogram_interval: body.histogram_interval ?? null,
      histogram: [{ timestamp: start, label: null, count: 3, ...(body.histogram_breakdown_field ? { breakdown: [{ key: 'api', count: 2 }, { key: '__other__', count: 1 }] } : {}) }],
    }));
    renderExplore();
    await screen.findByRole('img', { name: 'Document counts over time' });
    expect(screen.getByLabelText('Histogram interval')).toBeDisabled();
    expect(search.mock.calls.at(-1)?.[1]).not.toHaveProperty('histogram_interval');
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    enterDateTime('Start', '2026-10-06T08:00:00');
    enterDateTime('End', '2026-10-06T09:00:00');
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(search.mock.calls.at(-1)?.[1]).toMatchObject({ histogram_interval: '1m', time_zone: 'UTC' }));
    expect(await screen.findByRole('option', { name: 'Auto - 1 minute' })).toBeInTheDocument();
    expect(within(screen.getByLabelText('Break down by')).getAllByRole('option').map((option) => option.textContent)).toEqual(['No breakdown', 'service']);

    fireEvent.change(screen.getByLabelText('Histogram interval'), { target: { value: '1s' } });
    await waitFor(() => expect(search.mock.calls.at(-1)?.[1]).toMatchObject({ histogram_interval: '1m' }));
    expect(await screen.findByRole('note')).toHaveTextContent('Scaled to 1 minute');
    fireEvent.change(screen.getByLabelText('Histogram interval'), { target: { value: '1h' } });
    await waitFor(() => expect(search.mock.calls.at(-1)?.[1]).toMatchObject({ histogram_interval: '1h' }));
    expect(screen.queryByRole('note')).not.toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Break down by'), { target: { value: 'service' } });
    await waitFor(() => expect(search.mock.calls.at(-1)?.[1]).toMatchObject({ histogram_breakdown_field: 'service' }));
    expect(await screen.findByRole('list', { name: 'Breakdown legend' })).toHaveTextContent('apiOther');

    const plot = document.querySelector('.histogram-interaction') as SVGRectElement;
    fireEvent.pointerDown(plot, { clientX: 60, button: 0 });
    fireEvent.pointerUp(plot, { clientX: 60 });
    await waitFor(() => expect(search.mock.calls.at(-1)?.[1].time_range).toEqual({ start, end: start + 3_600_000 - 1 }));
    expect(screen.getByRole('button', { name: 'Edit time window' })).toHaveTextContent(/08:00 - .*08:59/);

    fireEvent.click(screen.getByRole('button', { name: 'Hide chart' }));
    expect(screen.queryByRole('img', { name: 'Document counts over time' })).not.toBeInTheDocument();
    expect(screen.queryByLabelText('Histogram interval')).not.toBeInTheDocument();
    expect(window.localStorage.getItem('investigenie-histogram-visible')).toBe('false');
    fireEvent.click(screen.getByRole('button', { name: 'Show chart' }));
    expect(screen.getByRole('img', { name: 'Document counts over time' })).toBeInTheDocument();
  });

  it('pages using the applied query, reuses histograms, caches previous pages, and resets on search', async () => {
    const page = (id: string, cursor: string | null) => ({ took: 2, timed_out: false, total: 101, total_relation: 'eq', hits: [{ index: 'events', id, score: null, source: { message: `${id} message` } }], histogram: [], next_cursor: cursor });
    vi.spyOn(api, 'connections').mockResolvedValue([connectionFixture(elasticsearchIntegration)]);
    vi.spyOn(api, 'dataViews').mockResolvedValue([{ id: 'view', revision: 'r1', name: 'Events', connection_id: 'source', index: 'events', time_field: '@timestamp' }]);
    vi.spyOn(api, 'fields').mockResolvedValue([{ name: '@timestamp', field_type: 'date' }]);
    const search = vi.spyOn(api, 'search').mockResolvedValueOnce({ ...page('first-page', 'cursor-one'), histogram: [{ timestamp: 1000, label: null, count: 101 }] }).mockResolvedValueOnce(page('second-page', null)).mockResolvedValueOnce(page('fresh-search', null));
    renderExplore();
    expect(await screen.findByText('first-page')).toBeInTheDocument();
    expect(search.mock.calls[0][1]).toMatchObject({ cursor: '', from: 0, size: 50 });
    fireEvent.change(screen.getByLabelText('Lucene query'), { target: { value: 'unsent edit' } });
    fireEvent.click(screen.getByRole('button', { name: 'Next page' }));
    expect(await screen.findByText('second-page')).toBeInTheDocument();
    expect(search.mock.calls[1][1]).toMatchObject({ cursor: 'cursor-one', include_histogram: false, query: '' });
    expect(screen.getByText('Page 2 · 1 documents')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();
    expect(await screen.findByRole('img', { name: 'Document counts over time' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Previous page' }));
    expect(screen.getByText('first-page')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Next page' }));
    expect(screen.getByText('second-page')).toBeInTheDocument();
    expect(search).toHaveBeenCalledTimes(2);
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByText('fresh-search')).toBeInTheDocument();
    expect(search.mock.calls[2][1]).toMatchObject({ cursor: '', query: 'unsent edit' });
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
  });

  it('retains the current page when continuation fails and allows retry', async () => {
    vi.spyOn(api, 'connections').mockResolvedValue([connectionFixture(elasticsearchIntegration)]);
    vi.spyOn(api, 'dataViews').mockResolvedValue([{ id: 'view', revision: 'r1', name: 'Events', connection_id: 'source', index: 'events', time_field: '@timestamp' }]);
    vi.spyOn(api, 'fields').mockResolvedValue([]);
    const result = { took: 1, timed_out: false, total: 2, total_relation: 'eq', hits: [{ index: 'events', id: 'kept-page', score: null, source: { message: 'kept-page message' } }], histogram: [], next_cursor: 'retry-cursor' };
    const search = vi.spyOn(api, 'search').mockResolvedValueOnce(result).mockRejectedValueOnce(new Error('Snapshot expired; run a new search')).mockResolvedValueOnce({ ...result, hits: [], next_cursor: null });
    renderExplore();
    await screen.findByText('kept-page');
    fireEvent.click(screen.getByRole('button', { name: 'Next page' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Snapshot expired');
    expect(screen.getByText('kept-page')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Next page' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled());
    expect(search).toHaveBeenCalledTimes(3);
    expect(screen.getByText('kept-page')).toBeInTheDocument();
  });

  it('checks each saved datasource when navigating to Connections without consuming list responses', async () => {
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith('/api/connections')) return jsonResponse({ connections: [
        connectionFixture(elasticsearchIntegration),
        connectionFixture(clickhouseIntegration, { id: 'warehouse', name: 'Warehouse' }),
      ] });
      if (url.endsWith('/api/data-views')) return jsonResponse({ data_views: [] });
      if (url.endsWith('/source/status')) return jsonResponse({ status: 'not_connected' });
      if (url.endsWith('/status')) return jsonResponse({ status: 'connected' });
      throw new Error(`Unexpected request: ${url}`);
    });
    render(<App />);
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    fireEvent.click(screen.getByRole('button', { name: 'Connections' }));
    expect(await screen.findByText('Not connected')).toBeInTheDocument();
    expect(await screen.findByText('Connected')).toBeInTheDocument();
    expect(screen.getByText('Warehouse')).toBeInTheDocument();
    expect(fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/status'))).toHaveLength(2);
  });

  it('opens InvestiGenie AI as the default home page and shows the empty state when no Data Views exist', async () => {
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input, init) => {
      const url = String(input);
      if (url.endsWith('/api/connections')) return jsonResponse({ connections: [] });
      if (url.endsWith('/api/data-views')) return jsonResponse({ data_views: [] });
      if (url.endsWith('/api/ai/investigate')) return { ok: false, status: 503, json: async () => ({ error: { code: 'ai_not_configured', message: 'AI investigation is not configured on this server' } }) } as Response;
      throw new Error(`Unexpected request: ${url} ${init?.method ?? 'GET'}`);
    });
    render(<App />);
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(screen.getByRole('heading', { name: 'InvestiGenie AI', level: 1 })).toBeInTheDocument();
    expect(screen.getByLabelText('InvestiGenie').tagName).toBe('IMG');
    expect(screen.getByLabelText('InvestiGenie')).toHaveAttribute('src', expect.stringMatching(/^data:image\/svg\+xml/));
    expect(await screen.findByText('Create a Data View before starting an investigation.')).toBeInTheDocument();
  });

  it('opens AI model management from the Management nav and the AI empty state', async () => {
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith('/api/connections')) return jsonResponse({ connections: [] });
      if (url.endsWith('/api/data-views')) return jsonResponse({ data_views: [] });
      if (url.endsWith('/api/ai/models')) return jsonResponse({ models: [] });
      throw new Error(`Unexpected request: ${url}`);
    });
    renderExplore();
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    fireEvent.click(screen.getByRole('button', { name: 'AI Models' }));
    expect(await screen.findByRole('heading', { name: 'Saved AI models' })).toBeInTheDocument();
    expect(await screen.findByText('No AI models configured')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'InvestiGenie AI' }));
    expect(await screen.findByText('No saved AI models')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Open AI model management' }));
    expect(await screen.findByRole('heading', { name: 'Saved AI models' })).toBeInTheDocument();
  });

  it('keeps Discover empty and error-free when startup requests fail without a selected view', async () => {
    vi.mocked(fetch).mockRejectedValue(new TypeError('Failed to fetch'));
    renderExplore();
    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalledTimes(2));
    expect(screen.getByLabelText('DATA VIEW')).toHaveValue('');
    expect(screen.getByRole('button', { name: 'Create data view' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Search' })).toBeDisabled();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(vi.mocked(fetch).mock.calls.some(([url]) => /\/fields$|\/search$/.test(String(url)))).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'Data Views' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Connections' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(vi.mocked(fetch)).toHaveBeenCalledTimes(2);
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Cannot reach the InvestiGenie API');
    expect(screen.getAllByRole('alert')).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Explore' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Create data view' })).toBeInTheDocument();
  });

  it('loads a saved data view and fields, then searches and displays a document hit', async () => {
    const fetchMock = vi.mocked(fetch);
    fetchMock
      .mockResolvedValueOnce(jsonResponse({ connections: [connectionFixture(elasticsearchIntegration, { id: 'logs-local', name: 'Local logs' })] }))
      .mockResolvedValueOnce(jsonResponse({ data_views: [{ id: 'logs-view', revision: 'logs-revision', name: 'Application logs', connection_id: 'logs-local', index: 'logs-*', time_field: '@timestamp' }] }))
      .mockResolvedValueOnce(jsonResponse({ fields: [
        { name: '@timestamp', field_type: 'date' },
        { name: 'service', field_type: 'object' },
        { name: 'service.name', field_type: 'keyword' },
        { name: 'host', field_type: 'object' },
        { name: 'host.name', field_type: 'keyword' },
        { name: 'items', field_type: 'nested' },
        { name: 'items.name', field_type: 'keyword' },
      ] }))
      .mockResolvedValueOnce(jsonResponse({
        took: 2,
        timed_out: false,
        total: 1,
        total_relation: 'eq',
        hits: [],
        histogram: [{ timestamp: 1791400000000, label: '2026-10-07T20:26:40.000Z', count: 1 }],
      }))
      .mockResolvedValueOnce(jsonResponse({
        took: 4,
        timed_out: false,
        total: 1,
        total_relation: 'eq',
        hits: [{ index: 'logs-2026', id: 'doc-1', score: 0.8, source: { message: 'GET /health', service: { name: 'gateway' } } }],
        histogram: [{ timestamp: 1791400000000, label: '2026-10-07T20:26:40.000Z', count: 1 }],
      }));

    renderExplore();
    expect(await screen.findByText('service.name')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add host to selected fields' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add service to selected fields' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add items to selected fields' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add host.name to selected fields' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add items.name to selected fields' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: 'Available Fields' }).querySelector('.field-count')).toHaveTextContent('3');
    const dataViewHeader = screen.getByLabelText('DATA VIEW').closest('.query-topline') as HTMLElement;
    expect(dataViewHeader).toHaveTextContent('Application logs');
    expect(dataViewHeader).not.toHaveTextContent('logs-*');
    expect(dataViewHeader).not.toHaveTextContent('@timestamp');
    expect(await screen.findByRole('img', { name: 'Document counts over time' })).toBeInTheDocument();
    expect(JSON.parse(String(fetchMock.mock.calls[3][1]?.body))).toMatchObject({
      index: 'logs-*',
      query: '',
      histogram_field: '@timestamp',
    });
    fireEvent.change(screen.getByLabelText('Lucene query'), { target: { value: 'service.name:gateway' } });
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));

    expect(await screen.findByText('GET /health')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();
    expect(screen.queryByRole('heading', { name: 'Documents' })).not.toBeInTheDocument();
    expect(document.querySelector('.results-heading')).toHaveTextContent('Took 4 ms');
    expect(screen.getByRole('region', { name: 'Documents' })).toHaveAttribute('tabindex', '0');
    expect(screen.getAllByRole('columnheader').map((header) => header.textContent)).toEqual(['@timestamp', 'Document']);
    expect(screen.queryByText('logs-2026')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Expand document doc-1' }));
    expect(document.querySelector('.expanded-row td')).toHaveAttribute('colspan', '2');
    expect(await screen.findAllByText('service.name')).toHaveLength(2);
    expect(screen.getByText('gateway')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Open full document' })).toBeInTheDocument();
    const availableFields = screen.getByRole('region', { name: 'Available Fields' });
    const selectedFields = screen.getByRole('region', { name: 'Selected Fields' });
    expect(within(selectedFields).getByRole('img', { name: '@timestamp is the required time field' })).toBeInTheDocument();
    expect(within(availableFields).queryByText('@timestamp', { exact: true })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Remove @timestamp from selected fields' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add @timestamp to selected fields' })).not.toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Data View Fields' })).toBeInTheDocument();
    fireEvent.click(within(availableFields).getByRole('button', { name: 'Add service.name to selected fields' }));
    expect(within(availableFields).queryByRole('button', { name: 'Add service.name to selected fields' })).not.toBeInTheDocument();
    expect(within(selectedFields).getByRole('button', { name: 'Remove service.name from selected fields' })).toHaveTextContent('-');
    expect(screen.getByRole('columnheader', { name: 'service.name' })).toBeInTheDocument();
    expect(document.querySelector('.selected-field-cell')).toHaveTextContent('gateway');
    expect(document.querySelector('.expanded-row td')).toHaveAttribute('colspan', '3');
    expect(screen.getByLabelText('Lucene query')).toHaveValue('service.name:gateway');
    fireEvent.click(within(selectedFields).getByRole('button', { name: 'Remove service.name from selected fields' }));
    expect(screen.queryByRole('columnheader', { name: 'service.name' })).not.toBeInTheDocument();
    expect(within(selectedFields).getByRole('img', { name: '@timestamp is the required time field' })).toBeInTheDocument();
    expect(screen.getAllByRole('columnheader', { name: '@timestamp' })).toHaveLength(1);
    expect(within(availableFields).getByRole('button', { name: 'Add service.name to selected fields' })).toHaveTextContent('+');
    expect(document.querySelector('.expanded-row td')).toHaveAttribute('colspan', '2');
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(5));
    expect(JSON.parse(String(fetchMock.mock.calls[4][1]?.body))).toMatchObject({
      index: 'logs-*',
      query: 'service.name:gateway',
      size: 50,
      histogram_field: '@timestamp',
    });
    expect(String(fetchMock.mock.calls[4][0])).toContain('/api/connections/logs-local/search');
    fetchMock.mockRejectedValueOnce(new TypeError('Failed to fetch'));
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Cannot reach the InvestiGenie API');
  });

  it('switches query language and searches the selected saved data view', async () => {
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith('/api/integrations')) return jsonResponse({ integrations: [
        elasticsearchIntegration, clickhouseIntegration,
      ] });
      if (url.endsWith('/api/connections')) return jsonResponse({ connections: [
        connectionFixture(elasticsearchIntegration, { id: 'es-local', name: 'Logs' }),
        connectionFixture(clickhouseIntegration, { id: 'ch-warehouse', name: 'Warehouse', url: 'http://localhost:8123', database: 'analytics' }),
      ] });
      if (url.endsWith('/api/data-views')) return jsonResponse({ data_views: [
        { id: 'logs-view', revision: 'logs-revision', name: 'Logs', connection_id: 'es-local', index: 'logs-*', time_field: '@timestamp' },
        { id: 'warehouse-view', revision: 'warehouse-revision', name: 'Warehouse events', connection_id: 'ch-warehouse', index: 'events', time_field: 'created_at' },
      ] });
      if (url.endsWith('/api/connections/es-local/indices/logs-*/fields')) return jsonResponse({ fields: [{ name: '@timestamp', field_type: 'date' }] });
      if (url.endsWith('/api/connections/es-local/search')) return jsonResponse({ took: 1, timed_out: false, total: 0, total_relation: 'eq', hits: [], histogram: [] });
      if (url.endsWith('/api/connections/ch-warehouse/indices/events/fields')) return jsonResponse({ fields: [{ name: 'event_id', field_type: 'UInt64' }, { name: 'details', field_type: 'JSON' }, { name: 'tags', field_type: 'Array(String)' }, { name: 'optional', field_type: 'Nullable(String)' }] });
      if (url.endsWith('/api/connections/ch-warehouse/search')) return jsonResponse({ took: 2, timed_out: false, total: 1, total_relation: 'eq', columns: ['event_id', 'created_at', 'details', 'tags', 'optional'], hits: [{ index: 'events', id: 'row-1', score: null, source: { event_id: 42, created_at: '2026-10-08 00:00:00', details: { active: true }, tags: ['api', 'gateway'], optional: null } }], histogram: [{ timestamp: 1791417600000, label: null, count: 250 }] });
      throw new Error(`Unexpected request: ${url}`);
    });

    renderExplore();
    expect(await screen.findByText('0 matching documents')).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('DATA VIEW'), { target: { value: 'warehouse-view' } });

    expect(await screen.findByRole('textbox', { name: 'SQL query' })).toBeInTheDocument();
    expect(await screen.findByRole('button', { name: 'Expand document row-1' })).toBeInTheDocument();
    expect(screen.getByRole('navigation', { name: 'Document pages' })).toBeInTheDocument();
    expect(document.querySelector('.results-heading')).toHaveTextContent('Took 2 ms');
    expect(screen.queryByRole('heading', { name: 'Documents' })).not.toBeInTheDocument();
    expect(screen.getByRole('columnheader', { name: 'Document' })).toBeInTheDocument();
    expect(screen.getByRole('columnheader', { name: 'created_at' })).toBeInTheDocument();
    expect(within(screen.getByRole('region', { name: 'Selected Fields' })).getByRole('img', { name: 'created_at is the required time field' })).toBeInTheDocument();
    expect(within(screen.getByRole('region', { name: 'Available Fields' })).queryByText('created_at', { exact: true })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Remove created_at from selected fields' })).not.toBeInTheDocument();
    expect(screen.getAllByRole('columnheader').map((header) => header.textContent)).toEqual(['created_at', 'Document']);
    expect(screen.getByText('2026-10-08 00:00:00')).toBeInTheDocument();
    expect(screen.queryByRole('columnheader', { name: 'event_id' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Expand document row-1' }));
    expect(screen.getByText('42')).toBeInTheDocument();
    expect(screen.getByText('details.active')).toBeInTheDocument();
    expect(screen.getByText('["api","gateway"]')).toBeInTheDocument();
    expect(await screen.findByRole('img', { name: 'Document counts over time' })).toBeInTheDocument();
    expect(screen.getByText('created_at · 250 matching documents')).toBeInTheDocument();
    expect(screen.getAllByText('event_id')).toHaveLength(2);
    const requestsBeforeOpen = fetchMock.mock.calls.length;
    fireEvent.click(screen.getByRole('button', { name: 'Open full document' }));
    const documentDialog = screen.getByRole('dialog');
    expect(within(documentDialog).queryByText('events', { exact: true })).not.toBeInTheDocument();
    expect(within(documentDialog).getByText('Returned by ClickHouse query')).toBeInTheDocument();
    expect(JSON.parse(documentDialog.querySelector('pre')?.textContent ?? '')).toEqual({ event_id: 42, created_at: '2026-10-08 00:00:00', details: { active: true }, tags: ['api', 'gateway'], optional: null });
    expect(fetchMock.mock.calls.length).toBe(requestsBeforeOpen);
    fireEvent.click(screen.getByRole('button', { name: 'Close document' }));
    fireEvent.click(screen.getByRole('button', { name: 'Add event_id to selected fields' }));
    fireEvent.click(screen.getByRole('button', { name: 'Add details to selected fields' }));
    fireEvent.click(screen.getByRole('button', { name: 'Add tags to selected fields' }));
    fireEvent.click(screen.getByRole('button', { name: 'Add optional to selected fields' }));
    expect(screen.getAllByRole('columnheader').map((header) => header.textContent)).toEqual(['created_at', 'Document', 'event_id', 'details', 'tags', 'optional']);
    expect([...document.querySelectorAll('.selected-field-cell')].map((cell) => cell.textContent)).toEqual(['42', '{"active":true}', '["api","gateway"]', '—']);
    expect(document.querySelector('.expanded-row td')).toHaveAttribute('colspan', '6');
    expect(fetchMock.mock.calls.length).toBe(requestsBeforeOpen);
    expect(screen.getByLabelText('SQL query')).toHaveValue('');
    expect(screen.queryByLabelText('Sort')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Document preview has no single sortable field' })).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: 'Sort event_id ascending' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Sort event_id descending' })).toBeEnabled());
    expect(screen.getByRole('columnheader', { name: /event_id/ })).toHaveAttribute('aria-sort', 'ascending');
    let sqlSort = fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/api/connections/ch-warehouse/search')).at(-1);
    expect(JSON.parse(String(sqlSort?.[1]?.body))).toMatchObject({ sort_by: 'event_id', sort_order: 'asc', from: 0 });
    fireEvent.click(screen.getByRole('button', { name: 'Sort event_id descending' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Sort event_id ascending' })).toBeEnabled());
    expect(screen.getByRole('columnheader', { name: /event_id/ })).toHaveAttribute('aria-sort', 'descending');
    sqlSort = fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/api/connections/ch-warehouse/search')).at(-1);
    expect(JSON.parse(String(sqlSort?.[1]?.body))).toMatchObject({ sort_by: 'event_id', sort_order: 'desc' });
    fireEvent.change(screen.getByLabelText('Filter fields'), { target: { value: 'event_id' } });
    expect(screen.getByRole('button', { name: 'Remove event_id from selected fields' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Remove details from selected fields' })).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Filter fields'), { target: { value: '' } });
    fireEvent.change(screen.getByLabelText('DATA VIEW'), { target: { value: 'logs-view' } });
    await screen.findByText('0 matching documents');
    expect(within(screen.getByRole('region', { name: 'Selected Fields' })).getByRole('img', { name: '@timestamp is the required time field' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Remove event_id from selected fields' })).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('DATA VIEW'), { target: { value: 'warehouse-view' } });
    await screen.findByRole('button', { name: 'Remove event_id from selected fields' });
    await screen.findByRole('button', { name: 'Expand document row-1' });
    expect(screen.getByRole('columnheader', { name: 'details' })).toBeInTheDocument();
    await waitFor(() => expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith('/api/connections/ch-warehouse/search'))).toBe(true));
    const sqlSearch = fetchMock.mock.calls.find(([url]) => String(url).endsWith('/api/connections/ch-warehouse/search'));
    expect(JSON.parse(String(sqlSearch?.[1]?.body))).toMatchObject({
      index: 'events',
      query: 'SELECT * FROM `events` LIMIT 50',
      histogram_field: 'created_at',
    });
    fetchMock.mockResolvedValueOnce(jsonResponse({
      took: 1, timed_out: false, total: 1, total_relation: 'eq', columns: ['total'],
      hits: [{ index: 'events', id: 'row-1', score: null, source: { total: 250 } }],
      histogram: [], histogram_unavailable_reason: "The SQL result does not include the data view's time field.",
    }));
    fireEvent.change(screen.getByLabelText('SQL query'), { target: { value: 'SELECT count() AS total FROM events' } });
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByText("The SQL result does not include the data view's time field.")).toBeInTheDocument();
    expect(screen.queryByRole('img', { name: 'Document counts over time' })).not.toBeInTheDocument();
    expect([...document.querySelectorAll('.selected-field-cell')].map((cell) => cell.textContent)).toEqual(['—', '—', '—', '—']);
    expect(screen.getByRole('button', { name: 'event_id is not in the SQL result' })).toBeDisabled();
  });

  it.each(['inline', 'lookup'] as const)('supports a third source with metadata-driven %s documents and capabilities', async (documentMode) => {
    const inline = documentMode === 'inline';
    const descriptor = { ...thirdIntegration, query: {
      ...thirdIntegration.query, document_mode: documentMode,
      sort_requires_projection: inline, histogram_total_from_buckets: inline,
    } };
    const index = 'audit"events';
    const field = 'actor"name';
    const source = { created_at: '2026-10-08T09:00:00Z', [field]: 'reader' };
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith('/connections')) return jsonResponse({ connections: [connectionFixture(descriptor)] });
      if (url.endsWith('/data-views')) return jsonResponse({ data_views: [{ id: 'view', revision: 'view-revision', name: 'Archived events', connection_id: 'source', index, time_field: 'created_at' }] });
      if (url.endsWith('/fields')) return jsonResponse({ fields: [{ name: 'created_at', field_type: 'date' }, { name: field, field_type: 'keyword' }] });
      if (url.includes('/documents/')) return jsonResponse({ index, id: 'record-1', found: true, source: { ...source, detail: 'Looked up' } });
      if (url.endsWith('/search')) return jsonResponse({
        took: 2, timed_out: false, total: 1, total_relation: 'eq', columns: ['created_at'],
        histogram: [{ timestamp: Date.parse(source.created_at), count: 37, label: null }],
        hits: [{ index, id: 'record-1', score: null, source }],
      });
      throw new Error(`Unexpected request: ${url}`);
    });
    renderExplore();
    await screen.findByRole('button', { name: 'Expand document record-1' });
    const queryInput = screen.getByLabelText('ArchiveQL query');
    expect(queryInput).toHaveAttribute('placeholder', 'READ "audit""events"');
    expect(queryInput).toHaveValue('');
    expect(screen.getByText('Read stored archive events')).toBeInTheDocument();
    expect(screen.getByText('Fields loaded from Archive collection schema')).toBeInTheDocument();
    expect(document.querySelector('.histogram-heading')).toHaveTextContent(`created_at · ${inline ? 37 : 1} matching documents`);
    const searches = () => fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/search'));
    expect(JSON.parse(String(searches()[0][1]?.body))).toMatchObject({ index, query: 'READ "audit""events"' });
    expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith('/integrations'))).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: `Add ${field} to selected fields` }));
    if (inline) {
      expect(screen.getByRole('button', { name: `${field} is not in the ArchiveQL result` })).toBeDisabled();
    } else {
      fireEvent.click(screen.getByRole('button', { name: `Sort ${field} ascending` }));
      await waitFor(() => expect(screen.getByRole('button', { name: `Sort ${field} descending` })).toBeEnabled());
      expect(JSON.parse(String(searches().at(-1)?.[1]?.body))).toMatchObject({ sort_by: field, sort_order: 'asc' });
    }
    fireEvent.click(screen.getByRole('button', { name: 'Expand document record-1' }));
    const beforeDocument = fetchMock.mock.calls.length;
    fireEvent.click(screen.getByRole('button', { name: 'Open full document' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(inline ? 'Returned by Archive query' : 'Retrieved from Archive')).toBeInTheDocument();
    expect(JSON.parse(dialog.querySelector('pre')?.textContent ?? '')).toEqual(inline ? source : { ...source, detail: 'Looked up' });
    expect(fetchMock.mock.calls.length).toBe(beforeDocument + (inline ? 0 : 1));
    if (!inline) expect(String(fetchMock.mock.calls.at(-1)?.[0])).toContain('/indices/audit%22events/documents/record-1');
    fireEvent.click(screen.getByRole('button', { name: 'Close document' }));
    fireEvent.click(screen.getByRole('button', { name: /actor"name keyword/ }));
    expect(queryInput).toHaveValue('READ "audit""events" PROJECT "actor""name"');
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Search' })).toBeEnabled());
    expect(JSON.parse(String(searches().at(-1)?.[1]?.body))).toMatchObject({ query: 'READ "audit""events" PROJECT "actor""name"' });
    fireEvent.change(queryInput, { target: { value: 'READ custom FILTER severity:high' } });
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Search' })).toBeEnabled());
    expect(JSON.parse(String(searches().at(-1)?.[1]?.body))).toMatchObject({ query: 'READ custom FILTER severity:high' });
    expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith('/integrations'))).toBe(false);
  });

  it('lists only saved views in Discover after creation and clears removed views', async () => {
    let saved = false;
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input, init) => {
      const url = String(input);
      if (url.endsWith('/api/connections')) return jsonResponse({ connections: [
        connectionFixture(elasticsearchIntegration),
      ] });
      if (url.endsWith('/api/data-views')) {
        if (init?.method === 'POST') saved = true;
        return jsonResponse({ data_views: saved ? [
          { id: 'saved-logs', revision: 'logs-revision', name: 'Saved logs', connection_id: 'source', index: 'logs', time_field: 'created_at' },
        ] : [] });
      }
      if (url.endsWith('/api/data-views/saved-logs') && init?.method === 'DELETE') {
        saved = false;
        return jsonResponse({});
      }
      if (url.endsWith('/indices')) return jsonResponse({ indices: [{ name: 'logs', document_count: '10', health: 'green' }] });
      if (url.endsWith('/fields')) return jsonResponse({ fields: [{ name: 'created_at', field_type: 'date' }] });
      if (url.endsWith('/search')) return jsonResponse({ took: 1, timed_out: false, total: 1, total_relation: 'eq', hits: [{ index: 'logs', id: 'sort-doc', score: null, source: { created_at: '2026-10-08 00:00:00' } }], histogram: [] });
      throw new Error(`Unexpected request: ${url}`);
    });
    renderExplore();
    expect(await screen.findByRole('option', { name: 'No data views available' })).toBeInTheDocument();
    expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith('/indices'))).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'Data Views' }));
    fireEvent.click(screen.getByRole('button', { name: /Add data view/ }));
    fireEvent.change(screen.getByLabelText('Data view name'), { target: { value: 'Saved logs' } });
    fireEvent.change(screen.getByLabelText('Connection'), { target: { value: 'source' } });
    fireEvent.click(screen.getByRole('combobox', { name: 'Index / table' }));
    fireEvent.click(await screen.findByRole('option', { name: 'logs' }));
    await waitFor(() => expect(screen.getByLabelText('Time series time field')).toHaveValue('created_at'));
    fireEvent.click(screen.getByRole('button', { name: 'Create data view' }));
    await screen.findByText('Data view validated and created.');
    fireEvent.click(screen.getByRole('button', { name: 'Explore' }));
    expect(screen.getByLabelText('DATA VIEW')).toHaveValue('saved-logs');
    expect(screen.getAllByRole('option', { name: 'Saved logs' })).toHaveLength(1);
    expect(screen.queryByRole('option', { name: 'logs' })).not.toBeInTheDocument();
    await screen.findByText('1 matching documents');
    const search = fetchMock.mock.calls.find(([url]) => String(url).endsWith('/search'));
    expect(JSON.parse(String(search?.[1]?.body))).toMatchObject({ index: 'logs', histogram_field: 'created_at' });
    expect(screen.queryByLabelText('Sort')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Sort created_at ascending' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Sort created_at descending' })).toBeEnabled());
    expect(screen.getByRole('columnheader', { name: /created_at/ })).toHaveAttribute('aria-sort', 'ascending');
    expect(fetchMock.mock.calls.some(([url, init]) => String(url).endsWith('/search') && JSON.parse(String(init?.body)).sort_by === 'created_at' && JSON.parse(String(init?.body)).sort_order === 'asc')).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'Sort created_at descending' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Sort created_at ascending' })).toBeEnabled());
    expect(screen.getByRole('columnheader', { name: /created_at/ })).toHaveAttribute('aria-sort', 'descending');
    fireEvent.click(screen.getByRole('button', { name: 'Data Views' }));
    fireEvent.click(screen.getByRole('button', { name: 'Remove Saved logs' }));
    await screen.findByText('Saved logs removed.');
    fireEvent.click(screen.getByRole('button', { name: 'Explore' }));
    expect(screen.getByLabelText('DATA VIEW')).toHaveValue('');
    expect(screen.queryByRole('option', { name: 'Saved logs' })).not.toBeInTheDocument();
    expect(screen.queryByText('1 matching documents')).not.toBeInTheDocument();
  });

  it('keeps documents beyond ten in the scroll region and displays response timing beside the count', async () => {
    vi.mocked(fetch).mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith('/connections')) return jsonResponse({ connections: [connectionFixture(elasticsearchIntegration)] });
      if (url.endsWith('/data-views')) return jsonResponse({ data_views: [{ id: 'view', revision: 'view-revision', name: 'Logs', connection_id: 'source', index: 'logs', time_field: '@timestamp' }] });
      if (url.endsWith('/fields')) return jsonResponse({ fields: [{ name: '@timestamp', field_type: 'date' }] });
      if (url.endsWith('/search')) return jsonResponse({
        took: 52, timed_out: true, total: 12, total_relation: 'eq', histogram: [],
        hits: Array.from({ length: 12 }, (_, index) => ({ index: 'logs', id: `doc-${index + 1}`, score: null, source: { message: `Record ${index + 1}` } })),
      });
      throw new Error(`Unexpected request: ${url}`);
    });
    renderExplore();
    await screen.findByRole('button', { name: 'Expand document doc-12' });
    const viewport = screen.getByRole('region', { name: 'Documents' });
    expect(within(viewport).getAllByRole('button', { name: /^Expand document/ })).toHaveLength(12);
    expect(document.querySelector('.results-heading')).toHaveTextContent('12 matching documents');
    expect(document.querySelector('.results-heading')).toHaveTextContent('Took 52 ms');
    expect(document.querySelector('.results-heading')).toHaveTextContent('Timed out; results may be partial');
    expect(screen.queryByRole('heading', { name: 'Documents' })).not.toBeInTheDocument();
    fireEvent.click(within(viewport).getByRole('button', { name: 'Expand document doc-12' }));
    expect(within(viewport).getByRole('button', { name: 'Open full document' })).toBeInTheDocument();
  });

  it.each(['elasticsearch', 'clickhouse'])('applies, retains, validates, and clears time windows for %s', async (connectionType) => {
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith('/connections')) return jsonResponse({ connections: [connectionFixture(connectionType === 'clickhouse' ? clickhouseIntegration : elasticsearchIntegration)] });
      if (url.endsWith('/data-views')) return jsonResponse({ data_views: [{ id: 'view', revision: 'view-revision', name: 'Events', connection_id: 'source', index: 'events', time_field: 'created_at' }] });
      if (url.endsWith('/fields')) return jsonResponse({ fields: [{ name: 'created_at', field_type: 'date' }] });
      if (url.endsWith('/search')) return jsonResponse({ took: 1, timed_out: false, total: 1, total_relation: 'eq', histogram: [], columns: ['created_at'], hits: [{ index: 'events', id: 'doc-1', score: null, source: { created_at: '2026-10-08 09:30:00' } }] });
      throw new Error(`Unexpected request: ${url}`);
    });
    renderExplore();
    await screen.findByRole('button', { name: 'Expand document doc-1' });
    const toolbar = document.querySelector('.query-topline') as HTMLElement;
    expect([...toolbar.children].map((element) => element.tagName)).toEqual(['LABEL', 'SELECT', 'DIV']);
    const initialRequests = fetchMock.mock.calls.length;
    const start = '2026-10-08T09:00:00';
    const end = '2026-10-08T10:00:00';
    expect(screen.queryByLabelText('Start date')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    enterDateTime('Start', start);
    enterDateTime('End', end);
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(fetchMock.mock.calls.length).toBe(initialRequests);
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Search' })).toBeEnabled());
    const latestSearch = () => JSON.parse(String(fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/search')).at(-1)?.[1]?.body));
    expect(latestSearch()).toMatchObject({ histogram_field: 'created_at', time_range: { start: Date.parse(`${start}Z`), end: Date.parse(`${end}Z`) } });
    fireEvent.click(screen.getByRole('button', { name: 'Sort created_at ascending' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Sort created_at descending' })).toBeEnabled());
    expect(latestSearch()).toMatchObject({ sort_by: 'created_at', sort_order: 'asc', time_range: { start: Date.parse(`${start}Z`), end: Date.parse(`${end}Z`) } });
    const beforeInvalid = fetchMock.mock.calls.length;
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    enterDateTime('Start', '2026-10-08T11:00:00');
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('start no later than end');
    expect(fetchMock.mock.calls.length).toBe(beforeInvalid);
    fireEvent.click(screen.getByRole('button', { name: 'Clear time window' }));
    expect(screen.getByText('All time')).toBeInTheDocument();
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Search' })).toBeEnabled());
    expect(latestSearch()).not.toHaveProperty('time_range');
  });

  it.each(['elasticsearch', 'clickhouse'])('shares the chosen timezone across dates and graph for %s without shifting search instants', async (connectionType) => {
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith('/connections')) return jsonResponse({ connections: [connectionFixture(connectionType === 'clickhouse' ? clickhouseIntegration : elasticsearchIntegration)] });
      if (url.endsWith('/data-views')) return jsonResponse({ data_views: [{ id: 'view', revision: 'view-revision', name: 'Events', connection_id: 'source', index: 'events', time_field: 'created_at' }] });
      if (url.endsWith('/fields')) return jsonResponse({ fields: [{ name: 'created_at', field_type: connectionType === 'clickhouse' ? 'DateTime' : 'date' }, { name: 'updated_at', field_type: connectionType === 'clickhouse' ? 'DateTime64(3)' : 'date' }] });
      const source = { created_at: '2026-10-06T08:18:00Z', updated_at: '2026-10-06T09:18:00Z' };
      if (url.includes('/documents/')) return jsonResponse({ index: 'events', id: 'doc-1', found: true, source });
      if (url.endsWith('/search')) return jsonResponse({ took: 1, timed_out: false, total: 1, total_relation: 'eq', columns: ['created_at', 'updated_at'], histogram: [{ timestamp: Date.parse(source.created_at), count: 1, label: null }], hits: [{ index: 'events', id: 'doc-1', score: null, source }] });
      throw new Error(`Unexpected request: ${url}`);
    });
    const rendered = renderExplore();
    await screen.findByRole('button', { name: 'Expand document doc-1' });
    fireEvent.click(screen.getByRole('button', { name: 'Add updated_at to selected fields' }));
    fireEvent.click(screen.getByRole('button', { name: 'Expand document doc-1' }));
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    enterDateTime('Start', '2026-10-06T08:00:00');
    enterDateTime('End', '2026-10-06T10:00:00');
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    const requestsBeforeZoneChange = fetchMock.mock.calls.length;
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    fireEvent.change(screen.getByLabelText('Timezone'), { target: { value: 'Europe/Berlin' } });
    expect(fetchMock.mock.calls.length).toBe(requestsBeforeZoneChange);
    expect(screen.getByLabelText('Start date')).toHaveValue('2026-10-06');
    expect(screen.getByLabelText('Start time')).toHaveValue('10:00:00');
    expect(screen.getByLabelText('End time')).toHaveValue('12:00:00');
    expect(document.querySelector('.time-cell')).toHaveTextContent('2026-10-06 10:18:00');
    expect(document.querySelector('.selected-field-cell')).toHaveTextContent('2026-10-06 11:18:00');
    expect(document.querySelector('.source-fields')).toHaveTextContent('2026-10-06 10:18:00');
    expect(document.querySelector('.histogram-interval')).toHaveTextContent('Europe/Berlin');
    expect(window.localStorage.getItem('investigenie-timezone')).toBe('Europe/Berlin');
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    fireEvent.click(screen.getByRole('button', { name: 'Open full document' }));
    const dialog = await screen.findByRole('dialog');
    expect(JSON.parse(dialog.querySelector('pre')?.textContent ?? '')).toEqual({ created_at: '2026-10-06T08:18:00Z', updated_at: '2026-10-06T09:18:00Z' });
    fireEvent.click(screen.getByRole('button', { name: 'Close document' }));
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Search' })).toBeEnabled());
    const latest = fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/search')).at(-1);
    expect(JSON.parse(String(latest?.[1]?.body))).toMatchObject({ time_range: { start: Date.parse('2026-10-06T08:00:00Z'), end: Date.parse('2026-10-06T10:00:00Z') } });
    rendered.unmount();
    renderExplore();
    await screen.findByRole('button', { name: 'Expand document doc-1' });
    expect(document.querySelector('.time-cell')).toHaveTextContent('2026-10-06 10:18:00');
    expect(screen.getByRole('button', { name: 'Edit time window' })).toHaveAttribute('title', 'Search time window in Europe/Berlin');
  });

  it('toggles dark mode and restores the saved choice on remount', async () => {
    const firstRender = render(<App />);
    const darkModeButton = screen.getByRole('button', { name: 'Switch to dark mode' });
    fireEvent.click(darkModeButton);

    await waitFor(() => expect(document.documentElement.dataset.theme).toBe('dark'));
    expect(window.localStorage.getItem('investigenie-theme')).toBe('dark');
    expect(screen.getByRole('button', { name: 'Switch to light mode' })).toHaveAttribute('aria-pressed', 'true');

    firstRender.unmount();
    renderExplore();
    expect(await screen.findByRole('button', { name: 'Switch to light mode' })).toHaveAttribute('aria-pressed', 'true');
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('starts at InvestiGenie AI and orders Explore, Dashboard, and Management navigation', async () => {
    render(<App />);
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    expect(screen.getByRole('heading', { name: 'InvestiGenie AI', level: 1 })).toHaveClass('workspace-title');
    const primaryButtons = within(screen.getByRole('navigation', { name: 'Main menu' })).getAllByRole('button');
    expect(primaryButtons.slice(0, 3).map((button) => button.getAttribute('aria-label'))).toEqual(['InvestiGenie AI', 'Explore', 'Dashboard']);
    expect(screen.queryByLabelText('CONNECTION')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Refresh' })).not.toBeInTheDocument();
    expect(document.querySelector('.rail-bottom .status-light')).toBeNull();
    expect(document.querySelector('.app-shell')).not.toHaveClass('nav-expanded');

    fireEvent.click(screen.getByRole('button', { name: 'Expand menu' }));

    expect(document.querySelector('.app-shell')).toHaveClass('nav-expanded');
    expect(screen.getByText('Explore', { selector: '.nav-label' })).toBeInTheDocument();
    expect(screen.getByText('Dashboard', { selector: '.nav-label' })).toBeInTheDocument();
    expect(screen.getByText('Connections', { selector: '.nav-label' })).toBeInTheDocument();
    expect(screen.getByText('Data Views', { selector: '.nav-label' })).toBeInTheDocument();
    const management = screen.getByRole('group', { name: 'Management' });
    expect(within(management).getAllByRole('button').map((button) => button.getAttribute('aria-label'))).toEqual(['AI Models', 'Connections', 'Data Views']);
    expect(within(management).getByRole('button', { name: 'Connections' })).toBeInTheDocument();
    expect(within(management).getByRole('button', { name: 'Data Views' })).toBeInTheDocument();
    expect(within(management).getByRole('button', { name: 'AI Models' })).toBeInTheDocument();
    expect(within(management).queryByRole('button', { name: 'Explore' })).not.toBeInTheDocument();
    fireEvent.click(within(management).getByRole('button', { name: 'Connections' }));
    expect(screen.getByRole('heading', { name: 'Connections', level: 1 })).toBeInTheDocument();
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'Explore' }));
    expect(screen.getByRole('heading', { name: 'Explore', level: 1 })).toBeInTheDocument();
    fireEvent.click(within(management).getByRole('button', { name: 'Data Views' }));
    expect(screen.getByRole('heading', { name: 'Data Views', level: 1 })).toBeInTheDocument();
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    await waitFor(() => expect(window.localStorage.getItem('investigenie-nav-expanded')).toBe('true'));
  });
});