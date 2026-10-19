export function errorMessage(reason: unknown, fallback = 'An unexpected request error occurred'): string {
  return reason instanceof Error ? reason.message : fallback;
}

export function formatCount(value: string | null | undefined, options: Intl.NumberFormatOptions = {}): string {
  if (!value) return '\u2014';
  const count = Number(value);
  return Number.isFinite(count) ? new Intl.NumberFormat(undefined, options).format(count) : value;
}
