import { useCallback, useEffect, useRef, useState } from 'react'
import { api, errorText, type ProbeItem } from './api'

/** Esc 关闭 + 打开时把焦点移到对话框内，关闭时还给触发元素。 */
function useDialog(onClose: () => void) {
  const ref = useRef<HTMLDivElement | null>(null)
  // onClose 是内联闭包（每次渲染引用都变）：直接进 deps 会让 effect 重跑、
  // 开着对话框时焦点被反复抢走。用 ref 存最新回调，effect 只跑一次。
  const onCloseRef = useRef(onClose)
  onCloseRef.current = onClose
  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null
    ref.current?.focus()
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        onCloseRef.current()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('keydown', onKey)
      prev?.focus?.()
    }
  }, [])
  return ref
}

/** 批量探测：一次看清所有项目能怎么跑，勾选后**只写本机层**。 */
export default function ProbePanel({ onClose, onDone }: { onClose: () => void; onDone: () => Promise<void> | void }) {
  const [items, setItems] = useState<ProbeItem[] | null>(null)
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [report, setReport] = useState<string[] | null>(null)
  const dialogRef = useDialog(onClose)

  const load = useCallback(async () => {
    setBusy(true)
    try {
      setItems(await api.probeAll())
      setError(null)
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const toggle = (key: string) =>
    setPicked((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })

  const adopt = async () => {
    if (!items) return
    setBusy(true)
    try {
      const payload: [number, ProbeItem['candidates'][number]['spec']][] = []
      for (const it of items) {
        it.candidates.forEach((c, i) => {
          if (picked.has(`${it.project_id}:${i}`)) payload.push([it.project_id, c.spec])
        })
      }
      const saved = await api.adoptCandidates(payload)
      setReport(saved.map((n) => `已保存：${n}`))
      setError(null)
      setPicked(new Set())
      await load()
      await onDone()
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const selectable = (items ?? []).filter((i) => !i.has_descriptor)

  return (
    <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6">
      <div
        ref={dialogRef}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label="批量探测"
        className="flex max-h-[85vh] w-full max-w-3xl flex-col rounded-xl border border-border bg-surface p-5"
      >
        <div className="flex items-center gap-3">
          <h2 className="text-base font-semibold">批量探测</h2>
          <span className="text-xs text-muted">
            只读扫描：{selectable.length} 个项目可采集；勾选后写入<strong>本机</strong>，不动任何仓库
          </span>
          <button type="button" onClick={onClose} className="ml-auto rounded border border-border px-2 py-1 text-xs ">
            关闭
          </button>
        </div>

        {error && (
          <div className="mt-3 rounded border border-error/50 bg-error/10 px-2 py-1.5 text-xs text-error ">
            {error}
          </div>
        )}
        {report && report.length > 0 && (
          <ul className="mt-3 max-h-24 overflow-auto rounded border border-success/50 bg-success/10 px-2 py-1.5 text-xs text-success ">
            {report.map((r) => (
              <li key={r}>{r}</li>
            ))}
          </ul>
        )}

        <div className="mt-3 min-h-0 flex-1 overflow-auto">
          {busy && !items && <div className="p-4 text-sm text-muted">扫描中…</div>}
          {items && items.length === 0 && (
            <div className="p-4 text-sm text-muted">还没有项目。</div>
          )}
          <ul className="space-y-2">
            {(items ?? []).map((it) => (
              <li key={it.project_id} className="rounded border border-border p-2 ">
                <div className="flex flex-wrap items-center gap-2 text-xs">
                  <span className="shrink-0 font-semibold">{it.project_name}</span>
                  <span className="min-w-0 flex-1 truncate font-mono text-[10px] text-muted" title={it.path}>{it.path}</span>
                  {it.has_descriptor && (
                    <span className="rounded border border-success/40 bg-success/10 px-1 text-[10px] text-fg">
                      已有 .json
                    </span>
                  )}
                  {it.has_services && !it.has_descriptor && (
                    <span
                      className="rounded bg-surface-raised px-1 text-[10px] text-muted "
                      title="已有服务；采纳同名会覆盖那一条"
                    >
                      已有服务
                    </span>
                  )}
                </div>
                {it.candidates.length === 0 ? (
                  <div className="mt-1 text-[11px] text-muted">没有可自动采纳的候选</div>
                ) : (
                  <ul className="mt-1 space-y-1">
                    {it.candidates.map((c, i) => {
                      const key = `${it.project_id}:${i}`
                      // 有描述文件的项目由文件说了算，这里不允许写入（后端也会拒绝）
                      const disabled = it.has_descriptor
                      return (
                        <li key={key} className="flex items-center gap-2">
                          <input
                            type="checkbox"
                            checked={picked.has(key)}
                            disabled={disabled}
                            onChange={() => toggle(key)}
                            aria-label={`采纳 ${it.project_name} 的 ${c.label}`}
                          />
                          <span className="rounded bg-surface-raised px-1 font-mono text-[10px] ">
                            {c.kind}
                          </span>
                          <code className="font-mono text-[11px]">{c.label}</code>
                          <span className="text-[10px] text-muted">{c.spec.name}</span>
                        </li>
                      )
                    })}
                  </ul>
                )}
              </li>
            ))}
          </ul>
        </div>

        <div className="mt-3 flex items-center gap-2">
          <span className="text-xs text-muted">已选 {picked.size} 项</span>
          <button
            type="button"
            onClick={() => void adopt()}
            disabled={busy || picked.size === 0}
            className="ml-auto rounded bg-primary px-3 py-1.5 text-sm text-primary-fg disabled:opacity-40"
          >
            采纳到本机
          </button>
        </div>
      </div>
    </div>
  )
}
