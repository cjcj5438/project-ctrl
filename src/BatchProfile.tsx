import { useEffect, useMemo, useRef, useState } from 'react'
import { api, errorText, type BatchItem, type ProjectInfo } from './api'
import { Badge, Button, Spinner } from './components'
import { IconCheck, IconSparkles, IconTrash, IconX } from './icons'
import { subscribe } from './util'

const STATE_TONE: Record<string, 'neutral' | 'success' | 'warning' | 'error'> = {
  queued: 'neutral',
  running: 'warning',
  done: 'success',
  failed: 'error',
  adopted: 'success',
  dismissed: 'neutral',
}

const STATE_TEXT: Record<string, string> = {
  queued: '排队中',
  running: '生成中',
  done: '已完成',
  failed: '失败',
  adopted: '已采纳',
  dismissed: '已忽略',
}

/** 正在跑（决定是否显示取消按钮与禁用重复提交）。 */
const busyState = (st: string) => st === 'queued' || st === 'running'

/**
 * 「AI 批量画像」浮层：选项目 → `ai_batch_profile` → 订阅 `ai:batch`。
 *
 * `ai:batch` 的载荷是**完整**数组（不是增量），每次直接替换整张表。
 */
export default function BatchProfile({
  projects,
  onClose,
  onAdopted,
}: {
  projects: ProjectInfo[]
  onClose: () => void
  onAdopted: () => Promise<void> | void
}) {
  const candidates = useMemo(() => projects.filter((p) => !p.missing), [projects])
  const [checked, setChecked] = useState<Set<number>>(
    () => new Set(candidates.filter((p) => p.summary === null).map((p) => p.id)),
  )
  /** 已见过默认勾选判定的项目：列表晚到时只补新出现的，不动用户的手动勾选。 */
  const seen = useRef<Set<number> | null>(null)
  const [rows, setRows] = useState<BatchItem[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [starting, setStarting] = useState(false)
  const aliveRef = useRef(true)

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  // 项目列表可能晚于浮层到达：新出现的「无画像」项目补进默认勾选
  useEffect(() => {
    if (seen.current === null) {
      seen.current = new Set(candidates.map((p) => p.id))
      return
    }
    const known = seen.current
    const fresh = candidates.filter((p) => !known.has(p.id))
    fresh.forEach((p) => known.add(p.id))
    const pick = fresh.filter((p) => p.summary === null).map((p) => p.id)
    if (pick.length === 0) return
    setChecked((s) => new Set([...s, ...pick]))
  }, [candidates])

  // 打开时先取一次现存任务：上次没跑完的批量不该看不见
  useEffect(() => {
    void (async () => {
      try {
        const list = await api.aiBatchStatus()
        if (aliveRef.current && list.length > 0) setRows(list)
      } catch {
        // 没有任务 / 读取失败都不该拦住用户重新发起
      }
    })()
  }, [])

  // 订阅在前、补拉在后：onReady 里补一次全量，杀订阅缝隙
  useEffect(() => {
    const stop = subscribe<BatchItem[]>(
      'ai:batch',
      (e) => {
        if (aliveRef.current) setRows(e.payload)
      },
      () => {
        void api
          .aiBatchStatus()
          .then((list) => {
            if (aliveRef.current && list.length > 0) setRows(list)
          })
          .catch(() => {})
      },
    )
    return stop
  }, [])

  const active = rows?.some((r) => busyState(r.state)) ?? false

  const start = async () => {
    setStarting(true)
    try {
      await api.aiBatchProfile([...checked])
      const list = await api.aiBatchStatus()
      if (!aliveRef.current) return
      setRows(list)
      setError(null)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    } finally {
      if (aliveRef.current) setStarting(false)
    }
  }

  const adopt = async (r: BatchItem) => {
    if (!r.profile) return
    try {
      await api.aiBatchAdopt(r.project_id, true, true, r.profile.tags)
      await onAdopted()
      setError(null)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    }
  }

  const dismiss = async (r: BatchItem) => {
    try {
      await api.aiBatchDismiss(r.project_id)
      setError(null)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    }
  }

  const adoptAll = async () => {
    const done = (rows ?? []).filter((r) => r.state === 'done' && r.profile !== null)
    if (done.length === 0) return
    setStarting(true)
    try {
      for (const r of done) await api.aiBatchAdopt(r.project_id, true, true, [])
      await onAdopted()
      setError(null)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    } finally {
      if (aliveRef.current) setStarting(false)
    }
  }

  const cancel = async () => {
    try {
      await api.aiBatchCancel()
      setError(null)
    } catch (e) {
      if (aliveRef.current) setError(errorText(e))
    }
  }

  const toggle = (id: number) =>
    setChecked((s) => {
      const next = new Set(s)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })

  return (
    <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6">
      <div
        className="flex max-h-[85vh] w-full max-w-2xl flex-col overflow-hidden rounded-[var(--radius-panel)] border border-border-strong bg-surface shadow-overlay"
        role="dialog"
        aria-modal="true"
        aria-label="AI 批量画像"
      >
        <div className="flex items-center gap-2 border-b border-border px-4 py-2.5">
          <IconSparkles size={14} className="text-ai" />
          <h2 className="text-[13px] font-semibold">AI 批量画像</h2>
          {active && <Spinner size={12} />}
          <button
            type="button"
            onClick={onClose}
            className="ml-auto rounded-[var(--radius-control)] px-2 py-0.5 text-[12px] text-muted hover:bg-surface-raised hover:text-fg"
            aria-label="关闭"
          >
            关闭
          </button>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-4">
          {error && (
            <div className="mb-3 rounded-[var(--radius-card)] border border-error/40 bg-error/10 px-3 py-2 text-[12px] text-error">
              <span className="break-words">{error}</span>
            </div>
          )}

          {rows === null ? (
            <>
              <div className="flex items-center gap-2">
                <span className="text-[12px] text-muted">
                  给项目生成「简介 / 技术栈 / 标签」；默认勾选还没有画像的项目。
                </span>
                <div className="ml-auto flex gap-1.5">
                  <Button size="sm" variant="ghost" onClick={() => setChecked(new Set(candidates.map((p) => p.id)))}>
                    全选
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setChecked(new Set())}>
                    全不选
                  </Button>
                </div>
              </div>
              {candidates.length === 0 ? (
                <div className="mt-4 rounded-[var(--radius-card)] border border-dashed border-border p-6 text-center text-[13px] text-muted">
                  没有可画像的项目（路径丢失的项目已排除）。
                </div>
              ) : (
                <div className="mt-2 max-h-[45vh] overflow-y-auto rounded-[var(--radius-card)] border border-border">
                  {candidates.map((p) => (
                    <label
                      key={p.id}
                      className="flex cursor-pointer items-center gap-2 border-b border-border px-3 py-1.5 text-[12px] last:border-b-0 hover:bg-surface-raised"
                    >
                      <input
                        type="checkbox"
                        className="accent-[var(--primary)]"
                        checked={checked.has(p.id)}
                        onChange={() => toggle(p.id)}
                      />
                      <span className="min-w-0 flex-1 truncate">{p.name}</span>
                      {p.summary !== null ? (
                        <Badge tone="neutral">已有画像</Badge>
                      ) : (
                        <Badge tone="warning">无画像</Badge>
                      )}
                    </label>
                  ))}
                </div>
              )}
            </>
          ) : (
            <div className="flex flex-col gap-2">
              {rows.length === 0 ? (
                <div className="rounded-[var(--radius-card)] border border-dashed border-border p-6 text-center text-[13px] text-muted">
                  这次批量任务没有条目。
                </div>
              ) : (
                rows.map((r) => (
                  <div key={r.project_id} className="rounded-[var(--radius-card)] border border-border p-2.5">
                    <div className="flex items-center gap-2">
                      <span className="min-w-0 flex-1 truncate text-[12px] font-medium">{r.name}</span>
                      <Badge tone={STATE_TONE[r.state] ?? 'neutral'}>{STATE_TEXT[r.state] ?? r.state}</Badge>
                    </div>
                    {r.error && <div className="mt-1 break-words text-[11px] text-error">{r.error}</div>}
                    {r.profile && (
                      <div className="mt-1 text-[11px] text-muted">
                        <div className="break-words text-fg">{r.profile.summary}</div>
                        {r.profile.stack.length > 0 && (
                          <div className="mt-0.5 font-mono">{r.profile.stack.join(' · ')}</div>
                        )}
                        {r.profile.tags.length > 0 && <div className="mt-0.5">标签：{r.profile.tags.join('、')}</div>}
                      </div>
                    )}
                    <div className="mt-1.5 flex gap-1.5">
                      <Button
                        size="sm"
                        variant="primary"
                        disabled={r.state !== 'done' || r.profile === null}
                        onClick={() => void adopt(r)}
                      >
                        <IconCheck size={11} />
                        采纳
                      </Button>
                      <Button
                        size="sm"
                        disabled={r.state === 'dismissed' || r.state === 'adopted'}
                        onClick={() => void dismiss(r)}
                      >
                        <IconX size={11} />
                        忽略
                      </Button>
                    </div>
                  </div>
                ))
              )}
            </div>
          )}
        </div>

        <div className="flex items-center gap-2 border-t border-border px-4 py-2.5">
          {rows === null ? (
            <Button
              variant="primary"
              disabled={starting || checked.size === 0}
              onClick={() => void start()}
            >
              {starting ? <Spinner size={11} /> : <IconSparkles size={12} />}
              开始（{checked.size}）
            </Button>
          ) : (
            <>
              <Button
                variant="primary"
                disabled={starting || !(rows.some((r) => r.state === 'done'))}
                onClick={() => void adoptAll()}
              >
                全部采纳
              </Button>
              <Button variant="danger" disabled={!active} onClick={() => void cancel()}>
                <IconTrash size={12} />
                取消任务
              </Button>
              {!active && (
                // 上一批已结束：后端允许重新组批，这里给回选择界面
                <Button onClick={() => setRows(null)}>新一批</Button>
              )}
            </>
          )}
          <Button className="ml-auto" onClick={onClose}>
            关闭
          </Button>
        </div>
      </div>
    </div>
  )
}
