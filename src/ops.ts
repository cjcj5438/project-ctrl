import type { ServiceStatus } from './api'

/**
 * 运维条的取数与排序：抽成纯函数，便于在没有 UI 的情况下断言。
 *
 * 排序是「异常优先」——崩溃的服务必须排在运行中的前面，否则一眼扫过去会漏掉。
 * 只保留有意义的相位：`Stopped`/`Idle` 不进运维条（那是历史，不是现状）。
 */
const RANK: Record<string, number> = {
  Crashed: 0,
  Failed: 0,
  Running: 1,
  Step: 2,
}

export function opsRows(statuses: Record<string, ServiceStatus>): ServiceStatus[] {
  return Object.values(statuses)
    .filter((st) => st.phase in RANK)
    .sort((a, b) => {
      const d = RANK[a.phase] - RANK[b.phase]
      if (d !== 0) return d
      // 同相位按「项目 + 服务名」稳定排序，避免每次刷新顺序抖动
      return a.project_id - b.project_id || a.name.localeCompare(b.name)
    })
}

/** 异常计数（崩溃 + 失败）。 */
export function opsBadCount(rows: ServiceStatus[]): number {
  return rows.filter((r) => r.phase === 'Crashed' || r.phase === 'Failed').length
}
