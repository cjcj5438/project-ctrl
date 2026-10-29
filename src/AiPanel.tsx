import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  errorText,
  type AiCardAction,
  type AiEnv,
  type AiItem,
  type AiModel,
  type AiTask,
  type ModelRef,
  type ServiceStatus,
} from './api'
import { AiCard } from './AiCards'
import {
  Button,
  EmptyState,
  IconButton,
  Menu,
  Select,
  Spinner,
  StatusDot,
  TextArea,
} from './components'
import {
  IconActivity,
  IconAlertTriangle,
  IconCheck,
  IconChevronRight,
  IconFileText,
  IconIdCard,
  IconPanelRight,
  IconSend,
  IconSparkles,
  IconSquare,
  IconStethoscope,
  IconWrench,
} from './icons'
import { renderMarkdown } from './Readme'
import { useAiSession } from './useAiSession'
import { subscribe } from './util'

/** 判定「已经贴底」的阈值：小于它才自动滚动。 */
const STICK_PX = 40

const modelValue = (m: ModelRef) => `${m.provider}/${m.id}`

const parseModel = (models: AiModel[], value: string): ModelRef | null => {
  const hit = models.find((m) => modelValue(m) === value)
  return hit ? { provider: hit.provider, id: hit.id } : null
}

const NOTICE_CLASS: Record<string, string> = {
  error: 'text-error',
  warn: 'text-warning',
  info: 'text-muted',
}

const TOOL_TONE: Record<string, string> = {
  ok: 'text-success',
  error: 'text-error',
  blocked: 'text-warning',
  running: 'text-muted',
}

/**
 * AI 抽屉：会话头部（模型 / 状态 / 停止 / 新对话）+ 快捷任务 + 转录 + 输入框。
 *
 * 所有写操作都经 `useAiSession` 落到 IPC；本组件只管渲染与局部 UI 状态。
 */
