/** 日志游标合并（Part3a）：快照 + 增量按 (epoch, seq) 对账，不重不漏。 */

export interface LogSnapshot {
  epoch: number
  next: number
  lines: string[]
}

export interface LogBatch {
  epoch: number
  base: number
  lines: string[]
}

export interface LogCursor {
  epoch: number
  next: number
}

export interface LogState {
  lines: string[]
  cursor: LogCursor | null
}

const CAP = 2000

/** 快照：旧 epoch / 同 epoch 不前进的晚到快照一律忽略；否则全量接管。 */
export function applySnapshot(
  state: LogState,
  snap: LogSnapshot,
): LogState {
  const cur = state.cursor
  if (cur) {
    if (snap.epoch < cur.epoch) return state
    if (snap.epoch === cur.epoch && snap.next <= cur.next) return state
  }
  return { lines: snap.lines.slice(-CAP), cursor: { epoch: snap.epoch, next: snap.next } }
}

/** 增量批：旧 epoch 丢弃；新 epoch 替换；同 epoch 只追加 seq > cursor.next 的后缀。 */
export function applyBatch(state: LogState, batch: LogBatch): LogState {
  const cur = state.cursor
  if (!cur) {
    // 尚未见过快照：先收下本批（行序号从 base 起连续），等快照/since 对账。
    return {
      lines: batch.lines.slice(-CAP),
      cursor: { epoch: batch.epoch, next: batch.base + batch.lines.length },
    }
  }
  if (batch.epoch < cur.epoch) return state
  if (batch.epoch > cur.epoch) {
    return {
      lines: batch.lines.slice(-CAP),
      cursor: { epoch: batch.epoch, next: batch.base + batch.lines.length },
    }
  }
  const skip = Math.max(0, cur.next - batch.base)
  if (skip >= batch.lines.length) return state
  const fresh = batch.lines.slice(skip)
  return {
    lines: [...state.lines, ...fresh].slice(-CAP),
    cursor: { epoch: cur.epoch, next: batch.base + batch.lines.length },
  }
}
