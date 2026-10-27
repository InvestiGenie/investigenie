export type IndexSummary = {
  name: string;
  document_count: string | null;
  health: string | null;
};

export type ConnectionType = string;

export type IntegrationIconKey = 'elasticsearch' | 'clickhouse' | 'bigquery' | 'loki' | 'prometheus' | 'victorialogs' | 'victoriametrics';

export type ConnectionStatus = 'connected' | 'not_connected';

export type ConnectionConfiguration = Record<string, string | boolean | null>;

export type ConnectionField = {
  name: string;
  label: string;
  input_type: 'text' | 'url' | 'password' | 'checkbox' | 'select';
  required: boolean;
  secret: boolean;
  editable: boolean;
  group?: string;
  placeholder: string;
  options?: Array<{ value: string; label: string }>;
  visible_when?: { field: string; value: string | boolean };
};

export type QueryDescriptor = {
  placeholder: string;
  default_query: string;
  field_query: string;
  identifier_quote: string;
  help: string;
  document_mode: 'lookup' | 'inline';
  sort_requires_projection: boolean;
  histogram_total_from_buckets: boolean;
  schema_label: string;
};

export type IntegrationDescriptor = {
  id: ConnectionType;
  display_name: string;
  icon?: IntegrationIconKey;
  query_language: string;
  description: string;
  connection_fields: ConnectionField[];
  query: QueryDescriptor;
};

export type ConnectionSummary = {
  id: string;
  revision: string;
  name: string;
  connection_type: ConnectionType;
  url: string;
  database: string | null;
  status: 'connected';
  integration: IntegrationDescriptor;
  configuration: ConnectionConfiguration;
};

export type DataViewSummary = {
  id: string;
  revision: string;
  name: string;
  connection_id: string;
  index: string;
  time_field: string;
};

export type AIModelSummary = {
  id: string;
  revision: string;
  name: string;
  api_url: string;
  model: string;
  has_api_key: true;
};

export type KnowledgeKind = 'service_context' | 'runbook' | 'resolved_incident';
export type KnowledgeStatus = 'draft' | 'published' | 'archived';
export type KnowledgeInput = {
  title: string;
  kind: KnowledgeKind;
  status: KnowledgeStatus;
  content: string;
  services: string[];
  environments: string[];
  connection_ids: string[];
  data_view_ids: string[];
  tags: string[];
  source_url: string | null;
};
export type KnowledgeEntry = KnowledgeInput & {
  id: string;
  revision: string;
  created_at_ms: number;
  updated_at_ms: number;
};

export type AIModelRequest = {
  name: string;
  api_url: string;
  model: string;
  api_key?: string;
};

export type InvestigationFinding = {
  title: string;
  detail: string;
  severity: 'critical' | 'high' | 'medium' | 'low' | 'info';
  confidence: 'high' | 'medium' | 'low';
  evidence: Array<{
    data_view_id: string;
    data_view_name: string;
    record_ids: string[];
    count: number;
  }>;
};

export type InvestigationResult = {
  summary: string;
  findings: InvestigationFinding[];
  sources: Array<{
    data_view_id: string;
    name: string;
    index: string;
    records_examined: number;
    batches_processed?: number;
    query_summary: string;
    sample_records_available?: number;
    sample_limit?: number;
    aggregate_records?: number | null;
    evidence_queries?: Array<{ sampling: string; query_summary: string; records_returned: number; aggregate_records: number | null }>;
  }>;
  caveats: string[];
  token_usage: Array<{
    stage: string;
    prompt_tokens: number | null;
    completion_tokens: number | null;
    total_tokens: number | null;
  }>;
  complete?: boolean;
  evidence_status?: 'sample_supported' | 'insufficient_evidence';
  conclusions_verified?: boolean;
};

export type InvestigationControls = {
  ai_timeout_seconds: number;
  time_budget_seconds: number;
};

export type InvestigationJobStatus = 'running' | 'stopping' | 'awaiting_decision' | 'completed' | 'terminated' | 'failed';

export type InvestigationJob = {
  id: string;
  status: InvestigationJobStatus;
  progress: {
    stage: string;
    records_examined: number;
    batches_processed: number;
    analysis_cycles: number;
    elapsed_ms: number;
  };
  time_budget_seconds: number;
  pause_reason: string | null;
  result: InvestigationResult | null;
  error: { code: string; message: string } | null;
};

