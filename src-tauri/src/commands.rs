//! Tauri commands：组合 db + logic + 文件系统读取。
//!
//! 所有写/执行类命令只接受 `project_id` 与服务名，绝不接受前端传入的任意路径。

use crate::db;
use crate::git;
use crate::logic;
use crate::readme;
use crate::size;
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

/// 取项目路径并要求目录存在（供所有新增的写/执行类命令使用）。
fn existing_project_path(id: i64) -> Result<PathBuf, String> {
    let base = project_path(id)?;
    if !base.is_dir() {
        return Err("项目路径不存在".to_string());
    }
    Ok(base)
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
// 服务运行器（多服务）
// ---------------------------------------------------------------------------

use crate::localstate;
use crate::plan::{self, Layer, Resolved};
use crate::services::{Derived, Descriptor, ServiceSpec};
use crate::supervisor::{self, Phase, ServiceStatus};

#[derive(Serialize)]
pub struct ImportReport {
    /// 新建立配方的项目 id。
    pub imported: Vec<i64>,
    /// 已有配方而跳过 / cwd 不存在的条目（人读说明）。
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

/// 一个服务的完整视图（声明 + 来源 + 运行态）。
#[derive(Serialize)]
pub struct ServiceView {
    pub spec: ServiceSpec,
    /// `descriptor` | `db`
    pub layer: String,
    pub origin: String,
    pub keep_alive: bool,
    pub port_offset: u16,
    pub status: Option<ServiceStatus>,
    /// 声明与探测结果的一致性提示（命令可能已失效 / 有未声明的候选）。
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
pub struct ProjectServices {
    pub services: Vec<ServiceView>,
    /// 仓库内有描述文件时的解析/校验错误（文件坏了也要能看见）。
    pub descriptor_error: Option<String>,
    pub descriptor_exists: bool,
    pub detected: crate::detect::DetectResult,
}

fn layer_name(l: Layer) -> &'static str {
    match l {
        Layer::Descriptor => "descriptor",
        Layer::Db => "db",
    }
}

/// 把 `Resolved` 转成给前端的视图，并附上状态与警告。
fn view_of(sup: &supervisor::Supervisor, r: &Resolved) -> ServiceView {
    let key = r.key();
    let status = sup.status_of(&key);
    let mut warnings = Vec::new();
    if let Some(docker) = &r.spec.docker {
        for f in &docker.files {
            if !r.base.join(f).is_file() {
                warnings.push(format!("compose 文件不存在：{}", f));
            }
        }
    }
    if let Some(run) = r.spec.run.as_ref() {
        if let Some(program) = run.first() {
            let path = crate::execenv::path_for(r.derived.node_pin.as_deref());
            if crate::execenv::resolve(program, &path).is_none() {
                warnings.push(format!("缺少可执行文件：{program}"));
            }
        }
    }
    // #48：<1024 特权端口只做非阻断提示（放 warnings，不进制flight 阻断链——
    // preflight 的任一条都会挡住启动，特权端口在 sudo/root 下仍可绑定，不能一律拦死）。
    if let Ok(Some(wanted)) = crate::plan::wanted_port(r) {
        if wanted < 1024 && crate::ports::is_free(wanted) {
            warnings.push(format!(
                "端口 {wanted} 是特权端口（<1024），非管理员权限无法绑定：请改用 1024 以上端口或用 sudo 启动 App"
            ));
        }
    }
    ServiceView {
        layer: layer_name(r.layer).to_string(),
        origin: r.derived.origin.clone(),
        keep_alive: r.keep_alive,
        port_offset: r.port_offset,
        spec: r.spec.clone(),
        status,
        warnings,
    }
}

fn resolve_all_or_err(id: i64) -> Result<(PathBuf, Vec<Resolved>, localstate::Overrides), String> {
    let base = existing_project_path(id)?;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let (items, overrides) = plan::resolve_all(&conn, id, &base)?;
    Ok((base, items, overrides))
}

/// 项目下所有服务 + 探测结果。每次调用都重扫（纯读文件，毫秒级），保证不会显示过期信息。
#[tauri::command]
pub fn project_services<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> Result<ProjectServices, String> {
    let base = existing_project_path(id)?;
    let sup = supervisor::state(&app);
    let descriptor = crate::descriptor::read(&base);
    let descriptor_error = descriptor.as_ref().err().cloned();
    let descriptor_exists = crate::descriptor::exists(&base);
    let conn = db::open_db().map_err(|e| e.to_string())?;
    // 描述文件坏了：报错给 UI，但不假装数据库里的旧配方仍然有效（文件才是真相源）
    let items = if descriptor_error.is_some() {
        Vec::new()
    } else {
        plan::resolve_all(&conn, id, &base)?.0
    };
    let services: Vec<ServiceView> = items.iter().map(|r| view_of(&sup, r)).collect();
    let detected = crate::detect::detect(&base);
    Ok(ProjectServices { services, descriptor_error, descriptor_exists, detected })
}

/// 已声明服务的状态（全局，前端按 project_id 过滤）。
#[tauri::command]
pub fn service_status<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Vec<ServiceStatus> {
    supervisor::state(&app).statuses()
}

#[tauri::command]
pub fn service_logs<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64, name: String) -> Vec<String> {
    supervisor::state(&app).logs_of(&supervisor::key_of(id, &name))
}

/// 空数组 = 可执行；否则每条是「为什么不能点」。
#[tauri::command]
pub fn service_preflight(id: i64, name: String, step: Option<String>) -> Result<Vec<String>, String> {
    let (_, items, _) = resolve_all_or_err(id)?;
    let r = items
        .iter()
        .find(|i| i.spec.name == name)
        .ok_or_else(|| format!("该项目没有名为 {name} 的服务"))?;
    let argv = match step.as_deref() {
        Some(s) => r
            .spec
            .step(s)
            .ok_or_else(|| format!("{} 没有名为 {s} 的步骤", name))?
            .argv
            .clone(),
        None => {
            // #6：docker-only 服务没有 run 但可启动（compose 分支）：预检改为验
            // compose 文件存在性（文案与 services validate 同款），不报“没有 run 命令”。
            if !r.spec.is_longrun() {
                if let Some(docker) = &r.spec.docker {
                    let mut problems = Vec::new();
                    for f in &docker.files {
                        if !r.base.join(f).is_file() {
                            problems.push(format!("compose 文件不存在：{f}"));
                        }
                    }
                    return Ok(problems);
                }
                return Ok(vec![format!("{name} 没有 run 命令")]);
            }
            r.spec.run_argv()
        }
    };
    Ok(preflight_problems(r, &argv))
}

fn preflight_problems(r: &Resolved, argv: &[String]) -> Vec<String> {
    let path = crate::execenv::path_for(r.derived.node_pin.as_deref());
    let mut out = Vec::new();
    if let Some(program) = argv.first() {
        if crate::execenv::resolve(program, &path).is_none() {
            out.push(format!("缺少可执行文件：{program}"));
        }
    }
    for tool in &r.derived.toolchain {
        if crate::execenv::resolve(tool, &path).is_none() {
            out.push(format!("缺少可执行文件：{tool}（项目需要它才能构建/运行）"));
        }
    }
    if let Some(pin) = r.derived.node_pin.as_deref() {
        if crate::execenv::node_bin_dir(pin).is_none() {
            out.push(format!("缺少 node {pin}（nvm install {pin}）"));
        }
    }
    if let Err(e) = r.cwd() {
        out.push(e);
    }
    out.dedup();
    out
}

/// 启动一个服务（含 depends_on 拓扑顺序，先起依赖）。
#[tauri::command]
pub fn service_start<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
    name: String,
) -> Result<ServiceStatus, String> {
    let (base, items, _) = resolve_all_or_err(id)?;
    let target = items
        .iter()
        .find(|i| i.spec.name == name)
        .ok_or_else(|| format!("该项目没有名为 {name} 的服务"))?
        .clone();

    // 依赖先按拓扑顺序启动（已在运行的跳过）。只验 target 闭包：无关服务的
    // 环/错指不影响本次启动（#24，见 plan::dependency_chain）。
    let chain: Vec<Resolved> =
        plan::dependency_chain(&items, &name)?.into_iter().cloned().collect();
    let sup = supervisor::state(&app);
    let project_path = base.to_string_lossy().into_owned();
    // #21：收全部活跃 port（与 run_adhoc 一致），不只收同项目。
    let mut taken: HashSet<u16> = sup.statuses().into_iter().filter_map(|s| s.port).collect();

    for dep in &chain {
        if sup.is_active(&dep.key()) {
            continue;
        }
        start_one(&app, &sup, &project_path, dep, &mut taken, true)?;
    }

    start_one(&app, &sup, &project_path, &target, &mut taken, false)
}

