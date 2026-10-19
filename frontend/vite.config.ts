import { defineConfig } from 'vitest/config';
import { loadEnv } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, '..', '');
  const webOrigin = env.WEB_ORIGIN ? new URL(env.WEB_ORIGIN) : undefined;
  const port = webOrigin
    ? Number(webOrigin.port) || (webOrigin.protocol === 'https:' ? 443 : 80)
    : 5173;

  return {
    envDir: '..',
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
