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

// ---------------------------------------------------------------------------
// 服务运行器
// ---------------------------------------------------------------------------

use crate::services::ServiceConfig;
use crate::supervisor::{self, Phase};

#[derive(Serialize)]
pub struct ImportReport {
    /// 新建立配方的项目 id。
    pub imported: Vec<i64>,
    /// 已有配方而跳过 / cwd 不存在的条目（人读说明）。
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

/// 取项目路径并要求目录存在（供所有新增的写/执行类命令使用）。
fn existing_project_path(id: i64) -> Result<PathBuf, String> {
    let base = project_path(id)?;
    if !base.is_dir() {
        return Err("项目路径不存在".to_string());
    }
    Ok(base)
}

fn load_config(conn: &rusqlite::Connection, id: i64) -> Result<ServiceConfig, String> {
    match db::service_config(conn, id).map_err(|e| e.to_string())? {
        Some(json) => ServiceConfig::from_json(&json),
        None => Ok(ServiceConfig::default()),
    }
}

#[tauri::command]
pub fn service_config(id: i64) -> Result<Option<ServiceConfig>, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    match db::service_config(&conn, id).map_err(|e| e.to_string())? {
        Some(json) => ServiceConfig::from_json(&json).map(Some),
        None => Ok(None),
    }
}

