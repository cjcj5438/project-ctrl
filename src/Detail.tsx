import { useCallback, useState } from 'react'
import { api, errorText, type ProjectInfo } from './api'
import ReadmeSection from './Readme'
import { pickDirectory } from './util'

/** 详情页：项目信息 + tag 编辑 + 打开按钮 + README 区域。 */
export default function DetailView({
  project,
  onBack,
  onChanged,
  onRemoved,
}: {
  project: ProjectInfo
  onBack: () => void
  onChanged: () => Promise<void> | void
  onRemoved: () => void
}) {
  const [readmeDirty, setReadmeDirty] = useState(false)
  const [guard, setGuard] = useState<null | (() => void)>(null)
  const [removeOpen, setRemoveOpen] = useState(false)
  const [tagInput, setTagInput] = useState('')
  const [error, setError] = useState<string | null>(null)

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

  return (
    <div className="min-h-screen bg-neutral-100 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      <div className="mx-auto max-w-4xl px-6 py-6">
        <button
          onClick={() => guardUnsaved(onBack)}
          className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
        >
          ← 返回
        </button>

        {error && (
          <div className="mt-4 rounded-lg border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-200">
            {error}
          </div>
        )}

        <div className="mt-4 flex items-center gap-2">
          <h1 className="text-xl font-bold">{project.name}</h1>
          {project.missing && (
            <span className="rounded bg-red-100 px-1.5 py-0.5 text-xs text-red-700 dark:bg-red-900 dark:text-red-200">
              路径丢失
            </span>
          )}
        </div>
        <div className="mt-1 break-all text-xs text-neutral-500">{project.path}</div>
        <div className="mt-1 text-xs text-neutral-500">创建日期：{project.created_date ?? '未知'}</div>

        {project.missing ? (
          <div className="mt-6 flex gap-2">
            <button onClick={relink} className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-700">
              重新关联
            </button>
            <button
              onClick={() => guardUnsaved(remove)}
              className="rounded border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
            >
              移除
            </button>
          </div>
        ) : (
          <>
            <div className="mt-4">
              <div className="text-sm font-semibold text-neutral-500">tag</div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5">
                {project.manual_tags.map((t) => (
                  <span
                    key={t}
                    className="inline-flex items-center gap-1 rounded-full bg-blue-100 px-2 py-0.5 text-xs text-blue-800 dark:bg-blue-900 dark:text-blue-100"
                  >
                    {t}
                    <button onClick={() => removeTag(t)} title="移除 tag" className="opacity-60 hover:opacity-100">
                      ×
                    </button>
                  </span>
                ))}
                {project.auto_tags.map((t) => (
                  <span
                    key={t}
                    className="rounded-full border border-dashed border-neutral-400 px-2 py-0.5 text-xs text-neutral-500 dark:border-neutral-600 dark:text-neutral-300"
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
                  className="w-32 rounded-full border border-neutral-300 bg-white px-2 py-0.5 text-xs outline-none focus:border-blue-500 dark:border-neutral-700 dark:bg-neutral-800"
                />
                <button onClick={addTag} className="rounded-full border border-neutral-300 px-2 py-0.5 text-xs dark:border-neutral-700">
                  添加
                </button>
              </div>
            </div>

            <div className="mt-4 flex flex-wrap gap-2 text-xs">
              <button onClick={() => run(() => api.openInFinder(project.id))} className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700">
                Finder
              </button>
              <button onClick={() => run(() => api.openInIterm(project.id))} className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700">
                iTerm
              </button>
              <button onClick={() => run(() => api.openInVscode(project.id))} className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700">
                VS Code
              </button>
              {project.github_url && (
                <button onClick={() => run(() => api.openGithub(project.id))} className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700">
                  GitHub
                </button>
              )}
              <button
                onClick={() => guardUnsaved(() => setRemoveOpen(true))}
                className="ml-auto rounded border border-red-300 px-2 py-1 text-red-700 dark:border-red-800 dark:text-red-300"
              >
                从管理器移除
              </button>
            </div>

            <ReadmeSection key={project.path} projectId={project.id} onDirtyChange={onDirtyChange} onError={onError} />
          </>
        )}

        {removeOpen && (
          <div className="fixed inset-0 z-30 flex items-center justify-center bg-black/40 p-6">
            <div className="w-full max-w-sm rounded-xl bg-white p-5 dark:bg-neutral-900">
              <h3 className="font-semibold">移除项目</h3>
              <p className="mt-2 text-sm">从管理器移除「{project.name}」？只删记录，不动文件。</p>
              <div className="mt-4 flex justify-end gap-2">
                <button onClick={() => setRemoveOpen(false)} className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700">
                  取消
                </button>
                <button onClick={remove} className="rounded-lg bg-red-600 px-3 py-1.5 text-sm text-white hover:bg-red-700">
                  确认
                </button>
              </div>
            </div>
          </div>
        )}

        {guard && (
          <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-6">
            <div className="w-full max-w-sm rounded-xl bg-white p-5 dark:bg-neutral-900">
              <h3 className="font-semibold">放弃未保存的修改？</h3>
              <p className="mt-2 text-sm">README 有未保存的修改，继续将丢失这些修改。</p>
              <div className="mt-4 flex justify-end gap-2">
                <button onClick={() => setGuard(null)} className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700">
                  取消
                </button>
                <button
                  onClick={() => {
                    const p = guard
                    setGuard(null)
                    p()
                  }}
                  className="rounded-lg bg-red-600 px-3 py-1.5 text-sm text-white hover:bg-red-700"
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
