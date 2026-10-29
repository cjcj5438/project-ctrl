import { useEffect, useState } from 'react'
import type { ProjectInfo } from './api'
import { Segmented } from './components'
import { IconGlobe, IconGrid, IconPencil, IconServer, IconSettings, IconX } from './icons'
import { applyTheme, loadTheme, saveTheme, watchSystemTheme, type ThemeMode } from './theme'
import type { Route } from './route'

/** 侧栏的 AI 会话占位（AI 步骤接线；本步骤只渲染传进来的数据）。 */
export interface AiSession {
  session_id: string
  project_id: number
  project_name: string
  state: string
  task: string | null
}

/** 会话里需要出现在侧栏的状态（其余状态不占位）。 */
const LIVE_STATES: Record<string, true> = { busy: true, recovering: true, awaiting_user: true }

const NAV = [
  { view: 'projects', label: '项目', Icon: IconGrid },
  { view: 'services', label: '服务', Icon: IconServer },
  { view: 'external', label: '外部服务', Icon: IconGlobe },
  { view: 'settings', label: '设置', Icon: IconSettings },
] as const

/**
 * 原生外壳的侧栏：导航 + tag 筛选 + AI 任务 + 主题。
 *
 * 顶部 52px 是窗口拖动区（titleBarStyle: Overlay 的交通灯在左上角），
 * 所以内容整体左内边距 76px，避免任何东西压在红绿灯下面。
 */
