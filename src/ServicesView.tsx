import type { ServiceStatus } from './api'
import { Button, StatusDot } from './components'
import { opsBadCount } from './ops'
import { IconStethoscope } from './icons'

const PHASE_TEXT: Record<ServiceStatus['phase'], string> = {
  Idle: '未运行',
  Step: '执行中',
  Running: '运行中',
  Stopped: '已停止',
  Crashed: '已崩溃',
  Failed: '失败',
}

const toneOf = (phase: ServiceStatus['phase']): 'idle' | 'busy' | 'ok' | 'bad' =>
  phase === 'Running' ? 'ok' : phase === 'Step' ? 'busy' : phase === 'Crashed' || phase === 'Failed' ? 'bad' : 'idle'

/**
 * 服务页：所有活跃/异常服务的运行态视图。
 *
 * 这不是「另一个项目列表」——只有真正在跑或刚崩掉的服务才在这里出现，
 * 空态要说清楚「没有服务在跑」以及去哪儿启动。
 */
export default function ServicesView({
  rows,
  projectName,
  onOpen,
  onDiagnose,
}: {
  rows: ServiceStatus[]
  projectName: (id: number) => string
  onOpen: (id: number, tab?: 'overview' | 'services' | 'source' | 'readme') => void
  onDiagnose: (projectId: number, serviceName: string) => void
}) {
  const bad = opsBadCount(rows)

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex h-[52px] shrink-0 items-center gap-3 border-b border-border px-4">
        <h1 data-tauri-drag-region className="text-[17px] font-semibold">
          服务
        </h1>
        <span data-tauri-drag-region className="tabular text-[12px] text-muted">
          {rows.length} 个活跃 · {bad} 个异常
        </span>
        <div data-tauri-drag-region className="h-full flex-1" />
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        {rows.length === 0 ? (
          <div className="rounded-card border border-dashed border-border p-8 text-center text-sm text-muted">
            没有正在运行或异常的服务。到项目详情页的「服务」面板里启动，或用工具栏的「批量探测…」。
          </div>
        ) : (
          <>
            <OpsBar rows={rows} projectName={projectName} onOpen={(id) => onOpen(id, 'services')} />
            <div className="mt-3 overflow-hidden rounded-card border border-border">
              <table className="w-full text-left text-xs">
                <thead className="bg-surface-raised text-muted">
                  <tr>
                    <th className="px-3 py-2 font-medium">项目</th>
                    <th className="px-3 py-2 font-medium">服务</th>
                    <th className="px-3 py-2 font-medium">状态</th>
                    <th className="px-3 py-2 font-medium">端口</th>
                    <th className="px-3 py-2 font-medium">PID</th>
                    <th className="px-3 py-2 font-medium">重启</th>
                    <th className="px-3 py-2 font-medium">提示</th>
                    <th className="px-3 py-2" />
                  </tr>
                </thead>
                <tbody>
                  {rows.map((st) => {
                    const broken = st.phase === 'Crashed' || st.phase === 'Failed'
                    return (
                      <tr
                        key={st.key}
                        className="cursor-pointer border-t border-border hover:bg-surface-raised"
                        onClick={() => onOpen(st.project_id, 'services')}
                      >
                        <td className="px-3 py-1.5">{projectName(st.project_id)}</td>
                        <td className="px-3 py-1.5 font-medium">{st.name}</td>
                        <td className="px-3 py-1.5">
                          <StatusDot
                            tone={toneOf(st.phase)}
                            label={st.step ? `${PHASE_TEXT[st.phase]} · ${st.step}` : PHASE_TEXT[st.phase]}
                          />
                        </td>
                        <td className="tabular px-3 py-1.5 font-mono text-muted">{st.port ?? '—'}</td>
                        <td className="tabular px-3 py-1.5 font-mono text-muted">{st.pid ?? '—'}</td>
                        <td className="tabular px-3 py-1.5 text-muted">{st.restarts > 0 ? st.restarts : '—'}</td>
                        <td className="max-w-[280px] truncate px-3 py-1.5 text-warning" title={st.note ?? ''}>
                          {st.note ?? ''}
                        </td>
                        <td className="px-3 py-1.5 text-right">
                          {broken && (
                            <Button
                              size="sm"
                              onClick={(e) => {
                                e.stopPropagation()
                                onDiagnose(st.project_id, st.name)
                              }}
                              title={`让 AI 诊断 ${st.name} 的失败原因`}
                            >
                              <IconStethoscope size={12} />
                              AI 诊断
                            </Button>
                          )}
                        </td>
                      </tr>
                    )
                  })}
                </tbody>
              </table>
            </div>
          </>
        )}
      </div>
    </div>
  )
}

/** 运维条：一条紧凑的实时状态带，异常优先。 */
function OpsBar({
  rows,
  projectName,
  onOpen,
}: {
  rows: ServiceStatus[]
  projectName: (id: number) => string
  onOpen: (id: number) => void
}) {
  const bad = opsBadCount(rows)
  return (
    <div className="flex flex-wrap items-center gap-2 rounded-lg border border-border bg-surface-raised px-3 py-2 text-xs">
      <span className="font-medium text-muted">运行中</span>
      {rows.slice(0, 6).map((st) => (
        <button
          type="button"
          key={st.key}
          onClick={() => onOpen(st.project_id)}
          className="inline-flex items-center gap-1.5 rounded-full border border-border bg-surface px-2 py-0.5 hover:border-success"
          title={`${projectName(st.project_id)} / ${st.name}（${PHASE_TEXT[st.phase]}${st.port != null ? ` · 端口 ${st.port}` : ''}）`}
          aria-label={`${projectName(st.project_id)} / ${st.name}：${PHASE_TEXT[st.phase]}${st.port != null ? `，端口 ${st.port}` : ''}`}
        >
          <StatusDot tone={st.phase === 'Running' ? 'ok' : st.phase === 'Step' ? 'busy' : 'bad'} title={PHASE_TEXT[st.phase]} />
          <span>{st.name}</span>
          {st.port != null && <span className="tabular font-mono text-muted">:{st.port}</span>}
        </button>
      ))}
      {rows.length > 6 && <span className="text-muted">+{rows.length - 6}</span>}
      {bad > 0 && <span className="ml-auto text-error">{bad} 个异常</span>}
    </div>
  )
}
