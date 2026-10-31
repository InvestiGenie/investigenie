import { Database } from 'lucide-react';
import bigQueryLogo from './assets/connectors/bigquery.svg';
import clickHouseLogo from './assets/connectors/clickhouse.svg';
import elasticsearchLogo from './assets/connectors/elasticsearch.svg';
import lokiLogo from './assets/connectors/loki.png';
import mimirLogo from './assets/connectors/mimir.png';
import prometheusLogo from './assets/connectors/prometheus.svg';
import victoriaLogsLogo from './assets/connectors/victorialogs.svg';
import victoriaMetricsLogo from './assets/connectors/victoriametrics.svg';
import type { IntegrationDescriptor, IntegrationIconKey } from './api';

const connectorLogos: Record<IntegrationIconKey, string> = {
  bigquery: bigQueryLogo,
  clickhouse: clickHouseLogo,
  elasticsearch: elasticsearchLogo,
  loki: lokiLogo,
  mimir: mimirLogo,
  prometheus: prometheusLogo,
  victorialogs: victoriaLogsLogo,
  victoriametrics: victoriaMetricsLogo,
};

export default function IntegrationIcon({ integration }: { integration?: Pick<IntegrationDescriptor, 'id' | 'icon'> | null }) {
  const iconKey = integration?.icon && connectorLogos[integration.icon] ? integration.icon : undefined;
  const logo = iconKey ? connectorLogos[iconKey] : undefined;
  return <span className={`source-mark ${integration?.id ?? ''}`} data-icon={iconKey ?? 'database'} aria-hidden="true">{logo ? <img src={logo} alt="" /> : <Database size={16} strokeWidth={1.8} />}</span>;
}
