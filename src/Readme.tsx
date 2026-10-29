import { useEffect, useMemo, useRef, useState } from 'react'
import DOMPurify from 'dompurify'
import { marked } from 'marked'
import { EditorView, basicSetup } from 'codemirror'
import { EditorState } from '@codemirror/state'
import { markdown, markdownLanguage } from '@codemirror/lang-markdown'
import { api, errorText, isConflict, type ReadmeInfo } from './api'
import { DiffView } from './DiffView'

const EXTERNAL_LINK = /^(https?:|mailto:)/i

/** Markdown -> 清洗后的安全 HTML。DOMPurify 去掉 script、iframe、事件属性、javascript: 链接等。 */
export function renderMarkdown(md: string): string {
  const raw = marked.parse(md, { async: false }) as string
  return DOMPurify.sanitize(raw, {
    FORBID_TAGS: ['script', 'style', 'iframe', 'object', 'embed', 'form', 'input', 'button', 'link', 'meta', 'base'],
    FORBID_ATTR: ['srcdoc'],
  })
}

/** 渲染 Markdown：点击链接走 open_external（其余不跳转）；相对路径图片换成 data URL。 */
function MarkdownView({ id, markdown: md }: { id: number; markdown: string }) {
  const ref = useRef<HTMLDivElement>(null)
  const html = useMemo(() => renderMarkdown(md), [md])

  useEffect(() => {
    const el = ref.current
    if (!el) return
    let cancelled = false
    el.querySelectorAll('img').forEach((img) => {
      const src = img.getAttribute('src') ?? ''
      if (!src || /^(https?:|data:|blob:)/i.test(src)) return
      api
        .readProjectImage(id, src)
        .then((dataUrl) => {
          if (!cancelled) img.setAttribute('src', dataUrl)
        })
        .catch(() => {
          if (cancelled) return
          img.removeAttribute('src')
          img.setAttribute('alt', img.getAttribute('alt') || '图片无法加载')
        })
    })
    return () => {
      cancelled = true
    }
  }, [html, id])

  const onClick = (e: React.MouseEvent) => {
    const anchor = (e.target as HTMLElement).closest('a')
    if (!anchor) return
    e.preventDefault()
    const href = anchor.getAttribute('href') ?? ''
    // 外链打开失败必须可见：用户点了却没反应是最糟的体验
    if (EXTERNAL_LINK.test(href)) api.openExternal(href).catch((err) => console.warn('打开外链失败', href, errorText(err)))
  }

  return <div ref={ref} className="md-body" onClick={onClick} dangerouslySetInnerHTML={{ __html: html }} />
}

/** 一个带「取消 / 确认」的应用内模态框。 */
function Confirm({
  title,
  children,
  confirmText,
  danger,
  onConfirm,
  onCancel,
}: {
  title: string
  children: React.ReactNode
  confirmText: string
  danger?: boolean
  onConfirm: () => void
  onCancel: () => void
}) {
  return (
    <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim p-6">
      <div className="w-full max-w-sm rounded-xl bg-surface p-5 ">
        <h3 className="font-semibold">{title}</h3>
        <div className="mt-2 text-sm">{children}</div>
        <div className="mt-4 flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-lg border border-border px-3 py-1.5 text-sm "
          >
            取消
          </button>
          <button
            type="button"
            onClick={onConfirm}
            className={`rounded-lg px-3 py-1.5 text-sm hover:brightness-95 ${
              danger ? 'bg-error-solid text-on-solid' : 'bg-primary text-primary-fg'
            }`}
          >
            {confirmText}
          </button>
        </div>
      </div>
    </div>
  )
}

