import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import AIModelsPage from './AIModelsPage';
import { api, type AIModelSummary } from './api';

const profile: AIModelSummary = {
  id: 'model-one', revision: 'rev-7', name: 'Primary', api_url: 'https://provider.example/v1', model: 'model-x', has_api_key: true,
};

describe('AIModelsPage', () => {
  afterEach(() => vi.restoreAllMocks());

  it('shows an empty state and opens the provider endpoint form', async () => {
    vi.spyOn(api, 'aiModels').mockResolvedValue([]);
    render(<AIModelsPage />);
    expect(await screen.findByText('No AI models configured')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Add AI model' }));
    expect(await screen.findByRole('heading', { name: 'Add AI model' })).toBeInTheDocument();
    expect(screen.getByLabelText('Provider API endpoint')).toHaveAttribute('type', 'url');
    expect(screen.getByLabelText('API key')).toHaveAttribute('type', 'password');
  });

  it('creates a profile with a key and reloads only key-free summaries', async () => {
    vi.spyOn(api, 'aiModels').mockResolvedValueOnce([]).mockResolvedValueOnce([profile]);
    const add = vi.spyOn(api, 'addAIModel').mockResolvedValue(profile);
    render(<AIModelsPage />);
    fireEvent.click(await screen.findByRole('button', { name: 'Add AI model' }));
    fireEvent.change(await screen.findByLabelText('Profile name'), { target: { value: 'Primary' } });
    fireEvent.change(screen.getByLabelText('Provider API endpoint'), { target: { value: profile.api_url } });
    fireEvent.change(screen.getByLabelText('Model identifier'), { target: { value: profile.model } });
    fireEvent.change(screen.getByLabelText('API key'), { target: { value: 'secret-value' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test and save' }));
    await waitFor(() => expect(add).toHaveBeenCalledWith({ name: 'Primary', api_url: profile.api_url, model: profile.model, api_key: 'secret-value' }));
    expect(await screen.findByText('API key saved')).toBeInTheDocument();
    expect(screen.queryByText('secret-value')).not.toBeInTheDocument();
  });

  it('retains an existing key by omitting it and allows an explicitly checked replacement', async () => {
    vi.spyOn(api, 'aiModels').mockResolvedValue([profile]);
    const update = vi.spyOn(api, 'updateAIModel').mockResolvedValue(profile);
    render(<AIModelsPage />);
    const row = await screen.findByText(/provider\.example/);
    fireEvent.click(within(row.closest('article') as HTMLElement).getByRole('button', { name: 'Edit Primary' }));
    expect(screen.queryByLabelText('API key')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Test and save changes' }));
    await waitFor(() => expect(update).toHaveBeenLastCalledWith('model-one', {
      name: 'Primary', api_url: profile.api_url, model: profile.model,
    }, 'rev-7'));
    fireEvent.click(await screen.findByRole('button', { name: 'Edit Primary' }));
    fireEvent.click(screen.getByLabelText('Replace API key'));
    fireEvent.change(screen.getByLabelText('API key'), { target: { value: 'new-secret' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test and save changes' }));
    await waitFor(() => expect(update).toHaveBeenLastCalledWith('model-one', {
      name: 'Primary', api_url: profile.api_url, model: profile.model, api_key: 'new-secret',
    }, 'rev-7'));
  });

  it('uses the listed revision for delete and shows conflicts without retry', async () => {
    vi.spyOn(api, 'aiModels').mockResolvedValue([profile]);
    const remove = vi.spyOn(api, 'removeAIModel').mockRejectedValue(new Error('Profile revision is stale. Reload before trying again.'));
    render(<AIModelsPage />);
    const modelUrl = await screen.findByText(/provider\.example/);
    fireEvent.click(within(modelUrl.closest('article') as HTMLElement).getByRole('button', { name: 'Remove Primary' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Profile revision is stale. Reload before trying again.');
    expect(remove).toHaveBeenCalledWith('model-one', 'rev-7');
    expect(remove).toHaveBeenCalledOnce();
  });
});
