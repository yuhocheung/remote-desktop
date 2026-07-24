import { defineConfig } from 'vite';
import { fileURLToPath } from 'node:url';

export default defineConfig({
  resolve: {
    alias: {
      // wasm-bindgen 生成的绑定（cargo build --target wasm32 + wasm-bindgen CLI 产物）。
      '@rdcore': fileURLToPath(new URL('../rdcore-web/pkg', import.meta.url)),
    },
  },
  server: {
    // COOP/COEP：为 SharedArrayBuffer / 高性能 WASM 做准备（也便于未来多线程 WASM）。
    headers: {
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
    },
    fs: {
      // 允许 vite 服务仓库内 app/ 之外的 pkg 目录。
      allow: ['..'],
    },
  },
  worker: {
    format: 'es',
  },
});
