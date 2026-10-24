import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from 'react';
import { ArrowUp, BookOpen, BrainCircuit, Check, CircleAlert, CirclePause, Database, Play, Save, Search, Square, Trash2, X } from 'lucide-react';
import { api, type AIConversation, type AIConversationSummary, type AIModelSummary, type DataViewSummary, type InvestigationJob, type InvestigationResult, type KnowledgeInput, type SearchRequest } from './api';
import TimeRangePicker from './TimeRangePicker';
import { getPreference, setPreference, type PreferenceName } from './preferences';
import { errorMessage } from './format';

const TIMEOUT_OPTIONS = [15, 30, 60, 120, 300, 600];
const CHECK_IN_OPTIONS = [60, 120, 300, 600, 1800, 3600];
const ACTIVE_POLL_MS = 1_000;
const PAUSED_POLL_MS = 5_000;
const MAX_DOCUMENT_SOURCE_CHARS = 6_000;

export type DocumentInvestigationSeed = {
  requestId: string;
  dataViewId: string;
  dataViewName: string;
  index: string;
  documentId: string;
  source: Record<string, unknown> | null;
  timeRange: SearchRequest['time_range'] | null;
};

function documentInvestigationPrompt(seed: DocumentInvestigationSeed): string {
  const serializedSource = seed.source === null ? 'No document source was returned.' : JSON.stringify(seed.source, null, 2);
  const truncated = serializedSource.length > MAX_DOCUMENT_SOURCE_CHARS;
  const source = serializedSource.slice(0, MAX_DOCUMENT_SOURCE_CHARS);
  return [
    'Investigate this document for errors, anomalies, or signs of a production issue. Use the selected Data View to find relevant context and cite this document in your findings.',
    `Data View: ${seed.dataViewName}`,
    `Index or table: ${seed.index}`,
    `Document ID: ${seed.documentId}`,
    `Document source${truncated ? ' (truncated to fit the investigation prompt)' : ''}:`,
    source,
  ].join('\n\n');
}

function durationLabel(seconds: number): string {
  return seconds < 60 ? `${seconds} sec` : `${seconds / 60} min`;
}

