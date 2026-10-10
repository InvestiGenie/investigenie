import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from 'react';
import { ArrowUp, BrainCircuit, Check, CircleAlert, Database, Search, X } from 'lucide-react';
import { api, type AIModelSummary, type DataViewSummary, type InvestigationResult } from './api';
import TimeRangePicker from './TimeRangePicker';

type NorthstarAIPageProps = {
  dataViews: DataViewSummary[];
  onManageModels?: () => void;
};

export default function NorthstarAIPage({ dataViews, onManageModels }: NorthstarAIPageProps) {
  const [models, setModels] = useState<AIModelSummary[]>([]);
  const [modelStatus, setModelStatus] = useState<'loading' | 'ready' | 'empty' | 'failed'>('loading');
  const [modelError, setModelError] = useState('');
  const [modelAttempt, setModelAttempt] = useState(0);
  const [selectedModelId, setSelectedModelId] = useState('');
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [dataViewFilter, setDataViewFilter] = useState('');
  const [dataViewPickerOpen, setDataViewPickerOpen] = useState(false);
  const [question, setQuestion] = useState('');
  const [result, setResult] = useState<InvestigationResult | null>(null);
  const [submittedQuestion, setSubmittedQuestion] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [initialTime] = useState(() => Date.now());
  const [startDate, setStartDate] = useState(() => new Date(initialTime - 30 * 60_000).toISOString().slice(0, 19));
  const [endDate, setEndDate] = useState(() => new Date(initialTime).toISOString().slice(0, 19));
  const [timeZone, setTimeZone] = useState('UTC');
  const pickerRef = useRef<HTMLDivElement>(null);
  const pickerSearchRef = useRef<HTMLInputElement>(null);
  const pickerTriggerRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const controller = new AbortController();
    setModelStatus('loading');
    setModelError('');
    api.aiModels(controller.signal)
      .then((available) => {
        if (controller.signal.aborted) return;
        setModels(available);
        setModelStatus(available.length ? 'ready' : 'empty');
        try {
          const remembered = window.localStorage.getItem('northstar-ai-model');
          setSelectedModelId(available.some((model) => model.id === remembered) ? remembered! : available[0]?.id ?? '');
        } catch {
          setSelectedModelId(available[0]?.id ?? '');
        }
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        setModelError(reason instanceof Error ? reason.message : 'Could not load AI models');
        setModelStatus('failed');
      });
    return () => controller.abort();
  }, [modelAttempt]);

  useEffect(() => {
    if (!dataViewPickerOpen) return;
    const closeOnOutsideClick = (event: PointerEvent) => {
      if (event.target instanceof Node && !pickerRef.current?.contains(event.target)) setDataViewPickerOpen(false);
    };
    document.addEventListener('pointerdown', closeOnOutsideClick);
    pickerSearchRef.current?.focus();
    return () => document.removeEventListener('pointerdown', closeOnOutsideClick);
  }, [dataViewPickerOpen]);

  function selectModel(id: string) {
    setSelectedModelId(id);
    try { window.localStorage.setItem('northstar-ai-model', id); } catch {}
  }

  function toggleDataView(id: string) {
    setSelectedIds((current) => current.includes(id)
      ? current.filter((selected) => selected !== id)
      : current.length < 5 ? [...current, id] : current);
  }

  const normalizedViewFilter = dataViewFilter.trim().toLocaleLowerCase();
  const filteredDataViews = dataViews.filter((view) =>
    !normalizedViewFilter || `${view.name} ${view.index} ${view.time_field}`.toLocaleLowerCase().includes(normalizedViewFilter),
  );
  const visibleDataViews = filteredDataViews.slice(0, 20);
  const selectedDataViews = selectedIds.flatMap((id) => {
    const view = dataViews.find((item) => item.id === id);
    return view ? [view] : [];
  });

  async function investigate(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const prompt = question.trim();
    if (!prompt || !selectedIds.length || !selectedModelId || busy) return;
    setBusy(true);
    setError('');
    setResult(null);
    setSubmittedQuestion(prompt);
    try {
      const timeRange = startDate && endDate ? { start: Date.parse(`${startDate}Z`), end: Date.parse(`${endDate}Z`) } : null;
      const investigation = await api.investigate(prompt, selectedIds, selectedModelId, timeRange);
      setResult(investigation);
      setQuestion('');
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : 'The investigation could not be completed.');
    } finally {
      setBusy(false);
    }
  }

  function handleComposerKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      event.currentTarget.form?.requestSubmit();
    }
  }

  return <main className="ai-page">
    <section className="ai-workspace" aria-label="Northstar AI investigation">
      <div className="ai-controls-row">
      <div className="ai-dataview-picker" ref={pickerRef}>
        <button ref={pickerTriggerRef} type="button" className={`ai-dataview-trigger${dataViewPickerOpen ? ' open' : ''}`} aria-label="Choose Data Views" aria-expanded={dataViewPickerOpen} aria-controls="ai-dataview-menu" disabled={dataViews.length === 0} onClick={() => setDataViewPickerOpen((open) => !open)}>
          <Database size={14} aria-hidden="true"/><span>Data Views</span><span className="ai-dataview-count">{selectedIds.length}/5</span><span className="ai-dataview-chevron" aria-hidden="true">▾</span>
        </button>
        {dataViewPickerOpen && <div className="ai-dataview-menu" id="ai-dataview-menu" role="dialog" aria-label="Select Data Views">
          <label className="ai-view-search"><Search size={14} aria-hidden="true"/><span className="sr-only">Search Data Views</span><input ref={pickerSearchRef} aria-label="Search Data Views" value={dataViewFilter} onChange={(event) => setDataViewFilter(event.target.value)} placeholder="Search by name, index, or time field" onKeyDown={(event) => { if (event.key === 'Escape') { setDataViewPickerOpen(false); pickerTriggerRef.current?.focus(); } }} /></label>
          <div className="ai-view-result-count">{filteredDataViews.length} matches{filteredDataViews.length > visibleDataViews.length ? ` · showing first ${visibleDataViews.length}` : ''}</div>
          <div className="ai-source-list">{visibleDataViews.map((view) => {
            const checked = selectedIds.includes(view.id);
            return <label className={`ai-source${checked ? ' selected' : ''}`} key={view.id}>
              <input type="checkbox" checked={checked} disabled={!checked && selectedIds.length >= 5} onChange={() => toggleDataView(view.id)} />
              <span className="ai-source-check" aria-hidden="true">{checked && <Check size={12} />}</span>
              <span className="ai-source-name"><strong>{view.name}</strong><small>{view.index} · {view.time_field}</small></span>
            </label>;
          })}{visibleDataViews.length === 0 && <p className="ai-empty-sources">No Data Views match that search.</p>}</div>
        </div>}
      </div>
      <TimeRangePicker start={startDate} end={endDate} timeZone={timeZone} onTimeZoneChange={setTimeZone} onChange={(start, end) => { setStartDate(start); setEndDate(end); }} />
      </div>
      {selectedDataViews.length > 0 && <div className="ai-selected-views" aria-label="Selected Data Views">{selectedDataViews.map((view) => <span className="ai-selected-view" key={view.id}>{view.name}<button type="button" aria-label={`Remove ${view.name}`} title={`Remove ${view.name}`} onClick={() => toggleDataView(view.id)}><X size={12} /></button></span>)}</div>}
      {dataViews.length === 0 && <p className="ai-empty-sources">Create a Data View before starting an investigation.</p>}

      <section className="ai-transcript" aria-live="polite" aria-label="Investigation result">
        {!submittedQuestion && !busy && <div className="ai-welcome"><h2>Start a new investigation</h2>{modelStatus === 'empty' && <button className="sample-query" onClick={() => onManageModels?.()}>Open AI model management</button>}</div>}
        {submittedQuestion && <div className="ai-user-message"><span>You</span><p>{submittedQuestion}</p></div>}
        {busy && <div className="ai-thinking" role="status"><span className="spinner dark" /> Investigation in progress…</div>}
        {error && <div className="alert ai-error" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{error}</span></div>}
        {result && <article className="ai-result">
          <div className="ai-result-label"><BrainCircuit size={15} aria-hidden="true" /> Investigation</div>
          <p className="ai-summary">{result.summary}</p>
          <section className="ai-token-usage" aria-label="Token usage">
            <strong>Provider token usage</strong>
            {result.token_usage.some((call) => call.total_tokens !== null)
              ? <div>{result.token_usage.map((call) => <span key={call.stage}>{call.stage}: {call.total_tokens === null ? 'not reported' : `${call.total_tokens.toLocaleString()} total (${call.prompt_tokens?.toLocaleString() ?? '—'} prompt + ${call.completion_tokens?.toLocaleString() ?? '—'} completion)`}</span>)}</div>
              : <span>Provider did not report token usage.</span>}
          </section>
          {result.findings.length > 0 && <section className="ai-findings" aria-label="Findings">
            <h3>Findings</h3>
            {result.findings.map((finding, index) => <article className="ai-finding" key={`${finding.title}-${index}`}>
              <div className="ai-finding-heading"><h4>{finding.title}</h4><span className={`ai-severity severity-${finding.severity}`}>{finding.severity}</span></div>
              <p>{finding.detail}</p>
              <div className="ai-finding-meta"><span>{finding.confidence} confidence</span>{finding.evidence.map((evidence) => <span key={`${evidence.data_view_id}-${evidence.record_ids.join('-')}`}>{evidence.data_view_name}: {evidence.count.toLocaleString()} records{evidence.record_ids.length ? ` · ${evidence.record_ids.join(', ')}` : ''}</span>)}</div>
            </article>)}
          </section>}
          <details className="ai-sources-used"><summary>Sources and query scope</summary>
            {result.sources.map((source) => <div className="ai-source-used" key={source.data_view_id}><strong>{source.name}</strong><span>{source.index} · {source.records_examined.toLocaleString()} records examined{source.batches_processed !== undefined ? ` · ${source.batches_processed.toLocaleString()} batches` : ''}</span><small>{source.query_summary}</small></div>)}
          </details>
          {result.caveats.length > 0 && <div className="ai-caveats"><strong>Limits</strong>{result.caveats.map((caveat, index) => <p key={index}>{caveat}</p>)}</div>}
        </article>}
      </section>

      <form className="ai-composer" onSubmit={(event) => void investigate(event)}>
        <label className="sr-only" htmlFor="ai-question">Ask Northstar AI</label>
        <textarea id="ai-question" value={question} maxLength={2000} rows={3} placeholder="Describe what you’re investigating…" onChange={(event) => setQuestion(event.target.value)} onKeyDown={handleComposerKeyDown} disabled={busy || dataViews.length === 0 || !selectedModelId} />
        <div className="ai-composer-footer">
          <div className="ai-model-picker">
            {modelStatus === 'loading' && <span role="status">Loading AI models...</span>}
            {modelStatus === 'failed' && <div className="alert ai-error" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{modelError}</span><button type="button" className="icon-button" aria-label="Retry loading AI models" title="Retry loading AI models" onClick={() => setModelAttempt((attempt) => attempt + 1)}>↻</button></div>}
            {modelStatus === 'empty' && <><span>No saved AI models</span><button type="button" className="quiet-button" onClick={() => onManageModels?.()}>Manage AI models</button></>}
            {modelStatus === 'ready' && <label className="ai-model-compact" htmlFor="ai-model-select"><span className="sr-only">Model</span><select id="ai-model-select" aria-label="AI model" value={selectedModelId} disabled={busy} onChange={(event) => selectModel(event.target.value)}>{models.map((model) => <option key={model.id} value={model.id}>{model.name} · {model.model}</option>)}</select></label>}
          </div>
          <button className="ai-send" type="submit" aria-label="Run investigation" title="Run investigation" disabled={busy || !question.trim() || selectedIds.length === 0 || !selectedModelId}><ArrowUp size={17} aria-hidden="true" /></button>
        </div>
      </form>
      <p className="ai-privacy-note">Connection credentials remain in Northstar. Selected evidence is sent to the configured AI provider for analysis.</p>
    </section>
  </main>;
}
