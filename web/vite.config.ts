import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import tailwind from '@tailwindcss/vite';
import path from 'node:path';

// `npm run dev` proxies the API to a local publisher-server (PUBLISHER_BIND).
const backend = process.env.PUBLISHER_BACKEND ?? 'http://127.0.0.1:8090';

export default defineConfig({
  plugins: [react(), tailwind()],
  resolve: { alias: { '@': path.resolve(import.meta.dirname, './src') } },
  server: { proxy: { '/publisher': backend } },
  test: { environment: 'jsdom', include: ['src/**/*.test.ts', 'src/**/*.test.tsx'] },
});