export type AIConversationMessage = {
  role: 'user' | 'assistant';
  content: string;
  result?: InvestigationResult;
};

export type AIConversation = {
  id: string;
  revision: string;
  title: string;
  model_id: string;
  data_view_ids: string[];
  created_at_ms: number;
  updated_at_ms: number;
  messages: AIConversationMessage[];
};

export type AIConversationSummary = Pick<AIConversation, 'id' | 'revision' | 'title' | 'updated_at_ms'>;

export type CreateDataViewRequest = Omit<DataViewSummary, 'id' | 'revision'>;

export type AddConnectionRequest = ConnectionConfiguration & {
  name: string;
  connection_type: ConnectionType;
};

export type UpdateConnectionRequest = ConnectionConfiguration & {
  name: string;
};

export type FieldSummary = {
  name: string;
  field_type: string;
  is_time_field?: boolean;
};

export type SearchHit = {
  index: string;
  id: string;
  score: number | null;
  source: Record<string, unknown> | null;
};

export type SearchResult = {
  next_cursor?: string | null;
  took: number | null;
  timed_out: boolean;
  total: number;
  total_relation: string;
  hits: SearchHit[];
  histogram: HistogramBucket[];
  histogram_unavailable_reason?: string | null;
  histogram_interval?: string | null;
  columns?: string[];
};

export const BREAKDOWN_OTHER_KEY = '__other__';

export type HistogramBucket = {
  timestamp: number;
  label: string | null;
  count: number;
  breakdown?: Array<{ key: string; count: number }>;
};

export type DocumentResult = {
  index: string;
  id: string;
  found: boolean;
  source: Record<string, unknown> | null;
};

export type SearchRequest = {
  cursor?: string;
  include_histogram?: boolean;
  index: string;
  query: string;
  from: number;
  size: number;
  sort_by?: string;
  sort_order?: 'asc' | 'desc';
  histogram_field?: string;
  time_range?: { start: number; end: number };
  histogram_interval?: string;
  histogram_breakdown_field?: string;
  time_zone?: string;
};

// Production builds are served by the API itself, so they default to the same origin.
const apiBase = (import.meta.env.VITE_API_BASE_URL ?? (import.meta.env.DEV ? 'http://127.0.0.1:3000' : '')).replace(/\/$/, '');

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`${apiBase}${path}`, init);
  } catch (reason) {
    if (init?.signal?.aborted || (reason instanceof DOMException && reason.name === 'AbortError')) throw reason;
    throw new Error('Cannot reach the InvestiGenie API. Check that the API is running and its URL and allowed web origin match this page.');
  }
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const message = isRecord(body) && isRecord(body.error) && typeof body.error.message === 'string'
      ? body.error.message
      : `Request failed (${response.status})`;
    throw new Error(message);
  }
  return body as T;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function jsonMutation(method: 'POST' | 'PUT' | 'DELETE', body?: unknown, revision?: string): RequestInit {
  return {
    method,
    headers: {
      ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
      ...(revision !== undefined ? { 'If-Match': `"${revision}"` } : {}),
    },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  };
}

