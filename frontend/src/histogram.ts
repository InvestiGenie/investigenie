import { Temporal } from '@js-temporal/polyfill';

export const BAR_TARGET = 50;
export const MAX_BARS = 100;

export type IntervalChoice = 'auto' | '1ms' | '1s' | '1m' | '1h' | '1d' | '1w' | '1M' | '1y';

export const INTERVAL_CHOICES: Array<{ value: IntervalChoice; label: string }> = [
  { value: 'auto', label: 'Auto' },
  { value: '1ms', label: 'Millisecond' },
  { value: '1s', label: 'Second' },
  { value: '1m', label: 'Minute' },
  { value: '1h', label: 'Hour' },
  { value: '1d', label: 'Day' },
  { value: '1w', label: 'Week' },
  { value: '1M', label: 'Month' },
  { value: '1y', label: 'Year' },
];

const UNIT_MS: Record<string, number> = { ms: 1, s: 1_000, m: 60_000, h: 3_600_000, d: 86_400_000, w: 604_800_000, M: 2_592_000_000, y: 31_536_000_000 };
const UNIT_NAMES: Record<string, string> = { ms: 'millisecond', s: 'second', m: 'minute', h: 'hour', d: 'day', w: 'week', M: 'month', y: 'year' };

const NICE_INTERVALS = ['1ms', '5ms', '10ms', '50ms', '100ms', '500ms', '1s', '5s', '10s', '30s', '1m', '5m', '10m', '30m', '1h', '3h', '12h', '1d', '1w', '1M', '1y'];

function parseInterval(interval: string): { count: number; unit: string } | null {
  const match = /^(\d+)(ms|s|m|h|d|w|M|y)$/.exec(interval);
  return match ? { count: Number(match[1]), unit: match[2] } : null;
}

export function intervalMillis(interval: string): number {
  const parsed = parseInterval(interval);
  return parsed ? parsed.count * UNIT_MS[parsed.unit] : 60_000;
}

export function isCalendarInterval(interval: string): boolean {
  const unit = parseInterval(interval)?.unit;
  return unit === 'w' || unit === 'M' || unit === 'y';
}

export function describeInterval(interval: string): string {
  const parsed = parseInterval(interval);
  if (!parsed) return interval;
  const name = UNIT_NAMES[parsed.unit];
  return parsed.count === 1 ? `1 ${name}` : `${parsed.count} ${name}s`;
}

export type ResolvedInterval = { interval: string; scaled: boolean };

/** Mirrors Kibana's Discover: auto targets ~50 bars, and fixed choices scale up past 100 bars. */
export function resolveInterval(choice: IntervalChoice, range: { start: number; end: number }): ResolvedInterval {
  const span = Math.max(1, range.end - range.start);
  const fits = (interval: string) => span / intervalMillis(interval) <= MAX_BARS;
  if (choice === 'auto') {
    const candidates = NICE_INTERVALS.filter(fits);
    const best = candidates.reduce<string | null>((current, interval) => {
      const distance = Math.abs(Math.log(span / intervalMillis(interval) / BAR_TARGET));
      return current === null || distance < Math.abs(Math.log(span / intervalMillis(current) / BAR_TARGET)) ? interval : current;
    }, null);
    return { interval: best ?? '1y', scaled: false };
  }
  if (fits(choice)) return { interval: choice, scaled: false };
  const scaled = NICE_INTERVALS.find((interval) => intervalMillis(interval) >= intervalMillis(choice) && fits(interval)) ?? '1y';
  return { interval: scaled, scaled: true };
}

export function bucketEnd(timestamp: number, interval: string, timeZone: string): number {
  const parsed = parseInterval(interval);
  if (!parsed || !isCalendarInterval(interval)) return timestamp + intervalMillis(interval);
  const unit = parsed.unit === 'w' ? 'weeks' : parsed.unit === 'M' ? 'months' : 'years';
  try {
    return Temporal.Instant.fromEpochMilliseconds(timestamp).toZonedDateTimeISO(timeZone).add({ [unit]: parsed.count }).epochMilliseconds;
  } catch {
    return timestamp + intervalMillis(interval);
  }
}

const TICK_STEPS = [
  1, 2, 5, 10, 20, 50, 100, 200, 500,
  1_000, 2_000, 5_000, 10_000, 15_000, 30_000,
  60_000, 120_000, 300_000, 600_000, 900_000, 1_800_000,
  3_600_000, 7_200_000, 10_800_000, 21_600_000, 43_200_000,
  86_400_000, 172_800_000, 604_800_000,
];
const DAY = 86_400_000;

