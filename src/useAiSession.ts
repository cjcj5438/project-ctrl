import { useCallback, useEffect, useRef, useState } from 'react'
import {
  api,
  errorText,
  type AiCardAction,
  type AiItem,
  type AiSessionView,
  type AiTask,
  type ItemsPayload,
  type ModelRef,
} from './api'
import { applyBatch, applySnapshot, initialAi, type AiState } from './aiStore'
import { subscribe } from './util'

/** 检测到 gap 后延迟重拉快照的时长：把连续几批合并成一次请求。 */
const GAP_DEBOUNCE_MS = 150

export interface AiSessionApi {
  state: AiSessionView | null
  items: AiItem[]
  error: string | null
  send: (text: string) => Promise<void>
  retry: () => Promise<void>
  abort: () => Promise<void>
  /** 「新对话」：关掉当前会话再开一个空会话（同一项目）。 */
  close: () => Promise<void>
  setModel: (model: ModelRef | null, thinking: string | null) => Promise<void>
  runTask: (task: AiTask) => Promise<void>
  cardAction: (cardId: string, action: AiCardAction) => Promise<void>
}

/**
 * 一个项目的 AI 会话：`ai_open` 取快照 → 订阅 `ai:items` / `ai:session`。
 *
 * 事件流是有缝的（listen 生效前的事件收不到），所以 `subscribe` 的 onReady 里
 * 补拉一次 `ai_snapshot`；增量批出现 gap 时也一样重拉。两者都由 rev 对账，
 * 晚到的快照不会覆盖更新的本地状态（见 `aiStore`）。
 */
export function useAiSession(projectId: number | null): AiSessionApi {
  const [store, setStore] = useState<AiState>(initialAi)
  const [session, setSession] = useState<AiSessionView | null>(null)
  const [error, setError] = useState<string | null>(null)

  /** 当前会话 id：所有回调都从这里取，闭包里不会留旧 id。 */
  const sidRef = useRef<string | null>(null)
  /** 状态的权威副本：事件处理里同步读，不依赖渲染时机。 */
  const storeRef = useRef<AiState>(store)
  const gapTimer = useRef<number | null>(null)
  /** 「新对话」用：自增即让下面的 effect 重建会话与订阅。 */
  const [epoch, setEpoch] = useState(0)
  /** 打开会话的 promise：面板挂载即发任务时，先等它落定再发 IPC。 */
  const readyRef = useRef<Promise<void> | null>(null)

  const commit = useCallback((s: AiState) => {
    if (s === storeRef.current) return
    storeRef.current = s
    setStore(s)
  }, [])

  const refetch = useCallback(async () => {
    const sid = sidRef.current
    if (!sid) return
    try {
      const snap = await api.aiSnapshot(sid)
      // 期间又换了会话：这份快照属于旧会话，丢弃
      if (sidRef.current !== sid) return
      commit(applySnapshot(storeRef.current, snap))
      setSession(snap.session)
    } catch (e) {
      setError(errorText(e))
    }
  }, [commit])

  const scheduleRefetch = useCallback(() => {
    clearTimeout(gapTimer.current ?? undefined)
    gapTimer.current = window.setTimeout(() => {
      gapTimer.current = null
      void refetch()
    }, GAP_DEBOUNCE_MS)
  }, [refetch])

  useEffect(() => {
    return () => clearTimeout(gapTimer.current ?? undefined)
  }, [])

  useEffect(() => {
    let alive = true
    let unItems: (() => void) | null = null
    let unSession: (() => void) | null = null

    sidRef.current = null
    storeRef.current = initialAi()
    setStore(storeRef.current)
    setSession(null)
    setError(null)

    if (projectId === null) {
      readyRef.current = null
      return () => {
        alive = false
      }
    }

    readyRef.current = (async () => {
      try {
        const snap = await api.aiOpen(projectId)
        if (!alive) return
        const sid = snap.session.session_id
        sidRef.current = sid
        commit(applySnapshot(initialAi(), snap))
        setSession(snap.session)

        unItems = subscribe<ItemsPayload>(
          'ai:items',
          (e) => {
            const p = e.payload
            if (p.session_id !== sidRef.current) return
            const got = applyBatch(storeRef.current, p)
            if (got.gap) {
              scheduleRefetch()
              return
            }
            commit(got.state)
          },
          // 订阅真正生效前的事件已经丢了：补一次快照，rev 对账保证不重不漏
          () => void refetch(),
        )
        unSession = subscribe<AiSessionView>('ai:session', (e) => {
          if (e.payload.session_id === sidRef.current) setSession(e.payload)
        })
      } catch (e) {
        if (alive) setError(errorText(e))
      }
    })()

    return () => {
      alive = false
      readyRef.current = null
      // 卸载后迟到的补缝请求直接空转，不会把状态写到已卸载的组件上
      sidRef.current = null
      unItems?.()
      unSession?.()
    }
  }, [projectId, epoch, commit, refetch, scheduleRefetch])

  /**
   * 统一的调用闸门：先等会话开好（面板挂载即发任务时 aiOpen 还没落定），
   * 没有会话就报错，失败一律落进 `error`，绝不往外抛。
   */
  const call = useCallback(async (fn: (sid: string) => Promise<void>): Promise<void> => {
    if (readyRef.current) await readyRef.current
    const sid = sidRef.current
    if (!sid) {
      setError('AI 会话尚未就绪')
      return
    }
    try {
      await fn(sid)
      setError(null)
    } catch (e) {
      setError(errorText(e))
    }
  }, [])

  const send = useCallback(
    (text: string) => call((sid) => api.aiSend(sid, text)),
    [call],
  )
  const retry = useCallback(() => call((sid) => api.aiRetry(sid)), [call])
  const abort = useCallback(() => call((sid) => api.aiAbort(sid)), [call])
  const setModel = useCallback(
    (model: ModelRef | null, thinking: string | null) =>
      call((sid) => api.aiSetModel(sid, model, thinking)),
    [call],
  )
  const runTask = useCallback(
    (task: AiTask) => call((sid) => api.aiRunTask(sid, task)),
    [call],
  )
  const cardAction = useCallback(
    (cardId: string, action: AiCardAction) =>
      call((sid) => api.aiCardAction(sid, cardId, action)),
    [call],
  )

  /** 新对话：关掉旧会话（后端同时清掉 by_project 映射），再让 effect 开一个新的。 */
  const close = useCallback(async () => {
    const sid = sidRef.current
    if (sid) {
      try {
        await api.aiClose(sid)
      } catch (e) {
        setError(errorText(e))
      }
    }
    sidRef.current = null
    storeRef.current = initialAi()
    setStore(storeRef.current)
    setSession(null)
    setEpoch((n) => n + 1)
  }, [])

  return {
    state: session,
    items: store.items,
    error,
    send,
    retry,
    abort,
    close,
    setModel,
    runTask,
    cardAction,
  }
}
