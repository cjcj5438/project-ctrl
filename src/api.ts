import { invoke } from '@tauri-apps/api/core'

export interface ProjectInfo {
  id: number
  name: string
  path: string
  created_date: string | null
  missing: boolean
  manual_tags: string[]
  auto_tags: string[]
  github_url: string | null
  has_git: boolean
  /** AI 生成的项目描述；没跑过画像时为 null。 */
  summary: string | null
  /** AI 识别的技术栈；没跑过画像时为空数组。 */
  stack: string[]
}

export interface ScanCandidate {
  name: string
  path: string
}

export interface ReadmeInfo {
  exists: boolean
  filename: string | null
  content: string | null
  mtime: string | null
}

/** save_readme 的错误：kind==='conflict' 时磁盘 mtime 已变。 */
export type ReadmeError = { kind: 'conflict'; current_mtime: string } | { kind: 'message'; message: string }

export interface GitResult {
  text: string
  ok: boolean
  code: number | null
  timed_out: boolean
  truncated: boolean
}

export function isConflict(e: unknown): e is { kind: 'conflict'; current_mtime: string } {
  return typeof e === 'object' && e !== null && 'kind' in e && e.kind === 'conflict'
}

export type Phase = 'Idle' | 'Step' | 'Running' | 'Stopped' | 'Crashed' | 'Failed'

export interface ServiceStatus {
  /** `"{project_id}/{name}"`，精确匹配用。 */
  key: string
  project_id: number
  name: string
  phase: Phase
  /** 仅 phase==='Step' 时指出当前步骤名。 */
  step: string | null
  port: number | null
  pid: number | null
  exit_code: number | null
  note: string | null
  /** 本次运行已自动重启的次数。 */
  restarts: number
  /** 本次运行（不含重启）起始 unix 秒。 */
  started_at: number
}

export type Restart = 'never' | 'on-failure' | 'always'

export interface Step {
  name: string
  argv: string[]
  timeout_s: number
}

export interface Health {
  type: 'port' | 'http'
  path?: string | null
  timeout_ms?: number | null
}

export interface Ports {
  listen?: number | null
  requires?: number[]
}

export interface DockerSpec {
  files: string[]
  project_name?: string | null
  services: string[]
}

/** 服务声明：仓库内 `.project-ctrl.json` 与数据库共用同一份结构。 */
export interface ServiceSpec {
  name: string
  cwd?: string | null
  run?: string[] | null
  steps?: Step[]
  env_files?: string[]
  env?: Record<string, string>
  ports?: Ports
  health?: Health | null
  depends_on?: string[]
  restart?: Restart
  docker?: DockerSpec | null
}

export interface ServiceView {
  spec: ServiceSpec
  /** `descriptor` | `db` */
  layer: string
  origin: string
  keep_alive: boolean
  port_offset: number
  status: ServiceStatus | null
  warnings: string[]
}

export interface ProjectServices {
  services: ServiceView[]
  /** 仓库内有描述文件但解析失败时的错误（UI 必须显示）。 */
  descriptor_error: string | null
  descriptor_exists: boolean
  detected: DetectResult
}

/**
 * set_service_override 的 patch 形状（三态语义，与后端 OverridePatch 对齐）：
 * 外层 null = 删整条服务覆盖；字段缺席 = 不变；字段 null = 删该键；
 * env 逐键三态（缺席不变/null删键/有值设置），env_files 整体三态（缺席不变/null清空/数组替换）。
 */
export interface ServiceOverride {
  port_offset?: number | null
  keep_alive?: boolean | null
  restart?: Restart | null
  env?: Record<string, string | null>
  env_files?: string[] | null
}

export interface ComposeCandidate {
  file: string
  services: string[]
  ports: number[]
  overrides: string[]
}