#[tauri::command]
pub fn save_service_config(id: i64, config: ServiceConfig) -> Result<(), String> {
    let base = existing_project_path(id)?;
    let mut config = config;
    // 保存时刷新 toolchain，供 preflight 直接读取而不用重扫目录
    let detected = crate::detect::detect(&base);
    config.toolchain = detected.toolchain;
    let json = config.to_json()?;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::set_service_config(&conn, id, &json).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_service_config(id: i64) -> Result<(), String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::delete_service_config(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn detect_service(id: i64) -> Result<crate::detect::DetectResult, String> {
    let base = existing_project_path(id)?;
    Ok(crate::detect::detect(&base))
}

#[tauri::command]
pub fn service_status<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Vec<supervisor::ServiceStatus> {
    supervisor::state(&app).statuses()
}

#[tauri::command]
pub fn service_logs<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> Vec<String> {
    supervisor::state(&app).logs_of(id)
}

/// 空数组 = 可执行；否则每条是「为什么不能点」。
#[tauri::command]
pub fn service_preflight(id: i64, step: String) -> Result<Vec<String>, String> {
    let base = existing_project_path(id)?;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let cfg = load_config(&conn, id)?;
    let argv = match step.as_str() {
        "install" => cfg.install.clone(),
        "build" => cfg.build.clone(),
        "start" => {
            if cfg.start.is_empty() {
                None
            } else {
                Some(cfg.start.clone())
            }
        }
        other => return Err(format!("未知步骤：{other}")),
    };
    let Some(argv) = argv else {
        return Ok(vec![match step.as_str() {
            "install" => "该项目没有安装命令".to_string(),
            "build" => "该项目没有构建命令".to_string(),
            _ => "该项目没有启动命令".to_string(),
        }]);
    };
    let _ = base;
    Ok(preflight_problems(&cfg, &argv))
}

fn preflight_problems(cfg: &ServiceConfig, argv: &[String]) -> Vec<String> {
    let path = crate::execenv::path_for(cfg);
    let mut out = Vec::new();
    if let Some(program) = argv.first() {
        if crate::execenv::resolve(program, &path).is_none() {
            out.push(format!("缺少可执行文件：{program}"));
        }
    }
    for tool in &cfg.toolchain {
        if crate::execenv::resolve(tool, &path).is_none() {
            out.push(format!("缺少可执行文件：{tool}（项目需要它才能构建/运行）"));
        }
    }
    if let Some(pin) = cfg.node_pin.as_deref() {
        if crate::execenv::node_bin_dir(pin).is_none() {
            out.push(format!("缺少 node {pin}（nvm install {pin}）"));
        }
    }
    out.dedup();
    out
}

#[tauri::command]
pub fn service_start<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> Result<supervisor::ServiceStatus, String> {
    let base = existing_project_path(id)?;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let cfg = load_config(&conn, id)?;
    if cfg.start.is_empty() {
        return Err("该项目没有启动命令".to_string());
    }
    if let Some(first) = preflight_problems(&cfg, &cfg.start).into_iter().next() {
        return Err(first);
    }
    let sup = supervisor::state(&app);
    if sup.is_active(id) {
        return Err("该服务已在运行".to_string());
    }

    let mut notes: Vec<String> = Vec::new();
    for p in crate::ports::missing_requires(&cfg.requires_ports) {
        notes.push(format!("端口 {p} 无人监听：先起依赖容器（docker compose up -d）"));
    }

    let mut argv = cfg.start.clone();
    let mut extra_env: Vec<(String, String)> = Vec::new();
    let mut assign_port = None;
    let mut observe = None;
    if let Some(wanted) = cfg.dev_port {
        let pick = crate::ports::pick(wanted).ok_or_else(|| {
            format!(
                "端口 {wanted}–{} 全被占用",
                wanted.saturating_add(crate::ports::PICK_ATTEMPTS - 1)
            )
        })?;
        assign_port = Some(pick.port);
        if pick.shifted {
            let who = crate::ports::holder(wanted).unwrap_or_else(|| "其它进程".to_string());
            notes.push(format!(
                "已改用 {}（{wanted} 被 {who} 占用；项目内硬编码的 {wanted} 未改）",
                pick.port
            ));
        }
        let (planned, env) = crate::ports::plan_argv(wanted, pick.port, &argv, &base);
        argv = planned;
        if env.iter().any(|(k, _)| k == "PORT") {
            notes.push(format!("（已注入 PORT={}，若项目忽略该变量则仍监听原端口）", pick.port));
        }
        extra_env = env;
        observe = Some((
            crate::ports::listening_in(wanted..=wanted.saturating_add(crate::ports::OBSERVE_SPAN)),
            wanted..=wanted.saturating_add(crate::ports::OBSERVE_SPAN),
        ));
    }

    let note = if notes.is_empty() { None } else { Some(notes.join("；")) };
    sup.clear_logs(id);
    let ctx = std::sync::Arc::new(supervisor::Ctx::<R>::new(app.clone()));
    sup.spawn(
        ctx.clone(),
        supervisor::Launch {
            project_id: id,
            base: &base,
            argv: &argv,
            cfg: &cfg,
            extra_env: &extra_env,
            phase: Phase::Running,
            port: assign_port,
            note,
        },
    )?;
    supervisor::Ctx::<R>::new(app.clone()).emit_state(id);

    // 实测校准：等某个端口真正开始监听，再把真相写回状态（顺延是否生效只有实测能证明）
    if let (Some((before, range)), Some(assigned)) = (observe, assign_port) {
        let sup2: supervisor::Supervisor = (*sup).clone();
        let ctx2 = supervisor::Ctx::<R>::new(app.clone());
        std::thread::spawn(move || {
            match crate::ports::wait_listening(&before, range, std::time::Duration::from_secs(15)) {
                Some(actual) => {
                    let note = if actual == assigned {
                        None
                    } else {
                        Some(format!("实际监听 {actual}"))
                    };
                    sup2.set_port(id, Some(actual), note);
                }
                None => sup2.set_port(id, None, Some("未检测到监听端口".to_string())),
            }
            ctx2.emit_state(id);
        });
    }
    sup.status_of(id).ok_or_else(|| "状态丢失".to_string())
}

#[tauri::command]
pub fn service_stop<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> Result<(), String> {
    supervisor::state(&app).stop(id);
    supervisor::Ctx::<R>::new(app.clone()).emit_state(id);
    Ok(())
}

/// 立即返回；进度靠 `svc:state` / `svc:log` 事件。
#[tauri::command]
pub fn service_step<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64, step: String) -> Result<(), String> {
    let base = existing_project_path(id)?;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let cfg = load_config(&conn, id)?;
    let (argv, phase) = match step.as_str() {
        "install" => (cfg.install.clone(), Phase::Installing),
        "build" => (cfg.build.clone(), Phase::Building),
        other => return Err(format!("未知步骤：{other}")),
    };
    let argv = argv.ok_or_else(|| {
        format!("该项目没有{}命令", if step == "install" { "安装" } else { "构建" })
    })?;
    if let Some(first) = preflight_problems(&cfg, &argv).into_iter().next() {
        return Err(first);
    }
    let sup = supervisor::state(&app);
    if sup.is_active(id) {
        return Err("已有运行中的进程".to_string());
    }
    let ctx = std::sync::Arc::new(supervisor::Ctx::<R>::new(app.clone()));
    sup.clear_logs(id);
    sup.spawn(
        ctx.clone(),
        supervisor::Launch {
            project_id: id,
            base: &base,
            argv: &argv,
            cfg: &cfg,
            extra_env: &[],
            phase,
            port: None,
            note: None,
        },
    )?;
    ctx.emit_state(id);

    // 超时看护：到点时只有「这一代运行仍在跑」才杀，避免误伤 30 分钟后同项目的新进程
    let started = sup.gen_of(id);
    let app2 = app.clone();
    let sup2: supervisor::Supervisor = (*sup).clone();
    std::thread::spawn(move || {
        std::thread::sleep(crate::execenv::STEP_TIMEOUT);
        if sup2.gen_of(id) == started && sup2.is_active(id) {
            sup2.stop(id);
            supervisor::Ctx::new(app2.clone()).emit_state(id);
        }
    });
    Ok(())
}

#[tauri::command]
pub fn open_service_url<R: tauri::Runtime>(id: i64, app: tauri::AppHandle<R>) -> Result<(), String> {
    let port = supervisor::state(&app).port_of(id).ok_or_else(|| "该服务没有监听端口".to_string())?;
    run_command("open", &[&format!("http://127.0.0.1:{port}")])
}

#[tauri::command]
pub fn source_info(id: i64) -> Result<crate::source::SourceInfo, String> {
    let base = existing_project_path(id)?;
    Ok(crate::source::info(&base))
}

#[tauri::command]
pub fn source_fetch(id: i64) -> Result<git::GitResult, String> {
    let base = existing_project_path(id)?;
    Ok(crate::source::fetch(&base))
}

#[tauri::command]
pub fn source_pull(id: i64) -> Result<git::GitResult, String> {
    let base = existing_project_path(id)?;
    Ok(crate::source::pull(&base))
}

#[tauri::command]
pub fn clone_repo(url: String, parent_dir: String) -> Result<i64, String> {
    if !Path::new(&parent_dir).is_dir() {
        return Err(format!("目标目录不存在：{parent_dir}"));
    }
    let (dest, result) = crate::source::clone(&url, &parent_dir)?;
    if !result.ok {
        return Err(result.text);
    }
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::add_project(&conn, &dest).map_err(|e| e.to_string())
}

/// 从 `~/my/PM2/ecosystem.config.js` 导入配方。只读该文件，不改它。
#[tauri::command]
pub fn import_pm2() -> Result<ImportReport, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let path = home.join("my/PM2/ecosystem.config.js");
    let mut report = ImportReport { imported: Vec::new(), skipped: Vec::new(), errors: Vec::new() };
    if !path.is_file() {
        report.errors.push("未找到 ~/my/PM2/ecosystem.config.js".to_string());
        return Ok(report);
    }

    let path_s = path.to_string_lossy().into_owned();
    let path_env = crate::execenv::base_path();
    let node = crate::execenv::resolve("node", &path_env).ok_or_else(|| "缺少可执行文件：node".to_string())?;
    let script = "const a=require(process.argv[1]).apps;process.stdout.write(JSON.stringify(a.map(x=>({name:x.name,cwd:x.cwd,script:x.script,args:x.args,env:x.env||{}}))))";
    let out = std::process::Command::new(node)
        .args(["-e", script, &path_s])
        .output()
        .map_err(|e| format!("无法执行 node：{e}"))?;
    if !out.status.success() {
        report
            .errors
            .push(format!("解析 ecosystem.config.js 失败：{}", String::from_utf8_lossy(&out.stderr).trim()));
        return Ok(report);
    }

    #[derive(serde::Deserialize)]
    struct App {
        name: String,
        cwd: String,
        script: String,
        #[serde(default)]
        args: Option<String>,
        #[serde(default)]
        env: std::collections::BTreeMap<String, serde_json::Value>,
    }

    let apps: Vec<App> = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("解析 PM2 输出失败：{e}"))?;
    let conn = db::open_db().map_err(|e| e.to_string())?;

    for a in apps {
        if !Path::new(&a.cwd).is_dir() {
            report.errors.push(format!("跳过 {}：{} 不存在", a.name, a.cwd));
            continue;
        }
        let pid = db::add_project(&conn, &a.cwd).map_err(|e| e.to_string())?;
        if db::service_config(&conn, pid).map_err(|e| e.to_string())?.is_some() {
            report.skipped.push(format!("{}：已有配方", a.name));
            continue;
        }
        let mut start = vec![a.script.clone()];
        if let Some(args) = &a.args {
            start.extend(args.split_whitespace().map(|s| s.to_string()));
        }
        let mut env = std::collections::BTreeMap::new();
        let mut dev_port = None;
        for (k, v) in &a.env {
            if k == "PORT" {
                dev_port = v.as_u64().and_then(|n| u16::try_from(n).ok());
                continue;
            }
            if k == "NODE_ENV" {
                continue;
            }
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
        let cfg = ServiceConfig {
            start,
            env,
            dev_port,
            origin: "pm2".to_string(),
            toolchain: crate::detect::detect(Path::new(&a.cwd)).toolchain,
            ..Default::default()
        };
        db::set_service_config(&conn, pid, &cfg.to_json()?).map_err(|e| e.to_string())?;
        report.imported.push(pid);
    }
    Ok(report)
}