/** README 区域：渲染 / 编辑 / 生成 / 冲突处理。 */
export default function ReadmeSection({
  projectId,
  onDirtyChange,
  onError,
  aiDraft,
  onAiDraftApplied,
}: {
  projectId: number
  onDirtyChange: (dirty: boolean) => void
  onError: (msg: string | null) => void
  /** AI 生成的草稿：nonce 变化即灌进编辑器（有未保存修改时先走确认流程）。 */
  aiDraft?: { content: string; nonce: number }
  /** 草稿已灌入：父组件据此清掉它，切页签再回来不会把用户后续编辑顶掉。 */
  onAiDraftApplied: () => void
}) {
  const [info, setInfo] = useState<ReadmeInfo | null>(null)
  const [loading, setLoading] = useState(true)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')
  const [savedContent, setSavedContent] = useState('')
  const [mtime, setMtime] = useState<string | null>(null)
  const [conflict, setConflict] = useState(false)
  const [confirmDiscard, setConfirmDiscard] = useState<null | (() => void)>(null)
  const [aiBanner, setAiBanner] = useState(false)
  const [showAiDiff, setShowAiDiff] = useState(false)
  const appliedNonce = useRef<number | null>(null)
  /** 已挂到确认框上、但用户还没决定的 AI 草稿（取消后可重来）。 */
  const armedNonce = useRef<number | null>(null)

  const editorRef = useRef<HTMLDivElement>(null)
  const viewRef = useRef<EditorView | null>(null)

  const dirty = editing && draft !== savedContent

  useEffect(() => {
    onDirtyChange(dirty)
  }, [dirty, onDirtyChange])

  const apply = (next: ReadmeInfo) => {
    setInfo(next)
    setSavedContent(next.content ?? '')
    setMtime(next.mtime)
  }

  useEffect(() => {
    let alive = true
    setLoading(true)
    api
      .readReadme(projectId)
      .then((r) => {
        if (alive) apply(r)
      })
      .catch((e) => {
        if (alive) onError(errorText(e))
      })
      .finally(() => {
        if (alive) setLoading(false)
      })
    return () => {
      alive = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectId])

  // CodeMirror 实例（编辑态）
  useEffect(() => {
    if (!editing || !editorRef.current) return
    const view = new EditorView({
      parent: editorRef.current,
      state: EditorState.create({
        doc: draft,
        extensions: [
          basicSetup,
          markdown({ base: markdownLanguage }),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) setDraft(u.state.doc.toString())
          }),
        ],
      }),
    })
    viewRef.current = view
    return () => {
      view.destroy()
      viewRef.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editing])

  // 外部改动 draft（如冲突后重新载入）时，把 CodeMirror 文档同步过来；
  // 正常输入时两者一致，不会触发替换。
  useEffect(() => {
    const view = viewRef.current
    if (!editing || !view) return
    const cur = view.state.doc.toString()
    if (cur !== draft) view.dispatch({ changes: { from: 0, to: cur.length, insert: draft } })
  }, [draft, editing])

  // 最新 save 引用（避免 Cmd+S 闭包捕获过期 draft/mtime）
  const saveRef = useRef<(force?: boolean) => Promise<void>>(async () => {})

  // Cmd+S 保存：window 级 keydown，preventDefault 阻止 WebView「保存网页」
  useEffect(() => {
    if (!editing) return
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && (e.key === 's' || e.key === 'S')) {
        e.preventDefault()
        void saveRef.current()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [editing])

  // AI 草稿：只在 nonce 变化时应用一次；脏编辑器先确认（同一套 confirmDiscard）
  useEffect(() => {
    if (!aiDraft) return
    if (appliedNonce.current === aiDraft.nonce || armedNonce.current === aiDraft.nonce) return
    const content = aiDraft.content
    const applyDraft = () => {
      appliedNonce.current = aiDraft.nonce
      armedNonce.current = null
      setDraft(content)
      setEditing(true)
      setAiBanner(true)
      setShowAiDiff(false)
      onError(null)
      onAiDraftApplied()
    }
    if (editing && draft !== savedContent) {
      // 脏编辑器：先问一句。用户取消时回到原状，什么都不丢
      armedNonce.current = aiDraft.nonce
      setConfirmDiscard(() => applyDraft)
    } else {
      applyDraft()
    }
  }, [aiDraft, editing, draft, savedContent, onError, onAiDraftApplied])

  const save = async (force = false) => {
    try {
      const newMtime = await api.saveReadme(projectId, draft, mtime, force)
      setMtime(newMtime)
      setSavedContent(draft)
      setInfo((i) => (i ? { ...i, content: draft, mtime: newMtime } : i))
      setConflict(false)
      // 存盘后草稿就是正文了：AI 提示与改动对照都该收起
      setAiBanner(false)
      setShowAiDiff(false)
      onError(null)
    } catch (e) {
      if (isConflict(e)) {
        setConflict(true)
      } else {
        onError(errorText(e))
      }
    }
  }
  saveRef.current = save

  const startEdit = () => {
    setDraft(savedContent)
    setEditing(true)
    onError(null)
  }

  const exitEdit = () => {
    setEditing(false)
    setConflict(false)
  }

  const requestExitEdit = () => {
    if (dirty) setConfirmDiscard(() => exitEdit)
    else exitEdit()
  }

  const generate = async () => {
    try {
      const r = await api.generateReadme(projectId)
      apply(r)
      setDraft(r.content ?? '')
      setEditing(true)
      onError(null)
    } catch (e) {
      onError(errorText(e))
    }
  }

  const reloadFromDisk = async () => {
    const r = await api.readReadme(projectId)
    apply(r)
    setDraft(r.content ?? '')
    setConflict(false)
  }

  if (loading) return <div className="text-sm text-muted">README 载入中…</div>

  return (
    <section aria-label="README">
      <div className="flex items-center gap-3 pb-2">
        <h2 className="text-[13px] font-semibold">README</h2>
        {info?.filename && <span className="font-mono text-[11px] text-muted">{info.filename}</span>}
        {editing && (
          <>
            {dirty && <span className="text-xs text-warning">●未保存</span>}
            <div className="ml-auto flex gap-2">
              <button
                type="button"
                onClick={requestExitEdit}
                className="rounded border border-border px-2 py-1 text-xs "
              >
                取消编辑
              </button>
              <button
                type="button"
                onClick={() => void save()}
                className="rounded bg-primary px-3 py-1 text-xs text-primary-fg hover:brightness-95"
              >
                保存
              </button>
            </div>
          </>
        )}
        {!editing && info?.exists && (
          <button
            type="button"
            onClick={startEdit}
            className="ml-auto rounded border border-border px-2 py-1 text-xs "
          >
            编辑
          </button>
        )}
      </div>

      {aiBanner && editing && (
        <div className="mb-2 flex items-center gap-2 rounded-[var(--radius-card)] border border-ai/40 bg-ai/10 px-3 py-1.5 text-[12px]">
          <span className="text-ai">AI 草稿（未保存）</span>
          <button
            type="button"
            onClick={() => setShowAiDiff((v) => !v)}
            aria-expanded={showAiDiff}
            className="rounded text-[11px] text-muted underline hover:text-fg"
          >
            {showAiDiff ? '收起改动' : '查看改动'}
          </button>
          <button
            type="button"
            onClick={() => {
              setAiBanner(false)
              setShowAiDiff(false)
            }}
            className="ml-auto rounded text-[11px] text-muted underline hover:text-fg"
            aria-label="收起 AI 草稿提示"
          >
            关闭
          </button>
        </div>
      )}

      {aiBanner && showAiDiff && (
        <div className="mb-2">
          <DiffView before={savedContent} after={draft} />
        </div>
      )}

      {editing ? (
        <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
          <div
            ref={editorRef}
            className="min-h-[320px] overflow-auto rounded-[var(--radius-card)] border border-border bg-surface-sunken"
          />
          <div className="max-h-[70vh] overflow-auto rounded-[var(--radius-card)] border border-border bg-surface p-4 ">
            <MarkdownView id={projectId} markdown={draft} />
          </div>
        </div>
      ) : !info?.exists ? (
        <div className="rounded-[var(--radius-card)] border border-dashed border-border p-6 text-center text-sm text-muted ">
          <p>暂无 README</p>
          <button
            type="button"
            onClick={generate}
            className="mt-3 rounded bg-primary px-3 py-1.5 text-sm text-primary-fg hover:brightness-95"
          >
            生成
          </button>
        </div>
      ) : (
        <div className="rounded-[var(--radius-card)] border border-border bg-surface p-4 ">
          <MarkdownView id={projectId} markdown={savedContent} />
        </div>
      )}

      {conflict && (
        <Confirm
          title="README 已被外部修改"
          confirmText="覆盖保存"
          danger
          onConfirm={() => void save(true)}
          onCancel={() => setConflict(false)}
        >
          <p>文件在你编辑期间被外部修改了。请选择处理方式（不会静默覆盖）。</p>
          <div className="mt-3 flex flex-col gap-2">
            <button
              type="button"
              onClick={() => void reloadFromDisk()}
              className="rounded border border-border px-3 py-1.5 text-sm "
            >
              放弃我的修改并重新载入
            </button>
          </div>
        </Confirm>
      )}

      {confirmDiscard && (
        <Confirm
          title="放弃未保存的修改？"
          confirmText="放弃修改"
          danger
          onConfirm={() => {
            confirmDiscard()
            setConfirmDiscard(null)
          }}
          onCancel={() => {
            // 用户选择保留现有修改：AI 草稿就此作废（不再重复弹窗）
            appliedNonce.current = armedNonce.current
            armedNonce.current = null
            setConfirmDiscard(null)
          }}
        >
          <p>README 有未保存的修改，放弃后将丢失。</p>
        </Confirm>
      )}
    </section>
  )
}
