import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it } from 'vitest';
import DateTimeInput from './DateTimeInput';

describe('DateTimeInput', () => {
  function Field() {
    const [value, setValue] = useState('2026-10-08T15:41:53');
    return <DateTimeInput label="Start" value={value} timeZone="UTC" onChange={setValue} />;
  }

  it('uses separate text date and 24-hour time fields without native datetime UI', () => {
    render(<Field />);
    expect(screen.getByLabelText('Start date')).toHaveValue('2026-10-08');
    expect(screen.getByLabelText('Start time')).toHaveValue('15:41:53');
    expect(screen.getByLabelText('Start date')).toHaveAttribute('type', 'text');
    fireEvent.change(screen.getByLabelText('Start time'), { target: { value: '08:18:00' } });
    expect(screen.getByLabelText('Start time')).toHaveValue('08:18:00');
  });

  it('selects a calendar day while preserving the time and closes the calendar', () => {
    render(<Field />);
    fireEvent.click(screen.getByRole('button', { name: 'Choose start date' }));
    expect(screen.getByRole('group', { name: 'Start calendar' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Friday, October 9/ }));
    expect(screen.getByLabelText('Start date')).toHaveValue('2026-10-09');
    expect(screen.getByLabelText('Start time')).toHaveValue('15:41:53');
    expect(screen.queryByRole('group', { name: 'Start calendar' })).not.toBeInTheDocument();
  });
});
