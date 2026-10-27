import { describe, expect, it } from 'vitest'
import { applyBatch, applySnapshot, type LogState } from './logCursor'

const empty: LogState = { lines: [], cursor: null }

describe('applySnapshot', () => {
  it('首个快照接管', () => {
    const st = applySnapshot(empty, { epoch: 1, next: 3, lines: ['a', 'b', 'c'] })
    expect(st.lines).toEqual(['a', 'b', 'c'])
    expect(st.cursor).toEqual({ epoch: 1, next: 3 })
  })

  it('晚到快照（同 epoch 不前进）被忽略', () => {
    // 先喂增量把 next 推到 5，再喂 next=2 的晚到快照（含 Z2a 的空快照）。
    let st = applySnapshot(empty, { epoch: 1, next: 2, lines: ['a', 'b'] })
    st = applyBatch(st, { epoch: 1, base: 2, lines: ['c', 'd', 'e'] })
    expect(st.lines).toEqual(['a', 'b', 'c', 'd', 'e'])
    const late = applySnapshot(st, { epoch: 1, next: 2, lines: ['a', 'b'] })
    expect(late).toBe(st)
    const emptyLate = applySnapshot(st, { epoch: 1, next: 0, lines: [] })
    expect(emptyLate).toBe(st)
  })

  it('旧 epoch 快照被忽略', () => {
    const st = applySnapshot(empty, { epoch: 2, next: 1, lines: ['x'] })
    expect(applySnapshot(st, { epoch: 1, next: 9, lines: ['old'] })).toBe(st)
  })

  it('同 epoch 前进的快照覆盖接管', () => {
    const st = applySnapshot(empty, { epoch: 1, next: 1, lines: ['a'] })
    const next = applySnapshot(st, { epoch: 1, next: 3, lines: ['a', 'b', 'c'] })
    expect(next.lines).toEqual(['a', 'b', 'c'])
    expect(next.cursor).toEqual({ epoch: 1, next: 3 })
  })
})

describe('applyBatch', () => {
  it('重叠后缀去重（D1）', () => {
    // 快照 [L1..L5]，增量批 base=3 [L4,L5,L6] → 只追加 L6。
    let st = applySnapshot(empty, { epoch: 1, next: 5, lines: ['L1', 'L2', 'L3', 'L4', 'L5'] })
    st = applyBatch(st, { epoch: 1, base: 3, lines: ['L4', 'L5', 'L6'] })
    expect(st.lines).toEqual(['L1', 'L2', 'L3', 'L4', 'L5', 'L6'])
    expect(st.cursor).toEqual({ epoch: 1, next: 6 })
    // 全重复批：忽略。
    expect(applyBatch(st, { epoch: 1, base: 3, lines: ['L4', 'L5', 'L6'] })).toBe(st)
  })

  it('旧 epoch 批丢弃（D2 remount 双重放）', () => {
    const st = applySnapshot(empty, { epoch: 2, next: 2, lines: ['n1', 'n2'] })
    expect(applyBatch(st, { epoch: 1, base: 0, lines: ['n1', 'n2'] })).toBe(st)
  })

  it('新 epoch 批替换', () => {
    const st = applySnapshot(empty, { epoch: 1, next: 2, lines: ['o1', 'o2'] })
    const next = applyBatch(st, { epoch: 2, base: 0, lines: ['n1'] })
    expect(next.lines).toEqual(['n1'])
    expect(next.cursor).toEqual({ epoch: 2, next: 1 })
  })

  it('无游标时先收下增量', () => {
    const st = applyBatch(empty, { epoch: 1, base: 0, lines: ['a'] })
    expect(st.lines).toEqual(['a'])
    expect(st.cursor).toEqual({ epoch: 1, next: 1 })
  })
})