function elapsedLabel(milliseconds: number): string {
  const seconds = Math.floor(milliseconds / 1000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

function storedSeconds(name: PreferenceName, options: number[], fallback: number): number {
  const saved = Number(getPreference(name));
  return options.includes(saved) ? saved : fallback;
}

const activeStatuses = new Set<InvestigationJob['status']>(['running', 'stopping', 'awaiting_decision']);

type KnowledgeDraftContext = { investigationId: string; question: string; modelId: string; dataViewIds: string[] };

function boundedUtf8(value: string, bytes: number): string {
  return new TextDecoder().decode(new TextEncoder().encode(value).subarray(0, bytes), { stream: true });
}

function investigationKnowledgeDraft(result: InvestigationResult, context: KnowledgeDraftContext): KnowledgeInput {
  const content = [
    '> Unverified investigation draft. Review the evidence, confirm the cause, and record the resolution before publishing.',
    '## Investigation reference',
    `Investigation ID: ${context.investigationId}\n\nAI model profile: ${context.modelId}`,
    '## Original question', context.question,
    '## Summary', result.summary,
    '## Findings',
    ...result.findings.flatMap((finding) => [
      `### ${finding.title}`, `Severity: ${finding.severity}; confidence: ${finding.confidence}.`, finding.detail,
      ...finding.evidence.map((evidence) => `- Data View: ${evidence.data_view_name} (${evidence.data_view_id}); observed count: ${evidence.count}; record IDs: ${evidence.record_ids.join(', ') || 'None supplied'}.`),
    ]),
    '## Sources and query scope',
    ...result.sources.map((source) => `### ${source.name}\n\nData View: ${source.data_view_id}\n\nIndex/table: ${source.index}\n\nRecords examined: ${source.records_examined}\n\n${source.query_summary}`),
    '## Caveats', ...(result.caveats.length ? result.caveats.map((caveat) => `- ${caveat}`) : ['No additional caveats recorded.']),
    '## Confirmed cause and resolution', 'Not confirmed. Add the verified cause, resolution, and recovery checks during review.',
  ].join('\n\n');
  const truncated = '\n\n> Content was truncated to the knowledge size limit. Review the original investigation for the full evidence and caveats.';
  const contentBytes = new TextEncoder().encode(content).length;
  const draft: KnowledgeInput = {
    title: boundedUtf8(context.question.trim() || result.summary.trim() || 'Investigation draft', 256),
    kind: 'resolved_incident', status: 'draft',
    content: contentBytes <= 32_768 ? content : boundedUtf8(content, 32_768 - new TextEncoder().encode(truncated).length) + truncated,
    services: [], environments: [], connection_ids: [], tags: ['investigation'], source_url: null,
    data_view_ids: [...new Set([...context.dataViewIds, ...result.sources.map((source) => source.data_view_id)])].slice(0, 20),
  };
  let contentLimit = 32_768;
  const suffixBytes = new TextEncoder().encode(truncated).length;
  while (new TextEncoder().encode(JSON.stringify(draft)).length > 65_536 && contentLimit > suffixBytes) {
    contentLimit = Math.floor(contentLimit / 2);
    draft.content = boundedUtf8(content, Math.max(0, contentLimit - suffixBytes)) + truncated;
  }
  if (new TextEncoder().encode(JSON.stringify(draft)).length > 65_536) throw new Error('The knowledge draft scope exceeds the supported size limit.');
  return draft;
}

function InvestigationResultView({ result, label, draftContext, onOpenKnowledge }: { result: InvestigationResult; label: string; draftContext?: KnowledgeDraftContext; onOpenKnowledge?: (id: string) => void }) {
  const [saving, setSaving] = useState(false);
  const [savedDraftId, setSavedDraftId] = useState<string | null>(null);
  const [saveError, setSaveError] = useState('');
  const saveInFlight = useRef(false);

  async function saveKnowledgeDraft() {
    if (!draftContext || result.complete === false || saveInFlight.current || savedDraftId) return;
    saveInFlight.current = true;
    setSaving(true);
    setSaveError('');
    try {
      const draft = await api.addKnowledge(investigationKnowledgeDraft(result, draftContext));
      setSavedDraftId(draft.id);
    } catch (reason) {
      setSaveError(errorMessage(reason, 'Could not save the knowledge draft.'));
    } finally {
      saveInFlight.current = false;
      setSaving(false);
    }
  }

  return <article className="ai-result">
    <div className="ai-result-label"><BrainCircuit size={15} aria-hidden="true" /> {label}</div>
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
    {draftContext && result.complete !== false && <div className="ai-knowledge-actions">
      <button type="button" className="quiet-button" disabled={saving || savedDraftId !== null} onClick={() => void saveKnowledgeDraft()}>{savedDraftId ? <Check size={15} aria-hidden="true" /> : <Save size={15} aria-hidden="true" />}{saving ? 'Saving knowledge draft...' : savedDraftId ? 'Knowledge draft saved' : 'Save as knowledge draft'}</button>
      {savedDraftId && <span role="status">Saved as draft.</span>}
      {savedDraftId && onOpenKnowledge && <button type="button" className="quiet-button" onClick={() => onOpenKnowledge(savedDraftId)}><BookOpen size={15} aria-hidden="true" /> Review knowledge draft</button>}
      {saveError && <div className="alert ai-error" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{saveError}</span></div>}
    </div>}
  </article>;
}

type InvestiGenieAIPageProps = {
  dataViews: DataViewSummary[];
  onManageModels?: () => void;
  initialDocument?: DocumentInvestigationSeed | null;
  onDocumentInvestigationConsumed?: (requestId: string) => void;
  onOpenKnowledge?: (id: string) => void;
};

export default function InvestiGenieAIPage({ dataViews, onManageModels, initialDocument, onDocumentInvestigationConsumed, onOpenKnowledge }: InvestiGenieAIPageProps) {
  const [models, setModels] = useState<AIModelSummary[]>([]);
  const [modelStatus, setModelStatus] = useState<'loading' | 'ready' | 'empty' | 'failed'>('loading');
  const [modelError, setModelError] = useState('');
  const [modelAttempt, setModelAttempt] = useState(0);
  const [selectedModelId, setSelectedModelId] = useState('');
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [dataViewFilter, setDataViewFilter] = useState('');
  const [dataViewPickerOpen, setDataViewPickerOpen] = useState(false);
  const [question, setQuestion] = useState('');
  const [job, setJob] = useState<InvestigationJob | null>(null);
  const [conversations, setConversations] = useState<AIConversationSummary[]>([]);
  const [activeConversation, setActiveConversation] = useState<AIConversation | null>(null);
  const [conversationLoading, setConversationLoading] = useState(false);
  const [conversationBusy, setConversationBusy] = useState(false);
  const [conversationError, setConversationError] = useState('');
  const [submittedQuestion, setSubmittedQuestion] = useState('');
  const [submittedScope, setSubmittedScope] = useState({ dataViewIds: [] as string[], modelId: '' });
  const [starting, setStarting] = useState(false);
  const [deciding, setDeciding] = useState(false);
  const [error, setError] = useState('');
  const [aiTimeout, setAiTimeout] = useState(() => storedSeconds('ai-timeout', TIMEOUT_OPTIONS, 60));
  const [checkIn, setCheckIn] = useState(() => storedSeconds('ai-check-in', CHECK_IN_OPTIONS, 120));
  const [initialTime] = useState(() => Date.now());
  const [startDate, setStartDate] = useState(() => new Date(initialTime - 30 * 60_000).toISOString().slice(0, 19));
  const [endDate, setEndDate] = useState(() => new Date(initialTime).toISOString().slice(0, 19));
  const [timeZone, setTimeZone] = useState('UTC');
  const pickerRef = useRef<HTMLDivElement>(null);
  const pickerSearchRef = useRef<HTMLInputElement>(null);
  const pickerTriggerRef = useRef<HTMLButtonElement>(null);
  const handledDocumentRequest = useRef<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    api.aiConversations(controller.signal)
      .then((items) => { if (!controller.signal.aborted) setConversations(items); })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setConversationError(errorMessage(reason, 'Could not load AI history'));
      });
    return () => controller.abort();
  }, []);

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
          const remembered = getPreference('ai-model');
          setSelectedModelId(available.some((model) => model.id === remembered) ? remembered! : available[0]?.id ?? '');
        } catch {
          setSelectedModelId(available[0]?.id ?? '');
        }
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        setModelError(errorMessage(reason, 'Could not load AI models'));
        setModelStatus('failed');
      });
    return () => controller.abort();
  }, [modelAttempt]);

  useEffect(() => {
    if (!initialDocument || handledDocumentRequest.current === initialDocument.requestId || modelStatus === 'loading') return;
    if (modelStatus === 'ready' && !selectedModelId) return;
    handledDocumentRequest.current = initialDocument.requestId;
    onDocumentInvestigationConsumed?.(initialDocument.requestId);

    const prompt = documentInvestigationPrompt(initialDocument);
    setActiveConversation(null);
    setJob(null);
    setSubmittedQuestion('');
    setQuestion(prompt);
    setSelectedIds([initialDocument.dataViewId]);
    setDataViewFilter('');
    setStartDate(initialDocument.timeRange ? new Date(initialDocument.timeRange.start).toISOString().slice(0, 19) : '');
    setEndDate(initialDocument.timeRange ? new Date(initialDocument.timeRange.end).toISOString().slice(0, 19) : '');
    setError('');

    if (!dataViews.some((view) => view.id === initialDocument.dataViewId)) {
      setError('The document’s Data View is no longer available.');
      return;
    }
    if (modelStatus !== 'ready') {
      setError(modelStatus === 'empty' ? 'Add an AI model before investigating this document.' : modelError || 'Could not load AI models.');
      return;
    }
    void startNewInvestigation(prompt, [initialDocument.dataViewId], selectedModelId, initialDocument.timeRange);
  }, [initialDocument, modelStatus, selectedModelId, modelError, dataViews, onDocumentInvestigationConsumed]);

  useEffect(() => {
    if (!dataViewPickerOpen) return;
    const closeOnOutsideClick = (event: PointerEvent) => {
      if (event.target instanceof Node && !pickerRef.current?.contains(event.target)) setDataViewPickerOpen(false);
    };
    document.addEventListener('pointerdown', closeOnOutsideClick);
    pickerSearchRef.current?.focus();
    return () => document.removeEventListener('pointerdown', closeOnOutsideClick);
  }, [dataViewPickerOpen]);

  useEffect(() => {
    if (!job || !activeStatuses.has(job.status)) return;
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      api.investigation(job.id, controller.signal)
        .then((next) => {
          if (controller.signal.aborted) return;
          setJob(next);
          if (next.status === 'failed') setError(next.error?.message ?? 'The investigation could not be completed.');
        })
        .catch((reason: unknown) => {
          if (controller.signal.aborted) return;
          setError(errorMessage(reason, 'Lost track of the investigation.'));
          setJob((current) => current && { ...current, status: 'failed' });
        });
    }, job.status === 'awaiting_decision' ? PAUSED_POLL_MS : ACTIVE_POLL_MS);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [job]);

  function selectModel(id: string) {
    setSelectedModelId(id);
    setPreference('ai-model', id);
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

  const busy = starting || (job !== null && activeStatuses.has(job.status));
  const result = job?.result ?? null;

  async function refreshConversations() {
    try {
      setConversations(await api.aiConversations());
      setConversationError('');
    } catch (reason) {
      setConversationError(errorMessage(reason, 'Could not refresh AI history'));
    }
  }

  async function startNewInvestigation(
    prompt: string,
    dataViewIds: string[],
    modelId: string,
    timeRange: SearchRequest['time_range'] | null,
  ) {
    setStarting(true);
    setError('');
    setJob(null);
    setSubmittedQuestion(prompt);
    setSubmittedScope({ dataViewIds: [...dataViewIds], modelId });
    try {
      const started = await api.startInvestigation(prompt, dataViewIds, modelId, timeRange, { ai_timeout_seconds: aiTimeout, time_budget_seconds: checkIn });
      setJob(started);
      setQuestion('');
      setActiveConversation(null);
      await refreshConversations();
    } catch (reason) {
      setError(errorMessage(reason, 'The investigation could not be started.'));
    } finally {
      setStarting(false);
    }
  }

  async function openConversation(id: string) {
    if (!id) {
      setActiveConversation(null);
      setJob(null);
      setSubmittedQuestion('');
      setQuestion('');
      return;
    }
    setConversationLoading(true);
    setConversationError('');
    setError('');
    try {
      const conversation = await api.aiConversation(id);
      setActiveConversation(conversation);
      setJob(null);
      setSubmittedQuestion('');
      setSelectedIds(conversation.data_view_ids);
      setSelectedModelId(conversation.model_id);
      setQuestion('');
    } catch (reason) {
      setConversationError(errorMessage(reason, 'Could not load this conversation'));
    } finally {
      setConversationLoading(false);
    }
  }

  async function removeConversation() {
    if (!activeConversation || busy || conversationBusy) return;
    setConversationBusy(true);
    setConversationError('');
    try {
      await api.removeAIConversation(activeConversation.id, activeConversation.revision);
      setConversations((current) => current.filter((item) => item.id !== activeConversation.id));
      setActiveConversation(null);
      setJob(null);
      setSubmittedQuestion('');
      setError('');
    } catch (reason) {
      setConversationError(errorMessage(reason, 'Could not delete this conversation'));
    } finally {
      setConversationBusy(false);
    }
  }

  async function investigate(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const prompt = question.trim();
    if (!prompt || busy || conversationBusy || (!activeConversation && (!selectedIds.length || !selectedModelId))) return;
    if (activeConversation) {
      setConversationBusy(true);
      setConversationError('');
      try {
        const updated = await api.continueAIConversation(activeConversation.id, prompt, activeConversation.revision);
        setActiveConversation(updated);
        setQuestion('');
        await refreshConversations();
      } catch (reason) {
        setConversationError(errorMessage(reason, 'Could not continue this conversation'));
      } finally {
        setConversationBusy(false);
      }
      return;
    }
    const timeRange = startDate && endDate ? { start: Date.parse(`${startDate}Z`), end: Date.parse(`${endDate}Z`) } : null;
    await startNewInvestigation(prompt, selectedIds, selectedModelId, timeRange);
  }

  async function decide(action: 'continue' | 'terminate') {
    if (!job || deciding) return;
    setDeciding(true);
    setError('');
    try {
      setJob(await api.decideInvestigation(job.id, action));
    } catch (reason) {
      setError(errorMessage(reason, 'The decision could not be sent.'));
      api.investigation(job.id).then(setJob).catch(() => undefined);
    } finally {
      setDeciding(false);
    }
  }

  useEffect(() => {
    if (!job || activeStatuses.has(job.status)) return;
    let active = true;
    api.aiConversation(job.id)
      .then((conversation) => {
        if (active) setActiveConversation(conversation);
        return refreshConversations();
      })
      .catch(() => undefined);
    return () => { active = false; };
  }, [job?.id, job?.status]);

  function updateTimeout(seconds: number) {
    setAiTimeout(seconds);
    setPreference('ai-timeout', String(seconds));
  }

  function updateCheckIn(seconds: number) {
    setCheckIn(seconds);
    setPreference('ai-check-in', String(seconds));
  }

  function handleComposerKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      event.currentTarget.form?.requestSubmit();
    }
  }

  return <main className="ai-page">
    <section className="ai-workspace" aria-label="InvestiGenie AI investigation">
      <div className="ai-history-row">
        <label htmlFor="ai-conversation-history">AI history</label>
        <select id="ai-conversation-history" aria-label="AI conversation history" value={activeConversation?.id ?? ''} disabled={conversationLoading || busy || conversationBusy} onChange={(event) => void openConversation(event.target.value)}>
          <option value="">New investigation</option>
          {conversations.map((conversation) => <option key={conversation.id} value={conversation.id}>{conversation.title}</option>)}
        </select>
        {activeConversation && <button type="button" className="quiet-button" aria-label="Delete conversation" title="Delete conversation" disabled={busy || conversationBusy} onClick={() => void removeConversation()}><Trash2 size={14} aria-hidden="true" /></button>}
        {conversationLoading && <span role="status">Loading history…</span>}
      </div>
      {conversationError && <div className="alert" role="alert"><span>{conversationError}</span><button type="button" aria-label="Dismiss history error" onClick={() => setConversationError('')}>×</button></div>}
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
        {!submittedQuestion && !activeConversation && !busy && <div className="ai-welcome"><h2>Start a new investigation</h2>{modelStatus === 'empty' && <button className="sample-query" onClick={() => onManageModels?.()}>Open AI model management</button>}</div>}
        {activeConversation
          ? activeConversation.messages.map((message, index) => message.role === 'user'
            ? <div className="ai-user-message" key={`${index}-user`}><span>You</span><p>{message.content}</p></div>
            : message.result
              ? <InvestigationResultView key={`${activeConversation.id}-${index}-result`} result={message.result} label={message.result.complete === false ? 'Partial investigation' : 'Investigation'} draftContext={{ investigationId: activeConversation.id, question: activeConversation.messages.find((item) => item.role === 'user')?.content ?? activeConversation.title, modelId: activeConversation.model_id, dataViewIds: activeConversation.data_view_ids }} onOpenKnowledge={onOpenKnowledge} />
              : <div className="ai-assistant-message" key={`${index}-assistant`}><span>InvestiGenie AI</span><p>{message.content}</p></div>)
          : submittedQuestion && <div className="ai-user-message"><span>You</span><p>{submittedQuestion}</p></div>}
        {starting && <div className="ai-thinking" role="status"><span className="spinner dark" /> Starting investigation…</div>}
        {job && (job.status === 'running' || job.status === 'stopping') && <div className="ai-progress" role="status">
          <span className="spinner dark" />
          <div className="ai-progress-text">
            <strong>{job.status === 'stopping' ? 'Stopping and keeping findings so far…' : job.progress.stage || 'Investigation in progress…'}</strong>
            <small>{job.progress.records_examined.toLocaleString()} records · {job.progress.batches_processed.toLocaleString()} batches · {job.progress.analysis_cycles.toLocaleString()} analysis cycles · {elapsedLabel(job.progress.elapsed_ms)} elapsed</small>
          </div>
          {job.status === 'running' && <button type="button" className="quiet-button ai-stop" disabled={deciding} onClick={() => void decide('terminate')}><Square size={12} aria-hidden="true" /> Stop</button>}
        </div>}
        {job?.status === 'awaiting_decision' && <section className="ai-decision" role="group" aria-label="Continue or stop the investigation">
          <div className="ai-decision-heading"><CirclePause size={16} aria-hidden="true" /><strong>Should InvestiGenie keep investigating?</strong></div>
          <p>{job.pause_reason}</p>
          <small>{job.progress.records_examined.toLocaleString()} records examined in {job.progress.batches_processed.toLocaleString()} batches · {elapsedLabel(job.progress.elapsed_ms)} elapsed</small>
          <div className="ai-decision-actions">
            <button type="button" className="search-button" disabled={deciding} onClick={() => void decide('continue')}><Play size={13} aria-hidden="true" /> Continue investigation</button>
            <button type="button" className="quiet-button" disabled={deciding} onClick={() => void decide('terminate')}><Square size={12} aria-hidden="true" /> Stop and show findings</button>
          </div>
        </section>}
        {error && <div className="alert ai-error" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{error}</span></div>}
        {job?.status === 'terminated' && !result && <p className="ai-empty-sources">The investigation was stopped before any findings were produced.</p>}
        {result && !activeConversation && <InvestigationResultView key={job?.id} result={result} label={job?.status === 'completed' ? 'Investigation' : job && activeStatuses.has(job.status) ? 'Findings so far' : 'Partial investigation'} draftContext={job?.status === 'completed' ? { investigationId: job.id, question: submittedQuestion, ...submittedScope } : undefined} onOpenKnowledge={onOpenKnowledge} />}
      </section>

      <form className="ai-composer" onSubmit={(event) => void investigate(event)}>
        <label className="sr-only" htmlFor="ai-question">Ask InvestiGenie AI</label>
        <textarea id="ai-question" value={question} maxLength={8000} rows={3} placeholder={activeConversation ? 'Ask a follow-up about this investigation…' : 'Describe what you’re investigating…'} onChange={(event) => setQuestion(event.target.value)} onKeyDown={handleComposerKeyDown} disabled={busy || conversationBusy || (!activeConversation && (dataViews.length === 0 || !selectedModelId))} />
        <div className="ai-composer-footer">
          <div className="ai-model-picker">
            {modelStatus === 'loading' && <span role="status">Loading AI models...</span>}
            {modelStatus === 'failed' && <div className="alert ai-error" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{modelError}</span><button type="button" className="icon-button" aria-label="Retry loading AI models" title="Retry loading AI models" onClick={() => setModelAttempt((attempt) => attempt + 1)}>↻</button></div>}
            {modelStatus === 'empty' && <><span>No saved AI models</span><button type="button" className="quiet-button" onClick={() => onManageModels?.()}>Manage AI models</button></>}
            {modelStatus === 'ready' && <label className="ai-model-compact" htmlFor="ai-model-select"><span className="sr-only">Model</span><select id="ai-model-select" aria-label="AI model" value={selectedModelId} disabled={busy} onChange={(event) => selectModel(event.target.value)}>{models.map((model) => <option key={model.id} value={model.id}>{model.name} · {model.model}</option>)}</select></label>}
            <label className="ai-run-setting" title="How long to wait for each AI provider response"><span>Timeout</span><select aria-label="AI response timeout" value={aiTimeout} disabled={busy} onChange={(event) => updateTimeout(Number(event.target.value))}>{TIMEOUT_OPTIONS.map((seconds) => <option key={seconds} value={seconds}>{durationLabel(seconds)}</option>)}</select></label>
            <label className="ai-run-setting" title="Ask whether to continue after the investigation runs this long"><span>Ask after</span><select aria-label="Ask to continue after" value={checkIn} disabled={busy} onChange={(event) => updateCheckIn(Number(event.target.value))}>{CHECK_IN_OPTIONS.map((seconds) => <option key={seconds} value={seconds}>{durationLabel(seconds)}</option>)}</select></label>
          </div>
          <button className="ai-send" type="submit" aria-label={activeConversation ? 'Send follow-up' : 'Run investigation'} title={activeConversation ? 'Send follow-up' : 'Run investigation'} disabled={busy || conversationBusy || !question.trim() || (!activeConversation && (selectedIds.length === 0 || !selectedModelId))}>{conversationBusy ? <span className="spinner" /> : <ArrowUp size={17} aria-hidden="true" />}</button>
        </div>
      </form>
      <p className="ai-privacy-note">Connection credentials remain in InvestiGenie. Selected evidence is sent to the configured AI provider for analysis.</p>
    </section>
  </main>;
}
