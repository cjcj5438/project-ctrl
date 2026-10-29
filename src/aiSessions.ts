/** 侧栏 AI 会话列表的纯逻辑：事件合并与活跃筛选（形状见 `api.ts` 的 `AiSessionView`）。 */

import type { AiSessionView } from './api'

/** 需要出现在侧栏的状态；其余状态（idle / sleeping / failed）不占位。 */
const LIVE: Record<string, true> = { busy: true, recovering: true, awaiting_user: true }

/**
 * 按 `session_id` upsert：新会话追加，已有会话原地替换（顺序不变）。
 * 事件可能乱序到达，但状态字段是快照式的，后到的直接覆盖即可。
 */
export function mergeSession(list: AiSessionView[], next: AiSessionView): AiSessionView[] {
  const at = list.findIndex((s) => s.session_id === next.session_id)
  if (at < 0) return [...list, next]
  const out = list.slice()
  out[at] = next
  return out
}

/** 只留活跃会话，按最后活动时间倒序（最新的在最上面）。 */
export function liveSessions(list: AiSessionView[]): AiSessionView[] {
  return list.filter((s) => LIVE[s.state] === true).sort((a, b) => b.last_active - a.last_active)
}
