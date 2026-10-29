//! AI 会话运行时：起一个本地 `pi` 编码代理（RPC 模式），驱动它的 JSONL 协议，
//! 把对话流式推给前端，并负责看护它（看门狗、崩溃恢复、休眠、退出）。
//!
//! 线程分工（全部同步实现，没有 tokio）：
//! | 线程 | 职责 |
//! |---|---|
//! | `ai-out-{sid}` | 读子进程 stdout → 分帧 → 分类 → 更新条目 / 回应待答命令 |
//! | `ai-err-{sid}` | 读 stderr，只留最后 [`STDERR_CAP`] 行（握手失败/崩溃时给用户看） |
//! | `ai-wait-{sid}` | `child.wait()`：正常退出是崩溃路径，主动杀则安静收尾 |
//! | `ai-flush` | 全局唯一，50ms 一次，把脏了的会话条目合成一批 emit |
//! | `ai-sweep` | 全局唯一，30s 一次：卡死的回合终止、长时间空闲的会话休眠 |
//!
//! 关键不变量：
//! - 每个会话有自己的 `gate_lock`，spawn / 发 prompt / abort / sleep 互斥；
//!   持锁时**绝不**再调用会加锁的包装方法（`spawn` 会死锁，用 `spawn_inner`）。
//! - `live` 计数只在「槽位被清掉」时递减（wait 线程或超时兜底，二者互斥于 `proc` 槽），
//!   不会重复扣减。
//! - 崩溃恢复**复用同一个 session id**：pi 的会话文件在 `--session-dir` 下，重连即续上下文。

// 命令表由后续步骤注册（`ai_open`/`ai_send`/…），本步还没有调用方。
#![allow(dead_code)]

use crate::ai_gate;
use crate::ai_proto::{self, Incoming, LineSplitter};
use crate::ai_settings::{self, ModelRef, PiBin};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::Emitter;

pub const EV_ITEMS: &str = "ai:items";
pub const EV_SESSION: &str = "ai:session";
pub const EV_BATCH: &str = "ai:batch";

/// 同时运行的会话进程上限。
pub const MAX_LIVE: usize = 6;
/// 每个会话保留的条目数（超出丢最旧的）。
pub const ITEM_CAP: usize = 400;
/// 条目正文上限：助手/工具正文超过就截断（避免一次工具输出把内存吃光）。
const TEXT_CAP: usize = 64 * 1024;
const DETAIL_CAP: usize = 4000;
/// 条目批量推送的间隔。
const FLUSH: Duration = Duration::from_millis(50);
/// 看门狗巡检间隔。
const SWEEP: Duration = Duration::from_secs(30);
/// 命令应答的默认等待上限；超时视为卡死。
const REQ_TIMEOUT: Duration = Duration::from_secs(15);
/// 握手（`get_state`）的等待上限。
const DEFAULT_HANDSHAKE: Duration = Duration::from_secs(20);
/// 一轮对话「完全没有输出」多久算卡死。
const BUSY_SILENT: Duration = Duration::from_secs(300);
/// 空闲多久后休眠（回收进程）。
const IDLE_SLEEP: u64 = 900;
/// abort 后等 `agent_settled` 的时长。
const ABORT_WAIT: Duration = Duration::from_secs(10);
/// 崩溃统计窗口与阈值：窗口内超过 [`CRASH_MAX`] 次就不再自动恢复。
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const CRASH_MAX: usize = 2;
/// 每个会话保留的 stderr 行数。
const STDERR_CAP: usize = 200;

/* ------------------------------------------------------------------------- *
 * 推给前端的结构
 * ------------------------------------------------------------------------- */

/// 会话状态机：`idle | starting | busy | sleeping | recovering | failed`。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct AiSessionView {
    pub session_id: String,
    pub project_id: i64,
    pub project_name: String,
    pub state: String,
    pub task: Option<String>,
    pub model: Option<String>,
    pub awaiting_user: bool,
    /// 最后一次活动（unix 秒）。
    pub last_active: u64,
}

/// 一条对话条目。`rev` 单调递增，前端按它去重/补缝。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub rev: u64,
    pub body: ItemBody,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemBody {
    User {
        text: String,
        task: Option<String>,
    },
    Assistant {
        text: String,
        thinking: String,
        streaming: bool,
    },
    /// `status`：`running | ok | error | blocked`；`detail` 截断到 4000 字。
    Tool {
        name: String,
        summary: String,
        status: String,
        detail: String,
    },
    /// 结构化卡片。本步只会产出 onboarding 卡，形状由后续步骤补齐。
    Card {
        card: Value,
    },
    Notice {
        level: String,
        text: String,
        retry: bool,
    },
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ItemsPayload {
    pub session_id: String,
    pub project_id: i64,
    pub from_rev: u64,
    pub to_rev: u64,
    pub items: Vec<Item>,
}

/// 项目列表上的会话徽标（由后续步骤填充 `profile`）。
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct BatchItem {
    pub project_id: i64,
    pub name: String,
    pub state: String,
    pub profile: Option<Value>,
    pub error: Option<String>,
}

/// `ai_open` / `ai_snapshot` 的返回：视图 + 当前最大 rev + 全量条目。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct AiSnapshot {
    pub session: AiSessionView,
    pub rev: u64,
    pub items: Vec<Item>,
}

/* ------------------------------------------------------------------------- *
 * 出口
 * ------------------------------------------------------------------------- */

/// 状态 / 条目 / 批量的出口。测试用记录型实现，生产用 [`Ctx`]。
pub trait AiSink: Send + Sync {
    fn items(&self, p: &ItemsPayload);
    fn session(&self, v: &AiSessionView);
    fn batch(&self, b: &[BatchItem]);
}

/// Tauri 侧实现：三条 payload 直接 emit 给前端。
pub struct Ctx<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> Ctx<R> {
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        Self { app }
    }
}

impl<R: tauri::Runtime> AiSink for Ctx<R> {
    fn items(&self, p: &ItemsPayload) {
        let _ = self.app.emit(EV_ITEMS, p.clone());
    }

    fn session(&self, v: &AiSessionView) {
        let _ = self.app.emit(EV_SESSION, v.clone());
    }

    fn batch(&self, b: &[BatchItem]) {
        let _ = self.app.emit(EV_BATCH, b.to_vec());
    }
}

/// 宿主能力钩子：`pc-host` 通道的实现（读项目上下文、日志、服务操作…）。
///
/// 形参是（会话句柄、方法名、参数）。返回的 `Value` 会被包成 `{"ok":true,"result":…}`。
pub type HostHandler =
    Arc<dyn Fn(&SessionHandle, &str, &Value) -> Result<Value, String> + Send + Sync>;

/// 交给宿主钩子的会话句柄：公开字段是只读快照，内部另存弱引用以便回写转录。
///
/// 用 `Weak` 而不是 `Arc`：任务层可能把句柄存进线程里长时间持有，
/// 强引用会让已 `close` 的会话永远回收不掉。会话被关掉后各方法统一报「会话已关闭」。
#[derive(Clone, Debug)]
pub struct SessionHandle {
    pub session_id: String,
    pub project_id: i64,
    pub project_name: String,
    pub root: PathBuf,
    /// 回到管理器的弱引用（后续步骤从卡片回调里回查会话状态用）。
    inner: Weak<Inner>,
    session: Weak<Session>,
}

impl SessionHandle {
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn live(&self) -> Result<Arc<Session>, String> {
        self.session.upgrade().ok_or_else(|| "会话已关闭".to_string())
    }

    /// 往本会话转录里追加一张卡片，返回卡片 id（调用方给定的 `id` 字段优先）。
    pub fn push_card(&self, card: Value) -> Result<String, String> {
        let s = self.live()?;
        let id = card
            .get("id")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("card-{}", s.next_item_no()));
        s.push_with_id(id.clone(), ItemBody::Card { card });
        Ok(id)
    }

    /// 就地改一张卡片（按 id 找 `ItemBody::Card`）。找不到返回 false。
    pub fn update_card(&self, card_id: &str, f: impl FnOnce(&mut Value)) -> bool {
        let Ok(s) = self.live() else { return false };
        s.update_item(card_id, |b| {
            if let ItemBody::Card { card } = b {
                f(card);
            }
        })
    }

    /// 挂起等待用户对某张卡片的决定。超时 / 会话被关都返回中文错误。
    ///
    /// 不持 `gate_lock`：宿主钩子线程本就在 `gate_lock` 之外跑（见 `handle_host`），
    /// 阻塞在这里不应挡住 spawn / abort / sleep。
    pub fn await_decision(&self, card_id: &str, timeout: Duration) -> Result<Value, String> {
        let s = self.live()?;
        let (tx, rx) = mpsc::sync_channel::<Value>(1);
        // 单槽即可：`decide` 投递后立刻返回，不必等接收方排队。
        s.decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(card_id.to_string(), tx);
        let got = rx.recv_timeout(timeout);
        // 三条路径都注销，避免残留发送端把后续投递误配给旧等待者。
        s.decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(card_id);
        match got {
            Ok(v) => Ok(v),
            Err(RecvTimeoutError::Timeout) => Err("用户未在时限内确认".to_string()),
            Err(RecvTimeoutError::Disconnected) => Err("已取消".to_string()),
        }
    }
}

/* ------------------------------------------------------------------------- *
 * 进程槽
 * ------------------------------------------------------------------------- */

/// 一次 spawn 的进程信息。`deliberate` 由本代读线程/wait 线程共享：
/// 主动杀（abort/sleep/close/退出）时置真，wait 线程据此不走崩溃路径。
struct Proc {
    pid: u32,
    pgid: i32,
    gen: u64,
    deliberate: Arc<AtomicBool>,
    /// 本代 stdout 读线程是否已把管道读干净。`child.wait()` 与读线程赛跑：
    /// 崩溃前刚吐出的事件可能还在管道里，结算前必须先等这一位。
    drained: Arc<AtomicBool>,
}

/// 会话内部状态（都是字符串，直接映进 [`AiSessionView`]）。
struct Status {
    state: String,
    task: Option<String>,
    /// 展示用 `provider/id`。
    model: Option<String>,
    /// 期望模型；进程重启时作为 `--model` 参数。
    desired_model: Option<ModelRef>,
    thinking: Option<String>,
    awaiting_user: bool,
    last_active: u64,
    last_stdout: Instant,
}

impl Status {
    fn new() -> Self {
        Self {
            state: "idle".to_string(),
            task: None,
            model: None,
            desired_model: None,
            thinking: None,
            awaiting_user: false,
            last_active: now_secs(),
            last_stdout: Instant::now(),
        }
    }
}

/// 条目环：`rev` 单调递增；`emitted` 是已推送到的最大 rev。
struct Log {
    items: VecDeque<Item>,
    next_rev: u64,
    emitted: u64,
    dirty: bool,
    /// 正在流式输出的助手条目 id。
    open: Option<String>,
}

impl Log {
    fn new() -> Self {
        Self { items: VecDeque::new(), next_rev: 1, emitted: 0, dirty: false, open: None }
    }

    fn push(&mut self, mut item: Item) {
        item.rev = self.next_rev;
        self.next_rev += 1;
        self.items.push_back(item);
        while self.items.len() > ITEM_CAP {
            self.items.pop_front();
        }
        self.dirty = true;
    }

    /// 就地改一条条目并刷新它的 `rev`（前端按 id + rev 覆盖）。
    /// 找不到返回 false（例如条目已被环挤掉）。
    fn update(&mut self, id: &str, f: impl FnOnce(&mut ItemBody)) -> bool {
        for it in self.items.iter_mut().rev() {
            if it.id == id {
                f(&mut it.body);
                it.rev = self.next_rev;
                self.next_rev += 1;
                self.dirty = true;
                return true;
            }
        }
        false
    }

    /// 取一批待推送条目。没有新增返回 None。
    fn take_batch(&mut self) -> Option<(u64, u64, Vec<Item>)> {
        if !self.dirty {
            return None;
        }
        let from = self.emitted;
        let to = self.next_rev - 1;
        if to <= from {
            self.dirty = false;
            return None;
        }
        let mut items: Vec<Item> = self.items.iter().filter(|i| i.rev > from).cloned().collect();
        items.sort_by_key(|i| i.rev);
        self.emitted = to;
        self.dirty = false;
        Some((from, to, items))
    }
}

