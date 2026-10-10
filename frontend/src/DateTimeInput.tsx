import { useState, type RefObject } from 'react';
import { CalendarDays, ChevronLeft, ChevronRight } from 'lucide-react';
import { DayPicker } from 'react-day-picker';
import { format, isValid, parseISO } from 'date-fns';
import 'react-day-picker/style.css';

type DateTimeInputProps = {
  label: 'Start' | 'End';
  value: string;
  timeZone: string;
  onChange: (value: string) => void;
  inputRef?: RefObject<HTMLInputElement | null>;
};

export default function DateTimeInput({ label, value, timeZone, onChange, inputRef }: DateTimeInputProps) {
  const [calendarOpen, setCalendarOpen] = useState(false);
  const [month, setMonth] = useState<Date>();
  const [emptyTime, setEmptyTime] = useState('00:00:00');
  const date = value.split('T')[0];
  const time = value.includes('T') ? value.split('T')[1] : emptyTime;
  const parsed = parseISO(date);
  const selected = isValid(parsed) ? parsed : undefined;

  function changeDate(next: string) {
    onChange(next ? `${next}T${time || '00:00:00'}` : '');
  }

  return <div className="date-time-control">
    <div className="date-time-fields">
      <label>{label} date<div className="calendar-input">
        <input ref={inputRef} type="text" aria-label={`${label} date`} placeholder="YYYY-MM-DD" value={date} maxLength={10} spellCheck={false} onChange={(event) => changeDate(event.target.value)} />
        <button className="calendar-toggle" aria-label={`Choose ${label.toLowerCase()} date`} title={`Choose ${label.toLowerCase()} date`} aria-expanded={calendarOpen} aria-controls={`${label.toLowerCase()}-calendar`} onClick={() => {
          if (!calendarOpen) setMonth(selected);
          setCalendarOpen((current) => !current);
        }}><CalendarDays size={15} aria-hidden="true" /></button>
      </div></label>
      <label>{label} time<input type="text" aria-label={`${label} time`} title={`24-hour time in ${timeZone}`} placeholder="HH:mm:ss" value={time} maxLength={12} spellCheck={false} onChange={(event) => {
        setEmptyTime(event.target.value);
        if (date) onChange(`${date}T${event.target.value}`);
      }} /></label>
    </div>
    {calendarOpen && <div className="range-calendar" id={`${label.toLowerCase()}-calendar`} role="group" aria-label={`${label} calendar`}>
      <DayPicker mode="single" selected={selected} month={month} onMonthChange={setMonth} captionLayout="dropdown" startMonth={new Date(1900, 0)} endMonth={new Date(2100, 11)} fixedWeeks showOutsideDays
        components={{ Chevron: ({ orientation }) => orientation === 'left' ? <ChevronLeft size={16} aria-hidden="true" /> : <ChevronRight size={16} aria-hidden="true" /> }}
        onSelect={(day) => { if (day) { changeDate(format(day, 'yyyy-MM-dd')); setCalendarOpen(false); } }} />
    </div>}
  </div>;
}