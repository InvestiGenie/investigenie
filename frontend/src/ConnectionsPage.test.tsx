import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import ConnectionsPage from './ConnectionsPage';
import { api } from './api';
import { clickhouseIntegration, connectionFixture, elasticsearchIntegration, thirdIntegration } from './test-fixtures';

function jsonResponse(body: unknown): Response {
  return { ok: true, status: 200, json: async () => body } as Response;
}

describe('ConnectionsPage', () => {
  beforeEach(() => {
    vi.spyOn(api, 'connectionStatus').mockResolvedValue('connected');
    vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith('/api/integrations')) {
        return Promise.resolve(jsonResponse({ integrations: [
          elasticsearchIntegration, clickhouseIntegration,
        ] }));
      }
      return Promise.resolve(jsonResponse({ connection: 'warehouse', status: 'connected' }));
    }));
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  it('tracks independent datasource loss and recovery without refreshing saved connections', async () => {
    vi.useFakeTimers();
    let sourceChecks = 0;
    const status = vi.mocked(api.connectionStatus).mockImplementation(async (id) => {
      if (id === 'source' && ++sourceChecks === 2) return 'not_connected';
      return 'connected';
    });
    const refresh = vi.fn();
    const { unmount } = render(<ConnectionsPage connections={[
      connectionFixture(elasticsearchIntegration),
      connectionFixture(clickhouseIntegration, { id: 'warehouse', name: 'Warehouse' }),
    ]} onConnectionsChange={refresh} />);
    expect(screen.getAllByText('Checking')).toHaveLength(2);
    await act(async () => {});
    expect(screen.getAllByText('Connected')).toHaveLength(2);
    await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
    expect(screen.getByText('Not connected')).toHaveClass('connection-health-offline');
    expect(screen.getAllByText('Connected')).toHaveLength(1);
    expect(screen.getByText('Source')).toBeInTheDocument();
    expect(screen.getByText('Warehouse')).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
    expect(screen.getAllByText('Connected')).toHaveLength(2);
    expect(refresh).not.toHaveBeenCalled();
    expect(status).toHaveBeenCalledTimes(6);
    unmount();
    await vi.advanceTimersByTimeAsync(10000);
    expect(status).toHaveBeenCalledTimes(6);
  });

  it('bounds stalled datasource probes at seven seconds and polls five seconds after completion', async () => {
    vi.useFakeTimers();
    vi.mocked(api.connectionStatus).mockImplementationOnce((_, signal) => new Promise((_, reject) => {
      signal?.addEventListener('abort', () => reject(signal.reason), { once: true });
    }));
    const { unmount } = render(<ConnectionsPage connections={[connectionFixture(elasticsearchIntegration)]} onConnectionsChange={vi.fn()} />);
    await act(async () => { await vi.advanceTimersByTimeAsync(6999); });
    expect(screen.getByText('Checking')).toBeInTheDocument();
    expect(api.connectionStatus).toHaveBeenCalledOnce();
    await act(async () => { await vi.advanceTimersByTimeAsync(1); });
    expect(screen.getByText('Not connected')).toBeInTheDocument();
    await act(async () => { await vi.advanceTimersByTimeAsync(4999); });
    expect(api.connectionStatus).toHaveBeenCalledOnce();
    await act(async () => { await vi.advanceTimersByTimeAsync(1); });
    expect(screen.getByText('Connected')).toBeInTheDocument();
    expect(api.connectionStatus).toHaveBeenCalledTimes(2);
    unmount();
  });

  it('aborts a pending probe on unmount without scheduling another probe', async () => {
    vi.useFakeTimers();
    vi.mocked(api.connectionStatus).mockImplementationOnce((_, signal) => new Promise((_, reject) => {
      signal?.addEventListener('abort', () => reject(signal.reason), { once: true });
    }));
    const { unmount } = render(<ConnectionsPage connections={[connectionFixture(elasticsearchIntegration)]} onConnectionsChange={vi.fn()} />);
    const signal = vi.mocked(api.connectionStatus).mock.calls[0][1];
    expect(signal?.aborted).toBe(false);
    unmount();
    await act(async () => { await vi.advanceTimersByTimeAsync(10000); });
    expect(signal?.aborted).toBe(true);
    expect(api.connectionStatus).toHaveBeenCalledOnce();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('marks every row offline on API failure without deleting saved records', async () => {
    vi.mocked(api.connectionStatus).mockRejectedValue(new Error('Request failed (503)'));
    const refresh = vi.fn();
    render(<ConnectionsPage connections={[
      connectionFixture(elasticsearchIntegration),
      connectionFixture(clickhouseIntegration, { id: 'warehouse', name: 'Warehouse' }),
    ]} onConnectionsChange={refresh} />);
    await waitFor(() => expect(screen.getAllByText('Not connected')).toHaveLength(2));
    expect(screen.getAllByTitle('Datasource connection status')).toHaveLength(2);
    expect(screen.getByText('Source')).toBeInTheDocument();
    expect(screen.getByText('Warehouse')).toBeInTheDocument();
    expect(refresh).not.toHaveBeenCalled();
  });

  it('retains the previous status while polling and ignores late timed-out responses', async () => {
    vi.useFakeTimers();
    let finish!: (status: 'connected') => void;
    vi.mocked(api.connectionStatus)
      .mockResolvedValueOnce('connected')
      .mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const { unmount } = render(<ConnectionsPage connections={[connectionFixture(elasticsearchIntegration)]} onConnectionsChange={vi.fn()} />);
    await act(async () => {});
    await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
    expect(screen.getByText('Connected')).toBeInTheDocument();
    expect(screen.queryByText('Checking')).not.toBeInTheDocument();
    await act(async () => { await vi.advanceTimersByTimeAsync(7000); });
    expect(screen.getByText('Not connected')).toBeInTheDocument();
    await act(async () => { finish('connected'); });
    expect(screen.getByText('Not connected')).toBeInTheDocument();
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each(['revision', 'id'] as const)('rechecks changed %s and cancels stale pending results', async (field) => {
    vi.useFakeTimers();
    let finish!: (status: 'connected') => void;
    vi.mocked(api.connectionStatus).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }))
      .mockResolvedValueOnce('not_connected');
    const connection = connectionFixture(thirdIntegration);
    const refresh = vi.fn();
    const { rerender, unmount } = render(<ConnectionsPage connections={[connection]} onConnectionsChange={refresh} />);
    const oldSignal = vi.mocked(api.connectionStatus).mock.calls[0][1];
    rerender(<ConnectionsPage connections={[{ ...connection, [field]: 'changed' }]} onConnectionsChange={refresh} />);
    expect(oldSignal?.aborted).toBe(true);
    await act(async () => {});
    expect(api.connectionStatus).toHaveBeenCalledTimes(2);
    expect(vi.mocked(api.connectionStatus).mock.calls[1][0]).toBe(field === 'id' ? 'changed' : connection.id);
    expect(screen.getByText('Not connected')).toBeInTheDocument();
    await act(async () => { finish('connected'); });
    expect(screen.getByText('Not connected')).toBeInTheDocument();
    unmount();
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(12000);
    expect(api.connectionStatus).toHaveBeenCalledTimes(2);
  });

  it.each([false, true])('retains the edit-opening revision across prop changes (conflict: %s)', async (conflict) => {
    const connection = connectionFixture(thirdIntegration, {
      id: 'archive', revision: 'original-revision', name: 'History', url: 'https://archive.example.com',
      configuration: { url: 'https://archive.example.com', namespace: 'audit', region: 'east', replicated: false },
    });
    const refresh = vi.fn().mockResolvedValue(undefined);
    const { rerender } = render(<ConnectionsPage connections={[connection]} onConnectionsChange={refresh} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit History' }));
    fireEvent.change(screen.getByLabelText('Namespace'), { target: { value: 'draft' } });
    rerender(<ConnectionsPage connections={[{ ...connection, revision: 'newer-revision', name: 'Changed elsewhere' }]} onConnectionsChange={refresh} />);
    expect(screen.getByLabelText('Connection name')).toHaveValue('History');
    expect(screen.getByLabelText('Access token')).toBeDisabled();
    expect(screen.queryByLabelText('Tenant')).not.toBeInTheDocument();
    expect(screen.getByText('Credentials remain on the server.')).toBeInTheDocument();
    expect(screen.queryByText(/held in API memory|API restarts/)).not.toBeInTheDocument();
    if (conflict) vi.mocked(fetch).mockResolvedValueOnce({
      ok: false, status: 409,
      json: async () => ({ error: { code: 'configuration_conflict', message: 'Connection changed elsewhere. Reload before saving.' } }),
    } as Response);
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    if (conflict) {
      expect(await screen.findByRole('alert')).toHaveTextContent('Connection changed elsewhere. Reload before saving.');
      expect(screen.getByLabelText('Namespace')).toHaveValue('draft');
      expect(refresh).not.toHaveBeenCalled();
      expect(screen.queryByRole('status')).not.toBeInTheDocument();
    } else {
      expect(await screen.findByRole('status')).toHaveTextContent('Connection updated and tested.');
      expect(refresh).toHaveBeenCalledOnce();
    }
    expect(vi.mocked(fetch)).toHaveBeenCalledOnce();
    expect(vi.mocked(fetch).mock.calls[0][1]).toMatchObject({ method: 'PUT', headers: { 'If-Match': '"original-revision"' } });
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).toEqual({
      name: 'History', url: 'https://archive.example.com', namespace: 'draft', region: 'east', replicated: false,
    });
    if (conflict) {
      fireEvent.click(screen.getByRole('button', { name: 'Back' }));
      fireEvent.click(screen.getByRole('button', { name: 'Edit Changed elsewhere' }));
      fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
      await screen.findByText('Connection updated and tested.');
      expect(vi.mocked(fetch).mock.calls[1][1]).toMatchObject({ headers: { 'If-Match': '"newer-revision"' } });
    }
  });

  it.each([false, true])('deletes with the row revision without retrying conflicts (conflict: %s)', async (conflict) => {
    const connection = connectionFixture(clickhouseIntegration, { name: 'Warehouse', revision: 'row-revision' });
    const refresh = vi.fn().mockResolvedValue(undefined);
    render(<ConnectionsPage connections={[connection]} onConnectionsChange={refresh} />);
    if (conflict) vi.mocked(fetch).mockResolvedValueOnce({
      ok: false, status: 409,
      json: async () => ({ error: { code: 'configuration_conflict', message: 'Connection changed elsewhere. Reload before deleting.' } }),
    } as Response);
    fireEvent.click(screen.getByRole('button', { name: 'Remove Warehouse' }));
    if (conflict) {
      expect(await screen.findByRole('alert')).toHaveTextContent('Connection changed elsewhere. Reload before deleting.');
      expect(refresh).not.toHaveBeenCalled();
      expect(screen.getByRole('button', { name: 'Remove Warehouse' })).toBeInTheDocument();
      expect(screen.queryByRole('status')).not.toBeInTheDocument();
    } else {
      expect(await screen.findByRole('status')).toHaveTextContent('Warehouse removed.');
      expect(refresh).toHaveBeenCalledOnce();
    }
    expect(vi.mocked(fetch)).toHaveBeenCalledOnce();
    expect(vi.mocked(fetch)).toHaveBeenCalledWith(expect.stringContaining('/api/connections/source'), {
      method: 'DELETE', headers: { 'If-Match': '"row-revision"' },
    });
  });

  it('distinguishes catalog failure from an empty catalog and recovers on retry', async () => {
    vi.mocked(fetch).mockRejectedValueOnce(new TypeError('Failed to fetch'));
    render(<ConnectionsPage connections={[]} onConnectionsChange={vi.fn()} />);
    expect(vi.mocked(fetch)).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    expect(screen.getByRole('status')).toHaveTextContent('Loading data sources...');
    expect(await screen.findByRole('alert')).toHaveTextContent('Cannot reach the InvestiGenie API');
    expect(screen.queryByText('No integrations are registered in the backend.')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Retry data sources' }));
    expect(await screen.findByRole('button', { name: /ClickHouse/ })).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(vi.mocked(fetch)).toHaveBeenCalledTimes(2);
  });

  it('shows an empty catalog message only after a successful empty response', async () => {
    vi.mocked(fetch).mockResolvedValueOnce(jsonResponse({ integrations: [] }));
    render(<ConnectionsPage connections={[]} onConnectionsChange={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    expect(await screen.findByText('No integrations are registered in the backend.')).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('shows saved connections until Add connection is pressed, then returns on cancel', () => {
    render(<ConnectionsPage
      connections={[connectionFixture(clickhouseIntegration, {
        id: 'warehouse',
        name: 'Warehouse',
        connection_type: 'clickhouse',
        url: 'http://localhost:8123',
        database: 'analytics',
        status: 'connected',
      })]}
      onConnectionsChange={vi.fn().mockResolvedValue(undefined)}
    />);

    expect(screen.getByText('Warehouse')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Use Warehouse in Discover' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    expect(screen.queryByText('Warehouse')).not.toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Choose a data source' })).toBeInTheDocument();
    expect(screen.queryByLabelText('Connection name')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.getByText('Warehouse')).toBeInTheDocument();
  });

  it('tests and adds a named ClickHouse connection with its database', async () => {
    const onConnectionsChange = vi.fn().mockResolvedValue(undefined);
    render(<ConnectionsPage connections={[]} onConnectionsChange={onConnectionsChange} />);

    expect(screen.getByText('No connections available')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    expect(screen.queryByText('No connections available')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('Connection name')).not.toBeInTheDocument();
    fireEvent.click(await screen.findByRole('button', { name: /ClickHouse/ }));
    expect(screen.getByRole('heading', { name: 'Configure ClickHouse' })).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Connection name'), { target: { value: 'Warehouse' } });
    fireEvent.change(screen.getByLabelText('HTTP endpoint'), { target: { value: 'http://localhost:8123' } });
    fireEvent.change(screen.getByLabelText('Database'), { target: { value: 'analytics' } });
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: 'reader' } });
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: 'secret' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test and add' }));

    await waitFor(() => expect(vi.mocked(fetch).mock.calls.some(([url, init]) => String(url).endsWith('/api/connections') && init?.method === 'POST')).toBe(true));
    const addCall = vi.mocked(fetch).mock.calls.find(([url, init]) => String(url).endsWith('/api/connections') && init?.method === 'POST');
    const request = JSON.parse(String(addCall?.[1]?.body));
    expect(request).toMatchObject({
      name: 'Warehouse',
      connection_type: 'clickhouse',
      url: 'http://localhost:8123',
      database: 'analytics',
      username: 'reader',
      password: 'secret',
    });
    expect(await screen.findByText('Connection tested and added.')).toBeInTheDocument();
    expect(onConnectionsChange).toHaveBeenCalledOnce();
  });

  it('submits a ClickHouse username with a genuinely blank password', async () => {
    render(<ConnectionsPage connections={[]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    fireEvent.click(await screen.findByRole('button', { name: /ClickHouse/ }));
    fireEvent.change(screen.getByLabelText('Connection name'), { target: { value: 'Playground' } });
    fireEvent.change(screen.getByLabelText('HTTP endpoint'), { target: { value: 'https://play.clickhouse.com:443/' } });
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: 'explorer' } });
    expect(screen.getByLabelText('Password')).toHaveValue('');
    fireEvent.click(screen.getByRole('button', { name: 'Test and add' }));
    await screen.findByText('Connection tested and added.');
    const addCall = vi.mocked(fetch).mock.calls.find(([, init]) => init?.method === 'POST');
    expect(JSON.parse(String(addCall?.[1]?.body))).toMatchObject({ username: 'explorer', password: '' });
  });

  it.each(['none', 'basic', 'api_key'])('retains the Elasticsearch %s authentication workflow through metadata', async (authType) => {
    render(<ConnectionsPage connections={[]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    fireEvent.click(await screen.findByRole('button', { name: /Elasticsearch/ }));
    expect(screen.getByLabelText('Authentication')).toHaveValue('none');
    expect(screen.queryByLabelText('Password')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('API key')).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Connection name'), { target: { value: 'Logs' } });
    fireEvent.change(screen.getByLabelText('HTTP endpoint'), { target: { value: 'https://logs.example.com' } });
    fireEvent.change(screen.getByLabelText('Authentication'), { target: { value: 'basic' } });
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: ' reader ' } });
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: ' secret ' } });
    fireEvent.change(screen.getByLabelText('Authentication'), { target: { value: authType } });
    if (authType === 'api_key') {
      expect(screen.queryByLabelText('Username')).not.toBeInTheDocument();
      fireEvent.change(screen.getByLabelText('API key'), { target: { value: ' key ' } });
    }
    const options = screen.getByRole('group', { name: 'Authentication options' });
    const authentication = within(options).getByLabelText('Authentication');
    const tls = within(options).getByLabelText('Skip TLS verification');
    expect(authentication.closest('.form-grid')).toBe(tls.closest('.form-grid'));
    expect(authentication.closest('.connection-field')).not.toHaveClass('form-field-full');
    expect(tls.closest('label')).not.toHaveClass('form-field-full');
    expect(tls).toBeEnabled();
    fireEvent.click(tls);
    fireEvent.click(screen.getByRole('button', { name: 'Test and add' }));
    await screen.findByText('Connection tested and added.');
    const addCall = vi.mocked(fetch).mock.calls.find(([, init]) => init?.method === 'POST');
    expect(JSON.parse(String(addCall?.[1]?.body))).toEqual({
      name: 'Logs', connection_type: 'elasticsearch', url: 'https://logs.example.com', tls_insecure: true,
      ...(authType === 'basic' ? { username: ' reader ', password: ' secret ' } : {}),
      ...(authType === 'api_key' ? { api_key: ' key ' } : {}),
    });
  });

  it('configures an unknown source using its custom controls, visibility, and flattened settings', async () => {
    vi.mocked(fetch).mockResolvedValueOnce(jsonResponse({ integrations: [thirdIntegration] }));
    render(<ConnectionsPage connections={[]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    expect(vi.mocked(fetch)).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    fireEvent.click(await screen.findByRole('button', { name: /Archive/ }));
    expect(screen.getByRole('heading', { name: 'Configure Archive' })).toBeInTheDocument();
    expect(screen.getByLabelText('Namespace')).toBeRequired();
    expect(screen.getByLabelText('Namespace')).toHaveAttribute('placeholder', 'events');
    expect(screen.getByLabelText('Region')).toHaveValue('west');
    expect(screen.getByLabelText('Tenant')).toHaveAttribute('type', 'text');
    expect(screen.getByLabelText('Access token')).toHaveAttribute('type', 'password');
    expect(screen.getByLabelText('Replicated')).not.toBeChecked();
    expect(screen.queryByLabelText('Replica collection')).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Connection name'), { target: { value: 'History' } });
    fireEvent.change(screen.getByLabelText('Archive endpoint'), { target: { value: 'https://archive.example.com' } });
    fireEvent.change(screen.getByLabelText('Namespace'), { target: { value: ' audit ' } });
    fireEvent.change(screen.getByLabelText('Region'), { target: { value: 'east' } });
    fireEvent.click(screen.getByLabelText('Replicated'));
    expect(screen.getByLabelText('Replica collection')).toBeRequired();
    fireEvent.change(screen.getByLabelText('Replica collection'), { target: { value: 'backup' } });
    fireEvent.click(screen.getByLabelText('Replicated'));
    expect(screen.queryByLabelText('Replica collection')).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Access token'), { target: { value: ' token ' } });
    fireEvent.change(screen.getByLabelText('Tenant'), { target: { value: 'customer' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test and add' }));
    await screen.findByText('Connection tested and added.');
    const addCall = vi.mocked(fetch).mock.calls.find(([, init]) => init?.method === 'POST');
    expect(JSON.parse(String(addCall?.[1]?.body))).toEqual({
      name: 'History', connection_type: 'archive', url: 'https://archive.example.com',
      namespace: 'audit', region: 'east', replicated: false, token: ' token ', tenant: 'customer',
    });
  });

  it('edits unknown-source public settings without catalog requests or resending secrets', async () => {
    render(<ConnectionsPage connections={[connectionFixture(thirdIntegration, {
      id: 'archive', name: 'History', url: 'https://archive.example.com',
      configuration: { url: 'https://archive.example.com', namespace: 'audit', region: 'east', replicated: true, replica: 'backup' },
    })]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    expect(screen.getByText('Archive · ArchiveQL')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Edit History' }));
    expect(vi.mocked(fetch)).not.toHaveBeenCalled();
    expect(screen.getByLabelText('Namespace')).toHaveValue('audit');
    expect(screen.getByLabelText('Region')).toHaveValue('east');
    expect(screen.getByLabelText('Replicated')).toBeChecked();
    expect(screen.getByLabelText('Replica collection')).toHaveValue('backup');
    expect(screen.getByLabelText('Access token')).toBeDisabled();
    expect(screen.queryByLabelText('Tenant')).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Namespace'), { target: { value: 'new-audit' } });
    fireEvent.change(screen.getByLabelText('Region'), { target: { value: 'west' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(vi.mocked(fetch)).toHaveBeenCalledOnce();
    const updateCall = vi.mocked(fetch).mock.calls[0];
    expect(String(updateCall[0])).toContain('/api/connections/archive');
    expect(updateCall[1]).toMatchObject({ headers: { 'If-Match': '"source-revision"' } });
    expect(JSON.parse(String(updateCall[1]?.body))).toEqual({
      name: 'History', url: 'https://archive.example.com', namespace: 'new-audit', region: 'west', replicated: true, replica: 'backup',
    });
  });

  it('resets metadata defaults and secrets when switching sources', async () => {
    vi.mocked(fetch).mockResolvedValueOnce(jsonResponse({ integrations: [thirdIntegration, clickhouseIntegration] }));
    render(<ConnectionsPage connections={[]} onConnectionsChange={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: /Add connection/ }));
    fireEvent.click(await screen.findByRole('button', { name: /Archive/ }));
    fireEvent.change(screen.getByLabelText('Connection name'), { target: { value: 'Draft' } });
    fireEvent.change(screen.getByLabelText('Access token'), { target: { value: 'secret' } });
    fireEvent.change(screen.getByLabelText('Region'), { target: { value: 'east' } });
    fireEvent.click(screen.getByRole('button', { name: 'Back' }));
    fireEvent.click(await screen.findByRole('button', { name: /ClickHouse/ }));
    expect(screen.getByLabelText('Connection name')).toHaveValue('');
    expect(screen.getByLabelText('Password')).toHaveValue('');
    expect(screen.queryByLabelText('Access token')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('Namespace')).not.toBeInTheDocument();
  });

  it.each([clickhouseIntegration, elasticsearchIntegration])('pairs credentials in a separate grid for $display_name', async (integration) => {
    render(<ConnectionsPage connections={[connectionFixture(integration, {
      configuration: { url: 'https://source.example.com', database: 'analytics', auth_type: 'basic' },
    })]} onConnectionsChange={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    const credentials = screen.getByRole('group', { name: 'Credentials' });
    const username = within(credentials).getByLabelText('Username');
    const password = within(credentials).getByLabelText('Password');
    const grid = username.closest('.form-grid');
    expect(grid).toBe(password.closest('.form-grid'));
    expect(grid).not.toBe(screen.getByLabelText('HTTP endpoint').closest('.form-grid'));
    expect(grid).not.toBe(screen.getByLabelText('Connection name').closest('.form-grid'));
    expect(username.closest('.connection-field')?.nextElementSibling).toBe(password.closest('.connection-field'));
    if (integration === elasticsearchIntegration) {
      const options = screen.getByRole('group', { name: 'Authentication options' });
      const authentication = within(options).getByLabelText('Authentication');
      const tls = within(options).getByLabelText('Skip TLS verification');
      expect(authentication.closest('.form-grid')).toBe(tls.closest('.form-grid'));
      expect(authentication.closest('.connection-field')).not.toHaveClass('form-field-full');
      expect(tls.closest('label')).not.toHaveClass('form-field-full');
      expect(tls).toBeDisabled();
      expect(options.nextElementSibling).toBe(credentials);
    } else {
      expect(grid).not.toBe(screen.getByLabelText('Database').closest('.form-grid'));
    }
  });

  it.each(['  reader  ', ''])('replaces a ClickHouse username exactly (%j)', async (username) => {
    render(<ConnectionsPage connections={[connectionFixture(clickhouseIntegration)]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    expect(screen.getByLabelText('Username')).toHaveValue('');
    expect(screen.getByLabelText('Username')).toBeDisabled();
    fireEvent.click(screen.getByRole('checkbox', { name: 'Replace Username' }));
    expect(screen.getByLabelText('Username')).toBeEnabled();
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: username } });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).toEqual({
      name: 'Source', url: 'http://localhost:9200', database: null, username,
    });
  });

  it.each(['  secret\t ', ''])('replaces a ClickHouse password exactly (%j)', async (password) => {
    render(<ConnectionsPage connections={[connectionFixture(clickhouseIntegration)]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    fireEvent.click(screen.getByRole('checkbox', { name: 'Replace Password' }));
    expect(screen.getByLabelText('Password')).not.toBeRequired();
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: password } });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).toEqual({
      name: 'Source', url: 'http://localhost:9200', database: null, password,
    });
  });

  it('omits a replacement after it is unchecked, retaining the local draft', async () => {
    render(<ConnectionsPage connections={[connectionFixture(clickhouseIntegration)]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    fireEvent.click(screen.getByRole('checkbox', { name: 'Replace Password' }));
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: 'draft' } });
    fireEvent.click(screen.getByRole('checkbox', { name: 'Replace Password' }));
    expect(screen.getByLabelText('Password')).toBeDisabled();
    expect(screen.getByLabelText('Password')).toHaveValue('draft');
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).not.toHaveProperty('password');
  });

  it('replaces a third-source secret through fallback grouping and retains it after a conflict', async () => {
    const connection = connectionFixture(thirdIntegration, {
      configuration: { url: 'https://archive.example.com', namespace: 'audit', token: 'must not prepopulate', tenant: 'must not expose' },
      revision: 'captured-revision',
    });
    const refresh = vi.fn();
    const { rerender } = render(<ConnectionsPage connections={[connection]} onConnectionsChange={refresh} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    expect(within(screen.getByRole('group', { name: 'Credentials' })).getByLabelText('Access token')).toHaveValue('');
    expect(screen.queryByLabelText('Tenant')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('checkbox', { name: 'Replace Access token' }));
    expect(screen.getByLabelText('Access token')).toBeRequired();
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(fetch).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('Access token'), { target: { value: '  replacement\t ' } });
    rerender(<ConnectionsPage connections={[{ ...connection, revision: 'new-revision' }]} onConnectionsChange={refresh} />);
    vi.mocked(fetch).mockResolvedValueOnce({
      ok: false, status: 409,
      json: async () => ({ error: { code: 'configuration_conflict', message: 'Changed elsewhere' } }),
    } as Response);
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Changed elsewhere');
    expect(screen.getByRole('checkbox', { name: 'Replace Access token' })).toBeChecked();
    expect(screen.getByLabelText('Access token')).toHaveValue('  replacement\t ');
    expect(refresh).not.toHaveBeenCalled();
    expect(vi.mocked(fetch).mock.calls[0][1]).toMatchObject({ headers: { 'If-Match': '"captured-revision"' } });
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).toMatchObject({ token: '  replacement\t ' });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(vi.mocked(fetch).mock.calls[1][1]).toMatchObject({ headers: { 'If-Match': '"captured-revision"' } });
  });

  it.each(['none', 'basic', 'api_key'])('loads saved Elasticsearch %s auth without prepopulating or sending secrets', async (authType) => {
    render(<ConnectionsPage connections={[connectionFixture(elasticsearchIntegration, {
      configuration: { url: 'https://logs.example.com', auth_type: authType, username: 'private', password: 'private', api_key: 'private' },
    })]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    expect(screen.getByLabelText('Authentication')).toHaveValue(authType);
    for (const label of authType === 'basic' ? ['Username', 'Password'] : authType === 'api_key' ? ['API key'] : []) {
      expect(screen.getByLabelText(label)).toHaveValue('');
      expect(screen.getByLabelText(label)).toBeDisabled();
      expect(screen.getByLabelText(label)).not.toBeRequired();
    }
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).toEqual({
      name: 'Source', url: 'https://logs.example.com', auth_type: authType,
    });
  });

  it.each([
    ['none', 'basic'], ['api_key', 'basic'], ['basic', 'api_key'], ['none', 'api_key'],
  ])('requires new visible credentials when switching Elasticsearch %s to %s', async (initialAuth, nextAuth) => {
    render(<ConnectionsPage connections={[connectionFixture(elasticsearchIntegration, {
      configuration: { url: 'https://logs.example.com', auth_type: initialAuth },
    })]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    fireEvent.change(screen.getByLabelText('Authentication'), { target: { value: nextAuth } });
    const labels = nextAuth === 'basic' ? ['Username', 'Password'] : ['API key'];
    for (const label of labels) {
      expect(screen.getByRole('checkbox', { name: `Replace ${label}` })).toBeChecked();
      expect(screen.getByRole('checkbox', { name: `Replace ${label}` })).toBeDisabled();
      expect(screen.getByLabelText(label)).toBeEnabled();
      expect(screen.getByLabelText(label)).toBeRequired();
    }
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(fetch).not.toHaveBeenCalled();
    for (const label of labels) fireEvent.change(screen.getByLabelText(label), { target: { value: ` ${label} replacement ` } });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).toEqual({
      name: 'Source', url: 'https://logs.example.com', auth_type: nextAuth,
      ...(nextAuth === 'basic' ? { username: ' Username replacement ', password: ' Password replacement ' } : { api_key: ' API key replacement ' }),
    });
  });

  it.each(['none', 'api_key'])('omits selected hidden secrets when switching basic to %s', async (nextAuth) => {
    render(<ConnectionsPage connections={[connectionFixture(elasticsearchIntegration, {
      configuration: { url: 'https://logs.example.com', auth_type: 'basic' },
    })]} onConnectionsChange={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit Source' }));
    fireEvent.click(screen.getByRole('checkbox', { name: 'Replace Password' }));
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: 'hidden draft' } });
    fireEvent.change(screen.getByLabelText('Authentication'), { target: { value: nextAuth } });
    expect(screen.queryByLabelText('Password')).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Authentication'), { target: { value: 'basic' } });
    expect(screen.getByRole('checkbox', { name: 'Replace Password' })).toBeChecked();
    expect(screen.getByLabelText('Password')).toHaveValue('hidden draft');
    fireEvent.change(screen.getByLabelText('Authentication'), { target: { value: nextAuth } });
    if (nextAuth === 'api_key') fireEvent.change(screen.getByLabelText('API key'), { target: { value: ' new key ' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Connection updated and tested.');
    expect(JSON.parse(String(vi.mocked(fetch).mock.calls[0][1]?.body))).toEqual({
      name: 'Source', url: 'https://logs.example.com', auth_type: nextAuth,
      ...(nextAuth === 'api_key' ? { api_key: ' new key ' } : {}),
    });
  });

  it('edits a saved connection without exposing or resending credentials', async () => {
    const onConnectionsChange = vi.fn().mockResolvedValue(undefined);
    render(<ConnectionsPage
      connections={[connectionFixture(clickhouseIntegration, {
        id: 'warehouse',
        name: 'Warehouse',
        connection_type: 'clickhouse',
        url: 'http://localhost:8123',
        database: 'analytics',
        status: 'connected',
      })]}
      onConnectionsChange={onConnectionsChange}
    />);

    fireEvent.click(screen.getByRole('button', { name: 'Edit Warehouse' }));
    expect(screen.getByRole('heading', { name: 'Edit ClickHouse' })).toBeInTheDocument();
    expect(screen.getByLabelText('Connection name')).toHaveValue('Warehouse');
    expect(screen.getByLabelText('HTTP endpoint')).toHaveValue('http://localhost:8123');
    expect(screen.getByLabelText('Database')).toHaveValue('analytics');
    expect(screen.getByLabelText('Password')).toBeDisabled();
    expect(vi.mocked(fetch)).not.toHaveBeenCalled();

    fireEvent.change(screen.getByLabelText('HTTP endpoint'), { target: { value: 'http://warehouse.internal:8123' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));

    await waitFor(() => expect(vi.mocked(fetch).mock.calls.some(([url, init]) => String(url).endsWith('/api/connections/warehouse') && init?.method === 'PUT')).toBe(true));
    const updateCall = vi.mocked(fetch).mock.calls.find(([url, init]) => String(url).endsWith('/api/connections/warehouse') && init?.method === 'PUT');
    const request = JSON.parse(String(updateCall?.[1]?.body));
    expect(updateCall?.[1]).toMatchObject({ headers: { 'If-Match': '"source-revision"' } });
    expect(request).toEqual({ name: 'Warehouse', url: 'http://warehouse.internal:8123', database: 'analytics' });
    expect(request).not.toHaveProperty('password');
    expect(request).not.toHaveProperty('username');
    expect(await screen.findByText('Connection updated and tested.')).toBeInTheDocument();
    expect(onConnectionsChange).toHaveBeenCalledOnce();
  });
});