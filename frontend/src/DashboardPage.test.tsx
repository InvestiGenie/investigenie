import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import DashboardPage, { normalizeGrafanaDashboard, normalizeKibanaExport } from './DashboardPage';
import { api, type ConnectionSummary, type DataViewSummary, type KnowledgeEntry } from './api';
import { elasticsearchIntegration, connectionFixture } from './test-fixtures';

const connections: ConnectionSummary[] = [connectionFixture(elasticsearchIntegration, {
  id: 'logs-source', name: 'Logs source', connection_type: 'elasticsearch',
  url: 'http://localhost:9200', database: null, status: 'connected',
})];
const dataViews: DataViewSummary[] = [{
  id: 'application-logs', name: 'Application logs', connection_id: 'logs-source',
  index: 'logs', time_field: '@timestamp', revision: 'view-revision',
}];

describe('Dashboard imports', () => {
  it('normalizes Grafana panels and targets', () => {
    const normalized = normalizeGrafanaDashboard({
      title: 'Checkout operations',
      panels: [
        { id: 1, title: 'Error rate', type: 'timeseries', targets: [{ expr: 'status >= 500' }] },
        { id: 2, title: 'Top services', type: 'table', targets: [{ expr: 'service:*' }] },
      ],
    });
    expect(normalized.source).toBe('grafana');
    expect(normalized.title).toBe('Checkout operations');
    expect(normalized.panels).toMatchObject([
      { title: 'Error rate', type: 'timeseries', query: 'status >= 500' },
      { title: 'Top services', type: 'table', query: 'service:*' },
    ]);
  });

  it('normalizes Kibana dashboard and visualization saved objects', () => {
    const ndjson = [
      JSON.stringify({ type: 'dashboard', id: 'dash-1', attributes: { title: 'Service overview', panelsJSON: JSON.stringify([{ panelIndex: 'vis-1', title: 'Requests' }]) } }),
      JSON.stringify({ type: 'visualization', id: 'vis-1', attributes: { title: 'Requests by service', visState: JSON.stringify({ type: 'line', params: { query: 'service:checkout' } }) } }),
    ].join('\n');
    const normalized = normalizeKibanaExport(ndjson);
    expect(normalized.source).toBe('kibana');
    expect(normalized.title).toBe('Service overview');
    expect(normalized.panels).toMatchObject([{ id: 'vis-1', title: 'Requests by service', type: 'timeseries', query: 'service:checkout' }]);
  });
});

