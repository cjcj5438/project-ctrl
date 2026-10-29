import { useMemo } from 'react'
import { api, type ProjectInfo } from './api'
import { Button, IconButton, Input, Menu, Segmented } from './components'
import {
  IconCode,
  IconFolder,
  IconGithub,
  IconPencil,
  IconPlus,
  IconRefresh,
  IconSearch,
  IconServer,
  IconSparkles,
  IconTerminal,
  IconTrash,
  IconWrench,
} from './icons'
import { humanSize } from './format'

export type SortMode = 'created' | 'name' | 'size'

const SORT_ITEMS: { id: SortMode; label: string }[] = [
  { id: 'created', label: '创建时间' },
  { id: 'name', label: '名称' },
  { id: 'size', label: '大小' },
]

/**
 * 项目列表页：头部（搜索 / 排序 / 添加 / 批量操作）+ 卡片网格。
 *
 * 只做「呈现 + 搜索 + 排序」；tag 的 AND 过滤在 App 里（侧栏持有筛选状态），
 * 打开 / 删除 / 重新关联 / 添加等动作全部回调给 App；唯一直接调 api 的是
 * 卡片上的四个「用外部应用打开」，它们没有需要 App 参与的状态。
 */
export default function ProjectList({
  projects,
  running,
  query,
  onQuery,
  sort,
  onSort,
  sizeOf,
  sizeErr,
  onOpen,
  onScan,
  onAddSingle,
  onClone,
  onProbe,
  onImportPm2,
  onBatchProfile,
  onRefresh,
  onEditTags,
  onRelink,
  onRemove,
  onRun,
}: {
  projects: ProjectInfo[]
  running: Set<number>
  query: string
  onQuery: (q: string) => void
  sort: SortMode
  onSort: (s: SortMode) => void
  sizeOf: (p: ProjectInfo) => number | null | undefined
  sizeErr: (p: ProjectInfo) => string | undefined
  onOpen: (id: number) => void
  onScan: () => void
  onAddSingle: () => void
  onClone: () => void
  onProbe: () => void
  onImportPm2: () => void
  onBatchProfile: () => void
  onRefresh: () => void
  onEditTags: (p: ProjectInfo) => void
  onRelink: (p: ProjectInfo) => void
  onRemove: (p: ProjectInfo) => void
  onRun: (fn: () => Promise<void>) => void
}) {
  // 搜索匹配名称 / 摘要 / 技术栈，大小写不敏感；再按选定模式排序
  const visible = useMemo(() => {
    const q = query.trim().toLowerCase()
    const list = q
      ? projects.filter(
          (p) =>
            p.name.toLowerCase().includes(q) ||
            (p.summary ?? '').toLowerCase().includes(q) ||
            p.stack.some((s) => s.toLowerCase().includes(q)),
        )
      : [...projects]
    return list.sort((a, b) => {
      if (sort === 'name') return a.name.localeCompare(b.name, 'zh-CN')
      if (sort === 'size') {
        const sa = sizeOf(a)
        const sb = sizeOf(b)
        // 未算完 / 失败的排在最后，不让占位符挤到有数据的上面
        return (typeof sb === 'number' ? sb : -1) - (typeof sa === 'number' ? sa : -1)
      }
      return (b.created_date ?? '').localeCompare(a.created_date ?? '')
    })
  }, [projects, query, sort, sizeOf])

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex h-[52px] shrink-0 items-center gap-3 border-b border-border px-4">
        <h1 data-tauri-drag-region className="shrink-0 text-[17px] font-semibold">
          项目
        </h1>
        <div className="relative shrink-0">
          <span className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-subtle">
            <IconSearch size={13} />
          </span>
          <Input
            value={query}
            onChange={(e) => onQuery(e.target.value)}
            placeholder="搜索名称 / 摘要 / 技术栈…"
            aria-label="搜索项目"
            className="w-56 pl-7"
          />
        </div>
        <Segmented items={SORT_ITEMS} value={sort} onChange={onSort} />
        {/* 空白处才拖动窗口：输入框与按钮都不带这个属性 */}
        <div data-tauri-drag-region className="h-full min-w-4 flex-1" />
        <span className="tabular shrink-0 text-[11px] text-muted">
          共 {visible.length} 个项目
        </span>
        <div className="flex shrink-0 items-center gap-1.5">
          <Menu
            align="end"
            trigger={
              <Button variant="primary" size="sm">
                <IconPlus size={13} />
                添加项目
              </Button>
            }
            items={[
              { label: '扫描文件夹…', icon: <IconFolder size={13} />, onSelect: onScan },
              { label: '添加单个项目…', icon: <IconPlus size={13} />, onSelect: onAddSingle },
              { label: '从 GitHub 克隆…', icon: <IconGithub size={13} />, onSelect: onClone },
              { label: '导入 PM2 配置…', icon: <IconServer size={13} />, onSelect: onImportPm2 },
            ]}
          />
          <IconButton label="批量探测" onClick={onProbe}>
            <IconWrench size={14} />
          </IconButton>
          <IconButton label="AI 批量画像" onClick={onBatchProfile}>
            <IconSparkles size={14} />
          </IconButton>
          <IconButton label="刷新" onClick={onRefresh}>
            <IconRefresh size={14} />
          </IconButton>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        {visible.length === 0 ? (
          <div className="mt-6 rounded-[var(--radius-card)] border border-dashed border-border p-8 text-center text-[13px] text-muted">
            {projects.length === 0
              ? '还没有项目。用右上角「添加项目」扫描文件夹、添加单个目录，或从 GitHub 克隆。'
              : '没有匹配的项目。清空搜索或侧栏的 tag 筛选。'}
          </div>
        ) : (
          <div className="grid gap-3 [grid-template-columns:repeat(auto-fill,minmax(280px,1fr))]">
            {visible.map((p) => (
              <ProjectCard
                key={p.id}
                project={p}
                running={running.has(p.id)}
                size={sizeOf(p)}
                sizeError={sizeErr(p)}
                onOpen={() => onOpen(p.id)}
                onEditTags={() => onEditTags(p)}
                onRelink={() => onRelink(p)}
                onRemove={() => onRemove(p)}
                onRun={onRun}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  )
}

/** 单张项目卡：整卡可点（Enter/Space 同效），动作行独立于卡片点击。 */
function ProjectCard({
  project,
  running,
  size,
  sizeError,
  onOpen,
  onEditTags,
  onRelink,
  onRemove,
  onRun,
}: {
  project: ProjectInfo
  running: boolean
  size: number | null | undefined
  sizeError: string | undefined
  onOpen: () => void
  onEditTags: () => void
  onRelink: () => void
  onRemove: () => void
  onRun: (fn: () => Promise<void>) => void
}) {
  const { id, name, summary, stack, manual_tags: manual, auto_tags: auto } = project
  // 技术栈只放 4 个，其余折成 +N，避免卡片高度被撑开
  const stackShown = stack.slice(0, 4)
  const stackMore = stack.length - stackShown.length
  const tags = [...manual, ...auto.filter((t) => !manual.includes(t))]
  const sizeText = size === undefined ? '计算中…' : size === null ? '—' : humanSize(size)

  return (
    <div
      role="button"
      tabIndex={0}
      aria-label={`打开项目 ${name}${running ? '（服务运行中）' : ''}`}
      onClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          onOpen()
        }
      }}
      className="group cursor-pointer rounded-[var(--radius-card)] border border-border bg-surface-raised p-3 transition-colors hover:border-border-strong"
    >
      <div className="flex items-center gap-2">
        <h2 className="min-w-0 flex-1 truncate text-[13px] font-semibold">{name}</h2>
        {running && (
          <span className="h-2 w-2 shrink-0 rounded-full bg-success" title="服务运行中" aria-hidden />
        )}
        {project.missing && (
          <span className="shrink-0 rounded border border-error/40 bg-error/10 px-1.5 py-0.5 text-[11px] text-error">
            路径丢失
          </span>
        )}
      </div>

      <p
        className={`mt-1 line-clamp-2 min-h-[2.4em] text-[12px] leading-snug ${
          summary ? 'text-muted' : 'text-subtle/70'
        }`}
        title={summary ?? undefined}
      >
        {summary || '暂无描述'}
      </p>

      <div className="mt-2 flex flex-wrap items-center gap-1">
        {stackShown.map((s) => (
          <span
            key={s}
            className="rounded border border-border bg-surface px-1.5 py-0.5 font-mono text-[10px] text-muted"
          >
            {s}
          </span>
        ))}
        {stackMore > 0 && <span className="text-[10px] text-subtle">+{stackMore}</span>}
        {tags.map((t) => (
          <span
            key={t}
            className={
              manual.includes(t)
                ? 'rounded-full border border-success/40 bg-success/10 px-1.5 py-0.5 text-[10px] text-fg'
                : 'rounded-full border border-dashed border-border px-1.5 py-0.5 text-[10px] text-muted'
            }
            title={manual.includes(t) ? '手动 tag' : '自动 tag（只读）'}
          >
            {t}
          </span>
        ))}
      </div>

      <div className="mt-2 flex items-center gap-1.5 text-[11px] text-muted">
        <span className="tabular truncate">{project.created_date ?? '未知'}</span>
        {!project.missing && (
          <>
            <span className="text-subtle">·</span>
            <span className="tabular shrink-0" title={sizeError ? `大小计算失败：${sizeError}` : undefined}>
              {sizeText}
            </span>
          </>
        )}
        {project.missing && (
          <span className="min-w-0 flex-1 truncate font-mono text-[10px] text-subtle" title={project.path}>
            {project.path}
          </span>
        )}
      </div>

      {/* 动作行独立成行并预留高度：悬停显现，不推挤上面的元信息、不产生跳变 */}
      <div
        className={`mt-1 flex h-6 items-center gap-0.5 transition-opacity ${
          project.missing ? '' : 'opacity-0 group-hover:opacity-100 group-focus-within:opacity-100'
        }`}
        onClick={(e) => e.stopPropagation()}
      >
        {project.missing ? (
          <>
            <Button size="sm" variant="primary" onClick={onRelink}>
              重新关联
            </Button>
            <Button size="sm" onClick={onRemove} aria-label={`移除 ${name}`}>
              <IconTrash size={12} />
              移除
            </Button>
          </>
        ) : (
          <>
            <IconButton
              label="在 Finder 中显示"
              className="h-6 w-6"
              onClick={() => onRun(() => api.openInFinder(id))}
            >
              <IconFolder size={13} />
            </IconButton>
            <IconButton
              label="在 iTerm 中打开"
              className="h-6 w-6"
              onClick={() => onRun(() => api.openInIterm(id))}
            >
              <IconTerminal size={13} />
            </IconButton>
            <IconButton
              label="在 VS Code 中打开"
              className="h-6 w-6"
              onClick={() => onRun(() => api.openInVscode(id))}
            >
              <IconCode size={13} />
            </IconButton>
            {project.github_url && (
              <IconButton
                label="打开 GitHub 仓库"
                className="h-6 w-6"
                onClick={() => onRun(() => api.openGithub(id))}
              >
                <IconGithub size={13} />
              </IconButton>
            )}
            <IconButton label="编辑 tag" className="h-6 w-6" onClick={onEditTags}>
              <IconPencil size={13} />
            </IconButton>
          </>
        )}
      </div>
    </div>
  )
}
