import { describe, expect, it } from 'vitest'
import type { AiSessionView } from './api'
import { liveSessions, mergeSession } from './aiSessions'

const view = (
  session_id: string,
  state: string,
  last_active: number,
  task: string | null = null,
): AiSessionView => ({
  session_id,
  project_id: 1,
  project_name: 'demo',
  state,
  task,
  model: null,
  awaiting_user: false,
  last_active,
})

describe('mergeSession', () => {
  it('新会话追加到末尾', () => {
    const list = [view('a', 'busy', 1)]
    const got = mergeSession(list, view('b', 'busy', 2))
    expect(got.map((s) => s.session_id)).toEqual(['a', 'b'])
    // 不改动入参
    expect(list.map((s) => s.session_id)).toEqual(['a'])
  })

  it('已有会话原地替换（顺序不变）', () => {
    const list = [view('a', 'busy', 1), view('b', 'busy', 2), view('c', 'idle', 3)]
    const got = mergeSession(list, view('b', 'awaiting_user', 9, '诊断'))
    expect(got.map((s) => s.session_id)).toEqual(['a', 'b', 'c'])
    expect(got[1].state).toBe('awaiting_user')
    expect(got[1].task).toBe('诊断')
    expect(got[1].last_active).toBe(9)
  })
})

describe('liveSessions', () => {
  it('掉 idle / sleeping / failed，只留活跃态', () => {
    const list = [
      view('a', 'busy', 1),
      view('b', 'idle', 5),
      view('c', 'recovering', 2),
      view('d', 'sleeping', 6),
      view('e', 'awaiting_user', 3),
      view('f', 'failed', 7),
    ]
    // 结果按 last_active 倒序：e(3) > c(2) > a(1)
    expect(liveSessions(list).map((s) => s.session_id)).toEqual(['e', 'c', 'a'])
  })

  it('按 last_active 倒序', () => {
    const list = [view('a', 'busy', 1), view('b', 'busy', 30), view('c', 'busy', 10)]
    expect(liveSessions(list).map((s) => s.session_id)).toEqual(['b', 'c', 'a'])
  })
})
