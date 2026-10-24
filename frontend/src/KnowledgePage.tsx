import { useEffect, useState, type FormEvent } from 'react';
import { Archive, BookOpen, Check, Edit3, Plus, RefreshCw, Save, Search, Trash2, Undo2, X } from 'lucide-react';
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { api, type ConnectionSummary, type DataViewSummary, type KnowledgeEntry, type KnowledgeInput, type KnowledgeKind, type KnowledgeStatus } from './api';
import { ErrorAlert, StatusNotice } from './PageFeedback';
import { errorMessage } from './format';
import { initialTimeZone } from './time';

const kinds: Record<KnowledgeKind, string> = { service_context: 'Service context', runbook: 'Runbook', resolved_incident: 'Resolved incident' };
const statuses: Record<KnowledgeStatus, string> = { draft: 'Draft', published: 'Published', archived: 'Archived' };
type KnowledgeForm = Omit<KnowledgeInput, 'services' | 'environments' | 'tags' | 'source_url'> & { services: string; environments: string; tags: string; source_url: string };

function formFor(entry?: KnowledgeEntry): KnowledgeForm {
  return {
    title: entry?.title ?? '', kind: entry?.kind ?? 'runbook', status: entry?.status ?? 'draft', content: entry?.content ?? '',
    services: entry?.services.join(', ') ?? '', environments: entry?.environments.join(', ') ?? '', tags: entry?.tags.join(', ') ?? '',
    connection_ids: entry?.connection_ids ?? [], data_view_ids: entry?.data_view_ids ?? [], source_url: entry?.source_url ?? '',
  };
}

function inputFor(form: KnowledgeForm): KnowledgeInput {
  const labels = (value: string) => [...new Set(value.split(',').map((label) => label.trim()).filter(Boolean))];
  return { ...form, title: form.title.trim(), services: labels(form.services), environments: labels(form.environments), tags: labels(form.tags), source_url: form.source_url.trim() || null };
}

