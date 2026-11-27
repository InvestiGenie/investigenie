import { useEffect, useMemo, useRef, useState, type ChangeEvent } from 'react';
import { Grid2X2, Plus, RefreshCw, Save, Trash2, Upload } from 'lucide-react';
import { api, type ConnectionSummary, type DataViewSummary, type KnowledgeEntry, type SearchResult } from './api';
import { ErrorAlert, StatusNotice } from './PageFeedback';
import { errorMessage } from './format';

export type DashboardPanelType = 'stat' | 'timeseries' | 'table' | 'logs';
export type DashboardPanel = {
  id: string;
  title: string;
  type: DashboardPanelType;
  data_view_id: string;
  query: string;
  histogram_field?: string;
};
export type DashboardDocument = {
  title: string;
  source: 'native' | 'grafana' | 'kibana';
  panels: DashboardPanel[];
};

function panelId(prefix: string, index: number): string {
  return `${prefix}-${index}-${Math.random().toString(36).slice(2, 8)}`;
}

function grafanaType(panel: Record<string, unknown>): DashboardPanelType {
  const type = String(panel.type ?? '').toLowerCase();
  if (type.includes('stat') || type.includes('gauge') || type.includes('singlestat')) return 'stat';
  if (type.includes('table')) return 'table';
  if (type.includes('log')) return 'logs';
  return 'timeseries';
}

export function normalizeGrafanaDashboard(input: unknown): DashboardDocument {
  const root = input && typeof input === 'object' ? input as Record<string, unknown> : {};
  const dashboard = (root.dashboard && typeof root.dashboard === 'object' ? root.dashboard : root) as Record<string, unknown>;
  const rawPanels = Array.isArray(dashboard.panels) ? dashboard.panels : [];
  const panels = rawPanels.flatMap((raw, index) => {
    if (!raw || typeof raw !== 'object') return [];
    const panel = raw as Record<string, unknown>;
    const nested = Array.isArray(panel.panels) ? panel.panels : [];
    const list = nested.length ? nested : [panel];
    return list.flatMap((nestedPanel, nestedIndex) => {
      if (!nestedPanel || typeof nestedPanel !== 'object') return [];
      const item = nestedPanel as Record<string, unknown>;
      const targets = Array.isArray(item.targets) ? item.targets : [];
      const expr = targets.map((target) => target && typeof target === 'object' ? String((target as Record<string, unknown>).expr ?? '') : '').find(Boolean) ?? '';
      return [{
        id: String(item.id ?? panelId('grafana', index * 20 + nestedIndex)),
        title: String(item.title ?? item.name ?? `Panel ${index + nestedIndex + 1}`),
        type: grafanaType(item),
        data_view_id: '',
        query: expr,
      }];
    });
  });
  return { title: String(dashboard.title ?? 'Imported Grafana dashboard'), source: 'grafana', panels };
}

