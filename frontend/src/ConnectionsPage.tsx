import { useEffect, useState, type FormEvent } from 'react';
import { api, type ConnectionConfiguration, type ConnectionField, type ConnectionSummary, type IntegrationDescriptor } from './api';
import ConnectionStatus from './ConnectionStatus';
import { errorMessage } from './format';
import { ErrorAlert, StatusNotice } from './PageFeedback';

type ConnectionsPageProps = {
  connections: ConnectionSummary[];
  onConnectionsChange: () => Promise<void>;
};

export default function ConnectionsPage({ connections, onConnectionsChange }: ConnectionsPageProps) {
  const [integration, setIntegration] = useState<IntegrationDescriptor | null>(null);
  const [configuration, setConfiguration] = useState<ConnectionConfiguration>({});
  const [stage, setStage] = useState<'list' | 'choose-source' | 'configure'>('list');
  const [editingConnectionId, setEditingConnectionId] = useState<string | null>(null);
  const [editingRevision, setEditingRevision] = useState('');
  const [savedConfiguration, setSavedConfiguration] = useState<ConnectionConfiguration>({});
  const [replacements, setReplacements] = useState<Record<string, boolean>>({});
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [integrations, setIntegrations] = useState<IntegrationDescriptor[]>([]);
  const [integrationStatus, setIntegrationStatus] = useState<'loading' | 'loaded' | 'failed'>('loading');
  const [integrationError, setIntegrationError] = useState('');
  const [integrationAttempt, setIntegrationAttempt] = useState(0);

  useEffect(() => {
    if (stage !== 'choose-source') return;
    const controller = new AbortController();
    setIntegrationStatus('loading');
    setIntegrationError('');
    setIntegrations([]);
    api.integrations(controller.signal)
      .then((available) => {
        if (!controller.signal.aborted) {
          setIntegrations(available);
          setIntegrationStatus('loaded');
        }
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) {
          setIntegrationError(errorMessage(reason, 'Could not load data sources'));
          setIntegrationStatus('failed');
        }
      });
    return () => controller.abort();
  }, [stage, integrationAttempt]);

  async function addConnection(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!integration) return;
    const wasEditing = editingConnectionId !== null;
    setBusy(true);
    setError('');
    setNotice('');
    const settings: ConnectionConfiguration = {};
    for (const field of visibleFields) {
      if (wasEditing && !field.editable) continue;
      if (!wasEditing && field.name === 'auth_type') continue;
      if (wasEditing && field.secret && !replacementEnabled(field)) continue;
      const value = configuration[field.name];
      const normalized = typeof value === 'string' && !field.secret ? value.trim() : value;
      if (normalized === '' && !field.required && !field.secret) {
        if (wasEditing) settings[field.name] = null;
      } else {
        settings[field.name] = normalized ?? '';
      }
    }

    try {
      if (editingConnectionId) {
        await api.updateConnection(editingConnectionId, {
          ...settings,
          name: name.trim(),
        }, editingRevision);
      } else {
        await api.addConnection({ ...settings, name: name.trim(), connection_type: integration.id });
      }
      await onConnectionsChange();
      setEditingConnectionId(null);
      setName('');
      setConfiguration({});
      setIntegration(null);
      setStage('list');
      setNotice(wasEditing ? 'Connection updated and tested.' : 'Connection tested and added.');
    } catch (reason) {
      setError(errorMessage(reason, 'Could not add connection'));
    } finally {
      setBusy(false);
    }
  }

  function editConnection(connection: ConnectionSummary) {
    setError('');
    setNotice('');
    setEditingConnectionId(connection.id);
    setEditingRevision(connection.revision);
    setIntegration(connection.integration);
    setName(connection.name);
    const initial = initialConfiguration(connection.integration, connection.configuration);
    setConfiguration(initial);
    setSavedConfiguration(initial);
    setReplacements({});
    setStage('configure');
  }

  function chooseIntegration(descriptor: IntegrationDescriptor) {
    setIntegration(descriptor);
    setEditingConnectionId(null);
    setName('');
    setConfiguration(initialConfiguration(descriptor));
    setSavedConfiguration({});
    setReplacements({});
    setEditingRevision('');
    setStage('configure');
  }

  const visibleFields = integration?.connection_fields.filter((field) =>
    (!editingConnectionId || field.editable || (field.group === 'authentication' && !field.secret)) &&
    (!field.visible_when || configuration[field.visible_when.field] === field.visible_when.value),
  ) ?? [];

  function replacementRequired(field: ConnectionField) {
    return Boolean(editingConnectionId && field.secret && field.required && field.visible_when &&
      configuration[field.visible_when.field] !== savedConfiguration[field.visible_when.field]);
  }

  function replacementEnabled(field: ConnectionField) {
    return replacements[field.name] === true || replacementRequired(field);
  }

  function renderField(field: ConnectionField) {
    const updateValue = (value: string | boolean) => setConfiguration((current) => ({ ...current, [field.name]: value }));
    const enabled = !editingConnectionId || (field.editable && (!field.secret || replacementEnabled(field)));
    const fullWidth = field.group !== 'authentication' && (field.input_type === 'checkbox' || (field.input_type === 'select' && field.group === 'credentials'));
    if (field.input_type === 'checkbox') return <label className={`checkbox-option connection-checkbox${fullWidth ? ' form-field-full' : ''}`} key={field.name}><input type="checkbox" disabled={!enabled} required={enabled && field.required} checked={configuration[field.name] === true} onChange={(event) => updateValue(event.target.checked)} /> {field.label}</label>;
    return <div className={fullWidth ? 'connection-field form-field-full' : 'connection-field'} key={field.name}>
      {editingConnectionId && field.secret && <label className="checkbox-option"><input type="checkbox" checked={enabled} disabled={replacementRequired(field)} onChange={(event) => setReplacements((current) => ({ ...current, [field.name]: event.target.checked }))} />Replace {field.label}</label>}
      <label>{field.label}{field.input_type === 'select'
        ? <select disabled={!enabled} required={enabled && field.required} value={String(configuration[field.name] ?? '')} onChange={(event) => updateValue(event.target.value)}>{field.options?.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}</select>
        : <input disabled={!enabled} required={enabled && field.required} type={field.input_type} value={String(configuration[field.name] ?? '')} placeholder={field.placeholder} autoComplete={field.secret ? 'new-password' : 'off'} onChange={(event) => updateValue(event.target.value)} />}</label>
    </div>;
  }

  const authenticationFields = visibleFields.filter((field) => field.group === 'authentication' && !field.secret);
  const credentialFields = visibleFields.filter((field) => field.group === 'credentials' || field.secret);
  const mainFields = visibleFields.filter((field) => field.group !== 'authentication' && field.group !== 'credentials' && !field.secret);

  async function removeConnection(connection: ConnectionSummary) {
    setError('');
    setNotice('');
    try {
      await api.removeConnection(connection.id, connection.revision);
      await onConnectionsChange();
      setNotice(`${connection.name} removed.`);
    } catch (reason) {
      setError(errorMessage(reason, 'Could not remove connection'));
    }
  }

  return <main className="connections-layout">
    <div className="connections-heading">
      {stage === 'list' && <button className="search-button add-connection-button" onClick={() => { setError(''); setNotice(''); setStage('choose-source'); }}><span aria-hidden="true">＋</span> Add connection</button>}
    </div>

    {error && <ErrorAlert message={error} onDismiss={() => setError('')} />}
    {notice && <StatusNotice message={notice} />}

    {stage === 'list' && <section className="connections-section" aria-labelledby="configured-connections-title">
      <div className="section-heading"><div><h2 id="configured-connections-title">Saved connections</h2><p>Endpoints are tested before they are added.</p></div><span className="field-count">{connections.length}</span></div>
      {connections.length === 0
        ? <p className="connections-empty">No connections available</p>
        : <div className="connection-list">{connections.map((connection) => <article className="connection-row" key={connection.id}>
          <span className={`source-mark ${connection.integration.id}`} aria-hidden="true">{connection.integration.display_name[0]}</span>
          <div className="connection-info"><strong>{connection.name}</strong><span>{connection.url}{connection.database ? ` · ${connection.database}` : ''}</span></div>
          <span className="connection-kind">{connection.integration.display_name} · {connection.integration.query_language}</span>
          <ConnectionStatus id={connection.id} revision={connection.revision} />
          <div className="connection-actions"><button className="edit-connection" aria-label={`Edit ${connection.name}`} title={`Edit ${connection.name}`} onClick={() => editConnection(connection)}>✎</button><button className="remove-connection" aria-label={`Remove ${connection.name}`} title={`Remove ${connection.name}`} onClick={() => void removeConnection(connection)}>×</button></div>
        </article>)}</div>}
    </section>}

    {stage === 'choose-source' && <section className="connections-section source-choice-section" aria-labelledby="choose-source-title">
      <div className="section-heading"><div><h2 id="choose-source-title">Choose a data source</h2><p>Select the type of connection to configure.</p></div><button className="quiet-button" onClick={() => { setError(''); setStage('list'); }}>Cancel</button></div>
      {integrationStatus === 'loading' ? <p className="connections-empty" role="status">Loading data sources...</p>
        : integrationStatus === 'failed' ? <div className="alert" role="alert"><span className="alert-symbol">!</span><span>{integrationError}</span><button className="icon-button" aria-label="Retry data sources" title="Retry data sources" onClick={() => setIntegrationAttempt((attempt) => attempt + 1)}>↻</button></div>
        : integrations.length === 0
        ? <p className="connections-empty">No integrations are registered in the backend.</p>
        : <div className="source-choice-list">{integrations.map((integration) => <button className="source-choice" key={integration.id} onClick={() => chooseIntegration(integration)}>
          <span className={`source-mark ${integration.id}`} aria-hidden="true">{integration.display_name[0]}</span><span><strong>{integration.display_name}</strong><small>{integration.query_language} · {integration.description}</small></span><span className="source-choice-arrow" aria-hidden="true">›</span>
        </button>)}</div>}
    </section>}

    {stage === 'configure' && integration && <section className="connections-section add-connection-section" aria-labelledby="add-connection-title">
      <div className="section-heading"><div><h2 id="add-connection-title">{editingConnectionId ? 'Edit' : 'Configure'} {integration.display_name}</h2><p>Credentials remain on the server.</p></div><button className="quiet-button" disabled={busy} onClick={() => { setEditingConnectionId(null); setStage(editingConnectionId ? 'list' : 'choose-source'); }}>Back</button></div>
      <form className="connection-form" onSubmit={(event) => void addConnection(event)}>

        <fieldset className="connection-field-group" aria-label="Connection details"><div className="form-grid">
          <label>Connection name<input required maxLength={64} value={name} onChange={(event) => setName(event.target.value)} placeholder="Production analytics" /></label>
          {mainFields.map(renderField)}
        </div></fieldset>
        {authenticationFields.length > 0 && <fieldset className="connection-field-group" aria-label="Authentication options"><div className="form-grid">
          {authenticationFields.map(renderField)}
        </div></fieldset>}
        {credentialFields.length > 0 && <fieldset className="connection-field-group" aria-label="Credentials"><div className="form-grid">
          {credentialFields.map(renderField)}
        </div></fieldset>}

        <div className="connection-form-footer"><span>InvestiGenie connections</span><button className="search-button" type="submit" disabled={busy}>{busy ? <span className="spinner" /> : <span aria-hidden="true">⌁</span>}{busy ? 'Testing connection' : editingConnectionId ? 'Save changes' : 'Test and add'}</button></div>
      </form>
    </section>}
  </main>;
}

function initialConfiguration(integration: IntegrationDescriptor, saved: ConnectionConfiguration = {}): ConnectionConfiguration {
  return Object.fromEntries(integration.connection_fields.map((field) => [field.name,
    !field.secret && saved[field.name] != null && saved[field.name] !== '' ? saved[field.name]
      : field.input_type === 'checkbox' ? false
      : field.input_type === 'select' ? field.options?.[0]?.value ?? '' : '',
  ]));
}