fn start_one<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    sup: &supervisor::Supervisor,
    project_path: &str,
    r: &Resolved,
    taken: &mut HashSet<u16>,
    as_dependency: bool,
) -> Result<ServiceStatus, String> {
    let key = r.key();
    // #6：docker-only 服务（无 run、有 docker）走 compose 分支：up 再用 ps 确认。
    if r.spec.docker.is_some() && !r.spec.is_longrun() {
        return start_docker(app, r);
    }
    if sup.is_active(&key) {
        if as_dependency {
            return sup.status_of(&key).ok_or_else(|| "状态丢失".to_string());
        }
        return Err(format!("{} 已在运行", r.spec.name));
    }
    let argv = r.spec.run_argv();
    if let Some(first) = preflight_problems(r, &argv).into_iter().next() {
        return Err(first);
    }
    let mut plan = plan::build_plan(project_path, r, taken)?;
    if let Some(p) = plan.spawn.port {
        taken.insert(p);
    }

    // depends_on 里声明了健康等待的：先等依赖端口就绪（最多 30s），失败只提示不阻塞
    if !plan.depends_on.is_empty() {
        let mut notes: Vec<String> = plan.spawn.note.iter().cloned().collect();
        for dep in &plan.depends_on {
            let dep_key = supervisor::key_of(r.project_id, dep);
            let dep_port = sup.port_of(&dep_key);
            if let Some(p) = dep_port {
                if !plan::wait_ready(Some(p), None, std::time::Duration::from_secs(30)) {
                    notes.push(format!("依赖 {dep} 的端口 {p} 未在 30s 内就绪"));
                }
            }
        }
        if !notes.is_empty() {
            plan.spawn.note = Some(notes.join("；"));
        }
    }

    sup.clear_logs(&key);
    let ctx = std::sync::Arc::new(supervisor::Ctx::<R>::new(app.clone()));
    sup.spawn(ctx.clone(), plan.spawn.clone())?;
    supervisor::Ctx::<R>::new(app.clone()).emit_state(&key);

    // 记住实际端口，下次启动沿用（URL 才稳定）
    if let Some(p) = plan.remember_port {
        localstate::remember_port(project_path, &r.spec.name, p);
    }

    // 实测校准 + 健康检查：等真正开始监听，把真相写回
    if plan.spawn.port.is_some() {
        let sup2 = (*sup).clone();
        let ctx2 = supervisor::Ctx::<R>::new(app.clone());
        let key2 = key.clone();
        let assigned = plan.spawn.port;
        let health = plan.health.clone();
        let timeout = health
            .as_ref()
            .and_then(|h| h.timeout_ms)
            .map(std::time::Duration::from_millis)
            .unwrap_or_else(|| std::time::Duration::from_secs(30));
        let project_path = project_path.to_string();
        let svc_name = r.spec.name.clone();
        std::thread::spawn(move || {
            let ok = plan::wait_ready(assigned, health.as_ref(), timeout);
            if ok {
                if let Some(p) = assigned {
                    sup2.set_port(&key2, Some(p), None);
                }
                localstate::remember_port(&project_path, &svc_name, assigned.unwrap_or(0));
            } else {
                sup2.set_port(&key2, assigned, Some("健康检查超时：进程在跑但未就绪".to_string()));
            }
            ctx2.emit_state(&key2);
        });
    }
    sup.status_of(&key).ok_or_else(|| "状态丢失".to_string())
}