/* ------------------------------------------------------------------------- *
 * 会话
 * ------------------------------------------------------------------------- */

struct Session {
    id: String,
    project_id: i64,
    project_name: String,
    root: PathBuf,
    st: Mutex<Status>,
    log: Mutex<Log>,
    /// 待应答命令：id → 单槽发送端。
    pending: Mutex<HashMap<String, SyncSender<Incoming>>>,
    /// 卡片上的用户决定：卡片 id → 单槽发送端（等待者 `await_decision`）。
    decisions: Mutex<HashMap<String, SyncSender<Value>>>,
    stdin: Mutex<Option<ChildStdin>>,
    proc: Mutex<Option<Proc>>,
    stderr: Mutex<VecDeque<String>>,
    crashes: Mutex<VecDeque<Instant>>,
    /// 正在进行的条目 id（崩溃时标成中断）。
    inflight: Mutex<Vec<String>>,
    last_prompt: Mutex<Option<String>>,
    /// 本轮待上报的模型失败原文：回合结束时统一报一条。
    /// 重试期间不报，否则用户最后看到的是「正在重试（3/3）」，像还有下一轮。
    pending_error: Mutex<Option<String>>,
    /// 挂着未回的宿主调用数（看门狗据此不误判卡死）。
    host_pending: AtomicUsize,
    cmd_seq: AtomicU64,
    item_seq: AtomicU64,
    /// spawn / 发 prompt / abort / sleep 的互斥锁。
    gate_lock: Mutex<()>,
}

impl Session {
    fn new(id: String, project_id: i64, project_name: String, root: PathBuf) -> Self {
        Self {
            id,
            project_id,
            project_name,
            root,
            st: Mutex::new(Status::new()),
            log: Mutex::new(Log::new()),
            pending: Mutex::new(HashMap::new()),
            decisions: Mutex::new(HashMap::new()),
            stdin: Mutex::new(None),
            proc: Mutex::new(None),
            stderr: Mutex::new(VecDeque::new()),
            crashes: Mutex::new(VecDeque::new()),
            inflight: Mutex::new(Vec::new()),
            last_prompt: Mutex::new(None),
            pending_error: Mutex::new(None),
            host_pending: AtomicUsize::new(0),
            cmd_seq: AtomicU64::new(0),
            item_seq: AtomicU64::new(0),
            gate_lock: Mutex::new(()),
        }
    }

    fn view(&self) -> AiSessionView {
        let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        AiSessionView {
            session_id: self.id.clone(),
            project_id: self.project_id,
            project_name: self.project_name.clone(),
            state: st.state.clone(),
            task: st.task.clone(),
            model: st.model.clone(),
            awaiting_user: st.awaiting_user,
            last_active: st.last_active,
        }
    }

    fn handle(self: &Arc<Self>, inner: &Arc<Inner>) -> SessionHandle {
        SessionHandle {
            session_id: self.id.clone(),
            project_id: self.project_id,
            project_name: self.project_name.clone(),
            root: self.root.clone(),
            inner: Arc::downgrade(inner),
            session: Arc::downgrade(self),
        }
    }

    /// 丢掉所有等决定的发送端：等待者收 `Disconnected`，不会挂到超时。
    fn drop_decisions(&self) {
        self.decisions.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn snapshot(&self) -> AiSnapshot {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        AiSnapshot {
            session: self.view(),
            rev: log.next_rev - 1,
            items: log.items.iter().cloned().collect(),
        }
    }

    fn next_item_no(&self) -> u64 {
        self.item_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn push_item(&self, body: ItemBody) {
        let id = match &body {
            ItemBody::Assistant { .. } => format!("a-{}", self.next_item_no()),
            ItemBody::User { .. } => format!("u-{}", self.next_item_no()),
            _ => format!("n-{}", self.next_item_no()),
        };
        self.push_with_id(id, body);
    }

    fn push_with_id(&self, id: String, body: ItemBody) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.push(Item { id, rev: 0, body });
    }

    fn update_item(&self, id: &str, f: impl FnOnce(&mut ItemBody)) -> bool {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.update(id, f)
    }

    fn push_notice(&self, level: &str, text: String, retry: bool) {
        self.push_item(ItemBody::Notice { level: level.to_string(), text, retry });
    }

    /// 回合结束：把本轮待报的模型失败落成一条 error 提示（每轮最多一条）。
    fn flush_pending_error(&self) {
        let Some(text) = self.pending_error.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        let body = if text.is_empty() {
            "模型调用失败".to_string()
        } else if text.starts_with("重试失败：") {
            text
        } else {
            format!("模型调用失败：{text}")
        };
        self.push_notice("error", body, true);
    }

    /// `message_end(stopReason=error)`：认证/选型问题立刻弹 onboarding 卡（和
    /// `send_prompt` 的拒绝路径复用同一套修配 UI）；其余失败只记进 `pending_error`，
    /// 等回合结束再报——pi 的 `message_end{error}` 先于 `auto_retry_start` 到达，
    /// 当场报会让「正在重试（3/3）」变成用户看到的最后一行。
    fn report_provider_error(&self, err: Option<&str>) {
        let err = err.map(str::trim).filter(|s| !s.is_empty());
        let onboarding = err.is_some_and(|e| {
            ["No model selected", "No API key", "Authentication failed"]
                .iter()
                .any(|p| e.starts_with(p))
        });
        if let (true, Some(problem)) = (onboarding, err) {
            let id = format!("onb-{}", self.next_item_no());
            let card = json!({ "kind": "onboarding", "id": id, "problem": problem });
            self.push_with_id(id, ItemBody::Card { card });
            return;
        }
        *self.pending_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(err.unwrap_or_default().to_string());
    }

    fn set_state(&self, inner: &Inner, state: &str) {
        {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.state = state.to_string();
            st.last_active = now_secs();
        }
        emit_session(inner, self);
    }

    fn stderr_tail(&self, n: usize) -> String {
        let g = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let start = g.len().saturating_sub(n);
        g.iter().skip(start).cloned().collect::<Vec<_>>().join("\n")
    }

    fn touch_stdout(&self) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.last_stdout = Instant::now();
        st.last_active = now_secs();
    }

    /* ----- 进程 I/O ----- */