function zoned(timestamp: number, timeZone: string) {
  return Temporal.Instant.fromEpochMilliseconds(timestamp).toZonedDateTimeISO(timeZone);
}

/** Tick positions aligned to wall-clock boundaries in the display timezone. */
export function timeTicks(start: number, end: number, width: number, timeZone: string): { ticks: number[]; step: number } {
  if (!(end > start)) return { ticks: [start], step: 60_000 };
  const target = Math.max(2, Math.floor(width / 110));
  const raw = (end - start) / target;
  const ticks: number[] = [];
  try {
    if (raw >= 28 * DAY) {
      const months = raw >= 360 * DAY ? Math.max(12, Math.ceil(raw / (365 * DAY)) * 12) : Math.max(1, Math.round(raw / (30 * DAY)));
      let cursor = zoned(start, timeZone).with({ day: 1 }).startOfDay();
      if (months >= 12) cursor = cursor.with({ month: 1 });
      while (cursor.epochMilliseconds < start) cursor = cursor.add({ months });
      while (cursor.epochMilliseconds <= end && ticks.length < 200) {
        ticks.push(cursor.epochMilliseconds);
        cursor = cursor.add({ months });
      }
      return { ticks, step: months * 30 * DAY };
    }
    const step = TICK_STEPS.find((candidate) => candidate >= raw) ?? 7 * DAY;
    if (step >= DAY) {
      const days = step / DAY;
      let cursor = zoned(start, timeZone).startOfDay();
      while (cursor.epochMilliseconds < start) cursor = cursor.add({ days: 1 });
      while (cursor.epochMilliseconds <= end && ticks.length < 200) {
        ticks.push(cursor.epochMilliseconds);
        cursor = cursor.add({ days });
      }
      return { ticks, step };
    }
    const offset = zoned(start, timeZone).offsetNanoseconds / 1_000_000;
    for (let tick = Math.ceil((start + offset) / step) * step - offset; tick <= end && ticks.length < 200; tick += step) ticks.push(tick);
    return { ticks, step };
  } catch {
    return { ticks: [start, end], step: end - start };
  }
}

function formatter(timeZone: string, options: Intl.DateTimeFormatOptions) {
  return new Intl.DateTimeFormat(undefined, { timeZone, hourCycle: 'h23', ...options });
}

/** Returns a primary label and, when the day changes, a secondary date line. */
export function tickLabel(timestamp: number, previous: number | null, step: number, timeZone: string): [string, string | null] {
  if (step >= 360 * DAY) return [formatter(timeZone, { year: 'numeric' }).format(timestamp), null];
  if (step >= 28 * DAY) return [formatter(timeZone, { month: 'short', year: 'numeric' }).format(timestamp), null];
  if (step >= DAY) return [formatter(timeZone, { month: 'short', day: 'numeric' }).format(timestamp), null];
  const time = step < 1_000
    ? formatter(timeZone, { hour: '2-digit', minute: '2-digit', second: '2-digit', fractionalSecondDigits: 3 }).format(timestamp)
    : step < 60_000
      ? formatter(timeZone, { hour: '2-digit', minute: '2-digit', second: '2-digit' }).format(timestamp)
      : formatter(timeZone, { hour: '2-digit', minute: '2-digit' }).format(timestamp);
  const day = formatter(timeZone, { month: 'short', day: 'numeric' });
  const changed = previous === null || day.format(previous) !== day.format(timestamp);
  return [time, changed ? day.format(timestamp) : null];
}

export function formatBucketTime(timestamp: number, timeZone: string, precise: boolean): string {
  const date = formatter(timeZone, { year: 'numeric', month: 'short', day: 'numeric' }).format(timestamp);
  const time = formatter(timeZone, precise
    ? { hour: '2-digit', minute: '2-digit', second: '2-digit', fractionalSecondDigits: 3 }
    : { hour: '2-digit', minute: '2-digit', second: '2-digit' }).format(timestamp);
  return `${date} @ ${time}`;
}

/** Rounds the y-axis maximum up to 1, 2, or 5 × 10^n and returns evenly spaced ticks. */
export function countTicks(maximum: number, count = 4): number[] {
  if (maximum <= 0) return [0, 1];
  const rough = maximum / count;
  const magnitude = 10 ** Math.floor(Math.log10(rough));
  const step = Math.max(1, [1, 2, 5, 10].map((factor) => factor * magnitude).find((candidate) => candidate >= rough) ?? 10 * magnitude);
  const ticks: number[] = [];
  for (let tick = 0; tick < maximum + step; tick += step) ticks.push(tick);
  return ticks;
}