/// docker-only 服务的启动（#6）：`compose up -d` 再用 `ps` 确认。
///
/// 语义：全部 declared services `state == "running"` 才算成功，否则把 ps/up 的
/// 真实错误上抛。返回的 ServiceStatus 是 ps 衍生的即时快照（不进 supervisor
/// 进程表：supervisor 只管本地子进程，容器的生死归 docker 管；前端展示与
/// `docker_ps` 面板一致，以 state/note 为准，不要按 Phase 做启停判断）。
fn start_docker<R: tauri::Runtime>(
    _app: &tauri::AppHandle<R>,
    r: &Resolved,
) -> Result<ServiceStatus, String> {
    let spec = r.spec.docker.clone().ok_or_else(|| format!("{} 不是 docker 服务", r.spec.name))?;
    crate::docker::up(&r.base, &spec)?;
    let views = crate::docker::ps(&r.base, &spec)?;
    // 全部 declared services 都 running 才算成（services 为空时 views 非空即成）。
    let wanted: Vec<&str> = spec.services.iter().map(String::as_str).collect();
    let mut not_running: Vec<String> = Vec::new();
    if wanted.is_empty() {
        if views.iter().all(|v| v.state != "running") {
            not_running.push("（无 running 容器）".to_string());
        }
    } else {
        for w in &wanted {
            let ok = views.iter().any(|v| v.service == *w && v.state == "running");
            if !ok {
                let actual = views
                    .iter()
                    .find(|v| v.service == *w)
                    .map(|v| v.state.as_str())
                    .unwrap_or("缺失");
                not_running.push(format!("{w}（{actual}）"));
            }
        }
    }
    if !not_running.is_empty() {
        return Err(format!("{}：以下容器未进入 running：{}", r.spec.name, not_running.join("、")));
    }
    let running: Vec<String> = views
        .iter()
        .filter(|v| v.state == "running")
        .map(|v| {
            if v.ports.is_empty() {
                v.service.clone()
            } else {
                format!("{}(:{})", v.service, v.ports)
            }
        })
        .collect();
    // 端口：取第一个 running 容器的 PublishedPort（ps 已按声明过滤），供“打开”按钮用。
    let port = views
        .iter()
        .filter(|v| v.state == "running")
        .filter_map(|v| v.ports.split(',').next()?.trim().parse::<u16>().ok())
        .next();
    Ok(ServiceStatus {
        key: r.key(),
        project_id: r.project_id,
        name: r.spec.name.clone(),
        phase: Phase::Running,
        step: None,
        port,
        pid: None,
        exit_code: None,
        note: Some(format!("docker 运行中：{}", running.join("、"))),
        restarts: 0,
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    })
}

#[tauri::command]
pub fn service_stop<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64, name: String) -> Result<(), String> {
    let key = supervisor::key_of(id, &name);
    supervisor::state(&app).stop(&key);
    localstate::clear_adopt(&key);
    supervisor::Ctx::<R>::new(app.clone()).emit_state(&key);
    Ok(())
}

#[tauri::command]
pub fn service_stop_all<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> Result<(), String> {
    let sup = supervisor::state(&app);
    for key in sup.active_keys_of_project(id) {
        sup.stop(&key);
        localstate::clear_adopt(&key);
        supervisor::Ctx::<R>::new(app.clone()).emit_state(&key);
    }
    Ok(())
}

/// 立即返回；进度靠 `svc:state` / `svc:log` 事件。
#[tauri::command]
pub fn service_step<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
    name: String,
    step: String,
) -> Result<(), String> {
    let (_, items, _) = resolve_all_or_err(id)?;
    let r = items
        .iter()
        .find(|i| i.spec.name == name)
        .ok_or_else(|| format!("该项目没有名为 {name} 的服务"))?;
    let spec = r.spec.step(&step).ok_or_else(|| format!("{name} 没有名为 {step} 的步骤"))?;
    let timeout_s = spec.timeout_s;
    let spawn = plan::build_step_plan(r, spec)?;
    if let Some(first) = preflight_problems(r, &spawn.argv).into_iter().next() {
        return Err(first);
    }
    let sup = supervisor::state(&app);
    let key = r.key();
    if sup.is_active(&key) {
        return Err(format!("{name} 已有运行中的进程"));
    }
    sup.clear_logs(&key);
    let ctx = std::sync::Arc::new(supervisor::Ctx::<R>::new(app.clone()));
    sup.spawn(ctx.clone(), spawn)?;
    supervisor::Ctx::<R>::new(app.clone()).emit_state(&key);

    // 超时看护：到点时只有「这一代运行仍在跑」才杀
    let started = sup.gen_of(&key);
    let app2 = app.clone();
    let sup2 = (*sup).clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(timeout_s));
        if sup2.gen_of(&key) == started && sup2.is_active(&key) {
            sup2.stop(&key);
            supervisor::Ctx::new(app2.clone()).emit_state(&key);
        }
    });
    Ok(())
}

#[tauri::command]
pub fn open_service_url<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64, name: String) -> Result<(), String> {
    let key = supervisor::key_of(id, &name);
    let port = supervisor::state(&app).port_of(&key).ok_or_else(|| "该服务没有监听端口".to_string())?;
    run_command("open", &[&format!("http://127.0.0.1:{port}")])
}

/// 保存单个服务到数据库（「一键采用」/手工填写）。不写仓库文件。
#[tauri::command]
pub fn save_service(id: i64, spec: ServiceSpec) -> Result<(), String> {
    let base = existing_project_path(id)?;
    let problems = spec.validate();
    if !problems.is_empty() {
        return Err(problems.join("；"));
    }
    let mut derived = Derived::manual();
    let detected = crate::detect::detect(&base);
    derived.toolchain = detected.toolchain;
    derived.node_pin = detected.node_pin;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::insert_service(&conn, id, &spec, &derived).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn delete_service(id: i64, name: String) -> Result<(), String> {
    let base = existing_project_path(id)?;
    if crate::descriptor::exists(&base) {
        return Err(format!(
            "该项目有 {}，请在该文件里删除服务（数据库不会覆盖已有声明）",
            crate::descriptor::FILE_NAME
        ));
    }
    let conn = db::open_db().map_err(|e| e.to_string())?;
    db::delete_service(&conn, id, &name).map_err(|e| e.to_string())
}

/// 固化为仓库内声明：把当前数据库里的服务写进 `.project-ctrl.json`。
/// 已存在时默认拒绝（`overwrite=false`），绝不静默覆盖团队文件。
///
/// # IPC 形状
///
/// `invoke('solidify_descriptor', { id, names, overwrite, confirm })`。
/// 参数名用单字 `confirm`（不是 `confirm_env`）：Tauri 默认把 Rust 的
/// `confirm_env` 映射成 `confirmEnv`，前端发 snake 会对不上（Round 1 教训）。
///
/// # 前端确认 UX 约定
///
/// 初次调用传 `confirm: false`。若任一待写 spec.env 非空，后端返回含
/// `ENV_CONFIRM_REQUIRED` 的错误（只列服务名+env key 名，**严禁出现 value**），
/// 且不写文件。前端此时弹确认对话框（展示错误全文），用户确认后再以
/// `confirm: true` 重调一次；其余错误直接展示不弹框。
#[tauri::command]
pub fn solidify_descriptor(
    id: i64,
    names: Vec<String>,
    overwrite: bool,
    confirm: bool,
) -> Result<String, String> {
    let base = existing_project_path(id)?;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let rows = db::services_of(&conn, id).map_err(|e| e.to_string())?;
    let mut specs = Vec::new();
    for row in rows {
        let spec = ServiceSpec::from_json(&row.config)?;
        if names.is_empty() || names.contains(&spec.name) {
            specs.push(spec);
        }
    }
    if specs.is_empty() {
        return Err("没有可固化的服务".to_string());
    }
    // env 确认门：任一待写 spec.env 非空且未确认 → 拒绝（只列 key 名，严禁 value）。
    if !confirm {
        let flagged: Vec<String> = specs
            .iter()
            .filter(|s| !s.env.is_empty())
            .map(|s| {
                let keys: Vec<&str> = s.env.keys().map(String::as_str).collect();
                format!("{}({})", s.name, keys.join(", "))
            })
            .collect();
        if !flagged.is_empty() {
            return Err(format!(
                "需要确认（ENV_CONFIRM_REQUIRED）：以下服务的 env 将写入可提交文件，仅列出变量名：{}。确认无密钥后用 confirm=true 重试",
                flagged.join("、")
            ));
        }
    }
    // 写前逐条 validate：坏 spec 不写文件。
    let mut problems = Vec::new();
    for s in &specs {
        problems.extend(s.validate());
    }
    if !problems.is_empty() {
        return Err(problems.join("；"));
    }
    let desc = Descriptor::new(specs);
    crate::descriptor::write(&base, &desc, overwrite)?;
    let json = crate::services::to_descriptor_json(&desc)?;
    Ok(json)
}

/// 描述文件的当前文本（供 UI 预览 diff / 手工编辑）。
#[tauri::command]
pub fn read_descriptor(id: i64) -> Result<Option<String>, String> {
    let base = existing_project_path(id)?;
    match std::fs::read_to_string(crate::descriptor::path_of(&base)) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取描述文件失败：{e}")),
    }
}