    fn write_frame(&self, line: &str) -> Result<(), String> {
        let mut g = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            Some(w) => w
                .write_all(line.as_bytes())
                .and_then(|_| w.flush())
                .map_err(|e| format!("写入 AI 进程失败：{e}")),
            None => Err("AI 进程未运行".to_string()),
        }
    }

    /// 发一条命令并等应答。`Err` 只表示传输层失败（写不进去 / 超时）。
    fn request(&self, command: &str, extra: Value, timeout: Duration) -> Result<Reply, String> {
        let id = format!("c{}", self.cmd_seq.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = mpsc::sync_channel::<Incoming>(1);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), tx);
        let frame = ai_proto::cmd(&id, command, extra);
        if let Err(e) = self.write_frame(&frame) {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(e);
        }
        let got = rx.recv_timeout(timeout);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        match got {
            Ok(Incoming::Response { success, error, .. }) => Ok(Reply { success, error }),
            Ok(_) => Err("AI 应答类型不符".to_string()),
            Err(RecvTimeoutError::Timeout) => Err(format!("AI 未响应（{command} 超时）")),
            Err(RecvTimeoutError::Disconnected) => Err("AI 进程已退出".to_string()),
        }
    }

    /// 清理进程槽（唯一会递减 `live` 的地方）。gen 不符说明槽里已是新一代 → 不动。
    fn clear_proc(&self, inner: &Inner, gen: u64) -> bool {
        let mut g = self.proc.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(g.as_ref(), Some(p) if p.gen == gen) {
            *g = None;
            inner.live.fetch_sub(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn mark_deliberate(&self) {
        if let Some(p) = self.proc.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            p.deliberate.store(true, Ordering::SeqCst);
        }
    }

    /// 主动杀掉当前进程组（不算崩溃）。先关 stdin 让它有机会优雅退出。
    fn kill_deliberate(&self, inner: &Inner) {
        let (gen, pgid) = match self.proc.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(p) => (p.gen, p.pgid),
            None => return,
        };
        self.mark_deliberate();
        let _ = self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
        kill_group(pgid, libc::SIGTERM);
        if !self.wait_gone(gen, Duration::from_millis(500)) {
            kill_group(pgid, libc::SIGKILL);
            // 兜底：wait 线程迟迟没回收（理论上不会），自己清槽，保证计数不漂。
            let _ = self.wait_gone(gen, Duration::from_millis(1000));
            if self.clear_proc(inner, gen) {
                let _ = self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
                forget_orphan(&self.id);
            }
        }
    }

    fn wait_gone(&self, gen: u64, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if !matches!(self.proc.lock().unwrap_or_else(|e| e.into_inner()).as_ref(), Some(p) if p.gen == gen) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /* ----- spawn ----- */

    fn spawn(self: &Arc<Self>, inner: &Arc<Inner>) -> Result<(), String> {
        let _g = self.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.spawn_inner(inner)
    }

    /// 起进程并握手。**调用方必须已持 `gate_lock`**。
    fn spawn_inner(self: &Arc<Self>, inner: &Arc<Inner>) -> Result<(), String> {
        if self.proc.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return Ok(());
        }
        if inner.shutting_down.load(Ordering::SeqCst) {
            return Err("应用正在退出".to_string());
        }
        install_assets()?;
        let bin = {
            let g = inner.bin.lock().unwrap_or_else(|e| e.into_inner());
            match g.clone() {
                Some(b) => b,
                None => ai_settings::resolve_pi(&ai_settings::load())?,
            }
        };
        make_room(inner, self)?;

        let root = assets_dir();
        let ext = root.join("pc-extension.ts");
        let system = root.join("system.md");
        let sessions = sessions_dir();
        std::fs::create_dir_all(&sessions).map_err(|e| format!("创建会话目录失败：{e}"))?;

        let (desired, thinking) = {
            let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            (st.desired_model.clone(), st.thinking.clone())
        };
        let mut args: Vec<String> = bin.prefix_args.clone();
        args.extend([
            "--mode".into(),
            "rpc".into(),
            "-ne".into(),
            "-e".into(),
            ext.display().to_string(),
            "-ns".into(),
            "-np".into(),
            "--no-themes".into(),
            "--no-mcp".into(),
            "-na".into(),
            "--offline".into(),
            "--tools".into(),
            "read,grep,find,ls,bash,pc_*".into(),
            "--append-system-prompt".into(),
            system.display().to_string(),
            "--session-dir".into(),
            sessions.display().to_string(),
            "--session-id".into(),
            self.id.clone(),
        ]);
        if let Some(m) = &desired {
            if !m.id.is_empty() {
                args.push("--model".into());
                args.push(format!("{}/{}", m.provider, m.id));
            }
        }
        if let Some(t) = &thinking {
            args.push("--thinking".into());
            args.push(t.clone());
        }

        let mut cmd = Command::new(&bin.program);
        cmd.args(&args);
        cmd.current_dir(&self.root);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.env_clear();
        let extra = [
            ("PI_TELEMETRY".to_string(), "0".to_string()),
            ("PC_AI_SESSION".to_string(), self.id.clone()),
        ];
        cmd.envs(crate::execenv::env_for(None, &BTreeMap::new(), &extra));
        // 只透传模型凭据类变量：其余一律不继承（与 supervisor 的 `env_clear` 策略一致）。
        for (k, v) in passthrough_env() {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| format!("启动 pi 失败：{e}"))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            let _ = child.kill();
            return Err("pi 的输入输出管道不可用".to_string());
        };
        if stdin.is_none() {
            let _ = child.kill();
            return Err("pi 的 stdin 不可用".to_string());
        }

        let gen = inner.spawn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let deliberate = Arc::new(AtomicBool::new(false));
        let drained = Arc::new(AtomicBool::new(false));
        *self.stdin.lock().unwrap_or_else(|e| e.into_inner()) = stdin;
        *self.proc.lock().unwrap_or_else(|e| e.into_inner()) = Some(Proc {
            pid,
            pgid: pid as i32,
            gen,
            deliberate: Arc::clone(&deliberate),
            drained: Arc::clone(&drained),
        });
        inner.live.fetch_add(1, Ordering::SeqCst);
        write_orphan(&self.id, pid, pid as i32);
        self.set_state(inner, "starting");

        // 读线程
        let s = Arc::clone(self);
        let i2 = Arc::clone(inner);
        let done = Arc::clone(&drained);
        let name = format!("ai-out-{}", self.id);
        let _ = std::thread::Builder::new().name(name).spawn(move || {
            read_stdout(s, i2, stdout);
            done.store(true, Ordering::SeqCst);
        });
        // stderr 线程
        let s = Arc::clone(self);
        let name = format!("ai-err-{}", self.id);
        let _ = std::thread::Builder::new().name(name).spawn(move || {
            read_stderr(s, stderr);
        });
        // wait 线程
        let s = Arc::clone(self);
        let i2 = Arc::clone(inner);
        let done = Arc::clone(&drained);
        let name = format!("ai-wait-{}", self.id);
        let _ = std::thread::Builder::new().name(name).spawn(move || {
            let status = child.wait();
            // 崩溃前最后吐出的事件可能还在管道里：等读线程读干净再结算，否则
            // provider 的失败会晚于 `on_crash` 落到会话里，漏掉本轮的终态提示。
            let end = Instant::now() + Duration::from_secs(2);
            while !done.load(Ordering::SeqCst) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            let killed = deliberate.load(Ordering::SeqCst);
            let cleared = s.clear_proc(&i2, gen);
            if cleared {
                let _ = s.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
                forget_orphan(&s.id);
            }
            if killed || i2.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            let code = match status {
                Ok(s) => s.code().map(|c| c.to_string()).unwrap_or_else(|| "被信号终止".to_string()),
                Err(e) => e.to_string(),
            };
            s.on_crash(&i2, code);
        });

        // 握手：`get_state` 回来才算起好了。
        let hs = Duration::from_millis(inner.handshake_ms.load(Ordering::SeqCst));
        match self.request("get_state", Value::Null, hs) {
            Ok(_) => {
                self.set_state(inner, "idle");
                Ok(())
            }
            Err(e) => {
                self.kill_deliberate(inner);
                let tail = self.stderr_tail(20);
                let body = if tail.trim().is_empty() { e } else { format!("{e}\n{tail}") };
                self.push_notice("error", format!("AI 进程启动失败：{body}"), true);
                self.set_state(inner, "failed");
                Err(format!("AI 进程启动失败：{body}"))
            }
        }
    }

    /* ----- 崩溃 ----- */

    fn mark_interrupted(&self) {
        let ids = {
            let mut g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *g)
        };
        self.set_open_assistant(None);
        for id in ids {
            self.update_item(&id, |b| match b {
                ItemBody::Tool { status, detail, .. } => {
                    if status != "blocked" {
                        *status = "error".to_string();
                    }
                    if detail.is_empty() {
                        detail.push_str("（进程中断）");
                    } else {
                        detail.push_str("\n（进程中断）");
                    }
                }
                ItemBody::Assistant { streaming, .. } => *streaming = false,
                _ => {}
            });
        }
    }

    fn set_open_assistant(&self, id: Option<String>) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.open = id;
    }

    /// 非主动退出：记录崩溃时刻，能恢复就复用同一 session id 重起。
    fn on_crash(self: &Arc<Self>, inner: &Arc<Inner>, code: String) {
        if inner.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let count = {
            let mut g = self.crashes.lock().unwrap_or_else(|e| e.into_inner());
            g.push_back(Instant::now());
            while g.front().is_some_and(|t| t.elapsed() > CRASH_WINDOW) {
                g.pop_front();
            }
            g.len()
        };
        self.mark_interrupted();
        // 进程没了：挂着的决定不可能再有答案，先唤醒等待者再谈恢复。
        self.drop_decisions();
        // 崩溃即回合结束（未必有 `agent_settled`）：provider 的失败先落成提示，
        // 崩溃/恢复通知排在它后面，用户读到的是「先原因、后结果」。
        self.flush_pending_error();
        let tail = self.stderr_tail(20);

        if count <= CRASH_MAX {
            self.set_state(inner, "recovering");
            self.push_notice("info", "AI 进程意外退出，已自动恢复上下文".to_string(), false);
            self.push_notice(
                "warn",
                format!("上一轮回复被中断（退出码 {code}），可以重试"),
                true,
            );
            match self.spawn(inner) {
                Ok(()) => {}
                Err(e) => {
                    self.push_notice("error", format!("AI 进程恢复失败：{e}"), true);
                    self.set_state(inner, "failed");
                }
            }
        } else {
            let body = if tail.trim().is_empty() { format!("退出码 {code}") } else { tail };
            self.push_notice("error", format!("AI 进程反复退出：{body}"), true);
            self.set_state(inner, "failed");
        }
    }

    /* ----- 事件处理 ----- */

    fn handle_line(self: &Arc<Self>, inner: &Arc<Inner>, line: &str) {
        let msg = match ai_proto::parse(line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("ai_session: 丢弃无法解析的行：{e}");
                return;
            }
        };
        match msg {
            Incoming::Response { ref id, .. } => {
                let Some(id) = id.clone() else { return };
                let tx = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                if let Some(tx) = tx {
                    let _ = tx.try_send(msg);
                }
            }
            Incoming::UiRequest { id, method, title, placeholder, message } => match method.as_str() {
                "input" => match title.as_deref() {
                    Some("pc-gate") => self.handle_gate(&id, placeholder.as_deref()),
                    Some("pc-host") => self.handle_host(inner, &id, placeholder.as_deref()),
                    _ => {
                        let _ = self.write_frame(&ai_proto::ui_cancelled(&id));
                    }
                },
                "notify" => {
                    let text = message.unwrap_or_default();
                    if !text.trim().is_empty() {
                        self.push_notice("info", text, false);
                    }
                }
                // select/confirm/editor 等本步不支持的交互：一律取消，让 pi 走兜底。
                _ => {
                    let _ = self.write_frame(&ai_proto::ui_cancelled(&id));
                }
            },
            Incoming::Event { kind, raw } => self.handle_event(inner, &kind, &raw),
        }
    }

    fn handle_gate(self: &Arc<Self>, id: &str, placeholder: Option<&str>) {
        let payload = placeholder.and_then(|p| serde_json::from_str::<Value>(p).ok());
        let Some(payload) = payload else {
            let _ = self.write_frame(&ai_proto::ui_value(
                id,
                &json!({ "allow": false, "reason": "权限请求格式错误，已拦截" }).to_string(),
            ));
            return;
        };
        let Some(tool) = payload.get("tool").and_then(Value::as_str) else {
            let _ = self.write_frame(&ai_proto::ui_value(
                id,
                &json!({ "allow": false, "reason": "权限请求格式错误，已拦截" }).to_string(),
            ));
            return;
        };
        let input = payload.get("input").cloned().unwrap_or(Value::Null);
        let call_id = payload.get("toolCallId").and_then(Value::as_str).map(str::to_string);
        let decision = ai_gate::decide(&self.root, tool, &input);
        let reply = json!({
            "allow": decision.allow,
            "reason": decision.reason.clone(),
            "patch": decision.patch.clone(),
        });
        let _ = self.write_frame(&ai_proto::ui_value(id, &reply.to_string()));

        if !decision.allow {
            let reason = decision.reason.clone().unwrap_or_else(|| "已拦截".to_string());
            if let Some(cid) = call_id {
                let item_id = format!("t-{cid}");
                let summary = tool_summary(tool, &input);
                let name = tool.to_string();
                let marked = self.update_item(&item_id, |b| {
                    if let ItemBody::Tool { status, detail, .. } = b {
                        *status = "blocked".to_string();
                        *detail = reason.clone();
                    }
                });
                if !marked {
                    self.push_with_id(
                        item_id,
                        ItemBody::Tool {
                            name,
                            summary,
                            status: "blocked".to_string(),
                            detail: reason,
                        },
                    );
                }
            }
        }
    }

    fn handle_host(self: &Arc<Self>, inner: &Arc<Inner>, id: &str, placeholder: Option<&str>) {
        let parsed = placeholder.and_then(|p| serde_json::from_str::<Value>(p).ok());
        let (method, params) = match parsed {
            Some(v) => (
                v.get("method").and_then(Value::as_str).unwrap_or_default().to_string(),
                v.get("params").cloned().unwrap_or(Value::Null),
            ),
            None => (String::new(), Value::Null),
        };
        let handler = inner.host.read().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(handler) = handler else {
            let _ = self.write_frame(&ai_proto::ui_value(
                id,
                &json!({ "ok": false, "error": "该能力尚未接入" }).to_string(),
            ));
            return;
        };
        let s = Arc::clone(self);
        let handle = self.handle(inner);
        let reply_id = id.to_string();
        self.host_pending.fetch_add(1, Ordering::SeqCst);
        {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.awaiting_user = true;
        }
        emit_session(inner, self);
        let inner2 = Arc::clone(inner);
        let _ = std::thread::Builder::new().name(format!("ai-host-{}", self.id)).spawn(move || {
            let result = handler(&handle, &method, &params);
            let body = match result {
                Ok(v) => json!({ "ok": true, "result": v }),
                Err(e) => json!({ "ok": false, "error": e }),
            };
            let _ = s.write_frame(&ai_proto::ui_value(&reply_id, &body.to_string()));
            s.host_pending.fetch_sub(1, Ordering::SeqCst);
            {
                let mut st = s.st.lock().unwrap_or_else(|e| e.into_inner());
                st.awaiting_user = false;
            }
            emit_session(&inner2, &s);
        });
    }

    fn handle_event(self: &Arc<Self>, inner: &Arc<Inner>, kind: &str, raw: &Value) {
        match kind {
            "message_start" => {
                if raw.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                    let id = format!("a-{}", self.next_item_no());
                    self.set_open_assistant(Some(id.clone()));
                    self.inflight.lock().unwrap_or_else(|e| e.into_inner()).push(id.clone());
                    self.push_with_id(
                        id,
                        ItemBody::Assistant {
                            text: String::new(),
                            thinking: String::new(),
                            streaming: true,
                        },
                    );
                    self.bump_busy(inner);
                }
            }
            "message_update" => {
                let ev = raw.get("assistantMessageEvent").cloned().unwrap_or(Value::Null);
                let delta = ev.get("delta").and_then(Value::as_str).unwrap_or_default().to_string();
                if delta.is_empty() {
                    return;
                }
                let open = self.log.lock().unwrap_or_else(|e| e.into_inner()).open.clone();
                let Some(open) = open else { return };
                match ev.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        self.update_item(&open, |b| {
                            if let ItemBody::Assistant { text, .. } = b {
                                append_capped(text, &delta, TEXT_CAP);
                            }
                        });
                    }
                    Some("thinking_delta") => {
                        self.update_item(&open, |b| {
                            if let ItemBody::Assistant { thinking, .. } = b {
                                append_capped(thinking, &delta, TEXT_CAP);
                            }
                        });
                    }
                    _ => {}
                }
            }
            "message_end" => {
                if raw.pointer("/message/role").and_then(Value::as_str) != Some("assistant") {
                    return;
                }
                let stop_reason = raw.pointer("/message/stopReason").and_then(Value::as_str);
                let provider_err = raw.pointer("/message/errorMessage").and_then(Value::as_str);
                let open = self.log.lock().unwrap_or_else(|e| e.into_inner()).open.clone();
                // pi 不保证先发 `message_start`（失败回合可能只有一条 `message_end`）：
                // 没有 open 条目也要把错误报出去，不能在这里提前返回。
                if let Some(open) = open {
                    let (text, thinking) = message_content(raw.get("message"));
                    // provider 失败时 pi 的 `content` 往往是空的：别把空气泡留在转录里。
                    let text = if stop_reason == Some("error") && text.trim().is_empty() {
                        "（本轮没有返回内容）".to_string()
                    } else {
                        text
                    };
                    self.update_item(&open, |b| {
                        if let ItemBody::Assistant { text: t, thinking: th, streaming } = b {
                            *t = clip(text, TEXT_CAP);
                            *th = clip(thinking, TEXT_CAP);
                            *streaming = false;
                        }
                    });
                    self.forget_inflight(&open);
                }
                self.set_open_assistant(None);
                // `aborted` 已由 abort 路径处理，这里不做额外动作。
                if stop_reason == Some("error") {
                    self.report_provider_error(provider_err);
                }
            }
            "tool_execution_start" => {
                let call_id = raw.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                let name = raw.get("toolName").and_then(Value::as_str).unwrap_or_default();
                if call_id.is_empty() {
                    return;
                }
                let args = raw.get("args").cloned().unwrap_or(Value::Null);
                let item_id = format!("t-{call_id}");
                let summary = tool_summary(name, &args);
                let tool_name = name.to_string();
                let existed = self.update_item(&item_id, |b| {
                    if let ItemBody::Tool { name, summary: su, status, .. } = b {
                        *name = tool_name.clone();
                        *su = summary.clone();
                        // 闸门已拦下的不能降级回 running。
                        if status != "blocked" {
                            *status = "running".to_string();
                        }
                    }
                });
                if !existed {
                    self.push_with_id(
                        item_id.clone(),
                        ItemBody::Tool {
                            name: tool_name,
                            summary,
                            status: "running".to_string(),
                            detail: String::new(),
                        },
                    );
                }
                self.inflight.lock().unwrap_or_else(|e| e.into_inner()).push(item_id);
                self.bump_busy(inner);
            }
            "tool_execution_end" => {
                let call_id = raw.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                if call_id.is_empty() {
                    return;
                }
                let item_id = format!("t-{call_id}");
                let is_error = raw.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let detail = clip(tool_result_text(raw.get("result")), DETAIL_CAP);
                self.update_item(&item_id, |b| {
                    if let ItemBody::Tool { status, detail: d, .. } = b {
                        if status != "blocked" {
                            *status = if is_error { "error".to_string() } else { "ok".to_string() };
                        }
                        *d = detail.clone();
                    }
                });
                self.forget_inflight(&item_id);
            }
            "agent_settled" => {
                {
                    let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
                    log.open = None;
                    // 先算好递增的 rev，避免在遍历里再借 `log`。
                    let mut next = log.next_rev;
                    for it in log.items.iter_mut() {
                        if let ItemBody::Assistant { streaming, .. } = &mut it.body {
                            if *streaming {
                                *streaming = false;
                                it.rev = next;
                                next += 1;
                            }
                        }
                    }
                    log.next_rev = next;
                    log.dirty = true;
                }
                self.inflight.lock().unwrap_or_else(|e| e.into_inner()).clear();
                {
                    let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                    // 退出/关闭过程中不要把状态拉回 idle。
                    if st.state != "failed" {
                        st.state = "idle".to_string();
                    }
                    st.task = None;
                    st.awaiting_user = false;
                    st.last_active = now_secs();
                }
                // 回合收尾：本轮攒下的模型失败现在落成最后一条提示。
                self.flush_pending_error();
                emit_session(inner, self);
            }
            "auto_retry_start" => {
                let attempt = raw.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                let max = raw.get("maxAttempts").and_then(Value::as_u64).unwrap_or(0);
                let why = raw.get("errorMessage").and_then(Value::as_str).unwrap_or("模型调用失败");
                self.push_notice(
                    "warn",
                    format!("模型调用失败，正在重试（{attempt}/{max}）：{why}"),
                    true,
                );
            }
            "auto_retry_end" => {
                let ok = raw.get("success").and_then(Value::as_bool).unwrap_or(false);
                if ok {
                    // 重试成功：本轮没有失败要报。
                    *self.pending_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    self.push_notice("info", "重试成功".to_string(), false);
                } else {
                    let why = raw.get("finalError").and_then(Value::as_str).unwrap_or("重试失败");
                    let text = format!("重试失败：{}", why.trim());
                    *self.pending_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
                }
            }
            "extension_error" => {
                let msg = raw.get("error").and_then(Value::as_str).unwrap_or("扩展执行出错");
                self.push_notice("error", format!("AI 扩展出错：{msg}"), false);
            }
            _ => {}
        }
    }

    fn forget_inflight(&self, id: &str) {
        let mut g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|x| x != id);
    }

    /// 收到第一条事件时把 `starting/idle` 拉成 `busy`（prompt 已先置 busy，幂等）。
    fn bump_busy(&self, inner: &Inner) {
        let changed = {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.last_stdout = Instant::now();
            st.last_active = now_secs();
            if st.state == "idle" || st.state == "starting" {
                st.state = "busy".to_string();
                true
            } else {
                false
            }
        };
        if changed {
            emit_session(inner, self);
        }
    }

    /* ----- 模型 ----- */

    fn sync_model(&self) -> Result<(), String> {
        let (desired, thinking) = {
            let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            (st.desired_model.clone(), st.thinking.clone())
        };
        if let Some(m) = desired {
            if m.id.is_empty() {
                return Ok(());
            }
            let label = format!("{}/{}", m.provider, m.id);
            let same = {
                let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                st.model.as_deref() == Some(label.as_str())
            };
            if !same {
                let rep = self.request(
                    "set_model",
                    json!({ "provider": m.provider, "modelId": m.id }),
                    REQ_TIMEOUT,
                )?;
                if rep.success {
                    let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                    st.model = Some(label);
                }
            }
        }
        if let Some(level) = thinking {
            if !level.is_empty() {
                let _ = self.request(
                    "set_thinking_level",
                    json!({ "level": level }),
                    REQ_TIMEOUT,
                );
            }
        }
        Ok(())
    }

    /* ----- 生命周期 ----- */

    fn abort_locked(self: &Arc<Self>, inner: &Arc<Inner>) -> Result<(), String> {
        let busy = {
            let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.state == "busy"
        };
        if !busy {
            return Ok(());
        }
        let _ = self.request("abort", Value::Null, REQ_TIMEOUT);
        // 等 agent_settled 把状态收回去再杀，给 pi 机会把会话文件写完整。
        let end = Instant::now() + ABORT_WAIT;
        while Instant::now() < end {
            let state = {
                let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                st.state.clone()
            };
            if state != "busy" {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.kill_deliberate(inner);
        // 本轮已终止：还在等用户确认的卡片一并作废，别让任务线程僵在那儿。
        self.drop_decisions();
        // 回合以 abort 收尾（可能没有 `agent_settled`）：把待报的失败落成提示。
        self.flush_pending_error();
        {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            if st.state != "failed" {
                st.state = "idle".to_string();
            }
            st.task = None;
            st.awaiting_user = false;
            st.last_active = now_secs();
        }
        emit_session(inner, self);
        Ok(())
    }

    fn sleep_locked(self: &Arc<Self>, inner: &Arc<Inner>) {
        if self.proc.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            self.set_state(inner, "sleeping");
            return;
        }
        let (gen, pgid) = match self.proc.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(p) => (p.gen, p.pgid),
            None => return,
        };
        self.mark_deliberate();
        self.drop_decisions();
        // 先关 stdin：pi 收到 EOF 会自己收尾，比直接发信号干净。
        let _ = self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
        if !self.wait_gone(gen, Duration::from_secs(5)) {
            kill_group(pgid, libc::SIGTERM);
            if !self.wait_gone(gen, Duration::from_millis(500)) {
                kill_group(pgid, libc::SIGKILL);
                let _ = self.wait_gone(gen, Duration::from_secs(1));
                if self.clear_proc(inner, gen) {
                    forget_orphan(&self.id);
                }
            }
        }
        self.set_state(inner, "sleeping");
    }
}

