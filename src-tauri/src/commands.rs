//! Tauri commands：组合 db + logic + 文件系统读取。

use crate::{db, git, logic, readme, size};
use crate::readme::ReadmeError;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct ProjectInfo {
    id: i64,
    name: String,
    path: String,
    created_date: Option<String>,
    missing: bool,
    manual_tags: Vec<String>,
    auto_tags: Vec<String>,
    github_url: Option<String>,
    has_git: bool,
}

#[derive(Serialize)]
pub struct ScanCandidate {
    name: String,
    path: String,
}

/// 读取目录 birthtime -> `YYYY-MM-DD`（本地日期）。`std::fs::Metadata::created()`
/// 在 macOS 上即文件系统 birthtime。平台不支持或拷贝/恢复导致读取失败时为 None。
pub fn created_date_of(path: &Path) -> Option<String> {
    let created = std::fs::metadata(path).ok()?.created().ok()?;
    let secs = created.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(logic::format_date_ymd(secs))
}

/// `.git` 状态：None=无；Some(true)=目录；Some(false)=文件（worktree/submodule）
pub fn git_kind_of(project: &Path) -> Option<bool> {
    let g = project.join(".git");
    let ft = std::fs::symlink_metadata(&g).ok()?.file_type();
    if ft.is_dir() {
        Some(true)
    } else {
        Some(false)
    }
}

/// 读 `.git/config` 文本；仅当 `.git` 是目录时才有 config（`.git` 为文件时返回 None）。
pub fn git_config_text_of(project: &Path) -> Option<String> {
    if git_kind_of(project) == Some(true) {
        std::fs::read_to_string(project.join(".git").join("config")).ok()
    } else {
        None
    }
}

/// 由项目路径现算 GitHub 仓库页 URL（无 .git / .git 为文件 / 非 github remote 时为 None）。
/// 与自动 tag `github` 共用 `logic::pick_github_url`，保证两者判断一致。
pub fn github_url_of(project: &Path) -> Option<String> {
    let config = git_config_text_of(project)?;
    logic::pick_github_url(&logic::extract_remote_urls(&config))
}

pub fn auto_tags_of(project: &Path) -> Vec<String> {
    logic::auto_tags(git_kind_of(project), git_config_text_of(project).as_deref())
}

#[tauri::command]
pub fn list_projects() -> Result<Vec<ProjectInfo>, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let rows = db::all_projects(&conn).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let p = PathBuf::from(&r.path);
        let missing = !p.exists();
        let manual_tags = db::tags_of(&conn, r.id).map_err(|e| e.to_string())?;
        let (auto_tags, github_url, has_git) = if missing {
            (vec![], None, false)
        } else {
            (auto_tags_of(&p), github_url_of(&p), git_kind_of(&p).is_some())
        };
        out.push(ProjectInfo {
            name: logic::project_name_of(&p),
            created_date: if missing { None } else { created_date_of(&p) },
            missing,
            manual_tags,
            auto_tags,
            github_url,
            has_git,
            id: r.id,
            path: r.path,
        });
    }
    // 默认创建时间倒序（缺失/未知沉底），同值按名称
    out.sort_by(|a, b| {
        b.created_date.cmp(&a.created_date).then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

#[tauri::command]
pub fn list_tags() -> Result<Vec<String>, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::all_tags(&conn).map_err(|e| e.to_string())
}

/// 仅 debug 构建生效的选目录旁路（自动化验证用）：读取数据目录下 `dev-pick-dir` 文件内容。
/// release 构建恒返回 None，正式行为不受影响。
#[tauri::command]
pub fn dev_pick_dir() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        if let Ok(s) = std::fs::read_to_string(db::data_dir().join("dev-pick-dir")) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

#[tauri::command]
pub fn scan_directory(root: String) -> Result<Vec<ScanCandidate>, String> {
    let root = PathBuf::from(root);
    let rd = std::fs::read_dir(&root).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((name, path, is_dir));
    }
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let already: HashSet<String> =
        db::all_project_paths(&conn).map_err(|e| e.to_string())?.into_iter().collect();
    Ok(logic::scan_candidates(entries, &already)
        .into_iter()
        .map(|(name, path)| ScanCandidate { name, path })
        .collect())
}

#[tauri::command]
pub fn add_projects(paths: Vec<String>, tags: Vec<String>) -> Result<Vec<i64>, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    for p in &paths {
        if !Path::new(p).exists() {
            return Err(format!("路径不存在：{p}"));
        }
        let id = db::add_project(&conn, p).map_err(|e| e.to_string())?;
        db::set_project_tags(&conn, id, &tags).map_err(|e| e.to_string())?;
        ids.push(id);
    }
    Ok(ids)
}

