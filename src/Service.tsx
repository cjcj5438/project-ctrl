import { useCallback, useEffect, useRef, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import {
  api,
  errorText,
  type DetectResult,
  type Phase,
  type ProjectInfo,
  type ServiceConfig,
  type ServiceStatus,
} from './api'

const PHASE_LABEL: Record<Phase, string> = {
  Idle: '未运行',
  Installing: '安装中',
  Building: '构建中',
  Running: '运行中',
  Stopped: '已停止',
  Crashed: '已崩溃',
  Failed: '失败',
}

const DOT: Record<Phase, string> = {
  Idle: 'bg-neutral-400',
  Installing: 'bg-amber-500',
  Building: 'bg-amber-500',
  Running: 'bg-emerald-500',
  Stopped: 'bg-neutral-400',
  Crashed: 'bg-red-500',
  Failed: 'bg-red-500',
}

/** argv 输入框 <-> 数组：空格分隔（不支持引号，含空格的参数请用「编辑」以外的场景）。 */
const joinArgv = (a: string[] | null): string => (a ?? []).join(' ')

/** 服务面板：配方、状态、启停、日志。折叠时只显示一行摘要。 */
export default function ServiceSection({
  project,
  onError,
}: {
  project: ProjectInfo
  onError: (m: string | null) => void
}) {
  const [opened, setOpened] = useState(false)
  const [loaded, setLoaded] = useState(false)
  const [cfg, setCfg] = useState<ServiceConfig | null>(null)
  const [detected, setDetected] = useState<DetectResult | null>(null)
  const [status, setStatus] = useState<ServiceStatus | null>(null)
  const [lines, setLines] = useState<string[]>([])
  const [problems, setProblems] = useState<string[]>([])
  const [busy, setBusy] = useState(false)
  const [editing, setEditing] = useState(false)
  const logRef = useRef<HTMLPreElement | null>(null)

  const load = useCallback(async () => {
    try {
      const [c, st] = await Promise.all([
        api.serviceConfig(project.id),
        api.serviceStatus(),
      ])
      setCfg(c)
      setLoaded(true)
      setStatus(st.find((s) => s.project_id === project.id) ?? null)
      setLines(await api.serviceLogs(project.id))
      if (!c) setDetected(await api.detectService(project.id))
      if (c) setProblems(await api.servicePreflight(project.id, 'start'))
      onError(null)
    } catch (e) {
      onError(errorText(e))
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [project.id])

  useEffect(() => {
    if (!opened) return
    void load()
  }, [opened, load])

  // 状态与日志订阅：只在展开时挂，避免列表页/详情页常驻监听
  useEffect(() => {
    if (!opened) return
    let alive = true
    const unlisteners: UnlistenFn[] = []
    void (async () => {
      const u1 = await listen<ServiceStatus>('svc:state', (e) => {
        if (!alive || e.payload.project_id !== project.id) return
        setStatus(e.payload)
        void refreshPreflight()
      })
      const u2 = await listen<{ project_id: number; lines: string[] }>('svc:log', (e) => {
        if (!alive || e.payload.project_id !== project.id) return
        setLines((prev) => [...prev, ...e.payload.lines].slice(-2000))
      })
      unlisteners.push(u1, u2)
    })()
    return () => {
      alive = false
      unlisteners.forEach((u) => u())
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [opened, project.id])

  // 日志自动滚到底
  useEffect(() => {
    const el = logRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [lines])

  const refreshPreflight = async () => {
    try {
      setProblems(await api.servicePreflight(project.id, 'start'))
    } catch {
      /* 预检失败不覆盖已有提示 */
    }
  }

  const act = async (fn: () => Promise<void>) => {
    setBusy(true)
    try {
      await fn()
      onError(null)
    } catch (e) {
      onError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const adopt = (candidate: string[]) =>
    act(async () => {
      const d = detected
      const next: ServiceConfig = {
        install: d?.install ?? null,
        build: d?.build ?? null,
        start: candidate,
        dev_port: d?.dev_port ?? null,
        requires_ports: d?.requires_ports ?? [],
        env: {},
        node_pin: d?.node_pin ?? null,
        toolchain: d?.toolchain ?? [],
        origin: 'auto',
      }
      await api.saveServiceConfig(project.id, next)
      setCfg(next)
      setProblems(await api.servicePreflight(project.id, 'start'))
    })

  const running = status?.phase === 'Running'
  const transitional = status?.phase === 'Installing' || status?.phase === 'Building'

  // cfg 为 null 有两种含义：还没读过（未展开）与确实没有配方，用 loaded 区分
  const summary = !loaded
    ? '展开查看'
    : !cfg
      ? '未配置'
      : status
        ? `${PHASE_LABEL[status.phase]}${status.port ? ` :${status.port}` : ''}`
        : '已配置'

  return (
    <details
      className="mt-4 rounded-lg border border-neutral-300 dark:border-neutral-700"
      onToggle={(e) => {
        if (e.currentTarget.open) setOpened(true)
      }}
    >
      <summary className="cursor-pointer px-3 py-2 text-sm font-semibold">
        服务
        {status && (
          <span
            className={`ml-2 inline-block h-2 w-2 rounded-full align-middle ${DOT[status.phase]}`}
            aria-hidden
          />
        )}
        <span className="ml-1 font-normal text-neutral-500">{summary}</span>
      </summary>

      <div className="border-t border-neutral-300 p-3 dark:border-neutral-700">
        {!cfg ? (
          <div>
            <div className="text-sm text-neutral-500">
              该项目还没有配方。下面是自动探测到的启动命令，选一个即可（也可以稍后手工编辑）。
            </div>
            <DetectProblems detect={detected} />
            <div className="mt-2 space-y-1.5">
              {(detected?.start_candidates ?? []).map((c) => (
                <div key={c.join(' ')} className="flex items-center gap-2">
                  <code className="rounded bg-neutral-100 px-1.5 py-0.5 font-mono text-[11px] dark:bg-neutral-800">
                    {c.join(' ')}
                  </code>
                  <button
                    onClick={() => void adopt(c)}
                    disabled={busy}
                    className="rounded border border-neutral-300 px-2 py-0.5 text-xs disabled:opacity-40 dark:border-neutral-700"
                  >
                    使用此命令
                  </button>
                </div>
              ))}
              {(detected?.start_candidates ?? []).length === 0 && (
                <div className="text-xs text-neutral-500">没有探测到可启动脚本，请手工填写。</div>
              )}
            </div>
            <button
              onClick={() => setEditing(true)}
              className="mt-3 rounded border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700"
            >
              手工填写命令…
            </button>
          </div>
        ) : (
          <>
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-neutral-600 dark:text-neutral-300">
              <span className="inline-flex items-center gap-1.5">
                <span className={`inline-block h-2 w-2 rounded-full ${DOT[status?.phase ?? 'Idle']}`} />
                {PHASE_LABEL[status?.phase ?? 'Idle']}
              </span>
              {status?.port != null && <span>端口 {status.port}</span>}
              {status?.pid != null && <span>PID {status.pid}</span>}
              {status?.exit_code != null && <span>退出码 {status.exit_code}</span>}
              <span>来源 {cfg.origin}</span>
            </div>
            {status?.note && (
              <div className="mt-1 text-xs text-amber-700 dark:text-amber-400">{status.note}</div>
            )}
            {problems.length > 0 && (
              <ul className="mt-1 list-inside list-disc text-xs text-red-600 dark:text-red-400">
                {problems.map((p) => (
                  <li key={p}>{p}</li>
                ))}
              </ul>
            )}

            <div className="mt-3 flex flex-wrap gap-2 text-xs">
              <button
                onClick={() => void act(async () => void (await api.serviceStart(project.id)))}
                disabled={busy || running || transitional || problems.length > 0}
                title={problems[0] ?? ''}
                className="rounded bg-emerald-600 px-2 py-1 text-white disabled:opacity-40"
              >
                启动
              </button>
              <button
                onClick={() => void act(async () => void (await api.serviceStop(project.id)))}
                disabled={busy || !status || status.phase === 'Stopped' || status.phase === 'Idle'}
                className="rounded border border-neutral-300 px-2 py-1 disabled:opacity-40 dark:border-neutral-700"
              >
                停止
              </button>
              <button
                onClick={() =>
                  void act(async () => {
                    await api.serviceStop(project.id)
                    await api.serviceStart(project.id)
                  })
                }
                disabled={busy || transitional}
                className="rounded border border-neutral-300 px-2 py-1 disabled:opacity-40 dark:border-neutral-700"
              >
                重启
              </button>
              <button
                onClick={() => void act(async () => void (await api.serviceStep(project.id, 'install')))}
                disabled={busy || transitional || running || !cfg.install}
                className="rounded border border-neutral-300 px-2 py-1 disabled:opacity-40 dark:border-neutral-700"
              >
                安装依赖
              </button>
              <button
                onClick={() => void act(async () => void (await api.serviceStep(project.id, 'build')))}
                disabled={busy || transitional || running || !cfg.build}
                className="rounded border border-neutral-300 px-2 py-1 disabled:opacity-40 dark:border-neutral-700"
              >
                构建
              </button>
              <button
                onClick={() => void act(async () => void (await api.openServiceUrl(project.id)))}
                disabled={status?.port == null}
                className="rounded border border-neutral-300 px-2 py-1 disabled:opacity-40 dark:border-neutral-700"
              >
                打开
              </button>
              <button
                onClick={() => setEditing(true)}
                className="ml-auto rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
              >
                编辑命令
              </button>
            </div>

            <div className="mt-3 overflow-hidden rounded border border-neutral-300 dark:border-neutral-700">
              <div className="flex items-center gap-2 border-b border-neutral-300 bg-neutral-100 px-2 py-1 dark:border-neutral-700 dark:bg-neutral-800">
                <span className="font-mono text-[11px] font-semibold">日志 · {lines.length} 行</span>
                <button
                  onClick={() => setLines([])}
                  className="ml-auto rounded border border-neutral-300 px-2 py-0.5 text-[11px] dark:border-neutral-600"
                >
                  清空
                </button>
              </div>
              <pre
                ref={logRef}
                className="max-h-[40vh] select-text overflow-auto whitespace-pre-wrap p-2 font-mono text-[11px] leading-relaxed"
              >
                {lines.length === 0 ? '（暂无输出）' : lines.join('\n')}
              </pre>
            </div>
          </>
        )}

        {editing && (
          <EditForm
            initial={cfg}
            onCancel={() => setEditing(false)}
            onSave={(next) =>
              void act(async () => {
                await api.saveServiceConfig(project.id, next)
                setCfg(next)
                setEditing(false)
                setProblems(await api.servicePreflight(project.id, 'start'))
              })
            }
          />
        )}
      </div>
    </details>
  )
}

/** 探测阶段的人读说明（技术栈未识别、engines 非精确版本等）。 */
function DetectProblems({ detect }: { detect: DetectResult | null }) {
  if (!detect || detect.problems.length === 0) return null
  return (
    <ul className="mt-2 list-inside list-disc text-xs text-neutral-500">
      {detect.problems.map((p) => (
        <li key={p}>{p}</li>
      ))}
    </ul>
  )
}

function EditForm({
  initial,
  onSave,
  onCancel,
}: {
  initial: ServiceConfig | null
  onSave: (c: ServiceConfig) => void
  onCancel: () => void
}) {
  const [install, setInstall] = useState(joinArgv(initial?.install ?? null))
  const [build, setBuild] = useState(joinArgv(initial?.build ?? null))
  const [start, setStart] = useState(joinArgv(initial?.start ?? null))
  const [port, setPort] = useState(initial?.dev_port?.toString() ?? '')
  const [requires, setRequires] = useState((initial?.requires_ports ?? []).join(' '))
  const [pin, setPin] = useState(initial?.node_pin ?? '')

  const input =
    'w-full rounded border border-neutral-300 bg-white px-2 py-1 font-mono text-xs dark:border-neutral-700 dark:bg-neutral-800'

  const save = () => {
    const cfg: ServiceConfig = {
      install: install.trim() ? install.split(/\s+/).filter(Boolean) : null,
      build: build.trim() ? build.split(/\s+/).filter(Boolean) : null,
      start: start.split(/\s+/).filter(Boolean),
      dev_port: port.trim() ? Number(port.trim()) : null,
      requires_ports: requires
        .split(/\s+/)
        .filter(Boolean)
        .map(Number)
        .filter((n) => Number.isFinite(n)),
      env: initial?.env ?? {},
      node_pin: pin.trim() ? pin.trim() : null,
      toolchain: initial?.toolchain ?? [],
      origin: 'manual',
    }
    onSave(cfg)
  }

  return (
    <div className="mt-3 rounded border border-neutral-300 p-3 dark:border-neutral-700">
      <div className="text-xs font-semibold">编辑命令（空格分隔参数）</div>
      <div className="mt-2 space-y-2">
        <label className="block text-[11px] text-neutral-500">
          安装
          <input value={install} onChange={(e) => setInstall(e.target.value)} placeholder="pnpm install" className={input} />
        </label>
        <label className="block text-[11px] text-neutral-500">
          构建
          <input value={build} onChange={(e) => setBuild(e.target.value)} placeholder="pnpm run build" className={input} />
        </label>
        <label className="block text-[11px] text-neutral-500">
          启动（必填）
          <input value={start} onChange={(e) => setStart(e.target.value)} placeholder="pnpm dev" className={input} />
        </label>
        <div className="flex gap-2">
          <label className="block flex-1 text-[11px] text-neutral-500">
            开发端口
            <input value={port} onChange={(e) => setPort(e.target.value)} placeholder="留空则不管理端口" className={input} />
          </label>
          <label className="block flex-1 text-[11px] text-neutral-500">
            依赖端口（须已监听）
            <input value={requires} onChange={(e) => setRequires(e.target.value)} placeholder="5432" className={input} />
          </label>
          <label className="block flex-1 text-[11px] text-neutral-500">
            node 版本
            <input value={pin} onChange={(e) => setPin(e.target.value)} placeholder="24.16.0" className={input} />
          </label>
        </div>
      </div>
      <div className="mt-3 flex justify-end gap-2 text-xs">
        <button onClick={onCancel} className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700">
          取消
        </button>
        <button
          onClick={save}
          disabled={start.split(/\s+/).filter(Boolean).length === 0}
          className="rounded bg-blue-600 px-2 py-1 text-white disabled:opacity-40"
        >
          保存
        </button>
      </div>
    </div>
  )
}
