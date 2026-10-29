import { useCallback, useEffect, useRef, useState } from 'react'
import { api, errorText, type ProjectInfo } from './api'
import Overview from './Overview'
import ReadmeSection from './Readme'
import ServiceSection from './Service'
import SourceSection from './Source'
import { Button, IconButton, Menu, Tabs } from './components'
import {
  IconChevronLeft,
  IconCode,
  IconFolder,
  IconGithub,
  IconMoreHorizontal,
  IconPencil,
  IconSparkles,
  IconTerminal,
} from './icons'
import { pickDirectory } from './util'
import type { AiIntent, AiTask, DetailTab } from './route'
import AiPanel from './AiPanel'

const TABS: { id: DetailTab; label: string }[] = [
  { id: 'overview', label: '概览' },
  { id: 'services', label: '服务' },
  { id: 'source', label: '源码' },
  { id: 'readme', label: 'README' },
]

/**
 * 项目详情：头部（返回 / 名称 / 路径 / 打开动作）+ 页签 + 内容区（右侧留 AI 抽屉槽）。
 *
 * README 有未保存修改时，任何会离开当前页签的动作都先确认——包括切换页签、
 * 返回列表、移除项目。确认框复用 Readme 里同一套文案。收起 AI 抽屉不算离开，
 * 所以不拦（草稿留着，用户随时可以再展开）。
 */