#[tauri::command]
pub fn set_project_tags(id: i64, tags: Vec<String>) -> Result<(), String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::set_project_tags(&conn, id, &tags).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rename_tag(old_name: String, new_name: String) -> Result<(), String> {
    let new_name = new_name.trim().to_string();
    if new_name.is_empty() {
        return Err("tag 名不能为空".to_string());
    }
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::rename_tag(&conn, &old_name, &new_name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_tag(name: String) -> Result<(), String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::delete_tag(&conn, &name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn relink_project(id: i64, new_path: String) -> Result<(), String> {
    if !Path::new(&new_path).exists() {
        return Err("新路径不存在".to_string());
    }
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::update_project_path(&conn, id, &new_path).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_project(id: i64) -> Result<(), String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::remove_project(&conn, id).map_err(|e| e.to_string())
}

/// 按 id 从库里取项目路径（命令不接受前端传入的路径，杜绝任意路径）。
fn project_path(id: i64) -> Result<PathBuf, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    match db::project_path(&conn, id).map_err(|e| e.to_string())? {
        Some(p) => Ok(PathBuf::from(p)),
        None => Err("项目不存在".to_string()),
    }
}

/// 以参数数组调用外部程序（绝不拼 shell 字符串）。失败时返回 stderr。
fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("无法启动 {program}：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let msg = stderr.trim();
    Err(if msg.is_empty() {
        format!("{program} 执行失败（退出码 {:?}）", output.status.code())
    } else {
        msg.to_string()
    })
}

#[tauri::command]
pub fn open_in_finder(id: i64) -> Result<(), String> {
    let path = project_path(id)?;
    run_command("open", &[&path.to_string_lossy()])
}

#[tauri::command]
pub fn open_in_vscode(id: i64) -> Result<(), String> {
    let path = project_path(id)?;
    run_command("open", &["-a", "Visual Studio Code", &path.to_string_lossy()])
}

#[tauri::command]
pub fn open_in_iterm(id: i64) -> Result<(), String> {
    let path = project_path(id)?;
    run_command("osascript", &["-e", logic::ITERM_SCRIPT, "--", &path.to_string_lossy()])
}

#[tauri::command]
pub fn open_github(id: i64) -> Result<(), String> {
    let path = project_path(id)?;
    let url = github_url_of(&path).ok_or_else(|| "该项目未识别到 GitHub remote".to_string())?;
    run_command("open", &[&url])
}

fn project_path_err(id: i64) -> Result<PathBuf, ReadmeError> {
    project_path(id).map_err(|m| ReadmeError::Message { message: m })
}

#[tauri::command]
pub fn read_readme(id: i64) -> Result<readme::ReadmeInfo, String> {
    let base = project_path(id)?;
    let name = readme::find_readme(&base).map_err(|e| e.to_string())?;
    readme::read_info(&base, name).map_err(ReadmeError::into_message)
}

#[tauri::command]
pub fn save_readme(
    id: i64,
    content: String,
    expected_mtime: Option<String>,
    force: bool,
) -> Result<String, ReadmeError> {
    let base = project_path_err(id)?;
    let name = readme::find_readme(&base)
        .map_err(|e| ReadmeError::Message { message: e.to_string() })?
        .ok_or_else(|| ReadmeError::Message { message: "该项目没有 README 文件".to_string() })?;
    readme::save(&base, &name, &content, expected_mtime.as_deref(), force)
}

#[tauri::command]
pub fn generate_readme(id: i64) -> Result<readme::ReadmeInfo, ReadmeError> {
    let base = project_path_err(id)?;
    let title = logic::project_name_of(&base);
    let date = created_date_of(&base).unwrap_or_else(logic::today_ymd);
    readme::generate(&base, &title, &date)
}

#[tauri::command]
pub fn read_project_image(id: i64, rel_path: String) -> Result<String, String> {
    let base = project_path(id)?;
    readme::read_image_data_url(&base, &rel_path).map_err(ReadmeError::into_message)
}

#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    if !readme::is_allowed_external(&url) {
        return Err(format!("不允许打开的链接：{url}"));
    }
    run_command("open", &[&url])
}

/// 统计项目磁盘占用（字节）。路径由 id 从库取；计算放到阻塞线程池，不占用 IPC/主线程。
#[tauri::command]
pub async fn project_size(id: i64) -> Result<u64, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let base = project_path(id)?;
        Ok::<u64, String>(size::dir_size(&base))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 只读执行 `git status` / `git log`（kind 走白名单，绝不接受任意子命令）。
#[tauri::command]
pub fn run_git(id: i64, kind: String) -> Result<git::GitResult, String> {
    let base = project_path(id)?;
    let kind = git::GitKind::parse(&kind).ok_or_else(|| format!("不支持的 git 命令：{kind}"))?;
    if git_kind_of(&base).is_none() {
        return Err("该项目没有 .git".to_string());
    }
    Ok(git::run(&base, kind))
}
