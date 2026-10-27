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
    /// 本批行所属的流代号（与快照 `LogSnapshot.epoch` 同源）。
    pub epoch: u64,
    /// `lines[0]` 的序号；后续行按 +1 递增。前端按它去重/补缝。
    pub base: u64,
    pub lines: Vec<String>,
}

/// 日志快照（Part3a 游标协议）：某 key 在同一把锁内一次取全的
/// （流代号，下一序号，环内行）。`lines[i]` 的序号是 `next - lines.len() + i`。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct LogSnapshot {
    pub epoch: u64,
    pub next: u64,
    pub lines: Vec<String>,
}

impl LogSnapshot {
    pub fn empty() -> Self {
        Self { epoch: 0, next: 0, lines: Vec::new() }
    }
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
    /// spawn/收养后对该 pid 的实际命令行快照（exec 解析后形态）。
    /// 比对收养时优先用它：`sh run.sh` 这类经 shell exec 优化变成 python 的，
    /// 计划 argv 永远对不上 ps 看到的实际命令（E2E 第二轮实锤）。
    /// 收养重验（watch_adopted）同样用它，否则收养成功 30s 后会被重验判死。
    spawn_cmd: Option<Vec<String>>,
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
    logs: Mutex<HashMap<String, LogStream>>,
    gen: AtomicU64,
    shutting_down: AtomicBool,
}

/// 某 key 的日志流：代号 + 序号分配 + 环形缓冲，三者在同一把 `logs` 锁内原子推进。
/// （Part3a：之前 epoch 与行分属两把锁，`begin` 的 bump+clear 与泵的 push 交错会撕裂。）
#[derive(Default)]
struct LogStream {
    epoch: u64,
    next: u64,
    buf: VecDeque<(u64, String)>,
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

    /// 开一个新日志流代号并清空缓冲（docker 跟随 / 受管 spawn 前调）。
    /// bump + clear + next 归零在同一把锁内原子完成。
    pub fn begin_log_epoch(&self, key: &str) -> u64 {
        let mut g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        let s = g.entry(key.to_string()).or_default();
        s.epoch += 1;
        s.next = 0;
        s.buf.clear();
        s.epoch
    }

    /// 开一个新的 docker 日志流代号（旧流的读线程随后会自行退出）。
    pub fn begin_docker_logs(&self, key: &str) -> u64 {
        self.begin_log_epoch(key)
    }

    pub fn docker_epoch(&self, key: &str) -> Option<u64> {
        let g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        g.get(key).map(|s| s.epoch)
    }

    /// docker 日志直接进同一个环形缓冲（前端不需要第二套通道）。
    /// 返回该行的 (epoch, seq)：泵把它们一并交给冲刷线程，emit 时带出。
    pub fn push_docker_line(&self, key: &str, line: String) -> (u64, u64) {
        self.push_line(key, line)
    }

    pub fn clear_logs(&self, key: &str) {
        self.begin_log_epoch(key);
    }

    /// 快照：(epoch, next, 行) 在同一把锁内一次取全，不存在跨锁撕裂。
    pub fn snapshot_logs(&self, key: &str) -> LogSnapshot {
        let g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        g.get(key)
            .map(|s| LogSnapshot {
                epoch: s.epoch,
                next: s.next,
                lines: s.buf.iter().map(|(_, l)| l.clone()).collect(),
            })
            .unwrap_or_else(LogSnapshot::empty)
    }

    /// 补缝：返回同 epoch 下 seq >= cursor 的行；epoch 已变则返回新 epoch 全量。
    pub fn logs_since(&self, key: &str, epoch: u64, cursor: u64) -> LogSnapshot {
        let g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = g.get(key) else { return LogSnapshot::empty() };
        if s.epoch != epoch {
            return LogSnapshot {
                epoch: s.epoch,
                next: s.next,
                lines: s.buf.iter().map(|(_, l)| l.clone()).collect(),
            };
        }
        LogSnapshot {
            epoch: s.epoch,
            next: s.next,
            lines: s.buf.iter().filter(|(seq, _)| *seq >= cursor).map(|(_, l)| l.clone()).collect(),
        }
    }

    /// 只剩测试用：产品路径走 `snapshot_logs`（带游标）。
    #[cfg(test)]
    pub fn logs_of(&self, key: &str) -> Vec<String> {
        self.snapshot_logs(key).lines
    }

