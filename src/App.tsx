import { useEffect, useMemo, useState } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import { api, type ProjectInfo, type ScanCandidate } from './api'

type SortMode = 'created' | 'name'

type Modal =
  | { kind: 'editTags'; project: ProjectInfo }
  | { kind: 'renameTag'; tag: string }
  | { kind: 'deleteTag'; tag: string }
  | { kind: 'singleTags'; path: string }
  | { kind: 'removeProject'; project: ProjectInfo }

async function pickDirectory(): Promise<string | null> {
  // dev 自动化验证旁路：debug 构建读 dev-pick-dir 控制文件；release 恒走原生弹窗。
  const bypass = await api.devPickDir()
  if (bypass) return bypass
  const sel = await open({ directory: true, multiple: false })
  if (!sel) return null
  return Array.isArray(sel) ? (sel[0] ?? null) : sel
}

function splitTags(s: string): string[] {
  return [...new Set(s.split(/[,，、\s]+/).map((t) => t.trim()).filter(Boolean))]
}

export default function App() {
  const [projects, setProjects] = useState<ProjectInfo[]>([])
  const [allTags, setAllTags] = useState<string[]>([])
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState<string[]>([])
  const [sort, setSort] = useState<SortMode>('created')
  const [error, setError] = useState<string | null>(null)
  const [modal, setModal] = useState<Modal | null>(null)
  const [modalInput, setModalInput] = useState('')

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
    return [...list].sort((a, b) =>
      sort === 'name'
        ? a.name.localeCompare(b.name, 'zh-CN')
        : (b.created_date ?? '').localeCompare(a.created_date ?? ''),
    )
  }, [projects, query, selected, sort])

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

  return (
    <div className="min-h-screen bg-neutral-100 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
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
              onClick={() => setSort(sort === 'created' ? 'name' : 'created')}
              className="rounded-lg border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
              title="切换排序"
            >
              排序：{sort === 'created' ? '创建时间倒序' : '名称'}
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
              className="rounded-xl border border-neutral-200 bg-white p-4 shadow-sm dark:border-neutral-800 dark:bg-neutral-900"
            >
              <div className="flex items-center gap-2">
                <h2 className="truncate font-semibold">{p.name}</h2>
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
              <div className="mt-3 flex gap-2 text-xs">
                {!p.missing ? (
                  <button
                    onClick={() => {
                      setModalInput(p.manual_tags.join('，'))
                      setModal({ kind: 'editTags', project: p })
                    }}
                    className="rounded border border-neutral-300 px-2 py-1 dark:border-neutral-700"
                  >
                    编辑 tag
                  </button>
                ) : (
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
      </div>
    </div>
  )
}
