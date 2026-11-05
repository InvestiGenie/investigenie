import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import InvestiGenieAIPage from './InvestiGenieAIPage';
import { api, type AIConversation, type InvestigationJob, type InvestigationResult } from './api';
import { clearPreference, getPreference } from './preferences';

const dataViews = [
  { id: 'service-one', revision: 'r1', name: 'Service 1', connection_id: 'source', index: 'service-one-*', time_field: '@timestamp' },
  { id: 'service-two', revision: 'r1', name: 'Service 2', connection_id: 'source', index: 'service-two-*', time_field: '@timestamp' },
];

const result: InvestigationResult = {
  summary: 'Service 2 timeouts rose after Service 1 errors increased.',
  findings: [{
    title: 'Correlated timeout increase',
    detail: 'Both views show related timeout events in the selected investigation window.',
    severity: 'high',
    confidence: 'medium',
    evidence: [{ data_view_id: 'service-two', data_view_name: 'Service 2', record_ids: ['event-1'], count: 1 }],
  }],
  sources: [{ data_view_id: 'service-two', name: 'Service 2', index: 'service-two-*', records_examined: 1, query_summary: 'Last 30 minutes; error_type EQ', sample_records_available: 1, sample_limit: 8, aggregate_records: 12, evidence_queries: [{ sampling: 'latest', query_summary: 'Last 30 minutes; error_type EQ', records_returned: 1, aggregate_records: 12 }] }],
  caveats: ['Analysis is limited to the last 30 minutes.'],
  token_usage: [
    { stage: 'planning', prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 },
    { stage: 'analysis', prompt_tokens: 300, completion_tokens: 60, total_tokens: 360 },
  ],
};

function openDataViewPicker() {
  fireEvent.click(screen.getByRole('button', { name: 'Choose Data Views' }));
}

const defaultControls = { ai_timeout_seconds: 60, time_budget_seconds: 120, adaptive_followup: false };

function job(overrides: Partial<InvestigationJob> = {}): InvestigationJob {
  return {
    id: 'job-1',
    conversation_id: 'job-1',
    status: 'completed',
    progress: { stage: 'Done', records_examined: 12, batches_processed: 1, analysis_cycles: 1, elapsed_ms: 4_000 },
    time_budget_seconds: 120,
    pause_reason: null,
    result,
    error: null,
    ...overrides,
  };
}

function conversationForResult(id: string, title: string, question: string, savedResult: InvestigationResult = result): AIConversation {
  return {
    id, revision: 'conversation-revision', title, model_id: 'model-primary', data_view_ids: ['service-one'],
    investigation_scope: { time_range: null }, created_at_ms: 1, updated_at_ms: 2,
    messages: [
      { role: 'user', content: question },
      { role: 'assistant', content: savedResult.summary, result: savedResult },
    ],
  };
}

