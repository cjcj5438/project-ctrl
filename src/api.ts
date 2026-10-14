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

export function isConflict(e: unknown): e is { kind: 'conflict'; current_mtime: string } {
  return typeof e === 'object' && e !== null && 'kind' in e && e.kind === 'conflict'
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
}
