import { useEffect, useMemo, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import BootStage from './components/BootStage'

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

/** 与 Rust 侧 updater::Phase 对应。 */
type UpdatePhase = 'downloading' | 'installing'

/** 与 Rust 侧 updater::Progress 对应,由 `update://progress` 事件推来。 */
interface UpdateProgress {
  phase: UpdatePhase
  downloaded: number
  total: number | null
  percent: number | null
}

/** 后端(deeptutor)版本分布,对应 Rust 侧 hotupdate::Versions。 */
interface BackendVersions {
  effective: string | null
  bundled: string | null
  using_overlay: boolean
  overlay_dir: string
}

/** 后端更新检查结果,对应 Rust 侧 hotupdate::UpdateInfo。 */
interface BackendUpdateInfo {
  available: boolean
  current: string | null
  bundled: string | null
  latest: string | null
  using_overlay: boolean
  source: string
  index_url: string
  wheel_size: number | null
}

/** 与 Rust 侧 hotupdate::Phase 对应。 */
type BackendPhase =
  | 'stopping'
  | 'resolving'
  | 'downloading'
  | 'extracting'
  | 'activating'
  | 'restarting'

/** 与 Rust 侧 hotupdate::Progress 对应,由 `backend-update://progress` 推来。 */
interface BackendProgress {
  phase: BackendPhase
  message: string
  index: number
  total: number
  downloaded: number
  bytes_total: number | null
}

const BACKEND_PHASE_LABEL: Record<BackendPhase, string> = {
  stopping: '正在停止后端',
  resolving: '正在解析依赖',
  downloading: '正在下载后端',
  extracting: '正在解压',
  activating: '正在激活',
  restarting: '正在重启后端',
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

/**
 * 每个启动阶段对应"轮廓至少该画到多少"。
 *
 * 只用阶段序号算进度会有一个毛病:每个阶段停留时长差别很大(探测 Python
 * 一两秒,等服务就绪可能二十秒),进度条会"跳一下然后僵住",看起来像卡死。
 * 所以这里给的是**下限**,阶段内部再让进度缓慢爬升(见下面的 creep)。
 */
const STAGE_FLOOR: Record<Stage, number> = {
  idle: 0.06,
  locating_python: 0.26,
  checking_deps: 0.44,
  starting: 0.62,
  probing: 0.8,
  ready: 1,
  failed: 0.06,
}

/** 阶段内最多爬到「到下一阶段差值的这个比例」—— 留一截,免得抢跑。 */
const CREEP_CAP = 0.8
/** 爬升时间常数(ms):越大越慢,4s 是"看着一直在动、又不至于假"的量级。 */
const CREEP_TAU = 4000

export default function App() {
  const [status, setStatus] = useState<StatusSnapshot>(EMPTY)
  const [logs, setLogs] = useState<LogLine[]>([])
  const [elapsed, setElapsed] = useState(0)
  const [restarting, setRestarting] = useState(false)
  const [lmStudio, setLmStudio] = useState<LmStudioInfo>({ detected: false, base_url: null, models: [] })
  const [update, setUpdate] = useState<UpdateCheck | null>(null)
  const [updating, setUpdating] = useState(false)
  const [updateProgress, setUpdateProgress] = useState<UpdateProgress | null>(null)
  const [backendVer, setBackendVer] = useState<BackendVersions | null>(null)
  const [backendUpdate, setBackendUpdate] = useState<BackendUpdateInfo | null>(null)
  const [backendUpdating, setBackendUpdating] = useState(false)
  const [backendProgress, setBackendProgress] = useState<BackendProgress | null>(null)
  const [showDiag, setShowDiag] = useState(false)
  /** 只用来每 120ms 触发一次重算;爬升量本身在 drawProgress 里现算(见那里的注释) */
  const [tick, setTick] = useState(0)
  const stageSinceRef = useRef(Date.now())
  const lastStageRef = useRef<string | null>(null)
  /** 进度只增不减:后端偶尔会回退阶段,画面不该跟着往回缩 */
  const progressRef = useRef(STAGE_FLOOR.idle)
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
    invoke<BackendVersions>('backend_versions').then(setBackendVer).catch(console.error)
    refreshLmStudio()

    const unState = listen<StatusSnapshot>('backend://state', e => setStatus(e.payload))
    const unLog = listen<LogLine>('backend://log', e =>
      setLogs(prev => [...prev, e.payload].slice(-300)),
    )
    // 下载/安装进度。注意:更新也可能从托盘菜单发起,那时本页早已被
    // DeepTutor Web UI 顶掉、收不到事件 —— 托盘侧会用 tooltip 兜底显示。
    const unProgress = listen<UpdateProgress>('update://progress', e => setUpdateProgress(e.payload))

    // 后端热更新:Rust 侧启动后会静默查一次,有新版才广播 available。
    const unBackendProgress = listen<BackendProgress>('backend-update://progress', e =>
      setBackendProgress(e.payload),
    )
    const unBackendAvail = listen<BackendUpdateInfo>('backend-update://available', e =>
      setBackendUpdate(e.payload),
    )

    const timer = setInterval(() => setElapsed(v => v + 1), 1000)
    return () => {
      unState.then(f => f())
      unLog.then(f => f())
      unProgress.then(f => f())
      unBackendProgress.then(f => f())
      unBackendAvail.then(f => f())
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

  // ⚠️ 阶段一变就重置爬升计时,而且必须在**渲染期**做,不能放 useEffect。
  // effect 要等这一轮渲染跑完才执行,而这一轮已经拿旧阶段的时间戳算过进度了 ——
  // 那一帧的进度会把上一阶段攒下的爬升全算进来(虚高一截),下一次 tick 又拽回
  // 新阶段的下限。开机动画正是在这一帧取"冲出/收回"的锚点,虚高会把收回量吃掉
  // (实测过冲 110 单位被抵消到只剩 2 单位,画面上等于没有折返)。
  if (lastStageRef.current !== status.stage) {
    lastStageRef.current = status.stage
    stageSinceRef.current = Date.now()
  }

  useEffect(() => {
    const t = setInterval(() => setTick(v => v + 1), 120)
    return () => clearInterval(t)
  }, [])

  // 失败才自动展开诊断面板。成功启动的用户不该看到探针、模型列表和日志。
  useEffect(() => {
    if (status.stage === 'failed' || status.state === 'failed') setShowDiag(true)
  }, [status.stage, status.state])

  const [headline, hint] = useMemo(() => {
    const parts = status.message.split('\n')
    return [parts[0] ?? '', parts.slice(1).join('\n')]
  }, [status.message])

  // ---- 自绘动画的进度:阶段下限 + 阶段内爬升 ----
  const drawProgress = useMemo(() => {
    const idx = STAGE_ORDER.indexOf(status.stage)
    const floor = STAGE_FLOOR[status.stage] ?? STAGE_FLOOR.idle
    const nextStage = idx >= 0 ? STAGE_ORDER[idx + 1] : STAGE_ORDER[0]
    const nextFloor = nextStage ? (STAGE_FLOOR[nextStage] ?? 1) : 1
    const span = Math.max(0, nextFloor - floor)
    // 爬升量现算,与 stageSinceRef 同源。存进 state 会晚一拍:阶段刚切换的那一帧
    // 拿到的还是上一阶段的值,进度就虚高了(见上面 reset 那段的注释)。
    const creep = 1 - Math.exp(-(Date.now() - stageSinceRef.current) / CREEP_TAU)
    return Math.min(1, floor + span * Math.min(CREEP_CAP, creep))
  }, [status.stage, tick])

  // 只增不减
  progressRef.current = Math.max(progressRef.current, drawProgress)
  const bootProgress = status.stage === 'failed' ? progressRef.current : drawProgress

  const versionLine = [
    '桌面壳 v' + import.meta.env.VITE_SHELL_VERSION,
    backendVer?.effective ? '后端 ' + backendVer.effective : null,
  ]
    .filter(Boolean)
    .join(' · ')

  const failed = status.stage === 'failed'
  const stageIndex = STAGE_ORDER.indexOf(status.stage)
  const progress = failed ? 1 : stageIndex < 0 ? 0.05 : (stageIndex + 1) / STAGE_ORDER.length

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
    setUpdateProgress(null)
    try {
      // Windows 上安装器拉起后本进程会退出,这个 await 通常不会返回。
      await invoke('install_update')
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `更新失败: ${e}` }])
      setUpdating(false)
      setUpdateProgress(null)
    }
  }

  const checkBackendUpdate = async () => {
    try {
      setBackendUpdate(await invoke<BackendUpdateInfo>('check_backend_update'))
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `检查后端更新失败: ${e}` }])
    }
  }

  const doBackendUpdate = async () => {
    setBackendUpdating(true)
    setBackendProgress(null)
    try {
      // 与桌面壳更新不同:这里只重启 Python 子进程,应用不退出。
      await invoke('install_backend_update')
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `后端更新失败: ${e}` }])
    } finally {
      setBackendUpdating(false)
      setBackendProgress(null)
      setBackendUpdate(null)
      invoke<BackendVersions>('backend_versions').then(setBackendVer).catch(console.error)
    }
  }

  const rollbackBackend = async () => {
    try {
      const msg = await invoke<string>('rollback_backend')
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: msg }])
      setBackendUpdate(null)
      invoke<BackendVersions>('backend_versions').then(setBackendVer).catch(console.error)
    } catch (e) {
      setLogs(prev => [...prev, { ts: '--:--:--', stream: 'shell', line: `回退失败: ${e}` }])
    }
  }

  // 后端更新的进度:优先用字节比例(下载阶段),否则用包序号。
  const backendPercent = (() => {
    const p = backendProgress
    if (!p) return null
    if (p.bytes_total && p.bytes_total > 0) {
      return Math.min(100, (p.downloaded / p.bytes_total) * 100)
    }
    if (p.total > 0 && p.index > 0) return (p.index / p.total) * 100
    return null
  })()

  const loadedCount = lmStudio.models.filter(m => m.state === 'loaded').length

  // 正常启动:只给开机动画。
  // 诊断面板(步骤条 / 探针 / 模型 / 日志)默认不出现 —— 只在失败时自动展开,
  // 或用户自己点开。成功启动的用户永远不需要看到那些。
  if (!showDiag) {
    return (
      <div className="shell">
        <div className="boot-wrap">
          <BootStage
            progress={bootProgress}
            stageKey={status.stage}
            label={headline}
            hint={hint}
            ready={status.state === 'ready'}
            failed={failed}
            versionLine={versionLine}
          />
          <button className="ghost mini boot-diag-toggle" onClick={() => setShowDiag(true)}>
            诊断信息
          </button>
        </div>
      </div>
    )
  }

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
          {!failed && (
            <button className="ghost mini" style={{ marginLeft: 'auto' }} onClick={() => setShowDiag(false)}>
              收起
            </button>
          )}
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
          {!update && !updating && (
            <button className="ghost mini" onClick={checkUpdate}>
              检查更新
            </button>
          )}
          {updating && (
            <div className="updater-row">
              <div className="updater-info">
                <span className="updater-new">
                  {updateProgress?.phase === 'installing'
                    ? '正在安装更新,程序即将重启…'
                    : '正在下载更新…'}
                </span>
                <div className="updater-track">
                  <div
                    className={`updater-fill${updateProgress?.percent == null ? ' indeterminate' : ''}`}
                    style={{ width: `${updateProgress?.percent ?? 0}%` }}
                  />
                </div>
              </div>
              <span className="updater-pct">
                {updateProgress?.percent != null
                  ? `${updateProgress.percent.toFixed(0)}%`
                  : updateProgress?.total == null && updateProgress
                    ? '准备中'
                    : ''}
              </span>
            </div>
          )}
          {!updating && update && !update.available && (
            <div className="updater-row">
              <span className="updater-ok">已是最新版本 (v{update.current_version})</span>
              <button className="ghost mini" onClick={checkUpdate}>
                重新检查
              </button>
            </div>
          )}
          {!updating && update && update.available && (
            <div className="updater-row">
              <div className="updater-info">
                <span className="updater-new">
                  发现新版本 v{update.latest_version}
                </span>
                {update.body && <span className="updater-body">{update.body}</span>}
              </div>
              <button className="mini" onClick={doUpdate}>
                立即更新
              </button>
            </div>
          )}
        </div>

        {/* 后端(deeptutor)版本与热更新。与上面的桌面壳更新是两条独立链路:
            这条只重启 Python 子进程,应用不退出,也不用重装 230 MB 安装包。 */}
        <div className="updater backend-updater">
          <div className="updater-row">
            <div className="updater-info">
              <span className="updater-new">
                后端 {backendVer?.effective ?? '未知'}
                {backendVer?.using_overlay && <span className="backend-badge">热更新</span>}
              </span>
              {backendVer?.using_overlay && (
                <span className="updater-body">
                  安装包内置 {backendVer.bundled ?? '未知'}
                </span>
              )}
            </div>
            {!backendUpdating && (
              <div className="row-actions">
                <button className="ghost mini" onClick={checkBackendUpdate}>
                  检查后端更新
                </button>
                {backendVer?.using_overlay && (
                  <button className="ghost mini" onClick={rollbackBackend}>
                    回退
                  </button>
                )}
              </div>
            )}
          </div>

          {backendUpdating && (
            <div className="updater-row">
              <div className="updater-info">
                <span className="updater-new">
                  {backendProgress ? BACKEND_PHASE_LABEL[backendProgress.phase] : '正在准备…'}
                  {(backendProgress?.total ?? 0) > 0 && (backendProgress?.index ?? 0) > 0
                    ? ` (${backendProgress?.index}/${backendProgress?.total})`
                    : ''}
                </span>
                <span className="updater-body">{backendProgress?.message ?? ''}</span>
                <div className="updater-track">
                  <div
                    className={`updater-fill${backendPercent == null ? ' indeterminate' : ''}`}
                    style={{ width: `${backendPercent ?? 0}%` }}
                  />
                </div>
              </div>
              <span className="updater-pct">
                {backendPercent != null ? `${backendPercent.toFixed(0)}%` : ''}
              </span>
            </div>
          )}

          {!backendUpdating && backendUpdate && !backendUpdate.available && (
            <div className="updater-row">
              <span className="updater-ok">
                后端已是最新 ({backendUpdate.current ?? '未知'})
              </span>
            </div>
          )}

          {!backendUpdating && backendUpdate && backendUpdate.available && (
            <div className="updater-row">
              <div className="updater-info">
                <span className="updater-new">后端有新版本 {backendUpdate.latest}</span>
                <span className="updater-body">
                  当前 {backendUpdate.current ?? '未知'}({backendUpdate.source})
                  {backendUpdate.wheel_size
                    ? ` · 主程序包约 ${(backendUpdate.wheel_size / 1048576).toFixed(1)} MB`
                    : ''}
                </span>
              </div>
              <button className="mini" onClick={doBackendUpdate}>
                更新后端
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