/// 命令应答投影。
struct Reply {
    success: bool,
    error: Option<String>,
}

/* ------------------------------------------------------------------------- *
 * 管理器
 * ------------------------------------------------------------------------- */

pub(crate) struct Inner {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    by_project: Mutex<HashMap<i64, String>>,
    live: AtomicUsize,
    shutting_down: AtomicBool,
    sink: OnceLock<Arc<dyn AiSink>>,
    host: RwLock<Option<HostHandler>>,
    /// 测试用的固定 pi 可执行文件；生产为 None（走设置解析）。
    bin: Mutex<Option<PiBin>>,
    /// 握手超时（毫秒）。测试会调小。
    handshake_ms: AtomicU64,
    spawn_gen: AtomicU64,
}

/// AI 会话管理器（Tauri managed state）。
#[derive(Clone)]
pub struct AiManager(Arc<Inner>);

impl Default for AiManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AiManager {
    pub fn new() -> Self {
        let inner = Arc::new(Inner {
            sessions: Mutex::new(HashMap::new()),
            by_project: Mutex::new(HashMap::new()),
            live: AtomicUsize::new(0),
            shutting_down: AtomicBool::new(false),
            sink: OnceLock::new(),
            host: RwLock::new(None),
            bin: Mutex::new(None),
            handshake_ms: AtomicU64::new(DEFAULT_HANDSHAKE.as_millis() as u64),
            spawn_gen: AtomicU64::new(0),
        });
        spawn_flusher(Arc::clone(&inner));
        spawn_sweeper(Arc::clone(&inner));
        Self(inner)
    }

    pub fn set_sink(&self, sink: Arc<dyn AiSink>) {
        let _ = self.0.sink.set(sink);
    }

    /// 把 Tauri 的 emit 实现装成出口（后续步骤在 `run` 里调）。
    pub fn attach<R: tauri::Runtime>(&self, app: &tauri::AppHandle<R>) {
        self.set_sink(Arc::new(Ctx::new(app.clone())));
    }

    pub fn set_host_handler(&self, h: HostHandler) {
        *self.0.host.write().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    /// 当前运行中的会话进程数。
    pub fn live(&self) -> usize {
        self.0.live.load(Ordering::SeqCst)
    }

    /// 取会话句柄（卡片动作 / 宿主钩子需要）。
    pub fn handle(&self, sid: &str) -> Result<SessionHandle, String> {
        let s = self.get(sid)?;
        Ok(s.handle(&self.0))
    }

    /// 把批量画像快照推给前端（`ai:batch`）。
    pub fn emit_batch(&self, items: &[BatchItem]) {
        if let Some(sink) = self.0.sink.get() {
            sink.batch(items);
        }
    }

    fn get(&self, sid: &str) -> Result<Arc<Session>, String> {
        self.0
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sid)
            .cloned()
            .ok_or_else(|| "会话不存在".to_string())
    }

    /// 打开（或复用）某个项目的会话。**不**起进程。
    pub fn open(&self, project_id: i64, path: &str, name: &str) -> Result<AiSnapshot, String> {
        {
            let by = self.0.by_project.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(sid) = by.get(&project_id).cloned() {
                if let Some(s) = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(&sid).cloned() {
                    return Ok(s.snapshot());
                }
            }
        }
        if path.trim().is_empty() {
            return Err("项目不存在".to_string());
        }
        let sid = new_session_id(project_id);
        let s = Arc::new(Session::new(
            sid.clone(),
            project_id,
            name.to_string(),
            PathBuf::from(path),
        ));
        let snap = s.snapshot();
        self.0.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(sid.clone(), s);
        self.0.by_project.lock().unwrap_or_else(|e| e.into_inner()).insert(project_id, sid);
        if let Some(sink) = self.0.sink.get() {
            sink.session(&snap.session);
        }
        Ok(snap)
    }

    pub fn snapshot(&self, sid: &str) -> Result<AiSnapshot, String> {
        self.get(sid).map(|s| s.snapshot())
    }