export function normalizeKibanaExport(text: string): DashboardDocument {
  const objects = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean).flatMap((line) => {
    try { return [JSON.parse(line) as Record<string, unknown>]; } catch { return []; }
  });
  const dashboard = objects.find((item) => item.type === 'dashboard') as Record<string, unknown> | undefined;
  const attrs = dashboard?.attributes && typeof dashboard.attributes === 'object' ? dashboard.attributes as Record<string, unknown> : {};
  const panelsJson = typeof attrs.panelsJSON === 'string' ? attrs.panelsJSON : '[]';
  let kibanaPanels: unknown[] = [];
  try { const parsed = JSON.parse(panelsJson); kibanaPanels = Array.isArray(parsed) ? parsed : []; } catch {}
  const visualizationById = new Map(objects.filter((item) => item.type === 'visualization').map((item) => [String(item.id), item]));
  const panels = kibanaPanels.flatMap((raw, index) => {
    if (!raw || typeof raw !== 'object') return [];
    const panel = raw as Record<string, unknown>;
    const embeddableConfig = panel.embeddableConfig && typeof panel.embeddableConfig === 'object' ? panel.embeddableConfig as Record<string, unknown> : {};
    const panelIndex = String(panel.panelIndex ?? embeddableConfig.id ?? index);
    const saved = visualizationById.get(panelIndex);
    const savedAttrs = saved?.attributes && typeof saved.attributes === 'object' ? saved.attributes as Record<string, unknown> : {};
    let visState: Record<string, unknown> = {};
    try { visState = typeof savedAttrs.visState === 'string' ? JSON.parse(savedAttrs.visState) as Record<string, unknown> : {}; } catch {}
    const typeName = String(visState.type ?? savedAttrs.visualizationType ?? 'table').toLowerCase();
    const type: DashboardPanelType = typeName.includes('metric') || typeName.includes('gauge') ? 'stat' : typeName.includes('table') ? 'table' : typeName.includes('line') || typeName.includes('area') || typeName.includes('histogram') ? 'timeseries' : 'table';
    const title = String(savedAttrs.title ?? panel.title ?? `Panel ${index + 1}`);
    const visQuery = visState.query && typeof visState.query === 'object' ? visState.query as Record<string, unknown> : {};
    const visParams = visState.params && typeof visState.params === 'object' ? visState.params as Record<string, unknown> : {};
    const queryValue = visQuery.query ?? visParams.query ?? '';
    return [{ id: panelIndex, title, type, data_view_id: '', query: typeof queryValue === 'string' ? queryValue : '' }];
  });
  return { title: String(attrs.title ?? 'Imported Kibana dashboard'), source: 'kibana', panels };
}

export function normalizeDashboardFile(text: string): DashboardDocument {
  const trimmed = text.trim();
  if (trimmed.includes('\n') && trimmed.split(/\r?\n/).some((line) => line.includes('"dashboard"') || line.includes('"visualization"'))) {
    return normalizeKibanaExport(trimmed);
  }
  try {
    const parsed = JSON.parse(trimmed);
    if (parsed && typeof parsed === 'object') {
      const ndjson = normalizeKibanaExport(trimmed);
      if (ndjson.panels.length) return ndjson;
      return normalizeGrafanaDashboard(parsed);
    }
  } catch {}
  return { title: 'Imported dashboard', source: 'native', panels: [] };
}

function dashboardFromEntry(entry: KnowledgeEntry): DashboardDocument {
  try {
    const parsed = JSON.parse(entry.content);
    if (parsed && typeof parsed === 'object' && Array.isArray(parsed.panels)) return parsed as DashboardDocument;
  } catch {}
  return { title: entry.title, source: 'native', panels: [] };
}

type Props = { connections: ConnectionSummary[]; dataViews: DataViewSummary[] };

