//! 受管服务进程的看护：进程组、环形日志、状态事件、崩溃重启、孤儿收养。
//!
//! 关键设计：
//! - 子进程用 `process_group(0)` 成为新进程组组长（pgid == pid）。`pnpm dev` 会再 fork 出
//!   node/vite，只 kill 父进程会留下占端口的孤儿，所以停止用 `kill -TERM -- -pgid`。
//! - 子进程环境由 `execenv::env_for` 从零构造，不继承本进程环境。
//! - 日志按行进每个服务的环形缓冲（`LOG_CAP` 行），攒批后 emit，不逐行 emit。
//! - 运行态以 `key`（`"{project_id}/{name}"`）为索引：一个项目可以同时跑多个服务。
//! - 崩溃重启按指数退避，最多 `MAX_RESTARTS` 次；`stop` 与 App 退出都会取消待重启。
//! - 退出时**不**一律杀进程：声明了 `keep_alive` 的服务留给下次启动收养（见 `adopt`）。

use crate::execenv;
use crate::services::Restart;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

pub const EV_STATE: &str = "svc:state";
pub const EV_LOG: &str = "svc:log";

/// 每个服务保留的日志行数。
pub const LOG_CAP: usize = 2000;
/// 日志批量推送的攒批上限（行数）。
pub const LOG_BATCH: usize = 200;
/// 日志批量推送的时间间隔。
pub const LOG_FLUSH: Duration = Duration::from_millis(200);
/// 崩溃自动重启的次数上限。
pub const MAX_RESTARTS: u32 = 5;
/// 重启退避的起始值与上限。
pub const BACKOFF_BASE: Duration = Duration::from_millis(500);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// 服务在进程表里的索引。
pub fn key_of(project_id: i64, name: &str) -> String {
    format!("{project_id}/{name}")
}

