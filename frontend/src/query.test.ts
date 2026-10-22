import { describe, expect, it } from 'vitest';
import { applyQueryTemplate } from './query';

describe('query templates', () => {
  it('quotes identifiers by doubling the configured quote without replacing tokens inside values', () => {
    expect(applyQueryTemplate('SELECT {field} FROM {index}', { identifier_quote: '`' }, {
      index: 'events`{field}', field: 'a`b',
    })).toBe('SELECT `a``b` FROM `events``{field}`');
    expect(applyQueryTemplate('READ {index} {field}', { identifier_quote: '"' }, {
      index: 'a"b', field: 'c"d',
    })).toBe('READ "a""b" "c""d"');
  });

  it('composes a field query without a leading conjunction when the query is empty', () => {
    const metadata = { identifier_quote: '' };
    expect(applyQueryTemplate('{query} AND {field}:', metadata, { index: 'logs', field: 'message' })).toBe('message:');
    expect(applyQueryTemplate('{query} AND {field}:', metadata, { index: 'logs', field: 'message', query: '   ' })).toBe('message:');
    expect(applyQueryTemplate('{query} AND {field}:', metadata, { index: 'logs', field: 'message', query: 'status:500' })).toBe('status:500 AND message:');
  });
});
