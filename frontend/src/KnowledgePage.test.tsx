import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import KnowledgePage from './KnowledgePage';
import { api, type KnowledgeEntry } from './api';

const entry: KnowledgeEntry = {
  id: 'entry-one', revision: 'revision-1', title: 'Database recovery', kind: 'runbook', status: 'draft',
  content: '# Recovery\n\nCheck the **connection pool**.', services: ['payments'], environments: ['production'],
  tags: ['database'], source_url: null, connection_ids: [], data_view_ids: [], created_at_ms: 1_700_000_000_000, updated_at_ms: 1_700_000_000_000,
};

describe('KnowledgePage', () => {
  beforeEach(() => { vi.spyOn(api, 'knowledgeEntries').mockResolvedValue([entry]); });
  afterEach(() => vi.restoreAllMocks());

  it('opens an explicitly requested saved draft for review without saving again', async () => {
    const update = vi.spyOn(api, 'updateKnowledge');
    render(<KnowledgePage initialEntryId={entry.id} />);
    expect(await screen.findByLabelText('Title')).toHaveValue(entry.title);
    expect(screen.getByLabelText('Content')).toHaveValue(entry.content);
    expect(screen.getByLabelText('Status')).toHaveValue('draft');
    expect(update).not.toHaveBeenCalled();
  });

  it('reports a missing requested draft without opening a blank editor', async () => {
    render(<KnowledgePage initialEntryId="deleted-entry" />);
    expect(await screen.findByRole('alert')).toHaveTextContent('no longer available');
    expect(screen.queryByLabelText('Title')).not.toBeInTheDocument();
  });

  it('searches content and filters type, service and status', async () => {
    render(<KnowledgePage />);
    await screen.findByText(entry.title);
    fireEvent.change(screen.getByLabelText('Search knowledge'), { target: { value: 'connection pool' } });
    expect(screen.getByText(entry.title)).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Filter by status'), { target: { value: 'published' } });
    expect(screen.getByText('No matching entries')).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Filter by status'), { target: { value: '' } });
    fireEvent.change(screen.getByLabelText('Filter by type'), { target: { value: 'runbook' } });
    fireEvent.change(screen.getByLabelText('Filter by service'), { target: { value: 'payments' } });
    fireEvent.change(screen.getByLabelText('Filter by environment'), { target: { value: 'production' } });
    expect(screen.getByText(entry.title)).toBeInTheDocument();
  });

  it('creates a draft and previews Markdown without HTML or tracking images', async () => {
    const add = vi.spyOn(api, 'addKnowledge').mockResolvedValue(entry);
    render(<KnowledgePage />);
    await screen.findByText(entry.title);
    fireEvent.click(screen.getByRole('button', { name: 'New entry' }));
    fireEvent.change(screen.getByLabelText('Title'), { target: { value: entry.title } });
    fireEvent.change(screen.getByLabelText('Services'), { target: { value: 'payments, checkout, payments' } });
    fireEvent.change(screen.getByLabelText('Content'), { target: { value: '# Safe preview\n\n<script>alert(1)</script>\n\n![tracker](https://example.com/track.png)\n\n[unsafe](javascript:alert(1))' } });
    fireEvent.click(screen.getByRole('tab', { name: 'Preview' }));
    expect(screen.getByRole('heading', { name: 'Safe preview' })).toBeInTheDocument();
    expect(document.querySelector('script')).toBeNull();
    expect(document.querySelector('img')).toBeNull();
    expect(screen.queryByRole('link', { name: 'unsafe' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Save entry' }));
    await waitFor(() => expect(add).toHaveBeenCalledWith(expect.objectContaining({ status: 'draft', services: ['payments', 'checkout'] })));
    expect(await screen.findByText('Knowledge entry saved.')).toBeInTheDocument();
  });

  it('publishes, archives and restores using each returned revision', async () => {
    const update = vi.spyOn(api, 'updateKnowledge')
      .mockResolvedValueOnce({ ...entry, status: 'published', revision: 'revision-2' })
      .mockResolvedValueOnce({ ...entry, status: 'archived', revision: 'revision-3' })
      .mockResolvedValueOnce({ ...entry, status: 'draft', revision: 'revision-4' });
    render(<KnowledgePage />);
    fireEvent.click(await screen.findByText(entry.title));
    fireEvent.click(screen.getByRole('button', { name: 'Publish' }));
    await screen.findByRole('button', { name: 'Return to draft' });
    expect(update).toHaveBeenLastCalledWith(entry.id, expect.objectContaining({ status: 'published' }), 'revision-1');
    fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Restore to draft' }));
    await screen.findByRole('button', { name: 'Publish' });
    expect(update).toHaveBeenLastCalledWith(entry.id, expect.objectContaining({ status: 'draft' }), 'revision-3');
  });

  it('preserves unsaved content on conflicts and never retries stale writes', async () => {
    const update = vi.spyOn(api, 'updateKnowledge').mockRejectedValue(new Error('Configuration changed; reload before editing'));
    render(<KnowledgePage />);
    fireEvent.click(await screen.findByText(entry.title));
    fireEvent.click(screen.getByRole('button', { name: 'Edit entry' }));
    fireEvent.change(screen.getByLabelText('Content'), { target: { value: 'My unsaved recovery notes' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save entry' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Configuration changed');
    expect(screen.getByLabelText('Content')).toHaveValue('My unsaved recovery notes');
    expect(update).toHaveBeenCalledOnce();
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false);
    fireEvent.click(screen.getByRole('button', { name: 'New entry' }));
    expect(confirm).toHaveBeenCalled();
    expect(screen.getByLabelText('Content')).toHaveValue('My unsaved recovery notes');
  });

  it('requires confirmation before permanent deletion', async () => {
    const remove = vi.spyOn(api, 'removeKnowledge').mockResolvedValue();
    const confirm = vi.spyOn(window, 'confirm').mockReturnValueOnce(false).mockReturnValueOnce(true);
    render(<KnowledgePage />);
    fireEvent.click(await screen.findByText(entry.title));
    fireEvent.click(screen.getByRole('button', { name: 'Delete entry permanently' }));
    expect(remove).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Delete entry permanently' }));
    await waitFor(() => expect(remove).toHaveBeenCalledWith(entry.id, entry.revision));
    expect(confirm).toHaveBeenCalledWith(expect.stringContaining('cannot be undone'));
    expect(await screen.findByText('No knowledge entries')).toBeInTheDocument();
  });

  it('reports load failures and can retry', async () => {
    vi.mocked(api.knowledgeEntries).mockRejectedValueOnce(new Error('Storage unavailable')).mockResolvedValueOnce([]);
    render(<KnowledgePage />);
    expect(await screen.findByRole('alert')).toHaveTextContent('Storage unavailable');
    fireEvent.click(screen.getByRole('button', { name: /retry/i }));
    expect(await screen.findByText('No knowledge entries')).toBeInTheDocument();
  });
});
