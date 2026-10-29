import { useState } from 'react'
import { api, errorText, type ImportReport, type ProjectInfo, type ScanCandidate } from './api'
import { Button, Input, Modal } from './components'
import { pickDirectory } from './util'

/** 弹层里统一的错误位：失败原因就地显示，不静默。 */
function ModalError({ text }: { text: string | null }) {
  if (!text) return null
  return (
    <div className="mb-2 rounded border border-error/40 bg-error/10 px-2 py-1.5 text-[12px] break-words text-error">
      {text}
    </div>
  )
}

/**
 * 扫描结果：勾选要加入的目录 + 批量 tag。
 * 勾选状态是本弹层私有的（取消即丢弃），App 只管「扫描」和「加入」两个瞬间。
 */
export function ScanModal({
  root,
  candidates,
  onCancel,
  onAdd,
}: {
  root: string
  candidates: ScanCandidate[]
  onCancel: () => void
  onAdd: (paths: string[], tags: string) => Promise<void>
}) {
  const [checked, setChecked] = useState<Set<string>>(() => new Set(candidates.map((c) => c.path)))
  const [tags, setTags] = useState('')
  const [busy, setBusy] = useState(false)
  const allChecked = candidates.length > 0 && checked.size === candidates.length

  const toggle = (path: string) =>
    setChecked((prev) => {
      const next = new Set(prev)
      if (next.has(path)) next.delete(path)
      else next.add(path)
      return next
    })

  return (
    <Modal
      title={`扫描结果：${root}`}
      onClose={onCancel}
      width="max-w-lg"
      footer={
        <>
          <span className="mr-auto text-[11px] text-muted">已加入管理的不显示；没勾选的不记忆。</span>
          <Button onClick={onCancel} disabled={busy}>
            取消
          </Button>
          <Button
            variant="primary"
            disabled={busy || checked.size === 0}
            onClick={() => {
              setBusy(true)
              void onAdd([...checked], tags).finally(() => setBusy(false))
            }}
          >
            加入（{checked.size}）
          </Button>
        </>
      }
    >
      <label className="flex items-center gap-2 text-[13px]">
        <input
          type="checkbox"
          checked={allChecked}
          onChange={() => setChecked(allChecked ? new Set() : new Set(candidates.map((c) => c.path)))}
        />
        全选（{checked.size}/{candidates.length}）
      </label>
      <div className="mt-2 space-y-1">
        {candidates.map((c) => (
          <label key={c.path} className="flex items-center gap-2 text-[13px]">
            <input type="checkbox" checked={checked.has(c.path)} onChange={() => toggle(c.path)} />
            <span className="shrink-0 font-medium">{c.name}</span>
            <span className="min-w-0 flex-1 truncate text-[11px] text-muted" title={c.path}>
              {c.path}
            </span>
          </label>
        ))}
        {candidates.length === 0 && (
          <div className="py-4 text-[13px] text-muted">没有可加入的新目录。</div>
        )}
      </div>
      <Input
        value={tags}
        onChange={(e) => setTags(e.target.value)}
        placeholder="给所选项目批量打 tag（逗号/空格分隔，可留空）"
        className="mt-3"
      />
    </Modal>
  )
}

/** tag 相关弹层：编辑某项目的 tag / tag 改名 / 删除 tag / 添加单项目。 */
export type TagModalKind =
  | { kind: 'editTags'; project: ProjectInfo }
  | { kind: 'renameTag'; tag: string }
  | { kind: 'deleteTag'; tag: string }
  | { kind: 'singleTags'; path: string }
  | { kind: 'removeProject'; project: ProjectInfo }

