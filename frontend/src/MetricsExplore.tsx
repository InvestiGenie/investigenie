import { useEffect, useRef, useState } from 'react';
import { Activity, ArrowRight, Search } from 'lucide-react';
import { api, type ConnectionSummary, type DataViewSummary, type MetricQueryResult } from './api';
import TimeRangePicker from './TimeRangePicker';
import { errorMessage } from './format';

type MetricsExploreProps = {
  connections: ConnectionSummary[];
  dataViews: DataViewSummary[];
  timeZone: string;
  startDate: string;
  endDate: string;
  onTimeZoneChange: (zone: string) => void;
  onTimeRangeChange: (start: string, end: string) => void;
  onManageDataViews: () => void;
};

const METRIC_STEPS = [
  { label: '1 second', value: 1_000 },
  { label: '5 seconds', value: 5_000 },
  { label: '15 seconds', value: 15_000 },
  { label: '30 seconds', value: 30_000 },
  { label: '1 minute', value: 60_000 },
  { label: '5 minutes', value: 300_000 },
  { label: '15 minutes', value: 900_000 },
];

function metricTimeRange(startDate: string, endDate: string): { start: number; end: number } {
  const utcInput = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,3})?)?$/;
  const start = utcInput.test(startDate) ? Date.parse(`${startDate}Z`) : NaN;
  const end = utcInput.test(endDate) ? Date.parse(`${endDate}Z`) : NaN;
  if (!startDate || !endDate || !Number.isFinite(start) || !Number.isFinite(end) || start > end) {
    throw new Error('Metric queries require a valid, bounded time window.');
  }
  return { start, end };
}

function seriesName(labels: Record<string, unknown>): string {
  const entries = Object.entries(labels).filter(([name]) => name !== '__name__');
  return entries.length ? entries.map(([name, value]) => `${name}=${String(value)}`).join(', ') : 'Single series';
}

function numberLabel(value: number): string {
  return new Intl.NumberFormat(undefined, { maximumSignificantDigits: 5 }).format(value);
}

function timeLabel(timestamp: number, timeZone: string): string {
  return new Intl.DateTimeFormat(undefined, {
    timeZone,
    hour: '2-digit',
    minute: '2-digit',
    month: 'short',
    day: 'numeric',
  }).format(timestamp);
}

function MetricSeriesChart({ result, timeZone, timeRange }: {
  result: MetricQueryResult;
  timeZone: string;
  timeRange: { start: number; end: number };
}) {
  const width = 960;
  const height = 300;
  const left = 64;
  const right = 18;
  const top = 18;
  const bottom = 38;
  const plotWidth = width - left - right;
  const plotHeight = height - top - bottom;
  const values = result.series.flatMap((series) => series.points.flatMap((point) => point.value === null ? [] : [point.value]));
  const rawMin = values.length ? Math.min(...values) : 0;
  const rawMax = values.length ? Math.max(...values) : 1;
  const padding = rawMin === rawMax ? Math.max(Math.abs(rawMin) * 0.05, 1) : (rawMax - rawMin) * 0.06;
  const min = rawMin - padding;
  const max = rawMax + padding;
  const span = Math.max(1, timeRange.end - timeRange.start);
  const x = (timestamp: number) => left + ((timestamp - timeRange.start) / span) * plotWidth;
  const y = (value: number) => top + ((max - value) / (max - min)) * plotHeight;
  const colors = ['#18785d', '#c66a36', '#3479a5', '#a8434b', '#8162a8', '#788b31'];
  const yTicks = Array.from({ length: 5 }, (_, index) => min + ((max - min) * index) / 4).reverse();
  const xTicks = [timeRange.start, timeRange.start + span / 2, timeRange.end];

  return <section className="metric-chart-region" aria-label="Metric time series">
    {values.length === 0
      ? <div className="metric-empty-chart">No numeric samples in this time window.</div>
      : <svg className="metric-chart" viewBox={`0 0 ${width} ${height}`} role="img" aria-label="Metric time series chart" preserveAspectRatio="none">
        {yTicks.map((tick, index) => {
          const tickY = top + (plotHeight * index) / 4;
          return <g key={index}>
            <line x1={left} x2={width - right} y1={tickY} y2={tickY} className="metric-grid-line" />
            <text x={left - 9} y={tickY + 4} textAnchor="end" className="metric-axis-label">{numberLabel(tick)}</text>
          </g>;
        })}
        <line x1={left} x2={left} y1={top} y2={height - bottom} className="metric-axis-line" />
        <line x1={left} x2={width - right} y1={height - bottom} y2={height - bottom} className="metric-axis-line" />
        {xTicks.map((tick) => <text key={tick} x={x(tick)} y={height - 12} textAnchor="middle" className="metric-axis-label">{timeLabel(tick, timeZone)}</text>)}
        {result.series.map((series, seriesIndex) => {
          const segments: string[] = [];
          let segment = '';
          for (const point of series.points) {
            if (point.value === null) {
              if (segment) segments.push(segment);
              segment = '';
              continue;
            }
            segment += `${segment ? ' L' : 'M'}${x(point.timestamp).toFixed(2)},${y(point.value).toFixed(2)}`;
          }
          if (segment) segments.push(segment);
          return segments.map((path, segmentIndex) => <path key={`${seriesIndex}-${segmentIndex}`} d={path} fill="none" stroke={colors[seriesIndex % colors.length]} strokeWidth="2" vectorEffect="non-scaling-stroke" />);
        })}
      </svg>}
    <div className="metric-legend" role="list" aria-label="Metric series">
      {result.series.map((series, index) => {
        const lastValue = [...series.points].reverse().find((point) => point.value !== null)?.value;
        return <div className="metric-legend-item" role="listitem" key={`${seriesName(series.labels)}-${index}`}>
          <span className="metric-legend-swatch" style={{ backgroundColor: colors[index % colors.length] }} aria-hidden="true" />
          <span className="metric-legend-name" title={seriesName(series.labels)}>{seriesName(series.labels)}</span>
          <strong>{lastValue === undefined || lastValue === null ? '—' : numberLabel(lastValue)}</strong>
        </div>;
      })}
    </div>
  </section>;
}

