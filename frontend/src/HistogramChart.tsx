import { Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import type { HistogramBucket } from './api';

type HistogramChartProps = {
  data: HistogramBucket[];
  timeZone?: string;
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

export default function HistogramChart({ data, timeZone = 'UTC' }: HistogramChartProps) {
  const buckets = [...data].sort((first, second) => first.timestamp - second.timestamp);
  const timestamps = [...new Set(buckets.map((bucket) => bucket.timestamp))];
  const intervals = timestamps.slice(1).map((timestamp, index) => timestamp - timestamps[index]);
  const bucketInterval = intervals.length > 0 ? Math.min(...intervals) : 60_000;
  const timeDomain = timestamps.length > 0
    ? [timestamps[0] - bucketInterval / 2, timestamps[timestamps.length - 1] + bucketInterval / 2]
    : [0, 60_000];
  const formatTick = (timestamp: number) => formatHistogramTime(timestamp, timeZone, bucketInterval < 60_000);
  return <div className="histogram-chart" role="img" aria-label="Document counts over time">
    <ResponsiveContainer width="100%" height="100%">
      <BarChart data={buckets} margin={{ top: 8, right: 16, bottom: 8, left: 0 }}>
        <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
        <XAxis dataKey="timestamp" type="number" scale="time" domain={timeDomain} ticks={timestamps} interval="preserveStartEnd" padding={{ left: 36, right: 36 }} tickFormatter={formatTick} minTickGap={24} tickLine={false} axisLine={{ stroke: 'var(--line)' }} tick={{ fill: 'var(--muted)', fontSize: 10 }} />
        <YAxis allowDecimals={false} width={42} axisLine={false} tickLine={false} tick={{ fill: 'var(--muted)', fontSize: 10 }} />
        <Tooltip labelFormatter={(value) => new Intl.DateTimeFormat(undefined, { timeZone, dateStyle: 'medium', timeStyle: 'long' }).format(Number(value))} formatter={(value) => [value, 'Documents']} cursor={{ fill: 'var(--green-soft)' }} contentStyle={{ background: 'var(--paper)', borderColor: 'var(--line)', color: 'var(--ink)' }} />
        <Bar dataKey="count" name="Documents" fill="#51b79a" radius={[2, 2, 0, 0]} maxBarSize={24} isAnimationActive={false} />
      </BarChart>
    </ResponsiveContainer>
  </div>;
}