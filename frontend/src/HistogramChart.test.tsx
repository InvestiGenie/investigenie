import { render } from '@testing-library/react';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import HistogramChart from './HistogramChart';

const chartMocks = vi.hoisted(() => ({ axis: vi.fn(), chart: vi.fn(), tooltip: vi.fn() }));

vi.mock('recharts', () => ({
  ResponsiveContainer: ({ children }: { children: ReactNode }) => children,
  BarChart: (props: { children: ReactNode }) => {
    chartMocks.chart(props);
    return <div>{props.children}</div>;
  },
  XAxis: (props: unknown) => { chartMocks.axis(props); return null; },
  YAxis: () => null,
  CartesianGrid: () => null,
  Tooltip: (props: unknown) => { chartMocks.tooltip(props); return null; },
  Bar: () => null,
}));

describe('HistogramChart', () => {
  beforeEach(() => vi.clearAllMocks());

  it('aligns ticks with sorted buckets and leaves half a bucket beyond both edges', () => {
    const timestamps = [1_791_417_600_000, 1_791_417_660_000, 1_791_417_780_000];
    const data = [timestamps[2], timestamps[0], timestamps[1]].map((timestamp) => ({ timestamp, count: 1, label: null }));
    render(<HistogramChart data={data} />);
    expect(chartMocks.axis.mock.calls[0][0]).toMatchObject({
      ticks: timestamps,
      domain: [timestamps[0] - 30_000, timestamps[2] + 30_000],
      interval: 'preserveStartEnd',
      padding: { left: 36, right: 36 },
    });
    expect(chartMocks.chart.mock.calls[0][0].data.map((bucket: { timestamp: number }) => bucket.timestamp)).toEqual(timestamps);
    expect(data[0].timestamp).toBe(timestamps[2]);
  });

  it('gives a single bucket a nonzero time domain', () => {
    render(<HistogramChart data={[{ timestamp: 60_000, count: 1, label: null }]} />);
    expect(chartMocks.axis.mock.calls[0][0].domain).toEqual([30_000, 90_000]);
  });

  it('includes seconds when adjacent buckets are less than a minute apart', () => {
    render(<HistogramChart data={[{ timestamp: 60_000, count: 1, label: null }, { timestamp: 90_000, count: 2, label: null }]} />);
    const formatter = chartMocks.axis.mock.calls[0][0].tickFormatter;
    expect(formatter(60_000)).not.toBe(formatter(90_000));
  });

  it('formats axis labels and tooltips in UTC rather than the browser timezone', () => {
    const timestamp = Date.parse('2026-10-06T08:19:20Z');
    render(<HistogramChart data={[{ timestamp, count: 1, label: null }]} />);
    expect(chartMocks.axis.mock.calls[0][0].tickFormatter(timestamp)).toBe(new Intl.DateTimeFormat(undefined, {
      timeZone: 'UTC', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit',
    }).format(timestamp));
    expect(chartMocks.tooltip.mock.calls[0][0].labelFormatter(timestamp)).toBe(new Intl.DateTimeFormat(undefined, {
      timeZone: 'UTC', dateStyle: 'medium', timeStyle: 'long',
    }).format(timestamp));
  });

  it('uses the selected timezone in both ticks and tooltips without changing bucket timestamps', () => {
    const timestamp = Date.parse('2026-10-06T08:18:00Z');
    render(<HistogramChart data={[{ timestamp, count: 1, label: null }]} timeZone="Europe/Berlin" />);
    expect(chartMocks.axis.mock.calls[0][0].tickFormatter(timestamp)).toBe(new Intl.DateTimeFormat(undefined, {
      timeZone: 'Europe/Berlin', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit',
    }).format(timestamp));
    expect(chartMocks.tooltip.mock.calls[0][0].labelFormatter(timestamp)).toBe(new Intl.DateTimeFormat(undefined, {
      timeZone: 'Europe/Berlin', dateStyle: 'medium', timeStyle: 'long',
    }).format(timestamp));
    expect(chartMocks.chart.mock.calls[0][0].data[0].timestamp).toBe(timestamp);
  });
});