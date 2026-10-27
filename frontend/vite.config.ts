import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig(() => {
  let webOrigin: URL | undefined;
  const origin = process.env.WEB_ORIGIN;
  if (origin !== undefined) {
    try {
      webOrigin = new URL(origin);
    } catch {
      throw new Error('WEB_ORIGIN must be a valid HTTP(S) origin');
    }
    if (!['http:', 'https:'].includes(webOrigin.protocol) || webOrigin.username || webOrigin.password || webOrigin.pathname !== '/' || webOrigin.search || webOrigin.hash) {
      throw new Error('WEB_ORIGIN must be a valid HTTP(S) origin');
    }
  }
  const port = webOrigin
    ? Number(webOrigin.port) || (webOrigin.protocol === 'https:' ? 443 : 80)
    : 5173;

  return {
    envDir: false as const,
    plugins: [react()],
    server: {
      host: webOrigin?.hostname ?? '127.0.0.1',
      port,
      strictPort: true,
    },
    test: {
      environment: 'jsdom',
      setupFiles: './src/test-setup.ts',
    },
  };
});
