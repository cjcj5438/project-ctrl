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