/// 服务在本机层的覆盖（端口偏移 / keep_alive / restart / env）。
///
/// 与描述文件方向相反：这里**不**校验未知键，用户手写的内容原样保留。
/// patch 三态语义（#22，见 `localstate::OverridePatch`）：字段缺席=不变，
/// `null`=删除该键，有值=设置。外层 `patch: null`=删除整个服务的覆盖。
#[tauri::command]
pub fn set_service_override(
    id: i64,
    name: String,
    patch: Option<localstate::OverridePatch>,
) -> Result<(), String> {
    let base = existing_project_path(id)?;
    let project_path = base.to_string_lossy().into_owned();
    // #19：覆盖层 env_files 与描述文件同规则校验（绝对路径/.. 拒绝入库）。
    if let Some(files) = patch.as_ref().and_then(|p| p.env_files.as_ref()).and_then(|o| o.as_ref()) {
        for f in files {
            if !crate::services::is_project_relative(f) {
                return Err(format!("{name}：env_files 必须是项目内的相对路径（{f}）"));
            }
        }
    }
    localstate::modify_overrides(|o| localstate::set_override(o, &project_path, &name, patch.as_ref()))
}

/// 读一个服务在本机层的覆盖（UI 打开开关时用）。
#[tauri::command]
pub fn service_override(id: i64, name: String) -> Result<Option<localstate::Override>, String> {
    let base = existing_project_path(id)?;
    let project_path = base.to_string_lossy().into_owned();
    let overrides = localstate::load_overrides()?;
    Ok(localstate::service_override(&overrides, &project_path, &name))
}

/// 批量探测：把每个项目的候选启动方式列出来，供一次性采纳。
///
/// 只读，不写库、不启动任何东西。用户勾选后才逐条保存（见 `adopt_candidates`）。
#[derive(Serialize)]
pub struct ProbeItem {
    pub project_id: i64,
    pub project_name: String,
    pub path: String,
    pub has_descriptor: bool,
    pub has_services: bool,
    /// 候选：`argv` 命令或 compose 栈，二选一。
    pub candidates: Vec<ProbeCandidate>,
}

#[derive(Serialize, Clone)]
pub struct ProbeCandidate {
    /// `script` | `compose` | `make` | `script-file`
    pub kind: String,
    /// 人读标签（命令本身或 compose 文件）。
    pub label: String,
    pub spec: ServiceSpec,
}

#[tauri::command]
pub fn probe_all() -> Result<Vec<ProbeItem>, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let rows = db::all_projects(&conn).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let base = PathBuf::from(&row.path);
        if !base.is_dir() {
            continue;
        }
        let names = db::service_names(&conn, row.id).map_err(|e| e.to_string())?;
        let d = crate::detect::detect(&base);
        let mut candidates = Vec::new();
        for argv in &d.start_candidates {
            candidates.push(ProbeCandidate {
                kind: "script".into(),
                label: argv.join(" "),
                spec: ServiceSpec {
                    name: simple_name(argv),
                    cwd: None,
                    run: Some(argv.clone()),
                    steps: d.build.clone().map(|b| vec![crate::services::Step {
                        name: "build".into(),
                        argv: b,
                        timeout_s: 1800,
                    }]).unwrap_or_default(),
                    env_files: Vec::new(),
                    env: Default::default(),
                    ports: crate::services::Ports {
                        listen: d.dev_port,
                        requires: d.requires_ports.clone(),
                    },
                    health: None,
                    depends_on: Vec::new(),
                    restart: crate::services::Restart::OnFailure,
                    docker: None,
                },
            });
        }
        for stack in &d.compose_stacks {
            // 顶层 compose 文件才直接采纳；子目录的留给单个项目里点选
            if stack.file.contains('/') {
                continue;
            }
            let mut files = vec![stack.file.clone()];
            files.extend(stack.overrides.iter().cloned());
            candidates.push(ProbeCandidate {
                kind: "compose".into(),
                label: stack.file.clone(),
                spec: ServiceSpec {
                    name: "docker".into(),
                    cwd: None,
                    run: None,
                    steps: Vec::new(),
                    env_files: Vec::new(),
                    env: Default::default(),
                    ports: crate::services::Ports { listen: None, requires: stack.ports.clone() },
                    health: None,
                    depends_on: Vec::new(),
                    restart: crate::services::Restart::OnFailure,
                    docker: Some(crate::services::DockerSpec {
                        files,
                        project_name: None,
                        services: stack.services.clone(),
                    }),
                },
            });
        }
        out.push(ProbeItem {
            project_id: row.id,
            project_name: crate::logic::project_name_of(&base),
            path: row.path,
            has_descriptor: crate::descriptor::exists(&base),
            has_services: !names.is_empty(),
            candidates,
        });
    }
    out.sort_by(|a, b| a.project_name.cmp(&b.project_name));
    Ok(out)
}

