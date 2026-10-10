import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { useState } from 'react';
import TimeRangePicker from './TimeRangePicker';

describe('TimeRangePicker', () => {
  it('changes displayed drafts without shifting the applied UTC window', () => {
    const change = vi.fn();
    function Picker() {
      const [zone, setZone] = useState('UTC');
      return <TimeRangePicker start="2026-10-06T08:18:00" end="2026-10-06T09:18:00" timeZone={zone} onTimeZoneChange={setZone} onChange={change} />;
    }
    render(<Picker />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    fireEvent.change(screen.getByLabelText('Timezone'), { target: { value: 'Europe/Berlin' } });
    expect(screen.getByLabelText('Start date')).toHaveValue('2026-10-06');
    expect(screen.getByLabelText('Start time')).toHaveValue('10:18:00');
    expect(screen.getByLabelText('End time')).toHaveValue('11:18:00');
    expect(change).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(change).toHaveBeenCalledWith('2026-10-06T08:18:00', '2026-10-06T09:18:00');
  });

  it('shows a compact summary and discards cancelled drafts', () => {
    const change = vi.fn();
    render(<TimeRangePicker start="" end="" onChange={change} />);
    expect(screen.getByText('All time')).toBeInTheDocument();
    expect(screen.queryByLabelText('Start date')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    fireEvent.change(screen.getByLabelText('Start date'), { target: { value: '2026-10-06' } });
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(change).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    expect(screen.getByLabelText('Start date')).toHaveValue('');
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('validates dates and applies preset windows in UTC', () => {
    const change = vi.fn();
    render(<TimeRangePicker start="" end="" onChange={change} />);
    fireEvent.click(screen.getByRole('button', { name: 'Edit time window' }));
    fireEvent.change(screen.getByLabelText('Start date'), { target: { value: '2026-10-06' } });
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(screen.getByRole('alert')).toHaveTextContent('Choose both dates');
    expect(change).not.toHaveBeenCalled();
    const now = vi.spyOn(Date, 'now').mockReturnValue(Date.parse('2026-10-06T09:00:00Z'));
    try {
      fireEvent.change(screen.getByLabelText('Time window preset'), { target: { value: '60' } });
      expect(screen.getByLabelText('Start date')).toHaveValue('2026-10-06');
      expect(screen.getByLabelText('Start time')).toHaveValue('08:00:00');
      expect(screen.getByLabelText('End time')).toHaveValue('09:00:00');
      fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
      expect(change).toHaveBeenCalledWith('2026-10-06T08:00:00', '2026-10-06T09:00:00');
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    } finally { now.mockRestore(); }
  });
});