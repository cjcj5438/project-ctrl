import { listen, type EventCallback, type EventName, type UnlistenFn } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'
import { api } from './api'

/** 选目录：dev 自动化验证旁路（debug 构建读 dev-pick-dir 控制文件）；release 恒走原生弹窗。 */
export async function pickDirectory(): Promise<string | null> {
  const bypass = await api.devPickDir()
  if (bypass) return bypass
  const sel = await open({ directory: true, multiple: false })
  if (!sel) return null
  return Array.isArray(sel) ? (sel[0] ?? null) : sel
}

/** 逗号/顿号/空格分隔的 tag 输入 -> 去重后的 tag 列表。 */
export function splitTags(s: string): string[] {
  return [...new Set(s.split(/[,，、\s]+/).map((t) => t.trim()).filter(Boolean))]
}

/**
 * 竞速安全的事件订阅：在 effect cleanup 里调用返回的函数即可。
 *
 * `listen()` 是异步的——直接 `void listen(...).then((f) => un.push(f))` 时，
 * cleanup 可能在 then 落定前执行，迟到的 unlisten 就永远没人调用
 * （快开快关卡片 / StrictMode remount 时监听常驻累积）。
 * 这里用 cleaned 标志关门：迟到即直接调用，保证不泄漏。
 */
export function subscribe<T>(
  event: EventName,
  handler: EventCallback<T>,
  onReady?: () => void,
): () => void {
  let unlisten: UnlistenFn | null = null
  let cleaned = false
  void listen<T>(event, handler).then((f) => {
    if (cleaned) {
      f()
    } else {
      unlisten = f
      // 订阅真正生效后回调：调用方在此补缝（since 拉取），杀订阅缝隙丢行（Z2b）。
      onReady?.()
    }
  })
  return () => {
    cleaned = true
    if (unlisten) unlisten()
  }
}
