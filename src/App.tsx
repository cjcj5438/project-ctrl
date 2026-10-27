import { useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  errorText,
  type ImportReport,
  type ProjectInfo,
  type ScanCandidate,
  type ServiceStatus,
} from './api'
import DetailView from './Detail'
import UpdateBanner from './Update'
import ProbePanel, { ExternalPanel } from './Probe'
import { ErrorBanner, StatusDot } from './components'
import { opsBadCount, opsRows } from './ops'
import { IconMonitor, IconMoon, IconPencil, IconSearch, IconSun, IconX } from './icons'
import { applyTheme, loadTheme, saveTheme, watchSystemTheme, type ThemeMode } from './theme'
import { humanSize } from './format'
import { pickDirectory, splitTags, subscribe } from './util'

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
  const [statuses, setStatuses] = useState<Record<string, ServiceStatus>>({})
  const [view, setView] = useState<'projects' | 'services'>('projects')
  const [probeOpen, setProbeOpen] = useState(false)
  const [externalOpen, setExternalOpen] = useState(false)
  const statusesRef = useRef<Record<string, ServiceStatus>>({})

  // 克隆 / PM2 导入的独立浮层状态（不塞进既有 Modal 联合类型）
  const [cloneOpen, setCloneOpen] = useState(false)
  const [cloneUrl, setCloneUrl] = useState('')
  const [cloneDir, setCloneDir] = useState<string | null>(null)
  const [cloneBusy, setCloneBusy] = useState(false)
  const [pm2Report, setPm2Report] = useState<ImportReport | null>(null)

  // 项目大小：仅前端内存缓存（key = id + 路径），会话内不重复计算；null 表示计算失败
  const [sizes, setSizes] = useState<Record<string, number | null>>({})
  const [sizeErrs, setSizeErrs] = useState<Record<string, string>>({})
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
        .catch((e) => {
          // 失败原因进 title：界面仍只显示 —，不打断列表
          setSizes((s) => ({ ...s, [key]: null }))
          setSizeErrs((s) => ({ ...s, [key]: errorText(e) }))
        })
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

  // 运行状态：初始化一次 + 订阅 svc:state（不轮询）。
  // 以 key 为粒度维护，项目徽章由「该项目有任一活跃服务」推出。
  useEffect(() => {
    const ACTIVE: ServiceStatus['phase'][] = ['Running', 'Step']
    const apply = (list: ServiceStatus[]) => {
      const byKey: Record<string, ServiceStatus> = {}
      list.forEach((st) => {
        byKey[st.key] = st
      })
      statusesRef.current = byKey
      setStatuses(byKey)
      setRunning(new Set(list.filter((st) => ACTIVE.includes(st.phase)).map((st) => st.project_id)))
    }
    const stop = subscribe<ServiceStatus>('svc:state', (e) => {
      const st = e.payload
      const merged = { ...statusesRef.current, [st.key]: st }
      statusesRef.current = merged
      setStatuses(merged)
      setRunning(
        new Set(
          Object.values(merged)
            .filter((s) => ACTIVE.includes(s.phase))
            .map((s) => s.project_id),
        ),
      )
    })
    let alive = true
    void (async () => {
      try {
        const list = await api.serviceStatus()
        if (alive) apply(list)
      } catch (e) {
        if (alive) setError(`服务状态读取失败：${errorText(e)}`)
      }
    })()
    return () => {
      alive = false
      stop()
    }
  }, [])

  const filterTags = useMemo(() => {
    const auto = new Set<string>()
    projects.forEach((p) => p.auto_tags.forEach((t) => auto.add(t)))
    return [...new Set([...allTags, ...auto])].sort((a, b) => a.localeCompare(b, 'zh-CN'))
  }, [allTags, projects])

  // 运维条：所有活跃/异常服务，按「异常 > 运行中 > 其它」排序。
  // 空数组时整条不渲染——没有运行中的服务就没有运维条，不做空装饰。
  const ops = useMemo(() => opsRows(statuses), [statuses])

  const projectName = (id: number) => projects.find((p) => p.id === id)?.name ?? `#${id}`

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
    <div className="min-h-screen bg-surface-raised text-fg ">
      <UpdateBanner />
      <div className="mx-auto max-w-6xl px-6 py-6">
        <header className="flex flex-wrap items-center gap-3">
          <h1 className="text-xl font-bold">项目管理</h1>
          <div className="relative ml-2">
            <span className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-muted">
              <IconSearch size={14} />
            </span>
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="按名称搜索…"
              aria-label="按名称搜索"
              className="w-56 rounded-lg border border-border bg-surface py-1.5 pl-7 pr-3 text-sm focus:border-primary"
            />
          </div>
          <div className="ml-auto flex items-center gap-2">
            <button
              type="button"
              onClick={() => setSort(NEXT_SORT[sort])}
              className="rounded-lg border border-border px-3 py-1.5 text-sm "
              title="切换排序"
            >
              排序：{SORT_LABEL[sort]}
            </button>
            <button
              type="button"
              onClick={doScan}
              className="rounded-lg border border-transparent bg-info px-3 py-1.5 text-sm text-on-info hover:brightness-95"
            >
              扫描文件夹…
            </button>
            <button
              type="button"
              onClick={doAddSingle}
              className="rounded-lg border border-border px-3 py-1.5 text-sm "
            >
              添加单个项目…
            </button>
            <button
              type="button"
              onClick={() => setCloneOpen(true)}
              className="rounded-lg border border-border px-3 py-1.5 text-sm "
            >
              从 GitHub 克隆…
            </button>
            <button
              type="button"
              onClick={() => setProbeOpen(true)}
              className="rounded-lg border border-border px-3 py-1.5 text-sm "
              title="只读扫描所有项目的可启动方式，勾选后只写本机"
            >
              批量探测…
            </button>
            <button
              type="button"
              onClick={() => setExternalOpen(true)}
              className="rounded-lg border border-border px-3 py-1.5 text-sm "
              title="launchd 与 PM2 的只读视图"
            >
              外部服务…
            </button>
            <ThemeButton />
          </div>
        </header>

        {error && <ErrorBanner text={error} onClose={() => setError(null)} />}

        {/* 双视图：项目（默认）与服务；服务视图只显示运行态，不是「另一个列表」 */}
        <div className="mt-4 flex items-center gap-1 text-sm" role="group" aria-label="视图切换">
          {(['projects', 'services'] as const).map((v) => (
            <button
              type="button"
              key={v}
              aria-pressed={view === v}
              onClick={() => setView(v)}
              className={`rounded-md px-2.5 py-1 ${
                view === v ? 'bg-surface text-fg shadow-sm' : 'text-muted hover:text-fg'
              }`}
            >
              {v === 'projects' ? '项目' : '服务'}
              {v === 'services' && ops.length > 0 && (
                <span className="ml-1.5 text-xs text-muted">{ops.length}</span>
              )}
            </button>
          ))}
        </div>

        {/* 运维条：只在有活跃/异常服务时出现 */}
        {view === 'projects' && ops.length > 0 && <OpsBar rows={ops} projectName={projectName} onOpen={setDetailId} />}

        <div className="mt-4 flex flex-wrap items-center gap-2">
          <span className="text-sm text-muted">筛选：</span>
          {filterTags.length === 0 && <span className="text-sm text-muted">暂无 tag</span>}
          {filterTags.map((t) => {
            const active = selected.includes(t)
            const manual = allTags.includes(t)
            return (
              <span
                key={t}
                className={`inline-flex items-center gap-1 rounded-full border px-2.5 py-1 text-xs ${
                  active
                    ? 'border-primary bg-primary text-primary-fg'
                    : manual
                      ? 'border-border bg-surface text-fg'
                      : 'border-dashed border-border bg-surface-raised text-muted'
                }`}
              >
                <button
                  type="button"
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
                      type="button"
                      onClick={() => {
                        setModalInput(t)
                        setModal({ kind: 'renameTag', tag: t })
                      }}
                      className="opacity-60 transition-opacity hover:opacity-100"
                      aria-label={`重命名 ${t}`}
                      title="改名"
                    >
                      <IconPencil size={12} />
                    </button>
                    <button
                      type="button"
                      onClick={() => setModal({ kind: 'deleteTag', tag: t })}
                      className="opacity-60 transition-opacity hover:opacity-100"
                      aria-label={`删除 ${t}`}
                      title="删除"
                    >
                      <IconX size={12} />
                    </button>
                  </>
                )}
              </span>
            )
          })}
          {selected.length > 0 && (
            <button type="button" onClick={() => setSelected([])} className="text-xs text-info hover:underline">
              清除筛选
            </button>
          )}
        </div>

        {view === 'projects' && <div className="mt-4 text-sm text-muted">共 {visible.length} 个项目</div>}

        {view === 'projects' && (
        <div className="mt-3 grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {visible.map((p) => (
            <div
              key={p.id}
              role="button"
              tabIndex={0}
              aria-label={`打开项目 ${p.name}${running.has(p.id) ? '（服务运行中）' : ''}`}
              onClick={() => setDetailId(p.id)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault()
                  setDetailId(p.id)
                }
              }}
              className="cursor-pointer rounded-xl border border-border bg-surface p-4 transition-colors hover:border-success focus-visible:border-success"
            >
              <div className="flex items-center gap-2">
                <h2 className="truncate font-semibold">{p.name}</h2>
                {running.has(p.id) && (
                  <span
                    className="shrink-0 inline-block h-2 w-2 rounded-full bg-success"
                    title="服务运行中"
                    aria-hidden
                  />
                )}
                {p.missing && (
                  <span className="shrink-0 rounded border border-error/40 bg-error/10 px-1.5 py-0.5 text-xs text-error">
                    路径丢失
                  </span>
                )}
              </div>
              <div className="mt-1 truncate text-xs text-muted" title={p.path}>
                {p.path}
              </div>
              <div className="mt-1 text-xs text-muted">创建日期：{p.created_date ?? '未知'}</div>
              {!p.missing && (
                <div className="mt-1 text-xs text-muted" title={sizeErrs[sizeKey(p)] ? `大小计算失败：${sizeErrs[sizeKey(p)]}` : undefined}>大小：{formatSize(p)}</div>
              )}
              <div className="mt-2 flex flex-wrap gap-1.5">
                {p.manual_tags.map((t) => (
                  <span
                    key={t}
                    className="inline-flex items-center gap-1 rounded-full border border-success/40 bg-success/10 px-2 py-0.5 text-xs text-fg"
                  >
                    {t}
                  </span>
                ))}
                {p.auto_tags
                  .filter((t) => !p.manual_tags.includes(t))
                  .map((t) => (
                    <span
                      key={t}
                      className="rounded-full border border-dashed border-border px-2 py-0.5 text-xs text-muted"
                    >
                      {t}
                    </span>
                  ))}
                {p.manual_tags.length === 0 && p.auto_tags.length === 0 && (
                  <span className="text-xs text-muted">无 tag</span>
                )}
              </div>
              <div className="mt-3 flex flex-wrap gap-2 text-xs" onClick={(e) => e.stopPropagation()}>
                {p.missing ? (
                  <>
                    <button
                      type="button"
                      onClick={() => relink(p)}
                      className="rounded bg-primary px-2 py-1 text-primary-fg"
                    >
                      重新关联
                    </button>
                    <button
                      type="button"
                      onClick={() => setModal({ kind: 'removeProject', project: p })}
                      className="rounded border border-border px-2 py-1 "
                    >
                      移除
                    </button>
                  </>
                ) : (
                  <>
                    <button
                      type="button"
                      onClick={() => {
                        setModalInput(p.manual_tags.join('，'))
                        setModal({ kind: 'editTags', project: p })
                      }}
                      className="rounded border border-border px-2 py-1 "
                    >
                      编辑 tag
                    </button>
                    <button
                      type="button"
                      onClick={() => run(() => api.openInFinder(p.id))}
                      className="rounded border border-border px-2 py-1 "
                    >
                      Finder
                    </button>
                    <button
                      type="button"
                      onClick={() => run(() => api.openInIterm(p.id))}
                      className="rounded border border-border px-2 py-1 "
                    >
                      iTerm
                    </button>
                    <button
                      type="button"
                      onClick={() => run(() => api.openInVscode(p.id))}
                      className="rounded border border-border px-2 py-1 "
                    >
                      VS Code
                    </button>
                    {p.github_url && (
                      <button
                        type="button"
                        onClick={() => run(() => api.openGithub(p.id))}
                        className="rounded border border-border px-2 py-1 "
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
        )}

        {view === 'services' && <ServicesView rows={ops} projectName={projectName} onOpen={setDetailId} />}

        {scanRoot && (
          <div className="fixed inset-0 flex items-center justify-center bg-scrim p-6">
            <div className="max-h-[80vh] w-full max-w-lg overflow-auto rounded-xl bg-surface p-5 ">
              <h3 className="font-semibold break-all">扫描结果：{scanRoot}</h3>
              <p className="mt-1 text-xs text-muted">
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
                    <span className="shrink-0 font-medium">{c.name}</span>
                    <span className="min-w-0 flex-1 truncate text-xs text-muted" title={c.path}>{c.path}</span>
                  </label>
                ))}
                {candidates.length === 0 && (
                  <div className="py-4 text-sm text-muted">没有可加入的新目录。</div>
                )}
              </div>
              <input
                value={batchTags}
                onChange={(e) => setBatchTags(e.target.value)}
                placeholder="给所选项目批量打 tag（逗号/空格分隔，可留空）"
                className="mt-4 w-full rounded-lg border border-border px-3 py-1.5 text-sm "
              />
              <div className="mt-4 flex justify-end gap-2">
                <button
                  type="button"
                  onClick={() => setScanRoot(null)}
                  className="rounded-lg border border-border px-3 py-1.5 text-sm "
                >
                  取消
                </button>
                <button
                  type="button"
                  onClick={doJoin}
                  disabled={checked.size === 0}
                  className="rounded-lg bg-primary px-3 py-1.5 text-sm text-primary-fg disabled:opacity-40"
                >
                  加入（{checked.size}）
                </button>
              </div>
            </div>
          </div>
        )}

        {modal && (
          <div className="fixed inset-0 flex items-center justify-center bg-scrim p-6">
            <div className="w-full max-w-sm rounded-xl bg-surface p-5 ">
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
                    <p className="mt-2 break-all text-xs text-muted">{modal.path}</p>
                  )}
                  <input
                    autoFocus
                    value={modalInput}
                    onChange={(e) => setModalInput(e.target.value)}
                    placeholder="tag（逗号/空格分隔，可留空）"
                    className="mt-3 w-full rounded-lg border border-border px-3 py-1.5 text-sm "
                  />
                </>
              )}
              <div className="mt-4 flex justify-end gap-2">
                <button
                  type="button"
                  onClick={() => setModal(null)}
                  className="rounded-lg border border-border px-3 py-1.5 text-sm "
                >
                  取消
                </button>
                <button
                  type="button"
                  onClick={confirmModal}
                  className={`rounded-lg px-3 py-1.5 text-sm text-primary-fg ${ modal.kind === 'deleteTag' || modal.kind === 'removeProject'
                      ? 'bg-error-solid text-on-solid hover:brightness-95'
                      : 'bg-primary hover:brightness-95'
                  }`}
                >
                  {modal.kind === 'deleteTag' || modal.kind === 'removeProject' ? '确认' : '确定'}
                </button>
              </div>
            </div>
          </div>
        )}
        {cloneOpen && (
          <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6">
            <div className="w-full max-w-md rounded-xl bg-surface p-5 ">
              <h3 className="font-semibold">从 GitHub 克隆</h3>
              <p className="mt-1 text-xs text-muted">
                支持 https / ssh 地址。克隆完成后自动加入管理器。
              </p>
              <input
                autoFocus
                value={cloneUrl}
                onChange={(e) => setCloneUrl(e.target.value)}
                placeholder="https://github.com/owner/repo.git"
                className="mt-3 w-full rounded-lg border border-border bg-surface px-3 py-1.5 font-mono text-xs "
              />
              <div className="mt-2 flex items-center gap-2">
                <button
                  type="button"
                  onClick={() => void run(async () => setCloneDir(await pickDirectory()))}
                  className="rounded-lg border border-border px-3 py-1.5 text-sm "
                >
                  选择父目录…
                </button>
                <span className="min-w-0 flex-1 truncate text-xs text-muted" title={cloneDir ?? ''}>
                  {cloneDir ?? '未选择'}
                </span>
              </div>
              <div className="mt-4 flex justify-end gap-2">
                <button
                  type="button"
                  onClick={() => setCloneOpen(false)}
                  className="rounded-lg border border-border px-3 py-1.5 text-sm "
                >
                  取消
                </button>
                <button
                  type="button"
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
                  className="rounded-lg bg-primary px-3 py-1.5 text-sm text-primary-fg disabled:opacity-40"
                >
                  {cloneBusy ? '克隆中…' : '克隆'}
                </button>
              </div>
            </div>
          </div>
        )}

        {pm2Report && (
          <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6">
            <div className="w-full max-w-md rounded-xl bg-surface p-5 ">
              <h3 className="font-semibold">导入 PM2 配置</h3>
              <p className="mt-2 text-sm">新增 {pm2Report.imported.length} 个项目的配方。</p>
              <ReportList title="已跳过" items={pm2Report.skipped} />
              <ReportList title="问题" items={pm2Report.errors} />
              <div className="mt-4 flex justify-end">
                <button
                  type="button"
                  onClick={() => {
                    setPm2Report(null)
                    void refresh()
                  }}
                  className="rounded-lg bg-primary px-3 py-1.5 text-sm text-primary-fg"
                >
                  关闭
                </button>
              </div>
            </div>
          </div>
        )}

      </div>

      {probeOpen && <ProbePanel onClose={() => setProbeOpen(false)} onDone={refresh} />}
      {externalOpen && <ExternalPanel onClose={() => setExternalOpen(false)} />}
    </div>
  )
}

