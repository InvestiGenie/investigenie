import { describe, expect, it } from 'vitest';
import { formatTimestamp, inputToUtc, utcToInput, validTimeZone } from './time';

describe('shared timezone conversion', () => {
  it('converts inputs and displays the same instant across UTC and UTC+2', () => {
    expect(inputToUtc('2026-10-06T10:18:00', 'Europe/Berlin')).toBe('2026-10-06T08:18:00');
    expect(utcToInput('2026-10-06T08:18:00', 'Europe/Berlin')).toBe('2026-10-06T10:18:00');
    expect(formatTimestamp('2026-10-06T08:18:00Z', 'Europe/Berlin')).toBe('2026-10-06 10:18:00');
    expect(formatTimestamp('2026-10-06 08:18:00', 'Europe/Berlin')).toBe('2026-10-06 10:18:00');
    expect(formatTimestamp('2026-10-06T10:18:00+02:00', 'UTC')).toBe('2026-10-06 08:18:00');
  });

  it('rejects daylight-saving skipped and repeated wall times instead of shifting them', () => {
    expect(() => inputToUtc('2026-03-29T02:30:00', 'Europe/Berlin')).toThrow('Daylight-saving');
    expect(() => inputToUtc('2026-10-25T02:30:00', 'Europe/Berlin')).toThrow('Daylight-saving');
  });

  it('retains fractional seconds when changing timezone', () => {
    const original = '2026-10-06T08:18:00.123';
    expect(inputToUtc(utcToInput(original, 'Europe/Berlin'), 'Europe/Berlin')).toBe(original);
  });

  it('preserves missing and non-date values and validates timezone preferences', () => {
    expect(formatTimestamp(null, 'UTC')).toBeNull();
    expect(formatTimestamp('not a date', 'UTC')).toBeNull();
    expect(validTimeZone('UTC')).toBe(true);
    expect(validTimeZone('invalid-zone')).toBe(false);
  });
});