export interface DetectResult {
  package_manager: string | null
  install: string[] | null
  build: string[] | null
  start_candidates: string[][]
  dev_port: number | null
  requires_ports: number[]
  node_pin: string | null
  toolchain: string[]
  problems: string[]
  compose_stacks: ComposeCandidate[]
  make_targets: string[]
  script_candidates: string[]
}

export interface ProbeCandidate {
  kind: 'script' | 'compose' | 'make' | 'script-file'
  label: string
  spec: ServiceSpec
}

export interface ProbeItem {
  project_id: number
  project_name: string
  path: string
  has_descriptor: boolean
  has_services: boolean
  candidates: ProbeCandidate[]
}

export interface ExternalEntry {
  source: 'pm2' | 'launchd'
  name: string
  cwd: string | null
  managed_project: string | null
  declared: boolean
  note: string
  config_path: string
  command: string[] | null
  changed_at?: number | null
}

export interface ExternalItem extends ExternalEntry {
  /** 与本 App 里同名声明的命令不一致时的说明。 */
  drift: { kind: string; detail: string } | null
}

export interface ContainerView {
  name: string
  service: string
  state: string
  status: string
  ports: string
}

export interface SourceInfo {
  is_repo: boolean
  branch: string | null
  detached: boolean
  upstream: string | null
  dirty: number
  ahead: number
  behind: number
  can_fetch: boolean
  can_pull: boolean
  pull_blocked_by: string | null
}

export interface ImportReport {
  imported: number[]
  skipped: string[]
  errors: string[]
}

/** 事件 payload（与服务端 `LogPayload` 一致）。 */
export interface LogPayload {
  key: string
  project_id: number
  name: string
  epoch: number
  base: number
  lines: string[]
}

/** 快照（与服务端 `LogSnapshot` 一致）。 */
export interface LogSnapshot {
  epoch: number
  next: number
  lines: string[]
}

/* ------------------------------------------------------------------------- *
 * AI（与 src-tauri/src/ai_*.rs 的 serde 形状逐字对齐）
 * ------------------------------------------------------------------------- */

/** 模型引用：provider 与 id 一起才唯一。 */
export interface ModelRef {
  provider: string
  id: string
}

/** `~/.project-ctrl/ai/settings.json` 的整份内容。 */
export interface AiSettings {
  pi_path: string | null
  main_model: ModelRef | null
  main_thinking: string
  light_model: ModelRef | null
  light_thinking: string
}

/** `ai_models` 的一项。 */
export interface AiModel {
  provider: string
  id: string
  name: string
  reasoning: boolean
  context_window: number
}

/** `ai_status`：pi 没装/版本过低都走 `problem`，命令本身永不报错。 */
export interface AiEnv {
  pi_path: string | null
  pi_version: string | null
  /** `resolve_pi` 的原始错误文本（直接展示）。 */
  problem: string | null
  settings: AiSettings
  live_processes: number
}

/** 会话视图（`state ∈ idle|starting|busy|recovering|sleeping|failed`）。 */
export interface AiSessionView {
  session_id: string
  project_id: number
  project_name: string
  state: string
  task: string | null
  /** 展示用 `provider/id`。 */
  model: string | null
  awaiting_user: boolean
  last_active: number
}

/** 条目正文：`type` 是 serde 的 tag。 */
export type AiItemBody =
  | { type: 'user'; text: string; task: string | null }
  | { type: 'assistant'; text: string; thinking: string; streaming: boolean }
  /** `status ∈ running|ok|error|blocked`。 */
  | { type: 'tool'; name: string; summary: string; status: string; detail: string }
  | { type: 'card'; card: AiCard }
  /** `level ∈ info|warn|error`。 */
  | { type: 'notice'; level: string; text: string; retry: boolean }

/** 一条对话条目；`rev` 单调递增，前端按它去重/补缝。 */
export interface AiItem {
  id: string
  rev: number
  body: AiItemBody
}

/** `ai:items` 事件载荷。 */
export interface ItemsPayload {
  session_id: string
  project_id: number
  from_rev: number
  to_rev: number
  items: AiItem[]
}