/** 导入报告里的一段（空则不渲染）。 */
function ReportList({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null
  return (
    <div className="mt-3">
      <div className="text-xs font-semibold text-muted">{title}</div>
      <ul className="mt-1 list-inside list-disc text-xs">
        {items.map((t) => (
          <li key={t}>{t}</li>
        ))}
      </ul>
    </div>
  )
}

/** 主题三档切换：跟随系统 / 浅色 / 深色，立即生效并持久化。 */
function ThemeButton() {
  const [mode, setMode] = useState<ThemeMode>(() => loadTheme())

  useEffect(() => {
    applyTheme(mode)
    saveTheme(mode)
  }, [mode])

  // 只有「跟随系统」需要监听系统变化
  useEffect(() => {
    if (mode !== 'auto') return
    return watchSystemTheme(() => applyTheme('auto'))
  }, [mode])

  const next: Record<ThemeMode, ThemeMode> = { auto: 'light', light: 'dark', dark: 'auto' }
  const label: Record<ThemeMode, string> = { auto: '跟随系统', light: '浅色', dark: '深色' }
  const icon = mode === 'light' ? <IconSun size={14} /> : mode === 'dark' ? <IconMoon size={14} /> : <IconMonitor size={14} />

  return (
    <button
      type="button"
      onClick={() => setMode(next[mode])}
      title={`主题：${label[mode]}（点击切换）`}
      aria-label={`主题：${label[mode]}，点击切换`}
      className="inline-flex items-center gap-1.5 rounded-lg border border-border px-2.5 py-1.5 text-sm text-muted hover:bg-surface hover:text-fg"
    >
      {icon}
      <span className="text-xs">{label[mode]}</span>
    </button>
  )
}

/** 服务视图：只有运行态，不重复项目列表。空态明确说明「没有服务在跑」。 */
function ServicesView({
  rows,
  projectName,
  onOpen,
}: {
  rows: ServiceStatus[]
  projectName: (id: number) => string
  onOpen: (id: number) => void
}) {
  if (rows.length === 0) {
    return (
      <div className="mt-6 rounded-card border border-dashed border-border p-8 text-center text-sm text-muted">
        没有正在运行或异常的服务。到项目详情页的「服务」面板里启动，或用工具栏的「批量探测…」。
      </div>
    )
  }
  return (
    <div className="mt-4 overflow-hidden rounded-card border border-border">
      <table className="w-full text-left text-xs">
        <thead className="bg-surface-raised text-muted">
          <tr>
            <th className="px-3 py-2 font-medium">项目</th>
            <th className="px-3 py-2 font-medium">服务</th>
            <th className="px-3 py-2 font-medium">状态</th>
            <th className="px-3 py-2 font-medium">端口</th>
            <th className="px-3 py-2 font-medium">PID</th>
            <th className="px-3 py-2 font-medium">重启</th>
            <th className="px-3 py-2 font-medium">提示</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((st) => {
            const tone =
              st.phase === 'Running' ? 'ok' : st.phase === 'Step' ? 'busy' : st.phase === 'Crashed' || st.phase === 'Failed' ? 'bad' : 'idle'
            return (
              <tr
                key={st.key}
                className="cursor-pointer border-t border-border hover:bg-surface-raised"
                onClick={() => onOpen(st.project_id)}
              >
                <td className="px-3 py-1.5">{projectName(st.project_id)}</td>
                <td className="px-3 py-1.5 font-medium">{st.name}</td>
                <td className="px-3 py-1.5">
                  <StatusDot tone={tone} label={st.step ? `${PHASE_TEXT[st.phase]} · ${st.step}` : PHASE_TEXT[st.phase]} />
                </td>
                <td className="px-3 py-1.5 font-mono text-muted">{st.port ?? '—'}</td>
                <td className="px-3 py-1.5 font-mono text-muted">{st.pid ?? '—'}</td>
                <td className="px-3 py-1.5 text-muted">{st.restarts > 0 ? st.restarts : '—'}</td>
                <td className="max-w-[280px] truncate px-3 py-1.5 text-warning" title={st.note ?? ''}>
                  {st.note ?? ''}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

const PHASE_TEXT: Record<ServiceStatus['phase'], string> = {
  Idle: '未运行',
  Step: '执行中',
  Running: '运行中',
  Stopped: '已停止',
  Crashed: '已崩溃',
  Failed: '失败',
}

/** 运维条：一条紧凑的实时状态带，异常优先。 */
function OpsBar({
  rows,
  projectName,
  onOpen,
}: {
  rows: ServiceStatus[]
  projectName: (id: number) => string
  onOpen: (id: number) => void
}) {
  const bad = opsBadCount(rows)
  return (
    <div className="mt-3 flex flex-wrap items-center gap-2 rounded-lg border border-border bg-surface-raised px-3 py-2 text-xs">
      <span className="font-medium text-muted">运行中</span>
      {rows.slice(0, 6).map((st) => {
        const tone = st.phase === 'Running' ? 'ok' : st.phase === 'Step' ? 'busy' : 'bad'
        return (
          <button
            type="button"
            key={st.key}
            onClick={() => onOpen(st.project_id)}
            className="inline-flex items-center gap-1.5 rounded-full border border-border bg-surface px-2 py-0.5 hover:border-success"
            title={`${projectName(st.project_id)} / ${st.name}（${PHASE_TEXT[st.phase]}${st.port != null ? ` · 端口 ${st.port}` : ''}）`}
            aria-label={`${projectName(st.project_id)} / ${st.name}：${PHASE_TEXT[st.phase]}${st.port != null ? `，端口 ${st.port}` : ''}`}
          >
            <StatusDot tone={tone} title={PHASE_TEXT[st.phase]} />
            <span>{st.name}</span>
            {st.port != null && <span className="font-mono text-muted">:{st.port}</span>}
          </button>
        )
      })}
      {rows.length > 6 && <span className="text-muted">+{rows.length - 6}</span>}
      {bad > 0 && <span className="ml-auto text-error">{bad} 个异常</span>}
    </div>
  )
}