/// 从 argv 里取一个**有信息量**的服务名。
///
/// 直接取「第一个非 flag 的第二个词」会把 `pnpm run dev` 命名成 `run`、
/// `cargo run --bin omp-hub` 命名成 `run`——这些名字对用户没有任何区分度。
/// 规则：跳过子命令动词（`run` / `exec` / `dlx` …），取真正的脚本名或二进制品名；
/// 仍然无意义时退化为 `app`。
fn simple_name(argv: &[String]) -> String {
    const VERBS: &[&str] = &["run", "exec", "dlx", "x"];
    const DEGENERATE: &[&str] = &["", "run", "start", "index", "main", "app"];
    // 所有非 flag 的 token（第一个是程序名本身，单独保留作兜底）
    let tokens: Vec<&String> = argv.iter().filter(|a| !a.starts_with('-')).collect();
    let mut rest = tokens.iter().skip(1).copied();
    // 跳过开头的动词（不论是不是包管理器：`cargo run --bin x` 也一样）
    let mut candidate = rest.next();
    while matches!(candidate, Some(c) if VERBS.contains(&c.as_str())) {
        candidate = rest.next();
    }
    // 全是动词（`npm start`）时用最后一个 token，最终会退化成 app
    let raw = match candidate {
        Some(c) => c.as_str(),
        None => tokens.last().map(|s| s.as_str()).unwrap_or("app"),
    };
    // 去掉目录与扩展名：`src/index.ts` → `index`，`scripts/serve.sh` → `serve`
    let stem = std::path::Path::new(raw)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.to_string());
    let cleaned: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ':')
        .take(24)
        .collect();
    if DEGENERATE.contains(&cleaned.as_str()) {
        "app".to_string()
    } else {
        cleaned
    }
}

/// 批量采纳到**本机层**（数据库）。绝不写仓库文件——批量操作不该让 N 个仓库同时变脏。
#[tauri::command]
pub fn adopt_candidates(items: Vec<(i64, ServiceSpec)>) -> Result<Vec<String>, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let mut saved = Vec::new();
    let mut problems = Vec::new();
    for (id, spec) in items {
        let problems_here = spec.validate();
        if !problems_here.is_empty() {
            problems.push(format!("{}：{}", spec.name, problems_here.join("；")));
            continue;
        }
        let base = match db::project_path(&conn, id).map_err(|e| e.to_string())? {
            Some(p) => PathBuf::from(p),
            None => {
                problems.push(format!("{}：项目不存在", spec.name));
                continue;
            }
        };
        if crate::descriptor::exists(&base) {
            problems.push(format!("{}：该项目已有描述文件，请编辑它", spec.name));
            continue;
        }
        let mut derived = Derived::manual();
        derived.toolchain = crate::detect::detect(&base).toolchain;
        db::insert_service(&conn, id, &spec, &derived).map_err(|e| e.to_string())?;
        saved.push(spec.name);
    }
    if !problems.is_empty() && saved.is_empty() {
        return Err(problems.join("；"));
    }
    Ok(saved)
}

/// 临时运行：不写数据库、不写仓库，直接按给定声明起一个进程。
///
/// 这是「先试试它能不能跑起来」的路径——关掉 App 就消失，不留下你从未确认过的配置。
#[tauri::command]
pub fn run_adhoc<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
    spec: ServiceSpec,
) -> Result<ServiceStatus, String> {
    let base = existing_project_path(id)?;
    let problems = spec.validate();
    if !problems.is_empty() {
        return Err(problems.join("；"));
    }
    if !spec.is_longrun() {
        return Err("临时运行需要一个 run 命令".to_string());
    }
    // #59：临时运行不支持 depends_on（依赖需要落库/声明才能解析启动顺序）。
    if !spec.depends_on.is_empty() {
        return Err("临时运行不支持 depends_on，请先落库/声明".to_string());
    }
    let resolved = Resolved {
        project_id: id,
        base: base.clone(),
        spec: spec.clone(),
        derived: Derived { origin: "adhoc".into(), ..Derived::default() },
        layer: Layer::Db,
        keep_alive: false,
        port_offset: 0,
        override_env_files: Vec::new(),
    };
    // #59：与 service_preflight 同款纯校验（缺二进制/缺目录直接拒绝，不 spawn）。
    if let Some(first) = preflight_problems(&resolved, &spec.run_argv()).into_iter().next() {
        return Err(first);
    }
    let sup = supervisor::state(&app);
    let key = resolved.key();
    if sup.is_active(&key) {
        return Err(format!("{} 已在运行", spec.name));
    }
    let project_path = base.to_string_lossy().into_owned();
    let mut taken: HashSet<u16> = sup.statuses().into_iter().filter_map(|s| s.port).collect();
    let plan = plan::build_plan(&project_path, &resolved, &taken)?;
    if let Some(p) = plan.spawn.port {
        taken.insert(p);
    }
    sup.clear_logs(&key);
    let ctx = std::sync::Arc::new(supervisor::Ctx::<R>::new(app.clone()));
    sup.spawn(ctx, plan.spawn.clone())?;
    supervisor::Ctx::<R>::new(app.clone()).emit_state(&key);
    sup.status_of(&key).ok_or_else(|| "状态丢失".to_string())
}

/// 跟随 compose 日志：接入与服务相同的日志面板，不需要第二套通道。
///
/// 同一服务重复调用会**顶掉**旧流（`epoch` 前进，旧读线程自行退出），
/// 因此不需要前端自己记「我已经开过一条」。
#[tauri::command]
pub fn docker_logs<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
    name: String,
) -> Result<u64, String> {
    use tauri::Manager;
    let (_, items, _) = resolve_all_or_err(id)?;
    let r = items.iter().find(|i| i.spec.name == name).ok_or_else(|| format!("没有服务 {name}"))?;
    let spec = r.spec.docker.clone().ok_or_else(|| format!("{name} 不是 docker 服务"))?;
    let key = supervisor::key_of(id, &name);

    // 先停掉同 key 的旧流，再开新的：否则两条流会往同一个环形缓冲里交错写。
    // #58 锁中毒用 into_inner（与 stop_all 一致）；#80 remove 后释锁再 stop，
    // 不把锁持过 blocking 的 stop。
    let old = app
        .state::<DockerLogsRegistry>()
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if let Some(old) = old {
        crate::docker::DockerLogs::stop(old);
    }

    let sup: supervisor::Supervisor = (*supervisor::state(&app)).clone();
    let sink = std::sync::Arc::new(supervisor::Ctx::<R>::new(app.clone()));
    let logs = crate::docker::DockerLogs::follow(&sup, sink, &r.base, &spec, id, &name)?;
    let epoch = logs.epoch();
    app.state::<DockerLogsRegistry>()
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, logs);
    Ok(epoch)
}

/// 停止 compose 日志跟随（不影响容器本身）。
#[tauri::command]
pub fn docker_logs_stop<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
    name: String,
) -> Result<(), String> {
    use tauri::Manager;
    let key = supervisor::key_of(id, &name);
    // #58/#80：同上——into_inner + 释锁后再 stop。
    let logs = app
        .state::<DockerLogsRegistry>()
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if let Some(logs) = logs {
        crate::docker::DockerLogs::stop(logs);
    }
    Ok(())
}