function MetricSeriesTable({ result }: { result: MetricQueryResult }) {
  return <div className="table-wrap metric-series-table" role="region" aria-label="Metric series table" tabIndex={0}>
    <table>
      <thead><tr><th>Series labels</th><th>Latest value</th><th>Points</th></tr></thead>
      <tbody>{result.series.map((series, index) => {
        const latest = [...series.points].reverse().find((point) => point.value !== null)?.value;
        return <tr key={`${seriesName(series.labels)}-${index}`}>
          <td className="metric-series-labels">{seriesName(series.labels)}</td>
          <td>{latest === undefined || latest === null ? '—' : numberLabel(latest)}</td>
          <td>{series.points.length}</td>
        </tr>;
      })}</tbody>
    </table>
  </div>;
}

export function TracesExplore() {
  return <section className="trace-explore-state" aria-labelledby="trace-explore-title">
    <Activity size={21} aria-hidden="true" />
    <h2 id="trace-explore-title">Tempo trace exploration is not available yet</h2>
    <p>TraceQL search, trace results, and span timelines need a Tempo integration. This view does not reinterpret logs as traces.</p>
  </section>;
}

export default function MetricsExplore({
  connections,
  dataViews,
  timeZone,
  startDate,
  endDate,
  onTimeZoneChange,
  onTimeRangeChange,
  onManageDataViews,
}: MetricsExploreProps) {
  const metricViews = dataViews.filter((view) => {
    const connection = connections.find((candidate) => candidate.id === view.connection_id);
    return connection?.integration.explore_modes?.includes('metrics');
  });
  const [selectedViewId, setSelectedViewId] = useState('');
  const [query, setQuery] = useState('');
  const [selectedMetricName, setSelectedMetricName] = useState('');
  const [selectedLabel, setSelectedLabel] = useState('');
  const [labelOperator, setLabelOperator] = useState('=');
  const [labelValue, setLabelValue] = useState('');
  const [stepMs, setStepMs] = useState(15_000);
  const [metricNames, setMetricNames] = useState<string[]>([]);
  const [metricLabels, setMetricLabels] = useState<string[]>([]);
  const [namesTruncated, setNamesTruncated] = useState(false);
  const [namesError, setNamesError] = useState('');
  const [result, setResult] = useState<MetricQueryResult | null>(null);
  const [resultView, setResultView] = useState<'chart' | 'table'>('chart');
  const [loadingNames, setLoadingNames] = useState(false);
  const [loadingQuery, setLoadingQuery] = useState(false);
  const [error, setError] = useState('');
  const requestController = useRef<AbortController | null>(null);
  const selectedView = metricViews.find((view) => view.id === selectedViewId) ?? metricViews[0];
  const connection = connections.find((candidate) => candidate.id === selectedView?.connection_id);

  useEffect(() => {
    if (!metricViews.some((view) => view.id === selectedViewId)) setSelectedViewId(metricViews[0]?.id ?? '');
  }, [metricViews, selectedViewId]);

  useEffect(() => {
    if (!connection) {
      setMetricNames([]);
      setMetricLabels([]);
      setResult(null);
      return;
    }
    const controller = new AbortController();
    setLoadingNames(true);
    setNamesError('');
    api.metricNames(connection.id, selectedView?.index ?? 'metrics', controller.signal)
      .then((catalog) => {
        if (controller.signal.aborted) return;
        setMetricNames(catalog.names);
        setNamesTruncated(catalog.truncated);
      })
      .catch((reason) => {
        if (!controller.signal.aborted) {
          setMetricNames([]);
          setNamesError(errorMessage(reason));
        }
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoadingNames(false);
      });
    api.fields(connection.id, selectedView?.index ?? 'metrics', controller.signal)
      .then((fields) => {
        if (controller.signal.aborted) return;
        setMetricLabels(fields
          .filter((field) => field.field_type === 'label' && /^[A-Za-z_][A-Za-z0-9_]*$/.test(field.name))
          .map((field) => field.name));
      })
      .catch(() => {
        if (!controller.signal.aborted) setMetricLabels([]);
      });
    return () => controller.abort();
  }, [connection, selectedView?.index]);

  useEffect(() => () => requestController.current?.abort(), []);

  async function runQuery() {
    if (!selectedView || !connection) return;
    requestController.current?.abort();
    const controller = new AbortController();
    requestController.current = controller;
    setLoadingQuery(true);
    setError('');
    try {
      const range = metricTimeRange(startDate, endDate);
      const metricResult = await api.metricQuery(connection.id, {
        index: selectedView.index,
        query: query.trim(),
        time_range: range,
        step_ms: stepMs,
      }, controller.signal);
      if (!controller.signal.aborted) {
        setResult(metricResult);
        setResultView('chart');
      }
    } catch (reason) {
      if (!controller.signal.aborted) setError(errorMessage(reason));
    } finally {
      if (!controller.signal.aborted) setLoadingQuery(false);
    }
  }

  function insertSelector() {
    if (!selectedLabel) {
      setQuery(selectedMetricName);
      return;
    }
    const matcher = `${selectedLabel}${labelOperator}${JSON.stringify(labelValue)}`;
    setQuery(`${selectedMetricName}{${matcher}}`);
  }

  let appliedRange: { start: number; end: number } | null = null;
  try {
    appliedRange = metricTimeRange(startDate, endDate);
  } catch {
    appliedRange = null;
  }

  return <section className="metric-explore" aria-label="Metrics Explore">
    <div className="metric-query-panel">
      <div className="metric-query-controls">
        <label className="metric-view-control">METRIC DATA VIEW
          <select aria-label="Metric Data View" value={selectedView?.id ?? ''} onChange={(event) => { setSelectedViewId(event.target.value); setResult(null); }}>
            {!metricViews.length && <option value="">No metric data views</option>}
            {metricViews.map((view) => <option key={view.id} value={view.id}>{view.name}</option>)}
          </select>
        </label>
        <TimeRangePicker start={startDate} end={endDate} timeZone={timeZone} onTimeZoneChange={onTimeZoneChange} onChange={onTimeRangeChange} />
        <label className="metric-step-control">STEP
          <select aria-label="Metric query step" value={stepMs} onChange={(event) => setStepMs(Number(event.target.value))}>
            {METRIC_STEPS.map((step) => <option key={step.value} value={step.value} disabled={!appliedRange || Math.floor((appliedRange.end - appliedRange.start) / step.value) + 1 > 500}>{step.label}</option>)}
          </select>
        </label>
      </div>
      <form className="metric-query-form" onSubmit={(event) => { event.preventDefault(); void runQuery(); }}>
        <select aria-label="Metric name" value={selectedMetricName} disabled={loadingNames || metricNames.length === 0} onChange={(event) => { setSelectedMetricName(event.target.value); setQuery(event.target.value); }}>
          <option value="">{loadingNames ? 'Loading metric names…' : 'Insert metric name'}</option>
          {metricNames.map((name) => <option value={name} key={name}>{name}</option>)}
        </select>
        <input aria-label="PromQL or MetricsQL query" placeholder="up or rate(http_requests_total[5m])" value={query} onChange={(event) => setQuery(event.target.value)} spellCheck={false} />
        <button className="search-button" type="submit" disabled={!connection || !query.trim() || !appliedRange || loadingQuery}>
          {loadingQuery ? <span className="spinner" /> : <Search size={14} aria-hidden="true" />}
          {loadingQuery ? 'Querying' : 'Run query'}
        </button>
      </form>
      <div className="metric-browser-row" role="group" aria-label="Metric and label browser">
        <label>Label field
          <select aria-label="Metric label field" value={selectedLabel} disabled={!metricLabels.length} onChange={(event) => setSelectedLabel(event.target.value)}>
            <option value="">Choose label</option>
            {metricLabels.map((label) => <option key={label} value={label}>{label}</option>)}
          </select>
        </label>
        <label>Match
          <select aria-label="Label matcher" value={labelOperator} onChange={(event) => setLabelOperator(event.target.value)}>
            <option value="=">Equals</option><option value="!=">Does not equal</option><option value="=~">Matches regex</option><option value="!~">Does not match regex</option>
          </select>
        </label>
        <label>Value
          <input aria-label="Label value" value={labelValue} onChange={(event) => setLabelValue(event.target.value)} />
        </label>
        <button className="quiet-button" type="button" disabled={!selectedMetricName && !selectedLabel} onClick={insertSelector}>Insert selector</button>
      </div>
      <div className="metric-query-footer">
        <span className="lucene-badge">{connection?.integration.query_language ?? 'PROMQL'}</span>
        <span>Range query · at most 50 series × 500 points</span>
        {!appliedRange && <span role="note">Choose a bounded time window for metric queries.</span>}
        {namesTruncated && <span role="note">Metric-name list capped at 500.</span>}
      </div>
    </div>

    {namesError && <div className="alert" role="alert"><span className="alert-symbol">!</span><span>{namesError}</span></div>}
    {error && <div className="alert" role="alert"><span className="alert-symbol">!</span><span>{error}</span><button aria-label="Dismiss error" onClick={() => setError('')}>×</button></div>}
    {!metricViews.length && <div className="empty-state"><h3>No metrics data views</h3><p>Create a Data View from a Prometheus or VictoriaMetrics connection.</p><button className="quiet-button" type="button" onClick={onManageDataViews}>Data Views <ArrowRight size={14} aria-hidden="true" /></button></div>}
    {loadingQuery && <div className="loading-state" role="status"><span className="spinner dark" /> Evaluating range query…</div>}
    {result && !loadingQuery && <>
      <div className="metric-result-heading">
        <div><h2>Time series</h2><p>{result.series.length} series · {result.step_ms >= 60_000 ? `${result.step_ms / 60_000}m` : `${result.step_ms / 1_000}s`} step · {appliedRange ? `${timeLabel(appliedRange.start, timeZone)} to ${timeLabel(appliedRange.end, timeZone)}` : ''}</p></div>
        <div className="metric-result-actions">
          <span className="query-duration">{result.series.reduce((count, series) => count + series.points.length, 0)} samples shown</span>
          <div className="metric-result-toggle" role="group" aria-label="Metric result view">
            <button type="button" aria-pressed={resultView === 'chart'} onClick={() => setResultView('chart')}>Chart</button>
            <button type="button" aria-pressed={resultView === 'table'} onClick={() => setResultView('table')}>Table</button>
          </div>
        </div>
      </div>
      {result.truncated && <p className="metric-truncation" role="status">Result reached its series or point limit; narrow the query or time window for complete results.</p>}
      {resultView === 'table'
        ? <MetricSeriesTable result={result} />
        : appliedRange && <MetricSeriesChart result={result} timeZone={timeZone} timeRange={appliedRange} />}
    </>}
    {!result && !loadingQuery && !error && metricViews.length > 0 && <div className="metric-empty-state">Choose a metric or enter a PromQL / MetricsQL expression, then run the query.</div>}
  </section>;
}