export default function DashboardPage({ connections, dataViews }: Props) {
  const fileRef = useRef<HTMLInputElement>(null);
  const [entries, setEntries] = useState<KnowledgeEntry[]>([]);
  const [selected, setSelected] = useState<KnowledgeEntry | null>(null);
  const [draft, setDraft] = useState<DashboardDocument>({ title: '', source: 'native', panels: [] });
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [results, setResults] = useState<Record<string, SearchResult | null>>({});
  const [panelErrors, setPanelErrors] = useState<Record<string, string>>({});

  useEffect(() => {
    const controller = new AbortController();
    api.knowledgeEntries(controller.signal).then((available) => {
      if (controller.signal.aborted) return;
      setEntries(available.filter((entry) => entry.kind === 'dashboard'));
    }).catch((reason) => setError(errorMessage(reason))).finally(() => setLoading(false));
    return () => controller.abort();
  }, []);

  async function refreshEntries() {
    setEntries((await api.knowledgeEntries()).filter((entry) => entry.kind === 'dashboard'));
  }

  function chooseEntry(entry: KnowledgeEntry) {
    setSelected(entry);
    setDraft(dashboardFromEntry(entry));
    setNotice(''); setError('');
  }

  async function importFile(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    if (!file) return;
    try {
      const normalized = normalizeDashboardFile(await file.text());
      setSelected(null);
      setDraft(normalized.title ? normalized : { ...normalized, title: file.name.replace(/\.(json|ndjson)$/i, '') });
      setNotice(`Imported ${normalized.panels.length} panel(s) from ${normalized.source}.`);
      setError('');
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      event.target.value = '';
    }
  }

  async function saveDraft() {
    if (!draft.title.trim()) { setError('Dashboard title is required.'); return; }
    setBusy(true); setError(''); setNotice('');
    try {
      const input = { title: draft.title.trim(), kind: 'dashboard' as const, status: 'published' as const, content: JSON.stringify(draft), steps: [], services: [], environments: [], connection_ids: [], data_view_ids: [...new Set(draft.panels.map((panel) => panel.data_view_id).filter(Boolean))], tags: ['dashboard', draft.source], source_url: null };
      const saved = selected
        ? await api.updateKnowledge(selected.id, input, selected.revision)
        : await api.addKnowledge(input);
      setSelected(saved);
      setDraft(dashboardFromEntry(saved));
      await refreshEntries();
      setNotice('Dashboard saved.');
    } catch (reason) { setError(errorMessage(reason)); }
    finally { setBusy(false); }
  }

  async function removeDraft() {
    if (!selected) return;
    if (!window.confirm(`Delete ${selected.title}?`)) return;
    setBusy(true); setError(''); setNotice('');
    try {
      await api.removeKnowledge(selected.id, selected.revision);
      setSelected(null);
      setDraft({ title: '', source: 'native', panels: [] });
      await refreshEntries();
      setNotice('Dashboard deleted.');
    } catch (reason) { setError(errorMessage(reason)); }
    finally { setBusy(false); }
  }

  async function runPanel(panel: DashboardPanel) {
    if (!panel.data_view_id) { setPanelErrors((current) => ({ ...current, [panel.id]: 'Choose a Data View.' })); return; }
    const dataView = dataViews.find((view) => view.id === panel.data_view_id);
    if (!dataView) { setPanelErrors((current) => ({ ...current, [panel.id]: 'The selected Data View is unavailable.' })); return; }
    setPanelErrors((current) => ({ ...current, [panel.id]: '' }));
    const now = Date.now();
    const request = {
      index: dataView.index,
      query: panel.query.trim() || '*',
      from: 0,
      size: panel.type === 'stat' ? 1 : 50,
      sort_by: dataView.time_field,
      sort_order: 'desc' as const,
      time_range: { start: now - 15 * 60_000, end: now },
      include_histogram: panel.type === 'timeseries',
      histogram_field: panel.type === 'timeseries' ? panel.histogram_field || dataView.time_field : undefined,
    };
    const connection = connections.find((item) => item.id === dataView.connection_id);
    if (!connection) { setPanelErrors((current) => ({ ...current, [panel.id]: 'The Data View connection is unavailable.' })); return; }
    try {
      setResults((current) => ({ ...current, [panel.id]: null }));
      const result = await api.search(connection.id, request);
      setResults((current) => ({ ...current, [panel.id]: result }));
    } catch (reason) {
      setPanelErrors((current) => ({ ...current, [panel.id]: errorMessage(reason) }));
    }
  }

  const mappedCount = useMemo(() => draft.panels.filter((panel) => panel.data_view_id).length, [draft.panels]);

  return <main className="connections-layout dashboard-layout">
    <div className="connections-heading"><button className="search-button add-connection-button" onClick={() => { setSelected(null); setDraft({ title: '', source: 'native', panels: [] }); setNotice(''); setError(''); }}><Plus size={16} /> New dashboard</button><button className="quiet-button" onClick={() => fileRef.current?.click()}><Upload size={16} /> Import Grafana/Kibana</button><input ref={fileRef} type="file" accept=".json,.ndjson,application/json" hidden onChange={importFile} /></div>
    {error && <ErrorAlert message={error} onDismiss={() => setError('')} />}{notice && <StatusNotice message={notice} />}
    <div className="dashboard-workspace">
      <aside className="dashboard-list"><h2>Dashboards</h2>{loading ? <p>Loading…</p> : entries.length === 0 ? <p className="connections-empty">No dashboards yet</p> : entries.map((entry) => <button key={entry.id} className={selected?.id === entry.id ? 'dashboard-list-item active' : 'dashboard-list-item'} onClick={() => chooseEntry(entry)}><Grid2X2 size={16} /><span>{entry.title}</span></button>)}</aside>
      <section className="dashboard-editor">
        <div className="dashboard-toolbar"><input aria-label="Dashboard title" value={draft.title} placeholder="Dashboard title" onChange={(event) => setDraft((current) => ({ ...current, title: event.target.value }))} /><span>{draft.panels.length} panels · {mappedCount} mapped</span><span className="dashboard-toolbar-actions"><button className="quiet-button" onClick={saveDraft} disabled={busy}><Save size={16} /> Save</button>{selected && <button className="quiet-button" onClick={() => void removeDraft()} disabled={busy}><Trash2 size={16} /> Delete</button>}</span></div>
        {draft.panels.length === 0 ? <div className="dashboard-empty"><Grid2X2 size={28} /><p>Import a Grafana or Kibana export, or add a native panel.</p><button className="search-button" onClick={() => setDraft((current) => ({ ...current, source: 'native', panels: [...current.panels, { id: panelId('native', current.panels.length), title: `Panel ${current.panels.length + 1}`, type: 'timeseries', data_view_id: '', query: '' }] }))}><Plus size={16} /> Add panel</button></div> : <div className="dashboard-grid">{draft.panels.map((panel) => {
          const result = results[panel.id];
          return <article className="dashboard-panel" key={panel.id}><header><input aria-label={`Panel title ${panel.id}`} value={panel.title} onChange={(event) => setDraft((current) => ({ ...current, panels: current.panels.map((item) => item.id === panel.id ? { ...item, title: event.target.value } : item) }))} /><button className="icon-button" title="Run panel" aria-label={`Run ${panel.title}`} onClick={() => void runPanel(panel)}><RefreshCw size={15} /></button><button className="icon-button" title="Remove panel" aria-label={`Remove ${panel.title}`} onClick={() => setDraft((current) => ({ ...current, panels: current.panels.filter((item) => item.id !== panel.id) }))}><Trash2 size={15} /></button></header>
            <div className="dashboard-panel-controls"><select aria-label={`Panel ${panel.title} type`} value={panel.type} onChange={(event) => setDraft((current) => ({ ...current, panels: current.panels.map((item) => item.id === panel.id ? { ...item, type: event.target.value as DashboardPanelType } : item) }))}><option value="stat">Stat</option><option value="timeseries">Time series</option><option value="table">Table</option><option value="logs">Logs</option></select><select aria-label={`Panel ${panel.title} Data View`} value={panel.data_view_id} onChange={(event) => setDraft((current) => ({ ...current, panels: current.panels.map((item) => item.id === panel.id ? { ...item, data_view_id: event.target.value } : item) }))}><option value="">Select Data View</option>{dataViews.map((view) => <option key={view.id} value={view.id}>{view.name}</option>)}</select><input aria-label={`Panel ${panel.title} query`} value={panel.query} placeholder="Query" onChange={(event) => setDraft((current) => ({ ...current, panels: current.panels.map((item) => item.id === panel.id ? { ...item, query: event.target.value } : item) }))} /></div>
            {panelErrors[panel.id] ? <p className="dashboard-panel-error">{panelErrors[panel.id]}</p> : result ? <div className="dashboard-panel-result">{panel.type === 'stat' ? <strong>{result.total}</strong> : panel.type === 'timeseries' ? <div className="dashboard-series">{result.histogram?.map((bucket) => <span key={bucket.timestamp} title={bucket.label ?? ''}>{bucket.count}</span>) ?? <span>No histogram</span>}</div> : <table><tbody>{result.hits.slice(0, 10).map((hit) => <tr key={`${hit.index}:${hit.id}`}><td>{hit.index}</td><td>{JSON.stringify(hit.source)}</td></tr>)}</tbody></table>}</div> : <p className="dashboard-panel-empty">Run panel to load data</p>}
          </article>; })}</div>}
      </section>
    </div>
  </main>;
}
