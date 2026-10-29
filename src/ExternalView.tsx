import { useEffect, useState } from 'react'
import { api, errorText, type ExternalItem } from './api'
import { Segmented } from './components'
import { IconExternal } from './icons'

type Filter = 'all' | 'launchd' | 'pm2'

/**
 * 外部服务页（只读）：launchd 与 PM2 已经在跑的东西。
 *
 * 只呈现事实，不接管：这里没有任何启停按钮，唯一的写操作是「显示配置」
 * （在 Finder 里选中配置文件）。
 */
export default function ExternalView() {
  const [entries, setEntries] = useState<ExternalItem[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [filter, setFilter] = useState<Filter>('all')

  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const list = await api.externalScan()
        if (!cancelled) setEntries(list)
      } catch (e) {
        if (!cancelled) setError(errorText(e))
      }
    })()
    return () => {
      cancelled = true
    }
  }, [])

  const shown = (entries ?? []).filter((e) => filter === 'all' || e.source === filter)

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex h-[52px] shrink-0 items-center gap-3 border-b border-border px-4">
        <h1 data-tauri-drag-region className="text-[17px] font-semibold">
          外部服务
        </h1>
        <span data-tauri-drag-region className="text-[12px] text-muted">
          launchd 与 PM2 已在跑，这里只呈现事实，不接管
        </span>
        <div data-tauri-drag-region className="h-full flex-1" />
        <div className="shrink-0">
          <Segmented<Filter>
            value={filter}
            onChange={setFilter}
            items={[
              { id: 'all', label: '全部' },
              { id: 'launchd', label: 'launchd' },
              { id: 'pm2', label: 'pm2' },
            ]}
          />
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto p-4">
        {error && (
          <div className="mb-3 rounded border border-error/50 bg-error/10 px-2 py-1.5 text-xs text-error">
            {error}
          </div>
        )}

        {!entries && <div className="p-4 text-sm text-muted">读取中…</div>}
        {entries && shown.length === 0 && (
          <div className="flex flex-col items-center gap-2 rounded-[var(--radius-card)] border border-dashed border-border p-8 text-center text-sm text-muted">
            <IconExternal size={20} />
            <span>没有找到外部条目。</span>
          </div>
        )}

        {shown.length > 0 && (
          <table className="w-full table-fixed text-left text-xs">
            <thead className="sticky top-0 bg-surface text-muted">
              <tr>
                <th className="w-16 py-1 pr-2">来源</th>
                <th className="w-[30%] py-1 pr-2">名称</th>
                <th className="w-[26%] py-1 pr-2">目录</th>
                <th className="w-[24%] py-1 pr-2">说明</th>
                <th className="w-20 py-1 pr-2">状态</th>
                <th className="w-16 py-1" />
              </tr>
            </thead>
            <tbody>
              {shown.map((e, i) => (
                <tr key={`${e.source}-${e.name}-${i}`} className="border-t border-border">
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
                      <span className={e.declared ? 'text-success' : 'text-warning'}>
                        {e.declared ? '已声明' : '已管理项目'}
                      </span>
                    ) : (
                      <span className="text-muted">无关</span>
                    )}
                  </td>
                  <td className="py-1">
                    <button
                      type="button"
                      onClick={() => void api.revealFile(e.config_path)}
                      className="rounded border border-border px-1.5 py-0.5 text-[10px]"
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
  )
}