    pub fn sessions(&self) -> Vec<AiSessionView> {
        let mut out: Vec<AiSessionView> = self
            .0
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| s.view())
            .collect();
        out.sort_by_key(|v| std::cmp::Reverse(v.last_active));
        out
    }

    /// 项目列表徽标用的批量快照（`profile` 留给后续步骤）。
    pub fn batch(&self) -> Vec<BatchItem> {
        let sessions: Vec<Arc<Session>> = self
            .0
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        sessions
            .into_iter()
            .map(|s| {
                let v = s.view();
                BatchItem {
                    project_id: v.project_id,
                    name: v.project_name,
                    state: v.state,
                    profile: None,
                    error: None,
                }
            })
            .collect()
    }

    pub fn send_prompt(&self, sid: &str, text: &str, task: Option<String>) -> Result<(), String> {
        let s = self.get(sid)?;
        let text = text.trim();
        if text.is_empty() {
            return Err("消息不能为空".to_string());
        }
        {
            let st = s.st.lock().unwrap_or_else(|e| e.into_inner());
            if st.state == "busy" {
                return Err("AI 正在运行，请等待或停止".to_string());
            }
        }
        let label = task.clone().unwrap_or_else(|| first_line(text, 60));
        s.push_item(ItemBody::User { text: text.to_string(), task: task.clone() });
        *s.last_prompt.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_string());

        let outcome = {
            let _g = s.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
            match s.spawn_inner(&self.0) {
                Ok(()) => match s.sync_model() {
                    Ok(()) => {
                        {
                            let mut st = s.st.lock().unwrap_or_else(|e| e.into_inner());
                            st.state = "busy".to_string();
                            st.task = Some(label);
                            st.awaiting_user = false;
                            st.last_active = now_secs();
                        }
                        emit_session(&self.0, &s);
                        s.request("prompt", json!({ "message": text }), REQ_TIMEOUT)
                    }
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            }
        };

        match outcome {
            Ok(rep) if rep.success => Ok(()),
            Ok(rep) => {
                let err = rep.error.unwrap_or_else(|| "AI 拒绝了这个请求".to_string());
                let onboarding = ["No model selected", "No API key", "Authentication failed"]
                    .iter()
                    .any(|p| err.starts_with(p));
                {
                    let mut st = s.st.lock().unwrap_or_else(|e| e.into_inner());
                    st.state = "idle".to_string();
                    st.task = None;
                }
                emit_session(&self.0, &s);
                let id = format!("onb-{}", s.next_item_no());
                if onboarding {
                    let card = json!({ "kind": "onboarding", "id": id, "problem": err });
                    s.push_with_id(id, ItemBody::Card { card });
                } else {
                    s.push_notice("error", err, false);
                }
                Ok(())
            }
            Err(e) => {
                // 写不进去 / 超时：进程还在就按卡死走崩溃恢复；
                // 进程已经没了（握手失败）则保持 spawn 给出的 failed，不重复恢复。
                if s.proc.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                    s.kill_deliberate(&self.0);
                    s.on_crash(&self.0, e.clone());
                }
                Err(e)
            }
        }
    }

    /// 投递用户对卡片的选择。没有等待者（已超时/已取消/会话已关）返回错误。
    pub fn decide(&self, session_id: &str, card_id: &str, decision: Value) -> Result<(), String> {
        let s = self.get(session_id)?;
        let tx = s
            .decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(card_id)
            .ok_or_else(|| "该卡片已失效".to_string())?;
        // 等待者超时后可能刚好注销：投递失败也按失效处理。
        tx.send(decision).map_err(|_| "该卡片已失效".to_string())
    }

    /// 标记本会话是否在等待用户（映进会话视图的 `awaiting_user`）。
    pub fn set_awaiting(&self, session_id: &str, on: bool) {
        let Ok(s) = self.get(session_id) else { return };
        {
            let mut st = s.st.lock().unwrap_or_else(|e| e.into_inner());
            st.awaiting_user = on;
        }
        emit_session(&self.0, &s);
    }

    pub fn retry(&self, sid: &str) -> Result<(), String> {
        let s = self.get(sid)?;
        let text = s
            .last_prompt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "还没有可重试的消息".to_string())?;
        self.send_prompt(sid, &text, None)
    }

    pub fn abort(&self, sid: &str) -> Result<(), String> {
        let s = self.get(sid)?;
        let _g = s.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
        s.abort_locked(&self.0)
    }

    /// 回收该会话的进程（省内存），下次 `send_prompt` 会用同一个 session id 重起。
    pub fn sleep(&self, sid: &str) -> Result<(), String> {
        let s = self.get(sid)?;
        let _g = s.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
        s.sleep_locked(&self.0);
        Ok(())
    }

    pub fn set_model(
        &self,
        sid: &str,
        model: Option<ModelRef>,
        thinking: Option<String>,
    ) -> Result<(), String> {
        let s = self.get(sid)?;
        if let Some(t) = &thinking {
            if !ai_settings::thinking_ok(t) {
                return Err(format!("思考档位无效：{t}"));
            }
        }
        {
            let mut st = s.st.lock().unwrap_or_else(|e| e.into_inner());
            st.desired_model = model.clone();
            if let Some(m) = &model {
                st.model = Some(format!("{}/{}", m.provider, m.id));
            }
            if thinking.is_some() {
                st.thinking = thinking;
            }
        }
        emit_session(&self.0, &s);
        let live = s.proc.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        if live {
            let _g = s.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = s.sync_model();
        }
        Ok(())
    }

    pub fn close(&self, sid: &str) -> Result<(), String> {
        let s = {
            let mut m = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
            m.remove(sid)
        };
        let Some(s) = s else { return Ok(()) };
        {
            let mut by = self.0.by_project.lock().unwrap_or_else(|e| e.into_inner());
            if by.get(&s.project_id).map(String::as_str) == Some(sid) {
                by.remove(&s.project_id);
            }
        }
        {
            let _g = s.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
            s.kill_deliberate(&self.0);
        }
        // 会话没了：等决定的线程必须立刻醒（收 Disconnected）。
        s.drop_decisions();
        forget_orphan(sid);
        remove_session_files(sid);
        Ok(())
    }

    pub fn close_project(&self, project_id: i64) -> Result<(), String> {
        let sid = {
            self.0
                .by_project
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&project_id)
        };
        match sid {
            Some(sid) => self.close(&sid),
            None => Ok(()),
        }
    }

    /// 退出前收尾：先让宿主钩子回滚进行中的校验，再逐级关流、发信号。
    pub fn shutdown_all(&self) {
        self.0.shutting_down.store(true, Ordering::SeqCst);
        let sessions: Vec<Arc<Session>> = self
            .0
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();

        // ① 回滚进行中的校验循环（没有宿主钩子就跳过）。
        let handler = self.0.host.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(h) = handler {
            for s in &sessions {
                let handle = s.handle(&self.0);
                let _ = h(&handle, "shutdown", &Value::Null);
            }
        }
        // ② 关闭所有 stdin（pi 收到 EOF 会自行收尾），并唤醒等决定的线程。
        for s in &sessions {
            s.mark_deliberate();
            let _ = s.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
            s.drop_decisions();
        }
        std::thread::sleep(Duration::from_millis(1500));
        // ③ SIGTERM，再等 0.5s，仍活着就 SIGKILL。
        let mut alive: Vec<(Arc<Session>, u64, i32)> = Vec::new();
        for s in &sessions {
            if let Some(p) = s.proc.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                kill_group(p.pgid, libc::SIGTERM);
                alive.push((Arc::clone(s), p.gen, p.pgid));
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        for (s, gen, pgid) in alive {
            let still = matches!(
                s.proc.lock().unwrap_or_else(|e| e.into_inner()).as_ref(),
                Some(p) if p.gen == gen
            );
            if still {
                kill_group(pgid, libc::SIGKILL);
                let _ = s.wait_gone(gen, Duration::from_secs(1));
                if s.clear_proc(&self.0, gen) {
                    forget_orphan(&s.id);
                }
            }
        }
        // ④ 清掉会话文件与孤儿记录。
        clear_dir(&sessions_dir());
        clear_dir(&orphan_dir());
    }

    /// 只给测试与后续步骤用：某会话当前的进程 pid。
    pub fn pid_of(&self, sid: &str) -> Option<u32> {
        let s = self.get(sid).ok()?;
        let g = s.proc.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|p| p.pid)
    }

    /// 测试用：固定 pi 可执行文件 + 记录型出口。握手超时可另调 [`Self::set_handshake`]。
    #[cfg(test)]
    pub fn for_test(bin: PiBin, sink: Arc<dyn AiSink>) -> Self {
        let m = Self::new();
        *m.0.bin.lock().unwrap_or_else(|e| e.into_inner()) = Some(bin);
        let _ = m.0.sink.set(sink);
        m
    }

    /// 缩短握手等待（测试里不想真等 20s）。
    #[cfg(test)]
    pub fn set_handshake(&self, d: Duration) {
        self.0.handshake_ms.store(d.as_millis() as u64, Ordering::SeqCst);
    }
}

/* ------------------------------------------------------------------------- *
 * 线程
 * ------------------------------------------------------------------------- */

fn emit_session(inner: &Inner, s: &Session) {
    if let Some(sink) = inner.sink.get() {
        sink.session(&s.view());
    }
}

/// 全局唯一的冲刷线程：每 [`FLUSH`] 把脏会话的条目合成一批 emit。
fn spawn_flusher(inner: Arc<Inner>) {
    let _ = std::thread::Builder::new().name("ai-flush".to_string()).spawn(move || loop {
        std::thread::sleep(FLUSH);
        let Some(sink) = inner.sink.get().map(Arc::clone) else { continue };
        let sessions: Vec<Arc<Session>> = inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for s in sessions {
            let payload = {
                let mut log = s.log.lock().unwrap_or_else(|e| e.into_inner());
                log.take_batch().map(|(from, to, items)| ItemsPayload {
                    session_id: s.id.clone(),
                    project_id: s.project_id,
                    from_rev: from,
                    to_rev: to,
                    items,
                })
            };
            if let Some(p) = payload {
                sink.items(&p);
            }
        }
    });
}

/// 全局唯一的看门狗：卡死的回合终止，长期空闲的会话休眠。
fn spawn_sweeper(inner: Arc<Inner>) {
    let _ = std::thread::Builder::new().name("ai-sweep".to_string()).spawn(move || loop {
        std::thread::sleep(SWEEP);
        if inner.shutting_down.load(Ordering::SeqCst) {
            continue;
        }
        let sessions: Vec<Arc<Session>> = inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for s in sessions {
            let (state, silent, idle_for) = {
                let st = s.st.lock().unwrap_or_else(|e| e.into_inner());
                (
                    st.state.clone(),
                    st.last_stdout.elapsed(),
                    now_secs().saturating_sub(st.last_active),
                )
            };
            let host = s.host_pending.load(Ordering::SeqCst);
            if state == "busy" && silent > BUSY_SILENT && host == 0 {
                {
                    let _g = s.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = s.abort_locked(&inner);
                }
                s.push_notice("warn", "AI 无响应，已终止本轮".to_string(), true);
            } else if state == "idle" && idle_for > IDLE_SLEEP {
                let _g = s.gate_lock.lock().unwrap_or_else(|e| e.into_inner());
                s.sleep_locked(&inner);
            }
        }
    });
}

