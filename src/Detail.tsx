import { useCallback, useState } from 'react'
import { api, errorText, type ProjectInfo } from './api'
import { humanSize } from './format'
import ReadmeSection from './Readme'
import ServiceSection from './Service'
import SourceSection from './Source'
import { ErrorBanner } from './components'
import { IconChevronLeft, IconX } from './icons'
import { pickDirectory } from './util'

/** 详情页：项目信息 + tag 编辑 + 打开按钮 + README 区域。 */
export default function DetailView({
  project,
  size,
  onBack,
  onChanged,
  onRemoved,
}: {
  project: ProjectInfo
  size?: number | null
  onBack: () => void
  onChanged: () => Promise<void> | void
  onRemoved: () => void
}) {
  const [readmeDirty, setReadmeDirty] = useState(false)
  const [guard, setGuard] = useState<null | (() => void)>(null)
  const [removeOpen, setRemoveOpen] = useState(false)
  const [tagInput, setTagInput] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [git, setGit] = useState<{ kind: 'status' | 'log'; loading: boolean; text: string } | null>(null)

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

  // 有未保存 README 修改时，先确认再执行后续动作
  const guardUnsaved = (proceed: () => void) => {
    if (readmeDirty) setGuard(() => proceed)
    else proceed()
  }

  const addTag = async () => {
    const t = tagInput.trim()
    if (!t) return
    await run(async () => {
      await api.setProjectTags(project.id, [...new Set([...project.manual_tags, t])])
      setTagInput('')
      await onChanged()
    })
  }

  const removeTag = async (t: string) => {
    await run(async () => {
      await api.setProjectTags(project.id, project.manual_tags.filter((x) => x !== t))
      await onChanged()
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
      onRemoved()
    })
  }

  const showGit = async (kind: 'status' | 'log') => {
    setGit({ kind, loading: true, text: '' })
    try {
      const r = await api.runGit(project.id, kind)
      setGit({ kind, loading: false, text: r.text })
    } catch (e) {
      setGit({ kind, loading: false, text: errorText(e) })
    }
  }

  return (
    <div className="min-h-screen bg-surface-raised text-fg ">
      <div className="mx-auto max-w-4xl px-6 py-6">
        <button
          onClick={() => guardUnsaved(onBack)}
          className="inline-flex items-center gap-1 rounded-lg border border-border px-3 py-1.5 text-sm"
        >
          <IconChevronLeft />
          返回
        </button>

        {error && <ErrorBanner text={error} onClose={() => setError(null)} />}

        <div className="mt-4 flex items-center gap-2">
          <h1 className="text-xl font-bold">{project.name}</h1>
          {project.missing && (
            <span className="rounded border border-error/40 bg-error/10 px-1.5 py-0.5 text-xs text-error">
              路径丢失
            </span>
          )}
        </div>
        <div className="mt-1 break-all text-xs text-muted">{project.path}</div>
        <div className="mt-1 text-xs text-muted">创建日期：{project.created_date ?? '未知'}</div>
        {!project.missing && (
          <div className="mt-1 text-xs text-muted">
            大小：{size === undefined ? '计算中…' : size === null ? '—' : humanSize(size)}
          </div>
        )}

        {project.missing ? (
          <div className="mt-6 flex gap-2">
            <button onClick={relink} className="rounded bg-primary px-3 py-1.5 text-sm text-primary-fg hover:brightness-110">
              重新关联
            </button>
            <button
              onClick={() => guardUnsaved(remove)}
              className="rounded border border-border px-3 py-1.5 text-sm "
            >
              移除
            </button>
          </div>
        ) : (
          <>
            <div className="mt-4">
              <div className="text-sm font-semibold text-muted">tag</div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5">
                {project.manual_tags.map((t) => (
                  <span
                    key={t}
                    className="inline-flex items-center gap-1 rounded-full border border-accent/40 bg-accent/10 px-2 py-0.5 text-xs text-fg"
                  >
                    {t}
                    <button
                      onClick={() => removeTag(t)}
                      title="移除 tag"
                      aria-label={`移除 tag ${t}`}
                      className="opacity-60 transition-opacity hover:opacity-100"
                    >
                      <IconX size={12} />
                    </button>
                  </span>
                ))}
                {project.auto_tags.map((t) => (
                  <span
                    key={t}
                    className="rounded-full border border-dashed border-border px-2 py-0.5 text-xs text-muted "
                    title="自动 tag（只读）"
                  >
                    {t}
                  </span>
                ))}
                <input
                  value={tagInput}
                  onChange={(e) => setTagInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') void addTag()
                  }}
                  placeholder="添加 tag…"
                  className="w-32 rounded-full border border-border bg-surface px-2 py-0.5 text-xs focus:border-accent"
                />
                <button onClick={addTag} className="rounded-full border border-border px-2 py-0.5 text-xs ">
                  添加
                </button>
              </div>
            </div>

            <div className="mt-4 flex flex-wrap gap-2 text-xs">
              <button onClick={() => run(() => api.openInFinder(project.id))} className="rounded border border-border px-2 py-1 ">
                Finder
              </button>
              <button onClick={() => run(() => api.openInIterm(project.id))} className="rounded border border-border px-2 py-1 ">
                iTerm
              </button>
              <button onClick={() => run(() => api.openInVscode(project.id))} className="rounded border border-border px-2 py-1 ">
                VS Code
              </button>
              {project.github_url && (
                <button onClick={() => run(() => api.openGithub(project.id))} className="rounded border border-border px-2 py-1 ">
                  GitHub
                </button>
              )}
              {project.has_git && (
                <>
                  <button
                    onClick={() => void showGit('status')}
                    className="rounded border border-border px-2 py-1 "
                  >
                    git status
                  </button>
                  <button
                    onClick={() => void showGit('log')}
                    className="rounded border border-border px-2 py-1 "
                  >
                    git log
                  </button>
                </>
              )}
              <button
                onClick={() => guardUnsaved(() => setRemoveOpen(true))}
                className="ml-auto rounded border border-error/50 px-2 py-1 text-error "
              >
                从管理器移除
              </button>
            </div>

            {git && (
              <div className="mt-4 overflow-hidden rounded-lg border border-border ">
                <div className="flex items-center gap-3 border-b border-border bg-surface-raised px-3 py-2 ">
                  <span className="font-mono text-xs font-semibold">$ git {git.kind}</span>
                  {git.loading && <span className="text-xs text-muted">执行中…</span>}
                  <button
                    onClick={() => setGit(null)}
                    className="ml-auto rounded border border-border px-2 py-0.5 text-xs "
                  >
                    关闭
                  </button>
                </div>
                <pre className="max-h-[50vh] select-text overflow-auto whitespace-pre p-3 font-mono text-xs leading-relaxed">
                  {git.loading ? '' : git.text}
                </pre>
              </div>
            )}

            <SourceSection project={project} />
            <ServiceSection project={project} onError={onError} />

            <ReadmeSection key={project.path} projectId={project.id} onDirtyChange={onDirtyChange} onError={onError} />
          </>
        )}

        {removeOpen && (
          <div className="fixed inset-0 z-30 flex items-center justify-center bg-black/40 p-6">
            <div className="w-full max-w-sm rounded-xl bg-surface p-5 ">
              <h3 className="font-semibold">移除项目</h3>
              <p className="mt-2 text-sm">从管理器移除「{project.name}」？只删记录，不动文件。</p>
              <div className="mt-4 flex justify-end gap-2">
                <button onClick={() => setRemoveOpen(false)} className="rounded-lg border border-border px-3 py-1.5 text-sm ">
                  取消
                </button>
                <button onClick={remove} className="rounded-lg bg-error-solid px-3 py-1.5 text-sm text-white hover:brightness-110">
                  确认
                </button>
              </div>
            </div>
          </div>
        )}

        {guard && (
          <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-6">
            <div className="w-full max-w-sm rounded-xl bg-surface p-5 ">
              <h3 className="font-semibold">放弃未保存的修改？</h3>
              <p className="mt-2 text-sm">README 有未保存的修改，继续将丢失这些修改。</p>
              <div className="mt-4 flex justify-end gap-2">
                <button onClick={() => setGuard(null)} className="rounded-lg border border-border px-3 py-1.5 text-sm ">
                  取消
                </button>
                <button
                  onClick={() => {
                    const p = guard
                    setGuard(null)
                    p()
                  }}
                  className="rounded-lg bg-error-solid px-3 py-1.5 text-sm text-white hover:brightness-110"
                >
                  放弃修改
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}
