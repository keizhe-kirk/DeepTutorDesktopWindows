import React from 'react'
import ReactDOM from 'react-dom/client'
import { invoke } from '@tauri-apps/api/core'
import App from './App'
import './styles.css'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
)

// 心跳联通测试(M0 阶段用作冒烟测试)
invoke('ping').then(v => console.log('[deep-shell] ping ->', v)).catch(e => console.error(e))
