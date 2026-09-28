import { useEffect, useRef } from 'react'
import { LOGO_LINES, LOGO_OFFSETS, LOGO_TOTAL_LEN } from '../assets/logo-lines'

/**
 * 开机动画:把 DeepTutor 的轮廓"一笔一笔画出来",画到哪儿由**真实启动阶段**决定。
 *
 * # 为什么是折返,不是一条平顺的 transition
 * 单段 transition 无论怎么调 cubic-bezier,值都是从起点**单调**走到终点,
 * 视觉上永远是"平顺过渡"。要让笔画看起来是"冲出去再落定",必须让它**画过头再退回来**。
 *
 * # ⚠️ 冲出只在「阶段跳变」时做,不能每次进度更新都做
 * 阶段内部进度是连续爬升的(见 App.tsx 的 creep)。如果每次爬升都冲一次:
 * 落定定时器是 240ms,而爬升每 120ms 就来一次 —— 定时器还没触发就被下一次
 * 冲出用 token 作废了,于是过冲**永远收不回来**,画面会一直停在过冲位置。
 * 所以这里区分两种更新:
 *   - 阶段跳变 -> 冲出 + 收回(有折返)
 *   - 阶段内爬升 -> 平顺推进(不冲)
 * 且冲出进行中时直接忽略爬升更新,免得把过冲打断。
 *
 * # ⚠️ 别给 stroke-dashoffset 加 transition-delay(错峰)
 * 每条轮廓延迟 12ms 看着是"依次起笔",但爬升是每帧都在写值的 ——
 * 过渡永远停在延迟期就被下一次写入重启,**根本没机会执行**,
 * 计算值会钉死在旧位置(实测设置值都写到 736 了,画面还停在 1330)。
 * 顺序起笔本来就是 applyDrawn 按累计长度分摊出来的,不需要延迟再叠一层。
 * 同理,爬升用的基线过渡必须**短 + 线性**:ease-in-out 起步斜率为 0,
 * 高频写入下每帧只挪 0.1%,一样会饿死。
 */

/** viewBox 裁到图案实际包围盒(x 105..919, y 156..868)再留一圈边,免得四周空太多。 */
const VIEWBOX = '95 146 834 732'

/**
 * ⚠️ 必须与 CSS 里的 --burst-ms / --settle-ms 保持一致。
 * CSS 那边能读变量,JS 的 setTimeout 只能写死 —— 改动时两边一起改。
 */
const BURST_MS = 240
const SETTLE_MS = 200

/**
 * 过冲量:按比例取再夹住,别写死。总周长约 10300,0.014 ≈ 145,夹到 [60,150]。
 * 换算到画面:视口约 210px 对应 834 个用户单位,145 单位 ≈ 37px 的笔尖冲出量。
 */
const OVERSHOOT = Math.min(150, Math.max(60, LOGO_TOTAL_LEN * 0.014))

/**
 * 系统开了"减弱动态效果"就别做冲出/落定 —— 那时所有过渡都被压到 1ms,
 * 折返会退化成"先跳过去停 240ms 再跳回来"的闪烁,比不做还糟。
 */
const REDUCED =
  typeof window !== 'undefined' &&
  typeof window.matchMedia === 'function' &&
  window.matchMedia('(prefers-reduced-motion: reduce)').matches

/** 把"已画到的总长度"分摊到每一条轮廓上。 */
function applyDrawn(nodes: ArrayLike<SVGPathElement>, drawn: number) {
  for (let i = 0; i < LOGO_LINES.length; i++) {
    const el = nodes[i]
    if (!el) continue
    const line = LOGO_LINES[i]
    const visible = Math.max(0, Math.min(line.len, drawn - LOGO_OFFSETS[i]))
    el.style.strokeDashoffset = String(line.len - visible)
    // 正在画的那一条高亮一点,视觉上能看出"笔走到哪儿了"
    el.classList.toggle('drawing', visible > 0 && visible < line.len)
  }
}

export interface BootStageProps {
  /** 0..1,由外层按真实启动阶段算出(含阶段内爬升) */
  progress: number
  /** 阶段标识。它变化时才会触发"冲出+落定";progress 连续变化不冲。 */
  stageKey: string
  /** 当前阶段文案 */
  label: string
  /** 阶段详情(次要一行,可空) */
  hint?: string
  ready: boolean
  failed: boolean
  /** 版本号一行,例如 "桌面壳 v0.2.6 · 后端 1.6.12" */
  versionLine?: string
}