/** `ai_open` / `ai_snapshot`：视图 + 当前最大 rev + 全量条目。 */
export interface AiSnapshot {
  session: AiSessionView
  rev: number
  items: AiItem[]
}

/** 完整任务：`kind` 是 serde 的 tag。 */
export type AiTask =
  | { kind: 'configure' }
  | { kind: 'diagnose'; service: string }
  | { kind: 'health' }
  | { kind: 'profile' }
  | { kind: 'readme' }

/** 卡片动作：`action` 是 serde 的 tag。 */
export type AiCardAction =
  | { action: 'approve'; run_steps: boolean }
  | { action: 'reject'; feedback: string | null }
  | { action: 'undo' }
  | { action: 'stop_services' }
  | { action: 'adopt_last' }
  | { action: 'apply_fix'; run_steps: boolean }
  | { action: 'reverify' }
  | { action: 'continue_diagnose' }
  | { action: 'adopt_profile'; summary: boolean; stack: boolean; tags: string[] }
  | { action: 'dismiss' }
  | { action: 'allow' }
  | { action: 'deny' }

/** 校验循环里单个服务的检查结果。 */
export interface AiServiceCheck {
  name: string
  ok: boolean
  phase: string
  port: number | null
  message: string
}

/** 诊断证据。 */
export interface AiEvidence {
  file: string | null
  line: number | null
  excerpt: string
}

/** 体检条目（`severity ∈ error|warn|info`）。 */
export interface AiHealthItem {
  severity: string
  title: string
  detail: string
  command: string | null
}

/**
 * 七种卡片：`kind` 是 serde 的 tag。
 *
 * 状态取值各自独立：proposal 的 `state` 是 `pending|auto|approved|rejected|expired`，
 * verify 的是 `running|passed|failed|rolled_back|undone|aborted`，
 * diagnosis 的是 `open|passed|failed`，profile/readme/service_control 的见后端。
 */
export type AiCard =
  | {
      kind: 'proposal'
      id: string
      attempt: number
      /** `db` | `descriptor`。 */
      layer: string
      before: string
      after: string
      env_keys: string[]
      run_steps_default: boolean
      notes: string
      state: string
    }
  | {
      kind: 'verify'
      id: string
      attempt: number
      state: string
      services: AiServiceCheck[]
      can_undo: boolean
      can_adopt_last: boolean
    }
  | {
      kind: 'diagnosis'
      id: string
      service: string
      summary: string
      cause: string
      /** `config|code|env|external|unknown`。 */
      category: string
      evidence: AiEvidence[]
      suggestion: string
      /** 有修复方案时后端会给完整集合（前端只判断存在性）。 */
      config_fix: ServiceSpec[] | null
      rounds: number
      state: string
    }
  | { kind: 'health'; id: string; summary: string; items: AiHealthItem[] }
  | {
      kind: 'profile'
      id: string
      summary: string
      stack: string[]
      tags: string[]
      state: string
    }
  | { kind: 'readme'; id: string; content: string; state: string }
  | {
      kind: 'service_control'
      id: string
      service: string
      /** `start|stop|restart`。 */
      action: string
      reason: string
      state: string
      result: string | null
    }
  /** 会话侧补的 onboarding 卡（不是 `Card` 枚举成员，形状见 ai_session.rs）。 */
  | { kind: 'onboarding'; id: string; problem: string }

/** `ai:batch` 载荷的一项（`ai:batch` 是**完整**数组）。 */
export interface BatchItem {
  project_id: number
  name: string
  /** `queued|running|done|failed|adopted|dismissed`。 */
  state: string
  profile: AiBatchProfile | null
  error: string | null
}

/** 批量画像结果：后端归一后严格是这三个字段。 */
export interface AiBatchProfile {
  summary: string
  stack: string[]
  tags: string[]
}

