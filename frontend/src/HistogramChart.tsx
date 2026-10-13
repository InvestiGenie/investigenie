import { useEffect, useId, useMemo, useRef, useState, type PointerEvent } from 'react';
import { BREAKDOWN_OTHER_KEY, type HistogramBucket } from './api';
import { bucketEnd, countTicks, formatBucketTime, intervalMillis, tickLabel, timeTicks } from './histogram';

type HistogramChartProps = {
  data: HistogramBucket[];
  timeZone?: string;
  timeRange?: { start: number; end: number };
  /** Effective bucket interval such as `30s` or `1M`; inferred from bucket spacing when omitted. */
  interval?: string | null;
  onBrush?: (range: { start: number; end: number }) => void;
  now?: number;
};

const MARGIN = { top: 10, right: 12, bottom: 30, left: 46 };
const SERIES_COLORS = ['#5036cf', '#08a9c9', '#e5793b', '#d6457a', '#3f9b6e'];
const OTHER_COLOR = '#a3acc4';
const DEFAULT_SIZE = { width: 640, height: 170 };

export function breakdownLabel(key: string): string {
  return key === BREAKDOWN_OTHER_KEY ? 'Other' : key;
}

function useSize() {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState(DEFAULT_SIZE);
  useEffect(() => {
    const element = ref.current;
    if (!element || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(([entry]) => {
      const { width, height } = entry.contentRect;
      if (width > 0 && height > 0) setSize({ width, height });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return [ref, size] as const;
}

export default function HistogramChart({ data, timeZone = 'UTC', timeRange, interval, onBrush, now = Date.now() }: HistogramChartProps) {
  const [container, { width, height }] = useSize();
  const svg = useRef<SVGSVGElement>(null);
  const hatchId = `${useId().replace(/:/g, '')}-partial`;
  const [hovered, setHovered] = useState<number | null>(null);
  const [brush, setBrush] = useState<{ from: number; to: number } | null>(null);

  const buckets = useMemo(() => [...data].sort((first, second) => first.timestamp - second.timestamp), [data]);
  const bucketMs = useMemo(() => {
    if (interval) return intervalMillis(interval);
    const gaps = buckets.slice(1).map((bucket, index) => bucket.timestamp - buckets[index].timestamp).filter((gap) => gap > 0);
    return gaps.length ? Math.min(...gaps) : 60_000;
  }, [buckets, interval]);
  const endOf = (timestamp: number) => interval ? bucketEnd(timestamp, interval, timeZone) : timestamp + bucketMs;

  const hasRange = timeRange !== undefined && Number.isFinite(timeRange.start) && Number.isFinite(timeRange.end) && timeRange.start <= timeRange.end;
  const domainStart = hasRange ? timeRange.start : buckets.length ? buckets[0].timestamp : 0;
  const lastEnd = buckets.length ? endOf(buckets[buckets.length - 1].timestamp) : domainStart + 60_000;
  const domainEnd = hasRange ? Math.max(timeRange.end, timeRange.start + 1) : Math.max(lastEnd, domainStart + 1);

  const plotWidth = Math.max(10, width - MARGIN.left - MARGIN.right);
  const plotHeight = Math.max(10, height - MARGIN.top - MARGIN.bottom);
  const x = (time: number) => MARGIN.left + ((time - domainStart) / (domainEnd - domainStart)) * plotWidth;
  const timeAt = (pixel: number) => domainStart + ((Math.min(Math.max(pixel, MARGIN.left), MARGIN.left + plotWidth) - MARGIN.left) / plotWidth) * (domainEnd - domainStart);

  const series = useMemo(() => {
    const keys: string[] = [];
    for (const bucket of buckets) for (const item of bucket.breakdown ?? []) if (!keys.includes(item.key)) keys.push(item.key);
    return [...keys.filter((key) => key !== BREAKDOWN_OTHER_KEY), ...keys.filter((key) => key === BREAKDOWN_OTHER_KEY)];
  }, [buckets]);
  const colorOf = (key: string) => key === BREAKDOWN_OTHER_KEY ? OTHER_COLOR : SERIES_COLORS[series.indexOf(key) % SERIES_COLORS.length];

  const maximum = Math.max(0, ...buckets.map((bucket) => bucket.count));
  const yTicks = countTicks(maximum);
  const yMax = yTicks[yTicks.length - 1];
  const y = (count: number) => MARGIN.top + plotHeight - (count / yMax) * plotHeight;
  const { ticks, step } = timeTicks(domainStart, domainEnd, plotWidth, timeZone);
  const compact = new Intl.NumberFormat(undefined, { notation: 'compact' });
  const precise = bucketMs < 1_000;

  const bars = buckets.map((bucket, index) => {
    const end = endOf(bucket.timestamp);
    const left = x(Math.max(bucket.timestamp, domainStart));
    const right = x(Math.min(end, domainEnd));
    const span = right - left;
    const gap = span > 4 ? Math.min(2, span * 0.15) : 0;
    const partial = hasRange && (bucket.timestamp < timeRange.start || end > timeRange.end + 1);
    return { bucket, index, end, left: left + gap / 2, width: Math.max(1, span - gap), partial };
  }).filter((bar) => bar.left <= MARGIN.left + plotWidth && bar.left + bar.width >= MARGIN.left);

  function pixelOf(event: PointerEvent<SVGRectElement>): number {
    const bounds = svg.current?.getBoundingClientRect();
    const scale = bounds && bounds.width > 0 ? width / bounds.width : 1;
    return (event.clientX - (bounds?.left ?? 0)) * scale;
  }

  function barAt(pixel: number) {
    return bars.find((bar) => pixel >= bar.left - 1 && pixel <= bar.left + bar.width + 1) ?? null;
  }

  function pointerDown(event: PointerEvent<SVGRectElement>) {
    if (!onBrush || event.button !== 0) return;
    event.currentTarget.setPointerCapture?.(event.pointerId);
    const pixel = pixelOf(event);
    setBrush({ from: pixel, to: pixel });
  }

  function pointerMove(event: PointerEvent<SVGRectElement>) {
    const pixel = pixelOf(event);
    if (brush) {
      setBrush({ ...brush, to: pixel });
      setHovered(null);
      return;
    }
    setHovered(barAt(pixel)?.index ?? null);
  }

  function pointerUp(event: PointerEvent<SVGRectElement>) {
    if (!brush || !onBrush) return;
    const pixel = pixelOf(event);
    setBrush(null);
    if (Math.abs(pixel - brush.from) >= 4) {
      const start = Math.round(timeAt(Math.min(brush.from, pixel)));
      const end = Math.round(timeAt(Math.max(brush.from, pixel)));
      if (end > start) onBrush({ start, end });
      return;
    }
    const bar = barAt(pixel);
    if (!bar) return;
    // Zoom into the clicked bucket, staying within the current window.
    const start = hasRange ? Math.max(bar.bucket.timestamp, timeRange.start) : bar.bucket.timestamp;
    const end = hasRange ? Math.min(bar.end - 1, timeRange.end) : bar.end - 1;
    if (end > start) onBrush({ start, end });
  }

  const hoveredBar = hovered === null ? null : bars.find((bar) => bar.index === hovered) ?? null;
  const brushLeft = brush ? Math.max(MARGIN.left, Math.min(brush.from, brush.to)) : 0;
  const brushRight = brush ? Math.min(MARGIN.left + plotWidth, Math.max(brush.from, brush.to)) : 0;
  const showNow = now >= domainStart && now <= domainEnd;

  return <div className="histogram-chart-wrap">
  <div className="histogram-chart" ref={container}>
    <svg ref={svg} width={width} height={height} viewBox={`0 0 ${width} ${height}`} role="img" aria-label="Document counts over time" className={onBrush ? 'brushable' : undefined}>
      <defs>
        <pattern id={hatchId} width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
          <line x1="0" y1="0" x2="0" y2="6" stroke="var(--paper)" strokeWidth="2.5" />
        </pattern>
      </defs>
      {yTicks.map((tick) => <g key={`y-${tick}`} className="histogram-y-tick">
        <line x1={MARGIN.left} x2={MARGIN.left + plotWidth} y1={y(tick)} y2={y(tick)} />
        <text x={MARGIN.left - 6} y={y(tick)} dy="0.32em" textAnchor="end">{compact.format(tick)}</text>
      </g>)}
      {bars.map((bar) => {
        const top = y(bar.bucket.count);
        const segments = bar.bucket.breakdown?.length ? series.reduce<Array<{ key: string; y: number; height: number }>>((stack, key) => {
          const count = bar.bucket.breakdown!.find((item) => item.key === key)?.count ?? 0;
          if (count <= 0) return stack;
          const base = stack.length ? stack[stack.length - 1].y : MARGIN.top + plotHeight;
          const segmentHeight = (count / yMax) * plotHeight;
          return [...stack, { key, y: base - segmentHeight, height: segmentHeight }];
        }, []) : null;
        return <g key={bar.bucket.timestamp} className={`histogram-bar${bar.partial ? ' partial' : ''}${hovered === bar.index ? ' hovered' : ''}`} data-timestamp={bar.bucket.timestamp}>
          {segments
            ? segments.map((segment) => <rect key={segment.key} x={bar.left} y={segment.y} width={bar.width} height={segment.height} fill={colorOf(segment.key)} />)
            : <rect x={bar.left} y={top} width={bar.width} height={MARGIN.top + plotHeight - top} fill="var(--brand-cyan)" />}
          {bar.partial && bar.bucket.count > 0 && <rect className="histogram-partial-overlay" x={bar.left} y={top} width={bar.width} height={MARGIN.top + plotHeight - top} fill={`url(#${hatchId})`} />}
        </g>;
      })}
      <line className="histogram-axis" x1={MARGIN.left} x2={MARGIN.left + plotWidth} y1={MARGIN.top + plotHeight} y2={MARGIN.top + plotHeight} />
      {ticks.map((tick, index) => {
        const [primary, secondary] = tickLabel(tick, index === 0 ? null : ticks[index - 1], step, timeZone);
        return <g key={`x-${tick}`} className="histogram-x-tick" transform={`translate(${x(tick)},${MARGIN.top + plotHeight})`}>
          <line y2="4" />
          <text y="14" textAnchor="middle">{primary}</text>
          {secondary && <text y="25" textAnchor="middle" className="histogram-x-date">{secondary}</text>}
        </g>;
      })}
      {showNow && <g className="histogram-now" transform={`translate(${x(now)},0)`}><line y1={MARGIN.top} y2={MARGIN.top + plotHeight} /><title>Now</title></g>}
      {brush && brushRight > brushLeft && <rect className="histogram-brush" x={brushLeft} y={MARGIN.top} width={brushRight - brushLeft} height={plotHeight} />}
      <rect
        className="histogram-interaction"
        x={MARGIN.left}
        y={MARGIN.top}
        width={plotWidth}
        height={plotHeight}
        fill="transparent"
        onPointerDown={pointerDown}
        onPointerMove={pointerMove}
        onPointerUp={pointerUp}
        onPointerLeave={() => { if (!brush) setHovered(null); }}
        onPointerCancel={() => setBrush(null)}
      />
    </svg>
    {hoveredBar && <div className="histogram-tooltip" role="tooltip" style={{ left: Math.min(Math.max(hoveredBar.left + hoveredBar.width / 2, 90), width - 90), top: MARGIN.top }}>
      <strong>{formatBucketTime(hoveredBar.bucket.timestamp, timeZone, precise)}</strong>
      <span className="histogram-tooltip-range">to {formatBucketTime(hoveredBar.end, timeZone, precise)}</span>
      <span className="histogram-tooltip-row"><span>Documents</span><b>{hoveredBar.bucket.count.toLocaleString()}</b></span>
      {hoveredBar.bucket.breakdown?.filter((item) => item.count > 0).map((item) => <span className="histogram-tooltip-row" key={item.key}><span><i style={{ background: colorOf(item.key) }} />{breakdownLabel(item.key)}</span><b>{item.count.toLocaleString()}</b></span>)}
      {hoveredBar.partial && <em>Partial bucket: only part of this interval is inside the selected time range.</em>}
    </div>}
  </div>
    {series.length > 0 && <ul className="histogram-legend" aria-label="Breakdown legend">
      {series.map((key) => <li key={key}><i style={{ background: colorOf(key) }} aria-hidden="true" />{breakdownLabel(key)}</li>)}
    </ul>}
  </div>;
}
