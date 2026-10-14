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
}