/// 外部监督者（launchd plist / PM2 ecosystem）的只读视图。
///
/// 刻意只读：这两套已经在跑，App 若去写 plist 或调 `pm2 start`，会和它们的保活策略
/// 抢同一个进程。这里只呈现事实，并标出「这条是否已在本 App 里管理」。
#[derive(Serialize)]
pub struct ExternalItem {
    #[serde(flatten)]
    pub entry: crate::external::ExternalEntry,
    /// 与本 App 里同名声明不一致时给出说明；一致或无声明为 None。
    pub drift: Option<crate::external::Drift>,
}

/// 每次都重新读文件：PM2 的 ecosystem 与 plist 都可能在你手工改过之后变了，
/// 缓存结果会让面板长期显示过期信息。
#[tauri::command]
pub fn external_scan() -> Result<Vec<ExternalItem>, String> {
    let home = crate::external::home();
    let mut entries = crate::external::scan(&home);
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let mut projects: Vec<(String, Vec<String>)> = Vec::new();
    // 项目路径 → 服务名 → 该服务的命令，供漂移比对
    let mut declared: std::collections::HashMap<(String, String), Vec<String>> = Default::default();
    for row in db::all_projects(&conn).map_err(|e| e.to_string())? {
        let names = db::service_names(&conn, row.id).map_err(|e| e.to_string())?;
        if let Ok((items, _)) = plan::resolve_all(&conn, row.id, std::path::Path::new(&row.path)) {
            for it in items {
                declared.insert((row.path.clone(), it.spec.name.clone()), it.spec.run_argv());
            }
        }
        projects.push((row.path, names));
    }
    crate::external::annotate(&mut entries, &projects);
    Ok(entries
        .into_iter()
        .map(|entry| {
            let drift = entry.cwd.as_ref().and_then(|cwd| {
                let cwd = crate::logic::normalize_path(cwd);
                declared.get(&(cwd, entry.name.clone())).and_then(|cmd| {
                    crate::external::drift_of(&entry, Some(cmd))
                })
            });
            ExternalItem { entry, drift }
        })
        .collect())
}

/// 在 Finder 里显示一个外部配置文件（只定位，不打开编辑器）。
///
/// 白名单（#54）：只允许 `external_scan` 产出的 `config_path`。调用时重扫取
/// 集合反查，命中才 `open -R -- <path>`（`--` 防文件名被当成 flag）；
/// 未命中 → 拒绝；scan 本身失败 → Err（不静默放行）。
#[tauri::command]
pub fn reveal_file(path: String) -> Result<(), String> {
    // 先判文件不存在（保持旧行为/旧断言兼容），再做扫描白名单反查。
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("文件不存在：{path}"));
    }
    let home = crate::external::home();
    let entries = crate::external::scan(&home);
    if entries.is_empty() {
        return Err("未扫描到任何外部配置，无法确认该路径是否可显示".to_string());
    }
    let hit = entries.iter().any(|e| e.config_path == path);
    if !hit {
        return Err(format!("该路径不在外部扫描结果中，拒绝显示：{path}"));
    }
    run_command("open", &["-R", "--", &path])
}

/// 启动 compose 栈（`docker compose up -d`）。
#[tauri::command]
pub fn docker_up(id: i64, name: String) -> Result<String, String> {
    let (base, items, _) = resolve_all_or_err(id)?;
    let r = items.iter().find(|i| i.spec.name == name).ok_or_else(|| format!("没有服务 {name}"))?;
    let spec = r.spec.docker.clone().ok_or_else(|| format!("{name} 不是 docker 服务"))?;
    let _ = base;
    crate::docker::up(&r.base, &spec)
}

#[tauri::command]
pub fn docker_down(id: i64, name: String) -> Result<String, String> {
    let (_, items, _) = resolve_all_or_err(id)?;
    let r = items.iter().find(|i| i.spec.name == name).ok_or_else(|| format!("没有服务 {name}"))?;
    let spec = r.spec.docker.clone().ok_or_else(|| format!("{name} 不是 docker 服务"))?;
    crate::docker::down(&r.base, &spec)
}

#[tauri::command]
pub fn docker_ps(id: i64, name: String) -> Result<Vec<crate::docker::ContainerView>, String> {
    let (_, items, _) = resolve_all_or_err(id)?;
    let r = items.iter().find(|i| i.spec.name == name).ok_or_else(|| format!("没有服务 {name}"))?;
    let spec = r.spec.docker.clone().ok_or_else(|| format!("{name} 不是 docker 服务"))?;
    crate::docker::ps(&r.base, &spec)
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
        if !db::service_names(&conn, pid).map_err(|e| e.to_string())?.is_empty() {
            report.skipped.push(format!("{}：已有服务声明", a.name));
            continue;
        }
        let mut run = vec![a.script.clone()];
        if let Some(args) = &a.args {
            run.extend(crate::external::split_args(args));
        }
        let mut env = std::collections::BTreeMap::new();
        let mut listen = None;
        for (k, v) in &a.env {
            if k == "PORT" {
                listen = v.as_u64().and_then(|n| u16::try_from(n).ok());
                continue;
            }
            if k == "NODE_ENV" {
                continue;
            }
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
        let spec = ServiceSpec {
            name: "default".into(),
            cwd: None,
            run: Some(run),
            steps: Vec::new(),
            env_files: Vec::new(),
            env,
            ports: crate::services::Ports { listen, requires: Vec::new() },
            health: None,
            depends_on: Vec::new(),
            restart: crate::services::Restart::OnFailure,
            docker: None,
        };
        let mut derived = Derived::manual();
        derived.origin = "pm2".into();
        derived.toolchain = crate::detect::detect(Path::new(&a.cwd)).toolchain;
        db::insert_service(&conn, pid, &spec, &derived).map_err(|e| e.to_string())?;
        report.imported.push(pid);
    }
    Ok(report)
}

