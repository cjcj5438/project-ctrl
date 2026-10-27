//! 把「声明 + 本机覆盖 + 观测状态」解算成可以直接执行的启动计划。
//!
//! 这里没有任何 IO 之外的副作用，全部逻辑都能单测：给定声明、覆盖、端口快照，就能断言
//! 最终会跑什么命令、用哪个端口、按什么顺序起。

use crate::db;
use crate::descriptor;
use crate::localstate::{self, Overrides};
use crate::services::{Derived, Health, HealthKind, ServiceSpec, Step};
use crate::supervisor::SpawnSpec;
use crate::supervisor::{Phase, key_of};
use crate::execenv;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 声明来自哪一层。UI 要显示出来，用户才知道改哪个文件会生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// 仓库内 `.project-ctrl.json`
    Descriptor,
    /// 数据库（UI 里点的「采用 / 手工填写」）
    Db,
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub project_id: i64,
    pub base: PathBuf,
    pub spec: ServiceSpec,
    pub derived: Derived,
    pub layer: Layer,
    /// 退出 App 时保留进程。
    pub keep_alive: bool,
    /// 本机覆盖里的端口偏移意图（绝对偏移量，0 表示不偏移）。
    pub port_offset: u16,
    pub override_env_files: Vec<String>,
}

impl Resolved {
    pub fn key(&self) -> String {
        key_of(self.project_id, &self.spec.name)
    }

    /// 服务的实际工作目录（项目根 + 声明的 cwd），并确认它存在。
    pub fn cwd(&self) -> Result<PathBuf, String> {
        let dir = match self.spec.cwd.as_deref() {
            Some(rel) if !rel.is_empty() => self.base.join(rel),
            _ => self.base.clone(),
        };
        if !dir.is_dir() {
            return Err(format!("{}：目录不存在（{}）", self.spec.name, dir.display()));
        }
        Ok(dir)
    }

    /// 该服务的 env 文件绝对路径（声明 + 本机追加）。
    pub fn env_file_paths(&self) -> Vec<PathBuf> {
        let mut names = self.spec.env_files.clone();
        for f in &self.override_env_files {
            if !names.contains(f) {
                names.push(f.clone());
            }
        }
        names.into_iter().map(|n| self.base.join(n)).collect()
    }
}

/// 读层：仓库声明优先，没写才用数据库。
pub fn load_descriptor_specs(base: &Path, project_id: i64) -> Result<Vec<Resolved>, String> {
    let Some(desc) = descriptor::read(base)? else { return Ok(Vec::new()) };
    Ok(desc
        .services
        .into_iter()
        .map(|spec| Resolved {
            project_id,
            base: base.to_path_buf(),
            derived: Derived { origin: "descriptor".into(), ..Derived::default() },
            keep_alive: false,
            port_offset: 0,
            override_env_files: Vec::new(),
            layer: Layer::Descriptor,
            spec,
        })
        .collect())
}

pub fn load_db_specs(conn: &rusqlite::Connection, project_id: i64, base: &Path) -> Result<Vec<Resolved>, String> {
    let rows = db::services_of(conn, project_id).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let spec = ServiceSpec::from_json(&row.config)?;
        out.push(Resolved {
            project_id,
            base: base.to_path_buf(),
            spec,
            derived: Derived::from_json(&row.derived),
            keep_alive: false,
            port_offset: 0,
            override_env_files: Vec::new(),
            layer: Layer::Db,
        });
    }
    Ok(out)
}

/// 该项目全部服务：仓库声明里有就用声明的整套，否则用数据库里的。
///
/// 不做「按名字合并」——一个项目只认一个真相源，否则会出现「改了文件却还在跑数据库里的旧命令」。
pub fn resolve_all(
    conn: &rusqlite::Connection,
    project_id: i64,
    base: &Path,
) -> Result<(Vec<Resolved>, Overrides), String> {
    let mut items = load_descriptor_specs(base, project_id)?;
    if items.is_empty() {
        items = load_db_specs(conn, project_id, base)?;
    }
    let overrides = localstate::load_overrides()?;
    let project_path = base.to_string_lossy().into_owned();
    for item in &mut items {
        let ov = localstate::service_override(&overrides, &project_path, &item.spec.name);
        let (merged, keep) = localstate::apply_override(&item.spec, ov.as_ref());
        item.spec = merged;
        item.keep_alive = keep;
        item.port_offset = ov.as_ref().and_then(|o| o.port_offset).unwrap_or(0);
        item.override_env_files = ov.as_ref().map(|o| o.env_files.clone()).unwrap_or_default();
    }
    Ok((items, overrides))
}

