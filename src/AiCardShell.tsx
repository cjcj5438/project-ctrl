import { Badge, Card } from './components'

/** 卡片状态 → 徽章色调。未知状态一律中性，不猜。 */
const STATE_TONE: Record<string, 'neutral' | 'success' | 'warning' | 'error'> = {
  pending: 'warning',
  auto: 'warning',
  running: 'warning',
  open: 'warning',
  approved: 'success',
  passed: 'success',
  done: 'success',
  adopted: 'success',
  failed: 'error',
  rejected: 'error',
  expired: 'warning',
  rolled_back: 'warning',
  aborted: 'warning',
  undone: 'neutral',
  dismissed: 'neutral',
}

/** 卡片状态的中文文案：卡片自身只带英文状态串。 */
export const STATE_TEXT: Record<string, string> = {
  pending: '待确认',
  auto: '自动修正',
  running: '验证中',
  open: '待处理',
  approved: '已批准',
  passed: '已通过',
  done: '已完成',
  adopted: '已采纳',
  failed: '失败',
  rejected: '已拒绝',
  expired: '已超时',
  rolled_back: '已回滚',
  aborted: '已中止',
  undone: '已撤销',
  dismissed: '已忽略',
}

/** 状态徽章：色调与文案都由这里决定，各卡片不再各写一份。 */
function StateBadge({ state }: { state: string }) {
  return <Badge tone={STATE_TONE[state] ?? 'neutral'}>{STATE_TEXT[state] ?? state}</Badge>
}

/** 卡片外壳：标题行 + 状态徽章，内容由各变体填充。 */
export function Shell({
  icon,
  title,
  state,
  children,
}: {
  icon: React.ReactNode
  title: React.ReactNode
  state?: string
  children: React.ReactNode
}) {
  return (
    <Card className="w-full p-3">
      <div className="flex items-center gap-1.5 text-[12px] font-semibold">
        <span className="text-ai">{icon}</span>
        <span className="min-w-0 flex-1 truncate">{title}</span>
        {state !== undefined && <StateBadge state={state} />}
      </div>
      <div className="mt-2 flex flex-col gap-2">{children}</div>
    </Card>
  )
}

/** 「字段名 + 内容」的一行，名字固定宽度，长内容自动换行。 */
export function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex gap-2 text-[12px]">
      <span className="w-14 shrink-0 text-muted">{label}</span>
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words text-fg">{children}</span>
    </div>
  )
}
