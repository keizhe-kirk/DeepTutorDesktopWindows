import { useEffect, useMemo, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

const WEB_URL = `http://127.0.0.1:${import.meta.env.VITE_DEEPTUTOR_WEB_PORT ?? 3782}`

type Stage =
  | 'idle'
  | 'locating_python'
  | 'checking_deps'
  | 'starting'
  | 'probing'
  | 'ready'
  | 'failed'

type ProcState = 'idle' | 'booting' | 'ready' | 'crashed' | 'stopped' | 'failed'

interface Health { api: boolean; web: boolean }

interface StatusSnapshot {
  state: ProcState
  stage: Stage
  message: string
  health: Health
  python: string | null
  pid: number | null
}

interface LogLine { ts: string; stream: string; line: string }

interface LmModel {
  id: string
  state: string
  publisher?: string | null
  arch?: string | null
  size_bytes?: number | null
  max_context_length?: number | null
}

interface LmStudioInfo {
  detected: boolean
  base_url: string | null
  models: LmModel[]
}

interface UpdateCheck {
  available: boolean
  current_version: string
  latest_version: string | null
  body: string | null
  date: string | null
}

const STAGE_ORDER: Stage[] = ['locating_python', 'checking_deps', 'starting', 'probing', 'ready']

const STAGE_LABEL: Record<Stage, string> = {
  idle: '待启动',
  locating_python: '探测 Python',
  checking_deps: '检查依赖',
  starting: '启动后端',
  probing: '等待服务就绪',
  ready: '加载界面',
  failed: '启动失败',
}

const EMPTY: StatusSnapshot = {
  state: 'booting',
  stage: 'idle',
  message: '正在初始化...',
  health: { api: false, web: false },
  python: null,
  pid: null,
}

export default function App() {
  const [status, setStatus] = useState<StatusSnapshot>(EMPTY)
  const [logs, setLogs] = useState<LogLine[]>([])
  const [elapsed, setElapsed] = useState(0)
  const [restarting, setRestarting] = useState(false)
  const [lmStudio, setLmStudio] = useState<LmStudioInfo>({ detected: false, base_url: null, models: [] })
  const [update, setUpdate] = useState<UpdateCheck | null>(null)
  const [updating, setUpdating] = useState(false)
  const consoleRef = useRef<HTMLDivElement>(null)

  const refreshLmStudio = async () => {
    try {
      const info = await invoke<LmStudioInfo>('lmstudio_status')
      setLmStudio(info)
    } catch (e) {
      console.error(e)
    }
  }

  useEffect(() => {
    // 首屏挂载时主动拉一次快照,避免错过挂载前已经发出的事件
    invoke<StatusSnapshot>('backend_status').then(setStatus).catch(console.error)
    invoke<LogLine[]>('backend_logs').then(setLogs).catch(console.error)
    refreshLmStudio()

    const unState = listen<StatusSnapshot>('backend://state', e => setStatus(e.payload))
    const unLog = listen<LogLine>('backend://log', e =>
      setLogs(prev => [...prev, e.payload].slice(-300)),
    )

    const timer = setInterval(() => setElapsed(v => v + 1), 1000)
    return () => {
      unState.then(f => f())
      unLog.then(f => f())
      clearInterval(timer)
    }
  }, [])

  // 后端就绪 -> 进入 DeepTutor Web UI。
  // 生产模式自动跳转;dev 模式不自动(避免 WebView2 0x8007139F),改为显示按钮。
  useEffect(() => {
    if (status.state === 'ready' && !import.meta.env.DEV) {
      window.location.replace(WEB_URL)
    }
  }, [status.state])

  useEffect(() => {
    const el = consoleRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [logs])

  const failed = status.stage === 'failed'
  const stageIndex = STAGE_ORDER.indexOf(status.stage)
  const progress = failed ? 1 : stageIndex < 0 ? 0.05 : (stageIndex + 1) / STAGE_ORDER.length

  const [headline, hint] = useMemo(() => {
    const parts = status.message.split('\n')
    return [parts[0] ?? '', parts.slice(1).join('\n')]
  }, [status.message])

  const retry = async () => {
    setRestarting(true)
    setLogs([])
    setElapsed(0)
    try {
      await invoke('backend_restart')
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `重启失败: ${e}` }])
    } finally {
      setRestarting(false)
    }
  }

  const toggleModel = async (m: LmModel) => {
    try {
      if (m.state === 'loaded') {
        await invoke('lmstudio_unload', { modelId: m.id })
      } else {
        await invoke('lmstudio_load', { modelId: m.id })
      }
      await refreshLmStudio()
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `模型操作失败: ${e}` }])
    }
  }

  const checkUpdate = async () => {
    try {
      const info = await invoke<UpdateCheck>('check_for_update')
      setUpdate(info)
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `检查更新失败: ${e}` }])
    }
  }

  const doUpdate = async () => {
    setUpdating(true)
    try {
      await invoke('install_update')
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `更新失败: ${e}` }])
      setUpdating(false)
    }
  }

  const loadedCount = lmStudio.models.filter(m => m.state === 'loaded').length

  return (
    <div className="shell">
      <div className="panel">
        <div className="brand">
          <div className="logo" />
          <div className="brand-text">
            <h1>
              DeepTutor
              {!failed && status.stage !== 'ready' && <span className="blink"> ·</span>}
            </h1>
            <p>Windows 桌面壳 · 正在接管后端生命周期</p>
          </div>
        </div>

        <div className={`status${failed ? ' failed' : ''}`}>
          {headline}
          {hint && (
            <div style={{ marginTop: 6, color: 'var(--muted)', fontSize: 12 }}>{hint}</div>
          )}
        </div>

        <div className={`bar${failed ? ' failed' : ''}`}>
          <span style={{ width: `${Math.round(progress * 100)}%` }} />
        </div>

        <div className="steps">
          {STAGE_ORDER.map((s, i) => (
            <span
              key={s}
              className={`step${status.stage === s ? ' active' : ''}${
                stageIndex > i || status.stage === 'ready' ? ' done' : ''
              }`}
            >
              {i + 1}. {STAGE_LABEL[s]}
            </span>
          ))}
        </div>

        <div className="probes">
          <span className="probe">
            <i className={`dot-ind${status.health.api ? ' up' : ''}`} /> API :8001
          </span>
          <span className="probe">
            <i className={`dot-ind${status.health.web ? ' up' : ''}`} /> Web :3782
          </span>
          <span className="probe">状态: {status.state}</span>
        </div>

        <div className="lmstudio">
          <div className="lmstudio-head">
            <span className="probe">
              <i className={`dot-ind${lmStudio.detected ? ' up' : ''}`} /> LM Studio
            </span>
            {lmStudio.detected && (
              <span className="lmstudio-loaded">{loadedCount} 个模型已加载</span>
            )}
            <button className="ghost mini" onClick={refreshLmStudio}>
              刷新
            </button>
          </div>
          {lmStudio.detected && lmStudio.models.length > 0 && (
            <div className="lmstudio-models">
              {lmStudio.models.slice(0, 6).map(m => (
                <div key={m.id} className="lmstudio-model">
                  <i className={`dot-ind${m.state === 'loaded' ? ' up' : ''}`} />
                  <span className="lmstudio-model-id" title={m.id}>
                    {m.id}
                  </span>
                  <button className="ghost mini" onClick={() => toggleModel(m)}>
                    {m.state === 'loaded' ? '卸载' : '加载'}
                  </button>
                </div>
              ))}
            </div>
          )}
          {!lmStudio.detected && (
            <div className="lmstudio-off">未检测到 LM Studio(可选,用于本地模型)</div>
          )}
        </div>

        <div className="console" ref={consoleRef}>
          {logs.length === 0 ? (
            <div className="console-empty">等待后端输出...</div>
          ) : (
            logs.map((l, i) => (
              <div key={i} className={`console-line ${l.stream}`}>
                <span className="ts">{l.ts}</span>
                <span className="stream">[{l.stream}]</span>
                <span className="text">{l.line}</span>
              </div>
            ))
          )}
        </div>

        {failed && (
          <div className="actions">
            <button onClick={retry} disabled={restarting}>
              {restarting ? '正在重试...' : '重试启动'}
            </button>
            <button
              className="ghost"
              onClick={() => navigator.clipboard?.writeText(logs.map(l => l.line).join('\n'))}
            >
              复制日志
            </button>
          </div>
        )}

        {status.state === 'ready' && (
          <div className="actions">
            <button onClick={() => window.location.assign(WEB_URL)}>
              打开 DeepTutor →
            </button>
            <span className="ready-hint">{WEB_URL}</span>
          </div>
        )}

        <div className="updater">
          {!update && (
            <button className="ghost mini" onClick={checkUpdate}>
              检查更新
            </button>
          )}
          {update && !update.available && (
            <div className="updater-row">
              <span className="updater-ok">已是最新版本 (v{update.current_version})</span>
              <button className="ghost mini" onClick={checkUpdate}>
                重新检查
              </button>
            </div>
          )}
          {update && update.available && (
            <div className="updater-row">
              <div className="updater-info">
                <span className="updater-new">
                  发现新版本 v{update.latest_version}
                </span>
                {update.body && <span className="updater-body">{update.body}</span>}
              </div>
              <button className="mini" onClick={doUpdate} disabled={updating}>
                {updating ? '下载中...' : '立即更新'}
              </button>
            </div>
          )}
        </div>

        <div className="meta">
          <span>{status.python ? `Python: ${status.python}` : 'Python: 未探测'}</span>
          <span>{status.pid ? `pid: ${status.pid}` : 'pid: -'}</span>
          <span>耗时 {elapsed}s</span>
        </div>
      </div>
    </div>
  )
}
