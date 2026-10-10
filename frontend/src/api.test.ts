import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, makeSearchRequest } from './api';

describe('API requests', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('probes API readiness and forwards cancellation', async () => {
    const signal = new AbortController().signal;
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ status: 'ready' }) });
    vi.stubGlobal('fetch', fetchMock);
    await expect(api.ready(signal)).resolves.toBeUndefined();
    expect(fetchMock).toHaveBeenCalledWith(expect.stringContaining('/api/health/ready'), { signal });
  });

  it.each([503, 500])('rejects unsuccessful readiness responses (%s)', async (status) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status, json: async () => null }));
    await expect(api.ready()).rejects.toThrow(`Request failed (${status})`);
  });

  it('rejects readiness transport failures', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('Failed to fetch')));
    await expect(api.ready()).rejects.toThrow('Cannot reach the Northstar API');
  });

  it.each(['connected', 'not_connected'] as const)('validates datasource status %s and forwards cancellation', async (status) => {
    const signal = new AbortController().signal;
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ status }) });
    vi.stubGlobal('fetch', fetchMock);
    await expect(api.connectionStatus('source/id', signal)).resolves.toBe(status);
    expect(fetchMock).toHaveBeenCalledWith(expect.stringContaining('/api/connections/source%2Fid/status'), { signal });
  });

  it.each([null, {}, [], 'connected', { status: 'ready' }, { status: true }, { status: null }])('rejects malformed successful status responses (%j)', async (body) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => body }));
    await expect(api.connectionStatus('source')).rejects.toThrow('Invalid datasource connection status response');
  });

  it('rejects a successful status response with invalid JSON', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => { throw new SyntaxError('Invalid JSON'); } }));
    await expect(api.connectionStatus('source')).rejects.toThrow('Invalid datasource connection status response');
  });

  it.each([404, 500, 503])('rejects HTTP status errors even with a connected response (%s)', async (status) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status, json: async () => ({ status: 'connected' }) }));
    await expect(api.connectionStatus('source')).rejects.toThrow(`Request failed (${status})`);
  });

  it('rejects status transport failures and preserves cancellation', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('Failed to fetch')));
    await expect(api.connectionStatus('source')).rejects.toThrow('Cannot reach the Northstar API');
    const controller = new AbortController();
    const cancellation = new DOMException('Aborted', 'AbortError');
    controller.abort();
    vi.mocked(fetch).mockRejectedValue(cancellation);
    await expect(api.connectionStatus('source', controller.signal)).rejects.toBe(cancellation);
  });

  it('creates configurations without revision preconditions or summary-only data', async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, json: async () => null });
    vi.stubGlobal('fetch', fetchMock);
    const connection = { name: 'Source', connection_type: 'clickhouse', url: 'http://localhost:8123' };
    const dataView = { name: 'Logs', connection_id: 'source', index: 'logs', time_field: '@timestamp' };
    await api.addConnection(connection);
    await api.addDataView(dataView);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    for (const [, init] of fetchMock.mock.calls) {
      expect(init.method).toBe('POST');
      expect(init.headers).toEqual({ 'Content-Type': 'application/json' });
      expect(JSON.parse(init.body)).not.toHaveProperty('revision');
      expect(JSON.parse(init.body)).not.toHaveProperty('id');
    }
    expect(JSON.parse(fetchMock.mock.calls[0][1].body)).toEqual(connection);
    expect(JSON.parse(fetchMock.mock.calls[1][1].body)).toEqual(dataView);
  });

  it('surfaces revision precondition errors without retrying', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: false, status: 428, json: async () => ({ error: { message: 'If-Match is required.' } }),
    });
    vi.stubGlobal('fetch', fetchMock);
    await expect(api.removeConnection('source', 'revision')).rejects.toThrow('If-Match is required.');
    expect(fetchMock).toHaveBeenCalledOnce();
  });

  it('sends quoted opaque revisions for every configuration update and delete', async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, json: async () => null });
    vi.stubGlobal('fetch', fetchMock);
    const connection = { name: 'Source' };
    const dataView = { name: 'Logs', connection_id: 'source', index: 'logs', time_field: '@timestamp' };
    await api.updateConnection('source/id', connection, 'opaque-revision');
    await api.removeConnection('source/id', 'opaque-revision');
    await api.updateDataView('view/id', dataView, 'opaque-revision');
    await api.removeDataView('view/id', 'opaque-revision');
    expect(fetchMock).toHaveBeenCalledTimes(4);
    for (const [url, init] of fetchMock.mock.calls) {
      expect(url).toMatch(/\/(source|view)%2Fid$/);
      expect(init.headers['If-Match']).toBe('"opaque-revision"');
    }
    expect(fetchMock.mock.calls.map(([, init]) => init.method)).toEqual(['PUT', 'DELETE', 'PUT', 'DELETE']);
    expect(JSON.parse(fetchMock.mock.calls[0][1].body)).toEqual(connection);
    expect(JSON.parse(fetchMock.mock.calls[2][1].body)).toEqual(dataView);
  });

  it.each(['updateConnection', 'removeConnection', 'updateDataView', 'removeDataView'] as const)(
    'surfaces configuration conflicts without retrying %s', async (operation) => {
      const fetchMock = vi.fn().mockResolvedValue({
        ok: false, status: 409,
        json: async () => ({ error: { code: 'configuration_conflict', message: 'Configuration changed. Reload before trying again.' } }),
      });
      vi.stubGlobal('fetch', fetchMock);
      const result = operation === 'updateConnection' ? api.updateConnection('source', { name: 'Source' }, 'old')
        : operation === 'updateDataView' ? api.updateDataView('view', { name: 'Logs', connection_id: 'source', index: 'logs', time_field: '@timestamp' }, 'old')
          : api[operation]('id', 'old');
      await expect(result).rejects.toThrow('Configuration changed. Reload before trying again.');
      expect(fetchMock).toHaveBeenCalledTimes(1);
    },
  );

  it('identifies transport failures as application API connectivity problems', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('Failed to fetch')));
    await expect(api.integrations()).rejects.toThrow('Cannot reach the Northstar API');
  });

  it('preserves aborted requests without reporting an offline API', async () => {
    const controller = new AbortController();
    const cancellation = new DOMException('Aborted', 'AbortError');
    controller.abort();
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(cancellation));
    await expect(api.integrations(controller.signal)).rejects.toBe(cancellation);
  });

  it('preserves validation messages returned by the backend', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: false, status: 400, json: async () => ({ error: { message: 'Invalid connection' } }),
    }));
    await expect(api.addConnection({ name: 'Source', connection_type: 'clickhouse', url: 'invalid' })).rejects.toThrow('Invalid connection');
  });
});