export function errorText(e: unknown): string {
  if (typeof e === 'string') return e
  if (typeof e === 'object' && e !== null && 'message' in e) return String(e.message)
  return String(e)
}

export const api = {
  listProjects: () => invoke<ProjectInfo[]>('list_projects'),
  listTags: () => invoke<string[]>('list_tags'),
  // 仅 debug 构建可能返回旁路目录（dev-pick-dir 控制文件）；release 恒为 null，走原生弹窗。
  devPickDir: () => invoke<string | null>('dev_pick_dir'),
  scanDirectory: (root: string) => invoke<ScanCandidate[]>('scan_directory', { root }),
  addProjects: (paths: string[], tags: string[]) => invoke<number[]>('add_projects', { paths, tags }),
  setProjectTags: (id: number, tags: string[]) => invoke<void>('set_project_tags', { id, tags }),
  setProjectProfile: (id: number, summary: string | null, stack: string[]) =>
    invoke<void>('set_project_profile', { id, summary, stack }),
  openInVscodeAt: (id: number, relPath: string, line: number | null) =>
    invoke<void>('open_in_vscode_at', { id, relPath, line }),
  renameTag: (oldName: string, newName: string) => invoke<void>('rename_tag', { oldName, newName }),
  deleteTag: (name: string) => invoke<void>('delete_tag', { name }),
  relinkProject: (id: number, newPath: string) => invoke<void>('relink_project', { id, newPath }),
  removeProject: (id: number) => invoke<void>('remove_project', { id }),
  openInFinder: (id: number) => invoke<void>('open_in_finder', { id }),
  openInIterm: (id: number) => invoke<void>('open_in_iterm', { id }),
  openInVscode: (id: number) => invoke<void>('open_in_vscode', { id }),
  openGithub: (id: number) => invoke<void>('open_github', { id }),
  readReadme: (id: number) => invoke<ReadmeInfo>('read_readme', { id }),
  saveReadme: (id: number, content: string, expectedMtime: string | null, force: boolean) =>
    invoke<string>('save_readme', { id, content, expectedMtime, force }),
  generateReadme: (id: number) => invoke<ReadmeInfo>('generate_readme', { id }),
  readProjectImage: (id: number, relPath: string) =>
    invoke<string>('read_project_image', { id, relPath }),
  openExternal: (url: string) => invoke<void>('open_external', { url }),
  runGit: (id: number, kind: 'status' | 'log') => invoke<GitResult>('run_git', { id, kind }),
  projectSize: (id: number) => invoke<number>('project_size', { id }),

  projectServices: (id: number) => invoke<ProjectServices>('project_services', { id }),
  saveService: (id: number, spec: ServiceSpec) => invoke<void>('save_service', { id, spec }),
  deleteService: (id: number, name: string) => invoke<void>('delete_service', { id, name }),
  solidifyDescriptor: (id: number, names: string[], overwrite: boolean, confirmEnv: boolean) =>
    invoke<string>('solidify_descriptor', { id, names, overwrite, confirm: confirmEnv }),
  readDescriptor: (id: number) => invoke<string | null>('read_descriptor', { id }),
  serviceStatus: () => invoke<ServiceStatus[]>('service_status'),
  serviceLogs: (id: number, name: string) => invoke<LogSnapshot>('service_logs', { id, name }),
  serviceLogsSince: (id: number, name: string, epoch: number, cursor: number) =>
    invoke<LogSnapshot>('service_logs_since', { id, name, epoch, cursor }),
  servicePreflight: (id: number, name: string, step?: string) =>
    invoke<string[]>('service_preflight', { id, name, step: step ?? null }),
  serviceStart: (id: number, name: string) => invoke<ServiceStatus>('service_start', { id, name }),
  serviceStop: (id: number, name: string) => invoke<void>('service_stop', { id, name }),
  serviceStopAll: (id: number) => invoke<void>('service_stop_all', { id }),
  serviceStep: (id: number, name: string, step: string) =>
    invoke<void>('service_step', { id, name, step }),
  openServiceUrl: (id: number, name: string) => invoke<void>('open_service_url', { id, name }),
  serviceOverride: (id: number, name: string) =>
    invoke<ServiceOverride | null>('service_override', { id, name }),
  setServiceOverride: (id: number, name: string, patch: ServiceOverride | null) =>
    invoke<void>('set_service_override', { id, name, patch }),
  dockerUp: (id: number, name: string) => invoke<string>('docker_up', { id, name }),
  dockerDown: (id: number, name: string) => invoke<string>('docker_down', { id, name }),
  dockerPs: (id: number, name: string) => invoke<ContainerView[]>('docker_ps', { id, name }),
  dockerLogs: (id: number, name: string) => invoke<number>('docker_logs', { id, name }),
  dockerLogsStop: (id: number, name: string) => invoke<void>('docker_logs_stop', { id, name }),
  probeAll: () => invoke<ProbeItem[]>('probe_all'),
  adoptCandidates: (items: [number, ServiceSpec][]) =>
    invoke<string[]>('adopt_candidates', { items }),
  runAdhoc: (id: number, spec: ServiceSpec) => invoke<ServiceStatus>('run_adhoc', { id, spec }),
  externalScan: () => invoke<ExternalItem[]>('external_scan'),
  revealFile: (path: string) => invoke<void>('reveal_file', { path }),

  sourceInfo: (id: number) => invoke<SourceInfo>('source_info', { id }),
  sourceFetch: (id: number) => invoke<GitResult>('source_fetch', { id }),
  sourcePull: (id: number) => invoke<GitResult>('source_pull', { id }),
  cloneRepo: (url: string, parentDir: string) => invoke<number>('clone_repo', { url, parentDir }),
  importPm2: () => invoke<ImportReport>('import_pm2'),

  // AI：设置 / 模型 / 会话 / 任务 / 批量画像
  aiStatus: () => invoke<AiEnv>('ai_status'),
  aiModels: (refresh: boolean) => invoke<AiModel[]>('ai_models', { refresh }),
  aiSaveSettings: (settings: AiSettings) => invoke<void>('ai_save_settings', { settings }),
  aiOpen: (id: number) => invoke<AiSnapshot>('ai_open', { id }),
  aiSnapshot: (sessionId: string) => invoke<AiSnapshot>('ai_snapshot', { sessionId }),
  aiSessions: () => invoke<AiSessionView[]>('ai_sessions'),
  aiSend: (sessionId: string, text: string) => invoke<void>('ai_send', { sessionId, text }),
  aiRetry: (sessionId: string) => invoke<void>('ai_retry', { sessionId }),
  aiAbort: (sessionId: string) => invoke<void>('ai_abort', { sessionId }),
  aiClose: (sessionId: string) => invoke<void>('ai_close', { sessionId }),
  aiSetModel: (sessionId: string, model: ModelRef | null, thinking: string | null) =>
    invoke<void>('ai_set_model', { sessionId, model, thinking }),
  aiRunTask: (sessionId: string, task: AiTask) => invoke<void>('ai_run_task', { sessionId, task }),
  aiCardAction: (sessionId: string, cardId: string, action: AiCardAction) =>
    invoke<void>('ai_card_action', { sessionId, cardId, action }),
  aiBatchProfile: (ids: number[]) => invoke<void>('ai_batch_profile', { ids }),
  aiBatchStatus: () => invoke<BatchItem[]>('ai_batch_status'),
  aiBatchAdopt: (projectId: number, summary: boolean, stack: boolean, tags: string[]) =>
    invoke<void>('ai_batch_adopt', { projectId, summary, stack, tags }),
  aiBatchDismiss: (projectId: number) => invoke<void>('ai_batch_dismiss', { projectId }),
  aiBatchCancel: () => invoke<void>('ai_batch_cancel'),
}
