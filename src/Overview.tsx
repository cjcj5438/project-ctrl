import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { api, errorText, type Phase, type ProjectInfo, type ServiceView, type SourceInfo } from './api'
import { Badge, Button, Card, Input } from './components'
import { humanSize } from './format'
import {
  IconActivity,
  IconCheck,
  IconIdCard,
  IconPencil,
  IconPlus,
  IconSparkles,
  IconStethoscope,
  IconWrench,
  IconX,
} from './icons'
import type { AiTask } from './route'

/** 画像约束：超限只提示、不提交（不做静默截断）。 */
const SUMMARY_MAX = 120
const STACK_MAX = 12
const STACK_ITEM_MAX = 32

const PHASE_LABEL: Record<Phase, string> = {
  Idle: '未运行',
  Step: '执行中',
  Running: '运行中',
  Stopped: '已停止',
  Crashed: '已崩溃',
  Failed: '失败',
}

/** 概览页：画像（摘要 + 技术栈，可编辑）+ 统计 + AI 入口。 */
export default function Overview({
  project,
  size,
  onChanged,
  onAiTask,
}: {
  project: ProjectInfo
  size?: number | null
  onChanged: () => Promise<void> | void
  onAiTask: (task: AiTask) => void
}) {
  const [services, setServices] = useState<ServiceView[] | null>(null)
  const [source, setSource] = useState<SourceInfo | null>(null)
  const [error, setError] = useState<string | null>(null)

  const [editing, setEditing] = useState(false)
  const [summary, setSummary] = useState('')
  const [stack, setStack] = useState<string[]>([])
  const [chipInput, setChipInput] = useState('')
  const [saving, setSaving] = useState(false)
  const aliveRef = useRef(true)

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  const load = useCallback(async () => {
    try {
      const ps = await api.projectServices(project.id)
      if (aliveRef.current) setServices(ps.services)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    }
    if (!project.has_git) return
    try {
      const si = await api.sourceInfo(project.id)
      if (aliveRef.current) setSource(si)
    } catch {
      // 源码统计失败不该挡住概览；Source 页签会给出具体原因
    }
  }, [project.id, project.has_git])

  useEffect(() => {
    void load()
  }, [load])

  const startEdit = () => {
    setSummary(project.summary ?? '')
    setStack(project.stack)
    setChipInput('')
    setEditing(true)
    setError(null)
  }

  const cancelEdit = () => {
    setEditing(false)
    setChipInput('')
  }

  const addChip = () => {
    const t = chipInput.trim()
    if (!t) return
    if (stack.includes(t)) {
      setChipInput('')
      return
    }
    if (t.length > STACK_ITEM_MAX) {
      setError(`单个技术栈最多 ${STACK_ITEM_MAX} 个字符`)
      return
    }
    if (stack.length >= STACK_MAX) {
      setError(`技术栈最多 ${STACK_MAX} 项`)
      return
    }
    setStack([...stack, t])
    setChipInput('')
    setError(null)
  }

  const overLimit = summary.length > SUMMARY_MAX
  const canSave = !overLimit && stack.length <= STACK_MAX && stack.every((s) => s.length <= STACK_ITEM_MAX)

  const save = async () => {
    setSaving(true)
    try {
      await api.setProjectProfile(
        project.id,
        summary.trim() === '' ? null : summary.trim(),
        stack,
      )
      if (!aliveRef.current) return
      setEditing(false)
      setError(null)
      await onChanged()
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    } finally {
      if (aliveRef.current) setSaving(false)
    }
  }

  const runningCount = (services ?? []).filter(
    (v) => v.status?.phase === 'Running' || v.status?.phase === 'Step',
  ).length
  const broken = (services ?? []).filter(
    (v) => v.status?.phase === 'Crashed' || v.status?.phase === 'Failed',
  )

  const sizeText = size === undefined ? '计算中…' : size === null ? '—' : humanSize(size)
  const branchText = useMemo(() => {
    if (!project.has_git) return '不是 git 仓库'
    if (!source) return '读取中…'
    if (!source.is_repo) return '不是 git 仓库'
    const bits = [source.detached ? 'detached HEAD' : (source.branch ?? '未知')]
    if (source.dirty > 0) bits.push(`${source.dirty} 处改动`)
    if (source.ahead > 0) bits.push(`领先 ${source.ahead}`)
    if (source.behind > 0) bits.push(`落后 ${source.behind}`)
    return bits.join(' · ')
  }, [project.has_git, source])

  return (
    <div className="flex flex-col gap-4">
      {error && (
        <div className="rounded-[var(--radius-card)] border border-error/40 bg-error/10 px-3 py-2 text-[12px] text-error">
          {error}
        </div>
      )}

      <Card className="bg-surface-raised p-4">
        <div className="flex items-center gap-2">
          <h2 className="text-[13px] font-semibold">画像</h2>
          {!editing && (
            <Button size="sm" variant="ghost" onClick={startEdit} className="ml-auto">
              <IconPencil size={12} />
              编辑
            </Button>
          )}
        </div>

        {editing ? (
          <div className="mt-2 flex flex-col gap-3">
            <div>
              <span className="mb-1 block text-[11px] text-muted">项目摘要</span>
              <textarea
                value={summary}
                onChange={(e) => setSummary(e.target.value)}
                rows={3}
                aria-label="项目摘要"
                placeholder="一句话说明这个项目是做什么的"
                className="w-full rounded-[var(--radius-control)] border border-border bg-surface px-2 py-1.5 text-[13px] text-fg placeholder:text-subtle focus:border-primary focus-visible:outline-none"
              />
              <div className="mt-1 flex items-center gap-2 text-[11px]">
                <span className={overLimit ? 'text-error' : 'text-subtle'}>
                  {summary.length} / {SUMMARY_MAX}
                </span>
                {overLimit && <span className="text-error">超出上限，无法保存</span>}
              </div>
            </div>

            <div>
              <span className="mb-1 block text-[11px] text-muted">技术栈</span>
              <div className="flex flex-wrap items-center gap-1">
                {stack.map((s) => (
                  <span
                    key={s}
                    className="inline-flex items-center gap-1 rounded border border-border bg-surface px-1.5 py-0.5 font-mono text-[11px]"
                  >
                    {s}
                    <button
                      type="button"
                      onClick={() => setStack(stack.filter((x) => x !== s))}
                      aria-label={`移除技术栈 ${s}`}
                      className="text-subtle hover:text-error"
                    >
                      <IconX size={10} />
                    </button>
                  </span>
                ))}
                <Input
                  value={chipInput}
                  onChange={(e) => setChipInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault()
                      addChip()
                    }
                  }}
                  placeholder="添加技术栈…"
                  aria-label="添加技术栈"
                  className="h-6 w-32 text-[12px]"
                />
                <Button size="sm" onClick={addChip} disabled={!chipInput.trim()}>
                  <IconPlus size={11} />
                  添加
                </Button>
              </div>
              <div className="mt-1 text-[11px] text-subtle">
                {stack.length} / {STACK_MAX} 项，每项 ≤ {STACK_ITEM_MAX} 字符（回车添加）
              </div>
            </div>

            <div className="flex justify-end gap-2">
              <Button size="sm" onClick={cancelEdit} disabled={saving}>
                取消
              </Button>
              <Button size="sm" variant="primary" onClick={() => void save()} disabled={saving || !canSave}>
                <IconCheck size={12} />
                {saving ? '保存中…' : '保存'}
              </Button>
            </div>
          </div>
        ) : (
          <div className="mt-2 flex flex-col gap-2">
            <p className={`text-[13px] leading-relaxed ${project.summary ? 'text-fg' : 'text-subtle'}`}>
              {project.summary ?? '暂无描述。点「编辑」手写，或让 AI 生成画像。'}
            </p>
            <div className="flex flex-wrap gap-1">
              {project.stack.length === 0 ? (
                <span className="text-[11px] text-subtle">暂无技术栈</span>
              ) : (
                project.stack.map((s) => (
                  <span
                    key={s}
                    className="rounded border border-border bg-surface px-1.5 py-0.5 font-mono text-[11px] text-muted"
                  >
                    {s}
                  </span>
                ))
              )}
            </div>
          </div>
        )}
      </Card>

      <Card className="bg-surface-raised p-4">
        <h2 className="text-[13px] font-semibold">统计</h2>
        <dl className="mt-2 grid gap-x-6 gap-y-1.5 text-[12px] sm:grid-cols-2">
          <Stat label="服务">
            {services === null
              ? '读取中…'
              : services.length === 0
                ? '未配置'
                : `${runningCount} / ${services.length} 运行中`}
          </Stat>
          <Stat label="磁盘占用">
            <span className="tabular">{project.missing ? '路径丢失' : sizeText}</span>
          </Stat>
          <Stat label="分支">
            <span className="tabular">{branchText}</span>
          </Stat>
          <Stat label="创建日期">
            <span className="tabular">{project.created_date ?? '未知'}</span>
          </Stat>
        </dl>
        {broken.length > 0 && (
          <div className="mt-3 flex flex-wrap items-center gap-1.5">
            <span className="text-[11px] text-muted">异常服务：</span>
            {broken.map((v) => (
              <Badge key={v.spec.name} tone="error">
                {v.spec.name} · {v.status ? PHASE_LABEL[v.status.phase] : '异常'}
              </Badge>
            ))}
          </div>
        )}
      </Card>

      {/* AI 入口：本步骤只负责呈现并把点击交给 onAiTask，行为由后续步骤接线 */}
      <Card className="p-4">
        <div className="flex items-center gap-2">
          <IconSparkles size={14} />
          <h2 className="text-[13px] font-semibold">AI</h2>
        </div>
        <div className="mt-2 grid gap-2 [grid-template-columns:repeat(auto-fill,minmax(200px,1fr))]">
          {services !== null && services.length === 0 && (
            <AiCard
              icon={<IconWrench size={14} />}
              title="AI 配置服务"
              hint="从仓库文件推断启动方式并生成服务声明"
              onClick={() => onAiTask({ kind: 'configure' })}
            />
          )}
          {broken.map((v) => (
            <AiCard
              key={v.spec.name}
              icon={<IconStethoscope size={14} />}
              title={`AI 诊断 ${v.spec.name}`}
              hint="读取日志与预检结果，给出失败原因"
              onClick={() => onAiTask({ kind: 'diagnose', service: v.spec.name })}
            />
          ))}
          <AiCard
            icon={<IconActivity size={14} />}
            title="AI 体检"
            hint="整体检查仓库结构、依赖与运行配置"
            onClick={() => onAiTask({ kind: 'health' })}
          />
          {!project.summary && (
            <AiCard
              icon={<IconIdCard size={14} />}
              title="AI 画像"
              hint="生成项目摘要与技术栈"
              onClick={() => onAiTask({ kind: 'profile' })}
            />
          )}
        </div>
      </Card>
    </div>
  )
}

function Stat({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-2">
      <dt className="w-16 shrink-0 text-muted">{label}</dt>
      <dd className="min-w-0 flex-1 break-words text-fg">{children}</dd>
    </div>
  )
}

function AiCard({
  icon,
  title,
  hint,
  onClick,
}: {
  icon: React.ReactNode
  title: string
  hint: string
  onClick: () => void
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex flex-col items-start gap-1 rounded-[var(--radius-card)] border border-border bg-surface p-2.5 text-left transition-colors hover:border-ai/60"
    >
      <span className="flex items-center gap-1.5 text-[12px] font-medium text-fg">
        <span className="text-ai">{icon}</span>
        {title}
      </span>
      <span className="text-[11px] leading-snug text-muted">{hint}</span>
    </button>
  )
}
