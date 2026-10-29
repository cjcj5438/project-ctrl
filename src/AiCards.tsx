import { useEffect, useMemo, useState } from 'react'
import { api, type AiCard as AiCardData, type AiCardAction, type AiHealthItem } from './api'
import { Row, Shell, STATE_TEXT } from './AiCardShell'
import { Badge, Button, Switch, TextArea } from './components'
import { DiffView } from './DiffView'
import {
  IconAlertTriangle,
  IconCheck,
  IconCopy,
  IconFileText,
  IconIdCard,
  IconSettings,
  IconSparkles,
  IconSquare,
  IconStethoscope,
  IconWrench,
} from './icons'
import type { AiTask } from './route'

/** 卡片能发出的动作；`onAction` 由抽屉接到 `ai_card_action`。 */
export type AiCardActionHandler = (cardId: string, action: AiCardAction) => void

const ACTION_TEXT: Record<string, string> = {
  start: '启动',
  stop: '停止',
  restart: '重启',
}

const CATEGORY_TEXT: Record<string, string> = {
  config: '配置问题',
  code: '代码问题',
  env: '环境问题',
  external: '外部依赖',
  unknown: '原因不明',
}

const SEVERITY_TONE: Record<string, 'neutral' | 'success' | 'warning' | 'error'> = {
  error: 'error',
  warn: 'warning',
  info: 'neutral',
}

const SEVERITY_TEXT: Record<string, string> = { error: '错误', warn: '警告', info: '提示' }
const SEVERITY_ORDER: Record<string, number> = { error: 0, warn: 1, info: 2 }

/* ------------------------------------------------------------------------- *
 * proposal：配方提案（JSON 前后对比 + 环境变量名单）
 * ------------------------------------------------------------------------- */

