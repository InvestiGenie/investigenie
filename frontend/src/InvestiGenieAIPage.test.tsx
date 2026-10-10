import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import InvestiGenieAIPage from './InvestiGenieAIPage';
import { api, type InvestigationResult } from './api';
import { clearPreference } from './preferences';

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
    evidence: [{ data_view_id: 'service-two', data_view_name: 'Service 2', record_ids: ['event-1'], count: 12 }],
  }],
  sources: [{ data_view_id: 'service-two', name: 'Service 2', index: 'service-two-*', records_examined: 12, query_summary: 'Last 30 minutes; error_type EQ' }],
  caveats: ['Analysis is limited to the last 30 minutes.'],
  token_usage: [
    { stage: 'planning', prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 },
    { stage: 'analysis', prompt_tokens: 300, completion_tokens: 60, total_tokens: 360 },
  ],
};

function openDataViewPicker() {
  fireEvent.click(screen.getByRole('button', { name: 'Choose Data Views' }));
}

describe('InvestiGenieAIPage', () => {
  beforeEach(() => {
    clearPreference('ai-model');
    vi.spyOn(api, 'aiModels').mockResolvedValue([{
      id: 'model-primary', revision: 'rev-1', name: 'Primary', api_url: 'https://provider.example/v1', model: 'model-x', has_api_key: true,
    }]);
  });
  afterEach(() => vi.restoreAllMocks());

  it('requires a selected model and views and submits their IDs with the question', async () => {
    const investigate = vi.spyOn(api, 'investigate').mockResolvedValue(result);
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    expect(await screen.findByRole('combobox', { name: /AI model/ })).toHaveValue('model-primary');
    expect(screen.getByRole('button', { name: 'Run investigation' })).toBeDisabled();
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.click(screen.getByLabelText(/Service 2/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'What is impacting the services?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await waitFor(() => expect(investigate).toHaveBeenCalledWith('What is impacting the services?', ['service-one', 'service-two'], 'model-primary', { start: expect.any(Number), end: expect.any(Number) }));
    expect(await screen.findByText(result.summary)).toBeInTheDocument();
  });

  it('renders findings, confidence, evidence and bounded sources', async () => {
    vi.spyOn(api, 'investigate').mockResolvedValue(result);
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
    expect(screen.getByText(/Last 30 minutes; error_type EQ/)).toBeInTheDocument();
    expect(screen.getByText(/Analysis is limited to the last 30 minutes/)).toBeInTheDocument();
  });

  it('shows provider setup errors without losing the question', async () => {
    vi.spyOn(api, 'investigate').mockRejectedValue(new Error('InvestiGenie AI is not configured'));
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
    const investigate = vi.spyOn(api, 'investigate').mockResolvedValue(result);
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
    const investigate = vi.spyOn(api, 'investigate').mockResolvedValue(result);
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
    }));
  });

  it('sends an explicit all-time scope when the time window is cleared', async () => {
    const investigate = vi.spyOn(api, 'investigate').mockResolvedValue(result);
    render(<InvestiGenieAIPage dataViews={dataViews} />);
    await screen.findByRole('combobox', { name: 'AI model' });
    fireEvent.click(screen.getByRole('button', { name: 'Clear time window' }));
    expect(screen.getByText('All time')).toBeInTheDocument();
    openDataViewPicker();
    fireEvent.click(screen.getByLabelText(/Service 1/));
    fireEvent.change(screen.getByLabelText('Ask InvestiGenie AI'), { target: { value: 'Any errors?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Run investigation' }));
    await waitFor(() => expect(investigate).toHaveBeenCalledWith('Any errors?', ['service-one'], 'model-primary', null));
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