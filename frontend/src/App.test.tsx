import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import App from './App';
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

describe('Discover', () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.removeItem('northstar-theme');
    window.localStorage.removeItem('northstar-nav-expanded');
    window.localStorage.removeItem('northstar-timezone');
    document.documentElement.dataset.theme = 'light';
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      if (String(input).endsWith('/status')) return jsonResponse({ status: 'connected' });
      throw new Error(`Unexpected request: ${String(input)}`);
    }));
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

  it('keeps Discover empty and error-free when startup requests fail without a selected view', async () => {
    vi.mocked(fetch).mockRejectedValue(new TypeError('Failed to fetch'));
    render(<App />);
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
    expect(await screen.findByRole('alert')).toHaveTextContent('Cannot reach the Northstar API');
    expect(screen.getAllByRole('alert')).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Discover' }));
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

    render(<App />);
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
    expect(screen.queryByRole('button', { name: /Previous|Next/ })).not.toBeInTheDocument();
    expect(document.querySelector('.pagination')).toBeNull();
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
    expect(await screen.findByRole('alert')).toHaveTextContent('Cannot reach the Northstar API');
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

    render(<App />);
    expect(await screen.findByText('0 matching documents')).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('DATA VIEW'), { target: { value: 'warehouse-view' } });

    expect(await screen.findByRole('textbox', { name: 'SQL query' })).toBeInTheDocument();
    expect(await screen.findByRole('button', { name: 'Expand document row-1' })).toBeInTheDocument();
    expect(document.querySelector('.pagination')).toBeNull();
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
    render(<App />);
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
    render(<App />);
    expect(await screen.findByRole('option', { name: 'No data views available' })).toBeInTheDocument();
    expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith('/indices'))).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'Data Views' }));
    fireEvent.click(screen.getByRole('button', { name: /Add data view/ }));
    fireEvent.change(screen.getByLabelText('Data view name'), { target: { value: 'Saved logs' } });
    fireEvent.change(screen.getByLabelText('Connection'), { target: { value: 'source' } });
    await screen.findByRole('option', { name: 'logs' });
    fireEvent.change(screen.getByLabelText('Index / table'), { target: { value: 'logs' } });
    await waitFor(() => expect(screen.getByLabelText('Time series time field')).toHaveValue('created_at'));
    fireEvent.click(screen.getByRole('button', { name: 'Create data view' }));
    await screen.findByText('Data view validated and created.');
    fireEvent.click(screen.getByRole('button', { name: 'Discover' }));
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
    fireEvent.click(screen.getByRole('button', { name: 'Discover' }));
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
    render(<App />);
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
    render(<App />);
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
    const rendered = render(<App />);
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
    expect(window.localStorage.getItem('northstar-timezone')).toBe('Europe/Berlin');
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
    render(<App />);
    await screen.findByRole('button', { name: 'Expand document doc-1' });
    expect(document.querySelector('.time-cell')).toHaveTextContent('2026-10-06 10:18:00');
    expect(screen.getByRole('button', { name: 'Edit time window' })).toHaveAttribute('title', 'Search time window in Europe/Berlin');
  });

  it('toggles dark mode and restores the saved choice on remount', async () => {
    const firstRender = render(<App />);
    const darkModeButton = screen.getByRole('button', { name: 'Switch to dark mode' });
    fireEvent.click(darkModeButton);

    await waitFor(() => expect(document.documentElement.dataset.theme).toBe('dark'));
    expect(window.localStorage.getItem('northstar-theme')).toBe('dark');
    expect(screen.getByRole('button', { name: 'Switch to light mode' })).toHaveAttribute('aria-pressed', 'true');

    firstRender.unmount();
    render(<App />);
    expect(await screen.findByRole('button', { name: 'Switch to light mode' })).toHaveAttribute('aria-pressed', 'true');
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('removes the Explore breadcrumb and expands the navigation labels', async () => {
    render(<App />);
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    expect(screen.getByRole('heading', { name: 'Discover', level: 1 })).toHaveClass('workspace-title');
    expect(screen.queryByText('Explore')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('CONNECTION')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Refresh' })).not.toBeInTheDocument();
    expect(document.querySelector('.rail-bottom .status-light')).toBeNull();
    expect(document.querySelector('.app-shell')).not.toHaveClass('nav-expanded');

    fireEvent.click(screen.getByRole('button', { name: 'Expand menu' }));

    expect(document.querySelector('.app-shell')).toHaveClass('nav-expanded');
    expect(screen.getByText('Discover', { selector: '.nav-label' })).toBeInTheDocument();
    expect(screen.getByText('Dashboards', { selector: '.nav-label' })).toBeInTheDocument();
    expect(screen.getByText('Connections', { selector: '.nav-label' })).toBeInTheDocument();
    expect(screen.getByText('Data Views', { selector: '.nav-label' })).toBeInTheDocument();
    const management = screen.getByRole('group', { name: 'Management' });
    expect(within(management).getAllByRole('button')).toHaveLength(2);
    expect(within(management).getByRole('button', { name: 'Connections' })).toBeInTheDocument();
    expect(within(management).getByRole('button', { name: 'Data Views' })).toBeInTheDocument();
    expect(within(management).queryByRole('button', { name: 'Discover' })).not.toBeInTheDocument();
    fireEvent.click(within(management).getByRole('button', { name: 'Connections' }));
    expect(screen.getByRole('heading', { name: 'Connections', level: 1 })).toBeInTheDocument();
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    fireEvent.click(within(management).getByRole('button', { name: 'Data Views' }));
    expect(screen.getByRole('heading', { name: 'Data Views', level: 1 })).toBeInTheDocument();
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    await waitFor(() => expect(window.localStorage.getItem('northstar-nav-expanded')).toBe('true'));
  });
});