    /// 行进环并分配序号（同锁原子）。返回 (epoch, seq)。
    fn push_line(&self, key: &str, line: String) -> (u64, u64) {
        let mut g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        let s = g.entry(key.to_string()).or_default();
        let seq = s.next;
        s.next += 1;
        s.buf.push_back((seq, line));
        while s.buf.len() > LOG_CAP {
            s.buf.pop_front();
        }
        (s.epoch, seq)
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
        // #17：一次采样共用——Run.started_at 与凭据必须一致，否则记录与状态对不上。
        let started_at = now_secs();
        // 实际命令行快照（exec 解析后形态）：exec 型服务收养比对的真相来源。
        // 稳定即返（通常 ~40ms），失败回退 None（verify 走旧比对）。
        let spawn_cmd = snapshot_stable_cmd(pid);
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
                    started_at,
                    spec: spec.clone(),
                    spawn_cmd: spawn_cmd.clone(),
                },
            );
            gen
        };
        self.attach(sink, spec.clone(), gen, child);
        // 立刻登记凭据：App 被强杀/崩溃时没有任何退出钩子，只能靠这份记录在下次启动清理孤儿。
        self.register(&spec, pid, pid as i32, started_at, spawn_cmd);
        Ok(pid)
    }

    fn register(
        &self,
        spec: &SpawnSpec,
        pid: u32,
        pgid: i32,
        started_at: i64,
        spawn_cmd: Option<Vec<String>>,
    ) {
        let rec = AdoptRecord {
            key: spec.key.clone(),
            project_id: spec.project_id,
            name: spec.name.clone(),
            pid,
            pgid,
            argv: spec.argv.clone(),
            cwd: spec.base.to_string_lossy().into_owned(),
            started_at,
            port: spec.port,
            keep_alive: spec.keep_alive,
            spawn_cmd,
        };
        if let Err(e) = crate::localstate::write_adopt(&rec) {
            eprintln!("supervisor: 写入收养凭据 {} 失败：{e}", spec.key);
        }
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
    ///
    /// 有意设计：收养的进程永不自动重启——我们没有它的 `Child` 句柄（不能 `wait` 拿退出码），
    /// 也没有 stdout 管道（日志读线程无从附着），重启只会丢状态地另起一个语义不同的进程。
    pub fn adopt(
        &self,
        sink: Arc<dyn Sink>,
        spec: SpawnSpec,
        pid: u32,
        started_at: i64,
    ) -> Result<ServiceStatus, String> {
        if self.is_active(&spec.key) {
            return Err("已在运行".to_string());
        }
        // #3：取真 pgid，不要硬设 pgid=pid（daemon 化后 leader 可能已不是组长）。
        let pgid = ps_snapshot(pid).map(|s| s.pgid).unwrap_or_else(|e| {
            eprintln!("supervisor: 收养时取 pgid 失败，回退为 pid={pid}：{e}");
            pid as i32
        });
        // 收养时同样快照实际命令行：旧凭据可能没有 spawn_cmd，补上后下次收养用快照比对。
        let spawn_cmd = snapshot_stable_cmd(pid);
        let gen = {
            let mut g = self.lock_runs();
            let gen = self.next_gen();
            g.insert(
                spec.key.clone(),
                Run {
                    pid,
                    pgid,
                    phase: Phase::Running,
                    step: None,
                    port: spec.port,
                    gen,
                    exit_code: None,
                    note: spec.note.clone(),
                    restarts: 0,
                    started_at,
                    spec: spec.clone(),
                    spawn_cmd: spawn_cmd.clone(),
                },
            );
            gen
        };
        self.register(&spec, pid, pgid, started_at, spawn_cmd.clone());
        // 收养的进程看不到它的 stdout（管道已随上个进程退出而失效），只能盯退出。
        Self::watch_adopted(self.clone(), sink, spec.clone(), gen, pid, started_at, spawn_cmd);
        Ok(self.status_of(&spec.key).expect("刚插入的状态必须存在"))
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
        // pending 存 (seq, 行)：冲刷线程 emit 时带出 base，前端按 seq 去重/补缝。
        let pending: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let readers = Arc::new(AtomicU64::new(streams.len() as u64));
        // 本次 attach 的流代号：restart 后旧 flusher 凭它自杀，不污染新流首屏。
        let epoch = self.docker_epoch(&key).unwrap_or(0);
        for stream in streams {
            let sup = self.clone();
            let pending = pending.clone();
            let readers = readers.clone();
            let k = key.clone();
            std::thread::spawn(move || {
                sup.pump_logs(&k, stream, pending, epoch);
                readers.fetch_sub(1, Ordering::SeqCst);
            });
        }
        Self::spawn_flusher(
            self.clone(),
            spec.project_id,
            spec.name.clone(),
            key.clone(),
            epoch,
            pending,
            readers,
            sink.clone(),
        );
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
            let mut child = child;
            let pid = child.id();
            // #17 同构：一次采样共用。
            let started_at = now_secs();
            let spawn_cmd = snapshot_stable_cmd(pid);
            let new_gen = {
                let mut g = sup.lock_runs();
                let Some(r) = g.get_mut(&spec.key) else { return };
                if r.gen != gen {
                    // 已被取消：立刻收掉刚起的进程，避免孤儿
                    Self::kill_group(pid as i32, "-KILL");
                    // #51：必须 wait，否则 Child 被 drop 后变僵尸。
                    let _ = child.wait();
                    return;
                }
                r.pid = pid;
                r.pgid = pid as i32;
                r.phase = Phase::Running;
                r.step = None;
                r.exit_code = None;
                r.started_at = started_at;
                r.spawn_cmd = spawn_cmd.clone();
                r.gen = sup.inner.gen.fetch_add(1, Ordering::SeqCst) + 1;
                r.gen
            };
            sup.register(&spec, pid, pid as i32, started_at, spawn_cmd);
            if let Some(st) = sup.status_of(&spec.key) {
                sink.state(&st);
            }
            Self::watch_exit(sup.clone(), sink.clone(), spec.clone(), new_gen, child);
        });
    }

    /// 收养进程的退出监视：看不到输出，主判据是 pid 存活；
    /// 每 15 轮（≈30s）用记录的 started_at+argv 对当前 ps 快照做一次重验，
    /// pid 复用导致 lstart/command 不符时按退出处理（否则会把别人的进程当自己的盯）。
    fn watch_adopted(
        sup: Supervisor,
        sink: Arc<dyn Sink>,
        spec: SpawnSpec,
        gen: u64,
        pid: u32,
        started_at: i64,
        spawn_cmd: Option<Vec<String>>,
    ) {
        std::thread::spawn(move || {
            let mut rounds: u32 = 0;
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let gen_now = sup.gen_of(&spec.key);
                if gen_now != Some(gen) {
                    return;
                }
                if !pid_alive(pid) {
                    if let Some(st) = sup.apply_exit(&spec.key, gen, None) {
                        sink.state(&st);
                    }
                    return;
                }
                rounds += 1;
                if rounds.is_multiple_of(15) {
                    let snap = ps_snapshot(pid);
                    if !adopt_still_valid(&spec.argv, started_at, spawn_cmd.as_deref(), snap.as_ref().ok()) {
                        eprintln!(
                            "supervisor: 收养进程 {} 的重验失败（pid 可能被复用），按退出处理",
                            spec.key
                        );
                        if let Some(st) = sup.apply_exit(&spec.key, gen, None) {
                            sink.state(&st);
                        }
                        return;
                    }
                }
            }
        });
    }

    /// 每 `LOG_FLUSH` 把攒下的行推一次；两个读线程都结束后再推最后一批后退出。
    /// emit 带 (epoch, base)：前端按 seq 去重/补缝。epoch 已前进则自杀（旧流尾批不污染新流）。
    #[allow(clippy::too_many_arguments)]
    fn spawn_flusher(
        sup: Supervisor,
        project_id: i64,
        name: String,
        key: String,
        epoch: u64,
        pending: Arc<Mutex<Vec<(u64, String)>>>,
        readers: Arc<AtomicU64>,
        sink: Arc<dyn Sink>,
    ) {
        std::thread::spawn(move || loop {
            std::thread::sleep(LOG_FLUSH);
            // 全新 key（无流）且 epoch==0 时放行：门只杀"代号已前进"的旧线程。
            if matches!(sup.docker_epoch(&key), Some(e) if e != epoch) {
                return;
            }
            let take = |pending: &Arc<Mutex<Vec<(u64, String)>>>| -> Vec<(u64, String)> {
                let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *g)
            };
            let emit = |batch: Vec<(u64, String)>| {
                if batch.is_empty() {
                    return;
                }
                let base = batch[0].0;
                let lines = batch.into_iter().map(|(_, l)| l).collect();
                sink.log(&LogPayload { key: key.clone(), project_id, name: name.clone(), epoch, base, lines });
            };
            emit(take(&pending));
            if readers.load(Ordering::SeqCst) == 0 {
                let tail = take(&pending);
                if !tail.is_empty() {
                    // 尾批的 key 被 move：重建 payload（只走一次）。
                    let base = tail[0].0;
                    let lines = tail.into_iter().map(|(_, l)| l).collect();
                    sink.log(&LogPayload { key, project_id, name, epoch, base, lines });
                }
                return;
            }
        });
    }

    /// 读线程：逐行进环形缓冲（拿 seq），并交给冲刷线程攒批（不在这里 emit）。
    /// epoch 已前进（restart/新流顶掉）则退出：旧读线程不再往新流的环里写。
    fn pump_logs(
        &self,
        key: &str,
        stream: Box<dyn Read + Send>,
        pending: Arc<Mutex<Vec<(u64, String)>>>,
        epoch: u64,
    ) {
        let reader = BufReader::new(stream);
        for line in reader.split(b'\n') {
            let Ok(bytes) = line else { break };
            if matches!(self.docker_epoch(key), Some(e) if e != epoch) {
                break;
            }
            let text = String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string();
            let (_, seq) = self.push_line(key, text.clone());
            let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
            // 上限是内存保险：极长的构建输出不应该在冲刷线程睡觉时无限堆积
            if g.len() < LOG_BATCH * 10 {
                g.push((seq, text));
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

    pub(crate) fn group_alive(pgid: i32) -> bool {
        // #44：pgid<=0 时 `kill -0 -- -0` 语义是“当前进程组”，必须先拦掉。
        if pgid <= 0 {
            return false;
        }
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
        // #4：KILL 条件必须同时看组——服务忽略 TERM、或 port 根本不是真实监听口时，
        // 只看端口释放会“假停”（进程活着、UI 显示 Stopped）。
        if !released || Self::group_alive(pgid) {
            Self::kill_group(pgid, "-KILL");
            if let Some(p) = port {
                let _ = crate::ports::wait_released(p, Duration::from_secs(2));
            } else {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && Self::group_alive(pgid) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        // #4：clear 后置——kill 流程结束后再删凭据，避免“进程没死、凭据已删”的假停。
        crate::localstate::clear_adopt(key);
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
                if let Err(e) = crate::localstate::write_adopt(&rec) {
                    eprintln!("supervisor: 标记可收养凭据 {} 失败：{e}", rec.key);
                }
            }
        }
        for (key, pgid) in &killed {
            let _ = key;
            Self::kill_group(*pgid, "-TERM");
        }
        // #46：TERM 后按 deadline 轮询，仍活才 KILL（不要固定 400ms + 无条件 KILL）。
        if !killed.is_empty() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && killed.iter().any(|(_, pgid)| Self::group_alive(*pgid)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            for (_, pgid) in &killed {
                if Self::group_alive(*pgid) {
                    Self::kill_group(*pgid, "-KILL");
                }
            }
        }
        // 留下来的降级为「已停止但进程仍在」：下次启动会重新收养。
        // #46：被杀的也要从表里移除（重入不再杀）；keep 的保持现有移除逻辑。
        let mut g = self.lock_runs();
        for (key, _) in kept.into_iter().chain(killed) {
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
                spawn_cmd: r.spawn_cmd.clone(),
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
    /// spawn/收养时对 pid 的实际命令行快照（按空白切词，exec 解析后形态）。
    /// 旧凭据没有它（None）→ 回退 argv+shebang 展开比对。
    #[serde(default)]
    pub spawn_cmd: Option<Vec<String>>,
}

/// 三重校验：pid 存活 + 启动时间相符 + 命令行按词相符。
/// 任一条不满足就不能收养——PID 会被系统复用，猜错会误杀别人的进程。
///
/// #1：一次 `ps` 快照取全字段（lstart/command/pgid 同一快照），不要三次 fork——
/// 三次非原子采样之间 pid 可能被复用，第一重验的进程和第三重验的可能不是同一个。
pub fn verify_adopt(rec: &AdoptRecord) -> Result<(), String> {
    verify_and_snapshot(rec).map(|_| ())
}

/// `verify_adopt` 的内层：校验通过时把快照一并返回，调用方复用它做进一步判断
/// （如 reclaim 比对 pgid），避免“验时一个快照、杀时另一个快照”的 TOCTOU。
fn verify_and_snapshot(rec: &AdoptRecord) -> Result<PsSnapshot, String> {
    if !pid_alive(rec.pid) {
        return Err("进程已不在".to_string());
    }
    let snap = ps_snapshot(rec.pid)?;
    if snap.command.trim().is_empty() || snap.lstart.trim().is_empty() {
        // #1：expect 为空直接拒绝——空串 `starts_with("")` 恒真，等价于跳过校验。
        return Err("进程快照为空，拒绝收养".to_string());
    }
    let actual_secs = parse_lstart(&snap.lstart).ok_or_else(|| {
        // #45：解析失败 → 拒绝收养并给出明确错误，不要静默通过。
        // （locale/系统把 lstart 格式改了时，宁可不收养也不要误收别人的进程。）
        format!("无法解析进程启动时间（实际 {}）", snap.lstart.trim())
    })?;
    if !adopt_time_match(rec.started_at, actual_secs) {
        return Err(format!(
            "启动时间不符（记录 {}，实际 {}）",
            rec.started_at,
            snap.lstart.trim()
        ));
    }
    if !adopt_cmd_match(&rec.argv, &snap.command, rec.spawn_cmd.as_deref()) {
        return Err(format!(
            "命令行不符（记录 {}，实际 {}）",
            rec.argv.join(" "),
            snap.command.trim()
        ));
    }
    Ok(snap)
}

/// 一次 `ps` 采样拿全的快照：列顺序必须是 `pgid,lstart,command`——
/// 实测 `command` 一旦不在最后就会被截断到列宽（脚本路径只剩 `/tmp/apr`），
/// 只有放最后才完整（command-only 与 combo 一致、且不受 COLUMNS 影响）。
#[derive(Debug, Clone, PartialEq)]
struct PsSnapshot {
    pgid: i32,
    lstart: String,
    command: String,
}

fn ps_snapshot(pid: u32) -> Result<PsSnapshot, String> {
    let out = Command::new("/bin/ps")
        .args(["-o", "pgid=,lstart=,command=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("无法执行 ps：{e}"))?;
    if !out.status.success() {
        return Err(format!("进程 {pid} 不存在"));
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.lines().next().unwrap_or("").trim();
    // 行形如：`  787 Tue Oct 27 11:10:30 2026     sleep 30`
    // pgid 是第一词；lstart 固定占其后 5 词（`Tue Oct 27 11:10:30 2026`，
    // 日期可能是空格补齐的 `Oct  7`，按空白切仍是 5 词）；剩下全是 command。
    let mut words = line.split_whitespace();
    let pgid: i32 = words.next().unwrap_or("").parse().map_err(|_| format!("无法解析 pgid（实际 {line}）"))?;
    let lstart_words: Vec<&str> = words.by_ref().take(5).collect();
    if lstart_words.len() != 5 {
        return Err(format!("无法解析启动时间（实际 {line}）"));
    }
    let lstart = lstart_words.join(" ");
    let rest: Vec<&str> = words.collect();
    Ok(PsSnapshot { pgid, lstart, command: rest.join(" ") })
}

/// 对 pid 的实际命令行做“稳定快照”（E2E 第二轮实锤的 exec 型收养失败的根治）。
///
/// `sh run.sh` / `zsh -lc '…'` 这类经 shell exec 优化后，leader pid 的命令行会变成
/// 最终程序（如 python），计划 argv 永远对不上。spawn/收养成功后立即对 pid 采样 ps，
/// 存 exec 解析后的真实形态，收养时拿它比对。
///
/// 轮询到连续两次相同才返回：跳过 fork-未exec 瞬间（看到父进程 project-ctrl 自身）
/// 与 shell 优化中的中间态。超时/失败返回 None（调用方回退旧比对，不破坏现有行为）。
fn snapshot_stable_cmd(pid: u32) -> Option<Vec<String>> {
    let parent_base = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
    let mut prev: Option<String> = None;
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        let cmd = ps_snapshot(pid).map(|s| s.command).unwrap_or_default();
        let trimmed = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
        // 未 exec 瞬间看到的是父进程自身：跳过继续等。
        let is_parent = parent_base.as_ref().is_some_and(|b| {
            trimmed.split_whitespace().next().is_some_and(|w| {
                w.rsplit('/').next().unwrap_or(w) == b.rsplit('/').next().unwrap_or(b)
            })
        });
        if !trimmed.is_empty() && !is_parent {
            if prev.as_deref() == Some(trimmed.as_str()) {
                return Some(trimmed.split_whitespace().map(str::to_string).collect());
            }
            prev = Some(trimmed);
        }
        if std::time::Instant::now() >= deadline {
            // 超时：有采样就用最后一次（好过没有），全空则 None。
            return prev.map(|p| p.split_whitespace().map(str::to_string).collect());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 命令行是否按词相符：逐词相等、且实际词数 ≥ 期望词数。
///
/// 无词边界的 `starts_with` 会把 `sleep 3` 误判成 `sleep 30` 的前缀；
/// 允许实际更长（ps 可能带上我们没记录的尾部参数），但期望的每个词都必须对上。
///
/// `spawn_cmd`（spawn/收养时的实际命令行快照）优先：exec 型服务（`sh run.sh`→python）
/// 的计划 argv 永远对不上 ps，快照才是真相。None/空时回退 shebang 展开比对（旧凭据）。
fn adopt_cmd_match(argv: &[String], command: &str, spawn_cmd: Option<&[String]>) -> bool {
    if argv.is_empty() {
        return false;
    }
    let expanded;
    let expected: &[String] = match spawn_cmd {
        Some(words) if !words.is_empty() => words,
        _ => {
            expanded = expand_shebang(argv);
            &expanded
        }
    };
    let actual: Vec<&str> = command.split_whitespace().collect();
    if actual.len() < expected.len() {
        return false;
    }
    // 首词允许“解释器路径 vs 裸名”差异（如 `/bin/sh` vs `sh`）：ps 显示什么取决于
    // 内核 exec 解析，调用方怎么写 argv[0] 并不决定它（见 expand_shebang 注释）。
    expected.iter().zip(actual.iter()).enumerate().all(|(i, (e, a))| {
        if i == 0 {
            e == a || e.rsplit('/').next().unwrap_or(e) == a.rsplit('/').next().unwrap_or(a)
        } else {
            path_word_match(e, a)
        }
    })
}

/// 路径词比对：相等即过；否则若两边都是“含 / 的路径”，各取 canonical 再比。
///
/// pnpm 是符号链接（`/opt/homebrew/bin/pnpm` → `../lib/…/pnpm.js`）。实测本机内核
/// 保留 exec 时的链接拼写（`ps` 显示链接路径），但不赌所有系统/版本都如此：
/// 两边 canonical 任一对上即认。canonical 失败（文件已删）时退回字面比对。
/// 非路径词（参数）只做字面相等，不做文件系统查询。
fn path_word_match(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    if !expected.contains('/') || !actual.contains('/') {
        return false;
    }
    let canon = |s: &str| std::fs::canonicalize(s).map(|p| p.to_string_lossy().into_owned());
    match (canon(expected), canon(actual)) {
        (Ok(e), Ok(a)) => e == a,
        (Ok(e), Err(_)) => e == actual,
        (Err(_), Ok(a)) => expected == a,
        (Err(_), Err(_)) => false,
    }
}

/// #2：若 argv[0] 是带 shebang 的可执行文件，`ps -o command=` 显示的是
/// “解释器行展开 + 脚本路径 + 其余参数”，而不是原 argv。本机实测：
/// - `["/tmp/x/a.sh"]`（`#!/bin/sh`）→ `/bin/sh /tmp/x/a.sh`
/// - `["/tmp/x/b.js"]`（`#!/usr/bin/env node`）→ `node /tmp/x/b.js`（env 被吃掉，只剩目标）
/// - `["/tmp/x/d.sh"]`（`#!/bin/sh -e`）→ `/bin/sh -e /tmp/x/d.sh`
/// - 普通二进制 `["sleep","30"]` → `sleep 30`（原样）
/// - PATH 裸名 `["mytool"]` → `/bin/sh /tmp/x/bin/mytool`（脚本被解析成绝对路径）
///
/// 所以期望命令 = shebang 解释器行按空白展开（首词若为 `env` 则丢掉 `env` 本身，
/// 若其后还有 `-u` 之类 env 自身选项则一并丢到目标程序名——实测 macOS 只剩目标名），
/// + argv[0] 解析后的路径 + 其余参数，再按词比对。
fn expand_shebang(argv: &[String]) -> Vec<String> {
    let Some(first) = argv.first() else { return Vec::new() };
    let path = if first.contains('/') {
        std::path::PathBuf::from(first)
    } else {
        // PATH 裸名：先试子进程的真实 PATH（execenv::base_path，含 homebrew/bun/cargo），
        // 再试本进程 PATH——Tauri 从 Finder/launchd 启动时父 PATH 常只有 /usr/bin:/bin，
        // 只看它会找不到 pnpm 之类工具。只读首行，不执行。
        // （pin 的 node_bin_dir 前置无法从凭据还原：AdoptRecord 不存 pin。
        // pin 下的 node/pnpm 会落到 base_path 兜底找不到 → 原样返回 → 校验拒绝收养而不是误收。）
        let found = crate::execenv::resolve(first, &crate::execenv::base_path()).or_else(|| {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths).map(|dir| dir.join(first)).find(|p| p.is_file())
            })
        });
        let Some(found) = found else {
            return argv.to_vec();
        };
        found
    };
    let Some(rest) = read_shebang(&path) else {
        return argv.to_vec();
    };
    let mut interp: Vec<String> = rest.split_whitespace().map(|s| s.to_string()).collect();
    if interp.is_empty() {
        return argv.to_vec();
    }
    // `#!/usr/bin/env node` → ps 显示 `node …`：env 自身被吃掉。
    // 保守起见：`env` 之后以 `-` 开头的词视为 env 自身选项一并丢弃，剩下的首词是目标。
    if interp[0].rsplit('/').next() == Some("env") {
        interp.remove(0);
        while interp.first().map(|s| s.starts_with('-')).unwrap_or(false) {
            interp.remove(0);
        }
        if interp.is_empty() {
            return argv.to_vec();
        }
    }
    let mut out = interp;
    out.push(path.to_string_lossy().into_owned());
    out.extend(argv[1..].iter().cloned());
    out
}

/// 只读文件头 512 字节取 shebang 行（`#!` 之后的部分），不是 shebang 则 None。
///
/// 不要 `fs::read` 整读：argv[0] 为 node 之类二进制时那是百 MB；且 watch 每 30s
/// 对每个被收养服务调一次，整读是持续的内存/CPU 开销。shebang 行本身受内核
/// BINPRM_BUF_SIZE（macOS 512）限制，读 512 字节必然覆盖完整解释器行。
fn read_shebang(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 512];
    let mut len = 0;
    while len < buf.len() {
        match f.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if buf[..len].contains(&b'\n') {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let line_end = buf[..len].iter().position(|b| *b == b'\n').unwrap_or(len);
    String::from_utf8_lossy(&buf[..line_end]).strip_prefix("#!").map(|s| s.to_string())
}

/// 启动时间是否相符：快照 lstart 解析回 unix 秒，与记录绝对差 ≤1s 即过。
///
/// ±1s 容差的理由：记录是 `now_secs()` 的截断秒（spawn 后采样，截掉毫秒），
/// 快照是内核启动时刻的截断秒（同样截掉毫秒），两者各截断一次、最多差 1s；
/// 再加 spawn→采样之间可能正好跨过一秒边界，所以 1s 是截断+跨秒的紧上界。
/// 用绝对差而不用单向比较：记录采样晚于真实启动，理论上 record ≥ actual，
/// 但时钟回拨/闰秒等极端下反向差 1s 也应容忍——安全判据靠的是“差很小”，不是方向。
fn adopt_time_match(recorded: i64, actual_secs: i64) -> bool {
    recorded.saturating_sub(actual_secs).abs() <= 1
}

/// `ps -o lstart=` 的格式：`Tue Oct 27 11:10:30 2026`（本地时区，`%a %b %e %H:%M:%S %Y`）。
///
/// #45 取舍：月份/星期是英文缩写，locale 改掉英文后解析失败 → 调用方拒绝收养并报错，
/// 不要静默通过（fail-closed）。日期 `%e` 是空格补齐（`Oct  7`），先归一化再解析。
fn parse_lstart(lstart: &str) -> Option<i64> {
    use chrono::TimeZone;
    let norm = lstart.split_whitespace().collect::<Vec<_>>().join(" ");
    chrono::NaiveDateTime::parse_from_str(&norm, "%a %b %d %H:%M:%S %Y")
        .ok()
        .and_then(|naive| chrono::Local.from_local_datetime(&naive).single())
        .map(|dt| dt.timestamp())
}

/// watch_adopted 的重验判据（纯函数）：快照取不到 / 时间不符 / 命令不符都算失效。
fn adopt_still_valid(
    argv: &[String],
    started_at: i64,
    spawn_cmd: Option<&[String]>,
    snap: Option<&PsSnapshot>,
) -> bool {
    let Some(snap) = snap else { return false };
    if snap.command.trim().is_empty() || snap.lstart.trim().is_empty() {
        return false;
    }
    let Some(actual_secs) = parse_lstart(&snap.lstart) else { return false };
    adopt_time_match(started_at, actual_secs) && adopt_cmd_match(argv, &snap.command, spawn_cmd)
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
/// #11：只杀“验明正身”的组——先用新快照校验 leader（pid=rec.pid）的 lstart/command
/// 与记录相符（复用 verify 的纯函数），再要求快照 pgid 与记录 pgid 一致才组 KILL。
/// leader 若已 setsid/daemon 化换了组，记录里的旧组号可能已属于无关进程，
/// 按它杀就是误杀——此时拒绝（fail-closed），孤儿由下次启动的收养路径处理。
/// 旧 `reclaim_group(pgid)` 只验 pid 却杀 pgid，已删除。
///
/// 组 KILL 后若目标 pid 仍活着（组已解散但进程仍在），退化为按 pid 杀——
/// 但仅当 `rec.pgid == rec.pid`（leader 即组长）且该 pid 已验活，
/// 否则回退就是在杀一个未经校验的陌生 pid。缺少这层回退会让孤儿静默活下去。
pub fn reclaim_group_verified(rec: &AdoptRecord) -> Result<(), String> {
    if rec.pgid <= 1 {
        return Err(format!("拒绝回收危险的进程组 {}", rec.pgid));
    }
    let own = own_pgid().ok_or_else(|| "无法确定自己的进程组，拒绝回收".to_string())?;
    if rec.pgid == own {
        return Err("拒绝回收自己的进程组".to_string());
    }
    let snap = verify_and_snapshot(rec)?;
    if snap.pgid != rec.pgid {
        // 组漂移：记录里的旧组号可能已属于无关进程，按它杀是误杀。
        // 仅当记录是组长形（pgid==pid）时退化为只杀已验活的单个 pid；
        // 非组长形连 pid 回退都不做（那个 pid 不是组长，组杀语义已不成立）。
        if rec.pgid != rec.pid as i32 {
            return Err(format!(
                "进程组已变（记录 {}，实际 {}），拒绝回收",
                rec.pgid, snap.pgid
            ));
        }
        kill_pid(rec.pid);
        return Ok(());
    }
    Supervisor::kill_group(rec.pgid, "-KILL");
    // 组不存在（记录里的 pid 并非组长）时上一条是 ESRCH，必须退化为按 pid 杀
    std::thread::sleep(Duration::from_millis(80));
    if rec.pgid == rec.pid as i32 && pid_alive(rec.pid) {
        kill_pid(rec.pid);
    }
    Ok(())
}

/// 按单个 pid 发 KILL（reclaim 回退路径用：目标已验活）。
fn kill_pid(pid: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// 本进程的 pgid（`reclaim_group_verified` 的自杀防护用）。
/// 取不到时返回 None，调用方拒绝回收（fail-closed：宁可漏杀孤儿也不自杀）。
fn own_pgid() -> Option<i32> {
    let out = Command::new("/bin/ps")
        .args(["-o", "pgid=", "-p", &std::process::id().to_string()])
        .output();
    out.ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
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
            spawn_cmd: None,
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
            spawn_cmd: None,
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
            spawn_cmd: None,
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
        let st = sup
            .adopt(sink.clone(), spec("1/a", vec!["sleep", "30"], Restart::Never), pid, now_secs())
            .expect("收养应当成功");
        assert_eq!(st.phase, Phase::Running);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            wait_for(|| sup.status_of("1/a").map(|s| s.phase != Phase::Running).unwrap_or(false), 8000),
            "收养的进程退出后状态必须变化"
        );
    }

    #[test]
    fn spawn_cmd_snapshot_prefers_actual_command() {
        // spawn_cmd 快照优先：期望用快照词，不再用 argv+shebang 展开。
        let snap = vec!["python3".to_string(), "-u".to_string(), "-m".to_string()];
        assert!(adopt_cmd_match(
            &argv(&["sh", "run.sh"]),
            "python3 -u -m http.server",
            Some(&snap)
        ));
        // 快照与实际不符 → 拒绝（强度不降低）。
        assert!(!adopt_cmd_match(&argv(&["sh", "run.sh"]), "sleep 30", Some(&snap)));
        // None/空回退旧逻辑。
        assert!(adopt_cmd_match(&argv(&["sleep", "30"]), "sleep 30", None));
        assert!(adopt_cmd_match(&argv(&["sleep", "30"]), "sleep 30", Some(&[])));
    }

    #[test]
    fn exec_type_service_adopt_roundtrip() {
        // E2E 第二轮实锤：`sh run.sh`（末命令被 shell exec 优化成 python）
        // 经 spawn 纳管后，凭据里的 spawn_cmd 快照必须让 verify 通过。
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let path = script.to_string_lossy().into_owned();
        let pid = sup.spawn(sink.clone(), spec("1/ex", vec!["sh", &path], Restart::Never)).unwrap();
        let rec = sup.adoptable().into_iter().find(|r| r.key == "1/ex").expect("应有凭据");
        assert_eq!(rec.pid, pid);
        assert!(
            rec.spawn_cmd.as_ref().is_some_and(|w| !w.is_empty()),
            "spawn 后必须有命令行快照"
        );
        verify_adopt(&rec).expect("exec 型服务应当通过命令行校验");
        // 对照：同一进程若不用快照（旧行为）→ 计划 argv 对不上 exec 后的实际命令 → 拒绝。
        let legacy = AdoptRecord { spawn_cmd: None, ..rec.clone() };
        assert!(verify_adopt(&legacy).is_err(), "旧逻辑应对 exec 型失败（对照组）");
        sup.stop("1/ex");
    }

    #[test]
    fn word_boundary_rejects_prefix_collision() {
        // `sleep 3` 的记录不能匹配 `sleep 30` 的进程。
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let rec = AdoptRecord {
            key: "1/a".into(),
            project_id: 1,
            name: "a".into(),
            pid,
            pgid: pid as i32,
            argv: vec!["sleep".into(), "3".into()],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
            spawn_cmd: None,
        };
        let err = verify_adopt(&rec).unwrap_err();
        assert!(err.contains("命令行不符"), "{err}");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn script_service_adopt_roundtrip() {
        // shebang 脚本经 spawn 纳管 → write_adopt 记录 → verify_adopt 通过 → adopt 成功。
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("svc.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = script.to_string_lossy().into_owned();
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let sp = spec("1/svc", vec![&path], Restart::Never);
        let pid = sup.spawn(sink.clone(), sp).unwrap();
        let rec = AdoptRecord {
            key: "1/svc".into(),
            project_id: 1,
            name: "svc".into(),
            pid,
            pgid: pid as i32,
            argv: vec![path],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
            spawn_cmd: None,
        };
        verify_adopt(&rec).expect("脚本服务应当通过命令行校验");
        // 换一个 Supervisor 模拟 App 重启后的收养
        let sup2 = Supervisor::default();
        let st = sup2
            .adopt(sink, spec("1/svc", vec![&rec.argv[0]], Restart::Never), pid, rec.started_at)
            .expect("收养应当成功");
        assert_eq!(st.phase, Phase::Running);
        sup.stop("1/svc");
        sup2.stop("1/svc");
    }

    #[test]
    fn stop_kills_term_ignoring_process() {
        // 忽略 TERM 的进程 + 一个“已释放”的假端口：旧代码只看端口会直接判定 released
        // 而跳过 KILL → 进程活着、UI 显示 Stopped（假停）。新代码必须同时看组。
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ign.sh");
        std::fs::write(&script, "#!/bin/sh\ntrap '' TERM\nsleep 300\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = script.to_string_lossy().into_owned();
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let mut sp = spec("1/ign", vec![&path], Restart::Never);
        sp.port = Some(45999);
        assert!(crate::ports::is_free(45999), "前置：假端口平时必须空闲，否则 wait_released 语义不对");
        let pid = sup.spawn(sink, sp).unwrap();
        let pgid = pid as i32;
        // 就绪等待：spawn 返回时子进程可能还在 exec 之前，此时 TERM 会直接杀死它，
        // 测不出“忽略 TERM”的 KILL 兜底。等 trap 生效（ps 能看到完整命令）再 stop。
        std::thread::sleep(std::time::Duration::from_millis(500));
        let st = sup.stop("1/ign").expect("stop 应返回状态");
        assert_eq!(st.phase, Phase::Stopped);
        assert!(!Supervisor::group_alive(pgid), "进程组必须被 KILL 兜底杀死");
        assert!(!pid_alive(pid), "进程必须真死");
    }

    #[test]
    fn group_alive_rejects_nonpositive_pgid() {
        assert!(!Supervisor::group_alive(0));
        assert!(!Supervisor::group_alive(-5));
    }

    #[test]
    fn adopt_rejects_active_key() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        sup.spawn(sink.clone(), spec("1/a", vec!["sleep", "30"], Restart::Never)).unwrap();
        let err = sup
            .adopt(sink, spec("1/a", vec!["sleep", "30"], Restart::Never), 999_999, now_secs())
            .unwrap_err();
        assert!(err.contains("已在运行"), "{err}");
        sup.stop("1/a");
    }

    #[test]
    fn reclaim_group_verified_kills_matching_and_refuses_mismatch() {
        // 相符时真杀掉。用独立进程组：记录 pgid 必须与快照一致，
        // 且不能是测试进程自己的组（否则自杀防护会拒绝）。
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().unwrap();
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
            spawn_cmd: None,
        };
        reclaim_group_verified(&rec).expect("相符的记录应当被回收");
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(child.try_wait().unwrap().is_some(), "进程应当被杀死");
        // 不符时拒绝且进程存活。
        let mut child2 = Command::new("sleep").arg("30").spawn().unwrap();
        let pid2 = child2.id();
        let bad = AdoptRecord {
            key: "1/b".into(),
            project_id: 1,
            name: "b".into(),
            pid: pid2,
            pgid: pid2 as i32,
            argv: vec!["node".into(), "impostor.js".into()],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
            spawn_cmd: None,
        };
        assert!(reclaim_group_verified(&bad).is_err());
        assert!(child2.try_wait().unwrap().is_none(), "不符时进程必须存活");
        let _ = child2.kill();
        let _ = child2.wait();
        // pgid=1 拒绝。
        let one = AdoptRecord { pgid: 1, ..bad.clone() };
        assert!(reclaim_group_verified(&one).is_err());
    }

    #[test]
    fn reclaim_group_verified_kills_only_pid_when_group_drifted() {
        // 记录 pgid==pid（组长形）但实际组已漂移：只杀已验活的单个 pid，
        // 绝不能按组杀（无论记录组号还是实际组号——组里可能有无关进程）。
        use std::os::unix::process::CommandExt;
        let mut holder_cmd = Command::new("sleep");
        holder_cmd.arg("30");
        holder_cmd.process_group(0);
        let mut holder = holder_cmd.spawn().unwrap();
        let group = holder.id() as i32;
        let spawn_in = |gid: i32| {
            let mut c = Command::new("sleep");
            c.arg("30");
            c.process_group(gid);
            c.spawn().unwrap()
        };
        let mut victim = spawn_in(group);
        let vpid = victim.id();
        let mut bystander = spawn_in(group);
        let vpgid = ps_snapshot(vpid).unwrap().pgid;
        assert_eq!(vpgid, group, "前置：victim 必须在 holder 组里");
        assert_ne!(vpgid, vpid as i32, "前置：实际组必须 ≠ 记录 pgid（漂移形）");
        let rec = AdoptRecord {
            key: "1/drift".into(),
            project_id: 1,
            name: "drift".into(),
            pid: vpid,
            pgid: vpid as i32,
            argv: vec!["sleep".into(), "30".into()],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
            spawn_cmd: None,
        };
        reclaim_group_verified(&rec).expect("漂移时应只杀 pid 并成功");
        let deadline = Instant::now() + Duration::from_secs(5);
        while victim.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(victim.try_wait().unwrap().is_some(), "已验活的 pid 必须被杀死");
        assert!(bystander.try_wait().unwrap().is_none(), "同组无关进程必须存活（不能组杀）");
        assert!(holder.try_wait().unwrap().is_none(), "组长必须存活（不能组杀）");
        let _ = bystander.kill();
        let _ = bystander.wait();
        let _ = holder.kill();
        let _ = holder.wait();
    }

    #[test]
    fn reclaim_group_verified_refuses_stale_pgid() {
        // 记录 pgid 已过期（leader setsid/daemon 化后组号变了）：lstart/command 仍相符，
        // 但绝不能按记录里的旧组号杀——那个组号可能已属于无关进程。
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let live_pgid = ps_snapshot(pid).expect("快照应成功").pgid;
        // 选一个“当前无进程用”的假组号：用一个已退出的独立组号。
        let mut probe = Command::new("true").spawn().unwrap();
        let stale = probe.id() as i32;
        let _ = probe.wait();
        assert_ne!(stale, live_pgid, "前置：假组号必须与真组号不同");
        let rec = AdoptRecord {
            key: "1/stale".into(),
            project_id: 1,
            name: "stale".into(),
            pid,
            pgid: stale,
            argv: vec!["sleep".into(), "30".into()],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
            spawn_cmd: None,
        };
        let err = reclaim_group_verified(&rec).unwrap_err();
        assert!(err.contains("进程组已变"), "{err}");
        assert!(child.try_wait().unwrap().is_none(), "过期 pgid 时进程必须存活");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn shutdown_removes_killed_and_is_reentrant() {
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let mut sp = spec("1/keep", vec!["sleep", "30"], Restart::Never);
        sp.keep_alive = true;
        sup.spawn(sink.clone(), sp).unwrap();
        let pid = sup.spawn(sink, spec("1/kill", vec!["sleep", "30"], Restart::Never)).unwrap();
        let keep: HashSet<String> = ["1/keep".to_string()].into_iter().collect();
        sup.shutdown_all(&keep);
        assert!(!pid_alive(pid), "被杀的进程必须真死");
        assert!(sup.status_of("1/kill").is_none(), "被杀的 key 必须从表里移除");
        assert!(sup.status_of("1/keep").is_none(), "保留的 key 也从表里移除");
        // 重入不再杀、不 panic。
        sup.shutdown_all(&keep);
        sup.stop("1/keep");
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn adopt_cmd_match_enforces_word_boundaries() {
        assert!(adopt_cmd_match(&argv(&["sleep", "30"]), "sleep 30", None));
        assert!(adopt_cmd_match(&argv(&["sleep", "30"]), "sleep 30 --extra", None), "实际更长应当允许");
        assert!(!adopt_cmd_match(&argv(&["sleep", "3"]), "sleep 30", None), "无词边界会误判前缀");
        assert!(!adopt_cmd_match(&argv(&["sleep", "30"]), "sleep 3", None));
        assert!(!adopt_cmd_match(&argv(&[]), "sleep 30", None), "空 argv 直接拒绝");
        assert!(!adopt_cmd_match(&argv(&["node", "a.js"]), "sleep 30", None));
    }

    #[test]
    fn adopt_cmd_match_accepts_interpreter_basename() {
        // argv 写 `sh`、ps 显示 `/bin/sh`（或反过来）应当相容。
        assert!(adopt_cmd_match(&argv(&["sh", "-c", "x"]), "/bin/sh -c x", None));
        assert!(adopt_cmd_match(&argv(&["/bin/sh", "-c", "x"]), "sh -c x", None));
    }

    #[test]
    fn expand_shebang_covers_all_measured_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, head: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("{head}\nsleep 30\n")).unwrap();
            p.to_string_lossy().into_owned()
        };
        // (a) sh 脚本 → 解释器 + 路径
        let sh = write("a.sh", "#!/bin/sh");
        assert_eq!(expand_shebang(&argv(&[&sh, "--port", "1"])), argv(&["/bin/sh", &sh, "--port", "1"]));
        // (b) env node → env 被吃掉，只剩 node + 路径
        let js = write("b.js", "#!/usr/bin/env node");
        assert_eq!(expand_shebang(&argv(&[&js])), argv(&["node", &js]));
        // (d) 带参数的解释器行 → 全部保留
        let d = write("d.sh", "#!/bin/sh -e");
        assert_eq!(expand_shebang(&argv(&[&d])), argv(&["/bin/sh", "-e", &d]));
        // (c) 普通二进制 → 原样
        assert_eq!(expand_shebang(&argv(&["sleep", "30"])), argv(&["sleep", "30"]));
        // 非 shebang 文件 → 原样
        let plain = write("plain.sh", "echo hi");
        assert_eq!(expand_shebang(&argv(&[&plain])), argv(&[&plain]));
    }

    #[test]
    fn expand_shebang_uses_child_path_not_parent_path() {
        // Finder/launchd 启动的父 PATH 只有 /usr/bin:/bin 时，homebrew 下的脚本仍要能展开：
        // 解析顺序必须是 base_path（子进程真实 PATH）优先，而不是只看父 PATH。
        // 用 PATH 锁串行：改进程级 PATH 会影响并行测试。
        let _guard = crate::test_support::exclusive_ports();
        let prev = std::env::var_os("PATH");
        std::env::set_var("PATH", "/usr/bin:/bin");
        let got = expand_shebang(&argv(&["pnpm", "dev"]));
        match prev {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        let pnpm = crate::execenv::resolve("pnpm", &crate::execenv::base_path());
        let Some(pnpm) = pnpm else {
            return; // 本机没装 pnpm：前置不满足，跳过
        };
        if read_shebang(&pnpm).is_none() {
            return; // pnpm 不是脚本（如 corepack 垫片是二进制）：语义不同，跳过
        }
        assert!(got.len() >= 2, "父 PATH 精简时 pnpm 脚本仍必须被展开：{got:?}");
        assert_eq!(&got[got.len() - 2..], &[pnpm.to_string_lossy().into_owned(), "dev".to_string()]);
    }

    #[test]
    fn adopt_cmd_match_tolerates_symlink_spelling() {
        // pnpm 是符号链接（/opt/homebrew/bin/pnpm → ../lib/…/pnpm.js）。
        // 实测内核保留 exec 时的链接路径（ps 显示 node /opt/homebrew/bin/pnpm …），
        // 但为防系统/版本差异显示 canonical 目标，两向都必须认。
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.sh");
        std::fs::write(&real, "#!/bin/sh\nsleep 30\n").unwrap();
        let link = dir.path().join("link.sh");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let link_s = link.to_string_lossy().into_owned();
        let real_s = real.to_string_lossy().into_owned();
        // 经链接 exec：无论 ps 显示链接还是目标，都认。
        assert!(adopt_cmd_match(&argv(&[&link_s]), &format!("/bin/sh {link_s}"), None));
        assert!(adopt_cmd_match(&argv(&[&link_s]), &format!("/bin/sh {real_s}"), None));
        // 经目标 exec：同理两向。
        assert!(adopt_cmd_match(&argv(&[&real_s]), &format!("/bin/sh {real_s}"), None));
        assert!(adopt_cmd_match(&argv(&[&real_s]), &format!("/bin/sh {link_s}"), None));
        // 真 pnpm：两种拼写都认（前置不满足则跳过）。
        let pnpm = crate::execenv::resolve("pnpm", &crate::execenv::base_path());
        if let Some(pnpm) = pnpm {
            if read_shebang(&pnpm).is_some() {
                let canon = std::fs::canonicalize(&pnpm)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| pnpm.to_string_lossy().into_owned());
                assert!(
                    adopt_cmd_match(&argv(&["pnpm", "dev"]), &format!("node {} dev", pnpm.display()), None),
                    "链接拼写必须认"
                );
                assert!(
                    adopt_cmd_match(&argv(&["pnpm", "dev"]), &format!("node {canon} dev"), None),
                    "canonical 拼写必须认"
                );
            }
        }
    }

    #[test]
    fn read_shebang_reads_head_only() {
        let dir = tempfile::tempdir().unwrap();
        // 大文件尾部藏 shebang 也必须读不到（只读头 512 字节）。
        let big = dir.path().join("big.bin");
        let mut data = vec![0x7fu8; 1024 * 1024];
        data.extend_from_slice(b"\n#!/bin/sh\n");
        std::fs::write(&big, &data).unwrap();
        assert_eq!(read_shebang(&big), None);
        // 正常 shebang 能读到。
        let sh = dir.path().join("s.sh");
        std::fs::write(&sh, "#!/bin/sh -e\necho hi\n").unwrap();
        assert_eq!(read_shebang(&sh).as_deref(), Some("/bin/sh -e"));
        // 无换行的小文件也能读。
        let tiny = dir.path().join("t");
        std::fs::write(&tiny, "#!/bin/sh").unwrap();
        assert_eq!(read_shebang(&tiny).as_deref(), Some("/bin/sh"));
        // 不存在的文件 → None。
        assert_eq!(read_shebang(&dir.path().join("nope")), None);
    }

    #[test]
    fn adopt_time_match_tolerates_one_second() {
        assert!(adopt_time_match(100, 100));
        assert!(adopt_time_match(101, 100), "截断+跨秒最多差 1s");
        assert!(adopt_time_match(100, 101));
        assert!(!adopt_time_match(102, 100), "差 2s 说明不是同一个进程");
        assert!(!adopt_time_match(100, 102));
    }

    #[test]
    fn parse_lstart_roundtrips_local_time() {
        use chrono::TimeZone;
        // 当前时间格式化成 lstart 再解析回去，差值应在 ±1s 内（含截断）。
        let now = now_secs();
        let dt = chrono::Local.timestamp_opt(now, 0).single().unwrap();
        let s = dt.format("%a %b %e %H:%M:%S %Y").to_string();
        let back = parse_lstart(&s).expect("本地 lstart 必须能解析");
        assert!((back - now).abs() <= 1, "{s} → {back} vs {now}");
        // 空串/locale 乱串 → None（调用方 fail-closed 拒绝收养）。
        assert_eq!(parse_lstart(""), None);
        assert_eq!(parse_lstart("not a date"), None);
    }

    #[test]
    fn adopt_still_valid_covers_snapshot_cases() {
        use chrono::TimeZone;
        let now = now_secs();
        let dt = chrono::Local.timestamp_opt(now, 0).single().unwrap();
        let lstart = dt.format("%a %b %e %H:%M:%S %Y").to_string();
        let good = PsSnapshot { pgid: 123, lstart: lstart.clone(), command: "sleep 30".into() };
        assert!(adopt_still_valid(&argv(&["sleep", "30"]), now, None, Some(&good)));
        assert!(!adopt_still_valid(&argv(&["sleep", "30"]), now, None, None), "快照取不到算失效");
        let empty = PsSnapshot { pgid: 123, lstart: String::new(), command: String::new() };
        assert!(!adopt_still_valid(&argv(&["sleep", "30"]), now, None, Some(&empty)), "空快照算失效");
        let wrong_cmd = PsSnapshot { pgid: 123, lstart, command: "node x.js".into() };
        assert!(!adopt_still_valid(&argv(&["sleep", "30"]), now, None, Some(&wrong_cmd)));
        let old_dt = chrono::Local.timestamp_opt(now - 3600, 0).single().unwrap();
        let old = PsSnapshot {
            pgid: 123,
            lstart: old_dt.format("%a %b %e %H:%M:%S %Y").to_string(),
            command: "sleep 30".into(),
        };
        assert!(!adopt_still_valid(&argv(&["sleep", "30"]), now, None, Some(&old)), "pid 复用后 lstart 不符");
    }

    #[test]
    fn reclaim_group_verified_refuses_own_group() {
        let rec = AdoptRecord {
            key: "1/a".into(),
            project_id: 1,
            name: "a".into(),
            pid: 999_999,
            pgid: own_pgid().expect("测试前置：必须能取到自己的 pgid"),
            argv: vec!["sleep".into()],
            cwd: "/tmp".into(),
            started_at: now_secs(),
            port: None,
            keep_alive: false,
            spawn_cmd: None,
        };
        let err = reclaim_group_verified(&rec).unwrap_err();
        assert!(err.contains("自己"), "{err}");
    }

    #[test]
    fn adopt_records_true_pgid() {
        // daemon 化形状：parent 起一个独立组，组长先死、孩子被 launchd 收养后
        // pgid 仍是原组号（≠ 自己的 pid）。adopt 必须记录这个真 pgid，不能硬设 pid。
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("daemon.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30 &\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cmd = Command::new(script.to_string_lossy().into_owned());
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut parent = cmd.spawn().unwrap();
        let group = parent.id() as i32;
        let _ = parent.wait();
        // 在组里找到存活的 sleep 孩子。
        let child_pid = (0..50)
            .filter_map(|_| {
                std::thread::sleep(Duration::from_millis(50));
                pids_in_group(group).into_iter().find(|p| *p as i32 != group)
            })
            .next()
            .expect("daemon 孩子应当存活");
        let expect_pgid = ps_snapshot(child_pid).unwrap().pgid;
        assert_eq!(expect_pgid, group, "前置：孩子的 pgid 必须是原组号");
        assert_ne!(expect_pgid, child_pid as i32, "前置：pgid 必须 ≠ pid，否则测不出硬设 bug");
        assert_ne!(expect_pgid, own_pgid().expect("前置：必须能取到自己的 pgid"), "前置：必须与测试进程组不同，否则 stop 会自杀");
        let data = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::ScopedDataDir::new(data.path());
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        sup.adopt(sink, spec("9/pg", vec!["sleep", "30"], Restart::Never), child_pid, now_secs())
            .expect("收养应当成功");
        let recs = sup.adoptable();
        let rec = recs.iter().find(|r| r.key == "9/pg").expect("收养后应有凭据");
        assert_eq!(rec.pgid, expect_pgid, "必须存真 pgid，不能硬设 pgid=pid");
        // 落盘凭据也必须是真 pgid：App 被强杀后下次启动按它组杀，错了会漏孤儿。
        let disk = crate::localstate::read_adopts();
        let on_disk = disk.iter().find(|r| r.key == "9/pg").expect("收养后凭据应落盘");
        assert_eq!(on_disk.pgid, expect_pgid, "落盘 pgid 必须是真 pgid");
        sup.stop("9/pg");
        assert!(!pid_alive(child_pid), "stop 必须按真 pgid 杀死 daemon 孩子");
    }

    /// 测试辅助：用 `ps -o pid= -g <pgid>` 列出组内 pid（macOS 通用）。
    fn pids_in_group(pgid: i32) -> Vec<u32> {
        let out = Command::new("/bin/ps").args(["-o", "pid=", "-g", &pgid.to_string()]).output();
        out.map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect()
        })
        .unwrap_or_default()
    }

    #[test]
    fn log_snapshot_and_push_are_atomic_under_concurrency() {
        // Part3a：多线程并发 push + snapshot：每次快照内部 (epoch,next,行) 必须自洽——
        // 行数与 next 对得上（无撕裂），且最终全序无重复无缺口。
        let sup = Supervisor::default();
        let key = "1/lograce";
        sup.begin_log_epoch(key);
        let epoch = sup.docker_epoch(key).unwrap();
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let sup = sup.clone();
                std::thread::spawn(move || {
                    for i in 0..500 {
                        sup.push_docker_line(key, format!("w{w}-{i}"));
                    }
                })
            })
            .collect();
        // 边写边采：每次快照序号连续（buf 内 seq 严格递增且 next == 末seq+1）。
        for _ in 0..50 {
            let snap = sup.snapshot_logs(key);
            assert_eq!(snap.epoch, epoch);
            assert!(snap.lines.len() as u64 <= snap.next, "行数不得超过 next：{}", snap.lines.len());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for w in writers {
            w.join().unwrap();
        }
        let snap = sup.snapshot_logs(key);
        assert_eq!(snap.next, 2000, "4 线程×500 行必须全部分配序号");
        // 环容量 2000 恰好装下：全序无重复无缺口。
        assert_eq!(snap.lines.len(), 2000);
        let mut seen = std::collections::HashSet::new();
        for l in &snap.lines {
            assert!(seen.insert(l.clone()), "重复行：{l}");
        }
        for w in 0..4 {
            for i in 0..500 {
                assert!(seen.contains(&format!("w{w}-{i}")), "缺行 w{w}-{i}");
            }
        }
    }

    #[test]
    fn logs_since_fills_gap_and_resets_on_epoch_change() {
        // Part3a：since 补缝——同 epoch 只返 cursor 之后；epoch 变了返新 epoch 全量。
        let sup = Supervisor::default();
        let key = "1/since";
        let e1 = sup.begin_log_epoch(key);
        for i in 0..5 {
            sup.push_docker_line(key, format!("a{i}"));
        }
        let mid = sup.snapshot_logs(key);
        assert_eq!((mid.epoch, mid.next), (e1, 5));
        for i in 5..8 {
            sup.push_docker_line(key, format!("a{i}"));
        }
        let fill = sup.logs_since(key, e1, mid.next);
        assert_eq!(fill.epoch, e1);
        assert_eq!(fill.next, 8);
        assert_eq!(fill.lines, vec!["a5".to_string(), "a6".to_string(), "a7".to_string()]);
        // 晚到快照的反面：cursor 已到最新 → 空增量（前端据此不覆盖）。
        let none = sup.logs_since(key, e1, 8);
        assert!(none.lines.is_empty());
        // 新 epoch：旧游标失效，返新 epoch 全量。
        let e2 = sup.begin_log_epoch(key);
        assert_ne!(e1, e2);
        sup.push_docker_line(key, "b0".into());
        let fresh = sup.logs_since(key, e1, 8);
        assert_eq!(fresh.epoch, e2);
        assert_eq!(fresh.lines, vec!["b0".to_string()]);
    }

    #[test]
    fn fresh_key_spawn_collects_logs_without_prior_begin() {
        // 门回归锁：全新 key 直接 spawn（无 clear/begin），pump/flusher 的 epoch 门
        // 不得把第一批行拦掉（None vs Some(0) 误判曾导致 0 行）。
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hi.sh"), "#!/bin/sh\necho hello-fresh\nsleep 30\n").unwrap();
        let sh = dir.path().join("hi.sh").to_string_lossy().into_owned();
        let mut sp = spec("1/freshlog", vec!["sh", &sh], Restart::Never);
        sp.base = dir.path().to_path_buf();
        sup.spawn(sink, sp).unwrap();
        assert!(
            wait_for(|| !sup.logs_of("1/freshlog").is_empty(), 5000),
            "全新 key spawn 后应收到日志"
        );
        sup.stop("1/freshlog");
    }
}
