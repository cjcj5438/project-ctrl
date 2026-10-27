import { useEffect, useMemo, useRef, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import {
  api,
  type ImportReport,
  type ProjectInfo,
  type ScanCandidate,
  type ServiceStatus,
} from './api'
import DetailView from './Detail'
import UpdateBanner from './Update'
import { humanSize } from './format'
import { pickDirectory, splitTags } from './util'

type SortMode = 'created' | 'name' | 'size'

const NEXT_SORT: Record<SortMode, SortMode> = { created: 'name', name: 'size', size: 'created' }
const SORT_LABEL: Record<SortMode, string> = { created: '创建时间倒序', name: '名称', size: '大小' }

/** 单个项目大小的并发计算上限。 */
const SIZE_CONCURRENCY = 3

/** 大小缓存的 key：项目 id + 路径（路径变化即视为需要重算）。 */
const sizeKey = (p: ProjectInfo): string => `${p.id}\u0000${p.path}`

type Modal =
  | { kind: 'editTags'; project: ProjectInfo }
  | { kind: 'renameTag'; tag: string }
  | { kind: 'deleteTag'; tag: string }
  | { kind: 'singleTags'; path: string }
  | { kind: 'removeProject'; project: ProjectInfo }

export default function App() {
  const [projects, setProjects] = useState<ProjectInfo[]>([])
  const [allTags, setAllTags] = useState<string[]>([])
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState<string[]>([])
  const [sort, setSort] = useState<SortMode>('created')
  const [error, setError] = useState<string | null>(null)
  const [modal, setModal] = useState<Modal | null>(null)
  const [modalInput, setModalInput] = useState('')
  const [detailId, setDetailId] = useState<number | null>(null)

  // 有服务处于运行中/过渡态的项目 id（来自 svc:state 事件，不轮询）
  const [running, setRunning] = useState<Set<number>>(new Set())

  // 克隆 / PM2 导入的独立浮层状态（不塞进既有 Modal 联合类型）
  const [cloneOpen, setCloneOpen] = useState(false)
  const [cloneUrl, setCloneUrl] = useState('')
  const [cloneDir, setCloneDir] = useState<string | null>(null)
  const [cloneBusy, setCloneBusy] = useState(false)
  const [pm2Report, setPm2Report] = useState<ImportReport | null>(null)

  // 项目大小：仅前端内存缓存（key = id + 路径），会话内不重复计算；null 表示计算失败
  const [sizes, setSizes] = useState<Record<string, number | null>>({})
  const sizeQueued = useRef<Set<string>>(new Set())
  const sizeQueue = useRef<string[]>([])
  const sizeActive = useRef(0)

  const sizeOf = (p: ProjectInfo): number | null | undefined => sizes[sizeKey(p)]

  const formatSize = (p: ProjectInfo): string => {
    const s = sizeOf(p)
    return s === undefined ? '计算中…' : s === null ? '—' : humanSize(s)
  }

  // 简单队列：并发上限 SIZE_CONCURRENCY
  const pump = () => {
    while (sizeActive.current < SIZE_CONCURRENCY && sizeQueue.current.length > 0) {
      const key = sizeQueue.current.shift() as string
      const id = Number(key.split('\u0000')[0])
      sizeActive.current += 1
      api
        .projectSize(id)
        .then((n) => setSizes((s) => ({ ...s, [key]: n })))
        .catch(() => setSizes((s) => ({ ...s, [key]: null })))
        .finally(() => {
          sizeActive.current -= 1
          pump()
        })
    }
  }

  // 扫描弹层
  const [scanRoot, setScanRoot] = useState<string | null>(null)
  const [candidates, setCandidates] = useState<ScanCandidate[]>([])
  const [checked, setChecked] = useState<Set<string>>(new Set())
  const [batchTags, setBatchTags] = useState('')

  const refresh = async () => {
    try {
      const [ps, ts] = await Promise.all([api.listProjects(), api.listTags()])
      setProjects(ps)
      setAllTags(ts)
      setError(null)
    } catch (e) {
      setError(String(e))
    }
  }

  useEffect(() => {
    refresh()
  }, [])

  // 运行状态：初始化一次 + 订阅 svc:state（不轮询）
  useEffect(() => {
    let alive = true
    let un: UnlistenFn | null = null
    const apply = (list: ServiceStatus[]) => {
      if (!alive) return
      setRunning(
        new Set(
          list
            .filter((s) => s.phase === 'Running' || s.phase === 'Building' || s.phase === 'Installing')
            .map((s) => s.project_id),
        ),
      )
    }
    void (async () => {
      try {
        apply(await api.serviceStatus())
      } catch {
        /* 后端不可用时保持空集合 */
      }
      un = await listen<ServiceStatus>('svc:state', (e) => {
        if (!alive) return
        const id = e.payload.project_id
        const active =
          e.payload.phase === 'Running' ||
          e.payload.phase === 'Building' ||
          e.payload.phase === 'Installing'
        setRunning((prev) => {
          const next = new Set(prev)
          if (active) next.add(id)
          else next.delete(id)
          return next
        })
      })
    })()
    return () => {
      alive = false
      if (un) un()
    }
  }, [])

  const filterTags = useMemo(() => {
    const auto = new Set<string>()
    projects.forEach((p) => p.auto_tags.forEach((t) => auto.add(t)))
    return [...new Set([...allTags, ...auto])].sort((a, b) => a.localeCompare(b, 'zh-CN'))
  }, [allTags, projects])

  const visible = useMemo(() => {
    const q = query.trim().toLowerCase()
    const list = projects.filter((p) => {
      if (q && !p.name.toLowerCase().includes(q)) return false
      const tags = new Set([...p.manual_tags, ...p.auto_tags])
      return selected.every((t) => tags.has(t))
    })
    return [...list].sort((a, b) => {
      if (sort === 'name') return a.name.localeCompare(b.name, 'zh-CN')
      if (sort === 'size') {
        const sa = typeof sizes[sizeKey(a)] === 'number' ? (sizes[sizeKey(a)] as number) : -1
        const sb = typeof sizes[sizeKey(b)] === 'number' ? (sizes[sizeKey(b)] as number) : -1
        return sb - sa // 未算完的（-1）排在最后
      }
      return (b.created_date ?? '').localeCompare(a.created_date ?? '')
    })
  }, [projects, query, selected, sort, sizes])

  // 未算过的、非丢失的项目排队后台计算（并发上限 3，结果只进内存缓存）
  useEffect(() => {
    for (const p of projects) {
      if (p.missing) continue
      const key = sizeKey(p)
      if (sizeQueued.current.has(key)) continue
      sizeQueued.current.add(key)
      sizeQueue.current.push(key)
    }
    pump()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projects])

  const run = async (fn: () => Promise<void>) => {
    try {
      await fn()
    } catch (e) {
      setError(String(e))
    }
  }

  const doScan = async () => {
    const dir = await pickDirectory()
    if (!dir) return
    await run(async () => {
      const cs = await api.scanDirectory(dir)
      setScanRoot(dir)
      setCandidates(cs)
      setChecked(new Set(cs.map((c) => c.path)))
      setBatchTags('')
    })
  }

  const doAddSingle = async () => {
    const dir = await pickDirectory()
    if (!dir) return
    setModalInput('')
    setModal({ kind: 'singleTags', path: dir })
  }

  const doJoin = async () => {
    const paths = [...checked]
    if (paths.length === 0) return
    await run(async () => {
      await api.addProjects(paths, splitTags(batchTags))
      setScanRoot(null)
      setCandidates([])
      await refresh()
    })
  }

  const confirmModal = async () => {
    if (!modal) return
    await run(async () => {
      switch (modal.kind) {
        case 'editTags':
          await api.setProjectTags(modal.project.id, splitTags(modalInput))
          break
        case 'renameTag':
          if (!modalInput.trim() || modalInput.trim() === modal.tag) return
          await api.renameTag(modal.tag, modalInput.trim())
          setSelected((s) => s.map((x) => (x === modal.tag ? modalInput.trim() : x)))
          break
        case 'deleteTag':
          await api.deleteTag(modal.tag)
          setSelected((s) => s.filter((x) => x !== modal.tag))
          break
        case 'singleTags':
          await api.addProjects([modal.path], splitTags(modalInput))
          break
        case 'removeProject':
          await api.removeProject(modal.project.id)
          break
      }
      setModal(null)
      await refresh()
    })
  }

  const relink = async (p: ProjectInfo) => {
    const dir = await pickDirectory()
    if (!dir) return
    await run(async () => {
      await api.relinkProject(p.id, dir)
      await refresh()
    })
  }

  const allChecked = candidates.length > 0 && checked.size === candidates.length

  const detailProject = detailId === null ? null : (projects.find((p) => p.id === detailId) ?? null)
  if (detailProject) {
    return (
      <>
        <UpdateBanner />
        <DetailView
          project={detailProject}
          size={sizeOf(detailProject)}
          onBack={() => setDetailId(null)}
          onChanged={refresh}
          onRemoved={() => {
            setDetailId(null)
            void refresh()
          }}
        />
      </>
    )
  }

  return (
    <div className="min-h-screen bg-neutral-100 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      <UpdateBanner />
      <div className="mx-auto max-w-6xl px-6 py-6">
        <header className="flex flex-wrap items-center gap-3">
          <h1 className="text-xl font-bold">项目管理</h1>
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="按名称搜索…"
            className="ml-4 w-56 rounded-lg border border-neutral-300 bg-white px-3 py-1.5 text-sm outline-none focus:border-blue-500 dark:border-neutral-700 dark:bg-neutral-900"
          />
          <div className="ml-auto flex items-center gap-2">
            <button
              onClick={() => setSort(NEXT_SORT[sort])}
              className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
              title="切换排序"
            >
              排序：{SORT_LABEL[sort]}
            </button>
            <button
              onClick={doScan}
              className="rounded-lg bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-700"
            >
              扫描文件夹…
            </button>
            <button
              onClick={doAddSingle}
              className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
            >
              添加单个项目…
            </button>
            <button
              onClick={() => setCloneOpen(true)}
              className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
            >
              从 GitHub 克隆…
            </button>
            <button
              onClick={() => void run(async () => setPm2Report(await api.importPm2()))}
              className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
            >
              导入 PM2 配置
            </button>
          </div>
        </header>

        {error && (
          <div className="mt-4 rounded-lg border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-200">
            {error}
          </div>
        )}

        <div className="mt-4 flex flex-wrap items-center gap-2">
          <span className="text-sm text-neutral-500">筛选：</span>
          {filterTags.length === 0 && <span className="text-sm text-neutral-400">暂无 tag</span>}
          {filterTags.map((t) => {
            const active = selected.includes(t)
            const manual = allTags.includes(t)
            return (
              <span
                key={t}
                className={`inline-flex items-center gap-1 rounded-full border px-2.5 py-1 text-xs ${
                  active
                    ? 'border-blue-600 bg-blue-600 text-white'
                    : manual
                      ? 'border-neutral-300 bg-white dark:border-neutral-700 dark:bg-neutral-900'
                      : 'border-dashed border-neutral-400 bg-neutral-200 dark:border-neutral-600 dark:bg-neutral-800'
                }`}
              >
                <button
                  onClick={() =>
                    setSelected((s) => (s.includes(t) ? s.filter((x) => x !== t) : [...s, t]))
                  }
                  title={manual ? '手动 tag' : '自动 tag'}
                >
                  {t}
                </button>
                {manual && (
                  <>
                    <button
                      onClick={() => {
                        setModalInput(t)
                        setModal({ kind: 'renameTag', tag: t })
                      }}
                      className="opacity-50 hover:opacity-100"
                      title="改名"
                    >
                      ✎
                    </button>
                    <button
                      onClick={() => setModal({ kind: 'deleteTag', tag: t })}
                      className="opacity-50 hover:opacity-100"
                      title="删除"
                    >
                      ✕
                    </button>
                  </>
                )}
              </span>
            )
          })}
          {selected.length > 0 && (
            <button onClick={() => setSelected([])} className="text-xs text-blue-600">
              清除筛选
            </button>
          )}
        </div>

        <div className="mt-4 text-sm text-neutral-500">共 {visible.length} 个项目</div>

        <div className="mt-3 grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {visible.map((p) => (
            <div
              key={p.id}
              onClick={() => setDetailId(p.id)}
              className="cursor-pointer rounded-xl border border-neutral-200 bg-white p-4 shadow-sm transition hover:border-blue-400 dark:border-neutral-800 dark:bg-neutral-900 dark:hover:border-blue-700"
            >
              <div className="flex items-center gap-2">
                <h2 className="truncate font-semibold">{p.name}</h2>
                {running.has(p.id) && (
                  <span
                    className="shrink-0 inline-block h-2 w-2 rounded-full bg-emerald-500"
                    title="服务运行中"
                    aria-hidden
                  />
                )}
                {p.missing && (
                  <span className="shrink-0 rounded bg-red-100 px-1.5 py-0.5 text-xs text-red-700 dark:bg-red-900 dark:text-red-200">
                    路径丢失
                  </span>
                )}
              </div>
              <div className="mt-1 truncate text-xs text-neutral-500" title={p.path}>
                {p.path}
              </div>
              <div className="mt-1 text-xs text-neutral-500">创建日期：{p.created_date ?? '未知'}</div>
              {!p.missing && (
                <div className="mt-1 text-xs text-neutral-500">大小：{formatSize(p)}</div>
              )}
              <div className="mt-2 flex flex-wrap gap-1.5">
                {p.manual_tags.map((t) => (
                  <span
                    key={t}
                    className="rounded-full bg-blue-100 px-2 py-0.5 text-xs text-blue-800 dark:bg-blue-900 dark:text-blue-100"
                  >
                    {t}
                  </span>
                ))}
                {p.auto_tags.map((t) => (
                  <span
                    key={t}
                    className="rounded-full border border-dashed border-neutral-400 px-2 py-0.5 text-xs text-neutral-500 dark:border-neutral-600 dark:text-neutral-300"
                  >
                    {t}
                  </span>
                ))}
                {p.manual_tags.length === 0 && p.auto_tags.length === 0 && (
                  <span className="text-xs text-neutral-400">无 tag</span>
                )}
              </div>
              <div className="mt-3 flex flex-wrap gap-2 text-xs" onClick={(e) => e.stopPropagation()}>
                {p.missing ? (
                  <>
                    <button
                      onClick={() => relink(p)}
                      className="rounded bg-blue-600 px-2 py-1 text-white"
                    >
                      重新关联
                    </button>
                    <button
                      onClick={() => setModal({ kind: 'removeProject', project: p })}
                      className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
                    >
                      移除
                    </button>
                  </>
                ) : (
                  <>
                    <button
                      onClick={() => {
                        setModalInput(p.manual_tags.join('，'))
                        setModal({ kind: 'editTags', project: p })
                      }}
                      className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
                    >
                      编辑 tag
                    </button>
                    <button
                      onClick={() => run(() => api.openInFinder(p.id))}
                      className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
                    >
                      Finder
                    </button>
                    <button
                      onClick={() => run(() => api.openInIterm(p.id))}
                      className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
                    >
                      iTerm
                    </button>
                    <button
                      onClick={() => run(() => api.openInVscode(p.id))}
                      className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
                    >
                      VS Code
                    </button>
                    {p.github_url && (
                      <button
                        onClick={() => run(() => api.openGithub(p.id))}
                        className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
                      >
                        GitHub
                      </button>
                    )}
                  </>
                )}
              </div>
            </div>
          ))}
        </div>

        {scanRoot && (
          <div className="fixed inset-0 flex items-center justify-center bg-black/40 p-6">
            <div className="max-h-[80vh] w-full max-w-lg overflow-auto rounded-xl bg-white p-5 dark:bg-neutral-900">
              <h3 className="font-semibold">扫描结果：{scanRoot}</h3>
              <p className="mt-1 text-xs text-neutral-500">
                已加入管理的不显示；没勾选的不记忆，下次还会出现。
              </p>
              <label className="mt-3 flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={allChecked}
                  onChange={() =>
                    setChecked(allChecked ? new Set() : new Set(candidates.map((c) => c.path)))
                  }
                />
                全选（{checked.size}/{candidates.length}）
              </label>
              <div className="mt-2 space-y-1.5">
                {candidates.map((c) => (
                  <label key={c.path} className="flex items-center gap-2 text-sm">
                    <input
                      type="checkbox"
                      checked={checked.has(c.path)}
                      onChange={() =>
                        setChecked((s) => {
                          const n = new Set(s)
                          if (n.has(c.path)) n.delete(c.path)
                          else n.add(c.path)
                          return n
                        })
                      }
                    />
                    <span className="font-medium">{c.name}</span>
                    <span className="truncate text-xs text-neutral-400">{c.path}</span>
                  </label>
                ))}
                {candidates.length === 0 && (
                  <div className="py-4 text-sm text-neutral-500">没有可加入的新目录。</div>
                )}
              </div>
              <input
                value={batchTags}
                onChange={(e) => setBatchTags(e.target.value)}
                placeholder="给所选项目批量打 tag（逗号/空格分隔，可留空）"
                className="mt-4 w-full rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700 dark:bg-neutral-800"
              />
              <div className="mt-4 flex justify-end gap-2">
                <button
                  onClick={() => setScanRoot(null)}
                  className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
                >
                  取消
                </button>
                <button
                  onClick={doJoin}
                  disabled={checked.size === 0}
                  className="rounded-lg bg-blue-600 px-3 py-1.5 text-sm text-white disabled:opacity-40"
                >
                  加入（{checked.size}）
                </button>
              </div>
            </div>
          </div>
        )}

        {modal && (
          <div className="fixed inset-0 flex items-center justify-center bg-black/40 p-6">
            <div className="w-full max-w-sm rounded-xl bg-white p-5 dark:bg-neutral-900">
              {modal.kind === 'deleteTag' ? (
                <>
                  <h3 className="font-semibold">删除 tag</h3>
                  <p className="mt-2 text-sm">
                    删除 tag「{modal.tag}」？会从所有项目摘掉。
                  </p>
                </>
              ) : modal.kind === 'removeProject' ? (
                <>
                  <h3 className="font-semibold">移除项目</h3>
                  <p className="mt-2 text-sm">
                    从管理器移除「{modal.project.name}」？只删记录，不动文件。
                  </p>
                </>
              ) : (
                <>
                  <h3 className="font-semibold">
                    {modal.kind === 'editTags' && `编辑「${modal.project.name}」的 tag`}
                    {modal.kind === 'renameTag' && `tag 改名：「${modal.tag}」`}
                    {modal.kind === 'singleTags' && '添加单个项目'}
                  </h3>
                  {modal.kind === 'singleTags' && (
                    <p className="mt-2 break-all text-xs text-neutral-500">{modal.path}</p>
                  )}
                  <input
                    autoFocus
                    value={modalInput}
                    onChange={(e) => setModalInput(e.target.value)}
                    placeholder="tag（逗号/空格分隔，可留空）"
                    className="mt-3 w-full rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700 dark:bg-neutral-800"
                  />
                </>
              )}
              <div className="mt-4 flex justify-end gap-2">
                <button
                  onClick={() => setModal(null)}
                  className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
                >
                  取消
                </button>
                <button
                  onClick={confirmModal}
                  className={`rounded-lg px-3 py-1.5 text-sm text-white ${
                    modal.kind === 'deleteTag' || modal.kind === 'removeProject'
                      ? 'bg-red-600 hover:bg-red-700'
                      : 'bg-blue-600 hover:bg-blue-700'
                  }`}
                >
                  {modal.kind === 'deleteTag' || modal.kind === 'removeProject' ? '确认' : '确定'}
                </button>
              </div>
            </div>
          </div>
        )}
        {cloneOpen && (
          <div className="fixed inset-0 z-30 flex items-center justify-center bg-black/40 p-6">
            <div className="w-full max-w-md rounded-xl bg-white p-5 dark:bg-neutral-900">
              <h3 className="font-semibold">从 GitHub 克隆</h3>
              <p className="mt-1 text-xs text-neutral-500">
                支持 https / ssh 地址。克隆完成后自动加入管理器。
              </p>
              <input
                autoFocus
                value={cloneUrl}
                onChange={(e) => setCloneUrl(e.target.value)}
                placeholder="https://github.com/owner/repo.git"
                className="mt-3 w-full rounded-lg border border-neutral-300 bg-white px-3 py-1.5 font-mono text-xs dark:border-neutral-700 dark:bg-neutral-800"
              />
              <div className="mt-2 flex items-center gap-2">
                <button
                  onClick={() => void run(async () => setCloneDir(await pickDirectory()))}
                  className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
                >
                  选择父目录…
                </button>
                <span className="truncate text-xs text-neutral-500" title={cloneDir ?? ''}>
                  {cloneDir ?? '未选择'}
                </span>
              </div>
              <div className="mt-4 flex justify-end gap-2">
                <button
                  onClick={() => setCloneOpen(false)}
                  className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
                >
                  取消
                </button>
                <button
                  onClick={() =>
                    void run(async () => {
                      if (!cloneDir) throw new Error('请先选择父目录')
                      setCloneBusy(true)
                      try {
                        await api.cloneRepo(cloneUrl.trim(), cloneDir)
                        setCloneOpen(false)
                        setCloneUrl('')
                        setCloneDir(null)
                        await refresh()
                      } finally {
                        setCloneBusy(false)
                      }
                    })
                  }
                  disabled={cloneBusy || !cloneUrl.trim() || !cloneDir}
                  className="rounded-lg bg-blue-600 px-3 py-1.5 text-sm text-white disabled:opacity-40"
                >
                  {cloneBusy ? '克隆中…' : '克隆'}
                </button>
              </div>
            </div>
          </div>
        )}

        {pm2Report && (
          <div className="fixed inset-0 z-30 flex items-center justify-center bg-black/40 p-6">
            <div className="w-full max-w-md rounded-xl bg-white p-5 dark:bg-neutral-900">
              <h3 className="font-semibold">导入 PM2 配置</h3>
              <p className="mt-2 text-sm">新增 {pm2Report.imported.length} 个项目的配方。</p>
              <ReportList title="已跳过" items={pm2Report.skipped} />
              <ReportList title="问题" items={pm2Report.errors} />
              <div className="mt-4 flex justify-end">
                <button
                  onClick={() => {
                    setPm2Report(null)
                    void refresh()
                  }}
                  className="rounded-lg bg-blue-600 px-3 py-1.5 text-sm text-white"
                >
                  关闭
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

/** 导入报告里的一段（空则不渲染）。 */
function ReportList({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null
  return (
    <div className="mt-3">
      <div className="text-xs font-semibold text-neutral-500">{title}</div>
      <ul className="mt-1 list-inside list-disc text-xs">
        {items.map((t) => (
          <li key={t}>{t}</li>
        ))}
      </ul>
    </div>
  )
}
