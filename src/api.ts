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

export type Phase = 'Idle' | 'Installing' | 'Building' | 'Running' | 'Stopped' | 'Crashed' | 'Failed'

export interface ServiceStatus {
  project_id: number
  phase: Phase
  port: number | null
  pid: number | null
  exit_code: number | null
  note: string | null
}

export interface ServiceConfig {
  install: string[] | null
  build: string[] | null
  start: string[]
  dev_port: number | null
  requires_ports: number[]
  env: Record<string, string>
  node_pin: string | null
  toolchain: string[]
  origin: string
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
  project_id: number
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

  serviceConfig: (id: number) => invoke<ServiceConfig | null>('service_config', { id }),
  saveServiceConfig: (id: number, config: ServiceConfig) =>
    invoke<void>('save_service_config', { id, config }),
  deleteServiceConfig: (id: number) => invoke<void>('delete_service_config', { id }),
  detectService: (id: number) => invoke<DetectResult>('detect_service', { id }),
  serviceStatus: () => invoke<ServiceStatus[]>('service_status'),
  serviceLogs: (id: number) => invoke<string[]>('service_logs', { id }),
  servicePreflight: (id: number, step: 'install' | 'build' | 'start') =>
    invoke<string[]>('service_preflight', { id, step }),
  serviceStart: (id: number) => invoke<ServiceStatus>('service_start', { id }),
  serviceStop: (id: number) => invoke<void>('service_stop', { id }),
  serviceStep: (id: number, step: 'install' | 'build') => invoke<void>('service_step', { id, step }),
  openServiceUrl: (id: number) => invoke<void>('open_service_url', { id }),

  sourceInfo: (id: number) => invoke<SourceInfo>('source_info', { id }),
  sourceFetch: (id: number) => invoke<GitResult>('source_fetch', { id }),
  sourcePull: (id: number) => invoke<GitResult>('source_pull', { id }),
  cloneRepo: (url: string, parentDir: string) => invoke<number>('clone_repo', { url, parentDir }),
  importPm2: () => invoke<ImportReport>('import_pm2'),
}