/// 拓扑排序：`depends_on` 先于自身。有环或指向未知服务时报错。
pub fn order_by_deps(items: &[Resolved]) -> Result<Vec<usize>, String> {
    let index: HashMap<&str, usize> =
        items.iter().enumerate().map(|(i, r)| (r.spec.name.as_str(), i)).collect();
    for r in items {
        for d in &r.spec.depends_on {
            if !index.contains_key(d.as_str()) {
                return Err(format!("{}：depends_on 指向不存在的服务 {d}", r.spec.name));
            }
        }
    }
    let mut state: Vec<u8> = vec![0; items.len()]; // 0=未访问 1=访问中 2=完成
    let mut out = Vec::with_capacity(items.len());
    // 显式栈避免深依赖链爆栈
    for start in 0..items.len() {
        if state[start] == 2 {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some((node, dep_i)) = stack.pop() {
            if state[node] == 2 {
                continue;
            }
            state[node] = 1;
            let deps = &items[node].spec.depends_on;
            if dep_i < deps.len() {
                stack.push((node, dep_i + 1));
                let dep = index[deps[dep_i].as_str()];
                match state[dep] {
                    0 => stack.push((dep, 0)),
                    1 => {
                        return Err(format!(
                            "{}：depends_on 存在循环依赖（{}）",
                            items[node].spec.name, deps[dep_i]
                        ));
                    }
                    _ => {}
                }
            } else {
                state[node] = 2;
                out.push(node);
            }
        }
    }
    Ok(out)
}

/// 期望端口：本机覆盖的偏移优先，其次声明里的 listen。
pub fn wanted_port(r: &Resolved) -> Option<u16> {
    r.spec.ports.listen.map(|p| p.saturating_add(r.port_offset))
}

/// 给某个服务挑端口：先试上次用过的那个（URL 稳定），再按期望端口顺延。
pub fn plan_port(project_path: &str, r: &Resolved, extra_taken: &HashSet<u16>) -> Result<Option<(u16, Option<String>)>, String> {
    let Some(wanted) = wanted_port(r) else { return Ok(None) };
    let remembered = localstate::observed_port(project_path, &r.spec.name);
    let mut notes: Vec<String> = Vec::new();

    let mut candidates: Vec<u16> = Vec::new();
    if let Some(p) = remembered {
        candidates.push(p);
    }
    for off in 0..crate::ports::PICK_ATTEMPTS {
        candidates.push(wanted.saturating_add(off));
    }
    candidates.dedup();

    for cand in candidates {
        if extra_taken.contains(&cand) {
            continue;
        }
        if crate::ports::is_free(cand) {
            if cand != wanted {
                let who = crate::ports::holder(wanted).unwrap_or_else(|| "其它进程".to_string());
                if remembered == Some(cand) {
                    notes.push(format!("沿用上次的端口 {cand}（期望 {wanted}）"));
                } else {
                    notes.push(format!("已改用 {cand}（{wanted} 被 {who} 占用）"));
                }
            }
            return Ok(Some((cand, if notes.is_empty() { None } else { Some(notes.join("；")) })));
        }
    }
    Err(format!(
        "端口 {wanted}–{} 全被占用",
        wanted.saturating_add(crate::ports::PICK_ATTEMPTS - 1)
    ))
}

/// 已解算的启动计划（端口已分配、env 已合并、argv 已注入）。
#[derive(Clone, Debug)]
pub struct StartPlan {
    pub spawn: SpawnSpec,
    /// 该服务声明的健康检查。
    pub health: Option<Health>,
    /// 端口实际被顺延的话要写回观测状态。
    pub remember_port: Option<u16>,
    pub depends_on: Vec<String>,
}

pub fn build_plan(
    project_path: &str,
    r: &Resolved,
    extra_taken: &HashSet<u16>,
) -> Result<StartPlan, String> {
    if !r.spec.is_longrun() {
        return Err(format!("{}：没有 run 命令（只有步骤），不能作为长驻服务启动", r.spec.name));
    }
    let cwd = r.cwd()?;
    let argv = r.spec.run_argv();
    let (port, port_note) = match plan_port(project_path, r, extra_taken)? {
        Some((p, note)) => (Some(p), note),
        None => (None, None),
    };

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for f in r.env_file_paths() {
        let text = std::fs::read_to_string(&f).map_err(|e| format!("读取 {} 失败：{e}", f.display()))?;
        for (k, v) in execenv::parse_env_file(&text) {
            env.insert(k, v);
        }
    }
    for (k, v) in &r.spec.env {
        env.insert(k.clone(), v.clone());
    }

    let mut extra: Vec<(String, String)> = Vec::new();
    let mut final_argv = argv.clone();
    // 端口计划必须**始终**执行：`node server.js` 这类命令没有任何框架自动读端口，
    // 只有 `PORT=N` 注入才能让它听在被分配的端口上（顺延与否都一样）。
    if let (Some(wanted), Some(actual)) = (wanted_port(r), port) {
        let (planned, injected) = crate::ports::plan_argv(wanted, actual, &argv, &cwd);
        final_argv = planned;
        extra = injected;
    }
    let pin = r.derived.node_pin.clone();
    let env_pairs = execenv::env_for(pin.as_deref(), &env, &extra);

    let mut notes: Vec<String> = Vec::new();
    if let Some(n) = port_note {
        notes.push(n);
    }
    if !r.spec.ports.requires.is_empty() {
        let missing = crate::ports::missing_requires(&r.spec.ports.requires);
        if !missing.is_empty() {
            notes.push(format!(
                "端口 {} 无人监听：先起依赖服务或容器",
                missing.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("、")
            ));
        }
    }

    Ok(StartPlan {
        spawn: SpawnSpec {
            key: r.key(),
            project_id: r.project_id,
            name: r.spec.name.clone(),
            base: cwd,
            argv: final_argv,
            env: env_pairs,
            phase: Phase::Running,
            step: None,
            port,
            note: if notes.is_empty() { None } else { Some(notes.join("；")) },
            restart: r.spec.restart,
            keep_alive: r.keep_alive,
        },
        health: r.spec.health.clone(),
        remember_port: port,
        depends_on: r.spec.depends_on.clone(),
    })
}

/// 一次性步骤的计划（不做端口分配）。
pub fn build_step_plan(r: &Resolved, step: &Step) -> Result<SpawnSpec, String> {
    let cwd = r.cwd()?;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for f in r.env_file_paths() {
        let text = std::fs::read_to_string(&f).map_err(|e| format!("读取 {} 失败：{e}", f.display()))?;
        for (k, v) in execenv::parse_env_file(&text) {
            env.insert(k, v);
        }
    }
    for (k, v) in &r.spec.env {
        env.insert(k.clone(), v.clone());
    }
    let pin = r.derived.node_pin.clone();
    Ok(SpawnSpec {
        key: r.key(),
        project_id: r.project_id,
        name: r.spec.name.clone(),
        base: cwd,
        argv: step.argv.clone(),
        env: execenv::env_for(pin.as_deref(), &env, &[]),
        phase: Phase::Step,
        step: Some(step.name.clone()),
        port: None,
        note: None,
        restart: crate::services::Restart::Never,
        keep_alive: false,
    })
}

/// 就绪探测：端口可连（`Port`）或 HTTP 2xx/3xx（`Http`）。
pub fn wait_ready(port: Option<u16>, health: Option<&Health>, timeout: Duration) -> bool {
    let kind = health.map(|h| h.kind).unwrap_or(HealthKind::Port);
    let deadline = std::time::Instant::now() + timeout;
    let path = health.and_then(|h| h.path.clone()).unwrap_or_else(|| "/".to_string());
    loop {
        let ok = match kind {
            HealthKind::Port => port.map(|p| !crate::ports::is_free(p)).unwrap_or(false),
            HealthKind::Http => port
                .map(|p| http_ok(p, &path))
                .unwrap_or(false),
        };
        if ok {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 最小 HTTP GET：不引第三方依赖，只判断首行状态码。任何连接错误都算「未就绪」。
pub fn http_ok(port: u16, path: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(800)));
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = match s.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    let Some(line) = head.lines().next() else { return false };
    // 形如 `HTTP/1.1 200 OK`
    let mut parts = line.split_whitespace();
    if parts.next().map(|p| p.starts_with("HTTP/")).unwrap_or(false) {
        if let Some(code) = parts.next().and_then(|c| c.parse::<u16>().ok()) {
            return (200..400).contains(&code);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{Ports as ServicePorts, Restart};

    fn res(name: &str, deps: &[&str]) -> Resolved {
        Resolved {
            project_id: 1,
            base: PathBuf::from("/tmp/proj"),
            spec: ServiceSpec {
                name: name.into(),
                cwd: None,
                run: Some(vec!["echo".into(), name.into()]),
                steps: vec![],
                env_files: vec![],
                env: BTreeMap::new(),
                ports: ServicePorts::default(),
                health: None,
                depends_on: deps.iter().map(|s| s.to_string()).collect(),
                restart: Restart::OnFailure,
                docker: None,
            },
            derived: Derived::default(),
            layer: Layer::Db,
            keep_alive: false,
            port_offset: 0,
            override_env_files: vec![],
        }
    }

    #[test]
    fn topo_order_puts_dependencies_first() {
        let items = vec![res("web", &["api"]), res("api", &["db"]), res("db", &[])];
        let order = order_by_deps(&items).unwrap();
        let names: Vec<&str> = order.iter().map(|i| items[*i].spec.name.as_str()).collect();
        assert_eq!(names, vec!["db", "api", "web"]);
    }

    #[test]
    fn cycle_is_reported() {
        let items = vec![res("a", &["b"]), res("b", &["a"])];
        assert!(order_by_deps(&items).unwrap_err().contains("循环依赖"));
    }

    #[test]
    fn unknown_dependency_is_reported() {
        let items = vec![res("a", &["ghost"])];
        assert!(order_by_deps(&items).unwrap_err().contains("ghost"));
    }

    #[test]
    fn wanted_port_applies_override_offset() {
        let mut r = res("a", &[]);
        r.spec.ports.listen = Some(5000);
        assert_eq!(wanted_port(&r), Some(5000));
        r.port_offset = 10;
        assert_eq!(wanted_port(&r), Some(5010));
    }

    #[test]
    fn plan_port_prefers_remembered_port_when_free() {
        let mut r = res("a", &[]);
        r.spec.ports.listen = Some(0);
        // 用一个几乎肯定空闲的高端口，避免与真实服务冲突
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free = held.local_addr().unwrap().port();
        drop(held);
        r.spec.ports.listen = Some(free);
        let (p, note) = plan_port("/nonexistent-project", &r, &HashSet::new()).unwrap().unwrap();
        assert_eq!(p, free);
        assert!(note.is_none(), "端口未被占用时不该有任何 note：{note:?}");
    }

    #[test]
    fn http_ok_is_false_when_nothing_listens() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(!http_ok(port, "/health"));
    }

    #[test]
    fn steps_only_service_cannot_start_as_longrun() {
        let mut r = res("a", &[]);
        r.spec.run = None;
        r.spec.steps = vec![Step { name: "build".into(), argv: vec!["true".into()], timeout_s: 10 }];
        let err = build_plan("/tmp", &r, &HashSet::new()).unwrap_err();
        assert!(err.contains("只有步骤"), "{err}");
    }

    #[test]
    fn build_plan_injects_port_env_and_merges_env_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env.local"), "FROM_FILE=1\nSHARED=file\n").unwrap();
        let mut r = res("a", &[]);
        r.base = dir.path().to_path_buf();
        r.spec.env_files = vec![".env.local".into()];
        r.spec.env.insert("SHARED".into(), "declared".into());
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        drop(held);
        r.spec.ports.listen = Some(port);
        let plan = build_plan("/tmp", &r, &HashSet::new()).unwrap();
        let get = |k: &str| plan.spawn.env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get("FROM_FILE"), Some("1"));
        assert_eq!(get("SHARED"), Some("declared"), "声明应覆盖 env 文件");
        assert_eq!(plan.remember_port, Some(port));
    }

    #[test]
    fn port_env_is_injected_even_when_the_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = res("a", &[]);
        r.base = dir.path().to_path_buf();
        r.spec.run = Some(vec!["node".into(), "server.js".into()]);
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        drop(held);
        r.spec.ports.listen = Some(port);
        let plan = build_plan("/tmp", &r, &HashSet::new()).unwrap();
        assert_eq!(plan.spawn.port, Some(port));
        let injected = plan.spawn.env.iter().find(|(k, _)| k == "PORT").map(|(_, v)| v.as_str());
        assert_eq!(injected, Some(port.to_string().as_str()), "未顺延时也必须注入 PORT");
    }

    #[test]
    fn build_step_plan_uses_step_argv_and_step_phase() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = res("a", &[]);
        r.base = dir.path().to_path_buf();
        let step = Step { name: "migrate".into(), argv: vec!["pnpm".into(), "db:migrate".into()], timeout_s: 60 };
        let plan = build_step_plan(&r, &step).unwrap();
        assert_eq!(plan.argv, vec!["pnpm", "db:migrate"]);
        assert_eq!(plan.phase, Phase::Step);
        assert_eq!(plan.step.as_deref(), Some("migrate"));
        assert_eq!(plan.restart, Restart::Never, "步骤不该被自动重启");
    }

    #[test]
    fn cwd_escape_is_rejected_before_spawn() {
        let mut r = res("a", &[]);
        r.spec.cwd = Some("../outside".into());
        assert!(r.cwd().is_err());
    }
}