describe('InvestiGenieAIPage', () => {
  beforeEach(() => {
    clearPreference('ai-model');
    clearPreference('ai-timeout');
    clearPreference('ai-check-in');
    vi.spyOn(api, 'aiConversations').mockResolvedValue([]);
    vi.spyOn(api, 'aiConversation').mockRejectedValue(new Error('Conversation not found'));
    vi.spyOn(api, 'settings').mockResolvedValue({ settings: { theme: 'system', default_timezone: 'UTC', investigation_history_retention_days: 30 }, revision: 'settings-revision' });
    vi.spyOn(api, 'aiModels').mockResolvedValue([{
      id: 'model-primary', revision: 'rev-1', name: 'Primary', api_url: 'https://provider.example/v1', model: 'model-x', has_api_key: true,
    }]);
  });
  afterEach(() => vi.restoreAllMocks());

  it('saves the configured history retention and refreshes the conversation list', async () => {
    const updateSettings = vi.spyOn(api, 'updateSettings').mockResolvedValue({
      settings: { theme: 'system', default_timezone: 'UTC', investigation_history_retention_days: 90 },
      revision: 'next-settings-revision',
    });
    const conversations = vi.spyOn(api, 'aiConversations').mockResolvedValue([]);
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    const retention = await screen.findByLabelText('History retention days');
    expect(retention).toHaveValue(30);
    fireEvent.change(retention, { target: { value: '90' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save history retention' }));
    await screen.findByRole('status', { name: '' });
    await waitFor(() => expect(updateSettings).toHaveBeenCalledWith(
      { theme: 'system', default_timezone: 'UTC', investigation_history_retention_days: 90 },
      'settings-revision',
    ));
    expect(conversations).toHaveBeenCalledTimes(2);
    expect(await screen.findByText('History retention saved.')).toBeInTheDocument();
  });

  it('separates deterministic sampling coverage, aggregate totals and cited records', async () => {
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ result: {
      ...result, evidence_status: 'sample_supported', conclusions_verified: false,
      playbook_evidence: [{
        playbook_id: 'default-playbook', playbook_title: 'Default investigation playbook', step_name: 'Checkout errors',
        data_view_id: 'service-one', data_view_name: 'Service 1', index: 'service-one-*', query: 'service="checkout"',
        breakdown_field: 'service',
        current: { time_range_start_ms: 10_000, time_range_end_ms: 19_999, matched_records: 12, total_relation: 'eq', aggregate_records: 12, aggregations: [{ timestamp: 10_000, count: 12 }], histogram_unavailable_reason: null },
        baseline: { time_range_start_ms: 0, time_range_end_ms: 9_999, matched_records: 3, total_relation: 'eq', aggregate_records: 3, aggregations: [{ timestamp: 0, count: 3 }], histogram_unavailable_reason: null },
        error: null,
      }],
      sources: [{ ...result.sources[0], records_examined: 3, sample_records_available: 3, sample_limit: 8, aggregate_records: 102,
        evidence_queries: [{ sampling: 'latest', query_summary: 'Filtered checkout events; last 30 minutes', records_returned: 2, aggregate_records: 102 }, { sampling: 'earliest', query_summary: 'Filtered checkout events; last 30 minutes', records_returned: 2, aggregate_records: null }] }],
    } }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByText('3 sampled records')).toBeInTheDocument();
    expect(screen.getByText('2 bounded evidence queries')).toBeInTheDocument();
    expect(screen.getByText(/Service 2: 1 cited sample records/)).toBeInTheDocument();
    expect(screen.getByText(/not verified causes/)).toBeInTheDocument();
    fireEvent.click(screen.getByText('Investigation playbook queries (1)'));
    expect(screen.getByText('service="checkout"')).toBeInTheDocument();
    expect(screen.getByText('Current · 10000 to 19999 Unix ms')).toBeInTheDocument();
    expect(screen.getByText('Baseline · 0 to 9999 Unix ms')).toBeInTheDocument();
    fireEvent.click(screen.getByText('Sources and query scope'));
    expect(screen.getByText('102 records counted by query aggregation (not a finding count)')).toBeInTheDocument();
    expect(screen.getByText(/2\. earliest sample/)).toBeInTheDocument();
  });

  it('does not turn insufficient evidence into an incident draft or an all-clear', async () => {
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ result: { ...result, findings: [], summary: 'Insufficient sampled evidence.', evidence_status: 'insufficient_evidence', conclusions_verified: false } }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByText(/An empty sample does not establish/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Save full investigation as Knowledge draft' })).not.toBeInTheDocument();
  });

  it('renders chat-only replies without investigation evidence panels', async () => {
    const reply = 'I am here to help. What would you like to investigate?';
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ result: {
      ...result,
      response_mode: 'conversation',
      summary: reply,
      findings: [],
      sources: [],
      playbook_evidence: [],
      caveats: [],
      token_usage: [],
      evidence_status: 'not_required',
    } }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'fuck you' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByText(reply)).toBeInTheDocument();
    expect(screen.getByText('Response')).toBeInTheDocument();
    expect(screen.queryByText('Evidence coverage')).not.toBeInTheDocument();
    expect(screen.queryByText(/AI hypotheses, not verified causes/)).not.toBeInTheDocument();
    expect(screen.queryByRole('region', { name: 'Findings' })).not.toBeInTheDocument();
  });

  it('renders grouped query results and the executed SQL without hypothesis panels', async () => {
    const sql = "SELECT `login`, count() AS `_ig_duplicate_count` FROM `events` GROUP BY `login` HAVING count() >= 2 ORDER BY `_ig_duplicate_count` DESC LIMIT 51";
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ result: {
      ...result,
      response_mode: 'data_query',
      summary: 'Found 1 duplicate group for login in the requested scope.',
      findings: [],
      sources: [],
      playbook_evidence: [],
      caveats: [],
      token_usage: [],
      evidence_status: 'query_result',
      query_result: {
        data_view_id: 'login-view',
        data_view_name: 'Login events',
        group_by: ['login'],
        sql,
        columns: ['login', '_ig_duplicate_count'],
        rows: [{ login: '0000Blaze', _ig_duplicate_count: 2 }],
        row_count: 1,
        truncated: false,
      },
    } }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Find duplicate logins and give me SQL' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByText('Found 1 duplicate group for login in the requested scope.')).toBeInTheDocument();
    expect(screen.getByRole('region', { name: 'Executed data query' })).toBeInTheDocument();
    expect(screen.getByText(sql)).toBeInTheDocument();
    expect(screen.getByText('0000Blaze')).toBeInTheDocument();
    expect(screen.getByText('2', { selector: 'td' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Save full investigation as Knowledge draft' })).not.toBeInTheDocument();
    expect(screen.queryByText('Evidence coverage')).not.toBeInTheDocument();
    expect(screen.queryByText(/AI hypotheses, not verified causes/)).not.toBeInTheDocument();
  });

  it('explicitly saves completed findings as an unverified scoped knowledge draft', async () => {
    const completed = job();
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(completed);
    vi.spyOn(api, 'aiConversation').mockResolvedValue(conversationForResult(completed.id, 'Why did payments fail?', 'Why did payments fail?'));
    const save = vi.spyOn(api, 'addKnowledge').mockImplementation(async (body) => ({ ...body, id: 'draft-1', revision: 'draft-revision', created_at_ms: 1, updated_at_ms: 1 }));
    const open = vi.fn();
    render(<InvestiGenieAIPage dataViews={dataViews} onOpenKnowledge={open} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Why did payments fail?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    const button = await screen.findByRole('button', { name: 'Save full investigation as Knowledge draft' });
    expect(save).not.toHaveBeenCalled();
    fireEvent.click(button);
    await screen.findByRole('button', { name: 'Full investigation saved as draft' });
    const input = save.mock.calls[0][0];
    expect(input).toMatchObject({ title: 'Why did payments fail?', status: 'draft', kind: 'resolved_incident', data_view_ids: ['service-one', 'service-two'] });
    for (const text of ['Unverified', 'job-1', 'model-primary', result.summary, result.findings[0].detail, 'event-1', result.sources[0].query_summary, result.caveats[0], 'Not confirmed']) expect(input.content).toContain(text);
    expect(screen.getByRole('button', { name: 'Full investigation saved as draft' })).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: 'Review draft' }));
    expect(open).toHaveBeenCalledWith('draft-1');
    expect(save).toHaveBeenCalledOnce();
  });

  it('saves history results using the stored investigation reference and scope', async () => {
    vi.mocked(api.aiConversations).mockResolvedValue([{ id: 'chat-1', revision: 'rev-1', title: 'Saved case', updated_at_ms: 10 }]);
    vi.mocked(api.aiConversation).mockResolvedValue({ id: 'chat-1', revision: 'rev-1', title: 'Saved case', updated_at_ms: 10, created_at_ms: 5, model_id: 'original-model', data_view_ids: ['service-two'], messages: [{ role: 'user', content: 'Original case question' }, { role: 'assistant', content: result.summary, result }] });
    const save = vi.spyOn(api, 'addKnowledge').mockImplementation(async (body) => ({ ...body, id: 'draft-2', revision: 'r2', created_at_ms: 1, updated_at_ms: 1 }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('option', { name: 'Saved case' });
    fireEvent.change(screen.getByLabelText('AI conversation history'), { target: { value: 'chat-1' } });
    fireEvent.click(await screen.findByRole('button', { name: 'Save full investigation as Knowledge draft' }));
    await screen.findByRole('button', { name: 'Full investigation saved as draft' });
    expect(save.mock.calls[0][0]).toMatchObject({ title: 'Original case question', data_view_ids: ['service-two'], status: 'draft' });
    expect(save.mock.calls[0][0].content).toContain('chat-1');
    expect(save.mock.calls[0][0].content).toContain('original-model');
  });

  it('prevents duplicate draft requests while a save is pending', async () => {
    const completed = job();
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(completed);
    vi.spyOn(api, 'aiConversation').mockResolvedValue(conversationForResult(completed.id, 'Investigate errors', 'Investigate errors'));
    let finish!: () => void;
    const pending = new Promise<void>((resolve) => { finish = resolve; });
    const save = vi.spyOn(api, 'addKnowledge').mockImplementation(async (body) => { await pending; return { ...body, id: 'draft-pending', revision: 'r1', created_at_ms: 1, updated_at_ms: 1 }; });
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    const button = await screen.findByRole('button', { name: 'Save full investigation as Knowledge draft' });
    fireEvent.click(button); fireEvent.click(button);
    expect(screen.getByRole('button', { name: 'Saving full investigation…' })).toBeDisabled();
    expect(save).toHaveBeenCalledOnce();
    finish();
    await screen.findByRole('button', { name: 'Full investigation saved as draft' });
  });

  it('does not promote partial findings reopened from history', async () => {
    vi.mocked(api.aiConversations).mockResolvedValue([{ id: 'partial-case', revision: 'r1', title: 'Partial case', updated_at_ms: 2 }]);
    vi.mocked(api.aiConversation).mockResolvedValue({ id: 'partial-case', revision: 'r1', title: 'Partial case', created_at_ms: 1, updated_at_ms: 2, model_id: 'model-primary', data_view_ids: ['service-one'], messages: [{ role: 'user', content: 'Investigate errors' }, { role: 'assistant', content: result.summary, result: { ...result, complete: false } }] });
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('option', { name: 'Partial case' });
    fireEvent.change(screen.getByLabelText('AI conversation history'), { target: { value: 'partial-case' } });
    await screen.findByText(result.summary);
    expect(screen.queryByRole('button', { name: 'Save full investigation as Knowledge draft' })).not.toBeInTheDocument();
  });

  it('keeps results visible and allows retry after a draft save fails', async () => {
    const completed = job();
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(completed);
    vi.spyOn(api, 'aiConversation').mockResolvedValue(conversationForResult(completed.id, 'Investigate errors', 'Investigate errors'));
    const save = vi.spyOn(api, 'addKnowledge').mockRejectedValueOnce(new Error('Knowledge storage unavailable')).mockImplementationOnce(async (body) => ({ ...body, id: 'draft-3', revision: 'r3', created_at_ms: 1, updated_at_ms: 1 }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Save full investigation as Knowledge draft' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Knowledge storage unavailable');
    expect(screen.getByText(result.summary)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Save full investigation as Knowledge draft' }));
    await screen.findByRole('button', { name: 'Full investigation saved as draft' });
    expect(save).toHaveBeenCalledTimes(2);
  });

  it.each(['running', 'awaiting_decision', 'terminated', 'failed'] as const)('does not offer knowledge drafts for a %s investigation', async (status) => {
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ status, result: { ...result, complete: false } }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await screen.findByText(result.summary);
    expect(screen.queryByRole('button', { name: 'Save full investigation as Knowledge draft' })).not.toBeInTheDocument();
  });

  it.each(['\u{1f600}', '\u0000'])('bounds Unicode and escaped draft content by UTF-8 and request-body bytes (%j)', async (character) => {
    const oversizedResult = { ...result, summary: character.repeat(12_000) };
    const completed = job({ result: oversizedResult });
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(completed);
    vi.spyOn(api, 'aiConversation').mockResolvedValue(conversationForResult(completed.id, 'Investigate errors', 'Investigate errors', oversizedResult));
    const save = vi.spyOn(api, 'addKnowledge').mockImplementation(async (body) => ({ ...body, id: 'draft-4', revision: 'r4', created_at_ms: 1, updated_at_ms: 1 }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker(); fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: '\u{1f600}'.repeat(100) } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Save full investigation as Knowledge draft' }));
    await screen.findByRole('button', { name: 'Full investigation saved as draft' });
    const input = save.mock.calls[0][0];
    expect(new TextEncoder().encode(input.title).length).toBeLessThanOrEqual(256);
    expect(new TextEncoder().encode(input.content).length).toBeLessThanOrEqual(32_768);
    expect(new TextEncoder().encode(JSON.stringify(input)).length).toBeLessThanOrEqual(65_536);
    expect(input.content).toContain('Content was truncated');
    expect(input.content).not.toContain('\ufffd');
  });

  it('requires a selected model and views and submits their IDs with the question', async () => {
    const investigate = vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    expect(await screen.findByRole('combobox', { name: /AI model/ })).toHaveValue('model-primary');
    expect(screen.getByRole('button', { name: 'Run investigation' })).toBeDisabled();
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.click(screen.getByLabelText(/Service 2/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'What is impacting the services?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await waitFor(() => expect(investigate).toHaveBeenCalledWith('What is impacting the services?', ['service-one', 'service-two'], 'model-primary', { start: expect.any(Number), end: expect.any(Number) }, defaultControls));
    expect(await screen.findByText(result.summary)).toBeInTheDocument();
  });

  it('starts an investigation from a selected Explore document', async () => {
    const timeRange = { start: Date.parse('2026-10-22T10:00:00Z'), end: Date.parse('2026-10-22T10:15:00Z') };
    const investigate = vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    const consumed = vi.fn();
    render(<InvestiGenieAIPage
      dataViews={dataViews}
      initialDocument={{
        requestId: 'document-request-1',
        dataViewId: 'service-one',
        dataViewName: 'Service 1',
        index: 'service-one-2026.10.22',
        documentId: 'event-17',
        source: { message: 'gateway returned 503', service: 'checkout' },
        timeRange,
      }}
      onDocumentInvestigationConsumed={consumed}
    />);

    await waitFor(() => expect(investigate).toHaveBeenCalledOnce());
    const [prompt, selectedIds, modelId, range, controls] = investigate.mock.calls[0];
    expect(prompt).toContain('Document ID: event-17');
    expect(prompt).toContain('"message": "gateway returned 503"');
    expect(selectedIds).toEqual(['service-one']);
    expect(modelId).toBe('model-primary');
    expect(range).toEqual(timeRange);
    expect(controls).toEqual(defaultControls);
    expect(consumed).toHaveBeenCalledWith('document-request-1');
  });

  it('reopens a saved investigation and continues with its transcript', async () => {
    const summary = { id: 'chat-1', revision: 'revision-1', title: 'Why did checkout fail?', updated_at_ms: 10 };
    const conversation: AIConversation = {
      ...summary,
      model_id: 'model-primary',
      data_view_ids: ['service-one'],
      created_at_ms: 5,
      messages: [
        { role: 'user', content: 'Why did checkout fail?' },
        { role: 'assistant', content: result.summary, result },
      ],
    };
    const updated: AIConversation = {
      ...conversation,
      revision: 'revision-2',
      messages: [...conversation.messages, { role: 'user', content: 'What next?' }, { role: 'assistant', content: 'Check the database pool.' }],
    };
    vi.mocked(api.aiConversations).mockResolvedValue([summary]);
    vi.mocked(api.aiConversation).mockResolvedValue(conversation);
    const followUp = vi.spyOn(api, 'continueAIConversation').mockResolvedValue(updated);
    const remove = vi.spyOn(api, 'removeAIConversation').mockResolvedValue(undefined);
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    const history = await screen.findByRole('combobox', { name: 'AI conversation history' });
    await screen.findByRole('option', { name: 'Why did checkout fail?' });
    fireEvent.change(history, { target: { value: 'chat-1' } });
    expect(await screen.findByText(result.summary)).toBeInTheDocument();
    expect(screen.getAllByText(result.summary)).toHaveLength(1);
    expect(screen.getByText('medium confidence')).toBeInTheDocument();
    expect(screen.getByText('analysis: 360 total (300 prompt + 60 completion)')).toBeInTheDocument();
    expect(screen.getByText(result.caveats[0])).toBeInTheDocument();
    expect(screen.getByText('Sources and query scope')).toBeInTheDocument();
    expect(screen.getByText('Saved investigation scope')).toBeInTheDocument();
    expect(screen.getByText(/This saved conversation has no source scope/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Edit time window' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Choose Data Views' })).toBeDisabled();
    expect(screen.getByRole('combobox', { name: 'AI model' })).toBeDisabled();
    expect(screen.getAllByText('Why did checkout fail?')).toHaveLength(2);
    expect(screen.getByRole('button', { name: 'Delete conversation' })).toBeEnabled();
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'What next?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send follow-up' }));
    expect(await screen.findByText('Check the database pool.')).toBeInTheDocument();
    expect(followUp).toHaveBeenCalledWith('chat-1', 'What next?', 'revision-1');
    fireEvent.click(screen.getByRole('button', { name: 'Delete conversation' }));
    expect(remove).toHaveBeenCalledWith('chat-1', 'revision-2');
    expect(await screen.findByRole('heading', { name: 'Start a new investigation' })).toBeInTheDocument();
    expect(screen.queryByText('Check the database pool.')).not.toBeInTheDocument();
  });

  it('continues a scoped conversation with new evidence and saves the whole session', async () => {
    const firstResult: InvestigationResult = { ...result, summary: 'Initial evidence: checkout returned 503.' };
    const secondResult: InvestigationResult = { ...result, summary: 'New evidence: level=error records confirm the 503 burst.' };
    const firstConversation: AIConversation = {
      id: 'ongoing-case', revision: 'revision-1', title: 'Checkout outage', model_id: 'model-primary',
      data_view_ids: ['service-one'], investigation_scope: { time_range: { start: 100, end: 200 } },
      created_at_ms: 1, updated_at_ms: 2,
      messages: [
        { role: 'user', content: 'Why did checkout fail?' },
        { role: 'assistant', content: firstResult.summary, result: firstResult },
      ],
    };
    const followUpMessage = 'I found level=error records; check whether they match the outage.';
    const continuedConversation: AIConversation = {
      ...firstConversation,
      revision: 'revision-3',
      updated_at_ms: 3,
      messages: [
        ...firstConversation.messages,
        { role: 'user', content: followUpMessage },
        { role: 'assistant', content: secondResult.summary, result: secondResult },
      ],
    };
    vi.mocked(api.aiConversations).mockResolvedValue([{ id: firstConversation.id, revision: firstConversation.revision, title: firstConversation.title, updated_at_ms: firstConversation.updated_at_ms }]);
    vi.spyOn(api, 'aiConversation')
      .mockResolvedValueOnce(firstConversation)
      .mockResolvedValueOnce(continuedConversation);
    const continueInvestigation = vi.spyOn(api, 'continueAIInvestigation').mockResolvedValue({
      conversation: {
        ...firstConversation,
        revision: 'revision-2',
        messages: [...firstConversation.messages, { role: 'user', content: followUpMessage }],
      },
      investigation: job({ id: 'turn-job', conversation_id: firstConversation.id, result: secondResult }),
    });
    const saveDraft = vi.spyOn(api, 'addKnowledge').mockImplementation(async (input) => ({
      ...input, id: 'session-draft', revision: 'draft-revision', created_at_ms: 1, updated_at_ms: 1,
    }));

    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('option', { name: 'Checkout outage' });
    fireEvent.change(screen.getByLabelText('AI conversation history'), { target: { value: firstConversation.id } });
    expect(await screen.findByText(firstResult.summary)).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: followUpMessage } });
    fireEvent.click(screen.getByRole('button', { name: 'Send follow-up' }));
    await waitFor(() => expect(continueInvestigation).toHaveBeenCalledWith(
      firstConversation.id,
      followUpMessage,
      'revision-1',
      defaultControls,
    ));
    expect(await screen.findByText(secondResult.summary)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Save full investigation as Knowledge draft' }));
    await waitFor(() => expect(saveDraft).toHaveBeenCalledTimes(1));
    const saved = saveDraft.mock.calls[0][0];
    expect(saved.content).toContain('Why did checkout fail?');
    expect(saved.content).toContain(followUpMessage);
    expect(saved.content).toContain(firstResult.summary);
    expect(saved.content).toContain(secondResult.summary);
  });

  it('renders findings, confidence, evidence and bounded sources', async () => {
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByRole('heading', { name: 'Correlated timeout increase' })).toBeInTheDocument();
    expect(screen.getByText('medium confidence')).toBeInTheDocument();
    expect(screen.getByText('planning: 120 total (100 prompt + 20 completion)')).toBeInTheDocument();
    expect(screen.getByText('analysis: 360 total (300 prompt + 60 completion)')).toBeInTheDocument();
    fireEvent.click(screen.getByText('Sources and query scope'));
    expect(screen.getAllByText(/Last 30 minutes; error_type EQ/)).toHaveLength(2);
    expect(screen.getByText(/Analysis is limited to the last 30 minutes/)).toBeInTheDocument();
  });

  it('shows the adaptive follow-up query and its reason in source details', async () => {
    const adaptiveResult: InvestigationResult = {
      ...result,
      sources: result.sources.map((source) => ({
        ...source,
        evidence_queries: [...(source.evidence_queries ?? []), {
          sampling: 'adaptive_follow_up',
          query_summary: 'Last 30 minutes; level EQ error',
          records_returned: 1,
          aggregate_records: null,
          reason: 'Check whether the errors coincide with error-level records.',
        }],
      })),
    };
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ result: adaptiveResult }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    fireEvent.click(await screen.findByText('Sources and query scope'));
    expect(screen.getByText(/adaptive follow-up query.*Check whether the errors coincide/)).toBeInTheDocument();
  });

  it('shows provider setup errors without losing the question', async () => {
    vi.spyOn(api, 'startInvestigation').mockRejectedValue(new Error('InvestiGenie AI is not configured'));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Investigate errors' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('InvestiGenie AI is not configured');
    expect(screen.getByLabelText('Ask InvestiGenie AI')).toHaveValue('Investigate errors');
  });

  it('allows a maximum of five views', () => {
    const views = Array.from({ length: 6 }, (_, index) => ({
      ...dataViews[0], id: `view-${index}`, name: `View ${index}`,
    }));
    render(<InvestiGenieAIPage dataViews={views} />);
    openDataViewPicker();
    for (let index = 0; index < 5; index++) fireEvent.click(screen.getByLabelText(new RegExp(`View ${index}`)));
    expect(screen.getByRole('button', { name: 'Choose Data Views' })).toHaveTextContent('5/5');
    expect(screen.getByLabelText(/View 5/)).toBeDisabled();
  });

  it('submits with Enter while Shift+Enter keeps the composer open for multiline text', async () => {
    const investigate = vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: /AI model/ });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    const composer = screen.getByLabelText('Ask InvestiGenie AI');
    fireEvent.change(composer, { target: { value: 'Investigate the outage' } });
    fireEvent.keyDown(composer, { key: 'Enter', shiftKey: true });
    expect(investigate).not.toHaveBeenCalled();
    fireEvent.keyDown(composer, { key: 'Enter', isComposing: true });
    expect(investigate).not.toHaveBeenCalled();
    fireEvent.keyDown(composer, { key: 'Enter', shiftKey: false });
    await waitFor(() => expect(investigate).toHaveBeenCalledOnce());
  });

  it('closes the Data View dropdown with Escape and restores trigger focus', () => {
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    const trigger = screen.getByRole('button', { name: 'Choose Data Views' });
    fireEvent.click(trigger);
    const search = screen.getByLabelText('Search Data Views');
    expect(search).toHaveFocus();
    fireEvent.keyDown(search, { key: 'Escape' });
    expect(screen.queryByRole('dialog', { name: 'Select Data Views' })).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it('searches large Data View lists, bounds rendered matches, and keeps selected chips removable', () => {
    const views = Array.from({ length: 120 }, (_, index) => ({
      ...dataViews[0], id: `view-${index}`, name: `Service ${index}`, index: `logs-${index}`,
    }));
    render(<InvestiGenieAIPage dataViews={views} />);
    openDataViewPicker();
    fireEvent.change(screen.getByLabelText('Search Data Views'), { target: { value: 'Service 10' } });
    expect(screen.getByText('11 matches')).toBeInTheDocument();
    expect(document.querySelectorAll('.ai-source-list .ai-source')).toHaveLength(11);
    fireEvent.click(screen.getByLabelText(/Service 101/));
    expect(screen.getByLabelText('Remove Service 101')).toBeInTheDocument();
    fireEvent.click(screen.getByLabelText('Remove Service 101'));
    expect(screen.queryByLabelText('Remove Service 101')).not.toBeInTheDocument();
  });

  it('shows an empty-state hint when no data views exist', () => {
    render(<InvestiGenieAIPage dataViews={[]} />);
    expect(screen.getByText('Create a Data View before starting an investigation.')).toBeInTheDocument();
    expect(screen.getByLabelText('Ask InvestiGenie AI')).toBeDisabled();
  });

  it('uses the Explore timeframe picker and keeps the model below the chat', async () => {
    const investigate = vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    const model = await screen.findByRole('combobox', { name: 'AI model' });
    expect(model.closest('.ai-composer-footer')).not.toBeNull();
    expect(screen.getByRole('heading', { name: 'Start a new investigation' })).toBeInTheDocument();
    expect(screen.queryByText('Investigate your data')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    fireEvent.change(screen.getByLabelText('Start date'), { target: { value: '2026-10-06' } });
    fireEvent.change(screen.getByLabelText('Start time'), { target: { value: '08:00:00' } });
    fireEvent.change(screen.getByLabelText('End date'), { target: { value: '2026-10-06' } });
    fireEvent.change(screen.getByLabelText('End time'), { target: { value: '09:00:00' } });
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Historical errors?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await waitFor(() => expect(investigate).toHaveBeenCalledWith('Historical errors?', ['service-one'], 'model-primary', {
      start: Date.parse('2026-10-06T08:00:00Z'), end: Date.parse('2026-10-06T09:00:00Z'),
    }, defaultControls));
  });

  it('sends an explicit all-time scope when the time window is cleared', async () => {
    const investigate = vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: 'AI model' });
    fireEvent.click(screen.getByRole('button', { name: 'Clear time window' }));
    expect(screen.getByText('All time')).toBeInTheDocument();
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Any errors?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await waitFor(() => expect(investigate).toHaveBeenCalledWith('Any errors?', ['service-one'], 'model-primary', null, defaultControls));
  });

  it('sends saved timeout and check-in controls with the investigation', async () => {
    const investigate = vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: 'AI model' });
    fireEvent.change(screen.getByLabelText('AI response timeout'), { target: { value: '300' } });
    fireEvent.change(screen.getByLabelText('Ask to continue after'), { target: { value: '600' } });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Large index?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await waitFor(() => expect(investigate).toHaveBeenCalledWith('Large index?', ['service-one'], 'model-primary', expect.anything(), { ai_timeout_seconds: 300, time_budget_seconds: 600, adaptive_followup: false }));
    expect(getPreference('ai-timeout')).toBe('300');
    expect(getPreference('ai-check-in')).toBe('600');
  });

  it('keeps adaptive follow-up off by default and sends the opt-in flag', async () => {
    const investigate = vi.spyOn(api, 'startInvestigation').mockResolvedValue(job());
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: 'AI model' });
    const adaptive = screen.getByRole('checkbox', { name: 'Allow one adaptive follow-up query' });
    expect(adaptive).not.toBeChecked();
    fireEvent.click(adaptive);
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Why are requests failing?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await waitFor(() => expect(investigate).toHaveBeenCalledWith(
      'Why are requests failing?',
      ['service-one'],
      'model-primary',
      expect.anything(),
      { ...defaultControls, adaptive_followup: true },
    ));
  });

  it('asks whether to continue, shows findings so far, and resumes polling until completion', async () => {
    const partial = { ...result, summary: 'Early timeout signals.', complete: false };
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ status: 'running', result: null, progress: { stage: 'Scanning Service 1', records_examined: 100, batches_processed: 1, analysis_cycles: 0, elapsed_ms: 1_000 } }));
    const poll = vi.spyOn(api, 'investigation')
      .mockResolvedValueOnce(job({ status: 'awaiting_decision', result: partial, pause_reason: 'The investigation has run for 2 minutes without finishing.' }))
      .mockResolvedValueOnce(job());
    const decide = vi.spyOn(api, 'decideInvestigation').mockResolvedValue(job({ status: 'running', result: partial }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: 'AI model' });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Why so slow?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByText('Scanning Service 1')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Stop/ })).toBeInTheDocument();
    const decision = await screen.findByRole('group', { name: 'Continue or stop the investigation' }, { timeout: 3_000 });
    expect(decision).toHaveTextContent('run for 2 minutes');
    expect(screen.getByText('Findings so far')).toBeInTheDocument();
    expect(screen.getByText('Early timeout signals.')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Continue investigation/ }));
    await waitFor(() => expect(decide).toHaveBeenCalledWith('job-1', 'continue'));
    expect(await screen.findByText(result.summary, undefined, { timeout: 3_000 })).toBeInTheDocument();
    expect(poll).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole('group', { name: 'Continue or stop the investigation' })).not.toBeInTheDocument();
  });

  it('stops a paused investigation and labels the findings as partial', async () => {
    const partial = { ...result, complete: false };
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ status: 'awaiting_decision', result: partial, pause_reason: 'The AI provider did not respond within the configured timeout.' }));
    const decide = vi.spyOn(api, 'decideInvestigation').mockResolvedValue(job({ status: 'terminated', result: partial }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: 'AI model' });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Errors?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByText(/did not respond within the configured timeout/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Stop and show findings/ }));
    await waitFor(() => expect(decide).toHaveBeenCalledWith('job-1', 'terminate'));
    expect(await screen.findByText('Partial investigation')).toBeInTheDocument();
    expect(screen.getByLabelText('Ask InvestiGenie AI')).not.toBeDisabled();
  });

  it('reports failed background investigations', async () => {
    vi.spyOn(api, 'startInvestigation').mockResolvedValue(job({ status: 'running', result: null }));
    vi.spyOn(api, 'investigation').mockResolvedValue(job({ status: 'failed', result: null, error: { code: 'source_query_failed', message: 'A batched source query failed' } }));
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: 'AI model' });
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Errors?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    expect(await screen.findByRole('alert', undefined, { timeout: 3_000 })).toHaveTextContent('A batched source query failed');
  });

  it('links to model management and disables investigations when no profile exists', async () => {
    vi.mocked(api.aiModels).mockResolvedValue([]);
    const onManageModels = vi.fn();
    render(<InvestiGenieAIPage dataViews={dataViews} onManageModels={onManageModels} />);
    expect(await screen.findByText('No saved AI models')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Open AI model management' }));
    expect(onManageModels).toHaveBeenCalledOnce();
    expect(screen.getByLabelText('Ask InvestiGenie AI')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Run investigation' })).toBeDisabled();
  });
});
