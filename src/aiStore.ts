/** AI 转录合并：快照 + 增量按 rev 对账，不重不漏（与后端 `Log` 的 rev 语义一致）。 */

import type { AiItem, AiSnapshot, ItemsPayload } from './api'

export interface AiState {
  /** 已应用到的最大 rev；0 表示还没见过任何东西。 */
  rev: number
  items: AiItem[]
}

/** 与后端 `ITEM_CAP` 对齐：超出即丢最旧的。 */
const CAP = 400

export function initialAi(): AiState {
  return { rev: 0, items: [] }
}

/**
 * 快照：不前进的快照一律忽略（晚到的 `ai_open` / 补缝请求常比事件旧），
 * 否则全量接管并裁到 CAP。
 */
export function applySnapshot(s: AiState, snap: AiSnapshot): AiState {
  if (snap.rev <= s.rev) return s
  return { rev: snap.rev, items: snap.items.slice(-CAP) }
}

/**
 * 增量批：`to_rev` 不前进则丢弃；`from_rev` 比本地 rev 大说明中间漏了批（gap），
 * 调用方据此重拉快照；否则按 id 覆盖（已有条目原地替换，新条目按到达顺序追加）。
 */
export function applyBatch(s: AiState, p: ItemsPayload): { state: AiState; gap: boolean } {
  if (p.to_rev <= s.rev) return { state: s, gap: false }
  if (p.from_rev > s.rev) return { state: s, gap: true }

  const items = s.items.slice()
  const index = new Map<string, number>()
  items.forEach((it, i) => index.set(it.id, i))
  for (const it of p.items) {
    const at = index.get(it.id)
    if (at === undefined) {
      index.set(it.id, items.length)
      items.push(it)
    } else {
      // 乱序/重复送达：同样以 rev 大的为准，避免用旧正文覆盖流式更新的条目
      if (items[at].rev <= it.rev) items[at] = it
    }
  }
  return { state: { rev: p.to_rev, items: items.slice(-CAP) }, gap: false }
}
