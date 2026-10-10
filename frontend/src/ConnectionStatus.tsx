import { useEffect, useState } from 'react';
import { api, type ConnectionStatus as Status } from './api';

export default function ConnectionStatus({ id, revision }: { id: string; revision: string }) {
  const [status, setStatus] = useState<Status | 'checking'>('checking');

  useEffect(() => {
    let disposed = false;
    let controller: AbortController;
    let deadline: ReturnType<typeof setTimeout> | undefined;
    let nextProbe: ReturnType<typeof setTimeout> | undefined;
    setStatus('checking');

    async function probe() {
      controller = new AbortController();
      try {
        const result = await Promise.race([
          api.connectionStatus(id, controller.signal),
          new Promise<never>((_, reject) => {
            deadline = setTimeout(() => {
              controller.abort();
              reject(new Error('Datasource status check timed out'));
            }, 7000);
          }),
        ]);
        if (!disposed) setStatus(result);
      } catch {
        if (!disposed) setStatus('not_connected');
      } finally {
        clearTimeout(deadline);
        if (!disposed) nextProbe = setTimeout(() => void probe(), 5000);
      }
    }

    void probe();
    return () => {
      disposed = true;
      controller.abort();
      clearTimeout(deadline);
      clearTimeout(nextProbe);
    };
  }, [id, revision]);

  const appearance = status === 'connected' ? 'ready' : status === 'not_connected' ? 'offline' : 'checking';
  return <span className={`connection-health connection-health-${appearance}`} title="Datasource connection status"><span className="status-light" /> {status === 'checking' ? 'Checking' : status === 'connected' ? 'Connected' : 'Not connected'}</span>;
}