import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import DataViewsPage from './DataViewsPage';
import type { ConnectionSummary } from './api';
import { connectionFixture, elasticsearchIntegration } from './test-fixtures';

const connections: ConnectionSummary[] = [connectionFixture(elasticsearchIntegration, {
  id: 'logs-source', name: 'Logs source', connection_type: 'elasticsearch',
  url: 'http://localhost:9200', database: null, status: 'connected',
})];

describe('DataViewsPage', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const body = url.endsWith('/indices')
        ? { indices: [{ name: 'logs', document_count: '10', health: 'green' }] }
        : url.endsWith('/fields')
          ? { fields: [{ name: 'timestamp', field_type: 'date' }, { name: 'event_time', field_type: 'date' }, { name: 'message', field_type: 'keyword' }] }
          : { status: 'configured' };
      return { ok: true, status: 200, json: async () => body } as Response;
    }));
  });

  async function configure() {
    fireEvent.click(screen.getByRole('button', { name: /Add data view/ }));
    fireEvent.change(screen.getByLabelText('Data view name'), { target: { value: 'Application logs' } });
    fireEvent.change(screen.getByLabelText('Connection'), { target: { value: 'logs-source' } });
    fireEvent.click(screen.getByRole('combobox', { name: 'Index / table' }));
    fireEvent.click(await screen.findByRole('option', { name: 'logs' }));
    await waitFor(() => expect(screen.getByLabelText('Time series time field')).toHaveValue('timestamp'));
    expect(screen.queryByRole('option', { name: /message/ })).not.toBeInTheDocument();
  }

  it.each([false, true])('retains the edit-opening revision across prop changes (conflict: %s)', async (conflict) => {
    const view = { id: 'application-logs', revision: 'original-revision', name: 'Application logs', connection_id: 'logs-source', index: 'logs', time_field: 'event_time' };
    const refresh = vi.fn().mockResolvedValue(undefined);
    const { rerender } = render(<DataViewsPage connections={connections} dataViews={[view]} onDataViewsChange={refresh} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Application logs' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save changes' })).toBeEnabled());
    fireEvent.change(screen.getByLabelText('Data view name'), { target: { value: 'Draft logs' } });
    rerender(<DataViewsPage connections={connections} dataViews={[{ ...view, revision: 'newer-revision', name: 'Changed elsewhere' }]} onDataViewsChange={refresh} />);
    expect(screen.getByLabelText('Data view name')).toHaveValue('Draft logs');
    expect(screen.queryByText(/held in API memory|API restarts/)).not.toBeInTheDocument();
    if (conflict) vi.mocked(fetch).mockResolvedValueOnce({
      ok: false, status: 409,
      json: async () => ({ error: { code: 'configuration_conflict', message: 'Data view changed elsewhere. Reload before saving.' } }),
    } as Response);
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    if (conflict) {
      expect(await screen.findByRole('alert')).toHaveTextContent('Data view changed elsewhere. Reload before saving.');
      expect(screen.getByLabelText('Data view name')).toHaveValue('Draft logs');
      expect(refresh).not.toHaveBeenCalled();
      expect(screen.queryByRole('status')).not.toBeInTheDocument();
    } else {
      expect(await screen.findByRole('status')).toHaveTextContent('Data view validated and updated.');
      expect(refresh).toHaveBeenCalledOnce();
    }
    const updates = vi.mocked(fetch).mock.calls.filter(([, init]) => init?.method === 'PUT');
    expect(updates).toHaveLength(1);
    expect(updates[0][1]).toMatchObject({ headers: { 'If-Match': '"original-revision"' } });
    expect(JSON.parse(String(updates[0][1]?.body))).toEqual({
      name: 'Draft logs', connection_id: 'logs-source', index: 'logs', time_field: 'event_time',
    });
    expect(vi.mocked(fetch)).toHaveBeenCalledTimes(3);
    if (conflict) {
      fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
      fireEvent.click(screen.getByRole('button', { name: 'Edit Changed elsewhere' }));
      await waitFor(() => expect(screen.getByRole('button', { name: 'Save changes' })).toBeEnabled());
      fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
      await screen.findByText('Data view validated and updated.');
      const latestUpdate = vi.mocked(fetch).mock.calls.filter(([, init]) => init?.method === 'PUT').at(-1);
      expect(latestUpdate?.[1]).toMatchObject({ headers: { 'If-Match': '"newer-revision"' } });
    }
  });

  it('shows delete conflicts without refreshing, removing the row, or retrying', async () => {
    const refresh = vi.fn();
    render(<DataViewsPage connections={connections} dataViews={[{
      id: 'application-logs', revision: 'row-revision', name: 'Application logs', connection_id: 'logs-source', index: 'logs', time_field: 'timestamp',
    }]} onDataViewsChange={refresh} />);
    vi.mocked(fetch).mockResolvedValueOnce({
      ok: false, status: 409,
      json: async () => ({ error: { code: 'configuration_conflict', message: 'Data view changed elsewhere. Reload before deleting.' } }),
    } as Response);
    fireEvent.click(screen.getByRole('button', { name: 'Remove Application logs' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Data view changed elsewhere. Reload before deleting.');
    expect(screen.getByRole('button', { name: 'Remove Application logs' })).toBeInTheDocument();
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
    expect(refresh).not.toHaveBeenCalled();
    expect(vi.mocked(fetch)).toHaveBeenCalledOnce();
    expect(vi.mocked(fetch)).toHaveBeenCalledWith(expect.stringContaining('/api/data-views/application-logs'), {
      method: 'DELETE', headers: { 'If-Match': '"row-revision"' },
    });
  });

  it('uses backend time-field metadata when present and falls back for older responses', async () => {
    const defaultFetch = vi.mocked(fetch).getMockImplementation()!;
    vi.mocked(fetch).mockImplementation(async (input, init) => String(input).endsWith('/fields') ? {
      ok: true, status: 200, json: async () => ({ fields: [
        { name: 'backend_time', field_type: 'custom_timestamp', is_time_field: true },
        { name: 'not_time', field_type: 'date', is_time_field: false },
        { name: 'legacy_time', field_type: 'Nullable(DateTime64(3))' },
      ] }),
    } as Response : defaultFetch(input, init));
    render(<DataViewsPage connections={connections} dataViews={[]} onDataViewsChange={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: /Add data view/ }));
    fireEvent.change(screen.getByLabelText('Connection'), { target: { value: 'logs-source' } });
    fireEvent.click(screen.getByRole('combobox', { name: 'Index / table' }));
    fireEvent.click(await screen.findByRole('option', { name: 'logs' }));
    expect(await screen.findByRole('option', { name: 'backend_time · custom_timestamp' })).toBeInTheDocument();
    expect(screen.getByLabelText('Time series time field')).toHaveValue('backend_time');
    expect(screen.getByRole('option', { name: 'legacy_time · Nullable(DateTime64(3))' })).toBeInTheDocument();
    expect(screen.queryByRole('option', { name: 'not_time · date' })).not.toBeInTheDocument();
  });

  it('searches large index lists, bounds rendered options, and selects with the keyboard', async () => {
    const defaultFetch = vi.mocked(fetch).getMockImplementation()!;
    const indices = Array.from({ length: 600 }, (_, index) => ({ name: `logs-app-${String(index).padStart(3, '0')}`, document_count: String(index * 1000), health: index % 2 ? 'yellow' : 'green' }));
    vi.mocked(fetch).mockImplementation(async (input, init) => String(input).endsWith('/indices')
      ? { ok: true, status: 200, json: async () => ({ indices: [{ name: 'metrics-system', document_count: '5', health: 'green' }, ...indices] }) } as Response
      : defaultFetch(input, init));
    render(<DataViewsPage connections={connections} dataViews={[]} onDataViewsChange={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: /Add data view/ }));
    const picker = screen.getByRole('combobox', { name: 'Index / table' });
    expect(picker).toBeDisabled();
    fireEvent.change(screen.getByLabelText('Connection'), { target: { value: 'logs-source' } });
    fireEvent.click(picker);
    await screen.findByRole('option', { name: 'logs-app-000' });
    expect(screen.queryByRole('option', { name: 'metrics-system' })).not.toBeInTheDocument();
    expect(within(screen.getByRole('listbox')).getAllByRole('option')).toHaveLength(200);
    expect(screen.getByText(/601 of 601 · showing first 200/)).toBeInTheDocument();
    fireEvent.change(picker, { target: { value: 'app 05' } });
    expect(screen.getByText('16 of 601')).toBeInTheDocument();
    fireEvent.change(picker, { target: { value: 'app-05' } });
    expect(within(screen.getByRole('listbox')).getAllByRole('option')).toHaveLength(10);
    expect(screen.getByText('10 of 601')).toBeInTheDocument();
    fireEvent.keyDown(picker, { key: 'ArrowDown' });
    expect(picker).toHaveAttribute('aria-activedescendant', screen.getByRole('option', { name: 'logs-app-051' }).id);
    fireEvent.keyDown(picker, { key: 'Enter' });
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    expect(picker).toHaveValue('logs-app-051');
    await waitFor(() => expect(screen.getByLabelText('Time series time field')).toHaveValue('timestamp'));
    fireEvent.click(picker);
    fireEvent.change(picker, { target: { value: 'no-such-index' } });
    expect(screen.getByText('No matches')).toBeInTheDocument();
    fireEvent.keyDown(picker, { key: 'Escape' });
    expect(picker).toHaveValue('logs-app-051');
  });

  it('allows a custom multi-index pattern and validates its fields before saving', async () => {
    render(<DataViewsPage connections={connections} dataViews={[]} onDataViewsChange={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: /Add data view/ }));
    fireEvent.change(screen.getByLabelText('Data view name'), { target: { value: 'Daily logs' } });
    fireEvent.change(screen.getByLabelText('Connection'), { target: { value: 'logs-source' } });
    const picker = screen.getByRole('combobox', { name: 'Index / table' });
    fireEvent.click(picker);
    fireEvent.change(picker, { target: { value: 'logs-*,metrics-*' } });
    fireEvent.click(await screen.findByRole('option', { name: 'Use index pattern or table: logs-*,metrics-*' }));
    await waitFor(() => expect(screen.getByLabelText('Time series time field')).toHaveValue('timestamp'));
    expect(screen.getByRole('button', { name: 'Create data view' })).toBeEnabled();
    fireEvent.click(screen.getByRole('button', { name: 'Create data view' }));
    expect(await screen.findByRole('status')).toHaveTextContent('Data view validated and created.');
    const createCall = vi.mocked(fetch).mock.calls.find(([, init]) => init?.method === 'POST');
    expect(JSON.parse(String(createCall?.[1]?.body))).toEqual({
      name: 'Daily logs', connection_id: 'logs-source', index: 'logs-*,metrics-*', time_field: 'timestamp',
    });
  });

  it('refreshes saved views only after successful server validation', async () => {
    const refresh = vi.fn().mockResolvedValue(undefined);
    render(<DataViewsPage connections={connections} dataViews={[]} onDataViewsChange={refresh} />);
    expect(screen.queryByLabelText('Data view name')).not.toBeInTheDocument();
    await configure();
    fireEvent.click(screen.getByRole('button', { name: 'Create data view' }));
    expect(await screen.findByRole('status')).toHaveTextContent('Data view validated and created.');
    expect(refresh).toHaveBeenCalledOnce();
    const createCall = vi.mocked(fetch).mock.calls.find(([, init]) => init?.method === 'POST');
    expect(JSON.parse(String(createCall?.[1]?.body))).toEqual({
      name: 'Application logs', connection_id: 'logs-source', index: 'logs', time_field: 'timestamp',
    });
  });

  it('does not refresh or announce success when validation fails', async () => {
    const refresh = vi.fn();
    render(<DataViewsPage connections={connections} dataViews={[]} onDataViewsChange={refresh} />);
    await configure();
    vi.mocked(fetch).mockResolvedValueOnce({
      ok: false, status: 400, json: async () => ({ error: { message: 'Invalid time field' } }),
    } as Response);
    fireEvent.click(screen.getByRole('button', { name: 'Create data view' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Invalid time field');
    expect(refresh).not.toHaveBeenCalled();
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });

  it('opens an edit form with the saved configuration and updates the same view', async () => {
    const refresh = vi.fn().mockResolvedValue(undefined);
    render(<DataViewsPage connections={connections} dataViews={[{
      id: 'application-logs', revision: 'view-revision', name: 'Application logs', connection_id: 'logs-source', index: 'logs', time_field: 'event_time',
    }]} onDataViewsChange={refresh} />);
    expect(document.querySelector('.data-view-row .source-mark img')).toBeInTheDocument();
    expect(document.querySelector('.data-view-row .source-mark')).toHaveAttribute('data-icon', 'elasticsearch');
    expect(screen.queryByLabelText('Data view name')).not.toBeInTheDocument();
    expect(vi.mocked(fetch)).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Edit Application logs' }));
    expect(screen.getByLabelText('Data view name')).toHaveValue('Application logs');
    expect(screen.getByLabelText('Connection')).toHaveValue('logs-source');
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save changes' })).toBeEnabled());
    expect(screen.getByLabelText('Index / table')).toHaveValue('logs');
    expect(screen.getByLabelText('Time series time field')).toHaveValue('event_time');
    fireEvent.change(screen.getByLabelText('Data view name'), { target: { value: 'Renamed logs' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(await screen.findByRole('status')).toHaveTextContent('Data view validated and updated.');
    const updateCall = vi.mocked(fetch).mock.calls.find(([, init]) => init?.method === 'PUT');
    expect(String(updateCall?.[0])).toContain('/api/data-views/application-logs');
    expect(updateCall?.[1]).toMatchObject({ headers: { 'If-Match': '"view-revision"' } });
    expect(JSON.parse(String(updateCall?.[1]?.body))).toEqual({
      name: 'Renamed logs', connection_id: 'logs-source', index: 'logs', time_field: 'event_time',
    });
    expect(refresh).toHaveBeenCalledOnce();
    expect(screen.queryByLabelText('Data view name')).not.toBeInTheDocument();
  });

  it('keeps rejected edits in the form without updating the list or announcing success', async () => {
    const refresh = vi.fn();
    render(<DataViewsPage connections={connections} dataViews={[{
      id: 'application-logs', revision: 'view-revision', name: 'Application logs', connection_id: 'logs-source', index: 'logs', time_field: 'timestamp',
    }]} onDataViewsChange={refresh} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Application logs' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save changes' })).toBeEnabled());
    vi.mocked(fetch).mockResolvedValueOnce({ ok: false, status: 400, json: async () => ({ error: { message: 'Invalid time field' } }) } as Response);
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Invalid time field');
    expect(refresh).not.toHaveBeenCalled();
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.getByRole('button', { name: 'Edit Application logs' })).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('removes a saved view and refreshes the list', async () => {
    const refresh = vi.fn().mockResolvedValue(undefined);
    render(<DataViewsPage connections={connections} dataViews={[{
      id: 'application-logs', revision: 'view-revision', name: 'Application logs', connection_id: 'logs-source', index: 'logs', time_field: 'timestamp',
    }]} onDataViewsChange={refresh} />);
    fireEvent.click(screen.getByRole('button', { name: 'Remove Application logs' }));
    expect(await screen.findByRole('status')).toHaveTextContent('Application logs removed.');
    expect(refresh).toHaveBeenCalledOnce();
    expect(vi.mocked(fetch)).toHaveBeenCalledWith(expect.stringContaining('/api/data-views/application-logs'), { method: 'DELETE', headers: { 'If-Match': '"view-revision"' } });
  });
});
