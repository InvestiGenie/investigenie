import { fireEvent, render, screen, waitFor } from '@testing-library/react';
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
    fireEvent.click(screen.getByRole('button', { name: 'Add panel' }));
    await waitFor(() => expect(screen.getByRole('textbox', { name: 'Dashboard title' })).toHaveValue('Imported operations'));
    fireEvent.change(screen.getByRole('combobox', { name: /Panel Panel 1 Data View/ }), { target: { value: 'application-logs' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(api.addKnowledge).toHaveBeenCalled());
  });
});
