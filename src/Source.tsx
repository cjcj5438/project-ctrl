import { useCallback, useEffect, useRef, useState } from 'react'
import { api, errorText, type GitResult, type ProjectInfo, type SourceInfo } from './api'

/** 源码更新面板：分支 / 落后 / 改动 + fetch / pull。折叠时只显示一行摘要。 */
export default function SourceSection({ project }: { project: ProjectInfo }) {
  const [opened, setOpened] = useState(false)
  const [info, setInfo] = useState<SourceInfo | null>(null)
  const [out, setOut] = useState<{ title: string; result: GitResult } | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const aliveRef = useRef(true)

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  const reload = useCallback(async () => {
    try {
      const next = await api.sourceInfo(project.id)
      if (!aliveRef.current) return
      setInfo(next)
      setError(null)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    }
  }, [project.id])

  useEffect(() => {
    if (!opened) return
    void reload()
  }, [opened, reload])

  const act = async (kind: 'fetch' | 'pull') => {
    setBusy(true)
    try {
      const result = kind === 'fetch' ? await api.sourceFetch(project.id) : await api.sourcePull(project.id)
      if (!aliveRef.current) return
      setOut({ title: `git ${kind}${kind === 'pull' ? ' --ff-only' : ''}`, result })
      await reload()
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    } finally {
      if (aliveRef.current) setBusy(false)
    }
  }

  const summary = !info
    ? project.has_git
      ? '展开查看'
      : '不是 git 仓库'
    : !info.is_repo
      ? '不是 git 仓库'
      : [info.detached ? 'detached HEAD' : info.branch, info.behind > 0 ? `落后 ${info.behind}` : null]
          .filter(Boolean)
          .join(' · ')

  return (
    <details
      className="mt-4 rounded-lg border border-border "
      onToggle={(e) => {
        if (e.currentTarget.open) setOpened(true)
      }}
    >
      <summary className="cursor-pointer px-3 py-2 text-sm font-semibold">
        源码 <span className="ml-1 font-normal text-muted">{summary}</span>
      </summary>
      <div className="border-t border-border p-3 ">
        {error && <div className="mb-2 text-xs text-error ">{error}</div>}
        {!project.has_git ? (
          <div className="text-sm text-muted">不是 git 仓库。</div>
        ) : !info ? (
          <div className="text-sm text-muted">读取中…</div>
        ) : !info.is_repo ? (
          <div className="text-sm text-muted">不是 git 仓库。</div>
        ) : (
          <>
            <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted ">
              <span>分支：{info.detached ? '（detached HEAD）' : (info.branch ?? '未知')}</span>
              <span>upstream：{info.upstream ?? '无'}</span>
              <span>改动：{info.dirty}</span>
              <span>落后：{info.behind}</span>
              <span>领先：{info.ahead}</span>
            </div>
            <div className="mt-3 flex flex-wrap gap-2 text-xs">
              <button
                type="button"
                onClick={() => void act('fetch')}
                disabled={busy}
                className="rounded border border-border px-2 py-1 disabled:opacity-40 "
              >
                检查更新（fetch）
              </button>
              <button
                type="button"
                onClick={() => void act('pull')}
                disabled={busy || !info.can_pull}
                title={info.can_pull ? '快进合并' : (info.pull_blocked_by ?? '')}
                className="rounded border border-border px-2 py-1 disabled:opacity-40 "
              >
                拉取（pull --ff-only）
              </button>
              {!info.can_pull && info.pull_blocked_by && (
                <span className="self-center text-muted">不可拉取：{info.pull_blocked_by}</span>
              )}
            </div>
            {out && (
              <div className="mt-3 overflow-hidden rounded border border-border ">
                <div className="flex items-center gap-2 border-b border-border bg-surface-raised px-2 py-1 ">
                  <span className="font-mono text-[11px] font-semibold">$ {out.title}</span>
                  <button
                    type="button"
                    onClick={() => setOut(null)}
                    className="ml-auto rounded border border-border px-2 py-0.5 text-[11px] "
                  >
                    关闭
                  </button>
                </div>
                <pre className="max-h-[30vh] select-text overflow-auto whitespace-pre p-2 font-mono text-[11px] leading-relaxed">
                  {out.result.text}
                </pre>
              </div>
            )}
          </>
        )}
      </div>
    </details>
  )
}
