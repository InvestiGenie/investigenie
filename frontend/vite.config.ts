import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { parse } from 'smol-toml';

export default defineConfig(() => {
  let webOrigin: URL | undefined;
  try {
    const configPath = fileURLToPath(new URL('../investigenie.toml', import.meta.url));
    const configuration = existsSync(configPath) ? parse(readFileSync(configPath, 'utf8')) : {};
    const server = configuration.server;
    const configuredOrigin = server && typeof server === 'object' && 'web_origin' in server ? server.web_origin : undefined;
    const origin = process.env.WEB_ORIGIN ?? configuredOrigin;
    if (origin !== undefined) {
      if (typeof origin !== 'string') throw new Error('Invalid origin type');
      webOrigin = new URL(origin);
      if (!['http:', 'https:'].includes(webOrigin.protocol) || webOrigin.username || webOrigin.password || webOrigin.pathname !== '/' || webOrigin.search || webOrigin.hash) throw new Error('Invalid origin');
    }
  } catch {
    throw new Error('Could not read a valid server.web_origin from investigenie.toml or WEB_ORIGIN');
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
