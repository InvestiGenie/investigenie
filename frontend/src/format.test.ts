import { describe, expect, it } from 'vitest';
import { errorMessage, formatCount } from './format';

describe('shared UI formatting', () => {
  it('preserves error messages and caller-specific fallbacks', () => {
    expect(errorMessage(new Error('Conflict'))).toBe('Conflict');
    expect(errorMessage(null)).toBe('An unexpected request error occurred');
    expect(errorMessage('unknown', 'Could not load data')).toBe('Could not load data');
  });

  it('formats ordinary and compact counts without changing unavailable values', () => {
    expect(formatCount('24000')).toBe(new Intl.NumberFormat().format(24000));
    expect(formatCount('24000', { notation: 'compact' })).toBe(new Intl.NumberFormat(undefined, { notation: 'compact' }).format(24000));
    expect(formatCount('0')).toBe('0');
    expect(formatCount(null)).toBe('\u2014');
    expect(formatCount(undefined)).toBe('\u2014');
    expect(formatCount('unknown')).toBe('unknown');
  });
});