function ProposalCard({
  card,
  onAction,
}: {
  card: Extract<AiCardData, { kind: 'proposal' }>
  onAction: AiCardActionHandler
}) {
  const [runSteps, setRunSteps] = useState(card.run_steps_default)
  const [rejecting, setRejecting] = useState(false)
  const [feedback, setFeedback] = useState('')
  // 服务端补丁会把 state 改成 approved/rejected/…，此时按钮只读
  const pending = card.state === 'pending'
  const auto = card.state === 'auto'

  return (
    <Shell icon={<IconWrench size={13} />} title={`服务配方（第 ${card.attempt} 轮）`} state={card.state}>
      {auto && (
        <div className="text-[12px] text-warning">自动修正 · 第 {card.attempt} 轮</div>
      )}
      {card.notes && <Row label="说明">{card.notes}</Row>}
      <Row label="写入层">{card.layer === 'descriptor' ? '仓库描述文件' : '本机数据库'}</Row>
      <DiffView before={card.before} after={card.after} />
      {card.env_keys.length > 0 && (
        <div className="rounded-[var(--radius-control)] border border-warning/40 bg-warning/10 px-2 py-1.5 text-[11px] text-warning">
          <div className="flex items-center gap-1">
            <IconAlertTriangle size={11} />
            这些变量会写进配方（只列名字）
          </div>
          <div className="mt-1 font-mono break-all">{card.env_keys.join('、')}</div>
        </div>
      )}
      {pending ? (
        <>
          <div className="flex items-center gap-2 text-[12px]">
            <Switch checked={runSteps} onChange={setRunSteps} label="先执行 install/build 步骤" />
            <span className="text-muted">先执行 install/build 步骤</span>
          </div>
          {rejecting ? (
            <div className="flex flex-col gap-1.5">
              <TextArea
                rows={3}
                autoFocus
                value={feedback}
                onChange={(e) => setFeedback(e.target.value)}
                placeholder="说明哪里不对（可选），AI 会据此重做"
                aria-label="拒绝原因"
              />
              <div className="flex gap-1.5">
                <Button
                  size="sm"
                  variant="danger"
                  onClick={() => onAction(card.id, { action: 'reject', feedback: feedback.trim() || null })}
                >
                  确认拒绝
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setRejecting(false)}>
                  取消
                </Button>
              </div>
            </div>
          ) : (
            <div className="flex gap-1.5">
              <Button
                size="sm"
                variant="primary"
                onClick={() => onAction(card.id, { action: 'approve', run_steps: runSteps })}
              >
                批准并验证
              </Button>
              <Button size="sm" onClick={() => setRejecting(true)}>
                拒绝
              </Button>
            </div>
          )}
        </>
      ) : (
        <div className="text-[12px] text-muted">{STATE_TEXT[card.state] ?? card.state}</div>
      )}
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * verify：逐服务的验证结果 + 撤销 / 采纳最后配置
 * ------------------------------------------------------------------------- */

function VerifyCard({
  card,
  onAction,
}: {
  card: Extract<AiCardData, { kind: 'verify' }>
  onAction: AiCardActionHandler
}) {
  const locked = card.state === 'undone' || card.state === 'aborted'
  return (
    <Shell icon={<IconCheck size={13} />} title={`验证结果（第 ${card.attempt} 轮）`} state={card.state}>
      {card.services.length === 0 ? (
        <div className="text-[12px] text-muted">正在应用配方并验证…</div>
      ) : (
        <div className="flex flex-col gap-1">
          {card.services.map((s) => (
            <div key={s.name} className="flex items-start gap-1.5 text-[12px]">
              <span className={`mt-0.5 shrink-0 ${s.ok ? 'text-success' : 'text-error'}`}>
                {s.ok ? <IconCheck size={12} /> : <IconAlertTriangle size={12} />}
              </span>
              <span className="shrink-0 font-medium">{s.name}</span>
              <span className="tabular shrink-0 font-mono text-[11px] text-muted">
                {s.port !== null ? `:${s.port}` : s.phase}
              </span>
              <span className={`min-w-0 flex-1 break-words ${s.ok ? 'text-muted' : 'text-error'}`}>
                {s.message}
              </span>
            </div>
          ))}
        </div>
      )}
      <div className="flex flex-wrap gap-1.5">
        <Button size="sm" disabled={locked} onClick={() => onAction(card.id, { action: 'stop_services' })}>
          <IconSquare size={11} />
          停止服务
        </Button>
        {card.can_undo && (
          <Button size="sm" variant="danger" disabled={locked} onClick={() => onAction(card.id, { action: 'undo' })}>
            撤销配置
          </Button>
        )}
        {card.can_adopt_last && (
          <Button size="sm" disabled={locked} onClick={() => onAction(card.id, { action: 'adopt_last' })}>
            仍然采用最后一次配置
          </Button>
        )}
      </div>
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * diagnosis：原因 + 证据（可点开 VS Code）+ 修复
 * ------------------------------------------------------------------------- */

function DiagnosisCard({
  card,
  projectId,
  onAction,
}: {
  card: Extract<AiCardData, { kind: 'diagnosis' }>
  projectId: number
  onAction: AiCardActionHandler
}) {
  const [runSteps, setRunSteps] = useState(false)
  // `passed` 是终态：诊断已闭环，不再给动作按钮
  const locked = card.state === 'passed'
  return (
    <Shell
      icon={<IconStethoscope size={13} />}
      title={`诊断 ${card.service}`}
      state={card.state}
    >
      <div className="flex items-center gap-1.5">
        <Badge tone="neutral">{CATEGORY_TEXT[card.category] ?? card.category}</Badge>
        {card.rounds > 0 && <span className="text-[11px] text-muted">已重验 {card.rounds} 轮</span>}
      </div>
      <Row label="现象">{card.summary}</Row>
      <Row label="原因">{card.cause}</Row>
      {card.evidence.length > 0 && (
        <div className="flex flex-col gap-1">
          <span className="text-[11px] text-muted">证据</span>
          {card.evidence.map((ev, i) => (
            <div key={i} className="rounded-[var(--radius-control)] border border-border bg-surface-sunken px-2 py-1 text-[11px]">
              {ev.file && (
                <button
                  type="button"
                  onClick={() =>
                    api
                      .openInVscodeAt(projectId, ev.file as string, ev.line)
                      .catch((e) => console.warn('打开证据失败', ev.file, String(e)))
                  }
                  className="font-mono text-info underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/70"
                >
                  {ev.file}
                  {ev.line !== null ? `:${ev.line}` : ''}
                </button>
              )}
              <pre className="mt-0.5 whitespace-pre-wrap break-words font-mono text-muted">{ev.excerpt}</pre>
            </div>
          ))}
        </div>
      )}
      <Row label="建议">{card.suggestion}</Row>
      <div className="flex flex-wrap items-center gap-2">
        {card.config_fix !== null && (
          <>
            <div className="flex items-center gap-1.5 text-[12px]">
              <Switch checked={runSteps} onChange={setRunSteps} label="先执行 install/build 步骤" />
              <span className="text-muted">先执行步骤</span>
            </div>
            <Button
              size="sm"
              variant="primary"
              disabled={locked}
              onClick={() => onAction(card.id, { action: 'apply_fix', run_steps: runSteps })}
            >
              应用并验证
            </Button>
          </>
        )}
        <Button size="sm" disabled={locked} onClick={() => onAction(card.id, { action: 'reverify' })}>
          重新验证
        </Button>
        {card.rounds < 3 && (
          <Button
            size="sm"
            disabled={locked}
            onClick={() => onAction(card.id, { action: 'continue_diagnose' })}
          >
            继续诊断
          </Button>
        )}
      </div>
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * health：按严重程度分组的体检结果
 * ------------------------------------------------------------------------- */

function HealthItemRow({ item }: { item: AiHealthItem }) {
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    if (!copied) return
    const timer = window.setTimeout(() => setCopied(false), 1500)
    return () => window.clearTimeout(timer)
  }, [copied])

  return (
    <div className="rounded-[var(--radius-control)] border border-border px-2 py-1.5">
      <div className="flex items-start gap-1.5">
        <span className="min-w-0 flex-1 text-[12px] font-medium">{item.title}</span>
        {item.command && (
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              navigator.clipboard
                .writeText(item.command as string)
                .then(() => setCopied(true))
                .catch((e) => console.warn('复制失败', String(e)))
            }}
            title={item.command}
          >
            <IconCopy size={11} />
            {copied ? '已复制' : '复制命令'}
          </Button>
        )}
      </div>
      <div className="mt-0.5 whitespace-pre-wrap break-words text-[11px] text-muted">{item.detail}</div>
      {item.command && (
        <pre className="mt-1 overflow-auto whitespace-pre rounded bg-surface-sunken px-1.5 py-1 font-mono text-[11px]">
          {item.command}
        </pre>
      )}
    </div>
  )
}

function HealthCard({
  card,
  onRunTask,
}: {
  card: Extract<AiCardData, { kind: 'health' }>
  onRunTask: (task: AiTask) => void
}) {
  // 后端已按 error → warn → info 排序，这里再稳定分组一次，组内保持原顺序
  const groups = useMemo(() => {
    const by: Record<string, AiHealthItem[]> = {}
    for (const it of card.items) {
      const key = it.severity in SEVERITY_TONE ? it.severity : 'info'
      by[key] = [...(by[key] ?? []), it]
    }
    return Object.keys(by).sort((a, b) => (SEVERITY_ORDER[a] ?? 9) - (SEVERITY_ORDER[b] ?? 9))
  }, [card.items])

  return (
    <Shell icon={<IconSparkles size={13} />} title="体检报告">
      <Row label="结论">{card.summary}</Row>
      {groups.map((sev) => {
        const rows = card.items.filter(
          (it) => (it.severity in SEVERITY_TONE ? it.severity : 'info') === sev,
        )
        return (
          <div key={sev} className="flex flex-col gap-1">
            <div className="flex items-center gap-1.5">
              <Badge tone={SEVERITY_TONE[sev]}>{SEVERITY_TEXT[sev]}</Badge>
              <span className="text-[11px] text-muted">{rows.length} 项</span>
            </div>
            {rows.map((it, i) => (
              <HealthItemRow key={`${it.title}-${i}`} item={it} />
            ))}
          </div>
        )
      })}
      <div>
        <Button size="sm" onClick={() => onRunTask({ kind: 'health' })}>
          重新体检
        </Button>
      </div>
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * profile：三段勾选 + 采纳
 * ------------------------------------------------------------------------- */

function ProfileCard({
  card,
  onAction,
}: {
  card: Extract<AiCardData, { kind: 'profile' }>
  onAction: AiCardActionHandler
}) {
  const [useSummary, setUseSummary] = useState(true)
  const [stack, setStack] = useState<string[]>(card.stack)
  const [tags, setTags] = useState<string[]>(card.tags)
  const locked = card.state !== 'pending'

  const toggle = (list: string[], v: string) =>
    list.includes(v) ? list.filter((x) => x !== v) : [...list, v]

  return (
    <Shell icon={<IconIdCard size={13} />} title="项目画像" state={card.state}>
      <label className="flex items-start gap-2 text-[12px]">
        <input
          type="checkbox"
          className="mt-0.5 shrink-0 accent-[var(--primary)]"
          checked={useSummary}
          disabled={locked}
          onChange={(e) => setUseSummary(e.target.checked)}
        />
        <span className="min-w-0 flex-1">
          <span className="block text-[11px] text-muted">摘要</span>
          <span className="block break-words">{card.summary}</span>
        </span>
      </label>
      {card.stack.length > 0 && (
        <div>
          <span className="text-[11px] text-muted">技术栈</span>
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1">
            {card.stack.map((s) => (
              <label key={s} className="flex items-center gap-1 text-[12px]">
                <input
                  type="checkbox"
                  className="accent-[var(--primary)]"
                  checked={stack.includes(s)}
                  disabled={locked}
                  onChange={() => setStack((cur) => toggle(cur, s))}
                />
                {s}
              </label>
            ))}
          </div>
        </div>
      )}
      {card.tags.length > 0 && (
        <div>
          <span className="text-[11px] text-muted">标签</span>
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1">
            {card.tags.map((t) => (
              <label key={t} className="flex items-center gap-1 text-[12px]">
                <input
                  type="checkbox"
                  className="accent-[var(--primary)]"
                  checked={tags.includes(t)}
                  disabled={locked}
                  onChange={() => setTags((cur) => toggle(cur, t))}
                />
                {t}
              </label>
            ))}
          </div>
        </div>
      )}
      {stack.length > 0 && stack.length < card.stack.length && (
        // adopt_profile 的 `stack` 只是布尔：后端按整套写入，无法只挑几项。
        <div className="text-[11px] text-warning">技术栈按整套采纳，取消的项也会一起写入。</div>
      )}
      <div className="flex gap-1.5">
        <Button
          size="sm"
          variant="primary"
          disabled={locked}
          onClick={() =>
            onAction(card.id, { action: 'adopt_profile', summary: useSummary, stack: stack.length > 0, tags })
          }
        >
          采纳所选
        </Button>
        <Button size="sm" disabled={locked} onClick={() => onAction(card.id, { action: 'dismiss' })}>
          忽略
        </Button>
      </div>
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * readme：草稿预览 → 送进编辑器
 * ------------------------------------------------------------------------- */

const README_PREVIEW_LINES = 12

function ReadmeCard({
  card,
  onOpenReadmeDraft,
  onAction,
}: {
  card: Extract<AiCardData, { kind: 'readme' }>
  onOpenReadmeDraft: (content: string) => void
  onAction: AiCardActionHandler
}) {
  const preview = card.content.split('\n').slice(0, README_PREVIEW_LINES).join('\n')
  const more = card.content.split('\n').length > README_PREVIEW_LINES
  const locked = card.state !== 'pending'

  return (
    <Shell icon={<IconFileText size={13} />} title="README 草稿" state={card.state}>
      <div className="relative overflow-hidden rounded-[var(--radius-control)] border border-border bg-surface-sunken">
        <pre className="max-h-40 overflow-hidden whitespace-pre-wrap break-words p-2 font-mono text-[11px] text-muted">
          {preview}
        </pre>
        {more && (
          <div
            aria-hidden
            className="pointer-events-none absolute inset-x-0 bottom-0 h-8 bg-linear-to-t from-[var(--surface-sunken)] to-transparent"
          />
        )}
      </div>
      <div className="flex gap-1.5">
        <Button
          size="sm"
          variant="primary"
          disabled={locked}
          onClick={() => {
            onOpenReadmeDraft(card.content)
            onAction(card.id, { action: 'dismiss' })
          }}
        >
          在编辑器中打开草稿
        </Button>
        <Button size="sm" disabled={locked} onClick={() => onAction(card.id, { action: 'dismiss' })}>
          忽略
        </Button>
      </div>
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * service_control：AI 请求动服务，用户放行
 * ------------------------------------------------------------------------- */

const CONTROL_OUTCOME: Record<string, string> = {
  done: '已执行',
  failed: '执行失败',
  rejected: '已拒绝',
  expired: '已超时',
}

function ServiceControlCard({
  card,
  onAction,
}: {
  card: Extract<AiCardData, { kind: 'service_control' }>
  onAction: AiCardActionHandler
}) {
  const pending = card.state === 'pending'
  return (
    <Shell icon={<IconWrench size={13} />} title="服务操作确认" state={card.state}>
      <div className="text-[12px]">
        AI 请求{ACTION_TEXT[card.action] ?? card.action} {card.service}：{card.reason}
      </div>
      {pending ? (
        <div className="flex gap-1.5">
          <Button size="sm" variant="primary" onClick={() => onAction(card.id, { action: 'allow' })}>
            允许
          </Button>
          <Button size="sm" onClick={() => onAction(card.id, { action: 'deny' })}>
            拒绝
          </Button>
        </div>
      ) : (
        <div className={`text-[12px] ${card.state === 'failed' ? 'text-error' : 'text-muted'}`}>
          {CONTROL_OUTCOME[card.state] ?? card.state}
          {card.result ? `：${card.result}` : ''}
        </div>
      )}
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * onboarding：pi 没配好
 * ------------------------------------------------------------------------- */

function OnboardingCard({
  card,
  onOpenSettings,
}: {
  card: Extract<AiCardData, { kind: 'onboarding' }>
  onOpenSettings: () => void
}) {
  return (
    <Shell icon={<IconSettings size={13} />} title="先配置 AI">
      <div className="break-words text-[12px] text-error">{card.problem}</div>
      <div>
        <Button size="sm" variant="primary" onClick={onOpenSettings}>
          打开设置
        </Button>
      </div>
    </Shell>
  )
}

/* ------------------------------------------------------------------------- *
 * 入口
 * ------------------------------------------------------------------------- */

/** 一条 `ItemBody::Card`：按 `kind` 分发。未知 kind 显示兜底提示，不崩。 */
export function AiCard({
  card,
  projectId,
  onAction,
  onRunTask,
  onOpenReadmeDraft,
  onOpenSettings,
}: {
  card: AiCardData
  projectId: number
  onAction: AiCardActionHandler
  onRunTask: (task: AiTask) => void
  onOpenReadmeDraft: (content: string) => void
  onOpenSettings: () => void
}) {
  switch (card.kind) {
    case 'proposal':
      return <ProposalCard card={card} onAction={onAction} />
    case 'verify':
      return <VerifyCard card={card} onAction={onAction} />
    case 'diagnosis':
      return <DiagnosisCard card={card} projectId={projectId} onAction={onAction} />
    case 'health':
      return <HealthCard card={card} onRunTask={onRunTask} />
    case 'profile':
      return <ProfileCard card={card} onAction={onAction} />
    case 'readme':
      return <ReadmeCard card={card} onOpenReadmeDraft={onOpenReadmeDraft} onAction={onAction} />
    case 'service_control':
      return <ServiceControlCard card={card} onAction={onAction} />
    case 'onboarding':
      return <OnboardingCard card={card} onOpenSettings={onOpenSettings} />
    default: {
      // 后端加了新卡片而前端还没跟上：显示原文，不静默吞掉
      const unknown: never = card
      return (
        <Shell icon={<IconAlertTriangle size={13} />} title="未知卡片">
          <pre className="whitespace-pre-wrap break-words font-mono text-[11px] text-muted">
            {JSON.stringify(unknown)}
          </pre>
        </Shell>
      )
    }
  }
}
