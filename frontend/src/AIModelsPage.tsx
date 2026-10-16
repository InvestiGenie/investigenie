import { useEffect, useState, type FormEvent } from 'react';
import { api, type AIModelRequest, type AIModelSummary } from './api';
import { errorMessage } from './format';
import { ErrorAlert, StatusNotice } from './PageFeedback';

export default function AIModelsPage() {
  const [models, setModels] = useState<AIModelSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState('');
  const [editingModel, setEditingModel] = useState<AIModelSummary | null>(null);
  const [formOpen, setFormOpen] = useState(false);
  const [name, setName] = useState('');
  const [apiUrl, setApiUrl] = useState('');
  const [modelName, setModelName] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [replaceKey, setReplaceKey] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [refreshAttempt, setRefreshAttempt] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setLoadError('');
    api.aiModels(controller.signal)
      .then((available) => {
        if (controller.signal.aborted) return;
        setModels(available);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setLoadError(errorMessage(reason));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [refreshAttempt]);

  function resetForm() {
    setEditingModel(null);
    setFormOpen(false);
    setName('');
    setApiUrl('');
    setModelName('');
    setApiKey('');
    setReplaceKey(false);
  }

  function configure(model?: AIModelSummary) {
    setError('');
    setNotice('');
    setEditingModel(model ?? null);
    setFormOpen(true);
    setName(model?.name ?? '');
    setApiUrl(model?.api_url ?? '');
    setModelName(model?.model ?? '');
    setApiKey('');
    setReplaceKey(false);
  }

  async function saveModel(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (busy) return;
    const body: AIModelRequest = { name: name.trim(), api_url: apiUrl.trim(), model: modelName.trim() };
    if (!editingModel || replaceKey) body.api_key = apiKey;
    setBusy(true);
    setError('');
    setNotice('');
    try {
      if (editingModel) await api.updateAIModel(editingModel.id, body, editingModel.revision);
      else await api.addAIModel(body);
      const updated = await api.aiModels();
      setModels(updated);
      resetForm();
      setNotice(editingModel ? 'AI model connection tested and updated.' : 'AI model connection tested and saved.');
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  async function removeModel(model: AIModelSummary) {
    setError('');
    setNotice('');
    try {
      await api.removeAIModel(model.id, model.revision);
      const updated = await api.aiModels();
      setModels(updated);
      setNotice(`${model.name} removed.`);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  return <main className="connections-layout ai-models-layout">
    <div className="connections-heading">
      {!formOpen && models.length > 0 && <button className="search-button add-connection-button" onClick={() => configure()}><span aria-hidden="true">＋</span> Add AI model</button>}
    </div>
    {error && <ErrorAlert message={error} onDismiss={() => setError('')} />}
    {loadError && <div className="alert" role="alert"><span className="alert-symbol">!</span><span>{loadError}</span><button className="icon-button" aria-label="Retry loading AI models" title="Retry loading AI models" onClick={() => setRefreshAttempt((attempt) => attempt + 1)}>↻</button></div>}
    {notice && <StatusNotice message={notice} />}

    {!editingModel && <section className="connections-section" aria-labelledby="saved-ai-models-title">
      <div className="section-heading"><div><h2 id="saved-ai-models-title">Saved AI models</h2><p>Provider connections are tested by the server before they are saved.</p></div><span className="field-count">{models.length}</span></div>
      {loading ? <p className="connections-empty" role="status">Loading AI models...</p>
        : models.length === 0 && !formOpen ? <div className="connections-empty ai-models-empty"><h3>No AI models configured</h3><p>Add a provider endpoint and model to start investigations.</p><button className="search-button" onClick={() => configure()}>Add AI model</button></div>
        : <div className="connection-list">{models.map((model) => <article className="ai-model-row" key={model.id}>
          <span className="source-mark ai-model-mark" aria-hidden="true">AI</span>
          <div className="connection-info"><strong>{model.name}</strong><span>{model.api_url} · {model.model}</span></div>
          <span className="ai-model-revision" title={`Revision ${model.revision}`}>Revision {model.revision}</span>
          <span className="ai-model-key-status">API key saved</span>
          <div className="connection-actions"><button className="edit-connection" aria-label={`Edit ${model.name}`} title={`Edit ${model.name}`} onClick={() => configure(model)}>✎</button><button className="remove-connection" aria-label={`Remove ${model.name}`} title={`Remove ${model.name}`} onClick={() => void removeModel(model)}>×</button></div>
        </article>)}</div>}
    </section>}

    {formOpen && <section className="connections-section add-connection-section" aria-labelledby="ai-model-form-title">
      <div className="section-heading"><div><h2 id="ai-model-form-title">{editingModel ? 'Edit AI model' : 'Add AI model'}</h2><p>Credentials stay on the server; the provider is tested when you save.</p></div><button className="quiet-button" disabled={busy} onClick={() => { resetForm(); setError(''); }}>Cancel</button></div>
      <form className="connection-form" onSubmit={(event) => void saveModel(event)}>
        <div className="form-grid">
          <label className="form-field-full">Profile name<input required maxLength={64} value={name} onChange={(event) => setName(event.target.value)} placeholder="Production analysis" /></label>
          <label className="form-field-full">Provider API endpoint<input required type="url" value={apiUrl} onChange={(event) => setApiUrl(event.target.value)} placeholder="https://api.example.com/v1" autoComplete="url" /></label>
          <label>Model identifier<input required value={modelName} onChange={(event) => setModelName(event.target.value)} placeholder="provider-model-name" /></label>
          {editingModel && <label className="checkbox-option ai-model-replace-toggle"><input type="checkbox" checked={replaceKey} onChange={(event) => { setReplaceKey(event.target.checked); setApiKey(''); }} /> Replace API key</label>}
          {(!editingModel || replaceKey) && <label className="form-field-full">API key<input required={!editingModel || replaceKey} type="password" value={apiKey} onChange={(event) => setApiKey(event.target.value)} placeholder={editingModel ? 'Enter a new API key' : 'Provider API key'} autoComplete="new-password" /></label>}
          {editingModel && !replaceKey && <p className="ai-model-key-hint form-field-full">An API key is already saved. Leave it unchanged, or select “Replace API key”.</p>}
        </div>
        <div className="connection-form-footer"><span>Profile revision {editingModel?.revision ?? 'New'}</span><button className="search-button" type="submit" disabled={busy}>{busy ? <span className="spinner" /> : <span aria-hidden="true">⌁</span>}{busy ? 'Testing connection' : editingModel ? 'Test and save changes' : 'Test and save'}</button></div>
      </form>
    </section>}
  </main>;
}