/// App 退出前的收尾：先停所有 compose 日志跟随，再处理受管进程。
/// - 声明了 `keep_alive` 的服务保留进程，并把凭据标记为「可收养」；
/// - 其余服务杀掉，并清掉凭据（避免下次启动对死 pid 做收养尝试）。
pub fn register_adoptable(sup: &supervisor::Supervisor) {
    // 保留决定在**退出这一刻**重新判定：用户可能在服务启动之后才勾上开关。
    let records = sup.adoptable();
    let overrides = localstate::load_overrides();
    if let Err(e) = &overrides {
        eprintln!("register_adoptable: 读取 overrides 失败，已按 spawn 值回退：{e}");
    }
    let paths: Result<std::collections::HashMap<i64, String>, String> = db::open_db()
        .map_err(|e| e.to_string())
        .and_then(|conn| {
            db::all_projects(&conn)
                .map_err(|e| e.to_string())
                .map(|rows| rows.into_iter().map(|p| (p.id, p.path)).collect())
        });
    if let Err(e) = &paths {
        eprintln!("register_adoptable: 读取项目表失败，已按 spawn 值回退：{e}");
    }
    let keep = decide_keep(&records, overrides, paths);
    // #12：clear 后置——先 shutdown_all(keep)，再对非 keep 的 key 清凭据。
    // 之前 clear 在 shutdown 之前，中间被 SIGKILL 会留下永久孤儿（有进程无凭据）。
    sup.shutdown_all(&keep);
    // TODO(集成者): 这里应对“非 keep 且进程组已死”的 key 才 clear，组仍活的保留
    // 记录（keep_alive=false 留待下次回收）。需要 supervisor::group_alive 提为
    // pub(crate) 后补校验：`if !keep.contains(&k) && !supervisor::group_alive(pgid)`。
    // supervisor.rs 非本 slice 文件，不能改——现先实现 clear 后置，校验待补。
    for rec in &records {
        if keep.contains(&rec.key) {
            if let Err(e) =
                localstate::write_adopt(&supervisor::AdoptRecord { keep_alive: true, ..rec.clone() })
            {
                // #52：写凭据失败必须可见（之前 `let _ =` 静默吞掉）。
                eprintln!("register_adoptable: 写收养凭据 {} 失败：{e}", rec.key);
            }
        } else {
            localstate::clear_adopt(&rec.key);
        }
    }
}

/// 退出时的保留决定（#13，纯函数）。
///
/// fail-closed：任一数据源失败（overrides Err 或项目表 Err）→ 回退各记录
/// spawn 时的 `keep_alive`，不置空（置空会静默全杀）。
pub fn decide_keep(
    live: &[supervisor::AdoptRecord],
    overrides: Result<localstate::Overrides, String>,
    paths: Result<std::collections::HashMap<i64, String>, String>,
) -> HashSet<String> {
    let (Ok(overrides), Ok(paths)) = (overrides, paths) else {
        return live.iter().filter(|r| r.keep_alive).map(|r| r.key.clone()).collect();
    };
    live.iter()
        .filter(|rec| {
            let Some(base) = paths.get(&rec.project_id) else { return false };
            localstate::service_override(&overrides, base, &rec.name)
                .and_then(|o| o.keep_alive)
                .unwrap_or(false)
        })
        .map(|rec| rec.key.clone())
        .collect()
}

/// 启动时收养上次留下的进程（三重校验，任一不过就清理记录）。
pub fn adopt_existing<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let records = localstate::read_adopts();
    if records.is_empty() {
        return;
    }
    let sup: supervisor::Supervisor = (*supervisor::state(app)).clone();
    let sink = std::sync::Arc::new(supervisor::Ctx::<R>::new(app.clone()));
    let conn = match db::open_db() {
        Ok(c) => c,
        // #50：db 打不开时不静默 return——凭据留待下次启动处理。
        Err(e) => {
            eprintln!("adopt_existing: 打开数据库失败，跳过本次收养：{e}");
            return;
        }
    };
    for rec in records {
        let key = rec.key.clone();
        if let Err(e) = supervisor::verify_adopt(&rec) {
            eprintln!("adopt_existing: 凭据 {key} 校验失败，已清理：{e}");
            localstate::clear_adopt(&key);
            continue;
        }
        // 上次不是「要保留」的进程，说明 App 是被强杀/崩溃的：回收孤儿。
        // 这一步不依赖项目是否还在库中——凭据本身已经证明它是我们起的进程。
        if !rec.keep_alive {
            // #11：只杀验明正身的组；校验失败时进程不是我们的，不杀只清记录。
            if let Err(e) = supervisor::reclaim_group_verified(&rec) {
                eprintln!("adopt_existing: 孤儿 {key} 回收被拒绝：{e}");
            }
            localstate::clear_adopt(&key);
            continue;
        }
        // #14：以下三处放弃路径——进程已验活，直接 clear 会留下带端口孤儿。
        // 先 reclaim_group_verified，Err（校验拒绝）则保留记录+eprintln，不 clear。
        let base = match db::project_path(&conn, rec.project_id).ok().flatten() {
            Some(p) => PathBuf::from(p),
            None => {
                eprintln!("adopt_existing: 凭据 {key} 的项目已不在库中，回收进程");
                if let Err(e) = supervisor::reclaim_group_verified(&rec) {
                    eprintln!("adopt_existing: {key} 回收被拒绝，保留记录：{e}");
                    continue;
                }
                localstate::clear_adopt(&key);
                continue;
            }
        };
        // 必须走 `resolve_all`：它负责「仓库声明优先、没有才用数据库」的回落。
        // 直接用 `load_db_specs`（空列表也是 Ok）会让描述文件里的服务永远找不到。
        let items = match plan::resolve_all(&conn, rec.project_id, &base) {
            Ok((items, _)) => items,
            Err(e) => {
                eprintln!("adopt_existing: 凭据 {key} 的声明解析失败，回收进程：{e}");
                if let Err(e) = supervisor::reclaim_group_verified(&rec) {
                    eprintln!("adopt_existing: {key} 回收被拒绝，保留记录：{e}");
                    continue;
                }
                localstate::clear_adopt(&key);
                continue;
            }
        };
        let Some(r) = items.into_iter().find(|i| i.spec.name == rec.name) else {
            eprintln!("adopt_existing: 凭据 {key} 的服务已改名/删除，回收进程");
            if let Err(e) = supervisor::reclaim_group_verified(&rec) {
                eprintln!("adopt_existing: {key} 回收被拒绝，保留记录：{e}");
                continue;
            }
            localstate::clear_adopt(&key);
            continue;
        };
        let spec = supervisor::SpawnSpec {
            key: key.clone(),
            project_id: rec.project_id,
            name: rec.name.clone(),
            base: PathBuf::from(&rec.cwd),
            argv: rec.argv.clone(),
            env: Vec::new(),
            phase: Phase::Running,
            step: None,
            port: rec.port,
            note: Some("已接管上次运行的服务".to_string()),
            restart: r.spec.restart,
            keep_alive: true,
        };
        if sup.adopt(sink.clone(), spec, rec.pid, rec.started_at).is_err() {
            // #50：key 已活跃（上次启动已收养过）：旧凭据作废，避免下次反复尝试。
            localstate::clear_adopt(&key);
            continue;
        }
        let st = sup.status_of(&key).expect("收养后状态必须存在");
        supervisor::Sink::state(sink.as_ref(), &st);
    }
}

