import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from 'react';
import { ArrowUp, BookOpen, BrainCircuit, Check, CircleAlert, CirclePause, Database, Play, Save, Search, Square, Trash2, X } from 'lucide-react';
import { api, type AIConversation, type AIConversationSummary, type AIModelSummary, type ApplicationSettings, type DataViewSummary, type InvestigationJob, type InvestigationResult, type KnowledgeInput, type SearchRequest } from './api';
import TimeRangePicker from './TimeRangePicker';
import { getPreference, setPreference, type PreferenceName } from './preferences';
import { errorMessage } from './format';

const TIMEOUT_OPTIONS = [15, 30, 60, 120, 300, 600];
const CHECK_IN_OPTIONS = [60, 120, 300, 600, 1800, 3600];
const ACTIVE_POLL_MS = 1_000;
const PAUSED_POLL_MS = 5_000;
const MAX_DOCUMENT_SOURCE_CHARS = 6_000;
const DEFAULT_HISTORY_RETENTION_DAYS = 30;

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

function boundedUtf8(value: string, bytes: number): string {
  return new TextDecoder().decode(new TextEncoder().encode(value).subarray(0, bytes), { stream: true });
}

function investigationConversationKnowledgeDraft(conversation: AIConversation): KnowledgeInput {
  const firstQuestion = conversation.messages.find((message) => message.role === 'user')?.content ?? conversation.title;
  const messages = conversation.messages.map((message, index) => {
    if (message.role === 'user') return `### User turn ${index + 1}\n${message.content}`;
    if (!message.result) return `### Assistant reply ${index + 1}\n${message.content}`;
    const result = message.result;
    const findings = result.findings.map((finding) => [
      `#### ${finding.title}`,
      `Severity: ${finding.severity}; confidence: ${finding.confidence}.`,
      finding.detail,
      ...finding.evidence.map((evidence) => `- ${evidence.data_view_name} (${evidence.data_view_id}); ${new Set(evidence.record_ids).size} cited sample records: ${evidence.record_ids.join(', ') || 'none'}.`),
    ].join('\n'));
    const sources = result.sources.map((source) => [
      `#### ${source.name} (${source.data_view_id})`,
      `Index/table: ${source.index}; ${source.records_examined} sampled records; ${source.sample_records_available ?? 'unknown'} supplied to model.`,
      source.query_summary,
      ...(source.evidence_queries ?? []).map((query) => `${query.sampling}: ${query.records_returned} rows; ${query.query_summary}${query.reason ? `; reason: ${query.reason}` : ''}`),
    ].join('\n'));
    const playbooks = (result.playbook_evidence ?? []).map((evidence) => [
      `#### Playbook: ${evidence.playbook_title} / ${evidence.step_name} / ${evidence.data_view_name}`,
      `Query: ${evidence.query || '(all records)'}; breakdown: ${evidence.breakdown_field ?? 'none'}`,
      `Current: ${JSON.stringify(evidence.current)}; baseline: ${JSON.stringify(evidence.baseline)}; error: ${evidence.error ?? 'none'}`,
    ].join('\n'));
    const dataQuery = result.query_result ? [
      `#### Executed grouped query: ${result.query_result.data_view_name}`,
      `Group by: ${result.query_result.group_by.join(', ')}`,
      `Rows returned: ${result.query_result.row_count}; truncated: ${result.query_result.truncated}`,
      '```sql', result.query_result.sql, '```',
      JSON.stringify(result.query_result.rows),
    ].join('\n') : '';
    return [
      `### Investigation result ${index + 1} (${result.complete === false ? 'partial' : 'complete'})`,
      `Summary: ${result.summary}`,
      'Findings:', ...(findings.length ? findings : ['No supported findings.']),
      'Sources and query scope:', ...(sources.length ? sources : ['No source details recorded.']),
      ...(dataQuery ? ['Executed data query:', dataQuery] : []),
      ...(playbooks.length ? ['Playbook evidence:', ...playbooks] : []),
      'Caveats:', ...(result.caveats.length ? result.caveats.map((caveat) => `- ${caveat}`) : ['- None recorded.']),
    ].join('\n\n');
  });
  const content = [
    '> Unverified multi-turn investigation draft. Review all turns and evidence, confirm the cause, and record the resolution before publishing.',
    '## Investigation reference',
    `Investigation ID: ${conversation.id}\n\nAI model profile: ${conversation.model_id}`,
    '## Original question', firstQuestion,
    '## Full conversation and investigation evidence', ...messages,
    '## Confirmed cause and resolution', 'Not confirmed. Add the verified cause, resolution, and recovery checks during review.',
  ].join('\n\n');
  const truncated = '\n\n> Content was truncated to the Knowledge size limit. Review AI history for the complete conversation.';
  const dataViewIds = [...new Set([
    ...conversation.data_view_ids,
    ...conversation.messages.flatMap((message) => message.result?.sources.map((source) => source.data_view_id) ?? []),
  ])].slice(0, 20);
  const draft: KnowledgeInput = {
    title: boundedUtf8(firstQuestion.trim() || conversation.title || 'Investigation session', 256),
    kind: 'resolved_incident',
    status: 'draft',
    content,
    services: [],
    environments: [],
    connection_ids: [],
    tags: ['investigation'],
    source_url: null,
    data_view_ids: dataViewIds,
  };
  let contentLimit = 32_768;
  const suffixBytes = new TextEncoder().encode(truncated).length;
  if (new TextEncoder().encode(draft.content).length > contentLimit) {
    draft.content = boundedUtf8(content, contentLimit - suffixBytes) + truncated;
  }
  while (new TextEncoder().encode(JSON.stringify(draft)).length > 65_536 && contentLimit > suffixBytes) {
    contentLimit = Math.floor(contentLimit / 2);
    draft.content = boundedUtf8(content, Math.max(0, contentLimit - suffixBytes)) + truncated;
  }
  if (new TextEncoder().encode(JSON.stringify(draft)).length > 65_536) throw new Error('The investigation session scope exceeds the supported size limit.');
  return draft;
}

