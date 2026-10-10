import { useEffect, useRef, useState } from 'react';
import { CalendarDays, ChevronDown, X } from 'lucide-react';
import { inputToUtc, timeZoneOptions, utcToInput } from './time';
import DateTimeInput from './DateTimeInput';

type TimeRangePickerProps = {
  start: string;
  end: string;
  onChange: (start: string, end: string) => void;
  timeZone?: string;
  onTimeZoneChange?: (zone: string) => void;
};

function rangeLabel(start: string, end: string, timeZone: string): string {
  if (!start && !end) return 'All time';
  const formatter = new Intl.DateTimeFormat(undefined, { timeZone, month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
  return `${formatter.format(Date.parse(`${start}Z`))} - ${formatter.format(Date.parse(`${end}Z`))}`;
}

export default function TimeRangePicker({ start, end, onChange, timeZone = 'UTC', onTimeZoneChange }: TimeRangePickerProps) {
  const [open, setOpen] = useState(false);
  const [draftStart, setDraftStart] = useState('');
  const [draftEnd, setDraftEnd] = useState('');
  const [error, setError] = useState('');
  const container = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const startInput = useRef<HTMLInputElement>(null);

  function close() {
    setOpen(false);
    trigger.current?.focus();
  }

  useEffect(() => {
    if (!open) return;
    startInput.current?.focus();
    function outside(event: PointerEvent) {
      if (!container.current?.contains(event.target as Node)) setOpen(false);
    }
    function escape(event: KeyboardEvent) {
      if (event.key === 'Escape') {
        setOpen(false);
        trigger.current?.focus();
      }
    }
    document.addEventListener('pointerdown', outside);
    document.addEventListener('keydown', escape);
    return () => {
      document.removeEventListener('pointerdown', outside);
      document.removeEventListener('keydown', escape);
    };
  }, [open]);

  function preset(minutes: string) {
    setError('');
    if (!minutes) {
      setDraftStart('');
      setDraftEnd('');
      return;
    }
    const now = Date.now();
    setDraftStart(utcToInput(new Date(now - Number(minutes) * 60_000).toISOString().slice(0, 19), timeZone));
    setDraftEnd(utcToInput(new Date(now).toISOString().slice(0, 19), timeZone));
  }

  function apply() {
    let utcStart: string;
    let utcEnd: string;
    try {
      utcStart = inputToUtc(draftStart, timeZone);
      utcEnd = inputToUtc(draftEnd, timeZone);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : 'Invalid dates');
      return;
    }
    const startTime = Date.parse(`${utcStart}Z`);
    const endTime = Date.parse(`${utcEnd}Z`);
    if ((draftStart || draftEnd) && (!draftStart || !draftEnd || !Number.isFinite(startTime) || !Number.isFinite(endTime) || startTime > endTime)) {
      setError('Choose both dates, with start no later than end.');
      return;
    }
    onChange(utcStart, utcEnd);
    close();
  }

  function changeTimeZone(zone: string) {
    try {
      const convertedStart = utcToInput(inputToUtc(draftStart, timeZone), zone);
      const convertedEnd = utcToInput(inputToUtc(draftEnd, timeZone), zone);
      setDraftStart(convertedStart);
      setDraftEnd(convertedEnd);
      onTimeZoneChange?.(zone);
      setError('');
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : 'Invalid dates');
    }
  }

  return <div className="time-window" ref={container}>
    <button ref={trigger} className="time-range-trigger" aria-label="Edit time window" aria-expanded={open} aria-haspopup="dialog" title={`Search time window in ${timeZone}`} onClick={() => {
      if (open) { close(); return; }
      setDraftStart(utcToInput(start, timeZone)); setDraftEnd(utcToInput(end, timeZone)); setError(''); setOpen(true);
    }}><CalendarDays size={15} aria-hidden="true" /><span>{rangeLabel(start, end, timeZone)}</span><small className="time-zone-label" title={timeZone}>{timeZone}</small><ChevronDown size={13} aria-hidden="true" /></button>
    {(start || end) && <button className="clear-time-window" aria-label="Clear time window" title="All time" onClick={() => { onChange('', ''); setOpen(false); }}><X size={14} aria-hidden="true" /></button>}
    {open && <section className="time-range-editor" role="dialog" aria-label="Time window">
      <div className="time-range-heading"><strong>Time window</strong><button className="clear-time-window" aria-label="Close time window" onClick={close}><X size={14} aria-hidden="true" /></button></div>
      {onTimeZoneChange && <label>Timezone<select aria-label="Timezone" value={timeZone} onChange={(event) => changeTimeZone(event.target.value)}>
        {[...new Set([timeZone, ...timeZoneOptions()])].map((zone) => <option key={zone} value={zone}>{zone.replaceAll('_', ' ')}</option>)}
      </select></label>}
      <label>Preset<select aria-label="Time window preset" defaultValue="custom" onChange={(event) => { if (event.target.value !== 'custom') preset(event.target.value); }}>
        <option value="custom">Custom range</option><option value="">All time</option><option value="15">Last 15 minutes</option><option value="60">Last hour</option><option value="1440">Last 24 hours</option><option value="10080">Last 7 days</option>
      </select></label>
      <DateTimeInput label="Start" value={draftStart} timeZone={timeZone} inputRef={startInput} onChange={(value) => { setDraftStart(value); setError(''); }} />
      <DateTimeInput label="End" value={draftEnd} timeZone={timeZone} onChange={(value) => { setDraftEnd(value); setError(''); }} />
      {error && <p className="time-range-error" role="alert">{error}</p>}
      <div className="time-range-actions"><button className="quiet-button" onClick={close}>Cancel</button><button className="search-button" onClick={apply}>Apply</button></div>
    </section>}
  </div>;
}