function EntryContent({ content }: { content: string }) {
  return <div className="knowledge-markdown"><Markdown remarkPlugins={[remarkGfm]} skipHtml
    urlTransform={(url) => /^(https?:\/\/|#)/i.test(url) ? url : ''}
    components={{ img: ({ alt }) => <span>{alt ? `[${alt}]` : '[Image]'}</span>, a: ({ href, children }) => href ? <a href={href} target="_blank" rel="noopener noreferrer">{children}</a> : <span>{children}</span> }}
  >{content}</Markdown></div>;
}

function ScopePicker({ label, choices, selected, onChange, disabled }: { label: string; choices: Array<{ id: string; name: string }>; selected: string[]; onChange: (ids: string[]) => void; disabled: boolean }) {
  const [query, setQuery] = useState('');
  const available = [...choices, ...selected.filter((id) => !choices.some((choice) => choice.id === id)).map((id) => ({ id, name: `${id} (unavailable)` }))];
  const matches = available.filter((choice) => choice.name.toLocaleLowerCase().includes(query.toLocaleLowerCase())).slice(0, 50);
  return <fieldset className="knowledge-scope" disabled={disabled}><legend>{label}</legend>
    <input aria-label={`Search ${label.toLowerCase()}`} value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search" />
    <div className="knowledge-scope-options">{matches.length === 0 ? <span className="knowledge-muted">No matches</span> : matches.map((choice) => <label key={choice.id}>
      <input type="checkbox" checked={selected.includes(choice.id)} disabled={!selected.includes(choice.id) && selected.length >= 20} onChange={(event) => onChange(event.target.checked ? [...selected, choice.id] : selected.filter((id) => id !== choice.id))} />{choice.name}
    </label>)}</div>
  </fieldset>;
}

type Props = { connections?: ConnectionSummary[]; dataViews?: DataViewSummary[]; onDirtyChange?: (dirty: boolean) => void; initialEntryId?: string | null };

export default function KnowledgePage({ connections = [], dataViews = [], onDirtyChange, initialEntryId = null }: Props) {
  const [entries, setEntries] = useState<KnowledgeEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [selected, setSelected] = useState<KnowledgeEntry | null>(null);
  const [form, setForm] = useState<KnowledgeForm | null>(null);
  const [baseline, setBaseline] = useState<KnowledgeForm | null>(null);
  const [preview, setPreview] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [query, setQuery] = useState('');
  const [status, setStatus] = useState('');
  const [kind, setKind] = useState('');
  const [service, setService] = useState('');
  const [environment, setEnvironment] = useState('');
  const dirty = form !== null && JSON.stringify(form) !== JSON.stringify(baseline);
  const contentBytes = new TextEncoder().encode(form?.content ?? '').length;

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setLoadError('');
    api.knowledgeEntries(controller.signal).then((available) => {
      if (!controller.signal.aborted) {
        setEntries(available);
        if (initialEntryId) {
          const entry = available.find((item) => item.id === initialEntryId);
          if (entry) {
            const next = formFor(entry);
            setSelected(entry); setForm(next); setBaseline(next); setPreview(false);
          } else {
            setError('The requested knowledge entry is no longer available.');
          }
        }
      }
    }).catch((reason: unknown) => {
      if (!controller.signal.aborted) setLoadError(errorMessage(reason));
    }).finally(() => {
      if (!controller.signal.aborted) setLoading(false);
    });
    return () => controller.abort();
  }, [attempt, initialEntryId]);

  useEffect(() => {
    onDirtyChange?.(dirty || busy);
    return () => onDirtyChange?.(false);
  }, [dirty, busy, onDirtyChange]);

  useEffect(() => {
    if (!dirty) return;
    const preventExit = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ''; };
    window.addEventListener('beforeunload', preventExit);
    return () => window.removeEventListener('beforeunload', preventExit);
  }, [dirty]);

  function canLeave() { return !busy && (!dirty || window.confirm('Discard unsaved knowledge changes?')); }
  function configure(entry?: KnowledgeEntry) {
    if (!canLeave()) return;
    const next = formFor(entry);
    setSelected(entry ?? null); setForm(next); setBaseline(next); setPreview(false); setError(''); setNotice('');
  }
  function open(entry: KnowledgeEntry) {
    if (!canLeave()) return;
    setSelected(entry); setForm(null); setBaseline(null); setError(''); setNotice('');
  }
  function accept(entry: KnowledgeEntry) {
    setEntries((current) => [entry, ...current.filter((item) => item.id !== entry.id)]);
    setSelected(entry); setForm(null); setBaseline(null);
  }
  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!form || busy) return;
    if (contentBytes > 32_768) { setError('Content exceeds the 32768-byte limit.'); return; }
    setBusy(true); setError(''); setNotice('');
    try {
      const input = inputFor(form);
      const entry = selected ? await api.updateKnowledge(selected.id, input, selected.revision) : await api.addKnowledge(input);
      accept(entry); setNotice('Knowledge entry saved.');
    } catch (reason) { setError(errorMessage(reason)); } finally { setBusy(false); }
  }
  async function changeStatus(next: KnowledgeStatus) {
    if (!selected || busy) return;
    setBusy(true); setError(''); setNotice('');
    try {
      accept(await api.updateKnowledge(selected.id, { ...inputFor(formFor(selected)), status: next }, selected.revision));
      setNotice(`Knowledge entry ${next === 'draft' ? 'returned to draft' : next}.`);
    } catch (reason) { setError(errorMessage(reason)); } finally { setBusy(false); }
  }
  async function remove() {
    if (!selected || busy || !window.confirm(`Permanently delete "${selected.title}"? This cannot be undone.`)) return;
    setBusy(true); setError(''); setNotice('');
    try {
      await api.removeKnowledge(selected.id, selected.revision);
      setEntries((current) => current.filter((entry) => entry.id !== selected.id));
      setSelected(null); setNotice('Knowledge entry permanently deleted.');
    } catch (reason) { setError(errorMessage(reason)); } finally { setBusy(false); }
  }

  const terms = query.toLocaleLowerCase().trim().split(/\s+/).filter(Boolean);
  const visible = entries.filter((entry) => {
    const text = [entry.title, entry.content, ...entry.tags, ...entry.services, ...entry.environments].join(' ').toLocaleLowerCase();
    return (!status || entry.status === status) && (!kind || entry.kind === kind) && (!service || entry.services.includes(service)) && (!environment || entry.environments.includes(environment)) && terms.every((term) => text.includes(term));
  });
  const services = [...new Set(entries.flatMap((entry) => entry.services))].sort();
  const environments = [...new Set(entries.flatMap((entry) => entry.environments))].sort();
  const date = (value: number) => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short', timeZone: initialTimeZone() }).format(value);

  return <main className="knowledge-page">
    <div className="knowledge-toolbar">
      <div className="knowledge-search"><Search size={16} aria-hidden="true" /><input aria-label="Search knowledge" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search knowledge" /></div>
      <select aria-label="Filter by status" value={status} onChange={(event) => setStatus(event.target.value)}><option value="">All statuses</option>{Object.entries(statuses).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
      <select aria-label="Filter by type" value={kind} onChange={(event) => setKind(event.target.value)}><option value="">All types</option>{Object.entries(kinds).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
      <select aria-label="Filter by service" value={service} onChange={(event) => setService(event.target.value)}><option value="">All services</option>{services.map((value) => <option key={value}>{value}</option>)}</select>
      <select aria-label="Filter by environment" value={environment} onChange={(event) => setEnvironment(event.target.value)}><option value="">All environments</option>{environments.map((value) => <option key={value}>{value}</option>)}</select>
      <button className="search-button" disabled={busy || loading || !!loadError} onClick={() => configure()}><Plus size={16} /> New entry</button>
    </div>
    {loadError && <div className="knowledge-error"><ErrorAlert message={loadError} onDismiss={() => setLoadError('')} /><button className="quiet-button" onClick={() => setAttempt((current) => current + 1)}><RefreshCw size={14} /> Retry loading knowledge</button></div>}
    {error && <div className="knowledge-error"><ErrorAlert message={error} onDismiss={() => setError('')} /><button className="quiet-button" disabled={busy} onClick={() => {
      if (!canLeave()) return;
      setForm(null); setSelected(null); setBaseline(null); setAttempt((current) => current + 1); setError('');
    }}><RefreshCw size={14} /> Reload entries</button></div>}
    {notice && <StatusNotice message={notice} />}
    <div className="knowledge-layout">
      <section className="knowledge-list" aria-label="Knowledge entries">
        <div className="knowledge-list-heading"><h2>Entries</h2><span>{visible.length} / {entries.length}</span></div>
        {loading ? <p role="status">Loading knowledge...</p> : !visible.length ? <div className="knowledge-empty"><BookOpen size={28} aria-hidden="true" /><h3>{entries.length ? 'No matching entries' : 'No knowledge entries'}</h3></div> : visible.map((entry) => <button className={`knowledge-list-item${selected?.id === entry.id ? ' selected' : ''}`} key={entry.id} aria-pressed={selected?.id === entry.id} disabled={busy} onClick={() => open(entry)}>
          <strong>{entry.title}</strong><span>{kinds[entry.kind]} <span className={`knowledge-status ${entry.status}`}>{statuses[entry.status]}</span></span>
          <span>{entry.services.join(', ') || 'All services'}</span><time dateTime={new Date(entry.updated_at_ms).toISOString()}>{date(entry.updated_at_ms)}</time>
        </button>)}
      </section>
      <section className="knowledge-detail" aria-label={form ? 'Knowledge editor' : 'Knowledge detail'}>
        {form ? <form onSubmit={(event) => void save(event)}>
          <div className="knowledge-detail-heading"><h2>{selected ? 'Edit entry' : 'New entry'}</h2><button type="button" className="quiet-button" disabled={busy} onClick={() => { if (canLeave()) { setForm(null); setBaseline(null); setError(''); } }}><X size={14} /> Cancel</button></div>
          <fieldset className="knowledge-fields" disabled={busy}>
            <label className="knowledge-full">Title<input required maxLength={256} value={form.title} onChange={(event) => setForm({ ...form, title: event.target.value })} /></label>
            <label>Type<select value={form.kind} onChange={(event) => setForm({ ...form, kind: event.target.value as KnowledgeKind })}>{Object.entries(kinds).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
            <label>Status<select value={form.status} onChange={(event) => setForm({ ...form, status: event.target.value as KnowledgeStatus })}>{Object.entries(statuses).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
            <label>Services<input value={form.services} onChange={(event) => setForm({ ...form, services: event.target.value })} placeholder="payments, checkout" /></label>
            <label>Environments<input value={form.environments} onChange={(event) => setForm({ ...form, environments: event.target.value })} placeholder="production, staging" /></label>
            <label className="knowledge-full">Tags<input value={form.tags} onChange={(event) => setForm({ ...form, tags: event.target.value })} placeholder="database, connection-pool" /></label>
            <label className="knowledge-full">Source link<input type="url" maxLength={2048} value={form.source_url} onChange={(event) => setForm({ ...form, source_url: event.target.value })} /></label>
          </fieldset>
          <div className="knowledge-scopes"><ScopePicker label="Connections" disabled={busy} choices={connections} selected={form.connection_ids} onChange={(ids) => setForm({ ...form, connection_ids: ids })} /><ScopePicker label="Data Views" disabled={busy} choices={dataViews} selected={form.data_view_ids} onChange={(ids) => setForm({ ...form, data_view_ids: ids })} /></div>
          <div className="knowledge-editor-heading"><label htmlFor="knowledge-content">Content</label><div role="tablist" aria-label="Content mode"><button type="button" role="tab" aria-selected={!preview} onClick={() => setPreview(false)}>Write</button><button type="button" role="tab" aria-selected={preview} onClick={() => setPreview(true)}>Preview</button></div></div>
          {preview ? <div className="knowledge-preview" role="tabpanel"><EntryContent content={form.content} /></div> : <textarea id="knowledge-content" required maxLength={32_768} disabled={busy} value={form.content} onChange={(event) => setForm({ ...form, content: event.target.value })} />}
          <div className="knowledge-footer"><span className={contentBytes > 32_768 ? 'knowledge-over-limit' : ''}>{contentBytes.toLocaleString()} / 32,768 bytes</span><button className="search-button" type="submit" disabled={busy || !form.content.trim() || !form.title.trim() || contentBytes > 32_768}><Save size={15} />{busy ? 'Saving...' : 'Save entry'}</button></div>
        </form> : selected ? <>
          <div className="knowledge-detail-heading"><div><span className={`knowledge-status ${selected.status}`}>{statuses[selected.status]}</span><h2>{selected.title}</h2><span className="knowledge-muted">{kinds[selected.kind]} · {date(selected.updated_at_ms)}</span></div><button className="quiet-button" disabled={busy} onClick={() => configure(selected)}><Edit3 size={14} /> Edit entry</button></div>
          <div className="knowledge-actions">
            {selected.status === 'draft' && <button className="quiet-button" disabled={busy} onClick={() => void changeStatus('published')}><Check size={14} /> Publish</button>}
            {selected.status === 'published' && <button className="quiet-button" disabled={busy} onClick={() => void changeStatus('draft')}><Undo2 size={14} /> Return to draft</button>}
            {selected.status === 'archived' ? <button className="quiet-button" disabled={busy} onClick={() => void changeStatus('draft')}><Undo2 size={14} /> Restore to draft</button> : <button className="quiet-button" disabled={busy} onClick={() => void changeStatus('archived')}><Archive size={14} /> Archive</button>}
            <button className="icon-button knowledge-delete" title="Delete entry permanently" aria-label="Delete entry permanently" disabled={busy} onClick={() => void remove()}><Trash2 size={15} /></button>
          </div>
          <dl className="knowledge-metadata">{([
            ['Services', selected.services.join(', ') || 'All services'], ['Environments', selected.environments.join(', ') || 'All environments'],
            ['Connections', selected.connection_ids.map((id) => connections.find((item) => item.id === id)?.name ?? `${id} (unavailable)`).join(', ') || 'All connections'],
            ['Data Views', selected.data_view_ids.map((id) => dataViews.find((item) => item.id === id)?.name ?? `${id} (unavailable)`).join(', ') || 'All Data Views'],
            ['Tags', selected.tags.join(', ') || 'None'], ['Created', date(selected.created_at_ms)],
          ]).map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{value}</dd></div>)}</dl>
          {selected.source_url && <a className="knowledge-source" href={selected.source_url} target="_blank" rel="noopener noreferrer">Source reference</a>}
          <EntryContent content={selected.content} />
        </> : <div className="knowledge-empty"><BookOpen size={34} aria-hidden="true" /><h2>{entries.length ? 'Select an entry' : 'Knowledge'}</h2></div>}
      </section>
    </div>
  </main>;
}
