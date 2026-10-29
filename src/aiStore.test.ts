import { describe, expect, it } from 'vitest'
import type { AiItem, AiSnapshot, ItemsPayload } from './api'
import { applyBatch, applySnapshot, initialAi, type AiState } from './aiStore'

const user = (id: string, rev: number, text: string): AiItem => ({
  id,
  rev,
  body: { type: 'user', text, task: null },
})

const snap = (rev: number, items: AiItem[]): AiSnapshot => ({
  session: {
    session_id: 's1',
    project_id: 1,
    project_name: 'demo',
    state: 'idle',
    task: null,
    model: null,
    awaiting_user: false,
    last_active: 0,
  },
  rev,
  items,
})

const batch = (from: number, to: number, items: AiItem[]): ItemsPayload => ({
  session_id: 's1',
  project_id: 1,
  from_rev: from,
  to_rev: to,
  items,
})

describe('applySnapshot', () => {
  it('首个快照接管', () => {
    const st = applySnapshot(initialAi(), snap(3, [user('a', 1, '你好')]))
    expect(st.rev).toBe(3)
    expect(st.items.map((i) => i.id)).toEqual(['a'])
  })

  it('不前进的晚到快照被忽略', () => {
    let st = applySnapshot(initialAi(), snap(2, [user('a', 1, 'a'), user('b', 2, 'b')]))
    st = applyBatch(st, batch(2, 5, [user('c', 5, 'c')])).state
    // 事件已经把 rev 推过 2：晚到的补缝快照不能把 c 抹掉
    const late = applySnapshot(st, snap(2, [user('a', 1, 'a')]))
    expect(late).toBe(st)
    expect(late.items.map((i) => i.id)).toEqual(['a', 'b', 'c'])
  })

  it('前进的快照全量接管', () => {
    let st = applySnapshot(initialAi(), snap(1, [user('a', 1, 'a')]))
    st = applySnapshot(st, snap(4, [user('a', 1, 'a'), user('b', 4, 'b')]))
    expect(st.rev).toBe(4)
    expect(st.items.map((i) => i.id)).toEqual(['a', 'b'])
  })
})

describe('applyBatch', () => {
  it('旧批（to_rev 不前进）被忽略', () => {
    const st: AiState = { rev: 5, items: [user('a', 5, 'a')] }
    const got = applyBatch(st, batch(0, 5, [user('a', 5, 'a')]))
    expect(got.gap).toBe(false)
    expect(got.state).toBe(st)
  })

  it('重叠批幂等（同 id 不重复追加）', () => {
    let st = applySnapshot(initialAi(), snap(2, [user('a', 1, 'a'), user('b', 2, 'b')]))
    const once = applyBatch(st, batch(2, 4, [user('c', 3, 'c'), user('d', 4, 'd')]))
    expect(once.gap).toBe(false)
    st = once.state
    expect(st.items.map((i) => i.id)).toEqual(['a', 'b', 'c', 'd'])
    // 同一批重放：rev 已到 4，整体丢弃
    expect(applyBatch(st, batch(2, 4, [user('c', 3, 'c'), user('d', 4, 'd')])).state).toBe(st)
  })

  it('同一 id 的后续批原地更新（顺序不变）', () => {
    let st = applySnapshot(initialAi(), snap(2, [user('a', 1, 'a'), user('b', 2, 'b')]))
    st = applyBatch(st, batch(2, 5, [{ id: 'a', rev: 5, body: { type: 'assistant', text: '改', thinking: '', streaming: false } }])).state
    expect(st.items.map((i) => i.id)).toEqual(['a', 'b'])
    expect(st.items[0].rev).toBe(5)
    expect(st.items[0].body.type).toBe('assistant')
  })

  it('from_rev 跳空 → gap', () => {
    const st = applySnapshot(initialAi(), snap(2, [user('a', 1, 'a')]))
    const got = applyBatch(st, batch(5, 6, [user('x', 6, 'x')]))
    expect(got.gap).toBe(true)
    expect(got.state).toBe(st)
  })

  it('首批 from_rev=0 正常接收（不误判 gap）', () => {
    let st = initialAi()
    const first = applyBatch(st, batch(0, 3, [user('a', 1, 'a'), user('b', 3, 'b')]))
    expect(first.gap).toBe(false)
    st = first.state
    expect(st.rev).toBe(3)
  })

  it('超过 400 条丢最旧的', () => {
    let st = initialAi()
    for (let i = 1; i <= 500; i += 1) {
      const got = applyBatch(st, batch(i - 1, i, [user(`i${i}`, i, String(i))]))
      expect(got.gap).toBe(false)
      st = got.state
    }
    expect(st.items).toHaveLength(400)
    expect(st.items[0].id).toBe('i101')
    expect(st.items[399].id).toBe('i500')
  })

  it('快照超长同样裁到 400', () => {
    const many = Array.from({ length: 450 }, (_, n) => user(`s${n}`, n + 1, 'x'))
    const st = applySnapshot(initialAi(), snap(450, many))
    expect(st.items).toHaveLength(400)
    expect(st.items[0].id).toBe('s50')
  })
})
