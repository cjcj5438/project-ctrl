import { useMemo } from 'react'
import { diffLines } from 'diff'

/** 一行差异：`add` 新增 / `del` 删除 / `ctx` 未变。 */
type DiffRowKind = 'add' | 'del' | 'ctx'

interface DiffRow {
  kind: DiffRowKind
  text: string
}

/** 渲染上限：超长配方只渲染前这么多行，剩下的用页脚报数。 */
const DIFF_ROW_CAP = 400

/** 按行求差异。 */
function diffRows(before: string, after: string): { rows: DiffRow[]; hidden: number } {
  const rows: DiffRow[] = []
  for (const part of diffLines(before, after)) {
    const kind: DiffRowKind = part.added ? 'add' : part.removed ? 'del' : 'ctx'
    const lines = part.value.split('\n')
    // diffLines 的 value 以 \n 结尾：去掉尾随空串（行内的真空白行保留）
    if (lines.length > 0 && lines[lines.length - 1] === '') lines.pop()
    for (const text of lines) rows.push({ kind, text })
  }
  const hidden = Math.max(0, rows.length - DIFF_ROW_CAP)
  return { rows: hidden > 0 ? rows.slice(0, DIFF_ROW_CAP) : rows, hidden }
}

const ROW_CLASS: Record<DiffRowKind, string> = {
  add: 'bg-success/10 text-success',
  del: 'bg-error/10 text-error',
  ctx: 'text-muted',
}

const ROW_MARK: Record<DiffRowKind, string> = {
  add: '+',
  del: '−',
  ctx: ' ',
}

/** 行级差异视图：新增绿 / 删除红 / 未变弱化，同一套视觉给配方提案与 README 草稿用。 */
export function DiffView({ before, after }: { before: string; after: string }) {
  const { rows, hidden } = useMemo(() => diffRows(before, after), [before, after])

  return (
    <div
      className="overflow-auto rounded-[var(--radius-control)] border border-border bg-surface-sunken"
      role="figure"
      aria-label="差异"
    >
      <div className="w-max min-w-full py-1 font-mono text-[11px] leading-[1.5]">
        {rows.map((r, i) => (
          <div key={i} className={`flex gap-1 px-2 ${ROW_CLASS[r.kind]}`}>
            <span aria-hidden className="w-2 shrink-0 select-none opacity-70">
              {ROW_MARK[r.kind]}
            </span>
            <span className="whitespace-pre">{r.text === '' ? ' ' : r.text}</span>
          </div>
        ))}
        {hidden > 0 && (
          <div className="px-2 py-0.5 text-subtle">…还有 {hidden} 行</div>
        )}
      </div>
    </div>
  )
}