function canSaveInvestigationSession(conversation: AIConversation): boolean {
  return conversation.messages.some((message) => message.result
    && message.result.response_mode !== 'conversation'
    && message.result.response_mode !== 'data_query'
    && message.result.complete !== false
    && message.result.evidence_status !== 'insufficient_evidence'
    && message.result.summary.trim().length > 0);
}

function InvestigationResultView({ result, label }: { result: InvestigationResult; label: string }) {
  const conversationalReply = result.response_mode === 'conversation';
  const dataQueryReply = result.response_mode === 'data_query';
  const nonInvestigationReply = conversationalReply || dataQueryReply;
  const dataQuery = result.query_result;

  function playbookWindowLabel(window: NonNullable<InvestigationResult['playbook_evidence']>[number]['current'], baseline = false) {
    if (!window) return baseline ? 'Not run (All time or unavailable range)' : 'Unavailable';
    if (window.time_range_start_ms === null || window.time_range_end_ms === null) return 'All time';
    return `${window.time_range_start_ms} to ${window.time_range_end_ms} Unix ms`;
  }

  return <article className="ai-result">
    <div className="ai-result-label"><BrainCircuit size={15} aria-hidden="true" /> {conversationalReply ? 'Response' : dataQueryReply ? 'Data query' : label}</div>
    {!nonInvestigationReply && <p className="ai-evidence-warning">AI hypotheses, not verified causes. Citations identify supporting samples; they do not prove the conclusion.</p>}
    {!nonInvestigationReply && <section className="ai-evidence-coverage" aria-label="Evidence coverage"><strong>Evidence coverage</strong>
      {result.sources.some((source) => source.evidence_queries !== undefined) ? <><span>{result.sources.reduce((total, source) => total + source.records_examined, 0).toLocaleString()} sampled records</span><span>{result.sources.reduce((total, source) => total + (source.evidence_queries?.length ?? 0), 0)} bounded evidence queries</span><span>Not an exhaustive or representative scan</span></> : <span>Detailed sampling coverage was not recorded in this saved result.</span>}
    </section>}
    {!nonInvestigationReply && result.playbook_evidence && result.playbook_evidence.length > 0 && <details className="ai-playbook-evidence"><summary>Investigation playbook queries ({result.playbook_evidence.length})</summary>
      {result.playbook_evidence.map((evidence, index) => <section className="ai-playbook-query" key={`${evidence.playbook_id}-${evidence.step_name}-${evidence.data_view_id}-${index}`}>
        <h4>{evidence.step_name} · {evidence.data_view_name}</h4>
        <span>{evidence.playbook_title} · {evidence.index}</span>
        <code className="ai-playbook-query-text">{evidence.query || '(empty query; all records)'}</code>
        {evidence.breakdown_field && <span>Breakdown: {evidence.breakdown_field}</span>}
        {evidence.error && <p className="ai-caveat">{evidence.error}</p>}
        {([['Current', evidence.current], ['Baseline', evidence.baseline]] as const).map(([label, window]) => <div className="ai-playbook-window" key={label}>
          <strong>{label} · {playbookWindowLabel(window, label === 'Baseline')}</strong>
          {window && <>
            <span>Matched: {window.matched_records === null ? 'unavailable' : `${window.matched_records.toLocaleString()}${window.total_relation === 'gte' ? '+' : ''}`}</span>
            <span>Aggregated: {window.aggregate_records === null ? 'unavailable' : window.aggregate_records.toLocaleString()}</span>
            {window.histogram_unavailable_reason && <span>{window.histogram_unavailable_reason}</span>}
            {window.aggregations.length > 0 && <pre>{JSON.stringify(window.aggregations, null, 2)}</pre>}
          </>}
        </div>)}
      </section>)}
    </details>}
    {!nonInvestigationReply && result.evidence_status === 'insufficient_evidence' && <p className="ai-evidence-warning" role="status">Insufficient evidence for an incident hypothesis. An empty sample does not establish that no incident occurred.</p>}
    <p className="ai-summary">{result.summary}</p>
    {dataQueryReply && dataQuery && <section className="ai-data-query-result" aria-label="Executed data query">
      <h3>Executed SQL · {dataQuery.data_view_name}</h3>
      <pre className="ai-executed-sql"><code>{dataQuery.sql}</code></pre>
      <div className="ai-query-result-count" role="status">
        {dataQuery.truncated
          ? `Showing ${dataQuery.row_count} groups; more may match.`
          : `${dataQuery.row_count} duplicate groups returned.`}
      </div>
      {dataQuery.rows.length > 0 && <div className="ai-query-result-table-wrap"><table className="ai-query-result-table">
        <thead><tr>{dataQuery.columns.map((column) => <th key={column} scope="col">{column}</th>)}</tr></thead>
        <tbody>{dataQuery.rows.map((row, rowIndex) => <tr key={rowIndex}>{dataQuery.columns.map((column) => {
          const value = row[column];
          return <td key={column}>{typeof value === 'string' ? value : value === null || value === undefined ? '—' : JSON.stringify(value)}</td>;
        })}</tr>)}</tbody>
      </table></div>}
      {dataQuery.rows.length === 0 && <p>No duplicate groups matched the query.</p>}
    </section>}
    {!nonInvestigationReply && <section className="ai-token-usage" aria-label="Token usage">
      <strong>Provider token usage</strong>
      {result.token_usage.some((call) => call.total_tokens !== null)
        ? <div>{result.token_usage.map((call) => <span key={call.stage}>{call.stage}: {call.total_tokens === null ? 'not reported' : `${call.total_tokens.toLocaleString()} total (${call.prompt_tokens?.toLocaleString() ?? '—'} prompt + ${call.completion_tokens?.toLocaleString() ?? '—'} completion)`}</span>)}</div>
        : <span>Provider did not report token usage.</span>}
    </section>}
    {!nonInvestigationReply && result.findings.length > 0 && <section className="ai-findings" aria-label="Findings">
      <h3>Findings</h3>
      {result.findings.map((finding, index) => <article className="ai-finding" key={`${finding.title}-${index}`}>
        <div className="ai-finding-heading"><h4>{finding.title}</h4><span className={`ai-severity severity-${finding.severity}`}>{finding.severity}</span></div>
        <span className="ai-hypothesis-label">Hypothesis · requires verification</span>
        <p>{finding.detail}</p>
        <div className="ai-finding-meta"><span>{finding.confidence} confidence</span><span>Confidence is model judgment</span>{finding.evidence.map((evidence) => <span key={`${evidence.data_view_id}-${evidence.record_ids.join('-')}`}>{evidence.data_view_name}: {new Set(evidence.record_ids).size} cited sample records{evidence.record_ids.length ? ` · ${evidence.record_ids.join(', ')}` : ' · no validated sample citations'}</span>)}</div>
      </article>)}
    </section>}
    {!nonInvestigationReply && <details className="ai-sources-used"><summary>Sources and query scope</summary>
      {result.sources.map((source) => <div className="ai-source-used" key={source.data_view_id}><strong>{source.name}</strong><span>{source.index} · {source.records_examined.toLocaleString()} {source.sample_limit === undefined ? 'records reported in saved result' : `sampled rows (limit ${source.sample_limit})`}</span>
        {source.sample_records_available !== undefined && <span>{source.sample_records_available} sample records supplied to the model</span>}
        {source.aggregate_records !== undefined && <span>{source.aggregate_records === null ? 'Query aggregation count unavailable' : `${source.aggregate_records.toLocaleString()} records counted by query aggregation (not a finding count)`}</span>}
        <small>{source.query_summary}</small>
        {source.evidence_queries?.map((query, index) => <small key={index}>{index + 1}. {query.sampling === 'adaptive_follow_up' ? 'adaptive follow-up query' : `${query.sampling} sample`} · {query.records_returned} records returned · {query.query_summary}{query.reason ? ` · ${query.reason}` : ''}</small>)}
      </div>)}
    </details>}
    {!nonInvestigationReply && result.caveats.length > 0 && <div className="ai-caveats"><strong>Limits</strong>{result.caveats.map((caveat, index) => <p key={index}>{caveat}</p>)}</div>}
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
  const [savingSessionDraft, setSavingSessionDraft] = useState(false);
  const [sessionDraftId, setSessionDraftId] = useState<string | null>(null);
  const [sessionDraftError, setSessionDraftError] = useState('');
  const [conversationLoading, setConversationLoading] = useState(false);
  const [conversationBusy, setConversationBusy] = useState(false);
  const [conversationError, setConversationError] = useState('');
  const [serverSettings, setServerSettings] = useState<ApplicationSettings>({});
  const [settingsRevision, setSettingsRevision] = useState('');
  const [retentionDays, setRetentionDays] = useState(String(DEFAULT_HISTORY_RETENTION_DAYS));
  const [retentionSaving, setRetentionSaving] = useState(false);
  const [retentionNotice, setRetentionNotice] = useState('');
  const [retentionError, setRetentionError] = useState('');
  const [submittedQuestion, setSubmittedQuestion] = useState('');
  const [starting, setStarting] = useState(false);
  const [deciding, setDeciding] = useState(false);
  const [error, setError] = useState('');
  const [aiTimeout, setAiTimeout] = useState(() => storedSeconds('ai-timeout', TIMEOUT_OPTIONS, 60));
  const [checkIn, setCheckIn] = useState(() => storedSeconds('ai-check-in', CHECK_IN_OPTIONS, 120));
  const [adaptiveFollowUp, setAdaptiveFollowUp] = useState(false);
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
    api.settings(controller.signal)
      .then(({ settings, revision }) => {
        if (controller.signal.aborted) return;
        setServerSettings(settings);
        setSettingsRevision(revision);
        const days = settings.investigation_history_retention_days;
        setRetentionDays(String(typeof days === 'number' && Number.isInteger(days) && days >= 1 && days <= 3_650 ? days : DEFAULT_HISTORY_RETENTION_DAYS));
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setRetentionError(errorMessage(reason, 'Could not load history retention settings'));
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

  async function saveHistoryRetention() {
    const days = Number(retentionDays);
    if (!Number.isInteger(days) || days < 1 || days > 3_650 || !settingsRevision || retentionSaving) {
      setRetentionError('Retention must be a whole number from 1 to 3650 days.');
      return;
    }
    setRetentionSaving(true);
    setRetentionError('');
    setRetentionNotice('');
    try {
      const updated = await api.updateSettings({ ...serverSettings, investigation_history_retention_days: days }, settingsRevision);
      setServerSettings(updated.settings);
      setSettingsRevision(updated.revision);
      setRetentionDays(String(days));
      setRetentionNotice('History retention saved.');
      await refreshConversations();
    } catch (reason) {
      setRetentionError(errorMessage(reason, 'Could not save history retention'));
    } finally {
      setRetentionSaving(false);
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
    setSessionDraftId(null);
    setSessionDraftError('');
    setSubmittedQuestion(prompt);
    try {
      const started = await api.startInvestigation(prompt, dataViewIds, modelId, timeRange, { ai_timeout_seconds: aiTimeout, time_budget_seconds: checkIn, adaptive_followup: adaptiveFollowUp });
      setJob(started);
      setQuestion('');
      setActiveConversation(null);
      void api.aiConversation(started.conversation_id).then(setActiveConversation).catch(() => undefined);
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
      setSessionDraftId(null);
      setSessionDraftError('');
      return;
    }
    setConversationLoading(true);
    setConversationError('');
    setError('');
    setSessionDraftId(null);
    setSessionDraftError('');
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

  async function saveInvestigationSession() {
    if (!activeConversation || !canSaveInvestigationSession(activeConversation) || busy || conversationLoading || savingSessionDraft || sessionDraftId) return;
    setSavingSessionDraft(true);
    setSessionDraftError('');
    try {
      const saved = await api.addKnowledge(investigationConversationKnowledgeDraft(activeConversation));
      setSessionDraftId(saved.id);
    } catch (reason) {
      setSessionDraftError(errorMessage(reason, 'Could not save the investigation session.'));
    } finally {
      setSavingSessionDraft(false);
    }
  }

  async function investigate(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const prompt = question.trim();
    if (!prompt || busy || conversationBusy || (!activeConversation && (!selectedIds.length || !selectedModelId))) return;
    if (activeConversation) {
      if (activeConversation.investigation_scope) {
        setStarting(true);
        setConversationError('');
        setJob(null);
        setSessionDraftId(null);
        setSessionDraftError('');
        try {
          const started = await api.continueAIInvestigation(activeConversation.id, prompt, activeConversation.revision, {
            ai_timeout_seconds: aiTimeout,
            time_budget_seconds: checkIn,
            adaptive_followup: adaptiveFollowUp,
          });
          setActiveConversation(started.conversation);
          setJob(started.investigation);
          setQuestion('');
          await refreshConversations();
        } catch (reason) {
          setConversationError(errorMessage(reason, 'Could not continue the investigation.'));
        } finally {
          setStarting(false);
        }
        return;
      }
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
    setConversationLoading(true);
    api.aiConversation(job.conversation_id || job.id)
      .then((conversation) => {
        if (active) setActiveConversation(conversation);
        return refreshConversations();
      })
      .catch(() => undefined)
      .finally(() => { if (active) setConversationLoading(false); });
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
          <label className="ai-retention-control" htmlFor="ai-history-retention">Retention (days)<input id="ai-history-retention" aria-label="History retention days" type="number" min="1" max="3650" step="1" value={retentionDays} disabled={retentionSaving || !settingsRevision} onChange={(event) => { setRetentionDays(event.target.value); setRetentionNotice(''); }} /></label>
          <button type="button" className="quiet-button ai-retention-save" aria-label="Save history retention" title="Save history retention" disabled={retentionSaving || !settingsRevision || !Number.isInteger(Number(retentionDays)) || Number(retentionDays) < 1 || Number(retentionDays) > 3_650 || Number(retentionDays) === Number(serverSettings.investigation_history_retention_days ?? DEFAULT_HISTORY_RETENTION_DAYS)} onClick={() => void saveHistoryRetention()}><Save size={14} aria-hidden="true" /></button>
        {activeConversation && <button type="button" className="quiet-button" aria-label="Delete conversation" title="Delete conversation" disabled={busy || conversationBusy} onClick={() => void removeConversation()}><Trash2 size={14} aria-hidden="true" /></button>}
        {conversationLoading && <span role="status">Loading history…</span>}
          {retentionSaving && <span role="status">Saving retention…</span>}
          {retentionNotice && <span className="ai-retention-notice" role="status">{retentionNotice}</span>}
      </div>
      {activeConversation && canSaveInvestigationSession(activeConversation) && <div className="ai-session-actions" role="group" aria-label="Investigation session actions">
        <button type="button" className="quiet-button" disabled={busy || conversationLoading || savingSessionDraft || !!sessionDraftId} onClick={() => void saveInvestigationSession()}>{savingSessionDraft ? 'Saving full investigation…' : sessionDraftId ? 'Full investigation saved as draft' : 'Save full investigation as Knowledge draft'}</button>
        {sessionDraftId && onOpenKnowledge && <button type="button" className="quiet-button" onClick={() => onOpenKnowledge(sessionDraftId)}><BookOpen size={15} aria-hidden="true" /> Review draft</button>}
        {sessionDraftError && <span className="ai-session-save-error" role="alert">{sessionDraftError}</span>}
      </div>}
      {conversationError && <div className="alert" role="alert"><span>{conversationError}</span><button type="button" aria-label="Dismiss history error" onClick={() => setConversationError('')}>×</button></div>}
        {retentionError && <div className="alert ai-retention-error" role="alert"><span>{retentionError}</span><button type="button" aria-label="Dismiss retention error" onClick={() => setRetentionError('')}>×</button></div>}
      <div className="ai-controls-row">
      <div className="ai-dataview-picker" ref={pickerRef}>
        <button ref={pickerTriggerRef} type="button" className={`ai-dataview-trigger${dataViewPickerOpen ? ' open' : ''}`} aria-label="Choose Data Views" aria-expanded={dataViewPickerOpen} aria-controls="ai-dataview-menu" disabled={dataViews.length === 0 || busy || !!activeConversation} onClick={() => setDataViewPickerOpen((open) => !open)}>
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
      {activeConversation ? <span className="ai-saved-scope">Saved investigation scope</span> : <TimeRangePicker start={startDate} end={endDate} timeZone={timeZone} onTimeZoneChange={setTimeZone} onChange={(start, end) => { setStartDate(start); setEndDate(end); }} />}
      </div>
      {selectedDataViews.length > 0 && <div className="ai-selected-views" aria-label="Selected Data Views">{selectedDataViews.map((view) => <span className="ai-selected-view" key={view.id}>{view.name}<button type="button" aria-label={`Remove ${view.name}`} title={`Remove ${view.name}`} disabled={busy || !!activeConversation} onClick={() => toggleDataView(view.id)}><X size={12} /></button></span>)}</div>}
      {activeConversation && !activeConversation.investigation_scope && <p className="ai-legacy-conversation-note" role="status">This saved conversation has no source scope. Replies use its history only; start a new investigation to query fresh evidence.</p>}
      {dataViews.length === 0 && <p className="ai-empty-sources">Create a Data View before starting an investigation.</p>}

      <section className="ai-transcript" aria-live="polite" aria-label="Investigation result">
        {!submittedQuestion && !activeConversation && !busy && <div className="ai-welcome"><h2>Start a new investigation</h2>{modelStatus === 'empty' && <button className="sample-query" onClick={() => onManageModels?.()}>Open AI model management</button>}</div>}
        {activeConversation
          ? activeConversation.messages.map((message, index) => message.role === 'user'
            ? <div className="ai-user-message" key={`${index}-user`}><span>You</span><p>{message.content}</p></div>
            : message.result
              ? <InvestigationResultView key={`${activeConversation.id}-${index}-result`} result={message.result} label={message.result.complete === false ? 'Partial investigation' : 'Investigation'} />
              : <div className="ai-assistant-message" key={`${index}-assistant`}><span>InvestiGenie AI</span><p>{message.content}</p></div>)
          : submittedQuestion && <div className="ai-user-message"><span>You</span><p>{submittedQuestion}</p></div>}
        {starting && <div className="ai-thinking" role="status"><span className="spinner dark" /> Starting investigation…</div>}
        {job && (job.status === 'running' || job.status === 'stopping') && <div className="ai-progress" role="status">
          <span className="spinner dark" />
          <div className="ai-progress-text">
            <strong>{job.status === 'stopping' ? 'Stopping and keeping findings so far…' : job.progress.stage || 'Investigation in progress…'}</strong>
            <small>{job.progress.records_examined.toLocaleString()} distinct sample rows collected · {job.progress.batches_processed.toLocaleString()} bounded source queries · {job.progress.analysis_cycles.toLocaleString()} analysis cycles · {elapsedLabel(job.progress.elapsed_ms)} elapsed</small>
          </div>
          {job.status === 'running' && <button type="button" className="quiet-button ai-stop" disabled={deciding} onClick={() => void decide('terminate')}><Square size={12} aria-hidden="true" /> Stop</button>}
        </div>}
        {job?.status === 'awaiting_decision' && <section className="ai-decision" role="group" aria-label="Continue or stop the investigation">
          <div className="ai-decision-heading"><CirclePause size={16} aria-hidden="true" /><strong>Should InvestiGenie keep investigating?</strong></div>
          <p>{job.pause_reason}</p>
          <small>{job.progress.records_examined.toLocaleString()} distinct sample rows collected in {job.progress.batches_processed.toLocaleString()} bounded source queries · {elapsedLabel(job.progress.elapsed_ms)} elapsed</small>
          <div className="ai-decision-actions">
            <button type="button" className="search-button" disabled={deciding} onClick={() => void decide('continue')}><Play size={13} aria-hidden="true" /> Continue investigation</button>
            <button type="button" className="quiet-button" disabled={deciding} onClick={() => void decide('terminate')}><Square size={12} aria-hidden="true" /> Stop and show findings</button>
          </div>
        </section>}
        {activeConversation && job?.conversation_id === activeConversation.id && activeStatuses.has(job.status) && job.result && <InvestigationResultView result={job.result} label="Findings so far" />}
        {error && <div className="alert ai-error" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{error}</span></div>}
        {job?.status === 'terminated' && !result && <p className="ai-empty-sources">The investigation was stopped before any findings were produced.</p>}
        {result && !activeConversation && <InvestigationResultView key={job?.id} result={result} label={job?.status === 'completed' ? 'Investigation' : job && activeStatuses.has(job.status) ? 'Findings so far' : 'Partial investigation'} />}
      </section>

      <form className="ai-composer" onSubmit={(event) => void investigate(event)}>
        <label className="sr-only" htmlFor="ai-question">Ask InvestiGenie AI</label>
        <textarea id="ai-question" value={question} maxLength={8000} rows={3} placeholder={activeConversation ? 'Ask a follow-up about this investigation…' : 'Describe what you’re investigating…'} onChange={(event) => setQuestion(event.target.value)} onKeyDown={handleComposerKeyDown} disabled={busy || conversationBusy || (!activeConversation && (dataViews.length === 0 || !selectedModelId))} />
        <div className="ai-composer-footer">
          <div className="ai-model-picker">
            {modelStatus === 'loading' && <span role="status">Loading AI models...</span>}
            {modelStatus === 'failed' && <div className="alert ai-error" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{modelError}</span><button type="button" className="icon-button" aria-label="Retry loading AI models" title="Retry loading AI models" onClick={() => setModelAttempt((attempt) => attempt + 1)}>↻</button></div>}
            {modelStatus === 'empty' && <><span>No saved AI models</span><button type="button" className="quiet-button" onClick={() => onManageModels?.()}>Manage AI models</button></>}
            {modelStatus === 'ready' && <label className="ai-model-compact" htmlFor="ai-model-select"><span className="sr-only">Model</span><select id="ai-model-select" aria-label="AI model" value={selectedModelId} disabled={busy || !!activeConversation} onChange={(event) => selectModel(event.target.value)}>{activeConversation && !models.some((model) => model.id === activeConversation.model_id) && <option value={activeConversation.model_id}>Saved model: {activeConversation.model_id}</option>}{models.map((model) => <option key={model.id} value={model.id}>{model.name} · {model.model}</option>)}</select></label>}
            <label className="ai-run-setting" title="How long to wait for each AI provider response"><span>Timeout</span><select aria-label="AI response timeout" value={aiTimeout} disabled={busy} onChange={(event) => updateTimeout(Number(event.target.value))}>{TIMEOUT_OPTIONS.map((seconds) => <option key={seconds} value={seconds}>{durationLabel(seconds)}</option>)}</select></label>
            <label className="ai-run-setting" title="Ask whether to continue after the investigation runs this long"><span>Ask after</span><select aria-label="Ask to continue after" value={checkIn} disabled={busy} onChange={(event) => updateCheckIn(Number(event.target.value))}>{CHECK_IN_OPTIONS.map((seconds) => <option key={seconds} value={seconds}>{durationLabel(seconds)}</option>)}</select></label>
            <label className="ai-run-setting ai-follow-up-setting" title="Allow one extra metadata-validated query when evidence is thin or conflicting"><input type="checkbox" aria-label="Allow one adaptive follow-up query" checked={adaptiveFollowUp} disabled={busy || (!!activeConversation && !activeConversation.investigation_scope)} onChange={(event) => setAdaptiveFollowUp(event.target.checked)} /><span>Adaptive follow-up</span></label>
          </div>
          <button className="ai-send" type="submit" aria-label={activeConversation ? 'Send follow-up' : 'Run investigation'} title={activeConversation ? 'Send follow-up' : 'Run investigation'} disabled={busy || conversationBusy || !question.trim() || (!activeConversation && (selectedIds.length === 0 || !selectedModelId))}>{conversationBusy ? <span className="spinner" /> : <ArrowUp size={17} aria-hidden="true" />}</button>
        </div>
      </form>
      <p className="ai-privacy-note">Connection credentials remain in InvestiGenie. Selected evidence is sent to the configured AI provider for analysis.</p>
    </section>
  </main>;
}
