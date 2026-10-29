import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  errorText,
  type AiSessionView,
  type ImportReport,
  type ProjectInfo,
  type ScanCandidate,
  type ServiceStatus,
  type SourceInfo,
} from './api'
import BatchProfile from './BatchProfile'
import Detail from './Detail'
import ExternalView from './ExternalView'
import ProjectList, { type SortMode } from './ProjectList'
import Settings from './Settings'
import ProbePanel from './Probe'
import ServicesView from './ServicesView'
import Sidebar, { type AiSession } from './Sidebar'
import UpdateBanner from './Update'
import { CloneModal, Pm2ReportModal, ScanModal, TagModal, type TagModalKind } from './AppModals'
import { ErrorBanner } from './components'
import { opsRows } from './ops'
import { liveSessions, mergeSession } from './aiSessions'
import type { AiTask, DetailTab, Route } from './route'
import { pickDirectory, splitTags, subscribe } from './util'

/** 单个项目大小的并发计算上限。 */
const SIZE_CONCURRENCY = 3

/** 大小缓存的 key：项目 id + 路径（路径变化即视为需要重算）。 */
const sizeKey = (p: ProjectInfo): string => `${p.id}\u0000${p.path}`

export default function App() {
  const [projects, setProjects] = useState<ProjectInfo[]>([])
  const [allTags, setAllTags] = useState<string[]>([])
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState<string[]>([])
  const [sort, setSort] = useState<SortMode>('created')
  const [error, setError] = useState<string | null>(null)
  const [modal, setModal] = useState<TagModalKind | null>(null)
  const [route, setRoute] = useState<Route>({ view: 'projects' })
  /** 侧栏 AI 任务列表：`ai_sessions` 命令 + `ai:session` 事件（全局，含所有项目）。 */
  const [aiSessions, setAiSessions] = useState<AiSessionView[]>([])

  // 有服务处于运行中/过渡态的项目 id（来自 svc:state 事件，不轮询）
  const [running, setRunning] = useState<Set<number>>(new Set())
  const [statuses, setStatuses] = useState<Record<string, ServiceStatus>>({})
  const [probeOpen, setProbeOpen] = useState(false)
  // 详情页「源码」页签的脏文件数，用于页签徽章
  const [sourceInfo, setSourceInfo] = useState<SourceInfo | null>(null)
  const statusesRef = useRef<Record<string, ServiceStatus>>({})

  // 克隆 / PM2 导入 / AI 批量画像的独立浮层状态（不塞进既有 Modal 联合类型）
  const [cloneOpen, setCloneOpen] = useState(false)
  const [pm2Report, setPm2Report] = useState<ImportReport | null>(null)
  const [batchOpen, setBatchOpen] = useState(false)

  // 项目大小：仅前端内存缓存（key = id + 路径），会话内不重复计算；null 表示计算失败
  const [sizes, setSizes] = useState<Record<string, number | null>>({})
  const [sizeErrs, setSizeErrs] = useState<Record<string, string>>({})
  const sizeQueued = useRef<Set<string>>(new Set())
  const sizeQueue = useRef<string[]>([])
  const sizeActive = useRef(0)
  // AI 任务请求序号：同一任务重复触发也能被消费方区分
  const aiNonce = useRef(0)

  // 扫描弹层
  const [scanRoot, setScanRoot] = useState<string | null>(null)
  const [candidates, setCandidates] = useState<ScanCandidate[]>([])

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

  // 侧栏 AI 任务：靠全局 `ai:session` 事件增量合并（后端 `emit_session` 在每次
  // 状态变化时发到 app handle 上，不按项目筛，顶层订阅就能看到所有会话）。
  const syncAiSessions = useCallback(() => {
    void api
      .aiSessions()
      .then(setAiSessions)
      .catch(() => {
        // 只是便利信息：失败就保持现状，不弹错误条
      })
  }, [])

  useEffect(() => {
    const stop = subscribe<AiSessionView>('ai:session', (e) => {
      setAiSessions((list) => mergeSession(list, e.payload))
    })
    return stop
  }, [])

  // 会话关闭时后端没有对应事件（已从内存表移除）：换路由时补读一次，顺带把
  // 已结束的会话清出列表（后端只是读内存 map，代价可忽略）。
  const routeProjectId = route.view === 'project' ? route.id : 0
  useEffect(() => {
    syncAiSessions()
  }, [route.view, routeProjectId, syncAiSessions])

  const liveAiSessions = useMemo(() => liveSessions(aiSessions), [aiSessions])

  const filterTags = useMemo(() => {
    const auto = new Set<string>()
    projects.forEach((p) => p.auto_tags.forEach((t) => auto.add(t)))
    return [...new Set([...allTags, ...auto])].sort((a, b) => a.localeCompare(b, 'zh-CN'))
  }, [allTags, projects])

  // 运维条 / 服务页的数据源：所有活跃/异常服务，按「异常 > 运行中 > 其它」排序
  const ops = useMemo(() => opsRows(statuses), [statuses])

  const projectName = (id: number) => projects.find((p) => p.id === id)?.name ?? `#${id}`

  // tag 的 AND 过滤在列表侧：侧栏只持有选中的 tag，搜索与排序在列表页内部
  const visible = useMemo(
    () =>
      projects.filter((p) => {
        const tags = new Set([...p.manual_tags, ...p.auto_tags])
        return selected.every((t) => tags.has(t))
      }),
    [projects, selected],
  )

  // 用 useCallback 固定引用：列表页的排序 useMemo 依赖它们，
  // 每次渲染新建函数会让「按大小排序」白算一遍
  const sizeOf = useCallback(
    (p: ProjectInfo): number | null | undefined => sizes[sizeKey(p)],
    [sizes],
  )
  const sizeErrOf = useCallback((p: ProjectInfo): string | undefined => sizeErrs[sizeKey(p)], [sizeErrs])

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

  const detailProject =
    route.view === 'project' ? (projects.find((p) => p.id === route.id) ?? null) : null

  // 当前项目的源码信息：只在详情页拉，供「源码」页签的脏文件徽章
  const detailId = route.view === 'project' && detailProject ? route.id : null
  useEffect(() => {
    setSourceInfo(null)
    if (detailId === null || !detailProject?.has_git) return
    let alive = true
    void (async () => {
      try {
        const si = await api.sourceInfo(detailId)
        if (alive) setSourceInfo(si)
      } catch {
        // 徽章只是便利信息：失败就不显示，源码页签里能看到具体错误
      }
    })()
    return () => {
      alive = false
    }
  }, [detailId, detailProject?.has_git])

  const runningServiceCount = useMemo(() => {
    if (detailId === null) return 0
    return Object.values(statuses).filter(
      (st) => st.project_id === detailId && (st.phase === 'Running' || st.phase === 'Step'),
    ).length
  }, [statuses, detailId])

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
    })
  }

  const doAddSingle = async () => {
    const dir = await pickDirectory()
    if (!dir) return
    setModal({ kind: 'singleTags', path: dir })
  }

  const doImportPm2 = async () => {
    await run(async () => {
      setPm2Report(await api.importPm2())
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

  /**
   * AI 步骤的挂载点：请求落进路由的 `ai` 字段，行为由后续步骤接管。
   * `nonce` 每次自增，同一任务连点两次也能让消费方识别成两次请求。
   */
  const requestAi = (id: number, tab: DetailTab, task: AiTask) => {
    aiNonce.current += 1
    setRoute({ view: 'project', id, tab, ai: { task, nonce: aiNonce.current } })
  }

  /** 详情页内发起的任务：保持当前页签，不把用户弹到别处。 */
  const requestAiHere = (id: number, tab: DetailTab, task: AiTask) => {
    aiNonce.current += 1
    setRoute((r) =>
      r.view === 'project' && r.id === id
        ? { ...r, ai: { task, nonce: aiNonce.current } }
        : { view: 'project', id, tab, ai: { task, nonce: aiNonce.current } },
    )
  }

  /**
   * 侧栏点已有的 AI 任务：只跳项目 + 打开抽屉，绝不顺手发任务。
   * 任务形如 `{kind:'diagnose'}` 之类的一次性指令，重放会重复消耗 AI。
   */
  const openAiSession = (s: AiSession) => {
    aiNonce.current += 1
    setRoute({ view: 'project', id: s.project_id, tab: 'services', ai: { open: true, nonce: aiNonce.current } })
  }

  return (
    <div className="flex h-screen flex-col overflow-hidden bg-surface text-fg">
      <UpdateBanner />
      <div className="flex min-h-0 flex-1">
        <Sidebar
          projects={projects}
          runningProjectIds={running}
          runningServiceCount={ops.filter((st) => st.phase === 'Running').length}
          route={route}
          onRoute={setRoute}
          onOpenAi={openAiSession}
          aiSessions={liveAiSessions}
          tags={{
            filterTags,
            allTags,
            selected,
            onToggle: (t) =>
              setSelected((s) => (s.includes(t) ? s.filter((x) => x !== t) : [...s, t])),
            onRename: (t) => setModal({ kind: 'renameTag', tag: t }),
            onDelete: (t) => setModal({ kind: 'deleteTag', tag: t }),
            onClear: () => setSelected([]),
          }}
        />

        <main className="flex min-w-0 flex-1 flex-col">
          {error && (
            <div className="shrink-0 px-3 pt-2">
              <ErrorBanner text={error} onClose={() => setError(null)} />
            </div>
          )}

          {detailProject ? (
            <Detail
              project={detailProject}
              size={sizeOf(detailProject)}
              runningServiceCount={runningServiceCount}
              dirtyCount={sourceInfo?.is_repo ? sourceInfo.dirty : null}
              onBack={() => setRoute({ view: 'projects' })}
              onChanged={refresh}
              onRemoved={() => {
                setRoute({ view: 'projects' })
                void refresh()
              }}
              onEditTags={(p) => setModal({ kind: 'editTags', project: p })}
              tab={route.view === 'project' ? route.tab : 'overview'}
              onTab={(tab) =>
                setRoute((r) => (r.view === 'project' ? { view: 'project', id: r.id, tab } : r))
              }
              onAiTask={(task) => requestAiHere(detailProject.id, route.view === 'project' ? route.tab : 'overview', task)}
              ai={route.view === 'project' ? route.ai : undefined}
              onRouteSettings={() => setRoute({ view: 'settings' })}
            />
          ) : route.view === 'services' ? (
            <ServicesView
              rows={ops}
              projectName={projectName}
              onOpen={(id, tab = 'services') => setRoute({ view: 'project', id, tab })}
              onDiagnose={(id, service) => requestAi(id, 'services', { kind: 'diagnose', service })}
            />
          ) : route.view === 'external' ? (
            <ExternalView />
          ) : route.view === 'settings' ? (
            <Settings />
          ) : (
            <ProjectList
              projects={visible}
              running={running}
              query={query}
              onQuery={setQuery}
              sort={sort}
              onSort={setSort}
              sizeOf={sizeOf}
              sizeErr={sizeErrOf}
              onOpen={(id) => setRoute({ view: 'project', id, tab: 'overview' })}
              onScan={doScan}
              onAddSingle={doAddSingle}
              onClone={() => setCloneOpen(true)}
              onProbe={() => setProbeOpen(true)}
              onImportPm2={doImportPm2}
              onBatchProfile={() => setBatchOpen(true)}
              onRefresh={refresh}
              onEditTags={(p) => setModal({ kind: 'editTags', project: p })}
              onRelink={relink}
              onRemove={(p) => setModal({ kind: 'removeProject', project: p })}
              onRun={run}
            />
          )}
        </main>
      </div>

      {scanRoot && (
        <ScanModal
          root={scanRoot}
          candidates={candidates}
          onCancel={() => {
            setScanRoot(null)
            setCandidates([])
          }}
          onAdd={async (paths, tags) => {
            await api.addProjects(paths, splitTags(tags))
            setScanRoot(null)
            setCandidates([])
            await refresh()
          }}
        />
      )}

      {modal && (
        <TagModal
          modal={modal}
          onCancel={() => setModal(null)}
          onSubmit={async (input) => {
            switch (modal.kind) {
              case 'editTags':
                await api.setProjectTags(modal.project.id, splitTags(input))
                break
              case 'renameTag': {
                const next = input.trim()
                if (!next || next === modal.tag) {
                  setModal(null)
                  return
                }
                await api.renameTag(modal.tag, next)
                setSelected((s) => s.map((x) => (x === modal.tag ? next : x)))
                break
              }
              case 'deleteTag':
                await api.deleteTag(modal.tag)
                setSelected((s) => s.filter((x) => x !== modal.tag))
                break
              case 'singleTags':
                await api.addProjects([modal.path], splitTags(input))
                break
              case 'removeProject':
                await api.removeProject(modal.project.id)
                break
            }
            setModal(null)
            await refresh()
          }}
        />
      )}

      {cloneOpen && (
        <CloneModal
          onClose={() => setCloneOpen(false)}
          onCloned={refresh}
        />
      )}

      {pm2Report && (
        <Pm2ReportModal
          report={pm2Report}
          onClose={() => {
            setPm2Report(null)
            void refresh()
          }}
        />
      )}

      {probeOpen && <ProbePanel onClose={() => setProbeOpen(false)} onDone={refresh} />}

      {batchOpen && (
        <BatchProfile
          projects={projects}
          onClose={() => setBatchOpen(false)}
          onAdopted={refresh}
        />
      )}
    </div>
  )
}