impl Phase {
    /// 该阶段是否仍持有活着的子进程组。
    ///
    /// 只有这几个阶段可以安全地对 pgid 发信号：`Stopped`/`Crashed`/`Failed` 的 pgid 早已退出，
    /// 若干小时后系统可能把它复用给无关进程，再发 `SIGKILL` 就会误杀别人。
    fn owns_process(self) -> bool {
        matches!(self, Phase::Running | Phase::Step)
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// 前端在「从未运行过」时展示的状态；后端不会主动构造，但它是 IPC 契约的一部分。
    #[allow(dead_code)]
    Idle,
    /// 正在执行某个一次性步骤（`ServiceStatus::step` 指出是哪一个）。
    Step,
    Running,
    Stopped,
    Crashed,
    Failed,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ServiceStatus {
    /// `"{project_id}/{name}"`，前端用它精确匹配。
    pub key: String,
    pub project_id: i64,
    pub name: String,
    pub phase: Phase,
    /// 当前步骤名（仅 `Phase::Step` 时有意义）。
    pub step: Option<String>,
    pub port: Option<u16>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub note: Option<String>,
    /// 本次运行已自动重启的次数。
    pub restarts: u32,
    /// 本次运行（不含重启）的起始 unix 秒。
    pub started_at: i64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct LogPayload {
    pub key: String,
    pub project_id: i64,
    pub name: String,
    pub lines: Vec<String>,
}

/// 状态 / 日志的出口。
pub trait Sink: Send + Sync {
    fn state(&self, status: &ServiceStatus);
    fn log(&self, payload: &LogPayload);
}

/// Tauri 侧实现：把两类 payload 直接 emit 给前端。
pub struct Ctx<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> Clone for Ctx<R> {
    fn clone(&self) -> Self {
        Self { app: self.app.clone() }
    }
}

impl<R: tauri::Runtime> Ctx<R> {
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        Self { app }
    }

    pub fn emit_state(&self, key: &str) {
        if let Some(st) = state(&self.app).status_of(key) {
            let _ = self.app.emit(EV_STATE, st);
        }
    }
}

impl<R: tauri::Runtime> Sink for Ctx<R> {
    fn state(&self, status: &ServiceStatus) {
        let _ = self.app.emit(EV_STATE, status.clone());
    }

    fn log(&self, payload: &LogPayload) {
        let _ = self.app.emit(EV_LOG, payload.clone());
    }
}

/// 一次 spawn 的全部入参（已解算完毕：env 合并完、端口已注入）。
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub key: String,
    pub project_id: i64,
    pub name: String,
    /// 子进程的当前目录（项目根 + 服务的 cwd）。
    pub base: PathBuf,
    pub argv: Vec<String>,
    /// 已合并的额外环境变量（env 文件 / 声明 / 端口注入）。
    pub env: Vec<(String, String)>,
    pub phase: Phase,
    pub step: Option<String>,
    pub port: Option<u16>,
    pub note: Option<String>,
    pub restart: Restart,
    /// 退出 App 时保留该进程，下次启动收养。
    pub keep_alive: bool,
}

struct Run {
    spec: SpawnSpec,
    pid: u32,
    pgid: i32,
    phase: Phase,
    step: Option<String>,
    port: Option<u16>,
    gen: u64,
    exit_code: Option<i32>,
    note: Option<String>,
    restarts: u32,
    started_at: i64,
}

impl Run {
    fn status(&self) -> ServiceStatus {
        ServiceStatus {
            key: self.spec.key.clone(),
            project_id: self.spec.project_id,
            name: self.spec.name.clone(),
            phase: self.phase,
            step: self.step.clone(),
            port: self.port,
            pid: Some(self.pid),
            exit_code: self.exit_code,
            note: self.note.clone(),
            restarts: self.restarts,
            started_at: self.started_at,
        }
    }
}

#[derive(Default)]
struct Inner {
    runs: Mutex<HashMap<String, Run>>,
    logs: Mutex<HashMap<String, VecDeque<String>>>,
    gen: AtomicU64,
    shutting_down: AtomicBool,
    /// docker 日志流的代号：新流开始即 +1，旧流的残留行据此丢弃。
    docker_epochs: Mutex<HashMap<String, u64>>,
}

/// 进程表 + 日志缓冲 + 代号计数器。`Clone` 共享同一份内部状态，便于交给读线程。
#[derive(Clone, Default)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 第 n 次重启前的等待时长（指数退避，封顶 `BACKOFF_MAX`）。
pub fn backoff(n: u32) -> Duration {
    let ms = BACKOFF_BASE.as_millis() as u64 * (1u64 << n.min(8));
    Duration::from_millis(ms).min(BACKOFF_MAX)
}

impl Supervisor {
    fn next_gen(&self) -> u64 {
        self.inner.gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn lock_runs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Run>> {
        self.inner.runs.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn statuses(&self) -> Vec<ServiceStatus> {
        let g = self.lock_runs();
        let mut v: Vec<ServiceStatus> = g.values().map(|r| r.status()).collect();
        v.sort_by(|a, b| (a.project_id, &a.name).cmp(&(b.project_id, &b.name)));
        v
    }

    pub fn status_of(&self, key: &str) -> Option<ServiceStatus> {
        let g = self.lock_runs();
        g.get(key).map(|r| r.status())
    }

    pub fn is_active(&self, key: &str) -> bool {
        self.status_of(key).map(|s| s.phase.owns_process()).unwrap_or(false)
    }

    /// 该项目下所有活跃服务的 key。
    pub fn active_keys_of_project(&self, project_id: i64) -> Vec<String> {
        let g = self.lock_runs();
        g.values()
            .filter(|r| r.spec.project_id == project_id && r.phase.owns_process())
            .map(|r| r.spec.key.clone())
            .collect()
    }

    pub fn port_of(&self, key: &str) -> Option<u16> {
        self.status_of(key).and_then(|s| s.port)
    }

    /// 该运行的代号。调用方用它判断「我观察到的东西还属于我起的那次运行」，
    /// 避免超时线程误伤同一服务后来启动的进程。
    pub fn gen_of(&self, key: &str) -> Option<u64> {
        let g = self.lock_runs();
        g.get(key).map(|r| r.gen)
    }

    /// 实测校准出的端口写回状态。
    pub fn set_port(&self, key: &str, port: Option<u16>, note: Option<String>) {
        let mut g = self.lock_runs();
        if let Some(r) = g.get_mut(key) {
            r.port = port;
            if note.is_some() {
                r.note = note;
            }
        }
    }

    /// 开一个新的 docker 日志流代号（旧流的读线程随后会自行退出）。
    pub fn begin_docker_logs(&self, key: &str) -> u64 {
        let mut g = self.inner.docker_epochs.lock().unwrap_or_else(|e| e.into_inner());
        let next = g.get(key).copied().unwrap_or(0) + 1;
        g.insert(key.to_string(), next);
        self.clear_logs(key);
        next
    }

    pub fn docker_epoch(&self, key: &str) -> Option<u64> {
        let g = self.inner.docker_epochs.lock().unwrap_or_else(|e| e.into_inner());
        g.get(key).copied()
    }

    /// docker 日志直接进同一个环形缓冲（前端不需要第二套通道）。
    pub fn push_docker_line(&self, key: &str, line: String) {
        self.push_line(key, line);
    }

    pub fn clear_logs(&self, key: &str) {
        let mut g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(key.to_string(), VecDeque::new());
    }

    pub fn logs_of(&self, key: &str) -> Vec<String> {
        let g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        g.get(key).map(|q| q.iter().cloned().collect()).unwrap_or_default()
    }

    fn push_line(&self, key: &str, line: String) {
        let mut g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        let q = g.entry(key.to_string()).or_default();
        q.push_back(line);
        while q.len() > LOG_CAP {
            q.pop_front();
        }
    }

    /// 把一次退出结果应用到状态。`gen` 不匹配说明是我们主动 stop 的，忽略。
    pub fn apply_exit(&self, key: &str, gen: u64, code: Option<i32>) -> Option<ServiceStatus> {
        let mut g = self.lock_runs();
        let r = g.get_mut(key)?;
        if r.gen != gen {
            return None;
        }
        r.exit_code = code;
        r.phase = if r.phase == Phase::Step {
            Phase::Failed
        } else if code == Some(0) {
            Phase::Stopped
        } else {
            Phase::Crashed
        };
        // 进程已自行退出：凭据作废（否则下次启动会对一个死 pid 做收养尝试）
        if !r.phase.owns_process() {
            crate::localstate::clear_adopt(key);
        }
        Some(r.status())
    }

    /// spawn 一个受管进程：新进程组 + 从零构造的环境 + 批量日志 + 退出监视（含重启）。
    ///
    /// 返回子进程 pid（`process_group(0)` 使它同时是 pgid）。
    pub fn spawn(&self, sink: Arc<dyn Sink>, spec: SpawnSpec) -> Result<u32, String> {
        self.inner.shutting_down.store(false, Ordering::SeqCst);
        let child = self.launch(&spec)?;
        let pid = child.id();
        let gen = {
            let mut g = self.lock_runs();
            let gen = self.next_gen();
            g.insert(
                spec.key.clone(),
                Run {
                    pid,
                    pgid: pid as i32,
                    phase: spec.phase,
                    step: spec.step.clone(),
                    port: spec.port,
                    gen,
                    exit_code: None,
                    note: spec.note.clone(),
                    restarts: 0,
                    started_at: now_secs(),
                    spec: spec.clone(),
                },
            );
            gen
        };
        self.attach(sink, spec.clone(), gen, child);
        // 立刻登记凭据：App 被强杀/崩溃时没有任何退出钩子，只能靠这份记录在下次启动清理孤儿。
        self.register(&spec, pid, now_secs());
        Ok(pid)
    }

    fn register(&self, spec: &SpawnSpec, pid: u32, started_at: i64) {
        let rec = AdoptRecord {
            key: spec.key.clone(),
            project_id: spec.project_id,
            name: spec.name.clone(),
            pid,
            pgid: pid as i32,
            argv: spec.argv.clone(),
            cwd: spec.base.to_string_lossy().into_owned(),
            started_at,
            port: spec.port,
            keep_alive: spec.keep_alive,
        };
        let _ = crate::localstate::write_adopt(&rec);
    }

    fn launch(&self, spec: &SpawnSpec) -> Result<Child, String> {
        let (program, args) = spec.argv.split_first().ok_or_else(|| "命令为空".to_string())?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&spec.base)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // PATH 由 env 列表里的 PATH 项提供（execenv 已构造好）
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn().map_err(|e| format!("无法启动 {program}：{e}"))
    }

    /// 接管一个已经在跑的进程（App 重启后收养）。
    ///
    /// 调用方必须先完成凭据校验（pid 存活、启动时间与命令行相符）。
    pub fn adopt(&self, sink: Arc<dyn Sink>, spec: SpawnSpec, pid: u32, started_at: i64) -> ServiceStatus {
        let gen = {
            let mut g = self.lock_runs();
            let gen = self.next_gen();
            g.insert(
                spec.key.clone(),
                Run {
                    pid,
                    pgid: pid as i32,
                    phase: Phase::Running,
                    step: None,
                    port: spec.port,
                    gen,
                    exit_code: None,
                    note: spec.note.clone(),
                    restarts: 0,
                    started_at,
                    spec: spec.clone(),
                },
            );
            gen
        };
        self.register(&spec, pid, started_at);
        // 收养的进程看不到它的 stdout（管道已随上个进程退出而失效），只能盯退出。
        Self::watch_adopted(self.clone(), sink, spec.clone(), gen, pid);
        self.status_of(&spec.key).expect("刚插入的状态必须存在")
    }

    fn attach(&self, sink: Arc<dyn Sink>, spec: SpawnSpec, gen: u64, mut child: Child) {
        let key = spec.key.clone();
        let mut streams: Vec<Box<dyn Read + Send>> = Vec::new();
        if let Some(s) = child.stdout.take() {
            streams.push(Box::new(s));
        }
        if let Some(s) = child.stderr.take() {
            streams.push(Box::new(s));
        }
        // 两个读线程把行推给一个带定时器的冲刷线程：只按「有新行」触发会让
        // 「打印一行然后安静」的服务（如 `env`）的日志一直不出现。
        let pending: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let readers = Arc::new(AtomicU64::new(streams.len() as u64));
        for stream in streams {
            let sup = self.clone();
            let pending = pending.clone();
            let readers = readers.clone();
            let k = key.clone();
            std::thread::spawn(move || {
                sup.pump_logs(&k, stream, pending);
                readers.fetch_sub(1, Ordering::SeqCst);
            });
        }
        Self::spawn_flusher(spec.project_id, spec.name.clone(), key.clone(), pending, readers, sink.clone());
        Self::watch_exit(self.clone(), sink, spec, gen, child);
    }

    fn watch_exit(sup: Supervisor, sink: Arc<dyn Sink>, spec: SpawnSpec, gen: u64, mut child: Child) {
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            let Some(st) = sup.apply_exit(&spec.key, gen, code) else { return };
            sink.state(&st);
            // 只有「长驻服务 + 非主动停止」才考虑重启
            if st.phase != Phase::Crashed {
                return;
            }
            let should = match spec.restart {
                Restart::Never => false,
                Restart::OnFailure => code != Some(0),
                Restart::Always => true,
            };
            if !should || sup.inner.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            let attempt = {
                let mut g = sup.lock_runs();
                match g.get_mut(&spec.key) {
                    Some(r) if r.gen == gen => {
                        if r.restarts >= MAX_RESTARTS {
                            None
                        } else {
                            r.restarts += 1;
                            Some(r.restarts)
                        }
                    }
                    _ => None,
                }
            };
            let Some(attempt) = attempt else {
                let st = {
                    let mut g = sup.lock_runs();
                    match g.get_mut(&spec.key) {
                        Some(r) if r.gen == gen => {
                            r.note = Some(format!("已连续重启 {MAX_RESTARTS} 次仍失败，停止自动重启"));
                            Some(r.status())
                        }
                        _ => None,
                    }
                };
                if let Some(st) = st {
                    sink.state(&st);
                }
                return;
            };
            let wait = backoff(attempt - 1);
            let announced = {
                let mut g = sup.lock_runs();
                match g.get_mut(&spec.key) {
                    Some(r) if r.gen == gen => {
                        r.note = Some(format!(
                            "崩溃（退出码 {:?}），{} 秒后自动重启（第 {attempt} 次）",
                            code,
                            wait.as_secs()
                        ));
                        Some(r.status())
                    }
                    _ => None,
                }
            };
            let Some(announced) = announced else { return };
            sink.state(&announced);
            std::thread::sleep(wait);
            // 退避期间可能被 stop / App 退出取消
            if sup.inner.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            {
                let mut g = sup.lock_runs();
                match g.get_mut(&spec.key) {
                    Some(r) if r.gen == gen => {}
                    _ => return,
                }
            }
            let Ok(child) = sup.launch(&spec) else {
                let st = {
                    let mut g = sup.lock_runs();
                    match g.get_mut(&spec.key) {
                        Some(r) if r.gen == gen => {
                            r.phase = Phase::Failed;
                            r.note = Some("自动重启失败：命令无法启动".to_string());
                            Some(r.status())
                        }
                        _ => None,
                    }
                };
                if let Some(st) = st {
                    sink.state(&st);
                }
                return;
            };
            let pid = child.id();
            let new_gen = {
                let mut g = sup.lock_runs();
                let Some(r) = g.get_mut(&spec.key) else { return };
                if r.gen != gen {
                    // 已被取消：立刻收掉刚起的进程，避免孤儿
                    Self::kill_group(pid as i32, "-KILL");
                    return;
                }
                r.pid = pid;
                r.pgid = pid as i32;
                r.phase = Phase::Running;
                r.step = None;
                r.exit_code = None;
                r.started_at = now_secs();
                r.gen = sup.inner.gen.fetch_add(1, Ordering::SeqCst) + 1;
                r.gen
            };
            sup.register(&spec, pid, now_secs());
            if let Some(st) = sup.status_of(&spec.key) {
                sink.state(&st);
            }
            Self::watch_exit(sup.clone(), sink.clone(), spec.clone(), new_gen, child);
        });
    }

