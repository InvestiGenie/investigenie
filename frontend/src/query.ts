import type { QueryDescriptor } from './api';

export function applyQueryTemplate(
  template: string,
  metadata: Pick<QueryDescriptor, 'identifier_quote'>,
  values: { index: string; field?: string; query?: string },
): string {
  const quote = metadata.identifier_quote;
  const identifier = (value: string) => quote ? `${quote}${value.replaceAll(quote, quote + quote)}${quote}` : value;
  const query = values.query ?? '';
  const composed = !query.trim() ? template.replace(/^\{query\}\s+AND\s+/i, '') : template;
  return composed.replace(/\{(index|field|query)\}/g, (_match, token: string) => {
    if (token === 'query') return query;
    return identifier(token === 'index' ? values.index : values.field ?? '');
  });
}

export function resolveSearchQuery(metadata: QueryDescriptor, index: string, query: string): string {
  return query.trim() ? query : applyQueryTemplate(metadata.default_query, metadata, { index, query });
}