describe('makeSearchRequest', () => {
  it('interprets start/end dates as UTC independently of the browser timezone', () => {
    const start = '2026-10-08T09:00:00';
    const end = '2026-10-08T10:00:00';
    expect(makeSearchRequest('events', 'SELECT * FROM events', 0, '', 'created_at', 'desc', start, end)).toMatchObject({
      query: 'SELECT * FROM events', time_range: { start: Date.parse(`${start}Z`), end: Date.parse(`${end}Z`) },
    });
  });

  it('searches 08:18 UTC when the user enters 08:18, not a local-time offset', () => {
    expect(makeSearchRequest('logs', '', 0, '', '@timestamp', 'desc', '2026-10-06T08:18', '2026-10-06T08:19:20')).toMatchObject({
      time_range: { start: Date.parse('2026-10-06T08:18:00Z'), end: Date.parse('2026-10-06T08:19:20Z') },
    });
  });

  it('rejects incomplete, invalid, reversed, and fieldless time windows', () => {
    for (const [start, end] of [['2026-10-08T09:00', ''], ['', '2026-10-08T10:00'], ['invalid', '2026-10-08T10:00'], ['2026-10-08T11:00', '2026-10-08T10:00']]) {
      expect(() => makeSearchRequest('events', '', 0, '', 'created_at', 'desc', start, end)).toThrow('Choose valid start and end dates');
    }
    expect(() => makeSearchRequest('events', '', 0, '', undefined, 'desc', '2026-10-08T09:00', '2026-10-08T10:00')).toThrow('requires the data view time field');
  });

  it('always requests a bounded page and preserves the Lucene query', () => {
    expect(makeSearchRequest('logs-*', 'service.name:gateway', 50, '')).toEqual({
      index: 'logs-*',
      query: 'service.name:gateway',
      from: 50,
      size: 50,
    });
  });

  it('adds descending field sort only when selected', () => {
    expect(makeSearchRequest('logs-*', '', 0, '@timestamp', '@timestamp')).toEqual({
      index: 'logs-*',
      query: '',
      from: 0,
      size: 50,
      sort_by: '@timestamp',
      sort_order: 'desc',
      histogram_field: '@timestamp',
    });
  });

  it('preserves ascending sort direction for a selected column', () => {
    expect(makeSearchRequest('events', 'SELECT * FROM events', 0, 'event_id', 'created_at', 'asc')).toMatchObject({
      sort_by: 'event_id', sort_order: 'asc', histogram_field: 'created_at',
    });
  });
});