export default function AiPanel({
  projectId,
  projectName,
  initialTask,
  taskNonce,
  onTaskConsumed,
  onOpenSettings,
  onOpenReadmeDraft,
  onClose,
}: {
  projectId: number
  projectName: string
  initialTask?: AiTask
  taskNonce?: number
  /** 任务已发出：父组件据此清掉它，收起面板再展开不会重跑。 */
  onTaskConsumed: () => void
  onOpenSettings: () => void
  onOpenReadmeDraft: (content: string) => void
  onClose: () => void
}) {
  const ai = useAiSession(projectId)
  const [env, setEnv] = useState<AiEnv | null>(null)
  const [models, setModels] = useState<AiModel[]>([])
  const [localError, setLocalError] = useState<string | null>(null)
  const [failed, setFailed] = useState<string[]>([])
  const [draft, setDraft] = useState('')
  /**
   * 模型下拉的当前值。后端选「跟随设置」时不会清 `model`（它只是不再指定模型），
   * 所以这里自己记住用户的选择，服务端再推模型时以服务端为准。
   */
  const [modelPick, setModelPick] = useState('')
  const serverModel = ai.state?.model ?? ''
  useEffect(() => {
    setModelPick(serverModel)
  }, [serverModel])

  const scrollRef = useRef<HTMLDivElement>(null)
  const stick = useRef(true)
  /** 已消费的任务 nonce：详情页重渲染不会重发同一个任务。 */
  const taskRef = useRef(ai.runTask)
  taskRef.current = ai.runTask
  /** 已发出的任务 nonce：父组件清理状态之前不重复发。 */
  const fired = useRef<number | null>(null)

  const busy =
    ai.state?.state === 'busy' ||
    ai.state?.state === 'starting' ||
    ai.state?.state === 'recovering'

  // pi 环境：问题状态下用引导块替换输入框
  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const e = await api.aiStatus()
        if (!alive) return
        setEnv(e)
        if (!e.problem) {
          const list = await api.aiModels(false)
          if (alive) setModels(list)
        }
      } catch (e) {
        if (alive) setLocalError(errorText(e))
      }
    })()
    return () => {
      alive = false
    }
  }, [])

  // 失败的服务：诊断入口的数据源（订阅 svc:state，不轮询）
  useEffect(() => {
    const pick = (list: ServiceStatus[]) =>
      list
        .filter((s) => s.project_id === projectId && (s.phase === 'Crashed' || s.phase === 'Failed'))
        .map((s) => s.name)
    let alive = true
    const byKey: Record<string, ServiceStatus> = {}
    void (async () => {
      try {
        const list = await api.serviceStatus()
        if (!alive) return
        // 快照可能比先到的事件旧：只补事件里还没有的 key
        list.forEach((s) => {
          if (!(s.key in byKey)) byKey[s.key] = s
        })
        setFailed(pick(Object.values(byKey)))
      } catch {
        // 诊断按钮只是便利入口：拿不到状态就当作没有异常服务
      }
    })()
    const stop = subscribe<ServiceStatus>('svc:state', (e) => {
      byKey[e.payload.key] = e.payload
      setFailed(pick(Object.values(byKey)))
    })
    return () => {
      alive = false
      stop()
    }
  }, [projectId])

  // 外部请求的任务：发一次就报「已消费」，父组件清掉后这个 effect 不会再触发
  useEffect(() => {
    if (!initialTask || taskNonce === undefined || fired.current === taskNonce) return
    fired.current = taskNonce
    stick.current = true
    void taskRef.current(initialTask)
    onTaskConsumed()
  }, [initialTask, taskNonce, onTaskConsumed])

  // 贴底时才自动滚动；用户往上翻就不打扰
  useEffect(() => {
    const el = scrollRef.current
    if (!el || !stick.current) return
    el.scrollTop = el.scrollHeight
  }, [ai.items])

  const onScroll = () => {
    const el = scrollRef.current
    if (!el) return
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight <= STICK_PX
  }

  const runTask = useCallback(
    (task: AiTask) => {
      stick.current = true
      void ai.runTask(task)
    },
    [ai],
  )

  const submit = () => {
    const text = draft.trim()
    if (!text || busy) return
    setDraft('')
    stick.current = true
    void ai.send(text)
  }

  // 当前模型可能不在列表里（任务按设置换过模型、pi 升级后列表变了）：补一个只读选项，避免下拉显示空白
  const modelOptions = useMemo(() => {
    const opts = [
      { value: '', label: '跟随设置' },
      ...models.map((m) => ({ value: modelValue(m), label: `${m.provider} / ${m.name}` })),
    ]
    if (modelPick !== '' && !opts.some((o) => o.value === modelPick)) {
      opts.push({ value: modelPick, label: modelPick })
    }
    return opts
  }, [models, modelPick])

  const err = ai.error ?? localError
  const dotTone = ai.state
    ? ai.state.state === 'failed'
      ? 'bad'
      : busy
        ? 'busy'
        : 'idle'
    : 'idle'
  const dotLabel = ai.state
    ? ai.state.awaiting_user
      ? '等待确认'
      : busy
        ? '运行中'
        : ai.state.state === 'failed'
          ? '异常'
          : '就绪'
    : '未连接'

  const disabled = busy

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 flex-col gap-1.5 border-b border-border px-2 py-2">
        <div className="flex items-center gap-1.5">
          <IconSparkles size={14} className="text-ai" />
          <span className="text-[13px] font-semibold">AI 助手</span>
          {busy && <Spinner size={11} />}
          <div className="ml-auto flex items-center gap-0.5">
            <IconButton label="停止本次生成" disabled={!busy} onClick={() => void ai.abort()}>
              <IconSquare size={13} />
            </IconButton>
            <Button size="sm" variant="ghost" onClick={() => void ai.close()} title="关闭当前会话并开一个新的">
              新对话
            </Button>
            <IconButton label="收起 AI 面板" onClick={onClose}>
              <IconPanelRight size={14} />
            </IconButton>
          </div>
        </div>
        <div className="flex items-center gap-1.5">
          <StatusDot tone={dotTone} label={dotLabel} title={`会话状态：${ai.state?.state ?? '未连接'}`} />
          <Select
            className="min-w-0 flex-1"
            value={modelPick}
            options={modelOptions}
            disabled={models.length === 0 || !ai.state}
            onChange={(v) => {
              setModelPick(v)
              // 「跟随设置」= 不再指定模型（thinking 传 null 表示保持不变）
              void ai.setModel(v === '' ? null : parseModel(models, v), null)
            }}
          />
        </div>
      </header>

      <div className="flex shrink-0 flex-wrap gap-1 border-b border-border px-2 py-1.5">
        <Button size="sm" variant="ghost" disabled={disabled} onClick={() => runTask({ kind: 'configure' })}>
          <IconWrench size={11} />
          配置服务
        </Button>
        {failed.length > 1 ? (
          <Menu
            align="start"
            trigger={
              <Button size="sm" variant="ghost" disabled={disabled}>
                <IconStethoscope size={11} />
                诊断
              </Button>
            }
            items={failed.map((name) => ({
              label: name,
              icon: <IconStethoscope size={12} />,
              onSelect: () => runTask({ kind: 'diagnose', service: name }),
            }))}
          />
        ) : (
          <Button
            size="sm"
            variant="ghost"
            disabled={disabled || failed.length === 0}
            title={failed.length === 0 ? '没有异常服务可诊断' : `诊断 ${failed[0]}`}
            onClick={() => failed.length === 1 && runTask({ kind: 'diagnose', service: failed[0] })}
          >
            <IconStethoscope size={11} />
            诊断
          </Button>
        )}
        <Button size="sm" variant="ghost" disabled={disabled} onClick={() => runTask({ kind: 'health' })}>
          <IconActivity size={11} />
          体检
        </Button>
        <Button size="sm" variant="ghost" disabled={disabled} onClick={() => runTask({ kind: 'profile' })}>
          <IconIdCard size={11} />
          画像
        </Button>
        <Button size="sm" variant="ghost" disabled={disabled} onClick={() => runTask({ kind: 'readme' })}>
          <IconFileText size={11} />
          补全 README
        </Button>
      </div>

      {err && (
        <div className="shrink-0 border-b border-error/40 bg-error/10 px-2 py-1 text-[11px] text-error">
          <span className="break-words">{err}</span>
        </div>
      )}

      <div ref={scrollRef} onScroll={onScroll} className="min-h-0 flex-1 overflow-y-auto px-2 py-2">
        {ai.items.length === 0 ? (
          <EmptyState
            icon={<IconSparkles size={20} />}
            title="问问 AI 关于这个项目的事"
            hint={`${projectName}：可以让它体检、诊断失败的服务、补全画像与 README。AI 只读，不会改动文件。`}
          />
        ) : (
          <div className="flex flex-col gap-2">
            {ai.items.map((it) => (
              <ItemView
                key={it.id}
                item={it}
                projectId={projectId}
                onAction={ai.cardAction}
                onRunTask={runTask}
                onOpenReadmeDraft={onOpenReadmeDraft}
                onOpenSettings={onOpenSettings}
                onRetry={() => void ai.retry()}
              />
            ))}
          </div>
        )}
      </div>

      <div className="shrink-0 border-t border-border px-2 py-2">
        {env?.problem ? (
          <div className="flex flex-col gap-1.5 rounded-[var(--radius-card)] border border-warning/40 bg-warning/10 px-2 py-1.5">
            <div className="flex items-start gap-1.5 text-[11px] text-warning">
              <IconAlertTriangle size={12} />
              <span className="min-w-0 flex-1 break-words">{env.problem}</span>
            </div>
            <div>
              <Button size="sm" variant="primary" onClick={onOpenSettings}>
                打开设置
              </Button>
            </div>
          </div>
        ) : (
          <>
            <TextArea
              rows={2}
              value={draft}
              disabled={disabled}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault()
                  submit()
                }
              }}
              placeholder="问点什么，或让 AI 帮你管理这个项目（只读）"
              aria-label="给 AI 的消息"
            />
            <div className="mt-1 flex items-center gap-1.5">
              <span className="min-w-0 flex-1 text-[11px] text-subtle">
                AI 全程只读，不会修改项目文件
              </span>
              <Button
                size="sm"
                variant="primary"
                disabled={disabled || draft.trim() === ''}
                onClick={submit}
                title="发送（Enter）"
              >
                <IconSend size={11} />
                发送
              </Button>
            </div>
          </>
        )}
      </div>
    </div>
  )
}

