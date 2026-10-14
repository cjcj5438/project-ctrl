//! Tauri commands：组合 db + logic + 文件系统读取。

use crate::{db, logic};
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
        let (auto_tags, github_url) = if missing {
            (vec![], None)
        } else {
            (auto_tags_of(&p), github_url_of(&p))
        };
        out.push(ProjectInfo {
            name: logic::project_name_of(&p),
            created_date: if missing { None } else { created_date_of(&p) },
            missing,
            manual_tags,
            auto_tags,
            github_url,
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