export default function BootStage({
  progress,
  stageKey,
  label,
  hint,
  ready,
  failed,
  versionLine,
}: BootStageProps) {
  const svgRef = useRef<SVGSVGElement>(null)
  /** 作废"等落定"的定时器:阶段连续推进时,旧定时器会把几何拽回旧位置。 */
  const tokenRef = useRef(0)
  const firstRef = useRef(true)
  const lastStageRef = useRef<string | null>(null)
  /** 冲出进行中的标志:期间忽略爬升更新 */
  const inBurstRef = useRef(false)
  /** 渲染期就能读到的最新进度,落定时用它(爬升可能又前进了一点) */
  const progressRef = useRef(progress)
  progressRef.current = progress

  useEffect(() => {
    const nodes = svgRef.current?.querySelectorAll<SVGPathElement>('path[data-line]')
    if (!nodes) return

    // ⚠️ 冲出还没落定:直接忽略。
    // 注意要先判断再动 token —— 否则会把在飞的那一轮的 token 顶掉,
    // 导致它自我作废、过冲再也没人收回。
    if (inBurstRef.current) return

    const tok = ++tokenRef.current
    const isStageJump = stageKey !== lastStageRef.current
    lastStageRef.current = stageKey

    // 首次挂载 + 阶段内爬升 + 减弱动态:平顺推进,不冲
    if (REDUCED || firstRef.current || !isStageJump) {
      firstRef.current = false
      applyDrawn(nodes, progress * LOGO_TOTAL_LEN)
      return
    }

    // ⚠️ 锚点 = 这次跳变要停在哪儿,冲出和收回都以它为基准。
    // 不能拿「收回那一刻的最新进度」当收回目标:爬升在冲出的 240ms 里会往上顶,
    // 实测正好顶掉 108 个单位,而过冲才 110 —— 收回量被抵消到只剩 2 个单位,
    // 画面上等于没有折返。锚定住,收回量才恒等于 OVERSHOOT。
    const anchor = progress * LOGO_TOTAL_LEN
    inBurstRef.current = true
    // ① 先换曲线(冲出是"猛起步+强减速")
    for (let i = 0; i < nodes.length; i++) nodes[i].classList.add('goup')
    requestAnimationFrame(() => {
      if (tok !== tokenRef.current) return
      // ② 再改几何:一口气画到「锚点 + OVERSHOOT」
      applyDrawn(nodes, anchor + OVERSHOOT)
      window.setTimeout(() => {
        if (tok !== tokenRef.current) return
        // ③ 收回:换落定曲线,退回锚点。
        for (let i = 0; i < nodes.length; i++) {
          nodes[i].classList.remove('goup')
          nodes[i].classList.add('goback')
        }
        applyDrawn(nodes, anchor)
        window.setTimeout(() => {
          if (tok !== tokenRef.current) return
          // ④ 交还给爬升:这 440ms 里爬升又前进了一截,这里换成基线过渡平顺接上,
          //    不然就是一次肉眼可见的跳。取 max 是防进度回退(见 App.tsx)。
          for (let i = 0; i < nodes.length; i++) nodes[i].classList.remove('goback')
          applyDrawn(nodes, Math.max(anchor, progressRef.current * LOGO_TOTAL_LEN))
          inBurstRef.current = false
        }, SETTLE_MS)
      }, BURST_MS)
    })
  }, [progress, stageKey])

  return (
    <div className={`boot${failed ? ' boot--failed' : ''}${ready ? ' boot--ready' : ''}`}>
      <div className="boot-mark">
        <svg
          ref={svgRef}
          className="boot-lines"
          viewBox={VIEWBOX}
          role="img"
          aria-label="DeepTutor 启动中"
        >
          <defs>
            {/* 笔触沿用原 logo 的走向:柔和蓝 → 灰绿 → 暖橙 → 柔红 */}
            <linearGradient id="bootStroke" x1="0" y1="0" x2="1" y2="1">
              <stop offset="0%" stopColor="#6FB3E0" />
              <stop offset="38%" stopColor="#7FA89B" />
              <stop offset="70%" stopColor="#E9A76B" />
              <stop offset="100%" stopColor="#D97B72" />
            </linearGradient>
          </defs>
          {LOGO_LINES.map((line, i) => (
            <path
              key={i}
              data-line=""
              d={line.d}
              style={{
                strokeDasharray: line.len,
                strokeDashoffset: line.len,
              }}
            />
          ))}
        </svg>
        {/* 画完之后切成真正的品牌 mark:线稿淡出,成品淡入。
            不是 morph(两者形状对不齐会很难看),是"先出后入"的交接。 */}
        <div className="boot-real" />
      </div>

      <div className="boot-label">
        <span className="boot-name">DeepTutor</span>
        {!failed && !ready && <span className="boot-dots" aria-hidden="true" />}
      </div>

      <div className="boot-status">{ready ? '正在打开 DeepTutor…' : label}</div>
      {hint && <div className="boot-hint">{hint}</div>}

      {versionLine && <div className="boot-version">{versionLine}</div>}
    </div>
  )
}
