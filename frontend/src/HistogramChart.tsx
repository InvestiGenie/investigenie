import { Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import type { HistogramBucket } from './api';

type HistogramChartProps = {
  data: HistogramBucket[];
  timeZone?: string;
  timeRange?: { start: number; end: number };
};

function formatHistogramTime(timestamp: number, timeZone: string, showSeconds = false): string {
  return new Intl.DateTimeFormat(undefined, {
    timeZone,
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
    ...(showSeconds ? { second: '2-digit' as const } : {}),
  }).format(timestamp);
}

export default function HistogramChart({ data, timeZone = 'UTC', timeRange }: HistogramChartProps) {
  const buckets = [...data].sort((first, second) => first.timestamp - second.timestamp);
  const timestamps = [...new Set(buckets.map((bucket) => bucket.timestamp))];
  const intervals = timestamps.slice(1).map((timestamp, index) => timestamp - timestamps[index]);
  const bucketInterval = intervals.length > 0 ? Math.min(...intervals) : 60_000;
  const hasTimeRange = timeRange !== undefined && Number.isFinite(timeRange.start) && Number.isFinite(timeRange.end) && timeRange.start <= timeRange.end;
  const timeDomain = hasTimeRange
    ? [timeRange.start, timeRange.end === timeRange.start ? timeRange.end + 1 : timeRange.end]
    : timestamps.length > 0
    ? [timestamps[0] - bucketInterval / 2, timestamps[timestamps.length - 1] + bucketInterval / 2]
    : [0, 60_000];
  const ticks = hasTimeRange
    ? Array.from({ length: 5 }, (_, index) => timeDomain[0] + (timeDomain[1] - timeDomain[0]) * index / 4)
    : timestamps;
  const formatTick = (timestamp: number) => formatHistogramTime(timestamp, timeZone, bucketInterval < 60_000);
  return <div className="histogram-chart" role="img" aria-label="Document counts over time">
    <ResponsiveContainer width="100%" height="100%">
      <BarChart data={buckets} margin={{ top: 8, right: 16, bottom: 8, left: 0 }}>
        <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
        <XAxis dataKey="timestamp" type="number" scale="time" domain={timeDomain} ticks={ticks} allowDataOverflow={hasTimeRange} interval="preserveStartEnd" padding={hasTimeRange ? { left: 0, right: 0 } : { left: 36, right: 36 }} tickFormatter={formatTick} minTickGap={24} tickLine={false} axisLine={{ stroke: 'var(--line)' }} tick={{ fill: 'var(--muted)', fontSize: 10 }} />
        <YAxis allowDecimals={false} width={42} axisLine={false} tickLine={false} tick={{ fill: 'var(--muted)', fontSize: 10 }} />
        <Tooltip labelFormatter={(value) => new Intl.DateTimeFormat(undefined, { timeZone, dateStyle: 'medium', timeStyle: 'long' }).format(Number(value))} formatter={(value) => [value, 'Documents']} cursor={{ fill: 'var(--green-soft)' }} contentStyle={{ background: 'var(--paper)', borderColor: 'var(--line)', color: 'var(--ink)' }} />
        <Bar dataKey="count" name="Documents" fill="var(--brand-cyan)" radius={[2, 2, 0, 0]} maxBarSize={24} isAnimationActive={false} />
      </BarChart>
    </ResponsiveContainer>
  </div>;
}