export default function Detail({
  project,
  size,
  runningServiceCount,
  dirtyCount,
  onBack,
  onChanged,
  onRemoved,
  onEditTags,
  tab,
  onTab,
  onAiTask,
  ai,
  onRouteSettings,
}: {
  project: ProjectInfo
  size?: number | null
  runningServiceCount: number
  dirtyCount: number | null
  onBack: () => void
  onChanged: () => Promise<void> | void
  onRemoved: () => void
  onEditTags: (p: ProjectInfo) => void
  tab: DetailTab
  onTab: (t: DetailTab) => void
  onAiTask: (task: AiTask) => void
  /** 外部（概览 / 服务页 / 侧栏）发起的 AI 请求：nonce 变化即打开抽屉。 */
  ai?: AiIntent
  /** 抽屉里「打开设置」的去处。 */
  onRouteSettings: () => void
}) {
  const [readmeDirty, setReadmeDirty] = useState(false)
  const [guard, setGuard] = useState<null | (() => void)>(null)
  const [removeOpen, setRemoveOpen] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [git, setGit] = useState<{ kind: 'status' | 'log'; loading: boolean; text: string } | null>(null)
  const [aiOpen, setAiOpen] = useState(false)
  /**
   * 待消费的 AI 任务：抽屉取走后立刻清空（`onTaskConsumed`），
   * 这样收起面板再从按钮展开不会重跑上一次的任务。
   */
  const [pendingAi, setPendingAi] = useState<{ task: AiTask; nonce: number } | null>(null)
  /** AI 生成的 README 草稿：nonce 让 Readme 只应用一次。 */
  const [readmeDraft, setReadmeDraft] = useState<{ content: string; nonce: number } | undefined>(undefined)
  const draftNonce = useRef(0)
  const aliveRef = useRef(true)
  const gitReq = useRef(0)

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  // 换项目时清空一次性的视图内状态：未保存标记挂在旧项目上，
  // 带着它去新项目会误判「有未保存修改」。
  useEffect(() => {
    setReadmeDirty(false)
    setGuard(null)
    setGit(null)
    setRemoveOpen(false)
    setError(null)
    setReadmeDraft(undefined)
    setPendingAi(null)
  }, [project.id])

  // 有新的 AI 请求落在路由上：打开抽屉；带任务的交给抽屉，`open` 只开抽屉
  // （侧栏点已有会话），不能顺手发一个任务。
  useEffect(() => {
    if (!ai) return
    setAiOpen(true)
    setPendingAi('task' in ai ? { task: ai.task, nonce: ai.nonce } : null)
  }, [ai?.nonce])

  const onDirtyChange = useCallback((d: boolean) => setReadmeDirty(d), [])
  const onError = useCallback((m: string | null) => setError(m), [])

  const run = async (fn: () => Promise<void>) => {
    try {
      await fn()
      setError(null)
    } catch (e) {
      setError(errorText(e))
    }
  }

  /** AI 草稿落到 README 页签：切过去并让 README 灌入内容（脏编辑器由它自己确认）。 */
  const openReadmeDraft = (content: string) => {
    draftNonce.current += 1
    setReadmeDraft({ content, nonce: draftNonce.current })
    if (tab !== 'readme') onTab('readme')
  }

  // 有未保存 README 修改时，先确认再执行后续动作
  const guardUnsaved = (proceed: () => void) => {
    if (readmeDirty) setGuard(() => proceed)
    else proceed()
  }

  const switchTab = (t: DetailTab) => {
    if (t === tab) return
    // 离开 README 才需要拦；在 README 里换到别的页签也一样拦。
    // 确认放弃后把脏标记清零：README 组件卸载时不会再回调 onDirtyChange，
    // 不清的话返回列表仍会被拦、页签栏还挂着「未保存」。
    guardUnsaved(() => {
      setReadmeDirty(false)
      // 离开 README 后草稿任务作废：再回来时不该把用户的后续编辑顶掉
      setReadmeDraft(undefined)
      onTab(t)
    })
  }

  const relink = async () => {
    const dir = await pickDirectory()
    if (!dir) return
    await run(async () => {
      await api.relinkProject(project.id, dir)
      await onChanged()
    })
  }

  const remove = async () => {
    await run(async () => {
      await api.removeProject(project.id)
      // removeProject 成功后父组件会卸载本页：守卫住后续 setState
      if (aliveRef.current) onRemoved()
    })
  }

  const showGit = async (kind: 'status' | 'log') => {
    const req = ++gitReq.current
    setGit({ kind, loading: true, text: '' })
    try {
      const r = await api.runGit(project.id, kind)
      // 后发先至：旧请求的结果丢弃，不覆盖新 kind 的面板
      if (aliveRef.current && req === gitReq.current) setGit({ kind, loading: false, text: r.text })
    } catch (e) {
      if (aliveRef.current && req === gitReq.current) setGit({ kind, loading: false, text: errorText(e) })
    }
  }

  const tabItems = TABS.map((t) => ({
    id: t.id,
    label: t.label,
    badge:
      t.id === 'services' && runningServiceCount > 0
        ? runningServiceCount
        : t.id === 'source' && dirtyCount !== null && dirtyCount > 0
          ? dirtyCount
          : undefined,
  }))

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex h-[52px] shrink-0 items-center gap-2 border-b border-border px-2">
        {/* 拖动区只挂在标题与空白上：按钮自身没有该属性，点击不会被当成拖窗口 */}
        <div className="flex min-w-0 flex-1 items-center gap-2 pr-2">
          <IconButton
            label="返回列表"
            onClick={() =>
              guardUnsaved(() => {
                setReadmeDirty(false)
                setReadmeDraft(undefined)
                onBack()
              })
            }
          >
            <IconChevronLeft size={15} />
          </IconButton>
          <h1 data-tauri-drag-region className="min-w-0 max-w-[40%] truncate text-[17px] font-semibold">
            {project.name}
          </h1>
          {project.missing && (
            <span className="shrink-0 rounded border border-error/40 bg-error/10 px-1.5 py-0.5 text-[11px] text-error">
              路径丢失
            </span>
          )}
          {!project.missing && (
            <button
              type="button"
              onClick={() => void run(() => api.openInFinder(project.id))}
              title={`${project.path}（点击在 Finder 中显示）`}
              className="min-w-0 max-w-[45%] truncate text-left font-mono text-[11px] text-muted hover:text-fg"
            >
              {project.path}
            </button>
          )}
          <div data-tauri-drag-region className="h-full min-w-2 flex-1" />
          <IconButton label="编辑 tag" className="h-6 w-6" onClick={() => onEditTags(project)}>
            <IconPencil size={12} />
          </IconButton>
        </div>

        <div className="flex shrink-0 items-center gap-0.5">
          {project.missing ? (
            <>
              <Button size="sm" variant="primary" onClick={relink}>
                重新关联
              </Button>
              <Button size="sm" variant="danger" onClick={() => setRemoveOpen(true)}>
                移除
              </Button>
            </>
          ) : (
            <>
              <IconButton label="在 Finder 中显示" onClick={() => void run(() => api.openInFinder(project.id))}>
                <IconFolder size={14} />
              </IconButton>
              <IconButton label="在 iTerm 中打开" onClick={() => void run(() => api.openInIterm(project.id))}>
                <IconTerminal size={14} />
              </IconButton>
              <IconButton label="在 VS Code 中打开" onClick={() => void run(() => api.openInVscode(project.id))}>
                <IconCode size={14} />
              </IconButton>
              {project.github_url && (
                <IconButton label="打开 GitHub 仓库" onClick={() => void run(() => api.openGithub(project.id))}>
                  <IconGithub size={14} />
                </IconButton>
              )}
              <Menu
                align="end"
                trigger={
                  <IconButton label="更多操作">
                    <IconMoreHorizontal size={14} />
                  </IconButton>
                }
                items={[
                  ...(project.has_git
                    ? [
                        { label: 'git status', onSelect: () => void showGit('status') },
                        { label: 'git log', onSelect: () => void showGit('log') },
                      ]
                    : []),
                  {
                    label: '从管理器移除',
                    danger: true,
                    onSelect: () => guardUnsaved(() => setRemoveOpen(true)),
                  },
                ]}
              />
            </>
          )}
          <Button
            size="sm"
            variant={aiOpen ? 'primary' : 'outline'}
            onClick={() => setAiOpen((v) => !v)}
            aria-pressed={aiOpen}
            title="AI 助手"
            className="ml-1"
          >
            <IconSparkles size={13} />
            AI 助手
          </Button>
        </div>
      </header>

      {/* 路径丢失时没有可读的内容：不显示页签，只留重新关联 / 移除 */}
      {!project.missing && (
        <div className="flex shrink-0 items-center gap-3 border-b border-border px-3">
          <Tabs items={tabItems} value={tab} onChange={(id) => switchTab(id as DetailTab)} />
          {readmeDirty && tab === 'readme' && (
            <span className="ml-auto shrink-0 pr-1 text-[11px] text-warning">●未保存</span>
          )}
        </div>
      )}

      {error && (
        <div className="mx-3 mt-2 flex items-start gap-2 rounded-[var(--radius-card)] border border-error/40 bg-error/10 px-3 py-2 text-[12px] text-error">
          <span className="min-w-0 flex-1 break-words">{error}</span>
          <button type="button" onClick={() => setError(null)} className="shrink-0 text-[11px] underline">
            关闭
          </button>
        </div>
      )}

      {git && (
        <div className="mx-3 mt-2 overflow-hidden rounded-[var(--radius-card)] border border-border">
          <div className="flex items-center gap-3 border-b border-border bg-surface-raised px-3 py-1.5">
            <span className="font-mono text-[11px] font-semibold">$ git {git.kind}</span>
            {git.loading && <span className="text-[11px] text-muted">执行中…</span>}
            <button
              type="button"
              onClick={() => setGit(null)}
              className="ml-auto rounded border border-border px-2 py-0.5 text-[11px]"
            >
              关闭
            </button>
          </div>
          <pre className="max-h-[35vh] select-text overflow-auto whitespace-pre bg-surface-sunken p-3 font-mono text-[11px] leading-relaxed">
            {git.loading ? '' : git.text}
          </pre>
        </div>
      )}

      <div className="flex min-h-0 flex-1">
        <div className="min-w-0 flex-1 overflow-y-auto p-4">
          {project.missing ? (
            <div className="mt-8 rounded-[var(--radius-card)] border border-dashed border-border p-8 text-center text-[13px] text-muted">
              项目路径已丢失：<span className="font-mono">{project.path}</span>
              <br />
              重新关联到新位置，或从管理器移除。
            </div>
          ) : tab === 'overview' ? (
            <Overview project={project} size={size} onChanged={onChanged} onAiTask={onAiTask} />
          ) : tab === 'services' ? (
            <ServiceSection
              project={project}
              onError={onError}
              onAiDiagnose={(service) => onAiTask({ kind: 'diagnose', service })}
            />
          ) : tab === 'source' ? (
            <SourceSection project={project} />
          ) : (
            <ReadmeSection
              key={project.path}
              projectId={project.id}
              onDirtyChange={onDirtyChange}
              onError={onError}
              aiDraft={readmeDraft}
              onAiDraftApplied={() => setReadmeDraft(undefined)}
            />
          )}
        </div>

        {aiOpen && !project.missing && (
          <aside
            className="flex w-[400px] shrink-0 flex-col overflow-hidden border-l border-border"
            data-ai-panel-slot
            aria-label="AI 助手"
          >
            <AiPanel
              projectId={project.id}
              projectName={project.name}
              initialTask={pendingAi?.task}
              taskNonce={pendingAi?.nonce}
              onTaskConsumed={() => setPendingAi(null)}
              onOpenSettings={onRouteSettings}
              onOpenReadmeDraft={openReadmeDraft}
              onClose={() => setAiOpen(false)}
            />
          </aside>
        )}
      </div>

      {removeOpen && (
        <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6">
          <div className="w-full max-w-sm rounded-xl bg-surface p-5">
            <h3 className="font-semibold">移除项目</h3>
            <p className="mt-2 text-sm">从管理器移除「{project.name}」？只删记录，不动文件。</p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setRemoveOpen(false)}
                className="rounded-lg border border-border px-3 py-1.5 text-sm"
              >
                取消
              </button>
              <button
                type="button"
                onClick={remove}
                className="rounded-lg bg-error-solid px-3 py-1.5 text-sm text-on-solid hover:brightness-95"
              >
                确认
              </button>
            </div>
          </div>
        </div>
      )}

      {guard && (
        <div className="fixed inset-0 z-40 flex items-center justify-center bg-scrim p-6">
          <div className="w-full max-w-sm rounded-xl bg-surface p-5">
            <h3 className="font-semibold">放弃未保存的修改？</h3>
            <p className="mt-2 text-sm">README 有未保存的修改，继续将丢失这些修改。</p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setGuard(null)}
                className="rounded-lg border border-border px-3 py-1.5 text-sm"
              >
                取消
              </button>
              <button
                type="button"
                onClick={() => {
                  const p = guard
                  setGuard(null)
                  p()
                }}
                className="rounded-lg bg-error-solid px-3 py-1.5 text-sm text-on-solid hover:brightness-95"
              >
                放弃修改
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
