import { Temporal } from '@js-temporal/polyfill';
import { getPreference } from './preferences';

export function validTimeZone(zone: string): boolean {
  try {
    new Intl.DateTimeFormat(undefined, { timeZone: zone });
    return true;
  } catch { return false; }
}

export function initialTimeZone(): string {
  try {
    const saved = getPreference('timezone');
    if (saved && validTimeZone(saved)) return saved;
  } catch {}
  return 'UTC';
}

export function timeZoneOptions(): string[] {
  const intl = Intl as typeof Intl & { supportedValuesOf?: (key: string) => string[] };
  const local = Intl.DateTimeFormat().resolvedOptions().timeZone;
  return [...new Set(['UTC', local, ...(intl.supportedValuesOf?.('timeZone') ?? ['Europe/London', 'Europe/Berlin', 'America/New_York', 'Asia/Dubai', 'Asia/Tokyo'])])];
}

export function utcToInput(utc: string, zone: string): string {
  if (!utc) return '';
  return Temporal.Instant.from(`${utc}Z`).toZonedDateTimeISO(zone).toPlainDateTime().toString();
}

export function inputToUtc(value: string, zone: string): string {
  if (!value) return '';
  try {
    const date = Temporal.PlainDateTime.from(value);
    return date.toZonedDateTime(zone, { disambiguation: 'reject' }).toInstant().toString().replace(/Z$/, '');
  } catch {
    throw new Error('Choose valid dates in this timezone. Daylight-saving skipped or repeated times need a different time or UTC.');
  }
}

export function timestampMillis(value: unknown): number | null {
  if (typeof value === 'number') return Number.isFinite(value) ? value : null;
  if (typeof value !== 'string') return null;
  const normalized = value.replace(' ', 'T');
  const date = /^\d{4}-\d{2}-\d{2}$/.test(normalized) ? `${normalized}T00:00:00Z`
    : /(?:Z|[+-]\d{2}:?\d{2})$/i.test(normalized) ? normalized : `${normalized}Z`;
  try { return Temporal.Instant.from(date).epochMilliseconds; } catch { return null; }
}

export function formatTimestamp(value: unknown, zone: string): string | null {
  const timestamp = timestampMillis(value);
  if (timestamp === null) return null;
  try {
    return Temporal.Instant.fromEpochMilliseconds(timestamp).toZonedDateTimeISO(zone).toPlainDateTime()
      .toString().replace('T', ' ');
  } catch { return null; }
}

export function isDateField(type: string): boolean {
  return /^(?:date|date_nanos|Date32|DateTime(?:64)?(?:\(.*\))?)$/i.test(type.replace(/^Nullable\((.*)\)$/i, '$1'));
}
