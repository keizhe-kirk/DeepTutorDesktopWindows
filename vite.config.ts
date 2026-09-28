import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import pkg from './package.json'

// Tauri 暴露的 dev URL:
// - 前端壳 UI 启动后由 Tauri 接管 webview
// - 主 DeepTutor 界面通过 http://127.0.0.1:3782 由后端提供
// 本项目 src/ 仅做启动/状态页,体积极小
export default defineConfig(async () => ({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: false,
    hmr: { protocol: 'ws', host: 'localhost', port: 1421 },
    watch: { ignored: ['**/src-tauri/**'] }
  },
  envPrefix: ['VITE_', 'TAURI_'],
  define: {
    // 让前端能跟着环境变量切端口(默认与壳层 BackendConfig 一致)
    'import.meta.env.VITE_DEEPTUTOR_WEB_PORT': JSON.stringify(
      process.env.DEEPTUTOR_WEB_PORT ?? '3782',
    ),
    // 版本号直接吃 package.json —— 免得启动页显示的版本要靠手抄兜底值,
    // 发版时漏改就显示成上一版(那是第五处版本号,最容易漏)。
    'import.meta.env.VITE_SHELL_VERSION': JSON.stringify(pkg.version),
  },
  build: {
    target: 'es2022',
    minify: !process.env.TAURI_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_DEBUG
  }
}))