export const api = {
  async knowledgeEntries(signal?: AbortSignal): Promise<KnowledgeEntry[]> {
    return (await request<{ entries: KnowledgeEntry[] }>('/api/knowledge', { signal })).entries;
  },
  async knowledgeEntry(id: string, signal?: AbortSignal): Promise<KnowledgeEntry> {
    return (await request<{ entry: KnowledgeEntry }>(`/api/knowledge/${encodeURIComponent(id)}`, { signal })).entry;
  },
  async addKnowledge(body: KnowledgeInput): Promise<KnowledgeEntry> {
    return (await request<{ entry: KnowledgeEntry }>('/api/knowledge', jsonMutation('POST', body))).entry;
  },
  async updateKnowledge(id: string, body: KnowledgeInput, revision: string): Promise<KnowledgeEntry> {
    return (await request<{ entry: KnowledgeEntry }>(`/api/knowledge/${encodeURIComponent(id)}`, jsonMutation('PUT', body, revision))).entry;
  },
  async removeKnowledge(id: string, revision: string): Promise<void> {
    await request(`/api/knowledge/${encodeURIComponent(id)}`, jsonMutation('DELETE', undefined, revision));
  },
  async ready(signal?: AbortSignal): Promise<void> {
    await request('/api/health/ready', { signal });
  },
  async integrations(signal?: AbortSignal): Promise<IntegrationDescriptor[]> {
    const response = await request<{ integrations: IntegrationDescriptor[] }>('/api/integrations', { signal });
    return response.integrations;
  },
  async connections(signal?: AbortSignal): Promise<ConnectionSummary[]> {
    const response = await request<{ connections: ConnectionSummary[] }>('/api/connections', { signal });
    return response.connections;
  },
  async connectionStatus(connectionId: string, signal?: AbortSignal): Promise<ConnectionStatus> {
    const response = await request<unknown>(`/api/connections/${encodeURIComponent(connectionId)}/status`, { signal });
    if (!isRecord(response) || (response.status !== 'connected' && response.status !== 'not_connected')) {
      throw new Error('Invalid datasource connection status response');
    }
    return response.status;
  },
  async dataViews(signal?: AbortSignal): Promise<DataViewSummary[]> {
    const response = await request<{ data_views: DataViewSummary[] }>('/api/data-views', { signal });
    return response.data_views;
  },
  async aiModels(signal?: AbortSignal): Promise<AIModelSummary[]> {
    const response = await request<{ models: AIModelSummary[] }>('/api/ai/models', { signal });
    return response.models;
  },
  async addAIModel(body: AIModelRequest): Promise<AIModelSummary> {
    const response = await request<{ model: AIModelSummary }>('/api/ai/models', jsonMutation('POST', body));
    return response.model;
  },
  async updateAIModel(modelId: string, body: AIModelRequest, revision: string): Promise<AIModelSummary> {
    const response = await request<{ model: AIModelSummary }>(`/api/ai/models/${encodeURIComponent(modelId)}`, jsonMutation('PUT', body, revision));
    return response.model;
  },
  async removeAIModel(modelId: string, revision: string): Promise<void> {
    await request(`/api/ai/models/${encodeURIComponent(modelId)}`, jsonMutation('DELETE', undefined, revision));
  },
  async investigate(question: string, dataViewIds: string[], aiModelId: string, timeRange: SearchRequest['time_range'] | null, signal?: AbortSignal): Promise<InvestigationResult> {
    return request<InvestigationResult>('/api/ai/investigate', {
      ...jsonMutation('POST', { question, data_view_ids: dataViewIds, ai_model_id: aiModelId, time_range: timeRange }),
      signal,
    });
  },
  async startInvestigation(question: string, dataViewIds: string[], aiModelId: string, timeRange: SearchRequest['time_range'] | null, controls: InvestigationControls): Promise<InvestigationJob> {
    const response = await request<{ investigation: InvestigationJob }>('/api/ai/investigations', jsonMutation('POST', { question, data_view_ids: dataViewIds, ai_model_id: aiModelId, time_range: timeRange, ...controls }));
    return response.investigation;
  },
  async investigation(id: string, signal?: AbortSignal): Promise<InvestigationJob> {
    const response = await request<{ investigation: InvestigationJob }>(`/api/ai/investigations/${encodeURIComponent(id)}`, { signal });
    return response.investigation;
  },
  async decideInvestigation(id: string, action: 'continue' | 'terminate'): Promise<InvestigationJob> {
    const response = await request<{ investigation: InvestigationJob }>(`/api/ai/investigations/${encodeURIComponent(id)}/decision`, jsonMutation('POST', { action }));
    return response.investigation;
  },
  async aiConversations(signal?: AbortSignal): Promise<AIConversationSummary[]> {
    const response = await request<{ conversations: AIConversationSummary[] }>('/api/ai/conversations', { signal });
    return response.conversations;
  },
  async aiConversation(id: string, signal?: AbortSignal): Promise<AIConversation> {
    const response = await request<{ conversation: AIConversation }>(`/api/ai/conversations/${encodeURIComponent(id)}`, { signal });
    return response.conversation;
  },
  async continueAIConversation(id: string, message: string, revision: string): Promise<AIConversation> {
    const response = await request<{ conversation: AIConversation }>(`/api/ai/conversations/${encodeURIComponent(id)}/messages`, jsonMutation('POST', { message }, revision));
    return response.conversation;
  },
  async removeAIConversation(id: string, revision: string): Promise<void> {
    await request(`/api/ai/conversations/${encodeURIComponent(id)}`, jsonMutation('DELETE', undefined, revision));
  },
  async addDataView(body: CreateDataViewRequest): Promise<void> {
    await request('/api/data-views', jsonMutation('POST', body));
  },
  async removeDataView(dataViewId: string, revision: string): Promise<void> {
    await request(`/api/data-views/${encodeURIComponent(dataViewId)}`, jsonMutation('DELETE', undefined, revision));
  },
  async updateDataView(dataViewId: string, body: CreateDataViewRequest, revision: string): Promise<void> {
    await request(`/api/data-views/${encodeURIComponent(dataViewId)}`, jsonMutation('PUT', body, revision));
  },
  async addConnection(body: AddConnectionRequest): Promise<void> {
    await request('/api/connections', jsonMutation('POST', body));
  },
  async updateConnection(connectionId: string, body: UpdateConnectionRequest, revision: string): Promise<void> {
    await request(`/api/connections/${encodeURIComponent(connectionId)}`, jsonMutation('PUT', body, revision));
  },
  async removeConnection(connectionId: string, revision: string): Promise<void> {
    await request(`/api/connections/${encodeURIComponent(connectionId)}`, jsonMutation('DELETE', undefined, revision));
  },
  async indices(connectionId: string, signal?: AbortSignal): Promise<IndexSummary[]> {
    const response = await request<{ indices: IndexSummary[] }>(`/api/connections/${encodeURIComponent(connectionId)}/indices`, { signal });
    return response.indices;
  },
  async fields(connectionId: string, index: string, signal?: AbortSignal): Promise<FieldSummary[]> {
    const response = await request<{ fields: FieldSummary[] }>(
      `/api/connections/${encodeURIComponent(connectionId)}/indices/${encodeURIComponent(index)}/fields`,
      { signal },
    );
    return response.fields;
  },
  search(connectionId: string, body: SearchRequest, signal?: AbortSignal): Promise<SearchResult> {
    return request<SearchResult>(`/api/connections/${encodeURIComponent(connectionId)}/search`, {
      ...jsonMutation('POST', body),
      signal,
    });
  },
  document(connectionId: string, hit: Pick<SearchHit, 'index' | 'id'>, signal?: AbortSignal): Promise<DocumentResult> {
    return request<DocumentResult>(
      `/api/connections/${encodeURIComponent(connectionId)}/indices/${encodeURIComponent(hit.index)}/documents/${encodeURIComponent(hit.id)}`,
      { signal },
    );
  },
};

