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

/** 本机层的覆盖（只由用户写）。 */
export interface ServiceOverride {
  port_offset?: number | null
  keep_alive?: boolean | null
  restart?: Restart | null
  env?: Record<string, string>
  env_files?: string[]
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
  lines: string[]
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
    invoke<string>('solidify_descriptor', { id, names, overwrite, confirm_env: confirmEnv }),
  readDescriptor: (id: number) => invoke<string | null>('read_descriptor', { id }),
  serviceStatus: () => invoke<ServiceStatus[]>('service_status'),
  serviceLogs: (id: number, name: string) => invoke<string[]>('service_logs', { id, name }),
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
}
