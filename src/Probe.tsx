import { useCallback, useEffect, useRef, useState } from 'react'
import { api, errorText, type ExternalItem, type ProbeItem } from './api'

/** Esc 关闭 + 打开时把焦点移到对话框内，关闭时还给触发元素。 */
function useDialog(onClose: () => void) {
  const ref = useRef<HTMLDivElement | null>(null)
  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null
    ref.current?.focus()
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        onClose()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('keydown', onKey)
      prev?.focus?.()
    }
  }, [onClose])
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
    <div className="fixed inset-0 z-30 flex items-center justify-center bg-black/50 p-6">
      <div
        ref={dialogRef}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label="批量探测"
        className="flex max-h-[85vh] w-full max-w-3xl flex-col rounded-xl border border-border bg-surface p-5 focus:outline-none"
      >
        <div className="flex items-center gap-3">
          <h2 className="text-base font-semibold">批量探测</h2>
          <span className="text-xs text-muted">
            只读扫描：{selectable.length} 个项目可采集；勾选后写入<strong>本机</strong>，不动任何仓库
          </span>
          <button onClick={onClose} className="ml-auto rounded border border-border px-2 py-1 text-xs ">
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
                  <span className="font-semibold">{it.project_name}</span>
                  <span className="font-mono text-[10px] text-muted">{it.path}</span>
                  {it.has_descriptor && (
                    <span className="rounded border border-accent/40 bg-accent/10 px-1 text-[10px] text-fg">
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

/** 外部监督者（launchd / PM2）只读视图：只看、不接管。 */
export function ExternalPanel({ onClose }: { onClose: () => void }) {
  const dialogRef = useDialog(onClose)
  const [entries, setEntries] = useState<ExternalItem[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [filter, setFilter] = useState<'all' | 'pm2' | 'launchd'>('all')

  useEffect(() => {
    void (async () => {
      try {
        setEntries(await api.externalScan())
      } catch (e) {
        setError(errorText(e))
      }
    })()
  }, [])

  const shown = (entries ?? []).filter((e) => filter === 'all' || e.source === filter)

  return (
    <div className="fixed inset-0 z-30 flex items-center justify-center bg-black/50 p-6">
      <div
        ref={dialogRef}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label="外部服务（只读）"
        className="flex max-h-[85vh] w-full max-w-3xl flex-col rounded-xl border border-border bg-surface p-5 focus:outline-none"
      >
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-base font-semibold">外部服务（只读）</h2>
          <span className="text-xs text-muted">launchd 与 PM2 已在跑，这里只呈现事实，不接管</span>
          <button onClick={onClose} className="ml-auto rounded border border-border px-2 py-1 text-xs ">
            关闭
          </button>
        </div>

        <div className="mt-3 flex gap-1 text-xs">
          {(['all', 'launchd', 'pm2'] as const).map((f) => (
            <button
              key={f}
              onClick={() => setFilter(f)}
              className={`rounded border px-2 py-0.5 transition-colors ${
                filter === f
                  ? 'border-primary bg-primary text-primary-fg'
                  : 'border-border text-muted hover:text-fg'
              }`}
            >
              {f === 'all' ? '全部' : f}
            </button>
          ))}
        </div>

        {error && (
          <div className="mt-3 rounded border border-error/50 bg-error/10 px-2 py-1.5 text-xs text-error ">
            {error}
          </div>
        )}

        <div className="mt-3 min-h-0 flex-1 overflow-auto">
          {!entries && <div className="p-4 text-sm text-muted">读取中…</div>}
          {entries && shown.length === 0 && (
            <div className="p-4 text-sm text-muted">没有找到外部条目。</div>
          )}
          {shown.length > 0 && (
            <table className="w-full table-fixed text-left text-xs">
              <thead className="sticky top-0 bg-surface text-muted ">
                <tr>
                  <th className="w-16 py-1 pr-2">来源</th>
                  <th className="w-[30%] py-1 pr-2">名称</th>
                  <th className="w-[26%] py-1 pr-2">目录</th>
                  <th className="w-[24%] py-1 pr-2">说明</th>
                  <th className="w-20 py-1 pr-2">状态</th>
                  <th className="w-16 py-1"></th>
                </tr>
              </thead>
              <tbody>
                {shown.map((e, i) => (
                  <tr key={`${e.source}-${e.name}-${i}`} className="border-t border-border ">
                    <td className="truncate py-1 pr-2 font-mono text-[10px]">{e.source}</td>
                    <td className="truncate py-1 pr-2" title={e.name}>
                      {e.name}
                    </td>
                    <td className="truncate py-1 pr-2 font-mono text-[10px] text-muted" title={e.cwd ?? ''}>
                      {e.cwd ?? '—'}
                    </td>
                    <td className="truncate py-1 pr-2 text-muted" title={e.note}>
                      {e.note}
                    </td>
                    <td className="py-1 pr-2">
                      {e.managed_project ? (
                        <span className={e.declared ? 'text-success ' : 'text-warning '}>
                          {e.declared ? '已声明' : '已管理项目'}
                        </span>
                      ) : (
                        <span className="text-muted">无关</span>
                      )}
                    </td>
                    <td className="py-1">
                      <button
                        onClick={() => void api.revealFile(e.config_path)}
                        className="rounded border border-border px-1.5 py-0.5 text-[10px] "
                      >
                        显示配置
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
    </div>
  )
}
