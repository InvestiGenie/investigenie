import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { Check, ChevronDown, Search } from 'lucide-react';

export type SearchableOption = {
  value: string;
  description?: string;
  badge?: string;
  tone?: string;
};

type SearchableSelectProps = {
  label: string;
  value: string;
  options: SearchableOption[];
  onChange: (value: string) => void;
  disabled?: boolean;
  loading?: boolean;
  placeholder?: string;
  emptyLabel?: string;
  maxRendered?: number;
};

export default function SearchableSelect({ label, value, options, onChange, disabled = false, loading = false, placeholder = 'Search…', emptyLabel = 'Select an option', maxRendered = 200 }: SearchableSelectProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const container = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const listId = useId();

  const matches = useMemo(() => {
    const terms = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
    const filtered = terms.length === 0 ? options : options.filter((option) => {
      const name = option.value.toLocaleLowerCase();
      return terms.every((term) => name.includes(term));
    });
    // Prefix matches first, then alphabetical, mirroring how people scan index names.
    const needle = query.trim().toLocaleLowerCase();
    return needle ? [...filtered].sort((left, right) => {
      const leftPrefix = left.value.toLocaleLowerCase().startsWith(needle) ? 0 : 1;
      const rightPrefix = right.value.toLocaleLowerCase().startsWith(needle) ? 0 : 1;
      return leftPrefix - rightPrefix || left.value.localeCompare(right.value);
    }) : filtered;
  }, [options, query]);
  const visible = matches.slice(0, maxRendered);

  useEffect(() => {
    if (!open) return;
    function outside(event: PointerEvent) {
      if (!container.current?.contains(event.target as Node)) close();
    }
    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [open]);

  useEffect(() => {
    list.current?.querySelector<HTMLElement>(`[data-index="${active}"]`)?.scrollIntoView?.({ block: 'nearest' });
  }, [active]);

  function show() {
    if (disabled) return;
    const selectedIndex = options.findIndex((option) => option.value === value);
    setQuery('');
    setActive(Math.max(0, selectedIndex));
    setOpen(true);
  }

  function close() {
    setOpen(false);
    setQuery('');
  }

  function choose(option: SearchableOption) {
    onChange(option.value);
    close();
  }

  function handleKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (!open) { show(); return; }
      const step = event.key === 'ArrowDown' ? 1 : -1;
      setActive((current) => visible.length === 0 ? 0 : (current + step + visible.length) % visible.length);
    } else if (event.key === 'Enter' && open) {
      event.preventDefault();
      if (visible[active]) choose(visible[active]);
    } else if (event.key === 'Escape' && open) {
      event.preventDefault();
      close();
    } else if (event.key === 'Home' && open) {
      event.preventDefault();
      setActive(0);
    } else if (event.key === 'End' && open) {
      event.preventDefault();
      setActive(Math.max(0, visible.length - 1));
    }
  }

  const activeId = open && visible[active] ? `${listId}-option-${active}` : undefined;

  return <div className={`searchable-select${open ? ' open' : ''}${disabled ? ' disabled' : ''}`} ref={container}>
    <div className="searchable-select-control" onClick={() => { if (!open) show(); input.current?.focus(); }}>
      {open ? <Search size={14} aria-hidden="true" /> : null}
      <input
        ref={input}
        role="combobox"
        aria-label={label}
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={activeId}
        disabled={disabled}
        value={open ? query : value}
        placeholder={open ? placeholder : emptyLabel}
        onFocus={show}
        onKeyDown={handleKeyDown}
        onChange={(event) => {
          const text = event.target.value;
          setQuery(text);
          setActive(0);
          setOpen(true);
          const exact = options.find((option) => option.value === text);
          if (exact) onChange(exact.value);
        }}
        autoComplete="off"
        spellCheck={false}
      />
      <ChevronDown size={14} aria-hidden="true" className="searchable-select-chevron" />
    </div>
    {open && <div className="searchable-select-menu">
      <div className="searchable-select-summary" aria-live="polite">
        {loading ? 'Loading…' : `${matches.length.toLocaleString()} of ${options.length.toLocaleString()}${matches.length > visible.length ? ` · showing first ${visible.length}; refine the search` : ''}`}
      </div>
      <ul id={listId} role="listbox" aria-label={`${label} options`} ref={list}>
        {visible.map((option, index) => <li
          key={option.value}
          id={`${listId}-option-${index}`}
          data-index={index}
          role="option"
          aria-label={option.value}
          aria-selected={option.value === value}
          className={`${index === active ? 'active' : ''}${option.value === value ? ' selected' : ''}`}
          onMouseDown={(event) => event.preventDefault()}
          onMouseEnter={() => setActive(index)}
          onClick={() => choose(option)}
          title={option.value}
        >
          <span className="searchable-select-check" aria-hidden="true">{option.value === value && <Check size={12} />}</span>
          <span className="searchable-select-name">{option.value}</span>
          {option.description && <small aria-hidden="true">{option.description}</small>}
          {option.badge && <span className={`searchable-select-badge ${option.tone ?? ''}`} aria-hidden="true">{option.badge}</span>}
        </li>)}
        {!loading && visible.length === 0 && <li className="searchable-select-empty" role="presentation">No matches</li>}
      </ul>
    </div>}
  </div>;
}
