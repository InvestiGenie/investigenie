import { useEffect, useState, type FormEvent } from 'react';
import { api, type ConnectionSummary, type DataViewSummary, type FieldSummary, type IndexSummary } from './api';
import IntegrationIcon from './IntegrationIcon';
import SearchableSelect from './SearchableSelect';
import { errorMessage, formatCount } from './format';
import { ErrorAlert, StatusNotice } from './PageFeedback';

type DataViewsPageProps = {
  connections: ConnectionSummary[];
  dataViews: DataViewSummary[];
  onDataViewsChange: () => Promise<void>;
};

function isTimeField(field: FieldSummary): boolean {
  if (field.is_time_field !== undefined) return field.is_time_field;
  const fieldType = field.field_type.replace(/^Nullable\((.*)\)$/i, '$1');
  return /^(date|date32|date_nanos|datetime(?:64)?(?:\([^)]*\))?)$/i.test(fieldType);
}

export default function DataViewsPage({ connections, dataViews, onDataViewsChange }: DataViewsPageProps) {
  const [stage, setStage] = useState<'list' | 'configure'>('list');
  const [editingViewId, setEditingViewId] = useState<string | null>(null);
  const [editingRevision, setEditingRevision] = useState('');
  const [name, setName] = useState('');
  const [connectionId, setConnectionId] = useState('');
  const [index, setIndex] = useState('');
  const [timeField, setTimeField] = useState('');
  const [indices, setIndices] = useState<IndexSummary[]>([]);
  const [indicesLoading, setIndicesLoading] = useState(false);
  const [fields, setFields] = useState<FieldSummary[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');

  useEffect(() => {
    if (stage !== 'configure' || !connectionId) {
      setIndices([]);
      return;
    }
    const controller = new AbortController();
    setIndices([]);
    setIndicesLoading(true);
    api.indices(connectionId, controller.signal)
      .then((available) => {
        if (!controller.signal.aborted) setIndices([...available].sort((left, right) => left.name.localeCompare(right.name)));
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(errorMessage(reason));
      })
      .finally(() => {
        if (!controller.signal.aborted) setIndicesLoading(false);
      });
    return () => controller.abort();
  }, [connectionId, stage]);

  useEffect(() => {
    if (stage !== 'configure' || !connectionId || !index) {
      setFields([]);
      return;
    }
    const controller = new AbortController();
    setFields([]);
    api.fields(connectionId, index, controller.signal)
      .then((available) => {
        if (controller.signal.aborted) return;
        setFields(available);
        const firstTimeField = available.find(isTimeField);
        setTimeField((current) => available.some((field) => field.name === current && isTimeField(field))
          ? current : firstTimeField?.name ?? '');
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(errorMessage(reason));
      });
    return () => controller.abort();
  }, [connectionId, index, stage]);

  function configureDataView(view?: DataViewSummary) {
    setError('');
    setNotice('');
    setEditingViewId(view?.id ?? null);
    setEditingRevision(view?.revision ?? '');
    setName(view?.name ?? '');
    setConnectionId(view?.connection_id ?? '');
    setIndex(view?.index ?? '');
    setTimeField(view?.time_field ?? '');
    setStage('configure');
  }

  async function createDataView(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const wasEditing = editingViewId !== null;
    setBusy(true);
    setError('');
    setNotice('');
    try {
      const request = { name: name.trim(), connection_id: connectionId, index, time_field: timeField };
      if (editingViewId) await api.updateDataView(editingViewId, request, editingRevision);
      else await api.addDataView(request);
      await onDataViewsChange();
      setName('');
      setStage('list');
      setEditingViewId(null);
      setNotice(wasEditing ? 'Data view validated and updated.' : 'Data view validated and created.');
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  async function removeDataView(view: DataViewSummary) {
    setError('');
    setNotice('');
    try {
      await api.removeDataView(view.id, view.revision);
      await onDataViewsChange();
      setNotice(`${view.name} removed.`);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  return <main className="connections-layout data-views-layout">
    <div className="connections-heading">
      {stage === 'list' && <button className="search-button add-connection-button" onClick={() => configureDataView()}><span aria-hidden="true">＋</span> Add data view</button>}
    </div>

    {error && <ErrorAlert message={error} onDismiss={() => setError('')} />}
    {notice && <StatusNotice message={notice} />}

    {stage === 'list' && <section className="connections-section" aria-labelledby="saved-data-views-title">
      <div className="section-heading"><div><h2 id="saved-data-views-title">Saved data views</h2><p>Only validated data views are available in Discover.</p></div><span className="field-count">{dataViews.length}</span></div>
      {dataViews.length === 0
        ? <p className="connections-empty">No data views available</p>
        : <div className="connection-list">{dataViews.map((view) => {
          const connection = connections.find((item) => item.id === view.connection_id);
          return <article className="data-view-row" key={view.id}>
            <IntegrationIcon integration={connection?.integration} />
            <div className="connection-info"><strong>{view.name}</strong><span>{connection?.name ?? 'Unavailable connection'} · {view.index}</span></div>
            <span className="connection-kind">Time field · {view.time_field}</span>
            <div className="connection-actions"><button className="edit-connection" aria-label={`Edit ${view.name}`} title={`Edit ${view.name}`} onClick={() => configureDataView(view)}>✎</button><button className="remove-connection" aria-label={`Remove ${view.name}`} title={`Remove ${view.name}`} onClick={() => void removeDataView(view)}>×</button></div>
          </article>;
        })}</div>}
    </section>}

    {stage === 'configure' && <section className="connections-section add-connection-section" aria-labelledby="create-data-view-title">
      <div className="section-heading"><div><h2 id="create-data-view-title">{editingViewId ? 'Edit data view' : 'Create a data view'}</h2><p>The connection, index/table, and time field are checked before saving.</p></div><button className="quiet-button" disabled={busy} onClick={() => { setError(''); setStage('list'); }}>Cancel</button></div>
      {connections.length === 0
        ? <p className="connections-empty">Add a connection before creating a data view.</p>
        : <form className="connection-form" onSubmit={(event) => void createDataView(event)}>
          <div className="form-grid">
            <label>Data view name<input required maxLength={64} value={name} onChange={(event) => setName(event.target.value)} placeholder="Application logs" /></label>
            <label>Connection<select required value={connectionId} onChange={(event) => { setConnectionId(event.target.value); setIndex(''); setTimeField(''); setError(''); }}><option value="">Select a connection</option>{connections.map((connection) => <option key={connection.id} value={connection.id}>{connection.name}</option>)}</select></label>
            <div className="form-field"><span className="form-field-label" aria-hidden="true">Index / table</span><SearchableSelect
              label="Index / table"
              value={index}
              disabled={!connectionId}
              loading={indicesLoading}
              placeholder="Search or enter an index pattern"
              emptyLabel={connectionId ? 'Select an index or table' : 'Select a connection first'}
              allowCustom
              customLabel="Use index pattern or table"
              options={indices.map((item) => ({
                value: item.name,
                description: item.document_count ? `${formatCount(item.document_count, { notation: 'compact' })} docs` : undefined,
                badge: item.health ?? undefined,
                tone: item.health ? `health-${item.health}` : undefined,
              }))}
              onChange={(value) => { setIndex(value); setTimeField(''); setError(''); }}
            /></div>
            <label>Time series time field<select required value={timeField} disabled={!index || fields.filter(isTimeField).length === 0} onChange={(event) => setTimeField(event.target.value)}><option value="">Select a date/time field</option>{fields.filter(isTimeField).map((field) => <option key={field.name} value={field.name}>{field.name} · {field.field_type}</option>)}</select></label>
          </div>
          {index && fields.length > 0 && fields.filter(isTimeField).length === 0 && <p className="connections-empty">The selected source has no date/time fields; this index/table cannot be used for a time-series data view.</p>}
          <div className="connection-form-footer"><span>InvestiGenie data views</span><button className="search-button" type="submit" disabled={busy || !connectionId || !fields.some((field) => field.name === timeField && isTimeField(field))}>{busy ? <span className="spinner" /> : <span aria-hidden="true">＋</span>}{busy ? 'Validating' : editingViewId ? 'Save changes' : 'Create data view'}</button></div>
        </form>}
    </section>}
  </main>;
}