/** 单条转录：按 `body.type` 分发。 */
function ItemView({
  item,
  projectId,
  onAction,
  onRunTask,
  onOpenReadmeDraft,
  onOpenSettings,
  onRetry,
}: {
  item: AiItem
  projectId: number
  onAction: (cardId: string, action: AiCardAction) => void
  onRunTask: (task: AiTask) => void
  onOpenReadmeDraft: (content: string) => void
  onOpenSettings: () => void
  onRetry: () => void
}) {
  const body = item.body
  switch (body.type) {
    case 'user':
      return <UserItem text={body.text} task={body.task} />
    case 'assistant':
      return <AssistantItem text={body.text} thinking={body.thinking} streaming={body.streaming} />
    case 'tool':
      return <ToolItem name={body.name} summary={body.summary} status={body.status} detail={body.detail} />
    case 'card':
      return (
        <AiCard
          card={body.card}
          projectId={projectId}
          onAction={onAction}
          onRunTask={onRunTask}
          onOpenReadmeDraft={onOpenReadmeDraft}
          onOpenSettings={onOpenSettings}
        />
      )
    case 'notice':
      return (
        <div className={`flex items-start gap-1.5 text-[11px] ${NOTICE_CLASS[body.level] ?? 'text-muted'}`}>
          <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{body.text}</span>
          {body.retry && (
            <Button size="sm" variant="ghost" onClick={onRetry}>
              重试
            </Button>
          )}
        </div>
      )
  }
}

