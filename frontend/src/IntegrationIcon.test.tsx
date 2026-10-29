import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import type { IntegrationIconKey } from './api';
import IntegrationIcon from './IntegrationIcon';

describe('IntegrationIcon', () => {
  it('shows the bundled mark for every connector icon key', () => {
    const keys: IntegrationIconKey[] = ['elasticsearch', 'clickhouse', 'bigquery', 'loki', 'mimir', 'prometheus', 'victorialogs', 'victoriametrics'];
    for (const icon of keys) {
      const { container, unmount } = render(<IntegrationIcon integration={{ id: icon, icon }} />);
      const mark = container.querySelector('.source-mark');
      expect(mark).toHaveAttribute('data-icon', icon);
      expect(mark?.querySelector('img')).toBeInTheDocument();
      unmount();
    }
  });

  it('falls back to the generic database icon for missing or unknown keys', () => {
    for (const integration of [{ id: 'custom' }, { id: 'custom', icon: 'unknown' as IntegrationIconKey }]) {
      const { container, unmount } = render(<IntegrationIcon integration={integration} />);
      expect(container.querySelector('.source-mark')).toHaveAttribute('data-icon', 'database');
      expect(container.querySelector('.source-mark img')).not.toBeInTheDocument();
      unmount();
    }
  });
});