export function TagModal({
  modal,
  onSubmit,
  onCancel,
}: {
  modal: TagModalKind
  onSubmit: (input: string) => Promise<void>
  onCancel: () => void
}) {
  const [input, setInput] = useState(() => {
    if (modal.kind === 'editTags') return modal.project.manual_tags.join('，')
    if (modal.kind === 'renameTag') return modal.tag
    return ''
  })
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  const danger = modal.kind === 'deleteTag' || modal.kind === 'removeProject'
  const title =
    modal.kind === 'deleteTag'
      ? '删除 tag'
      : modal.kind === 'removeProject'
        ? '移除项目'
        : modal.kind === 'editTags'
          ? `编辑「${modal.project.name}」的 tag`
          : modal.kind === 'renameTag'
            ? `tag 改名：「${modal.tag}」`
            : '添加单个项目'

  const submit = () => {
    setBusy(true)
    onSubmit(input)
      .catch((e) => setErr(errorText(e)))
      .finally(() => setBusy(false))
  }

  return (
    <Modal
      title={title}
      onClose={onCancel}
      width="max-w-sm"
      footer={
        <>
          <span className="mr-auto" />
          <Button onClick={onCancel} disabled={busy}>
            取消
          </Button>
          <Button variant={danger ? 'danger' : 'primary'} onClick={submit} disabled={busy}>
            {danger ? '确认' : '确定'}
          </Button>
        </>
      }
    >
      <ModalError text={err} />
      {modal.kind === 'deleteTag' ? (
        <p className="text-[13px]">删除 tag「{modal.tag}」？会从所有项目摘掉。</p>
      ) : modal.kind === 'removeProject' ? (
        <p className="text-[13px]">从管理器移除「{modal.project.name}」？只删记录，不动文件。</p>
      ) : (
        <>
          {modal.kind === 'singleTags' && (
            <p className="mb-2 font-mono text-[11px] break-all text-muted">{modal.path}</p>
          )}
          <Input
            autoFocus
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') submit()
            }}
            placeholder="tag（逗号/空格分隔，可留空）"
          />
        </>
      )}
    </Modal>
  )
}

/** 从 GitHub 克隆：克隆完成后自动刷新列表。 */
export function CloneModal({
  onClose,
  onCloned,
}: {
  onClose: () => void
  onCloned: () => Promise<void> | void
}) {
  const [url, setUrl] = useState('')
  const [dir, setDir] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  const clone = async () => {
    if (!dir) {
      setErr('请先选择父目录')
      return
    }
    setBusy(true)
    setErr(null)
    try {
      await api.cloneRepo(url.trim(), dir)
      await onCloned()
      onClose()
    } catch (e) {
      setErr(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Modal
      title="从 GitHub 克隆"
      onClose={() => {
        if (!busy) onClose()
      }}
      width="max-w-md"
      footer={
        <>
          <span className="mr-auto" />
          <Button onClick={onClose} disabled={busy}>
            取消
          </Button>
          <Button variant="primary" onClick={() => void clone()} disabled={busy || !url.trim() || !dir}>
            {busy ? '克隆中…' : '克隆'}
          </Button>
        </>
      }
    >
      <p className="text-[11px] text-muted">支持 https / ssh 地址。克隆完成后自动加入管理器。</p>
      <ModalError text={err} />
      <Input
        autoFocus
        value={url}
        onChange={(e) => setUrl(e.target.value)}
        placeholder="https://github.com/owner/repo.git"
        className="mt-2 font-mono text-[12px]"
      />
      <div className="mt-2 flex items-center gap-2">
        <Button
          size="sm"
          onClick={() =>
            void pickDirectory()
              .then(setDir)
              .catch((e) => setErr(errorText(e)))
          }
          disabled={busy}
        >
          选择父目录…
        </Button>
        <span className="min-w-0 flex-1 truncate text-[11px] text-muted" title={dir ?? ''}>
          {dir ?? '未选择'}
        </span>
      </div>
    </Modal>
  )
}

/** PM2 导入报告：只呈现结果，关闭时刷新列表拿到新配方。 */
export function Pm2ReportModal({
  report,
  onClose,
}: {
  report: ImportReport
  onClose: () => void
}) {
  return (
    <Modal
      title="导入 PM2 配置"
      onClose={onClose}
      width="max-w-md"
      footer={
        <>
          <span className="mr-auto" />
          <Button variant="primary" onClick={onClose}>
            关闭
          </Button>
        </>
      }
    >
      <p className="text-[13px]">新增 {report.imported.length} 个项目的配方。</p>
      <ReportList title="已跳过" items={report.skipped} />
      <ReportList title="问题" items={report.errors} />
    </Modal>
  )
}

/** 导入报告里的一段（空则不渲染）。 */
function ReportList({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null
  return (
    <div className="mt-3">
      <div className="text-[11px] font-semibold text-muted">{title}</div>
      <ul className="mt-1 list-inside list-disc text-[12px]">
        {items.map((t) => (
          <li key={t}>{t}</li>
        ))}
      </ul>
    </div>
  )
}
