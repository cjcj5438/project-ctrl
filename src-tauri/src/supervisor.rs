//! 受管服务进程的看护：进程组、环形日志、状态事件。
//!
//! 关键设计：
//! - 子进程用 `process_group(0)` 成为新进程组组长（pgid == pid）。`pnpm dev` 会再 fork 出
//!   node/vite，只 kill 父进程会留下占端口的孤儿，所以停止用 `kill -TERM -- -pgid`。
//! - 子进程环境由 `execenv::env_for` 从零构造，不继承本进程环境。
//! - 日志按行进每个项目的环形缓冲（`LOG_CAP` 行），攒批后 emit，不逐行 emit。
//! - 运行状态只在内存里；App 重启后一律从空开始，不做孤儿收养。
//! - App 退出（`RunEvent::ExitRequested`）时收掉全部进程组，否则端口会被残留进程占住。
//!
//! 事件出口抽象成 [`Sink`]：Tauri 侧用 [`Ctx`] 转发到窗口事件，测试侧用 [`Collector`] 收集，
//! 因此本模块的全部行为都可以单测而不需要跑起整个 App。

use crate::execenv;
use crate::services::ServiceConfig;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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

impl Phase {
    /// 该阶段是否仍持有活着的子进程组。
    ///
    /// 只有这三个阶段可以安全地对 pgid 发信号：`Stopped`/`Crashed`/`Failed` 的 pgid 早已退出，
    /// 若干小时后系统可能把它复用给无关进程，再发 `SIGKILL` 就会误杀别人。
    fn owns_process(self) -> bool {
        matches!(self, Phase::Running | Phase::Installing | Phase::Building)
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// 前端在「从未运行过」时展示的状态；后端不会主动构造，但它是 IPC 契约的一部分。
    #[allow(dead_code)]
    Idle,
    Installing,
    Building,
    Running,
    Stopped,
    Crashed,
    Failed,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ServiceStatus {
    pub project_id: i64,
    pub phase: Phase,
    pub port: Option<u16>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub note: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct LogPayload {
    pub project_id: i64,
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

    pub fn emit_state(&self, project_id: i64) {
        if let Some(st) = state(&self.app).status_of(project_id) {
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

struct Run {
    pid: u32,
    pgid: i32,
    phase: Phase,
    port: Option<u16>,
    gen: u64,
    exit_code: Option<i32>,
    note: Option<String>,
}

impl Run {
    fn status(&self, project_id: i64) -> ServiceStatus {
        ServiceStatus {
            project_id,
            phase: self.phase,
            port: self.port,
            pid: Some(self.pid),
            exit_code: self.exit_code,
            note: self.note.clone(),
        }
    }
}

#[derive(Default)]
struct Inner {
    runs: Mutex<HashMap<i64, Run>>,
    logs: Mutex<HashMap<i64, VecDeque<String>>>,
    gen: AtomicU64,
}

/// 进程表 + 日志缓冲 + 代号计数器。`Clone` 共享同一份内部状态，便于交给读线程。
#[derive(Clone, Default)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

/// 一次 spawn 的完整入参。
pub struct Launch<'a> {
    pub project_id: i64,
    pub base: &'a Path,
    pub argv: &'a [String],
    pub cfg: &'a ServiceConfig,
    pub extra_env: &'a [(String, String)],
    pub phase: Phase,
    pub port: Option<u16>,
    pub note: Option<String>,
}

impl Supervisor {
    fn next_gen(&self) -> u64 {
        self.inner.gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn statuses(&self) -> Vec<ServiceStatus> {
        let g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<ServiceStatus> = g.iter().map(|(id, r)| r.status(*id)).collect();
        v.sort_by_key(|s| s.project_id);
        v
    }

    pub fn status_of(&self, project_id: i64) -> Option<ServiceStatus> {
        let g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
        g.get(&project_id).map(|r| r.status(project_id))
    }

    pub fn is_active(&self, project_id: i64) -> bool {
        self.status_of(project_id).map(|s| s.phase.owns_process()).unwrap_or(false)
    }

    pub fn port_of(&self, project_id: i64) -> Option<u16> {
        self.status_of(project_id).and_then(|s| s.port)
    }

    /// 该运行的代号。调用方用它判断「我观察到的东西还属于我起的那次运行」，
    /// 避免 30 分钟的超时线程误伤同一项目后来启动的进程。
    pub fn gen_of(&self, project_id: i64) -> Option<u64> {
        let g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
        g.get(&project_id).map(|r| r.gen)
    }

    /// 实测校准出的端口写回状态。
    pub fn set_port(&self, project_id: i64, port: Option<u16>, note: Option<String>) {
        let mut g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = g.get_mut(&project_id) {
            r.port = port;
            if note.is_some() {
                r.note = note;
            }
        }
    }

    pub fn clear_logs(&self, project_id: i64) {
        let mut g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(project_id, VecDeque::new());
    }

    pub fn logs_of(&self, project_id: i64) -> Vec<String> {
        let g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        g.get(&project_id).map(|q| q.iter().cloned().collect()).unwrap_or_default()
    }

    fn push_line(&self, project_id: i64, line: String) {
        let mut g = self.inner.logs.lock().unwrap_or_else(|e| e.into_inner());
        let q = g.entry(project_id).or_default();
        q.push_back(line);
        while q.len() > LOG_CAP {
            q.pop_front();
        }
    }

    /// 把一次退出结果应用到状态。`gen` 不匹配说明是我们主动 stop 的，忽略。
    pub fn apply_exit(&self, project_id: i64, gen: u64, code: Option<i32>) -> Option<ServiceStatus> {
        let mut g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
        let r = g.get_mut(&project_id)?;
        if r.gen != gen {
            return None;
        }
        r.exit_code = code;
        r.phase = if matches!(r.phase, Phase::Installing | Phase::Building) {
            Phase::Failed
        } else if code == Some(0) {
            Phase::Stopped
        } else {
            Phase::Crashed
        };
        Some(r.status(project_id))
    }

    /// spawn 一个受管进程：新进程组 + 从零构造的环境 + 批量日志 + 退出监视。
    ///
    /// 返回子进程 pid（`process_group(0)` 使它同时是 pgid）。子进程的回收由监视线程负责；
    /// 本次运行的代号用 `gen_of` 读取。
    pub fn spawn(&self, sink: Arc<dyn Sink>, launch: Launch<'_>) -> Result<u32, String> {
        let Launch { project_id, base, argv, cfg, extra_env, phase, port, note } = launch;
        let (program, args) = argv.split_first().ok_or_else(|| "命令为空".to_string())?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(base)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in execenv::env_for(cfg, extra_env) {
            cmd.env(k, v);
        }
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| format!("无法启动 {program}：{e}"))?;
        let pid = child.id();
        let pgid = pid as i32;
        let gen = self.next_gen();
        {
            let mut g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            g.insert(project_id, Run { pid, pgid, phase, port, gen, exit_code: None, note });
        }

        let mut streams: Vec<Box<dyn Read + Send>> = Vec::new();
        if let Some(s) = child.stdout.take() {
            streams.push(Box::new(s));
        }
        if let Some(s) = child.stderr.take() {
            streams.push(Box::new(s));
        }
        // 两个读线程把行推给一个带定时器的冲刷线程：只按「有新行」触发会让
        // 「打印一行然后安静」的服务（如 `env`、一次性提示）的日志一直不出现。
        let pending: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let readers = Arc::new(AtomicU64::new(streams.len() as u64));
        for stream in streams {
            let sup = self.clone();
            let pending = pending.clone();
            let readers = readers.clone();
            std::thread::spawn(move || {
                sup.pump_logs(project_id, stream, pending);
                readers.fetch_sub(1, Ordering::SeqCst);
            });
        }
        Self::spawn_flusher(project_id, pending, readers, sink.clone());
        Self::watch_exit(self.clone(), sink, project_id, gen, child);
        Ok(pid)
    }

    /// 每 `LOG_FLUSH` 把攒下的行推一次；两个读线程都结束后再推最后一批后退出。
    fn spawn_flusher(
        project_id: i64,
        pending: Arc<Mutex<Vec<String>>>,
        readers: Arc<AtomicU64>,
        sink: Arc<dyn Sink>,
    ) {
        std::thread::spawn(move || loop {
            std::thread::sleep(LOG_FLUSH);
            let batch = {
                let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
                if g.is_empty() {
                    Vec::new()
                } else {
                    std::mem::take(&mut *g)
                }
            };
            if !batch.is_empty() {
                sink.log(&LogPayload { project_id, lines: batch });
            }
            if readers.load(Ordering::SeqCst) == 0 {
                let tail = {
                    let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut *g)
                };
                if !tail.is_empty() {
                    sink.log(&LogPayload { project_id, lines: tail });
                }
                return;
            }
        });
    }

    /// 读线程：逐行进环形缓冲，并交给冲刷线程攒批（不在这里 emit）。
    fn pump_logs(&self, project_id: i64, stream: Box<dyn Read + Send>, pending: Arc<Mutex<Vec<String>>>) {
        let reader = BufReader::new(stream);
        for line in reader.split(b'\n') {
            let Ok(bytes) = line else { break };
            let text = String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string();
            self.push_line(project_id, text.clone());
            let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
            // 上限是内存保险：极长的构建输出不应该在冲刷线程睡觉时无限堆积
            if g.len() < LOG_BATCH * 10 {
                g.push(text);
            }
        }
    }

    fn watch_exit(sup: Supervisor, sink: Arc<dyn Sink>, project_id: i64, gen: u64, mut child: Child) {
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            if let Some(st) = sup.apply_exit(project_id, gen, code) {
                sink.state(&st);
            }
        });
    }

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
    pub fn stop(&self, project_id: i64) -> Option<ServiceStatus> {
        let (pgid, port) = {
            let mut g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            let r = g.get_mut(&project_id)?;
            // 让已排队的监视线程结果失效：run 的 gen 前进，旧监视线程的 gen 就对不上了
            r.gen = self.next_gen();
            r.phase = Phase::Stopped;
            r.exit_code = None;
            (r.pgid, r.port)
        };
        Self::kill_group(pgid, "-TERM");
        let released = match port {
            Some(p) => crate::ports::wait_released(p, execenv::STOP_GRACE),
            // 没有端口可判，要求整个进程组消失
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
        self.status_of(project_id)
    }

    /// 退出时要收掉的 pgid：只取仍在活跃阶段的那些。
    fn pgids_to_stop(&self) -> Vec<i32> {
        let g = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
        g.values().filter(|r| r.phase.owns_process()).map(|r| r.pgid).collect()
    }

    /// App 退出时收掉全部进程组。不 emit（事件循环正在结束）。
    pub fn shutdown_all(&self) {
        let pgids = self.pgids_to_stop();
        if pgids.is_empty() {
            return;
        }
        for pgid in &pgids {
            Self::kill_group(*pgid, "-TERM");
        }
        std::thread::sleep(Duration::from_millis(400));
        for pgid in &pgids {
            Self::kill_group(*pgid, "-KILL");
        }
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

    fn with_run(phase: Phase, gen: u64) -> (Supervisor, i64) {
        let sup = Supervisor::default();
        let id = 1i64;
        {
            let mut g = sup.inner.runs.lock().unwrap();
            g.insert(
                id,
                Run { pid: 999_999, pgid: 999_999, phase, port: Some(8080), gen, exit_code: None, note: None },
            );
        }
        (sup, id)
    }

    #[test]
    fn log_ring_caps_at_2000() {
        let sup = Supervisor::default();
        for i in 1..=2500 {
            sup.push_line(7, format!("line {i}"));
        }
        let got = sup.logs_of(7);
        assert_eq!(got.len(), LOG_CAP);
        assert_eq!(got.first().map(String::as_str), Some("line 501"));
        assert_eq!(got.last().map(String::as_str), Some("line 2500"));
    }

    #[test]
    fn gen_bump_ignores_stale_exit() {
        let (sup, id) = with_run(Phase::Running, 5);
        {
            let mut g = sup.inner.runs.lock().unwrap();
            g.get_mut(&id).unwrap().gen = 6;
        }
        assert!(sup.apply_exit(id, 5, Some(1)).is_none());
        assert_eq!(sup.status_of(id).unwrap().phase, Phase::Running);
    }

    #[test]
    fn matching_gen_applies_crash() {
        let (sup, id) = with_run(Phase::Running, 5);
        let st = sup.apply_exit(id, 5, Some(2)).expect("应更新状态");
        assert_eq!(st.phase, Phase::Crashed);
        assert_eq!(st.exit_code, Some(2));
    }

    #[test]
    fn zero_exit_marks_stopped() {
        let (sup, id) = with_run(Phase::Running, 1);
        assert_eq!(sup.apply_exit(id, 1, Some(0)).unwrap().phase, Phase::Stopped);
    }

    #[test]
    fn build_step_failure_marks_failed() {
        let (sup, id) = with_run(Phase::Building, 1);
        assert_eq!(sup.apply_exit(id, 1, Some(1)).unwrap().phase, Phase::Failed);
    }

    #[test]
    fn stop_without_run_is_noop() {
        let sup = Supervisor::default();
        assert!(sup.stop(42).is_none());
    }

    #[test]
    fn statuses_include_finished_runs_and_active_flag() {
        let (sup, _) = with_run(Phase::Crashed, 1);
        assert_eq!(sup.statuses().len(), 1);
        assert!(!sup.is_active(1));
    }

    /// 退出时只能对仍活跃的进程组发信号。
    ///
    /// 已 Stopped / Crashed / Failed 的 pgid 早已退出；长时间运行后系统可能把该 pid 复用给
    /// 无关进程，再 `SIGKILL` 就会误杀别人。
    #[test]
    fn shutdown_only_targets_live_phases() {
        let sup = Supervisor::default();
        let entries = [
            (1i64, Phase::Running, 1001),
            (2, Phase::Installing, 1002),
            (3, Phase::Building, 1003),
            (4, Phase::Stopped, 1004),
            (5, Phase::Crashed, 1005),
            (6, Phase::Failed, 1006),
        ];
        {
            let mut g = sup.inner.runs.lock().unwrap();
            for (id, phase, pgid) in entries {
                g.insert(
                    id,
                    Run { pid: pgid as u32, pgid, phase, port: None, gen: 1, exit_code: None, note: None },
                );
            }
        }
        let mut got = sup.pgids_to_stop();
        got.sort_unstable();
        assert_eq!(got, vec![1001, 1002, 1003], "只应收活跃阶段");
    }

    /// `stop` 之后该记录不再被退出流程选中（否则 App 常开 + 多次启停就会累积陈旧 pgid）。
    #[test]
    fn stopped_run_is_not_shutdown_target() {
        let (sup, id) = with_run(Phase::Running, 1);
        assert_eq!(sup.pgids_to_stop(), vec![999_999]);
        sup.stop(id).expect("应有记录");
        assert!(sup.pgids_to_stop().is_empty(), "停止后不得再成为退出目标");
    }

    #[test]
    fn clear_logs_empties_buffer() {
        let sup = Supervisor::default();
        sup.push_line(3, "a".into());
        sup.clear_logs(3);
        assert!(sup.logs_of(3).is_empty());
    }

    /// 只打印一行就安静下来的进程，那行日志也必须被推给前端。
    ///
    /// 修复前的实现只在「又来了一行」时才冲刷，于是 `env` 这类一次性输出会一直停在
    /// 缓冲区里，UI 永远看到空日志。
    #[test]
    fn single_line_output_is_flushed_to_sink() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let argv = vec!["/bin/echo".to_string(), "hello-once".to_string()];
        let cfg = ServiceConfig::default();
        sup.spawn(
            sink.clone(),
            Launch {
                project_id: 21,
                base: dir.path(),
                argv: &argv,
                cfg: &cfg,
                extra_env: &[],
                phase: Phase::Installing,
                port: None,
                note: None,
            },
        )
        .expect("spawn 应成功");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got: Vec<String> = sink
                .logs
                .lock()
                .unwrap()
                .iter()
                .flat_map(|p| p.lines.clone())
                .collect();
            if got.iter().any(|l| l == "hello-once") {
                break;
            }
            assert!(Instant::now() < deadline, "5 秒内必须收到这一行；收到的是 {got:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(sup.logs_of(21), vec!["hello-once".to_string()]);
    }

    /// 端到端：真起一个进程组，验证日志、端口、停止后进程组消失。
    #[test]
    fn spawn_streams_logs_and_stop_kills_group() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("srv.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho starting\nsleep 120\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let argv = vec!["/bin/sh".to_string(), script.to_string_lossy().into_owned()];
        let cfg = ServiceConfig::default();
        let pid = sup
            .spawn(
                sink.clone(),
                Launch {
                    project_id: 11,
                    base: dir.path(),
                    argv: &argv,
                    cfg: &cfg,
                    extra_env: &[],
                    phase: Phase::Running,
                    port: None,
                    note: None,
                },
            )
            .expect("spawn 应成功");

        // 日志应在一秒内到达
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && sup.logs_of(11).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(sup.logs_of(11).first().map(String::as_str), Some("starting"));
        assert!(sup.is_active(11));

        // 停止后进程组必须消失
        let gen = sup.gen_of(11).expect("应有代号");
        sup.stop(11).expect("应有状态");
        assert_ne!(sup.gen_of(11), Some(gen), "stop 后代号必须前进");
        assert!(!Supervisor::group_alive(pid as i32), "进程组 {pid} 应已消失");
        assert_eq!(sup.status_of(11).unwrap().phase, Phase::Stopped);
    }

    /// 环境隔离：子进程只能看到白名单变量，看不到本进程的密钥。
    #[test]
    fn child_env_excludes_inherited_secrets() {
        std::env::set_var("PROJECT_CTRL_SECRET_KEY", "leak-me");
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let argv = vec!["/usr/bin/env".to_string()];
        let cfg = ServiceConfig::default();
        sup.spawn(
            sink,
            Launch {
                project_id: 12,
                base: dir.path(),
                argv: &argv,
                cfg: &cfg,
                extra_env: &[],
                phase: Phase::Installing,
                port: None,
                note: None,
            },
        )
        .expect("spawn 应成功");

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && sup.status_of(12).map(|s| s.phase) == Some(Phase::Installing) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let logs = sup.logs_of(12).join("\n");
        assert!(!logs.contains("PROJECT_CTRL_SECRET_KEY"), "不得泄漏继承的密钥: {logs}");
        assert!(logs.contains("PATH="), "应能看到白名单里的 PATH: {logs}");
        std::env::remove_var("PROJECT_CTRL_SECRET_KEY");
    }

    /// 停止不会误杀同机器上其它进程组（用一个独立的 sleep 对照）。
    #[test]
    fn stop_does_not_touch_other_groups() {
        let bystander = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let pid = bystander.id();
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::default();
        let sink = Arc::new(Collector::default());
        let argv = vec!["/bin/sleep".to_string(), "60".to_string()];
        let cfg = ServiceConfig::default();
        sup.spawn(
            sink,
            Launch {
                project_id: 13,
                base: dir.path(),
                argv: &argv,
                cfg: &cfg,
                extra_env: &[],
                phase: Phase::Running,
                port: None,
                note: None,
            },
        )
        .unwrap();
        sup.stop(13);

        let alive = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(alive, "旁观进程 {pid} 不应被杀");
        let _ = Command::new("/bin/kill").args(["-KILL", &pid.to_string()]).status();
    }
}