export const SEARCH_PAGE_SIZE = 50;
export const MAX_SEARCH_RECORDS = 100;

export function makeSearchRequest(
  index: string,
  query: string,
  from: number,
  sortBy: string,
  histogramField?: string,
  sortOrder: 'asc' | 'desc' = 'desc',
  startDate = '',
  endDate = '',
): SearchRequest {
  let timeRange: SearchRequest['time_range'];
  if (startDate || endDate) {
    const utcInput = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,3})?)?$/;
    const start = utcInput.test(startDate) ? Date.parse(`${startDate}Z`) : NaN;
    const end = utcInput.test(endDate) ? Date.parse(`${endDate}Z`) : NaN;
    if (!startDate || !endDate || !Number.isFinite(start) || !Number.isFinite(end) || start > end) {
      throw new Error('Choose valid start and end dates, with start no later than end.');
    }
    if (!histogramField) throw new Error('A time window requires the data view time field.');
    timeRange = { start, end };
  }
  return {
    index,
    query,
    from,
    size: SEARCH_PAGE_SIZE,
    ...(sortBy ? { sort_by: sortBy, sort_order: sortOrder } : {}),
    ...(histogramField ? { histogram_field: histogramField } : {}),
    ...(timeRange ? { time_range: timeRange } : {}),
  };
}