fn read_stdout(s: Arc<Session>, inner: Arc<Inner>, mut out: impl Read + Send) {
    let mut splitter = LineSplitter::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match out.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                s.touch_stdout();
                for line in splitter.push(&buf[..n]) {
                    match line {
                        Ok(l) => s.handle_line(&inner, &l),
                        Err(e) => eprintln!("ai_session: 丢弃异常行：{e}"),
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn read_stderr(s: Arc<Session>, mut err: impl Read + Send) {
    let mut splitter = LineSplitter::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match err.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for line in splitter.push(&buf[..n]).into_iter().flatten() {
                    let mut g = s.stderr.lock().unwrap_or_else(|e| e.into_inner());
                    if g.len() >= STDERR_CAP {
                        g.pop_front();
                    }
                    g.push_back(line);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/* ------------------------------------------------------------------------- *
 * 资源 / 目录 / 杂项
 * ------------------------------------------------------------------------- */

/// 资源目录覆盖：只给测试用（生产为 None）。
///
/// 测试若改 `HOME` 会污染整个进程——`docker compose` 这类外部命令按 `$HOME` 找插件，
/// 并行跑的 e2e 会因此莫名其妙地失败。所以隔离只覆盖这一个目录，不动进程环境。
static ASSETS_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 测试专用：把资源目录指到 `dir`，离开作用域时恢复。
#[cfg(test)]
struct AssetsGuard;

#[cfg(test)]
impl AssetsGuard {
    fn new(dir: &Path) -> Self {
        *ASSETS_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
        Self
    }
}

#[cfg(test)]
impl Drop for AssetsGuard {
    fn drop(&mut self) {
        *ASSETS_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// `~/.project-ctrl/ai`（扩展与系统提示词的落点）。
fn assets_dir() -> PathBuf {
    if let Some(dir) = ASSETS_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return dir.clone();
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".project-ctrl")
        .join("ai")
}

fn sessions_dir() -> PathBuf {
    crate::db::data_dir().join("ai").join("sessions")
}

fn orphan_dir() -> PathBuf {
    crate::localstate::state_dir().join("ai")
}

/// 把内嵌的扩展与系统提示词写到磁盘（内容不同才写）。
pub fn install_assets() -> Result<(), String> {
    let dir = assets_dir();
    write_if_changed(&dir.join("pc-extension.ts"), include_str!("ai_ext.ts"))?;
    write_if_changed(&dir.join("system.md"), include_str!("ai_system.md"))?;
    Ok(())
}

fn write_if_changed(path: &Path, bytes: &str) -> Result<(), String> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == bytes.as_bytes() {
            return Ok(());
        }
    }
    crate::localstate::atomic_write(path, bytes.as_bytes())
}

/// 启动清理：删掉上次的会话文件；把上次遗留的 pi 进程（验明正身后）收掉。
pub fn boot_cleanup() {
    clear_dir(&sessions_dir());
    let dir = orphan_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let rec = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if let Some(rec) = rec {
            kill_orphan(&rec);
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// 启动时间必须逐字相等：`ps` 的输出两侧都 trim 后比较（空值一律不算匹配）。
pub(crate) fn same_lstart(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && a == b
}

/// `cmd` 是 `ps -o command=` 的输出。true = 看起来是我们起的 pi。
///
/// pi 会把 `process.title` 改成 `pi`，命令行只剩 `pi`，所以不能再要求出现
/// `--mode rpc` / `pc-extension`；改用「像 pi」的白名单 + 明确的排除项。
pub(crate) fn looks_like_our_pi(cmd: &str) -> bool {
    let t = cmd.trim();
    let exe = t.split_whitespace().next().unwrap_or_default();
    // 排除项先判：本 App 自己的二进制（哪怕带着 `--mode rpc pc-extension`）绝不能被下面的
    // 白名单放过。只看可执行文件那一段，因为扩展路径本身就长在 `~/.project-ctrl` 下。
    if t.is_empty()
        || exe.contains("project-ctrl")
        || ["-zsh", "zsh", "bash", "-bash", "sh", "-sh"].iter().any(|p| t.starts_with(p))
    {
        return false;
    }
    // 白名单：进程名被改成 `pi`、带扩展/模式参数、node 起 cli.js 的形态。
    if t == "pi" || t.starts_with("pi ") {
        return true;
    }
    if t.contains("pi-coding-agent") || t.contains("pc-extension") || t.contains("--mode rpc") {
        return true;
    }
    if t == "node" || t.starts_with("node ") {
        return true;
    }
    exe.ends_with("cli.js")
}

fn kill_orphan(rec: &Value) {
    let Some(pid) = rec.get("pid").and_then(Value::as_u64) else { return };
    let Some(pgid) = rec.get("pgid").and_then(Value::as_i64) else { return };
    let lstart = rec.get("lstart").and_then(Value::as_str).unwrap_or_default();
    let pid = pid as u32;
    if !pid_alive(pid) {
        return;
    }
    // pid 会被系统复用：启动时间不符就绝不能杀。
    let actual = ps_lstart(pid).unwrap_or_default();
    if !same_lstart(&actual, lstart) {
        return;
    }
    // 进程组也要对得上：同 pid + 同启动时间的陌生进程仍不该动。
    let actual_pgid = ps_field(pid, "pgid=").unwrap_or_default();
    if actual_pgid.trim().is_empty() || actual_pgid.trim() != pgid.to_string() {
        return;
    }
    // 命令行只做「像 pi」的可行性判断（pi 把进程名改成 `pi` 后只剩 `pi`）。
    let cmd = ps_command(pid).unwrap_or_default();
    if !looks_like_our_pi(&cmd) {
        eprintln!("跳过回收 {pid}：命令行不像本 App 的 pi（{cmd}）");
        return;
    }
    let pgid = pgid as i32;
    if pgid <= 0 {
        return;
    }
    kill_group(pgid, libc::SIGTERM);
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end && pid_alive(pid) {
        std::thread::sleep(Duration::from_millis(20));
    }
    if pid_alive(pid) {
        kill_group(pgid, libc::SIGKILL);
    }
}

fn write_orphan(sid: &str, pid: u32, pgid: i32) {
    let lstart = ps_lstart(pid).unwrap_or_default();
    let rec = json!({ "pid": pid, "pgid": pgid, "lstart": lstart, "sid": sid });
    let _ = crate::localstate::atomic_write(
        &orphan_dir().join(format!("{sid}.json")),
        rec.to_string().as_bytes(),
    );
}

fn forget_orphan(sid: &str) {
    let _ = std::fs::remove_file(orphan_dir().join(format!("{sid}.json")));
}

/// 删掉该会话在 `--session-dir` 下的文件（pi 的会话文件名含 session id）。
fn remove_session_files(sid: &str) {
    let dir = sessions_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.contains(sid) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn clear_dir(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let _ = std::fs::remove_file(e.path());
    }
}

/// 只在进程上限内起新会话；满了先睡掉最久没动静的空闲会话。
fn make_room(inner: &Arc<Inner>, me: &Arc<Session>) -> Result<(), String> {
    if inner.live.load(Ordering::SeqCst) < MAX_LIVE {
        return Ok(());
    }
    let candidates: Vec<Arc<Session>> = inner
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|s| s.id != me.id)
        .cloned()
        .collect();
    // 两个锁分开取：先判状态、再判进程，避免与其它路径的加锁顺序交错。
    let victim = candidates
        .into_iter()
        .filter(|s| {
            let idle = s.st.lock().unwrap_or_else(|e| e.into_inner()).state == "idle";
            idle && s.proc.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        })
        .min_by_key(|s| s.st.lock().unwrap_or_else(|e| e.into_inner()).last_active);
    if let Some(v) = victim {
        if let Ok(g) = v.gate_lock.try_lock() {
            v.sleep_locked(inner);
            drop(g);
        }
    }
    if inner.live.load(Ordering::SeqCst) < MAX_LIVE {
        Ok(())
    } else {
        Err(format!("同时运行的 AI 会话已达上限（{MAX_LIVE}），请先结束其他任务"))
    }
}

/// 子进程只继承模型凭据类环境变量。
fn passthrough_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in std::env::vars() {
        let keep = k.ends_with("_API_KEY")
            || k.starts_with("ANTHROPIC_")
            || k.starts_with("AWS_")
            || k.starts_with("AZURE_OPENAI_")
            || k.starts_with("CLOUDFLARE_")
            || k == "PI_CODING_AGENT_DIR";
        if keep {
            out.push((k, v));
        }
    }
    out
}

fn kill_group(pgid: i32, sig: libc::c_int) {
    if pgid <= 1 {
        return;
    }
    // SAFETY：只是发信号；pgid 来自刚 spawn 且 `process_group(0)` 保证 pgid == pid 的子进程。
    unsafe {
        libc::kill(-pgid, sig);
    }
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ps_field(pid: u32, field: &str) -> Option<String> {
    let out = Command::new("/bin/ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(text)
}

fn ps_lstart(pid: u32) -> Option<String> {
    ps_field(pid, "lstart=")
}

fn ps_command(pid: u32) -> Option<String> {
    ps_field(pid, "command=")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn new_session_id(project_id: i64) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let raw = format!("pc-{project_id}-{ms}-{n}");
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' })
        .collect()
}

fn clip(s: impl AsRef<str>, cap: usize) -> String {
    let s = s.as_ref();
    if s.len() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap).collect();
    out.push_str("\n…（已截断）");
    out
}

fn append_capped(dst: &mut String, add: &str, cap: usize) {
    if dst.len() >= cap {
        return;
    }
    let room = cap - dst.len();
    if add.len() <= room {
        dst.push_str(add);
    } else {
        dst.extend(add.chars().take(room));
    }
}

fn first_line(text: &str, cap: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    clip(line, cap)
}

/// 从助手消息里抽正文与思考文本。
fn message_content(msg: Option<&Value>) -> (String, String) {
    let mut text = String::new();
    let mut thinking = String::new();
    let Some(content) = msg.and_then(|m| m.get("content")) else {
        return (text, thinking);
    };
    match content {
        Value::String(s) => text.push_str(s),
        Value::Array(items) => {
            for b in items {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                            thinking.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (text, thinking)
}

/// 工具结果 → 纯文本：优先拼 `content[].text`，否则退回 details / 原始 JSON。
fn tool_result_text(result: Option<&Value>) -> String {
    let Some(result) = result else { return String::new() };
    let mut out = String::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for b in items {
            if let Some(t) = b.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    if out.is_empty() {
        if let Some(d) = result.get("details") {
            if !d.is_null() {
                out = d.to_string();
            }
        }
    }
    if out.is_empty() {
        if let Some(s) = result.as_str() {
            out = s.to_string();
        } else if !result.is_null() {
            out = result.to_string();
        }
    }
    out
}

/// 工具调用的中文摘要（给对话流里的一行看）。
fn tool_summary(name: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    match name {
        "read" => format!("读取 {}", s("path")),
        "grep" => format!("搜索 \"{}\"", s("pattern")),
        "find" => format!("查找 {}", s("pattern")),
        "ls" => format!("列出 {}", s("path")),
        "bash" => format!("$ {}", s("command")),
        "pc_project_context" => "读取项目上下文".to_string(),
        "pc_service_logs" => "读取服务日志".to_string(),
        "pc_propose_services" => "提交服务配方".to_string(),
        "pc_report_diagnosis" => "提交诊断".to_string(),
        "pc_report_health" => "提交体检报告".to_string(),
        "pc_propose_profile" => "提交项目画像".to_string(),
        "pc_propose_readme" => "提交 README 草稿".to_string(),
        "pc_service_control" => "请求服务操作".to_string(),
        other => other.to_string(),
    }
}

/* ------------------------------------------------------------------------- *
 * Tauri 命令（由后续步骤注册）
 * ------------------------------------------------------------------------- */

fn manager<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<AiManager, String> {
    use tauri::Manager as _;
    app.try_state::<AiManager>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "AI 会话管理器未初始化".to_string())
}

/// 打开（或复用）项目的会话。不 spawn 进程。
#[tauri::command]
pub fn ai_open<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
) -> Result<AiSnapshot, String> {
    let conn = crate::db::open_db().map_err(|e| e.to_string())?;
    let path = crate::db::project_path(&conn, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "项目不存在".to_string())?;
    let name = crate::logic::project_name_of(Path::new(&path));
    manager(&app)?.open(id, &path, &name)
}

#[tauri::command]
pub fn ai_snapshot<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
) -> Result<AiSnapshot, String> {
    manager(&app)?.snapshot(&session_id)
}

#[tauri::command]
pub fn ai_sessions<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<Vec<AiSessionView>, String> {
    Ok(manager(&app)?.sessions())
}

#[tauri::command]
pub fn ai_send<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
    text: String,
) -> Result<(), String> {
    manager(&app)?.send_prompt(&session_id, &text, None)
}

#[tauri::command]
pub fn ai_retry<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
) -> Result<(), String> {
    manager(&app)?.retry(&session_id)
}

#[tauri::command]
pub fn ai_abort<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
) -> Result<(), String> {
    manager(&app)?.abort(&session_id)
}

#[tauri::command]
pub fn ai_close<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
) -> Result<(), String> {
    manager(&app)?.close(&session_id)
}

#[tauri::command]
pub fn ai_set_model<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
    model: Option<ModelRef>,
    thinking: Option<String>,
) -> Result<(), String> {
    manager(&app)?.set_model(&session_id, model, thinking)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScopedDataDir;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// 记录型出口：把推送原样存下来供断言。
    #[derive(Default)]
    struct Rec {
        items: StdMutex<Vec<ItemsPayload>>,
        sessions: StdMutex<Vec<AiSessionView>>,
        batches: StdMutex<Vec<BatchItem>>,
    }

    impl AiSink for Rec {
        fn items(&self, p: &ItemsPayload) {
            self.items.lock().unwrap_or_else(|e| e.into_inner()).push(p.clone());
        }
        fn session(&self, v: &AiSessionView) {
            self.sessions.lock().unwrap_or_else(|e| e.into_inner()).push(v.clone());
        }
        fn batch(&self, b: &[BatchItem]) {
            self.batches.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(b);
        }
    }

    impl Rec {
        /// 按 rev 覆盖合并所有批次的条目（同 id 取最大 rev）。
        fn all(&self) -> Vec<Item> {
            let mut by_id: HashMap<String, Item> = HashMap::new();
            for p in self.items.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                for it in &p.items {
                    match by_id.get(&it.id) {
                        Some(old) if old.rev >= it.rev => {}
                        _ => {
                            by_id.insert(it.id.clone(), it.clone());
                        }
                    }
                }
            }
            let mut out: Vec<Item> = by_id.into_values().collect();
            out.sort_by_key(|i| i.rev);
            out
        }

        fn assistants(&self) -> Vec<(String, String, bool)> {
            self.all()
                .into_iter()
                .filter_map(|i| match i.body {
                    ItemBody::Assistant { text, thinking, streaming } => Some((text, thinking, streaming)),
                    _ => None,
                })
                .collect()
        }

        fn tools(&self) -> Vec<(String, String, String, String)> {
            self.all()
                .into_iter()
                .filter_map(|i| match i.body {
                    ItemBody::Tool { name, summary, status, detail } => Some((name, summary, status, detail)),
                    _ => None,
                })
                .collect()
        }

        fn notices(&self) -> Vec<(String, String, bool)> {
            self.all()
                .into_iter()
                .filter_map(|i| match i.body {
                    ItemBody::Notice { level, text, retry } => Some((level, text, retry)),
                    _ => None,
                })
                .collect()
        }
    }

    /// 一套隔离环境：数据目录 + 资源目录都指到临时目录，pi 换成 fake 脚本。
    ///
    /// 刻意**不**改 `HOME`（见 `ASSETS_OVERRIDE` 的注释）。
    struct Env {
        _assets: AssetsGuard,
        _data: ScopedDataDir,
        _tmp: TempDir,
        root: PathBuf,
        argv_log: PathBuf,
        mgr: AiManager,
        sink: Arc<Rec>,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            self.mgr.shutdown_all();
        }
    }

    fn fake_pi(mode: Option<&str>, argv_log: &Path) -> PiBin {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ai_testdata/fake_pi.mjs");
        assert!(script.is_file(), "缺少 fake_pi 夹具：{}", script.display());
        let node = crate::execenv::resolve("node", &crate::execenv::path_for(None))
            .unwrap_or_else(|| PathBuf::from("node"));
        let mut prefix_args = vec![script.display().to_string()];
        prefix_args.push("--fake-argv-log".to_string());
        prefix_args.push(argv_log.display().to_string());
        if let Some(m) = mode {
            prefix_args.push("--fake-mode".to_string());
            prefix_args.push(m.to_string());
        }
        PiBin { program: node, prefix_args, version: "1.1.0".to_string() }
    }

    impl Env {
        fn new(tag: &str, mode: Option<&str>) -> Self {
            let tmp = tempfile::tempdir().expect("临时目录");
            let assets = tmp.path().join(format!("assets-{tag}"));
            let data = tmp.path().join(format!("data-{tag}"));
            std::fs::create_dir_all(&assets).expect("assets");
            std::fs::create_dir_all(&data).expect("data");
            let _assets = AssetsGuard::new(&assets);
            let _data = ScopedDataDir::new(&data);
            let root = tmp.path().join(format!("proj-{tag}"));
            std::fs::create_dir_all(&root).expect("项目根");
            let argv_log = tmp.path().join("argv.log");
            let sink = Arc::new(Rec::default());
            let mgr = AiManager::for_test(fake_pi(mode, &argv_log), sink.clone());
            Self { _assets, _data, _tmp: tmp, root, argv_log, mgr, sink }
        }

        fn open(&self) -> String {
            let path = self.root.to_str().expect("utf8 路径").to_string();
            self.mgr.open(1, &path, "demo").expect("open").session.session_id
        }

        fn argv_lines(&self) -> Vec<Vec<String>> {
            std::fs::read_to_string(&self.argv_log)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<Vec<String>>(l).ok())
                .collect()
        }

        /// 取会话句柄（任务层拿到的就是这个）。
        fn handle(&self, sid: &str) -> SessionHandle {
            self.mgr.get(sid).expect("会话应存在").handle(&self.mgr.0)
        }

        fn state(&self, sid: &str) -> String {
            self.mgr.snapshot(sid).map(|s| s.session.state).unwrap_or_default()
        }
    }

    fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    const WAIT: Duration = Duration::from_secs(20);

    #[test]
    fn 流式增量拼成一条助手消息() {
        let e = Env::new("stream", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "你好", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e.sink.assistants().iter().any(|(t, _, streaming)| !*streaming && t == "你好世界")),
            "助手正文应拼成「你好世界」，实际：{:?}",
            e.sink.assistants()
        );
        let assistants = e.sink.assistants();
        assert_eq!(assistants.len(), 1, "增量应合并成一条：{assistants:?}");
        assert!(!assistants[0].2, "结束后 streaming 必须为 false");
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "settle 后应回到 idle");
    }

    #[test]
    fn 崩溃后复用同一个会话恢复() {
        let e = Env::new("crash", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "CRASH", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e.argv_lines().len() >= 2),
            "崩溃后应重新 spawn，argv 日志：{:?}",
            e.argv_lines()
        );
        let lines = e.argv_lines();
        let sid_of = |line: &Vec<String>| -> String {
            line.iter()
                .position(|a| a == "--session-id")
                .and_then(|i| line.get(i + 1).cloned())
                .unwrap_or_default()
        };
        assert_eq!(sid_of(&lines[0]), sid, "第一个进程应使用会话 id：{:?}", lines[0]);
        assert_eq!(sid_of(&lines[1]), sid, "恢复必须复用同一个 session id：{:?}", lines[1]);
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(_, t, _)| t.contains("已自动恢复上下文"))),
            "应给出恢复提示：{:?}",
            e.sink.notices()
        );
    }

    #[test]
    fn 连续三次崩溃后置为失败并可重试() {
        let e = Env::new("crash3", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "CRASH", None).expect("第 1 次");
        assert!(wait_until(WAIT, || e.argv_lines().len() == 2), "第 1 次崩溃应恢复");
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "恢复后应 idle");
        e.mgr.send_prompt(&sid, "CRASH", None).expect("第 2 次");
        assert!(wait_until(WAIT, || e.argv_lines().len() == 3), "第 2 次崩溃应恢复");
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "恢复后应 idle");
        e.mgr.send_prompt(&sid, "CRASH", None).expect("第 3 次");
        assert!(
            wait_until(WAIT, || e.state(&sid) == "failed"),
            "第三次崩溃后应 failed，实际 {}",
            e.state(&sid)
        );
        // 条目由 50ms 冲刷线程推送：等它落地再断言，别和外发时序赛跑。
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(level, _, retry)| level == "error" && *retry)),
            "应有一条可重试的错误提示：{:?}",
            e.sink.notices()
        );
    }

    #[test]
    fn 握手超时置为失败并带上_stderr() {
        let e = Env::new("hangstart", Some("hang_start"));
        e.mgr.set_handshake(Duration::from_millis(800));
        let sid = e.open();
        let err = e.mgr.send_prompt(&sid, "你好", None).expect_err("握手失败应返回错误");
        assert!(err.contains("启动失败"), "错误文本：{err}");
        assert!(
            wait_until(Duration::from_secs(25), || e.state(&sid) == "failed"),
            "状态应为 failed，实际 {}",
            e.state(&sid)
        );
        assert!(
            wait_until(WAIT, || e
                .sink
                .notices()
                .iter()
                .any(|(level, text, _)| level == "error" && text.contains("fake-pi"))),
            "通知应带上 stderr 内容：{:?}",
            e.sink.notices()
        );
    }

    #[test]
    fn 休眠后再发消息复用会话重起() {
        let e = Env::new("sleep", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "你好", None).expect("发消息");
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "先跑完一轮");
        e.mgr.sleep(&sid).expect("休眠");
        assert_eq!(e.state(&sid), "sleeping");
        assert!(e.mgr.pid_of(&sid).is_none(), "休眠后进程应已回收");
        e.mgr.send_prompt(&sid, "再说一次", None).expect("重发");
        assert!(wait_until(WAIT, || e.argv_lines().len() >= 2), "应重新 spawn");
        let lines = e.argv_lines();
        let sid_of = |line: &Vec<String>| -> String {
            line.iter()
                .position(|a| a == "--session-id")
                .and_then(|i| line.get(i + 1).cloned())
                .unwrap_or_default()
        };
        assert_eq!(sid_of(&lines[0]), sid);
        assert_eq!(sid_of(&lines[1]), sid, "重起必须复用同一个 session id");
    }

    #[test]
    fn 退出时收掉整个进程组() {
        let e = Env::new("shutdown", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "你好", None).expect("发消息");
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "先跑完一轮");
        let pid = e.mgr.pid_of(&sid).expect("应有进程");
        e.mgr.shutdown_all();
        assert!(
            wait_until(Duration::from_secs(5), || !pid_alive(pid)),
            "进程 {pid} 应在 5s 内消失"
        );
        assert_eq!(e.mgr.live(), 0, "live 计数应归零");
    }

    #[test]
    fn 宿主调用往返() {
        let e = Env::new("host", None);
        e.mgr.set_host_handler(Arc::new(|_h, method: &str, params: &Value| {
            if method == "probe" {
                Ok(json!({ "echo": params.get("x").cloned().unwrap_or(Value::Null) }))
            } else {
                Err(format!("未知方法 {method}"))
            }
        }));
        let sid = e.open();
        e.mgr.send_prompt(&sid, "HOST", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e
                .sink
                .assistants()
                .iter()
                .any(|(t, _, streaming)| !*streaming && t.contains("宿主回声") && t.contains("你好世界"))),
            "宿主回包应进入助手正文：{:?}",
            e.sink.assistants()
        );
    }

    #[test]
    fn 闸门拦下危险命令() {
        let e = Env::new("gate", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "TOOL", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e.sink.tools().iter().any(|(_, _, status, _)| status == "blocked")),
            "危险命令应被标成 blocked：{:?}",
            e.sink.tools()
        );
        let tools = e.sink.tools();
        assert_eq!(tools.len(), 1, "只应有一条工具条目：{tools:?}");
        assert_eq!(tools[0].0, "bash");
        assert_eq!(tools[0].1, "$ rm -rf /");
        assert!(!tools[0].3.trim().is_empty(), "拦截理由不能为空");
    }

    #[test]
    fn 卡片决定往返() {
        let e = Env::new("decide", None);
        let sid = e.open();
        let h = e.handle(&sid);
        let card_id = h
            .push_card(json!({ "id": "svc-1", "kind": "services" }))
            .expect("追加卡片");
        assert_eq!(card_id, "svc-1", "应优先使用卡片自带的 id");

        let (tx, rx) = mpsc::channel();
        let h2 = h.clone();
        let waiter = std::thread::spawn(move || {
            let got = h2.await_decision("svc-1", Duration::from_secs(10));
            let _ = tx.send(got);
        });
        std::thread::sleep(Duration::from_millis(200));
        e.mgr
            .decide(&sid, "svc-1", json!({ "choice": "apply" }))
            .expect("投递决定");
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("等待者应被唤醒")
            .expect("应拿到决定");
        assert_eq!(got, json!({ "choice": "apply" }));
        waiter.join().expect("等待线程应正常结束");
        assert!(h.update_card("svc-1", |c| c["state"] = json!("applied")), "应能就地改卡片");
        let snap = e.mgr.snapshot(&sid).expect("快照");
        let card = snap
            .items
            .iter()
            .find(|i| i.id == "svc-1")
            .expect("卡片应在转录里");
        assert_eq!(card.body, ItemBody::Card { card: json!({ "id": "svc-1", "kind": "services", "state": "applied" }) });
    }

    #[test]
    fn 卡片决定超时() {
        let e = Env::new("decide-timeout", None);
        let sid = e.open();
        let h = e.handle(&sid);
        h.push_card(json!({ "id": "svc-2" })).expect("追加卡片");
        let err = h
            .await_decision("svc-2", Duration::from_millis(150))
            .expect_err("没有决定应超时");
        assert!(err.contains("用户未在时限内确认"), "错误文本：{err}");
        // 超时后该卡片不应再有等待者。
        assert_eq!(
            e.mgr.decide(&sid, "svc-2", json!({ "x": 1 })).unwrap_err(),
            "该卡片已失效"
        );
    }

    #[test]
    fn 关闭会话唤醒等待者() {
        let e = Env::new("decide-close", None);
        let sid = e.open();
        let h = e.handle(&sid);
        h.push_card(json!({ "id": "svc-3" })).expect("追加卡片");

        let (tx, rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let got = h.await_decision("svc-3", Duration::from_secs(30));
            let _ = tx.send(got);
        });
        // 等它真的挂上去，再关会话。
        assert!(
            wait_until(Duration::from_secs(2), || {
                e.mgr.get(&sid).map(|s| !s.decisions.lock().unwrap_or_else(|err| err.into_inner()).is_empty()).unwrap_or(false)
            }),
            "等待者应先注册"
        );
        e.mgr.close(&sid).expect("关闭会话");
        let got = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("关闭会话后等待者必须立刻醒，而不是挂到超时")
            .expect_err("关闭后应返回错误");
        assert_eq!(got, "已取消");
        waiter.join().expect("等待线程应正常结束");
    }

    #[test]
    fn provider_error_surfaces_as_notice() {
        let e = Env::new("provider-error", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "PROVIDER_ERROR", None).expect("发消息");
        // 失败先攒着，结算时才落成提示。
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "settle 后应回到 idle");
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(level, text, retry)| {
                level == "error" && text.contains("模型调用失败") && text.contains("400: boom") && *retry
            })),
            "provider 错误应在结算时变成一条可重试的错误提示：{:?}",
            e.sink.notices()
        );
        // 等冲刷落地再数，别和外发时序赛跑。
        std::thread::sleep(Duration::from_millis(200));
        let errors: Vec<(String, String, bool)> = e
            .sink
            .notices()
            .into_iter()
            .filter(|(level, _, _)| level == "error")
            .collect();
        assert_eq!(errors.len(), 1, "只应有一条错误提示：{errors:?}");
        assert!(errors[0].2, "错误提示应标成可重试：{errors:?}");
        // 错误提示必须是整个转录的最后一条。
        let all = e.sink.all();
        let last = all.last().expect("应有条目");
        assert!(
            matches!(&last.body, ItemBody::Notice { level, .. } if level == "error"),
            "错误提示应是最后一条：{:?}",
            all.iter().map(|i| &i.body).collect::<Vec<_>>()
        );
        // 空气泡必须被替换成占位文案，不能留一条空的助手消息。
        assert!(
            e.sink.assistants().iter().all(|(text, _, _)| !text.trim().is_empty()),
            "不应残留空的助手气泡：{:?}",
            e.sink.assistants()
        );
    }

    #[test]
    fn auth_shaped_provider_error_opens_onboarding_card() {
        let e = Env::new("provider-auth", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "PROVIDER_ERROR_AUTH", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e.sink.all().iter().any(|i| matches!(
                &i.body,
                ItemBody::Card { card }
                    if card.get("kind").and_then(Value::as_str) == Some("onboarding")
                        && card
                            .get("problem")
                            .and_then(Value::as_str)
                            .is_some_and(|p| p.contains("No API key"))
            ))),
            "认证类失败应弹 onboarding 卡：{:?}",
            e.sink.all()
        );
        assert!(
            e.sink.notices().iter().all(|(level, _, _)| level != "error"),
            "有 onboarding 卡时不应再报错误提示：{:?}",
            e.sink.notices()
        );
    }

    #[test]
    fn retry_exhaustion_reports_one_terminal_error() {
        let e = Env::new("retry-error", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "RETRY_ERROR", None).expect("发消息");
        // 等回合收尾，确保所有重试事件都已处理完。
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "settle 后应回到 idle");
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(level, _, _)| level == "error")),
            "重试耗尽后必须有终态错误提示：{:?}",
            e.sink.notices()
        );
        // 等冲刷落地再数，别和外发时序赛跑。
        std::thread::sleep(Duration::from_millis(200));
        let notices = e.sink.notices();
        let warns = notices
            .iter()
            .filter(|(level, text, _)| level == "warn" && text.contains("正在重试"))
            .count();
        assert!(warns >= 3, "每次重试都要有 warn 提示，实际 {warns}：{notices:?}");
        let errors: Vec<(String, String, bool)> = notices
            .into_iter()
            .filter(|(level, _, _)| level == "error")
            .collect();
        assert_eq!(errors.len(), 1, "重试耗尽只能报一条终态错误：{errors:?}");
        assert!(
            errors[0].1.starts_with("重试失败："),
            "终态错误应是重试失败：{errors:?}"
        );
        assert_eq!(errors[0].1, "重试失败：Connection error.");
        // B3 转录：每次失败留下一条占位气泡。
        let placeholders = e
            .sink
            .assistants()
            .iter()
            .filter(|(text, _, _)| text == "（本轮没有返回内容）")
            .count();
        assert_eq!(placeholders, 4, "每次失败都应有占位气泡：{:?}", e.sink.assistants());
        // 最后一条必须是那条错误，不能让用户以为还有第 4 次重试。
        let all = e.sink.all();
        let last = all.last().expect("应有条目");
        assert!(
            matches!(&last.body, ItemBody::Notice { level, text, .. } if level == "error" && text == "重试失败：Connection error."),
            "重试失败必须是最后一条：{:?}",
            all.iter().map(|i| &i.body).collect::<Vec<_>>()
        );
    }

    #[test]
    fn different_error_texts_in_one_turn_report_once() {
        let e = Env::new("mixed-error", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "MIXED_ERROR", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e.state(&sid) == "idle"),
            "settle 后应回到 idle，实际 {}",
            e.state(&sid)
        );
        // 等冲刷落地再数，别和外发时序赛跑。
        std::thread::sleep(Duration::from_millis(200));
        let errors: Vec<(String, String, bool)> = e
            .sink
            .notices()
            .into_iter()
            .filter(|(level, _, _)| level == "error")
            .collect();
        assert_eq!(
            errors.len(),
            1,
            "同一轮两处失败（文本不同）也只能报一条，实际：{errors:?}"
        );
    }

    #[test]
    fn provider_error_without_message_start_still_reported() {
        let e = Env::new("no-start-error", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "PROVIDER_ERROR_NO_START", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(level, text, retry)| {
                level == "error" && text.contains("400: no start") && *retry
            })),
            "没有 message_start 时错误也要报出来：{:?}",
            e.sink.notices()
        );
    }

    #[test]
    fn flag_clears_after_abort_so_next_turn_reports() {
        let e = Env::new("abort-clear", None);
        let sid = e.open();
        // 第一轮：报错但永不 settle，会话停在 busy，只能靠 abort 收尾。
        e.mgr.send_prompt(&sid, "PROVIDER_ERROR_NO_SETTLE", None).expect("第 1 轮");
        // 错误先攒着：此时不该有任何 error 提示。
        assert!(wait_until(WAIT, || e.state(&sid) == "busy"), "第 1 轮应 busy");
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            e.sink.notices().iter().all(|(level, _, _)| level != "error"),
            "abort 之前不该报错：{:?}",
            e.sink.notices()
        );
        e.mgr.abort(&sid).expect("abort");
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "abort 后应 idle");
        assert!(
            wait_until(WAIT, || e
                .sink
                .notices()
                .iter()
                .any(|(level, text, _)| level == "error" && text.contains("400: stalled"))),
            "abort 收尾时应把第 1 轮的失败报出来：{:?}",
            e.sink.notices()
        );
        // 第二轮同样的失败：待报槽必须已清，否则这一轮会被静默吞掉。
        e.mgr.send_prompt(&sid, "PROVIDER_ERROR", None).expect("第 2 轮");
        assert!(
            wait_until(WAIT, || e
                .sink
                .notices()
                .iter()
                .any(|(level, text, _)| level == "error" && text.contains("400: boom"))),
            "abort 之后的下一轮必须重新报错：{:?}",
            e.sink.notices()
        );
        std::thread::sleep(Duration::from_millis(200));
        let errors: Vec<(String, String, bool)> = e
            .sink
            .notices()
            .into_iter()
            .filter(|(level, _, _)| level == "error")
            .collect();
        assert_eq!(errors.len(), 2, "两轮各报一条：{errors:?}");
    }

    #[test]
    fn successful_retry_clears_pending_error() {
        let e = Env::new("retry-ok", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "RETRY_OK", None).expect("发消息");
        assert!(wait_until(WAIT, || e.state(&sid) == "idle"), "settle 后应回到 idle");
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(level, text, _)| level == "info" && text == "重试成功")),
            "重试成功应给一条 info 提示：{:?}",
            e.sink.notices()
        );
        // 等冲刷落地再数。
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            e.sink.notices().iter().all(|(level, _, _)| level != "error"),
            "重试成功就不该有任何错误提示：{:?}",
            e.sink.notices()
        );
    }

    #[test]
    fn flush_on_crash_reports_pending_error() {
        let e = Env::new("error-crash", None);
        let sid = e.open();
        e.mgr.send_prompt(&sid, "PROVIDER_ERROR_THEN_CRASH", None).expect("发消息");
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(level, text, _)| level == "error"
                && text.contains("400: then crash"))),
            "崩溃前攒下的模型失败必须在收尾时报出来：{:?}",
            e.sink.notices()
        );
        assert!(
            wait_until(WAIT, || e.sink.notices().iter().any(|(_, text, _)| text.contains("已自动恢复上下文"))),
            "崩溃后仍应有恢复提示：{:?}",
            e.sink.notices()
        );
        // 模型失败排在崩溃/恢复提示之前：先原因、后结果。
        let notices = e.sink.notices();
        let err_at = notices.iter().position(|(level, text, _)| level == "error" && text.contains("400: then crash"));
        let crash_at = notices.iter().position(|(_, text, _)| text.contains("已自动恢复上下文"));
        assert!(
            err_at.is_some() && crash_at.is_some() && err_at < crash_at,
            "模型失败应排在崩溃提示之前：{notices:?}"
        );
    }

    #[test]
    fn orphan_guard_accepts_titled_pi_and_rejects_others() {
        // pi 会把 process.title 改成 `pi`：只剩 `pi` 也必须认得。
        for cmd in [
            "pi",
            "pi --mode rpc -ne -e /Users/x/.project-ctrl/ai/pc-extension.ts",
            "node /opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js --mode rpc",
            "pi-coding-agent",
        ] {
            assert!(looks_like_our_pi(cmd), "应认为像我们的 pi：{cmd}");
        }
        // 排除项优先：带 `--mode rpc pc-extension` 的本 App 进程也不能认。
        for cmd in [
            "/Applications/project-ctrl.app/Contents/MacOS/project-ctrl",
            "-zsh",
            "zsh",
            "bash",
            "sleep 100",
            "",
            "/Applications/project-ctrl.app/Contents/MacOS/project-ctrl --mode rpc pc-extension",
        ] {
            assert!(!looks_like_our_pi(cmd), "不该认为像我们的 pi：{cmd}");
        }
    }

    #[test]
    fn orphan_lstart_must_match_exactly() {
        assert!(same_lstart("Wed Oct 28 13:31:02 2026", "Wed Oct 28 13:31:02 2026"));
        assert!(same_lstart("  Wed Oct 28 13:31:02 2026\n", "Wed Oct 28 13:31:02 2026"));
        assert!(!same_lstart("Wed Oct 28 13:31:02 2026", "Wed Oct 28 13:31:03 2026"));
        // 空值一律不算匹配：拿不到 lstart 时绝不能杀。
        assert!(!same_lstart("", ""));
        assert!(!same_lstart("Wed Oct 28 13:31:02 2026", ""));
    }

    #[test]
    fn 等待标记写进会话视图() {
        let e = Env::new("awaiting", None);
        let sid = e.open();
        e.mgr.set_awaiting(&sid, true);
        assert!(e.mgr.snapshot(&sid).expect("快照").session.awaiting_user);
        e.mgr.set_awaiting(&sid, false);
        assert!(!e.mgr.snapshot(&sid).expect("快照").session.awaiting_user);
        assert!(
            e.sink
                .sessions
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .iter()
                .any(|v| v.awaiting_user),
            "状态变化应推给前端"
        );
    }
}
