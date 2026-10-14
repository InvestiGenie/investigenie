import type { ConnectionField, ConnectionSummary, IntegrationDescriptor } from './api';

function field(name: string, label: string, overrides: Partial<ConnectionField> = {}): ConnectionField {
  return { name, label, input_type: 'text', required: false, secret: false, editable: true, placeholder: '', ...overrides };
}

export const elasticsearchIntegration: IntegrationDescriptor = {
  id: 'elasticsearch', display_name: 'Elasticsearch', query_language: 'Lucene', description: 'indices and data streams',
  connection_fields: [
    field('url', 'HTTP endpoint', { input_type: 'url', required: true, placeholder: 'https://search.example.com:9200' }),
    field('auth_type', 'Authentication', { input_type: 'select', group: 'authentication', options: [
      { value: 'none', label: 'No authentication' }, { value: 'basic', label: 'Username and password' }, { value: 'api_key', label: 'API key' },
    ] }),
    field('username', 'Username', { secret: true, required: true, group: 'credentials', visible_when: { field: 'auth_type', value: 'basic' } }),
    field('password', 'Password', { input_type: 'password', secret: true, required: true, group: 'credentials', visible_when: { field: 'auth_type', value: 'basic' } }),
    field('api_key', 'API key', { input_type: 'password', secret: true, required: true, group: 'credentials', visible_when: { field: 'auth_type', value: 'api_key' } }),
    field('tls_insecure', 'Skip TLS verification', { input_type: 'checkbox', editable: false, group: 'authentication' }),
  ],
  query: {
    placeholder: 'Enter a Lucene query, e.g. response.status_code:500', default_query: '', field_query: '{query} AND {field}:', identifier_quote: '',
    help: 'Lucene query runs against the selected Elasticsearch data view', document_mode: 'lookup',
    sort_requires_projection: false, histogram_total_from_buckets: false, schema_label: 'Elasticsearch mappings',
  },
};

export const clickhouseIntegration: IntegrationDescriptor = {
  id: 'clickhouse', display_name: 'ClickHouse', query_language: 'SQL', description: 'databases and tables',
  connection_fields: [
    field('url', 'HTTP endpoint', { input_type: 'url', required: true, placeholder: 'http://clickhouse.example.com:8123' }),
    field('database', 'Database', { placeholder: 'default' }),
    field('max_execution_time', 'Max execution time (seconds, optional)', { placeholder: 'Blank uses the server default (client timeout: 15s)' }),
    field('username', 'Username', { secret: true, group: 'credentials' }),
    field('password', 'Password', { input_type: 'password', secret: true, group: 'credentials' }),
  ],
  query: {
    placeholder: 'SELECT * FROM {index} LIMIT 50', default_query: 'SELECT * FROM {index} LIMIT 50',
    field_query: 'SELECT {field} FROM {index}', identifier_quote: '`',
    help: 'Read-only SQL runs against the selected ClickHouse table', document_mode: 'inline',
    sort_requires_projection: true, histogram_total_from_buckets: true, schema_label: 'ClickHouse table schema',
  },
};

export const thirdIntegration: IntegrationDescriptor = {
  id: 'archive', display_name: 'Archive', query_language: 'ArchiveQL', description: 'stored event collections',
  connection_fields: [
    field('url', 'Archive endpoint', { input_type: 'url', required: true, placeholder: 'https://archive.example.com' }),
    field('namespace', 'Namespace', { required: true, placeholder: 'events' }),
    field('region', 'Region', { input_type: 'select', options: [{ value: 'west', label: 'West' }, { value: 'east', label: 'East' }] }),
    field('replicated', 'Replicated', { input_type: 'checkbox' }),
    field('replica', 'Replica collection', { required: true, visible_when: { field: 'replicated', value: true } }),
    field('token', 'Access token', { input_type: 'password', required: true, secret: true, editable: true }),
    field('tenant', 'Tenant', { secret: true, editable: false }),
  ],
  query: {
    placeholder: 'READ {index}', default_query: 'READ {index}', field_query: 'READ {index} PROJECT {field}', identifier_quote: '"',
    help: 'Read stored archive events', document_mode: 'inline', sort_requires_projection: true,
    histogram_total_from_buckets: true, schema_label: 'Archive collection schema',
  },
};

export function connectionFixture(integration: IntegrationDescriptor, overrides: Partial<ConnectionSummary> = {}): ConnectionSummary {
  const url = 'http://localhost:9200';
  const connection = {
    id: 'source', revision: 'source-revision', name: 'Source', connection_type: integration.id, url, database: null, status: 'connected' as const,
    integration, ...overrides,
  };
  return { ...connection, configuration: overrides.configuration ?? {
    url: connection.url, ...(connection.database !== null ? { database: connection.database } : {}),
  } };
}