function UserItem({ text, task }: { text: string; task: string | null }) {
  return (
    <div className="flex flex-col items-end gap-1">
      {task && <span className="rounded-full bg-surface-sunken px-1.5 py-0.5 text-[10px] text-muted">{task}</span>}
      <div className="max-w-[85%] rounded-[var(--radius-card)] bg-primary/15 px-2.5 py-1.5 text-[12px] text-fg">
        <div className="whitespace-pre-wrap break-words">{text}</div>
      </div>
    </div>
  )
}

function AssistantItem({
  text,
  thinking,
  streaming,
}: {
  text: string
  thinking: string
  streaming: boolean
}) {
  const [showThinking, setShowThinking] = useState(false)
  const html = useMemo(() => renderMarkdown(text), [text])
  return (
    <div className="flex flex-col gap-1">
      {thinking !== '' && (
        <div>
          <button
            type="button"
            onClick={() => setShowThinking((v) => !v)}
            aria-expanded={showThinking}
            className="flex items-center gap-1 rounded text-[11px] text-muted hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/70"
          >
            <IconChevronRight
              size={11}
              className={`transition-transform duration-150 ${showThinking ? 'rotate-90' : ''}`}
            />
            思考过程
          </button>
          {showThinking && (
            <pre className="mt-1 max-h-60 overflow-auto whitespace-pre-wrap break-words rounded-[var(--radius-control)] border border-border bg-surface-sunken p-2 font-mono text-[11px] text-muted">
              {thinking}
            </pre>
          )}
        </div>
      )}
      <div
        className="md-body text-[12px]"
        dangerouslySetInnerHTML={{ __html: html }}
      />
      {streaming && <span aria-hidden className="inline-block h-3 w-1.5 animate-pulse bg-fg/70" />}
    </div>
  )
}

function ToolItem({
  name,
  summary,
  status,
  detail,
}: {
  name: string
  summary: string
  status: string
  detail: string
}) {
  const [open, setOpen] = useState(false)
  const icon =
    status === 'running' ? (
      <Spinner size={11} />
    ) : status === 'ok' ? (
      <IconCheck size={11} />
    ) : (
      <IconAlertTriangle size={11} />
    )
  return (
    <div className="flex flex-col gap-1">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        disabled={detail === ''}
        className="flex items-center gap-1.5 rounded text-left text-[11px] text-muted transition-colors hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/70 disabled:cursor-default"
        title={name}
      >
        <span className={`shrink-0 ${TOOL_TONE[status] ?? 'text-muted'}`}>{icon}</span>
        <span className="min-w-0 flex-1 truncate">{summary}</span>
      </button>
      {open && detail !== '' && (
        <pre className="max-h-60 overflow-auto whitespace-pre-wrap break-words rounded-[var(--radius-control)] border border-border bg-surface-sunken p-2 font-mono text-[11px] text-muted">
          {detail}
        </pre>
      )}
    </div>
  )
}
