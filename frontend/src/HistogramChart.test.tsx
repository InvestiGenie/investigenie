import { fireEvent, render, screen, within } from '@testing-library/react';
import { beforeAll, describe, expect, it, vi } from 'vitest';
import HistogramChart from './HistogramChart';
import { bucketEnd, countTicks, describeInterval, resolveInterval, tickLabel, timeTicks } from './histogram';

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
// Default chart size in jsdom: 640 wide, 46px left margin and 12px right margin.
const PLOT_LEFT = 46;
const PLOT_WIDTH = 640 - 46 - 12;

beforeAll(() => {
  if (!window.PointerEvent) Object.defineProperty(window, 'PointerEvent', { value: MouseEvent, configurable: true });
});

describe('histogram intervals', () => {
  it('picks Kibana-like automatic intervals near 50 bars', () => {
    expect(resolveInterval('auto', { start: 0, end: 15 * MINUTE })).toEqual({ interval: '30s', scaled: false });
    expect(resolveInterval('auto', { start: 0, end: HOUR })).toEqual({ interval: '1m', scaled: false });
    expect(resolveInterval('auto', { start: 0, end: DAY })).toEqual({ interval: '30m', scaled: false });
    expect(resolveInterval('auto', { start: 0, end: 7 * DAY })).toEqual({ interval: '3h', scaled: false });
  });

  it('keeps fixed intervals that fit and scales those that create too many buckets', () => {
    expect(resolveInterval('1h', { start: 0, end: DAY })).toEqual({ interval: '1h', scaled: false });
    expect(resolveInterval('1m', { start: 0, end: 7 * DAY })).toEqual({ interval: '3h', scaled: true });
    expect(describeInterval('3h')).toBe('3 hours');
    expect(describeInterval('1M')).toBe('1 month');
  });

  it('ends calendar buckets on calendar boundaries in the display timezone', () => {
    const february = Date.parse('2026-01-31T23:00:00Z');
    expect(bucketEnd(february, '1M', 'Europe/Berlin')).toBe(Date.parse('2026-02-28T23:00:00Z'));
    expect(bucketEnd(0, '30s', 'UTC')).toBe(30_000);
  });

  it('aligns ticks to local wall-clock boundaries and labels date changes', () => {
    const start = Date.parse('2026-10-06T00:00:00Z');
    const { ticks, step } = timeTicks(start, start + 6 * HOUR, 600, 'Asia/Kolkata');
    expect(step).toBe(2 * HOUR);
    expect(ticks.length).toBeGreaterThan(1);
    expect(ticks.every((tick) => (tick + 5.5 * HOUR) % step === 0)).toBe(true);
    const [, firstDate] = tickLabel(ticks[0], null, step, 'UTC');
    const [, sameDay] = tickLabel(ticks[1], ticks[0], step, 'UTC');
    expect(firstDate).not.toBeNull();
    expect(sameDay).toBeNull();
    expect(countTicks(7)).toEqual([0, 2, 4, 6, 8]);
  });
});

describe('HistogramChart', () => {
  const start = Date.parse('2026-10-06T08:00:00Z');
  const range = { start: start + 30_000, end: start + 10 * MINUTE };
  const data = [
    { timestamp: start, label: null, count: 4 },
    { timestamp: start + 5 * MINUTE, label: null, count: 9 },
  ];

  function interaction(container: HTMLElement) {
    return container.querySelector('.histogram-interaction') as SVGRectElement;
  }

  it('draws one bar per bucket across the full selected window and marks partial buckets', () => {
    const { container } = render(<HistogramChart data={data} interval="5m" timeRange={range} now={0} />);
    expect(screen.getByRole('img', { name: 'Document counts over time' })).toBeInTheDocument();
    const bars = container.querySelectorAll('.histogram-bar');
    expect(bars).toHaveLength(2);
    expect(bars[0]).toHaveClass('partial');
    expect(bars[1]).not.toHaveClass('partial');
    expect(container.querySelector('.histogram-now')).toBeNull();
  });

  it('shows the current-time marker when now is inside the window', () => {
    const { container } = render(<HistogramChart data={data} interval="5m" timeRange={range} now={start + 7 * MINUTE} />);
    expect(container.querySelector('.histogram-now')).not.toBeNull();
  });

  it('shows bucket range, count, breakdown, and partial-data notes on hover', () => {
    const stacked = data.map((bucket) => ({ ...bucket, breakdown: [{ key: 'api', count: bucket.count - 1 }, { key: '__other__', count: 1 }] }));
    const { container } = render(<HistogramChart data={stacked} interval="5m" timeRange={range} timeZone="UTC" now={0} />);
    expect(within(screen.getByRole('list', { name: 'Breakdown legend' })).getAllByRole('listitem').map((item) => item.textContent)).toEqual(['api', 'Other']);
    expect(container.querySelectorAll('.histogram-bar')[1].querySelectorAll('rect')).toHaveLength(2);
    fireEvent.pointerMove(interaction(container), { clientX: PLOT_LEFT + 5 });
    const tooltip = screen.getByRole('tooltip');
    expect(tooltip).toHaveTextContent('Documents4');
    expect(tooltip).toHaveTextContent('api3');
    expect(tooltip).toHaveTextContent('Other1');
    expect(tooltip).toHaveTextContent('Partial bucket');
    fireEvent.pointerLeave(interaction(container));
    expect(screen.queryByRole('tooltip')).not.toBeInTheDocument();
  });

  it('zooms to a dragged selection', () => {
    const onBrush = vi.fn();
    const { container } = render(<HistogramChart data={data} interval="5m" timeRange={range} onBrush={onBrush} now={0} />);
    fireEvent.pointerDown(interaction(container), { clientX: PLOT_LEFT, button: 0, pointerId: 1 });
    fireEvent.pointerMove(interaction(container), { clientX: PLOT_LEFT + PLOT_WIDTH / 2, pointerId: 1 });
    expect(container.querySelector('.histogram-brush')).not.toBeNull();
    fireEvent.pointerUp(interaction(container), { clientX: PLOT_LEFT + PLOT_WIDTH / 2, pointerId: 1 });
    const middle = range.start + (range.end - range.start) / 2;
    expect(onBrush).toHaveBeenCalledWith({ start: range.start, end: Math.round(middle) });
    expect(container.querySelector('.histogram-brush')).toBeNull();
  });

  it('zooms into a clicked bucket clipped to the selected window', () => {
    const onBrush = vi.fn();
    const { container } = render(<HistogramChart data={data} interval="5m" timeRange={range} onBrush={onBrush} now={0} />);
    fireEvent.pointerDown(interaction(container), { clientX: PLOT_LEFT + 5, button: 0 });
    fireEvent.pointerUp(interaction(container), { clientX: PLOT_LEFT + 5 });
    expect(onBrush).toHaveBeenCalledWith({ start: range.start, end: start + 5 * MINUTE - 1 });
  });

  it('infers bucket width from spacing when no interval is supplied and stays read-only without onBrush', () => {
    const { container } = render(<HistogramChart data={[{ timestamp: 60_000, count: 1, label: null }, { timestamp: 90_000, count: 2, label: null }]} now={0} />);
    expect(container.querySelectorAll('.histogram-bar')).toHaveLength(2);
    fireEvent.pointerDown(interaction(container), { clientX: PLOT_LEFT + 5, button: 0 });
    expect(container.querySelector('.histogram-brush')).toBeNull();
  });
});
