import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { UnlistenFn } from '@tauri-apps/api/event'
import {
  api,
  errorText,
  type ComposeCandidate,
  type ContainerView,
  type DetectResult,
  type Phase,
  type ProjectInfo,
  type ProjectServices,
  type ServiceOverride,
  type ServiceSpec,
  type ServiceStatus,
  type ServiceView,
} from './api'
import { Button } from './components'
import { subscribe } from './util'
const PHASE_LABEL: Record<Phase, string> = {
  Idle: '未运行',
  Step: '执行中',
  Running: '运行中',
  Stopped: '已停止',
  Crashed: '已崩溃',
  Failed: '失败',
}

const DOT: Record<Phase, string> = {
  Idle: 'bg-muted/50',
  Step: 'bg-warning',
  Running: 'bg-success',
  Stopped: 'bg-muted/50',
  Crashed: 'bg-error-solid',
  Failed: 'bg-error-solid',
}

/** argv 输入框 <-> 数组：空格分隔（含空格的参数请直接编辑描述文件）。 */
const joinArgv = (a: string[] | null | undefined): string => (a ?? []).join(' ')
const splitArgv = (s: string): string[] => s.split(/\s+/).filter(Boolean)

/** 运行态的索引键：与后端 `ServiceStatus::key` 一致（`"{project_id}/{name}"`）。 */
const statusKey = (projectId: number, name: string) => `${projectId}/${name}`

/** 每个服务独立一行状态，不依赖项目维度。 */
function statusLine(st: ServiceStatus | null) {
  if (!st) return { label: PHASE_LABEL.Idle, dot: DOT.Idle, detail: '' }
  const bits: string[] = []
  if (st.step) bits.push(st.step)
  if (st.port != null) bits.push(`:${st.port}`)
  if (st.restarts > 0) bits.push(`重启 ${st.restarts} 次`)
  return { label: PHASE_LABEL[st.phase], dot: DOT[st.phase], detail: bits.join(' · ') }
}

/**
 * docker-only 的状态快照：后端不进 supervisor、无事件通道，前端在 up/down 后
 * 主动读一次 dockerPs 推导 Running/Stopped。失败时抛给 act→onError，不写回旧态。
 */
async function dockerSnapshot(
  projectId: number,
  name: string,
  prev: ServiceStatus | null,
): Promise<ServiceStatus> {
  const views = await api.dockerPs(projectId, name)
  const running = views.filter((v) => v.state === 'running')
  const port =
    running
      .flatMap((v) => v.ports.split(','))
      .map((p) => Number(p.trim()))
      .find((n) => Number.isInteger(n) && n > 0 && n <= 65535) ?? null
  return {
    key: statusKey(projectId, name),
    project_id: projectId,
    name,
    phase: running.length > 0 ? 'Running' : 'Stopped',
    step: null,
    port,
    pid: null,
    exit_code: null,
    note:
      running.length > 0
        ? `docker 运行中：${running.map((v) => v.service).join('、')}`
        : null,
    restarts: prev?.restarts ?? 0,
    started_at: prev?.started_at ?? 0,
  }
}