#[cfg(test)]
mod name_tests {
    use super::simple_name;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn package_manager_verbs_are_skipped() {
        assert_eq!(simple_name(&v(&["pnpm", "run", "dev"])), "dev");
        assert_eq!(simple_name(&v(&["pnpm", "dev"])), "dev");
        assert_eq!(simple_name(&v(&["npm", "start"])), "app");
        assert_eq!(simple_name(&v(&["bun", "run", "server"])), "server");
    }

    #[test]
    fn paths_are_reduced_to_a_stem() {
        assert_eq!(simple_name(&v(&["node", "src/index.ts"])), "app");
        assert_eq!(simple_name(&v(&["node", "dist/server.js"])), "server");
        assert_eq!(simple_name(&v(&["bash", "scripts/serve.sh"])), "serve");
    }

    #[test]
    fn flags_never_become_names() {
        assert_eq!(simple_name(&v(&["cargo", "run", "--bin", "omp-hub"])), "omp-hub");
    }

    #[test]
    fn decide_keep_falls_back_to_spawn_value_when_sources_fail() {
        use crate::supervisor::AdoptRecord;
        fn rec(key: &str, pid: i64, name: &str, keep: bool) -> AdoptRecord {
            AdoptRecord {
                key: key.into(),
                project_id: pid,
                name: name.into(),
                pid: 1,
                pgid: 1,
                argv: vec!["x".into()],
                cwd: "/tmp".into(),
                started_at: 1,
                port: None,
                keep_alive: keep,
            }
        }
        let live = vec![rec("1/a", 1, "a", true), rec("1/b", 1, "b", false)];
        // 全好：以 overrides 为准。
        let mut o = crate::localstate::Overrides::default();
        crate::localstate::set_override(
            &mut o,
            "/x/a",
            "b",
            Some(&crate::localstate::OverridePatch {
                keep_alive: Some(Some(true)),
                ..Default::default()
            }),
        )
        .unwrap();
        let paths = std::collections::HashMap::from([(1i64, "/x/a".to_string())]);
        let keep = super::decide_keep(&live, Ok(o), Ok(paths.clone()));
        assert!(keep.contains("1/b"));
        assert!(!keep.contains("1/a"), "全好时 spawn 值不作数，以 overrides 为准");
        // db 失败：回退各记录 spawn 时的 keep_alive（a 保留、b 不保留）。
        let o2 = crate::localstate::Overrides::default();
        let keep = super::decide_keep(&live, Ok(o2), Err("db 锁".to_string()));
        assert!(keep.contains("1/a"));
        assert!(!keep.contains("1/b"));
        // overrides 失败：同样回退 spawn 值。
        let keep = super::decide_keep(&live, Err("坏了".to_string()), Ok(paths));
        assert!(keep.contains("1/a"));
        assert!(!keep.contains("1/b"));
    }

    #[test]
    fn poisoned_logs_registry_still_usable_via_into_inner() {
        // #58：登记表锁中毒后仍可用（into_inner，与 stop_all 一致）。
        use super::DockerLogsRegistry;
        let reg = DockerLogsRegistry::default();
        let _ = std::thread::scope(|s| {
            s.spawn(|| {
                let _g = reg.0.lock().unwrap();
                panic!("故意毒化锁");
            })
            .join()
        });
        // 锁已毒化：stop_all（into_inner 路径）不应 panic，且表仍可用。
        reg.stop_all();
        reg.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn docker_only_preflight_checks_compose_files_not_run() {
        // #6：走真实的 service_preflight（无需 AppHandle）——docker-only 服务
        // 文件缺失报“compose 文件不存在”，文件存在返回空（不报“没有 run 命令”）。
        let data = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::ScopedDataDir::new(data.path());
        let proj = tempfile::tempdir().unwrap();
        let base = proj.path().to_string_lossy().into_owned();
        let conn = crate::db::open_db().unwrap();
        let id = crate::db::add_project(&conn, &base).unwrap();
        let spec = crate::services::ServiceSpec::from_json(
            r#"{"name":"db","docker":{"files":["docker-compose.yml"],"services":["pg"]}}"#,
        )
        .unwrap();
        crate::db::insert_service(&conn, id, &spec, &crate::services::Derived::manual()).unwrap();
        drop(conn);
        let pf = super::service_preflight(id, "db".into(), None).unwrap();
        assert_eq!(pf, vec!["compose 文件不存在：docker-compose.yml"]);
        std::fs::write(proj.path().join("docker-compose.yml"), "services: {}\n").unwrap();
        let pf = super::service_preflight(id, "db".into(), None).unwrap();
        assert!(pf.is_empty(), "文件存在时应放行：{pf:?}");
    }

    #[test]
    fn steps_only_preflight_still_reports_missing_run() {
        // steps-only（无 run 无 docker）保持原问题，不受 docker 开洞影响。
        let data = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::ScopedDataDir::new(data.path());
        let proj = tempfile::tempdir().unwrap();
        let base = proj.path().to_string_lossy().into_owned();
        let conn = crate::db::open_db().unwrap();
        let id = crate::db::add_project(&conn, &base).unwrap();
        let spec = crate::services::ServiceSpec::from_json(
            r#"{"name":"jobs","steps":[{"name":"build","argv":["make"]}]}"#,
        )
        .unwrap();
        crate::db::insert_service(&conn, id, &spec, &crate::services::Derived::manual()).unwrap();
        drop(conn);
        let pf = super::service_preflight(id, "jobs".into(), None).unwrap();
        assert_eq!(pf, vec!["jobs 没有 run 命令"]);
    }
}

/// 正在跟随的 compose 日志流（按服务 key 登记，供停止与清理）。
#[derive(Default)]
pub struct DockerLogsRegistry(pub std::sync::Mutex<std::collections::HashMap<String, crate::docker::DockerLogs>>);

impl DockerLogsRegistry {
    /// 停掉全部日志跟随并清空登记表。
    ///
    /// App 退出时必须调用：`docker compose logs -f` 是**独立子进程**，父进程一旦退出，
    /// 它会被 launchd 收养继续跑（实测 PPID 变成 1），每次退出就漏一个。
    pub fn stop_all(&self) {
        // #80：drain-collect 后释锁再逐个停，不把锁持过 blocking 的 stop。
        let drained: Vec<crate::docker::DockerLogs> = {
            let mut g = match self.0.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            g.drain().map(|(_, logs)| logs).collect()
        };
        for logs in drained {
            crate::docker::DockerLogs::stop(logs);
        }
    }

    /// 当前登记的流数量（测试断言退出清理用）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.lock().map(|g| g.len()).unwrap_or(0)
    }
}