describe('DashboardPage', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('loads saved dashboards and saves an imported dashboard', async () => {
    const saved: KnowledgeEntry = {
      id: 'dashboard-1', revision: 'revision-1', title: 'Checkout', kind: 'dashboard', status: 'published',
      content: JSON.stringify({ title: 'Checkout', source: 'native', panels: [] }),
      steps: [], services: [], environments: [], connection_ids: [], data_view_ids: [], tags: ['dashboard'], source_url: null,
      created_at_ms: 1, updated_at_ms: 1,
    };
    vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([saved]);
    vi.spyOn(api, 'addKnowledge').mockResolvedValue(saved);

    render(<DashboardPage connections={connections} dataViews={dataViews} />);
    expect(await screen.findByText('Checkout')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'New dashboard' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Dashboard title' }), { target: { value: 'Imported operations' } });
    fireEvent.click(screen.getAllByRole('button', { name: 'Add panel' })[0]);
    fireEvent.change(screen.getByRole('combobox', { name: 'Panel Data View' }), { target: { value: 'application-logs' } });
    fireEvent.click(screen.getByRole('button', { name: 'Apply panel' }));
    expect(screen.getByRole('textbox', { name: 'Dashboard title' })).toHaveValue('Imported operations');
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(api.addKnowledge).toHaveBeenCalled());
  });

  it('previews a configured panel and applies it to the dashboard', async () => {
    vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([]);
    vi.spyOn(api, 'search').mockResolvedValue({ took: 1, timed_out: false, total: 42, total_relation: 'eq', hits: [], histogram: [] });
    render(<DashboardPage connections={connections} dataViews={dataViews} />);

    fireEvent.click(screen.getAllByRole('button', { name: 'Add panel' })[0]);
    fireEvent.change(screen.getByRole('textbox', { name: 'Panel title' }), { target: { value: 'Requests' } });
    fireEvent.click(screen.getByRole('button', { name: 'Stat' }));
    fireEvent.change(screen.getByRole('combobox', { name: 'Panel Data View' }), { target: { value: 'application-logs' } });
    fireEvent.change(screen.getByRole('textbox', { name: 'Panel query' }), { target: { value: 'status:500' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run query' }));
    expect(await screen.findByText('42')).toBeInTheDocument();
    expect(api.search).toHaveBeenCalledWith('logs-source', expect.objectContaining({ index: 'logs', query: 'status:500', size: 1, include_histogram: false, histogram_field: '@timestamp', time_range: expect.objectContaining({ start: expect.any(Number), end: expect.any(Number) }) }));
    fireEvent.click(screen.getByRole('button', { name: 'Apply panel' }));
    expect(screen.getByText('Requests')).toBeInTheDocument();
    expect(screen.getByText('42')).toBeInTheDocument();
  });

  it('discards panel changes when returning to the dashboard', async () => {
    vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([]);
    render(<DashboardPage connections={connections} dataViews={dataViews} />);
    fireEvent.click(screen.getAllByRole('button', { name: 'Add panel' })[0]);
    fireEvent.click(screen.getByRole('button', { name: 'Apply panel' }));
    fireEvent.click(screen.getByRole('button', { name: 'Edit Panel 1' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Panel title' }), { target: { value: 'Changed' } });
    fireEvent.click(screen.getByRole('button', { name: 'Back to dashboard' }));
    expect(screen.getByText('Panel 1')).toBeInTheDocument();
    expect(screen.queryByText('Changed')).not.toBeInTheDocument();
  });

  it('loads mapped saved panels and refreshes them on demand', async () => {
    const saved: KnowledgeEntry = {
      id: 'dashboard-1', revision: 'revision-1', title: 'Operations', kind: 'dashboard', status: 'published',
      content: JSON.stringify({ title: 'Operations', source: 'native', panels: [{ id: 'panel-1', title: 'Requests', type: 'stat', data_view_id: 'application-logs', query: '*' }] }),
      steps: [], services: [], environments: [], connection_ids: [], data_view_ids: [], tags: [], source_url: null,
      created_at_ms: 1, updated_at_ms: 1,
    };
    vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([saved]);
    vi.spyOn(api, 'search').mockResolvedValue({ took: 1, timed_out: false, total: 42, total_relation: 'eq', hits: [], histogram: [] });
    render(<DashboardPage connections={connections} dataViews={dataViews} />);

    fireEvent.click(await screen.findByRole('button', { name: 'Operations' }));
    expect(await screen.findByText('42')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Refresh dashboard' }));
    await waitFor(() => expect(api.search).toHaveBeenCalledTimes(2));
  });

  it('ignores a preview response after the query changes', async () => {
    vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([]);
    let resolveSearch: (result: Awaited<ReturnType<typeof api.search>>) => void = () => {};
    vi.spyOn(api, 'search').mockImplementation(() => new Promise((resolve) => { resolveSearch = resolve; }));
    render(<DashboardPage connections={connections} dataViews={dataViews} />);

    fireEvent.click(screen.getAllByRole('button', { name: 'Add panel' })[0]);
    fireEvent.click(screen.getByRole('button', { name: 'Stat' }));
    fireEvent.change(screen.getByRole('combobox', { name: 'Panel Data View' }), { target: { value: 'application-logs' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run query' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Panel query' }), { target: { value: 'status:500' } });
    await act(async () => resolveSearch({ took: 1, timed_out: false, total: 42, total_relation: 'eq', hits: [], histogram: [] }));
    expect(screen.queryByText('42')).not.toBeInTheDocument();
    expect(screen.getByText('No data loaded')).toBeInTheDocument();
  });

  it.each(['Time series', 'Stat', 'Table', 'Logs'])('sends the time field for %s panels', async (visualization) => {
    vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([]);
    vi.spyOn(api, 'search').mockResolvedValue({ took: 1, timed_out: false, total: 0, total_relation: 'eq', hits: [], histogram: [] });
    render(<DashboardPage connections={connections} dataViews={dataViews} />);

    fireEvent.click(screen.getAllByRole('button', { name: 'Add panel' })[0]);
    fireEvent.click(screen.getByRole('button', { name: visualization }));
    fireEvent.change(screen.getByRole('combobox', { name: 'Panel Data View' }), { target: { value: 'application-logs' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run query' }));
    await waitFor(() => expect(api.search).toHaveBeenCalledWith('logs-source', expect.objectContaining({ histogram_field: '@timestamp', include_histogram: visualization === 'Time series', time_range: expect.objectContaining({ start: expect.any(Number), end: expect.any(Number) }) })));
  });

  it('explains when a Data View has no time field instead of searching all time', () => {
    vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([]);
    const search = vi.spyOn(api, 'search');
    render(<DashboardPage connections={connections} dataViews={[{ ...dataViews[0], time_field: '' }]} />);

    fireEvent.click(screen.getAllByRole('button', { name: 'Add panel' })[0]);
    fireEvent.change(screen.getByRole('combobox', { name: 'Panel Data View' }), { target: { value: 'application-logs' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run query' }));
    expect(screen.getByRole('alert')).toHaveTextContent('The selected Data View needs a time field to search the last 15 minutes.');
    expect(search).not.toHaveBeenCalled();
  });
});