/** 服务面板：多服务的状态、启停、步骤、日志、docker 子容器。折叠时只显示汇总。 */
export default function ServiceSection({
  project,
  onError,
}: {
  project: ProjectInfo
  onError: (m: string | null) => void
}) {
  const [opened, setOpened] = useState(false)
  const [data, setData] = useState<ProjectServices | null>(null)
  const [statuses, setStatuses] = useState<Record<string, ServiceStatus>>({})
  const [busy, setBusy] = useState<string | null>(null)
  const [active, setActive] = useState<string | null>(null)
  const [editing, setEditing] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)

  const load = useCallback(async () => {
    try {
      const ps = await api.projectServices(project.id)
      setData(ps)
      const next: Record<string, ServiceStatus> = {}
      ps.services.forEach((v) => {
        if (v.status) next[statusKey(project.id, v.spec.name)] = v.status
      })
      // docker-only 不进 supervisor：后端 v.status 恒为 null，整表替换会把
      // Running 快照冲回「未运行」。对这类服务现算一次 dockerSnapshot；
      // 失败（daemon 没起等）则不写入，由下面的 prev 回退保留旧快照。
      const dockerOnly = ps.services.filter(
        (v) => !v.spec.run && v.spec.docker && !(statusKey(project.id, v.spec.name) in next),
      )
      const snaps = await Promise.all(
        dockerOnly.map(async (v) => {
          try {
            return await dockerSnapshot(project.id, v.spec.name, null)
          } catch {
            return null
          }
        }),
      )
      snaps.forEach((st) => {
        if (st) next[st.key] = st
      })
      setStatuses((prev) => {
        const merged = { ...next }
        // 后端无状态且现算失败的键：保留旧快照（服务仍存在才留，已删的不留）
        ps.services.forEach((v) => {
          const k = statusKey(project.id, v.spec.name)
          if (!(k in merged) && k in prev) merged[k] = prev[k]
        })
        return merged
      })
      onError(null)
    } catch (e) {
      onError(errorText(e))
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [project.id])

  useEffect(() => {
    if (opened) void load()
  }, [opened, load])

  // 只在展开时订阅，避免列表页常驻监听
  useEffect(() => {
    if (!opened) return
    return subscribe<ServiceStatus>('svc:state', (e) => {
      if (e.payload.project_id !== project.id) return
      setStatuses((prev) => ({ ...prev, [e.payload.key]: e.payload }))
    })
  }, [opened, project.id])

  const act = async (tag: string, fn: () => Promise<void>) => {
    setBusy(tag)
    try {
      await fn()
      onError(null)
    } catch (e) {
      onError(errorText(e))
    } finally {
      setBusy(null)
    }
  }

  const services = data?.services ?? []
  const running = services.filter((v) => {
    const st = statuses[statusKey(project.id, v.spec.name)]
    return st?.phase === 'Running' || st?.phase === 'Step'
  }).length

  const summary = !data
    ? '展开查看'
    : data.descriptor_error
      ? '描述文件有误'
      : services.length === 0
        ? '未配置'
        : `${services.length} 个服务${running > 0 ? ` · ${running} 个运行中` : ''}`

  return (
    <details
      className="mt-4 rounded-lg border border-border "
      onToggle={(e) => {
        if (e.currentTarget.open) setOpened(true)
      }}
    >
      <summary className="cursor-pointer px-3 py-2 text-sm font-semibold">
        服务
        {services.length > 0 && (
          <span
            className={`ml-2 inline-block h-2 w-2 rounded-full align-middle ${
              running > 0 ? DOT.Running : DOT.Stopped
            }`}
            aria-hidden
          />
        )}
        <span className="ml-1 font-normal text-muted">{summary}</span>
      </summary>

      <div className="border-t border-border p-3 ">
        {data?.descriptor_error && (
          <div className="mb-3 rounded border border-error/50 bg-error/10 px-2 py-1.5 text-xs text-error ">
            <div className="font-semibold">.project-ctrl.json 无法解析，服务不可用</div>
            <div className="mt-0.5 font-mono text-[11px] break-all">{data.descriptor_error}</div>
            <div className="mt-1 text-[11px] opacity-80">
              用编辑器修好该文件后点「刷新」；在修好之前，数据库里的旧配方不会顶替它。
            </div>
          </div>
        )}

        {services.map((v) => (
          <ServiceCard
            key={v.spec.name}
            project={project}
            view={v}
            status={statuses[statusKey(project.id, v.spec.name)] ?? v.status}
            busy={busy}
            open={active === v.spec.name}
            onToggle={() => setActive(active === v.spec.name ? null : v.spec.name)}
            onEdit={() => setEditing(v.spec.name)}
            onRefresh={load}
            onStatus={(st) =>
              setStatuses((prev) => ({ ...prev, [statusKey(project.id, v.spec.name)]: st }))
            }
            act={act}
          />
        ))}

        {data && !data.descriptor_error && services.length === 0 && (
          <DetectPanel
            project={project}
            detected={data.detected}
            onDone={() => void load()}
            onError={onError}
            busy={busy}
            act={act}
          />
        )}

        {data && services.length > 0 && (
          <div className="mt-3 flex flex-wrap items-center gap-2 text-xs">
            <button
              type="button"
              onClick={() => setCreating(true)}
              className="rounded border border-border px-2 py-1 "
            >
              添加服务…
            </button>
            <button
              type="button"
              onClick={() => void act('stop-all', async () => void (await api.serviceStopAll(project.id)))}
              disabled={busy !== null || running === 0}
              className="rounded border border-border px-2 py-1 disabled:opacity-40 "
            >
              全部停止
            </button>
            <button
              type="button"
              onClick={() => void act('refresh', load)}
              className="rounded border border-border px-2 py-1 "
            >
              刷新
            </button>
            <span className="ml-auto text-[11px] text-muted">
              来源：{data.descriptor_exists ? '.project-ctrl.json' : '本机数据库'}
            </span>
          </div>
        )}

        {(editing || creating) && (
          <EditForm
            initial={creating ? null : services.find((v) => v.spec.name === editing)?.spec ?? null}
            existingNames={services.map((v) => v.spec.name)}
            onCancel={() => {
              setEditing(null)
              setCreating(false)
            }}
            onSave={(spec) =>
              void act('save', async () => {
                await api.saveService(project.id, spec)
                setEditing(null)
                setCreating(false)
                await load()
              })
            }
          />
        )}
      </div>
    </details>
  )
}

function ServiceCard({
  project,
  view,
  status,
  busy,
  open,
  onToggle,
  onEdit,
  onRefresh,
  onStatus,
  act,
}: {
  project: ProjectInfo
  view: ServiceView
  status: ServiceStatus | null
  busy: string | null
  open: boolean
  onToggle: () => void
  onEdit: () => void
  onRefresh: () => Promise<void>
  onStatus: (st: ServiceStatus) => void
  act: (tag: string, fn: () => Promise<void>) => Promise<void>
}) {
  const spec = view.spec
  const name = spec.name
  const line = statusLine(status)
  const isRunning = status?.phase === 'Running'
  const isStep = status?.phase === 'Step'
  const [lines, setLines] = useState<string[]>([])
  const [problems, setProblems] = useState<string[] | null>(null)
  const [preflightErr, setPreflightErr] = useState<string | null>(null)
  const [containers, setContainers] = useState<ContainerView[] | null>(null)
  const [containersErr, setContainersErr] = useState<string | null>(null)
  const [logErr, setLogErr] = useState<string | null>(null)
  const logRef = useRef<HTMLPreElement | null>(null)
  // 日志快照的请求序号：展开 effect 与 docker effect 各拉一次 serviceLogs，
  // 后发先至的旧快照必须丢弃，否则它会覆盖已追加的增量行。
  const logReq = useRef(0)

  // 展开时才拉日志 + 预检，避免无谓 IO
  useEffect(() => {
    if (!open) return
    const req = ++logReq.current
    let alive = true
    setProblems(null)
    setPreflightErr(null)
    void (async () => {
      try {
        const l = await api.serviceLogs(project.id, name)
        if (alive && req === logReq.current) setLines(l)
      } catch (e) {
        if (alive && req === logReq.current) setLogErr(errorText(e))
      }
      try {
        const pf = await api.servicePreflight(project.id, name)
        if (alive) {
          setProblems(pf)
          setPreflightErr(null)
        }
      } catch (e) {
        // 预检失败必须可见：初值 null 本就禁用启动，文案说明是「查不出」而非「没问题」
        if (alive) setPreflightErr(errorText(e))
      }
    })()
    return subscribe<{ key: string; lines: string[] }>('svc:log', (e) => {
      if (e.payload.key !== statusKey(project.id, name)) return
      setLines((prev) => [...prev, ...e.payload.lines].slice(-2000))
    })
  }, [open, project.id, name])

  useEffect(() => {
    const el = logRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [lines])

  const loadContainers = async () => {
    try {
      setContainers(await api.dockerPs(project.id, name))
      setContainersErr(null)
    } catch (e) {
      // daemon 没起 / 命令失败必须可见：不能折成「没有运行中的容器」误导用户
      setContainers(null)
      setContainersErr(errorText(e))
    }
  }

  useEffect(() => {
    if (open && spec.docker) {
      setContainers(null)
      setContainersErr(null)
      void loadContainers()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, name])

  // compose 日志：展开且有 docker 时跟随，收起时停止（容器本身不受影响）。
  // 输出走与服务相同的 svc:log 通道。
  // 注意 deps 用 dockerKey（字符串）而不用 spec.docker 对象：父组件每次 load 都会
  // 重建 view/spec 对象，对象引用每次都变会导致 effect 反复重跑。
  // 排序保证：cleanup 等待 in-flight 的 dockerLogs 先落定（token 比对：只有最新
  // 一轮的启动才有效；旧轮的启动被顶掉后不再开流），再发 dockerLogsStop——
  //  stop 一定排在「本轮最后一次 start 落定」之后到达后端，
  // 不会再出现 stop 先到、start 后完成导致的 `logs -f` 常驻。
  const dockerKey = spec.docker
    ? `${spec.docker.files.join('+')}\n${spec.docker.project_name ?? ''}\n${spec.docker.services.join('+')}`
    : ''
  const dockerSeq = useRef(0)
  useEffect(() => {
    if (!open || !spec.docker) return
    const seq = ++dockerSeq.current
    let alive = true
    const req = ++logReq.current
    setLogErr(null)
    const started = (async () => {
      try {
        const l = await api.serviceLogs(project.id, name)
        if (alive && req === logReq.current && seq === dockerSeq.current) setLines(l)
        await api.dockerLogs(project.id, name)
      } catch (e) {
        // 跟随失败必须可见：静默吞掉会让面板卡在"暂无输出"，无从排查
        if (alive && seq === dockerSeq.current) setLogErr(errorText(e))
      }
    })()
    return () => {
      alive = false
      setLogErr(null)
      void started.then(() => {
        // 被更新的一轮顶替（StrictMode remount / dockerKey 变化）时跳过 stop：
        // 新轮的 dockerLogs 会在后端顶掉旧流，此时再 stop 会误杀刚开的新流
        // （面板变空白且无 follower——旧代码注释里记过的失败模式）。
        // 只有"没有后继者"（真正收起）时才停本轮的流。
        if (seq !== dockerSeq.current) return
        api.dockerLogsStop(project.id, name).catch((e) => {
          // 停止失败必须可见：流可能还在后端常驻（compose logs -f 孤儿）
          setLogErr((prev) => prev ?? `日志跟随停止失败：${errorText(e)}`)
        })
      })
    }
  }, [open, dockerKey, project.id, name])

  const stepLabel = (s: { name: string }) => s.name

  return (
    <div className="mb-2 rounded border border-border last:mb-0 ">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-2 py-1.5 text-left text-xs hover:bg-surface-raised "
      >
        <span className={`inline-block h-2 w-2 shrink-0 rounded-full ${line.dot}`} aria-hidden />
        <span className="font-semibold">{name}</span>
        <span className="text-muted">{line.label}</span>
        {line.detail && <span className="font-mono text-[11px] text-muted">{line.detail}</span>}
        {spec.docker && <span className="rounded border border-success/40 bg-success/10 px-1 text-[10px] text-fg">docker</span>}
        <span className="ml-auto font-mono text-[10px] text-muted">{view.layer === 'descriptor' ? '.json' : 'db'}</span>
      </button>

      {open && (
        <div className="border-t border-border p-2 ">
          {status?.note && (
            <div className="mb-1 text-[11px] text-warning ">{status.note}</div>
          )}
          {view.warnings.length > 0 && (
            <ul className="mb-1 list-inside list-disc text-[11px] text-warning ">
              {view.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}
          {(problems?.length ?? 0) > 0 && (
            <ul className="mb-1 list-inside list-disc text-[11px] text-error ">
              {(problems ?? []).map((p) => (
                <li key={p}>{p}</li>
              ))}
            </ul>
          )}
          {preflightErr && (
            <div className="mb-1 text-[11px] break-all text-error ">
              预检失败：{preflightErr}
            </div>
          )}

          <div className="flex flex-wrap gap-1.5 text-[11px]">
            <Button
              variant="primary"
              size="sm"
              onClick={() => void act(`start-${name}`, async () => {
                const st = await api.serviceStart(project.id, name)
                // docker-only 不进 supervisor：没有 svc:state 事件，必须把返回值写回，
                // 否则成功也显示「未运行」。本地进程的事件随后会覆盖同一条，无害。
                onStatus(st)
                if (spec.docker) await loadContainers()
              })}
              disabled={busy !== null || isRunning || isStep || problems === null || (problems?.length ?? 0) > 0 || !!preflightErr || (!spec.run && !spec.docker)}
              title={problems === null ? '预检中…' : (problems?.[0] ?? preflightErr ?? (!spec.run && !spec.docker ? '该服务没有 run 命令' : ''))}
            >
              {problems === null ? '预检中…' : '启动'}
            </Button>
            <Button size="sm"
              onClick={() => void act(`stop-${name}`, async () => {
                await api.serviceStop(project.id, name)
                // 停止语义（与后端约定）：serviceStop 对 docker-only 不碰容器，
                // 只停日志跟随/解绑——因此不写回 Stopped（容器还在 running，
                // 写 Stopped 是假信息）。
                // 本地进程的 Stopped 事件随后会覆盖同一条，无害。
              })}
              disabled={busy !== null || (!isRunning && !isStep)}
            >
              停止
            </Button>
            <Button size="sm"
              onClick={() =>
                void act(`restart-${name}`, async () => {
                  await api.serviceStop(project.id, name)
                  const st = await api.serviceStart(project.id, name)
                  onStatus(st)
                  if (spec.docker) await loadContainers()
                })
              }
              disabled={busy !== null || isStep || (!spec.run && !spec.docker)}
            >
              重启
            </Button>
            <Button size="sm"
              onClick={() => void act(`url-${name}`, async () => void (await api.openServiceUrl(project.id, name)))}
              disabled={status?.port == null}
            >
              打开
            </Button>
            {(spec.steps ?? []).map((s) => (
              <Button size="sm"
                key={s.name}
                onClick={() =>
                  void act(`step-${name}-${s.name}`, async () => void (await api.serviceStep(project.id, name, s.name)))
                }
                disabled={busy !== null || isRunning || isStep}
              >
                {stepLabel(s)}
              </Button>
            ))}
            {spec.docker && (
              <>
                <button
                  type="button"
                  onClick={() =>
                    void act(`up-${name}`, async () => {
                      await api.dockerUp(project.id, name)
                      await loadContainers()
                      // compose up 成功后同步一次状态快照：docker-only 无事件通道，
                      // 不写回则 badge/按钮停在旧态。读容器表推导 Running/Stopped。
                      if (!spec.run) onStatus(await dockerSnapshot(project.id, name, status))
                    })
                  }
                  disabled={busy !== null}
                  className="rounded bg-primary px-2 py-0.5 text-primary-fg disabled:opacity-40"
                >
                  compose up
                </button>
                <Button size="sm"
                  onClick={() =>
                    void act(`down-${name}`, async () => {
                      await api.dockerDown(project.id, name)
                      await loadContainers()
                      if (!spec.run) onStatus(await dockerSnapshot(project.id, name, status))
                    })
                  }
                  disabled={busy !== null}
                >
                  compose down
                </Button>
              </>
            )}
            <button
              type="button"
              onClick={onEdit}
              className="ml-auto rounded border border-border px-2 py-0.5 "
            >
              编辑
            </button>
            <button
              type="button"
              onClick={() => void act(`refresh-${name}`, onRefresh)}
              className="rounded border border-border px-2 py-0.5 "
            >
              刷新
            </button>
          </div>

          {spec.docker && (containers !== null || containersErr !== null) && (
            <div className="mt-2 overflow-hidden rounded border border-border ">
              <div className="flex items-center gap-2 border-b border-border bg-surface-raised px-2 py-1 text-[11px] font-semibold ">
                <span className="min-w-0 flex-1 truncate" title={spec.docker.files.join(' + ')}>
                  容器 · {spec.docker.files.join(' + ')}
                </span>
                {containersErr && (
                  <span className="shrink-0 font-normal text-error">读取失败</span>
                )}
              </div>
              {containersErr ? (
                <div className="flex flex-wrap items-center gap-2 p-2 text-[11px]">
                  <span className="min-w-0 flex-1 break-all text-error">容器状态读取失败：{containersErr}</span>
                  <button
                    type="button"
                    onClick={() => void loadContainers()}
                    className="shrink-0 rounded border border-border px-2 py-0.5 text-[11px] "
                  >
                    重试
                  </button>
                </div>
              ) : (containers?.length ?? 0) === 0 ? (
                <div className="p-2 text-[11px] text-muted">（没有运行中的容器）</div>
              ) : (
                <table className="w-full text-left text-[11px]">
                  <tbody>
                    {(containers ?? []).map((c) => (
                      <tr key={c.name} className="border-b border-border last:border-0 ">
                        <td className="px-2 py-1 font-mono">{c.service}</td>
                        <td className="px-2 py-1">{c.state}</td>
                        <td className="px-2 py-1 text-muted">{c.status}</td>
                        <td className="px-2 py-1 font-mono text-muted">{c.ports}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </div>
          )}

          <MeOverride project={project} name={name} view={view} onSaved={onRefresh} act={act} />

          <div className="mt-2 overflow-hidden rounded border border-border ">
            <div className="flex items-center gap-2 border-b border-border bg-surface-raised px-2 py-1 ">
              <span className="font-mono text-[11px] font-semibold">日志 · {lines.length} 行</span>
              <button
                type="button"
                onClick={() => setLines([])}
                className="ml-auto rounded border border-border px-2 py-0.5 text-[11px] "
              >
                清空
              </button>
            </div>
            {logErr && (
              <div className="border-b border-border bg-error/10 px-2 py-1 font-mono text-[11px] break-all text-error ">
                日志跟随失败：{logErr}
              </div>
            )}
            <pre
              ref={logRef}
              className="max-h-[40vh] select-text overflow-auto whitespace-pre-wrap p-2 font-mono text-[11px] leading-relaxed"
            >
              {lines.length === 0 && !logErr ? '（暂无输出）' : lines.join('\n')}
            </pre>
          </div>
        </div>
      )}
    </div>
  )
}

/** 本机覆盖：端口偏移与「退出后保留」。只写 `~/.project-ctrl/overrides.json`。 */
function MeOverride({
  project,
  name,
  view,
  onSaved,
  act,
}: {
  project: ProjectInfo
  name: string
  view: ServiceView
  onSaved: () => Promise<void>
  act: (tag: string, fn: () => Promise<void>) => Promise<void>
}) {
  const [offset, setOffset] = useState(String(view.port_offset || ''))
  const [keep, setKeep] = useState(view.keep_alive)
  // 脏字段不覆盖：svc:state 触发的 load 会重建 view，直接回写会把用户未保存的
  // 输入弹回（offset 正在打的字 / keep 刚勾的复选框）。
  const offsetFocused = useRef(false)
  const offsetDirty = (Number(offset || 0) || 0) !== view.port_offset
  const keepDirty = keep !== view.keep_alive

  useEffect(() => {
    if (!offsetFocused.current && !offsetDirty) {
      setOffset(view.port_offset ? String(view.port_offset) : '')
    }
    if (!keepDirty) {
      setKeep(view.keep_alive)
    }
  }, [view.port_offset, view.keep_alive, offsetDirty, keepDirty])

  const dirty = offsetDirty || keepDirty

  const save = () =>
    act(`me-${name}`, async () => {
      // 三态 patch：只发脏字段（缺席=不变），清零/取消发 null 删键——
      // 旧语义"缺字段=删"会误伤 restart/env 等本面板不显示的键。
      const next: ServiceOverride = {}
      if (offsetDirty) {
        const n = Number(offset || 0) || 0
        next.port_offset = n === 0 ? null : n
      }
      if (keep !== view.keep_alive) next.keep_alive = keep ? true : null
      await api.setServiceOverride(project.id, name, next)
      await onSaved()
    })

  return (
    <div className="mt-2 flex flex-wrap items-center gap-2 rounded border border-dashed border-border px-2 py-1.5 text-[11px] ">
      <span className="text-muted">本机</span>
      <label className="inline-flex items-center gap-1">
        端口偏移
        <input
          value={offset}
          onChange={(e) => setOffset(e.target.value.replace(/[^\d]/g, ''))}
          onFocus={() => {
            offsetFocused.current = true
          }}
          onBlur={() => {
            offsetFocused.current = false
          }}
          placeholder="0"
          aria-label="端口偏移"
          className="w-14 rounded border border-border px-1 py-0.5 font-mono "
        />
      </label>
      <label className="inline-flex items-center gap-1">
        <input type="checkbox" checked={keep} onChange={(e) => setKeep(e.target.checked)} />
        退出后保留进程
      </label>
      <Button size="sm"
        onClick={save}
        disabled={!dirty}
      >
        保存
      </Button>
    </div>
  )
}

/** 未配置时：探测结果 + 三条路径（保存到本机 / 固化为声明 / 手工填写）。 */
function DetectPanel({
  project,
  detected,
  onDone,
  onError,
  busy,
  act,
}: {
  project: ProjectInfo
  detected: DetectResult
  onDone: () => void
  onError: (m: string | null) => void
  busy: string | null
  act: (tag: string, fn: () => Promise<void>) => Promise<void>
}) {
  const [input, setInput] = useState('')
  const [envConfirm, setEnvConfirm] = useState<string | null>(null)
  const compose = detected.compose_stacks

  /** 探测结果 → 一条可保存的声明（不写仓库文件）。 */
  const fromCandidate = (argv: string[], listen: number | null, requires: number[]): ServiceSpec => ({
    name: guessName(argv),
    run: argv,
    steps: detected.build ? [{ name: 'build', argv: detected.build, timeout_s: 1800 }] : [],
    ports: { listen, requires },
    env: {},
  })

  const saveCandidate = (argv: string[]) =>
    act('adopt', async () => {
      await api.saveService(project.id, fromCandidate(argv, detected.dev_port, detected.requires_ports))
      onDone()
    })

  const saveCompose = (stack: ComposeCandidate) =>
    act('adopt-compose', async () => {
      const files = [stack.file, ...stack.overrides]
      await api.saveService(project.id, {
        name: stack.file.includes('/') ? stack.file.split('/')[0] : 'docker',
        docker: { files, services: stack.services, project_name: null },
        ports: { requires: stack.ports },
      })
      onDone()
    })

  /**
   * 固化为仓库声明。env 非空时后端先拒绝（Err 含 ENV_CONFIRM_REQUIRED，只列 key 名）：
   * 此时弹确认框展示后端原文，用户确认后再用 confirmEnv=true 重调。
   * 其余错误直接走 onError，不弹框。
   */
  const solidify = (confirmEnv: boolean) =>
    act('solidify', async () => {
      try {
        const text = await api.solidifyDescriptor(project.id, [], false, confirmEnv)
        onError(null)
        void text
        onDone()
      } catch (e) {
        const msg = errorText(e)
        if (!confirmEnv && msg.includes('ENV_CONFIRM_REQUIRED')) {
          setEnvConfirm(msg)
          return
        }
        onError(msg)
      }
    })

  return (
    <div>
      <div className="text-sm text-muted">
        该项目还没有服务声明。下面是从文件里探测到的候选——点「保存」只写本机数据库，不会动你的仓库。
      </div>

      {detected.problems.length > 0 && (
        <ul className="mt-2 list-inside list-disc text-xs text-muted">
          {detected.problems.map((p) => (
            <li key={p}>{p}</li>
          ))}
        </ul>
      )}

      {detected.start_candidates.length > 0 && (
        <div className="mt-2 space-y-1.5">
          <div className="text-[11px] font-semibold text-muted">package.json 脚本</div>
          {detected.start_candidates.map((c) => (
            <div key={c.join(' ')} className="flex items-center gap-2">
              <code className="rounded bg-surface-raised px-1.5 py-0.5 font-mono text-[11px] ">
                {c.join(' ')}
              </code>
              <button
                type="button"
                onClick={() => void saveCandidate(c)}
                disabled={busy !== null}
                className="rounded border border-border px-2 py-0.5 text-xs disabled:opacity-40 "
              >
                保存
              </button>
            </div>
          ))}
        </div>
      )}

      {compose.length > 0 && (
        <div className="mt-2 space-y-1.5">
          <div className="text-[11px] font-semibold text-muted">docker compose</div>
          {compose.map((s) => (
            <div key={s.file} className="flex flex-wrap items-center gap-2">
              <code className="rounded bg-surface-raised px-1.5 py-0.5 font-mono text-[11px] ">
                {s.file}
              </code>
              <span className="text-[11px] text-muted">{s.services.join(', ')}</span>
              <button
                type="button"
                onClick={() => void saveCompose(s)}
                disabled={busy !== null}
                className="rounded border border-border px-2 py-0.5 text-xs disabled:opacity-40 "
              >
                保存
              </button>
            </div>
          ))}
        </div>
      )}

      {detected.make_targets.length > 0 && (
        <div className="mt-2 text-[11px] text-muted">
          Makefile 目标：<span className="font-mono">{detected.make_targets.join(' ')}</span>
        </div>
      )}
      {detected.script_candidates.length > 0 && (
        <div className="mt-1 text-[11px] text-muted">
          脚本：<span className="font-mono">{detected.script_candidates.join(' ')}</span>
        </div>
      )}

      <div className="mt-3 flex flex-wrap items-center gap-2">
        <input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="手工输入启动命令，例如 pnpm dev"
          className="w-64 rounded border border-border px-2 py-1 font-mono text-xs "
        />
        <button
          type="button"
          onClick={() =>
            void act('adopt-manual', async () => {
              await api.saveService(project.id, fromCandidate(splitArgv(input), detected.dev_port, detected.requires_ports))
              onDone()
            })
          }
          disabled={busy !== null || splitArgv(input).length === 0}
          className="rounded bg-primary px-2 py-1 text-xs text-primary-fg disabled:opacity-40"
        >
          保存
        </button>
        <button
          type="button"
          onClick={() => void solidify(false)}
          disabled={busy !== null}
          title="把已有服务写进仓库内的 .project-ctrl.json"
          className="ml-auto rounded border border-border px-2 py-1 text-xs disabled:opacity-40 "
        >
          固化为仓库声明…
        </button>
      </div>
      {envConfirm !== null && (
        <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6">
          <div className="w-full max-w-md rounded-xl bg-surface p-5 ">
            <h3 className="font-semibold">确认写入环境变量？</h3>
            <p className="mt-2 text-sm">
              以下服务的 env 将写入可提交的 <span className="font-mono">.project-ctrl.json</span>。
              为防 secret 泄露，这里只列变量名，请确认无密钥后再继续。
            </p>
            <p className="mt-2 break-all text-xs text-warning">{envConfirm}</p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setEnvConfirm(null)}
                className="rounded-lg border border-border px-3 py-1.5 text-sm "
              >
                取消
              </button>
              <button
                type="button"
                onClick={() => {
                  setEnvConfirm(null)
                  void solidify(true)
                }}
                disabled={busy !== null}
                className="rounded-lg bg-primary px-3 py-1.5 text-sm text-primary-fg disabled:opacity-40"
              >
                确认写入
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}

/** 从 argv 里猜一个服务名（最多 24 字符，去重由后端约束）。 */
function guessName(argv: string[]): string {
  const cmd = argv.filter((a) => !a.startsWith('-'))[1] ?? argv[0] ?? 'service'
  const cleaned = cmd.replace(/[^a-zA-Z0-9:_-]/g, '').slice(0, 24)
  return cleaned || 'service'
}

function EditForm({
  initial,
  existingNames,
  onSave,
  onCancel,
}: {
  initial: ServiceSpec | null
  existingNames: string[]
  onSave: (c: ServiceSpec) => void
  onCancel: () => void
}) {
  const isEdit = initial !== null
  const [name, setName] = useState(initial?.name ?? '')
  const [cwd, setCwd] = useState(initial?.cwd ?? '')
  const [run, setRun] = useState(joinArgv(initial?.run ?? null))
  const [steps, setSteps] = useState(
    (initial?.steps ?? []).map((s) => `${s.name}=${joinArgv(s.argv)}`).join('\n'),
  )
  const [envFiles, setEnvFiles] = useState((initial?.env_files ?? []).join(' '))
  const [envLines, setEnvLines] = useState(
    Object.entries(initial?.env ?? {}).map(([k, v]) => `${k}=${v}`).join('\n'),
  )
  const [listen, setListen] = useState(initial?.ports?.listen?.toString() ?? '')
  const [requires, setRequires] = useState((initial?.ports?.requires ?? []).join(' '))
  const [depends, setDepends] = useState((initial?.depends_on ?? []).join(' '))
  const [restart, setRestart] = useState<'never' | 'on-failure' | 'always'>(
    initial?.restart ?? 'on-failure',
  )
  const [dockerFiles, setDockerFiles] = useState((initial?.docker?.files ?? []).join(' '))
  const [dockerServices, setDockerServices] = useState((initial?.docker?.services ?? []).join(' '))
  const [timeout, setTimeoutSec] = useState(
    initial?.health?.timeout_ms ? String(initial.health.timeout_ms / 1000) : '',
  )

  const dup = !isEdit && existingNames.includes(name.trim())
  const canSave = name.trim() !== '' && !dup && (splitArgv(run).length > 0 || splitArgv(steps).length > 0 || dockerFiles.trim() !== '')

  const input =
    'w-full rounded border border-border bg-surface px-2 py-1 font-mono text-xs  '

  const save = () => {
    const parsedSteps = steps
      .split('\n')
      .map((l) => l.trim())
      .filter(Boolean)
      .map((l) => {
        const [n, ...rest] = l.split('=')
        return { name: n.trim(), argv: splitArgv(rest.join('=')), timeout_s: 1800 }
      })
    const env: Record<string, string> = {}
    envLines
      .split('\n')
      .map((l) => l.trim())
      .filter(Boolean)
      .forEach((l) => {
        const i = l.indexOf('=')
        if (i > 0) env[l.slice(0, i).trim()] = l.slice(i + 1)
      })
    const spec: ServiceSpec = {
      name: name.trim(),
      cwd: cwd.trim() || null,
      run: splitArgv(run).length > 0 ? splitArgv(run) : null,
      steps: parsedSteps,
      env_files: splitArgv(envFiles),
      env,
      ports: {
        listen: listen.trim() ? Number(listen) : null,
        requires: splitArgv(requires).map(Number).filter((n) => Number.isFinite(n)),
      },
      health: splitArgv(run).length > 0
        ? { type: 'port', timeout_ms: timeout.trim() ? Number(timeout) * 1000 : null }
        : null,
      depends_on: splitArgv(depends),
      restart,
      docker: dockerFiles.trim()
        ? {
            files: splitArgv(dockerFiles),
            project_name: null,
            services: splitArgv(dockerServices),
          }
        : null,
    }
    onSave(spec)
  }

  const label = 'block text-[11px] text-muted'

  return (
    <div className="mt-3 rounded border border-border p-3 ">
      <div className="text-xs font-semibold">{isEdit ? `编辑 ${initial?.name}` : '添加服务'}</div>
      <div className="mt-2 space-y-2">
        <div className="flex gap-2">
          <label className="block w-40">
            <span className={label}>名称（唯一）</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={isEdit}
              className={input}
            />
          </label>
          <label className="block flex-1">
            <span className={label}>工作目录（相对项目根，可选）</span>
            <input value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="frontend" className={input} />
          </label>
        </div>
        {dup && <div className="text-[11px] text-error">已存在同名服务：{name}</div>}

        <label className="block">
          <span className={label}>启动命令（空格分隔参数）</span>
          <input value={run} onChange={(e) => setRun(e.target.value)} placeholder="pnpm dev" className={input} />
        </label>

        <label className="block">
          <span className={label}>一次性步骤（每行 `名称=命令`，默认不随启动执行）</span>
          <textarea
            value={steps}
            onChange={(e) => setSteps(e.target.value)}
            rows={2}
            placeholder={'install=pnpm install\nbuild=pnpm run build'}
            className={input}
          />
        </label>

        <div className="flex gap-2">
          <label className="block flex-1">
            <span className={label}>env 文件（相对项目根）</span>
            <input value={envFiles} onChange={(e) => setEnvFiles(e.target.value)} placeholder=".env.local" className={input} />
          </label>
          <label className="block flex-1">
            <span className={label}>依赖服务（depends_on）</span>
            <input value={depends} onChange={(e) => setDepends(e.target.value)} placeholder="db" className={input} />
          </label>
        </div>

        <label className="block">
          <span className={label}>环境变量（每行 `KEY=VALUE`）</span>
          <textarea value={envLines} onChange={(e) => setEnvLines(e.target.value)} rows={2} className={input} />
        </label>

        <div className="flex gap-2">
          <label className="block flex-1">
            <span className={label}>监听端口</span>
            <input value={listen} onChange={(e) => setListen(e.target.value.replace(/[^\d]/g, ''))} className={input} />
          </label>
          <label className="block flex-1">
            <span className={label}>依赖端口</span>
            <input value={requires} onChange={(e) => setRequires(e.target.value)} placeholder="5432" className={input} />
          </label>
          <label className="block flex-1">
            <span className={label}>就绪超时（秒）</span>
            <input value={timeout} onChange={(e) => setTimeoutSec(e.target.value.replace(/[^\d]/g, ''))} placeholder="30" className={input} />
          </label>
          <label className="block w-32">
            <span className={label}>崩溃重启</span>
            <select
              value={restart}
              onChange={(e) => setRestart(e.target.value as 'never' | 'on-failure' | 'always')}
              className={input}
            >
              <option value="never">不重启</option>
              <option value="on-failure">失败时</option>
              <option value="always">总是</option>
            </select>
          </label>
        </div>

        <div className="flex gap-2">
          <label className="block flex-1">
            <span className={label}>compose 文件（空格分隔，多个按序叠加）</span>
            <input value={dockerFiles} onChange={(e) => setDockerFiles(e.target.value)} placeholder="infra/docker-compose.yml" className={input} />
          </label>
          <label className="block flex-1">
            <span className={label}>compose 子服务</span>
            <input value={dockerServices} onChange={(e) => setDockerServices(e.target.value)} placeholder="postgres minio" className={input} />
          </label>
        </div>
      </div>

      <div className="mt-3 flex justify-end gap-2 text-xs">
        <button type="button" onClick={onCancel} className="rounded border border-border px-2 py-1 ">
          取消
        </button>
        <button
          type="button"
          onClick={save}
          disabled={!canSave}
          className="rounded bg-primary px-2 py-1 text-primary-fg disabled:opacity-40"
        >
          保存
        </button>
      </div>
    </div>
  )
}