export default function Sidebar({
  projects,
  runningProjectIds,
  runningServiceCount,
  route,
  onRoute,
  onOpenAi,
  tags,
  aiSessions = [],
}: {
  projects: ProjectInfo[]
  runningProjectIds: Set<number>
  runningServiceCount: number
  route: Route
  onRoute: (r: Route) => void
  /** 点侧栏的 AI 任务：跳到该项目并把抽屉打开（不发新任务）。 */
  onOpenAi: (s: AiSession) => void
  tags: {
    filterTags: string[]
    allTags: string[]
    selected: string[]
    onToggle: (tag: string) => void
    onRename: (tag: string) => void
    onDelete: (tag: string) => void
    onClear: () => void
  }
  aiSessions?: AiSession[]
}) {
  const live = aiSessions.filter((s) => LIVE_STATES[s.state] === true)
  const onProjects = route.view === 'projects'

  return (
    <aside className="flex w-[232px] shrink-0 flex-col overflow-hidden border-r border-border bg-[var(--sidebar)] backdrop-blur-xl">
      {/* 拖动区：空着即可，交通灯占掉左上角 */}
      <div data-tauri-drag-region className="h-[52px] shrink-0" />

      {/* 导航 / 标签 / AI 各自可滚，主题切换固定在底部 */}
      <div className="min-h-0 flex-1 overflow-y-auto">
        <nav aria-label="主导航" className="flex flex-col gap-0.5 px-1.5 pb-2">
          {NAV.map(({ view, label, Icon }) => {
            const active = route.view === view
            return (
              <button
                key={view}
                type="button"
                aria-current={active ? 'page' : undefined}
                onClick={() => onRoute({ view })}
                className={`flex h-7 items-center gap-2 rounded-[var(--radius-control)] px-2 text-[13px] transition-colors duration-150 ease-out ${
                  active ? 'bg-surface-raised text-fg' : 'text-muted hover:text-fg'
                }`}
              >
                <Icon size={14} />
                <span className="min-w-0 flex-1 truncate text-left">{label}</span>
                {view === 'projects' && projects.length > 0 && (
                  <span
                    className="tabular shrink-0 text-[11px] text-subtle"
                    title={`${runningProjectIds.size} 个项目有服务在跑`}
                  >
                    {projects.length}
                  </span>
                )}
                {view === 'services' && runningServiceCount > 0 && (
                  <span className="tabular shrink-0 rounded-full border border-success/50 px-1.5 text-[10px] text-success">
                    {runningServiceCount}
                  </span>
                )}
              </button>
            )
          })}
        </nav>

        <section aria-label="标签筛选" className="border-t border-border px-1.5 py-2">
          <div className="flex items-center gap-1 px-2 pb-1">
            <h2 className="min-w-0 flex-1 text-[11px] font-semibold text-subtle">标签</h2>
            {tags.selected.length > 0 && (
              <button
                type="button"
                onClick={tags.onClear}
                className="shrink-0 text-[10px] text-info hover:underline"
              >
                清除
              </button>
            )}
          </div>
          {!onProjects && tags.filterTags.length > 0 && (
            <p className="px-2 pb-1 text-[10px] text-subtle">筛选只在「项目」视图生效</p>
          )}
          <div className="flex flex-col gap-0.5">
            {tags.filterTags.length === 0 && (
              <p className="px-2 py-0.5 text-[11px] text-subtle">暂无 tag</p>
            )}
            {tags.filterTags.map((t) => {
              const active = tags.selected.includes(t)
              const manual = tags.allTags.includes(t)
              return (
                <div
                  key={t}
                  className={`group flex h-6 items-center gap-1 rounded-[var(--radius-control)] px-2 text-[12px] transition-colors duration-150 ease-out ${
                    active
                      ? 'bg-primary/15 text-fg'
                      : manual
                        ? 'text-fg hover:bg-surface-raised'
                        : 'text-muted hover:bg-surface-raised'
                  } ${manual ? '' : 'opacity-80'}`}
                >
                  <button
                    type="button"
                    aria-pressed={active}
                    onClick={() => tags.onToggle(t)}
                    title={manual ? '手动 tag' : '自动 tag'}
                    className="flex min-w-0 flex-1 items-center text-left"
                  >
                    <span
                      className={`truncate rounded-full border px-1.5 text-[11px] leading-[18px] transition-colors duration-150 ease-out ${
                        active
                          ? 'border-primary bg-primary text-primary-fg'
                          : manual
                            ? 'border-border bg-surface text-fg'
                            : 'border-dashed border-border text-muted'
                      }`}
                    >
                      {t}
                    </span>
                  </button>
                  {manual && (
                    <span className="flex shrink-0 items-center opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
                      <button
                        type="button"
                        onClick={() => tags.onRename(t)}
                        aria-label={`重命名 ${t}`}
                        title="改名"
                        className="rounded p-0.5 text-subtle hover:text-fg"
                      >
                        <IconPencil size={11} />
                      </button>
                      <button
                        type="button"
                        onClick={() => tags.onDelete(t)}
                        aria-label={`删除 ${t}`}
                        title="删除"
                        className="rounded p-0.5 text-subtle hover:text-error"
                      >
                        <IconX size={11} />
                      </button>
                    </span>
                  )}
                </div>
              )
            })}
          </div>
        </section>

        {/* AI 任务：没有活跃会话时整段不渲染，不做空装饰 */}
        {live.length > 0 && (
          <section aria-label="AI 任务" className="border-t border-border px-1.5 py-2">
            <h2 className="px-2 pb-1 text-[11px] font-semibold text-subtle">AI 任务</h2>
            <div className="flex flex-col gap-0.5">
              {live.map((s) => (
                <button
                  key={s.session_id}
                  type="button"
                  onClick={() => onOpenAi(s)}
                  className="flex items-center gap-1.5 rounded-[var(--radius-control)] px-2 py-1 text-left text-[12px] text-muted transition-colors hover:bg-surface-raised hover:text-fg"
                >
                  <span
                    className={`h-1.5 w-1.5 shrink-0 rounded-full ${
                      s.state === 'awaiting_user' ? 'bg-warning' : 'bg-ai'
                    }`}
                    aria-hidden
                  />
                  <span className="min-w-0 flex-1 truncate">
                    {s.project_name}
                    {s.task ? ` · ${s.task}` : ''}
                  </span>
                </button>
              ))}
            </div>
          </section>
        )}
      </div>

      <div className="shrink-0 border-t border-border p-2">
        <ThemeSegmented />
      </div>
    </aside>
  )
}

/** 主题三档：跟随系统 / 浅色 / 深色，立即生效并持久化。 */
function ThemeSegmented() {
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

  return (
    <Segmented
      value={mode}
      onChange={setMode}
      items={[
        { id: 'auto', label: '自动' },
        { id: 'light', label: '浅色' },
        { id: 'dark', label: '深色' },
      ]}
    />
  )
}
