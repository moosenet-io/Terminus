import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Dev-server proxy target is a LOCAL default only (matches the vite dev convention used by
// harmony-web); it is never compiled into the production bundle, which talks same-origin via
// window.location.origin (see src/lib/aggregationClient.ts).
export default defineConfig({
  plugins: [react()],
  base: '/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    rollupOptions: {
      output: {
        manualChunks: {
          vendor: ['react', 'react-dom', 'react-router-dom'],
        },
      },
    },
  },
  server: {
    port: 5174,
    proxy: {
      '/api': 'http://localhost:3100',
      '/ws': {
        target: 'ws://localhost:3100',
        ws: true,
      },
    },
  },
})
