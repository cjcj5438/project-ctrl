import type { AiTask } from './api'

/** AI 任务：形状定义在 `api.ts`（与后端 serde 对齐），路由只转发。 */
export type { AiTask }

/** 项目详情内的页签。 */
export type DetailTab = 'overview' | 'services' | 'source' | 'readme'

/**
 * 应用路由：不用 react-router（无依赖的必要性），状态留在 App。
 *
 * `ai` 是留给 AI 抽屉的挂载点：`task` 说明要做什么，`nonce` 保证同一任务
 * 重复触发时也能被消费方察觉（值变了就是新请求）；`open` 表示只打开抽屉
 * （侧栏点已有会话），不发任务。
 */
export type AiIntent = { task: AiTask; nonce: number } | { open: true; nonce: number }

export type Route =
  | { view: 'projects' }
  | { view: 'services' }
  | { view: 'external' }
  | { view: 'settings' }
  | { view: 'project'; id: number; tab: DetailTab; ai?: AiIntent }