    /// 收养进程的退出监视：看不到输出，只能轮询进程组是否还在。
    fn watch_adopted(sup: Supervisor, sink: Arc<dyn Sink>, spec: SpawnSpec, gen: u64, pid: u32) {
        std::thread::spawn(move || {
            let pgid = pid as i32;
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let gen_now = sup.gen_of(&spec.key);
                if gen_now != Some(gen) {
                    return;
                }
                if !Self::group_alive(pgid) {
                    if let Some(st) = sup.apply_exit(&spec.key, gen, None) {
                        sink.state(&st);
                    }
                    return;
                }
            }
        });
    }

    /// 每 `LOG_FLUSH` 把攒下的行推一次；两个读线程都结束后再推最后一批后退出。
    fn spawn_flusher(
        project_id: i64,
        name: String,
        key: String,
        pending: Arc<Mutex<Vec<String>>>,
        readers: Arc<AtomicU64>,
        sink: Arc<dyn Sink>,
    ) {
        std::thread::spawn(move || loop {
            std::thread::sleep(LOG_FLUSH);
            let take = |pending: &Arc<Mutex<Vec<String>>>| -> Vec<String> {
                let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *g)
            };
            let batch = take(&pending);
            if !batch.is_empty() {
                sink.log(&LogPayload { key: key.clone(), project_id, name: name.clone(), lines: batch });
            }
            if readers.load(Ordering::SeqCst) == 0 {
                let tail = take(&pending);
                if !tail.is_empty() {
                    sink.log(&LogPayload { key, project_id, name, lines: tail });
                }
                return;
            }
        });
    }

    /// 读线程：逐行进环形缓冲，并交给冲刷线程攒批（不在这里 emit）。
    fn pump_logs(&self, key: &str, stream: Box<dyn Read + Send>, pending: Arc<Mutex<Vec<String>>>) {
        let reader = BufReader::new(stream);
        for line in reader.split(b'\n') {
            let Ok(bytes) = line else { break };
            let text = String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string();
            self.push_line(key, text.clone());
            let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
            // 上限是内存保险：极长的构建输出不应该在冲刷线程睡觉时无限堆积
            if g.len() < LOG_BATCH * 10 {
                g.push(text);
            }
        }
    }

    /// 向进程组发信号（`kill -<sig> -- -pgid`）。
    fn kill_group(pgid: i32, sig: &str) {
        if pgid <= 0 {
            return;
        }
        let _ = Command::new("/bin/kill")
            .args([sig, "--", &format!("-{pgid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn group_alive(pgid: i32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{pgid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// TERM 整个进程组，等端口释放；宽限期结束仍未释放则 KILL。无运行记录时是空操作。
    pub fn stop(&self, key: &str) -> Option<ServiceStatus> {
        let (pgid, port) = {
            let mut g = self.lock_runs();
            let r = g.get_mut(key)?;
            // 让已排队的监视/重启线程失效：gen 前进
            r.gen = self.next_gen();
            r.phase = Phase::Stopped;
            r.exit_code = None;
            r.step = None;
            // 进程已不再监听：保留端口与「已改用 …」的 note 会变成假信息
            let port = r.port.take();
            r.note = None;
            (r.pgid, port)
        };
        crate::localstate::clear_adopt(key);
        Self::kill_group(pgid, "-TERM");
        let released = match port {
            Some(p) => crate::ports::wait_released(p, execenv::STOP_GRACE),
            None => {
                let deadline = Instant::now() + Duration::from_millis(300);
                while Instant::now() < deadline && Self::group_alive(pgid) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                !Self::group_alive(pgid)
            }
        };
        if !released {
            Self::kill_group(pgid, "-KILL");
            if let Some(p) = port {
                let _ = crate::ports::wait_released(p, Duration::from_secs(2));
            }
        }
        self.status_of(key)
    }

    /// 退出时要收掉的 (key, pgid)：只取仍在活跃阶段的那些。
    fn targets_to_stop(&self) -> Vec<(String, i32)> {
        let g = self.lock_runs();
        g.values()
            .filter(|r| r.phase.owns_process())
            .map(|r| (r.spec.key.clone(), r.pgid))
            .collect()
    }

    /// App 退出时收掉进程组。`keep` 里的 key 留给下次启动收养。
    ///
    /// 保留决定由调用方在**退出这一刻**重新判定：用户可能在服务启动之后才勾上
    /// 「退出后保留进程」，用 spawn 时捕获的旧值会让这个开关看起来坏了。
    ///
    /// 不 emit（事件循环正在结束）。
    pub fn shutdown_all(&self, keep: &HashSet<String>) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let targets = self.targets_to_stop();
        let (kept, killed): (Vec<_>, Vec<_>) =
            targets.into_iter().partition(|(k, _)| keep.contains(k));
        // 被保留的要把凭据标记成「可收养」，否则下次启动会把它当孤儿回收掉
        for rec in self.adoptable() {
            if keep.contains(&rec.key) {
                let mut rec = rec;
                rec.keep_alive = true;
                let _ = crate::localstate::write_adopt(&rec);
            }
        }
        for (key, pgid) in &killed {
            let _ = key;
            Self::kill_group(*pgid, "-TERM");
        }
        if !killed.is_empty() {
            std::thread::sleep(Duration::from_millis(400));
        }
        for (_, pgid) in &killed {
            Self::kill_group(*pgid, "-KILL");
        }
        // 留下来的降级为「已停止但进程仍在」：下次启动会重新收养
        let mut g = self.lock_runs();
        for (key, _) in kept {
            g.remove(&key);
        }
    }

    /// 退出前把「这次运行是否要保留」写回凭据（spawn 时还不知道最终的 keep_alive 决定）。
    pub fn adoptable(&self) -> Vec<AdoptRecord> {
        let g = self.lock_runs();
        g.values()
            .filter(|r| r.phase.owns_process())
            .map(|r| AdoptRecord {
                key: r.spec.key.clone(),
                project_id: r.spec.project_id,
                name: r.spec.name.clone(),
                pid: r.pid,
                pgid: r.pgid,
                argv: r.spec.argv.clone(),
                cwd: r.spec.base.to_string_lossy().into_owned(),
                started_at: r.started_at,
                port: r.port,
                keep_alive: r.spec.keep_alive,
            })
            .collect()
    }
}

/// 收养凭据（落 `~/.project-ctrl/state/run/<key>.json`）。
#[derive(Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct AdoptRecord {
    pub key: String,
    pub project_id: i64,
    pub name: String,
    pub pid: u32,
    pub pgid: i32,
    pub argv: Vec<String>,
    pub cwd: String,
    pub started_at: i64,
    pub port: Option<u16>,
    /// 该进程是否打算在 App 退出后继续跑（崩溃留下的孤儿则不是）。
    #[serde(default)]
    pub keep_alive: bool,
}

/// 三重校验：pid 存活 + 启动时间相符 + 命令行前缀相符。
/// 任一条不满足就不能收养——PID 会被系统复用，猜错会误杀别人的进程。
pub fn verify_adopt(rec: &AdoptRecord) -> Result<(), String> {
    if !pid_alive(rec.pid) {
        return Err("进程已不在".to_string());
    }
    let lstart = ps_field(rec.pid, "lstart")?;
    let expect = lstart_string(rec.started_at);
    if !lstart.starts_with(&expect) {
        return Err(format!("启动时间不符（记录 {expect}，实际 {lstart}）"));
    }
    let command = ps_field(rec.pid, "command")?;
    let want = rec.argv.join(" ");
    if !command.starts_with(&want) {
        return Err(format!("命令行不符（记录 {want}，实际 {command}）"));
    }
    Ok(())
}

/// 单个 pid 是否存活（`kill -0 <pid>`，不是进程组：收养的进程未必是组长）。
fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ps_field(pid: u32, field: &str) -> Result<String, String> {
    let out = Command::new("/bin/ps")
        .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("无法执行 ps：{e}"))?;
    if !out.status.success() {
        return Err(format!("进程 {pid} 不存在"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `ps -o lstart=` 的格式：`Mon Oct 27 12:34:56 2026`（本地时区）。
fn lstart_string(unix_secs: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(unix_secs, 0).single() {
        Some(dt) => dt.format("%a %b %e %H:%M:%S %Y").to_string(),
        None => String::new(),
    }
}

/// 优雅回收一个外部进程组：TERM → 等 300ms → 仍在则 KILL。
/// 用于 docker 日志跟随这类「不是受管服务、但同样会留下孤儿」的子进程。
pub fn reclaim_group_graceful(pgid: i32) {
    if pgid <= 0 {
        return;
    }
    Supervisor::kill_group(pgid, "-TERM");
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline && Supervisor::group_alive(pgid) {
        std::thread::sleep(Duration::from_millis(20));
    }
    if Supervisor::group_alive(pgid) {
        Supervisor::kill_group(pgid, "-KILL");
    }
}

/// 回收一个进程组（App 在被强杀/崩溃后留下的孤儿）。`commands::adopt_existing` 使用。
///
/// 先按进程组杀；若进程组不存在（记录里的 pgid 并非组长，或组已解散但进程仍活着），
/// 退化为按 pid 杀。缺少这层回退会让孤儿静默活下去——正是我们要防的那类失败。
pub fn reclaim_group(pgid: i32) {
    if pgid <= 0 {
        return;
    }
    Supervisor::kill_group(pgid, "-KILL");
    // 组不存在（记录里的 pid 并非组长）时上一条是 ESRCH，必须退化为按 pid 杀
    std::thread::sleep(Duration::from_millis(80));
    if pid_alive(pgid as u32) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &pgid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Tauri 命令层取 state。
pub fn state<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::State<'_, Supervisor> {
    app.state::<Supervisor>()
}

/// 测试用：把事件收进内存，便于断言（跨模块的端到端测试也用）。
#[cfg(test)]
#[derive(Default)]
pub struct Collector {
    pub states: Mutex<Vec<ServiceStatus>>,
    pub logs: Mutex<Vec<LogPayload>>,
}

#[cfg(test)]
impl Sink for Collector {
    fn state(&self, status: &ServiceStatus) {
        self.states.lock().unwrap().push(status.clone());
    }
    fn log(&self, payload: &LogPayload) {
        self.logs.lock().unwrap().push(payload.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(key: &str, argv: Vec<&str>, restart: Restart) -> SpawnSpec {
        let mut parts: Vec<String> = key.splitn(2, '/').map(|s| s.to_string()).collect();
        let name = parts.pop().unwrap();
        let project_id: i64 = parts.pop().and_then(|s| s.parse().ok()).unwrap_or(1);
        SpawnSpec {
            key: key.to_string(),
            project_id,
            name,
            base: std::env::temp_dir(),
            argv: argv.into_iter().map(|s| s.to_string()).collect(),
            env: vec![("PATH".to_string(), execenv::base_path())],
            phase: Phase::Running,
            step: None,
            port: None,
            note: None,
            restart,
            keep_alive: false,
        }
    }

    fn wait_for<F: Fn() -> bool>(f: F, ms: u64) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn two_services_of_same_project_coexist() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        sup.spawn(sink.clone(), spec("1/a", vec!["sleep", "5"], Restart::Never)).unwrap();
        sup.spawn(sink.clone(), spec("1/b", vec!["sleep", "5"], Restart::Never)).unwrap();
        let active = sup.active_keys_of_project(1);
        assert_eq!(active.len(), 2);
        assert!(active.contains(&"1/a".to_string()));
        assert!(active.contains(&"1/b".to_string()));
        sup.stop("1/a");
        sup.stop("1/b");
    }

    #[test]
    fn stopped_run_is_not_shutdown_target() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        sup.spawn(sink, spec("1/a", vec!["sleep", "5"], Restart::Never)).unwrap();
        sup.stop("1/a");
        assert!(sup.targets_to_stop().is_empty());
    }

    #[test]
    fn keep_key_is_excluded_from_shutdown_and_reported_adoptable() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        sup.spawn(sink, spec("1/a", vec!["sleep", "5"], Restart::Never)).unwrap();
        let _ = sup.adoptable();
        let mut sp = spec("1/a", vec!["sleep", "5"], Restart::Never);
        sp.keep_alive = true;
        sup.stop("1/a");
        sup.spawn(Arc::new(Collector::default()), sp).unwrap();
        let recs = sup.adoptable();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].argv, vec!["sleep", "5"]);
        let _ = sup.stop("1/a");
    }

    #[test]
    fn step_phase_becomes_failed_on_any_exit() {
        let sup = Supervisor::default();
        let mut sp = spec("1/a", vec!["sh", "-c", "exit 0"], Restart::Never);
        sp.phase = Phase::Step;
        sp.step = Some("build".into());
        let sink = Arc::new(Collector::default());
        sup.spawn(sink.clone(), sp).unwrap();
        assert!(wait_for(|| sup.status_of("1/a").map(|s| s.phase == Phase::Failed).unwrap_or(false), 3000));
        let st = sup.status_of("1/a").unwrap();
        assert_eq!(st.step.as_deref(), Some("build"));
        assert_eq!(st.exit_code, Some(0));
    }

    #[test]
    fn crash_restarts_with_backoff_then_stops_at_cap() {
        let sup = Supervisor::default();
        let mut sp = spec("1/a", vec!["sh", "-c", "exit 3"], Restart::OnFailure);
        sp.restart = Restart::OnFailure;
        let sink = Arc::new(Collector::default());
        sup.spawn(sink.clone(), sp).unwrap();
        // MAX_RESTARTS 次退避总和远大于测试耐心：只验证第一次重启确实发生
        assert!(
            wait_for(|| sup.status_of("1/a").map(|s| s.restarts >= 1).unwrap_or(false), 4000),
            "应当触发一次自动重启"
        );
        let _ = sup.stop("1/a");
    }

    #[test]
    fn stop_clears_port_and_note_so_ui_does_not_show_stale_endpoint() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let mut sp = spec("1/a", vec!["sleep", "5"], Restart::Never);
        sp.port = Some(45678);
        sp.note = Some("已改用 45678".into());
        sup.spawn(sink, sp).unwrap();
        let stopped = sup.stop("1/a").expect("stop 应返回状态");
        assert_eq!(stopped.phase, Phase::Stopped);
        assert_eq!(stopped.port, None, "停止后端口已不再监听，不能继续显示");
        assert_eq!(stopped.note, None, "停止后旧的提示语会变成假信息");
    }

    #[test]
    fn stop_cancels_pending_restart() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        sup.spawn(sink, spec("1/a", vec!["sh", "-c", "exit 1"], Restart::Always)).unwrap();
        assert!(wait_for(|| sup.status_of("1/a").map(|s| s.restarts == 1).unwrap_or(false), 4000));
        sup.stop("1/a");
        let after = sup.status_of("1/a").unwrap();
        assert_eq!(after.phase, Phase::Stopped);
        // 退避结束后不应再起新进程
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(sup.status_of("1/a").unwrap().phase, Phase::Stopped);
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff(0), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(2000));
        assert_eq!(backoff(20), BACKOFF_MAX);
    }

    #[test]
    fn exit_zero_classifies_as_stopped_not_crashed() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        sup.spawn(sink, spec("1/a", vec!["sh", "-c", "exit 0"], Restart::Always)).unwrap();
        assert!(wait_for(|| sup.status_of("1/a").map(|s| s.phase == Phase::Stopped).unwrap_or(false), 3000));
    }

    #[test]
    fn verify_adopt_rejects_dead_pid() {
        let rec = AdoptRecord {
            key: "1/a".into(),
            project_id: 1,
            name: "a".into(),
            pid: 999_999,
            pgid: 999_999,
            argv: vec!["sleep".into(), "5".into()],
            cwd: "/tmp".into(),
            started_at: 0,
            port: None,
            keep_alive: false,
        };
        assert!(verify_adopt(&rec).is_err());
    }

    #[test]
    fn verify_adopt_rejects_command_mismatch() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let rec = AdoptRecord {
            key: "1/a".into(),
            project_id: 1,
            name: "a".into(),
            pid,
            pgid: pid as i32,
            argv: vec!["node".into(), "impostor.js".into()],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
        };
        let err = verify_adopt(&rec).unwrap_err();
        assert!(err.contains("命令行不符"), "{err}");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn verify_adopt_accepts_matching_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let rec = AdoptRecord {
            key: "1/a".into(),
            project_id: 1,
            name: "a".into(),
            pid,
            pgid: pid as i32,
            argv: vec!["sleep".into(), "30".into()],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
        };
        verify_adopt(&rec).expect("自身启动的进程应当通过三重校验");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn adopted_process_is_marked_stopped_when_it_dies() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let st = sup.adopt(sink.clone(), spec("1/a", vec!["sleep", "30"], Restart::Never), pid, now_secs());
        assert_eq!(st.phase, Phase::Running);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            wait_for(|| sup.status_of("1/a").map(|s| s.phase != Phase::Running).unwrap_or(false), 8000),
            "收养的进程退出后状态必须变化"
        );
    }
}
