//! AI 任务层：预设任务、结构化卡片、服务配方的「提交 → 应用 → 验证 → 回滚」闭环，
//! 以及卡片动作（批准 / 驳回 / 撤销 / 采纳 / 按诊断修复…）。
//!
//! 线程分工（全部同步，没有 tokio）：
//! | 位置 | 干什么 |
//! |---|---|
//! | `ai-host-*`（pi 的宿主钩子线程） | 读上下文/日志、收提案与诊断、跑校验循环 |
//! | Tauri 命令线程 | `ai_run_task` 发任务、`ai_card_action` 处理卡片 |
//!
//! 关键不变量：
//! - 同一项目同时只允许一个校验循环（[`PROJECT_BUSY`]）；同一会话用 [`LOOPS`] 记住
//!   次数、快照与「上一轮是否跑步骤」，模型重试时据此自动应用。
//! - 校验循环里的互斥量只做短锁交接：绝不在持锁期间等进程、等用户或做磁盘 I/O。
//! - 任何写盘之后失败都必须能回到第一个提案之前的快照：数据库整表替换，
//!   描述文件按字节还原（原本不存在就删掉）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tauri::Manager;

use crate::ai_context;
use crate::ai_gate;
use crate::ai_proto;
use crate::ai_session::{self, BatchItem, HostHandler, SessionHandle};
use crate::ai_settings;
use crate::commands;
use crate::db;
use crate::descriptor;
use crate::detect;
use crate::plan;
use crate::services::{self, Derived, Descriptor, ServiceSpec};
use crate::supervisor;

/// 最多尝试几次。
const MAX_ATTEMPTS: u32 = 3;
/// 提案卡等用户确认的上限。
const PROPOSAL_TIMEOUT: Duration = Duration::from_secs(3600);
/// 启停服务的确认上限。
const CONTROL_TIMEOUT: Duration = Duration::from_secs(600);
/// 没有 `health.timeout_ms` 时的就绪等待。
const DEFAULT_HEALTH: Duration = Duration::from_secs(90);
/// 就绪后再观察多久（要求 Running 且重启次数不变）。
const OBSERVE: Duration = Duration::from_secs(3);
/// 既没端口也没健康检查时，要求连续 Running 的时长。
const STEADY: Duration = Duration::from_secs(5);
/// 轮询步长。
const POLL: Duration = Duration::from_millis(500);
/// 退出时等校验循环自己收尾的上限。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

/// 循环被用户/退出中止时的哨兵错误。
const ABORTED: &str = "用户已停止";

/* ------------------------------------------------------------------------- *
 * 预设任务
 * ------------------------------------------------------------------------- */

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AiTask {
    /// 配置/修正整套服务配方（覆盖语义）。
    Configure,
    /// 诊断某个服务。
    Diagnose { service: String },
    /// 只读体检。
    Health,
    /// 项目画像（简介 / 技术栈 / 标签）。
    Profile,
    /// 补全 README。
    Readme,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Main,
    Light,
}

impl AiTask {
    fn role(&self) -> Role {
        match self {
            Self::Configure | Self::Diagnose { .. } | Self::Readme => Role::Main,
            Self::Health | Self::Profile => Role::Light,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Configure => "配置服务".to_string(),
            Self::Diagnose { service } => format!("诊断 {service}"),
            Self::Health => "体检".to_string(),
            Self::Profile => "画像".to_string(),
            Self::Readme => "补全 README".to_string(),
        }
    }

    /// 完整任务提示词：项目事实 + 该角色的硬性收尾要求。
    fn prompt(&self, ctx: &Value) -> String {
        let ctx_json = serde_json::to_string_pretty(ctx).unwrap_or_else(|_| "{}".to_string());
        let head = format!("<project_context>\n{ctx_json}\n</project_context>");
        match self {
            Self::Configure => format!(
                "你是 project-ctrl 的服务配置助手。目标是给这个项目写出一份「能直接跑起来」的服务配方。\n\n\
{head}\n\n\
要求：\n\
1. 需要时用读取类工具（read / ls / grep）查看清单（package.json、pyproject.toml、Cargo.toml、Makefile…）、脚本与 compose 文件，按真实内容推断启动命令，不要凭猜测。\n\
2. 结束时必须调用 pc_propose_services 提交**完整的服务集合**。这是覆盖语义：集合里没有的现有服务会被删除；只提交改动的那一个，会把其余服务一起删掉。\n\
3. 每个服务要么有 run（长驻命令），要么有 steps（安装/迁移/构建等一次性步骤），也可以两者都有；命令一律写成 argv 数组，路径用项目内相对路径，不要拼 shell 字符串。\n\
4. 能确定监听端口的填 ports.listen；depends_on 只填本集合里出现过的服务名。\n\
5. 不要自己执行安装、构建或启动，也不要去做验证：app 会在用户确认后统一应用并验证。"
            ),
            Self::Diagnose { service } => format!(
                "你是 project-ctrl 的服务诊断助手。请诊断服务 `{service}` 为什么起不来或运行异常。\n\n\
{head}\n\n\
`<project_context>` 里的 `diagnosis` 字段给出了该服务的当前配方、运行状态、预检问题与最近 200 行日志。\n\n\
要求：\n\
1. 先看日志与相关配置/代码，再下结论；证据要给真实文件路径、行号和原文片段，不要空口断言。\n\
2. 结束时必须调用 pc_report_diagnosis。\n\
3. category 选 config 时，config_fix 必须是**完整的**受影响服务集合（覆盖语义：缺的服务会被删除），只提交你真正改过的那部分服务。\n\
4. 不要声称自己改过代码、装过依赖或重启过服务——你只能提交诊断与可选的配置修复。"
            ),
            Self::Health => format!(
                "你是 project-ctrl 的项目体检助手。请对下面的项目做一次只读检查。\n\n\
{head}\n\n\
要求：\n\
1. 只读：不要修改任何文件，也不要启停服务；需要时用读取类工具查看清单、锁文件、env 示例与配置。\n\
2. 结束时必须调用 pc_report_health，且只调用一次：把发现一次性交齐，按 error → warn → info 排序；没有问题就提交一条 info 说明检查过什么。"
            ),
            Self::Profile => format!(
                "你是 project-ctrl 的项目画像助手。请为下面的项目写一句简介、技术栈与标签。\n\n\
{head}\n\n\
要求：\n\
1. 只依据实际读到的清单与目录结构总结，不要编造技术栈。\n\
2. 项目已有手工标签时优先复用（贴合就沿用，不要另造近义词）；确实不合适才新增。\n\
3. 结束时必须调用 pc_propose_profile：summary ≤ 120 字，stack ≤ 12 项，tags ≤ 6 个。"
            ),
            Self::Readme => format!(
                "你是 project-ctrl 的 README 助手。请为下面的项目写一份完整的 README。\n\n\
{head}\n\n\
要求：\n\
1. 沿用现有 README 的语言；没有 README 时用中文。\n\
2. 面向第一次接触项目的人：写清怎么装、怎么跑、有哪些服务与端口、需要哪些环境变量（只写变量名，不要写值）。\n\
3. 命令必须与项目里的真实脚本一致，不要写不存在的 npm script。\n\
4. 结束时必须调用 pc_propose_readme，提交整篇文档（Markdown，≤60000 字）。"
            ),
        }
    }
}

/* ------------------------------------------------------------------------- *
 * 卡片（`ItemBody::Card` 的 JSON 就是这个形状）
 * ------------------------------------------------------------------------- */

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ServiceCheck {
    pub name: String,
    pub ok: bool,
    pub phase: String,
    pub port: Option<u16>,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Evidence {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    pub excerpt: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HealthItem {
    pub severity: String,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub command: Option<String>,
}

/// 七种卡片。字段顺序即语义顺序（`id` 在最前），`kind` 由 serde 的 tag 生成。
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Card {
    Proposal {
        id: String,
        attempt: u32,
        layer: String,
        before: String,
        after: String,
        env_keys: Vec<String>,
        run_steps_default: bool,
        notes: String,
        state: String,
    },
    Verify {
        id: String,
        attempt: u32,
        state: String,
        services: Vec<ServiceCheck>,
        can_undo: bool,
        can_adopt_last: bool,
    },
    Diagnosis {
        id: String,
        service: String,
        summary: String,
        cause: String,
        category: String,
        evidence: Vec<Evidence>,
        suggestion: String,
        config_fix: Option<Vec<ServiceSpec>>,
        rounds: u32,
        state: String,
    },
    Health {
        id: String,
        summary: String,
        items: Vec<HealthItem>,
    },
    Profile {
        id: String,
        summary: String,
        stack: Vec<String>,
        tags: Vec<String>,
        state: String,
    },
    Readme {
        id: String,
        content: String,
        state: String,
    },
    ServiceControl {
        id: String,
        service: String,
        action: String,
        reason: String,
        state: String,
        result: Option<String>,
    },
}

fn new_card_id(prefix: &str) -> String {
    let n = CARD_SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}-{n}")
}

static CARD_SEQ: AtomicU64 = AtomicU64::new(1);

/// 把卡片推进会话转录（`push_card` 用 `id` 字段作为条目 id）。
fn push_card(handle: &SessionHandle, card: &Card) -> Result<String, String> {
    let v = serde_json::to_value(card).map_err(|e| format!("卡片序列化失败：{e}"))?;
    handle.push_card(v)
}

/// 严格校验一份「完整集合」：各自合法、名字唯一、依赖都在集合内。
fn validate_set(specs: &[ServiceSpec]) -> Result<(), String> {
    if specs.is_empty() {
        return Err("至少要提交一个服务".to_string());
    }
    let mut problems: Vec<String> = Vec::new();
    let mut names: HashSet<&str> = HashSet::new();
    for s in specs {
        problems.extend(s.validate());
        if !names.insert(s.name.as_str()) {
            problems.push(format!("服务名重复：{}", s.name));
        }
    }
    for s in specs {
        for d in &s.depends_on {
            if !names.contains(d.as_str()) {
                problems.push(format!(
                    "{}：depends_on 指向本次提交里不存在的服务 {}",
                    s.name, d
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("；"))
    }
}

/// 就地把若干字段合并进一张卡片（找不到卡片时静默失败）。
fn patch_card(handle: &SessionHandle, card_id: &str, patch: Value) {
    handle.update_card(card_id, |card| {
        if let (Some(dst), Some(src)) = (card.as_object_mut(), patch.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
    });
}

/* ------------------------------------------------------------------------- *
 * 卡片动作
 * ------------------------------------------------------------------------- */

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CardAction {
    Approve { run_steps: bool },
    Reject { feedback: Option<String> },
    Undo,
    StopServices,
    AdoptLast,
    ApplyFix { run_steps: bool },
    Reverify,
    ContinueDiagnose,
    AdoptProfile { summary: bool, stack: bool, tags: Vec<String> },
    Dismiss,
    Allow,
    Deny,
}

/* ------------------------------------------------------------------------- *
 * 校验循环的全局状态
 * ------------------------------------------------------------------------- */

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layer {
    Db,
    Descriptor,
}

impl Layer {
    fn as_str(self) -> &'static str {
        match self {
            Self::Db => "db",
            Self::Descriptor => "descriptor",
        }
    }
}

/// 第一个提案之前的真相。
#[derive(Clone, Debug)]
enum Snapshot {
    /// 数据库层的整张服务表（config 已解析）。
    Db(Vec<(ServiceSpec, Derived)>),
    /// 描述文件的原始字节；`None` 表示当时文件不存在。
    Descriptor(Option<String>),
}

struct VerifyLoop {
    project_id: i64,
    layer: Layer,
    /// 已经用掉的尝试次数（0 只在测试里出现）。
    attempt: u32,
    snapshot: Snapshot,
    started: Vec<String>,
    /// 失败一次后，下一轮自动应用（不再弹提案卡）。
    auto_approve: bool,
    /// 上一轮用户对「跑步骤」的选择。
    run_steps: bool,
    last: Vec<ServiceSpec>,
    abort: Arc<AtomicBool>,
    /// 是否还占着 [`PROJECT_BUSY`]（通过之后释放，但循环留着给撤销用）。
    holds_busy: bool,
}

impl VerifyLoop {
    fn fresh(project_id: i64) -> Self {
        Self {
            project_id,
            layer: Layer::Db,
            attempt: 0,
            snapshot: Snapshot::Descriptor(None),
            started: Vec::new(),
            auto_approve: false,
            run_steps: false,
            last: Vec::new(),
            abort: Arc::new(AtomicBool::new(false)),
            holds_busy: false,
        }
    }
}

/// 每次提交前递增尝试次数；用尽即拒绝（第 4 次）。
fn begin_attempt(lp: &mut VerifyLoop) -> Result<u32, String> {
    if lp.attempt >= MAX_ATTEMPTS {
        return Err("验证次数已用尽".to_string());
    }
    lp.attempt += 1;
    Ok(lp.attempt)
}

/// 会话 → 进行中的校验循环。
static LOOPS: LazyLock<Mutex<HashMap<String, Arc<Mutex<VerifyLoop>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 项目 → 是否已有校验循环（同一项目同一时刻只允许一个）。
static PROJECT_BUSY: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 诊断卡片的隐藏数据（卡片 JSON 里不放完整修复集，避免前端反复渲染）。
struct DiagnosisRec {
    service: String,
    config_fix: Option<Vec<ServiceSpec>>,
    rounds: u32,
}

static DIAGNOSES: LazyLock<Mutex<HashMap<String, DiagnosisRec>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct ProfileRec {
    summary: String,
    stack: Vec<String>,
}

static PROFILES: LazyLock<Mutex<HashMap<String, ProfileRec>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static READMES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn rec_key(session_id: &str, card_id: &str) -> String {
    format!("{session_id}\u{1f}{card_id}")
}

fn lock_loops() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Mutex<VerifyLoop>>>> {
    LOOPS.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_busy() -> std::sync::MutexGuard<'static, HashSet<i64>> {
    PROJECT_BUSY.lock().unwrap_or_else(|e| e.into_inner())
}

fn get_loop(session_id: &str) -> Option<Arc<Mutex<VerifyLoop>>> {
    lock_loops().get(session_id).cloned()
}

fn with_loop<T>(lp: &Arc<Mutex<VerifyLoop>>, f: impl FnOnce(&mut VerifyLoop) -> T) -> T {
    let mut g = lp.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

fn abort_requested(lp: &Arc<Mutex<VerifyLoop>>) -> bool {
    with_loop(lp, |l| l.abort.load(Ordering::SeqCst))
}

fn check_abort(lp: &Arc<Mutex<VerifyLoop>>) -> Result<(), String> {
    if abort_requested(lp) {
        Err(ABORTED.to_string())
    } else {
        Ok(())
    }
}

fn push_started(lp: &Arc<Mutex<VerifyLoop>>, name: &str) {
    with_loop(lp, |l| {
        if !l.started.iter().any(|n| n == name) {
            l.started.push(name.to_string());
        }
    });
}

fn is_started(lp: &Arc<Mutex<VerifyLoop>>, name: &str) -> bool {
    with_loop(lp, |l| l.started.iter().any(|n| n == name))
}

fn take_started(lp: &Arc<Mutex<VerifyLoop>>) -> Vec<String> {
    with_loop(lp, |l| std::mem::take(&mut l.started))
}

fn release_busy(lp: &Arc<Mutex<VerifyLoop>>) {
    let pid = with_loop(lp, |l| {
        if !l.holds_busy {
            return None;
        }
        l.holds_busy = false;
        Some(l.project_id)
    });
    if let Some(pid) = pid {
        lock_busy().remove(&pid);
    }
}

/// 测试专用：忘掉某会话的校验循环（`LOOPS` / `PROJECT_BUSY` 是进程级全局量，
/// 用例之间共享；不清掉的话同 id 的下一个用例会撞「该项目已有一个校验在进行中」）。
#[cfg(test)]
pub(crate) fn forget_loop_for_test(session_id: &str) {
    if let Some(lp) = get_loop(session_id) {
        drop_loop(session_id, &lp);
    }
}

fn drop_loop(session_id: &str, lp: &Arc<Mutex<VerifyLoop>>) {
    {
        let mut m = lock_loops();
        if m.get(session_id).map(|x| Arc::ptr_eq(x, lp)).unwrap_or(false) {
            m.remove(session_id);
        }
    }
    release_busy(lp);
}

/* ------------------------------------------------------------------------- *
 * 层 / 快照 / 写盘 / 回滚
 * ------------------------------------------------------------------------- */

/// 当前生效的那一层：有 `.project-ctrl.json` 就只认文件，否则只认数据库。
fn effective_specs(project_id: i64) -> Result<(PathBuf, Vec<ServiceSpec>), String> {
    let base = commands::existing_project_path(project_id)?;
    if descriptor::exists(&base) {
        let d = descriptor::read(&base)?.ok_or_else(|| "描述文件读取失败".to_string())?;
        Ok((base, d.services))
    } else {
        let conn = db::open_db().map_err(|e| e.to_string())?;
        let rows = db::services_of(&conn, project_id).map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(ServiceSpec::from_json(&row.config)?);
        }
        Ok((base, out))
    }
}

fn capture_snapshot(base: &Path, layer: Layer, project_id: i64) -> Result<Snapshot, String> {
    match layer {
        Layer::Descriptor => Ok(Snapshot::Descriptor(
            std::fs::read_to_string(descriptor::path_of(base)).ok(),
        )),
        Layer::Db => {
            let conn = db::open_db().map_err(|e| e.to_string())?;
            let rows = db::services_of(&conn, project_id).map_err(|e| e.to_string())?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push((ServiceSpec::from_json(&row.config)?, Derived::from_json(&row.derived)));
            }
            Ok(Snapshot::Db(out))
        }
    }
}

/// 把整套配方写到生效层：数据库层整表替换，描述文件层整份覆盖。
fn apply_specs(
    project_id: i64,
    base: &Path,
    specs: &[ServiceSpec],
    layer: Layer,
) -> Result<(), String> {
    match layer {
        Layer::Descriptor => descriptor::write(base, &Descriptor::new(specs.to_vec()), true),
        Layer::Db => {
            // 与 save_service 一致：派生信息由探测得出，而不是让模型猜。
            let detected = detect::detect(base);
            let mut derived = Derived::manual();
            derived.toolchain = detected.toolchain;
            derived.node_pin = detected.node_pin;
            let rows: Vec<(ServiceSpec, Derived)> =
                specs.iter().map(|s| (s.clone(), derived.clone())).collect();
            let conn = db::open_db().map_err(|e| e.to_string())?;
            db::replace_services(&conn, project_id, &rows).map_err(|e| e.to_string())
        }
    }
}

fn rollback(base: &Path, project_id: i64, snap: &Snapshot) -> Result<(), String> {
    match snap {
        Snapshot::Db(rows) => {
            let conn = db::open_db().map_err(|e| e.to_string())?;
            db::replace_services(&conn, project_id, rows).map_err(|e| e.to_string())
        }
        Snapshot::Descriptor(text) => {
            let path = descriptor::path_of(base);
            match text {
                Some(bytes) => crate::localstate::atomic_write(&path, bytes.as_bytes()),
                None => match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(format!("回滚描述文件失败：{e}")),
                },
            }
        }
    }
}

/* ------------------------------------------------------------------------- *
 * 校验本身
 * ------------------------------------------------------------------------- */

struct Proposal {
    card: Value,
    run_steps_default: bool,
}

/// 组装提案卡（纯函数的一半：不碰会话、不等用户）。
fn build_proposal(
    base: &Path,
    layer: Layer,
    before_specs: &[ServiceSpec],
    specs: &[ServiceSpec],
    attempt: u32,
    notes: &str,
) -> Result<Proposal, String> {
    let before = match layer {
        Layer::Descriptor => {
            let path = descriptor::path_of(base);
            match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
                Err(e) => return Err(format!("读取描述文件失败：{e}")),
            }
        }
        Layer::Db => services::to_descriptor_json(&Descriptor::new(before_specs.to_vec()))?,
    };
    let after = services::to_descriptor_json(&Descriptor::new(specs.to_vec()))?;
    let env_keys: Vec<String> = specs
        .iter()
        .flat_map(|s| s.env.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let run_steps_default = specs.iter().any(|s| !s.steps.is_empty()) && missing_deps(base);
    let card = Card::Proposal {
        id: new_card_id("prop"),
        attempt,
        layer: layer.as_str().to_string(),
        before,
        after,
        env_keys,
        run_steps_default,
        notes: notes.to_string(),
        state: "pending".to_string(),
    };
    Ok(Proposal {
        card: serde_json::to_value(&card).map_err(|e| format!("卡片序列化失败：{e}"))?,
        run_steps_default,
    })
}

/// 依赖目录缺了：这类项目第一次跑步骤几乎是必须的。
fn missing_deps(base: &Path) -> bool {
    let missing = |manifest: &str, dir: &str| base.join(manifest).is_file() && !base.join(dir).is_dir();
    missing("package.json", "node_modules")
        || missing("pyproject.toml", ".venv")
        || missing("composer.json", "vendor")
}

struct Outcome {
    ok: bool,
    checks: Vec<ServiceCheck>,
    failures: Vec<Value>,
}

/// 步骤 ⑥–⑩：停旧 → 写盘 → 跑步骤 → 启动 → 健康检查。
///
/// 返回 `Err(ABORTED)` 表示用户/退出要求中止；其它 `Err` 是写盘或解析失败。
fn execute_plan<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lp: &Arc<Mutex<VerifyLoop>>,
    specs: &[ServiceSpec],
    run_steps: bool,
) -> Result<Outcome, String> {
    let (project_id, layer) = with_loop(lp, |l| (l.project_id, l.layer));
    let base = commands::existing_project_path(project_id)?;

    // ⑥ 停掉「旧集合 ∪ 新集合」里所有活跃服务（停一个没在跑的也无害）。
    let old = effective_specs(project_id).map(|(_, v)| v).unwrap_or_default();
    let mut names: HashSet<String> = old.iter().map(|s| s.name.clone()).collect();
    names.extend(specs.iter().map(|s| s.name.clone()));
    for name in &names {
        let _ = commands::stop_service(app, project_id, name);
    }
    check_abort(lp)?;

    // ⑦ 写盘。
    apply_specs(project_id, &base, specs, layer)?;
    check_abort(lp)?;

    let (_, items, _) = commands::resolve_all_or_err(project_id)?;

    // ⑧ 一次性步骤：按声明顺序，逐步等它离开 Step 阶段。
    if run_steps {
        if let Some(fail) = run_all_steps(app, lp, &items)? {
            let checks = checks_from_status(app, &items);
            return Ok(Outcome { ok: false, checks, failures: vec![fail] });
        }
    }
    check_abort(lp)?;

    // ⑨ 启动（依赖在前）。
    let mut failures: Vec<Value> = Vec::new();
    let mut start_failed: HashSet<String> = HashSet::new();
    for name in start_order(&items)? {
        check_abort(lp)?;
        match commands::start_service(app, project_id, &name) {
            Ok(_) => push_started(lp, &name),
            Err(e) => {
                start_failed.insert(name.clone());
                failures.push(json!({
                "name": name,
                "phase": "failed",
                "exit_code": Value::Null,
                "note": e,
                    "preflight": Vec::<String>::new(),
                    "logs": last_lines(app, project_id, &name, 150),
                }));
            }
        }
    }

    // ⑩ 健康检查：就绪后再观察 3s，要求仍然 Running 且没有偷偷重启。
    let mut checks: Vec<ServiceCheck> = Vec::new();
    for item in &items {
        check_abort(lp)?;
        let startable = item.spec.is_longrun() || item.spec.docker.is_some();
        let check = if !startable {
            ServiceCheck {
                name: item.spec.name.clone(),
                ok: true,
                phase: "stopped".to_string(),
                port: None,
                message: "仅一次性步骤".to_string(),
            }
        } else if !is_started(lp, &item.spec.name) {
            let status = supervisor::state(app)
                .status_of(&supervisor::key_of(project_id, &item.spec.name));
            ServiceCheck {
                name: item.spec.name.clone(),
                ok: false,
                phase: status.as_ref().map(|s| phase_str(s.phase)).unwrap_or("idle").to_string(),
                port: status.as_ref().and_then(|s| s.port),
                message: "启动失败".to_string(),
            }
        } else {
            check_one(app, item)
        };
        // 启动阶段已经记过一条的，不要重复记。
        if !check.ok && !start_failed.contains(&item.spec.name) {
            failures.push(failure_json(app, project_id, item, &check));
        }
        checks.push(check);
    }

    Ok(Outcome { ok: failures.is_empty(), checks, failures })
}

/// 启动顺序：每个可启动服务的依赖闭包在前，服务自身在后。
fn start_order(items: &[plan::Resolved]) -> Result<Vec<String>, String> {
    let startable = |r: &plan::Resolved| r.spec.is_longrun() || r.spec.docker.is_some();
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in items.iter().filter(|r| startable(r)) {
        for dep in plan::dependency_chain(items, &r.spec.name)? {
            if startable(dep) && seen.insert(dep.spec.name.clone()) {
                order.push(dep.spec.name.clone());
            }
        }
        if seen.insert(r.spec.name.clone()) {
            order.push(r.spec.name.clone());
        }
    }
    Ok(order)
}

/// 依次跑所有一次性的步骤；失败时返回一条失败记录（不再继续后面的步骤）。
fn run_all_steps<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lp: &Arc<Mutex<VerifyLoop>>,
    items: &[plan::Resolved],
) -> Result<Option<Value>, String> {
    for r in items {
        for step in &r.spec.steps {
            check_abort(lp)?;
            let name = &r.spec.name;
            if let Err(e) = commands::run_step(app, r.project_id, name, &step.name) {
                return Ok(Some(json!({
                    "name": name,
                    "phase": "failed",
                    "exit_code": Value::Null,
                    "note": format!("步骤 {} 启动失败：{e}", step.name),
                    "preflight": commands::preflight_problems(r, &step.argv),
                    "logs": last_lines(app, r.project_id, name, 150),
                })));
            }
            let key = supervisor::key_of(r.project_id, name);
            let deadline = Instant::now() + Duration::from_secs(step.timeout_s.max(1) + 5);
            loop {
                check_abort(lp)?;
                let phase = supervisor::state(app).status_of(&key).map(|s| s.phase);
                match phase {
                    Some(supervisor::Phase::Step) => {
                        if Instant::now() >= deadline {
                            return Ok(Some(json!({
                                "name": name,
                                "phase": "failed",
                                "exit_code": Value::Null,
                                "note": format!("步骤 {} 超时未结束", step.name),
                                "preflight": Vec::<String>::new(),
                                "logs": last_lines(app, r.project_id, name, 150),
                            })));
                        }
                        std::thread::sleep(POLL);
                    }
                    Some(supervisor::Phase::Failed) | Some(supervisor::Phase::Crashed) => {
                        return Ok(Some(json!({
                            "name": name,
                            "phase": phase.map(phase_str).unwrap_or("failed"),
                            "exit_code": Value::Null,
                            "note": format!("步骤 {} 失败", step.name),
                            "preflight": Vec::<String>::new(),
                            "logs": last_lines(app, r.project_id, name, 150),
                        })));
                    }
                    _ => break,
                }
            }
        }
    }
    Ok(None)
}

fn phase_str(p: supervisor::Phase) -> &'static str {
    match p {
        supervisor::Phase::Idle => "idle",
        supervisor::Phase::Step => "step",
        supervisor::Phase::Running => "running",
        supervisor::Phase::Stopped => "stopped",
        supervisor::Phase::Crashed => "crashed",
        supervisor::Phase::Failed => "failed",
    }
}

/// 单个服务的就绪判定。docker-only 服务以 `start_service` 成功为准。
fn check_one<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    item: &plan::Resolved,
) -> ServiceCheck {
    let name = item.spec.name.clone();
    let key = supervisor::key_of(item.project_id, &name);
    let sup = supervisor::state(app);
    let status = sup.status_of(&key);
    let port = status
        .as_ref()
        .and_then(|s| s.port)
        .or_else(|| plan::wanted_port(item).ok().flatten());

    if item.spec.docker.is_some() && !item.spec.is_longrun() {
        // docker-only 服务不进 supervisor：`start_service` 已经等过 `docker compose ps`
        // 全部 running，成功即算通过（规格里就是这么定的）。
        return ServiceCheck {
            name,
            ok: true,
            phase: "running".to_string(),
            port,
            message: "容器已启动".to_string(),
        };
    }

    let timeout = item
        .spec
        .health
        .as_ref()
        .and_then(|h| h.timeout_ms)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_HEALTH);

    let mut message = String::new();
    let ready = if item.spec.health.is_some() || port.is_some() {
        wait_ready_watched(&sup, &key, port, item.spec.health.as_ref(), timeout)
    } else {
        steady_running(&sup, &key, STEADY).map_err(|e| e.to_string())
    };
    let mut ok = ready.is_ok();
    if let Err(e) = ready {
        message = e;
    }
    if ok {
        let base_restarts = sup.status_of(&key).map(|s| s.restarts).unwrap_or(0);
        std::thread::sleep(OBSERVE);
        let after = sup.status_of(&key);
        let still_up = matches!(after.as_ref().map(|s| s.phase), Some(supervisor::Phase::Running))
            && after.as_ref().map(|s| s.restarts) == Some(base_restarts);
        ok = still_up;
        if !still_up {
            message = after
                .and_then(|s| s.note)
                .unwrap_or_else(|| "就绪后 3s 内退出或重启".to_string());
        }
    }

    ServiceCheck {
        name,
        ok,
        phase: sup
            .status_of(&key)
            .as_ref()
            .map(|s| phase_str(s.phase))
            .unwrap_or("stopped")
            .to_string(),
        port,
        message,
    }
}

/// 就绪探测：进程先崩了就别再等满 90s，直接把原因报出来。
fn wait_ready_watched(
    sup: &supervisor::Supervisor,
    key: &str,
    port: Option<u16>,
    health: Option<&crate::services::Health>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(st) = sup.status_of(key) {
            if matches!(st.phase, supervisor::Phase::Failed | supervisor::Phase::Crashed) {
                return Err(st.note.unwrap_or_else(|| "进程已退出".to_string()));
            }
        }
        if plan::wait_ready(port, health, Duration::ZERO) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("未在 {}s 内就绪", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 连续 `span` 内一直 Running 才算稳住。
fn steady_running(
    sup: &supervisor::Supervisor,
    key: &str,
    span: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + span;
    loop {
        match sup.status_of(key) {
            Some(st) if st.phase == supervisor::Phase::Running => {
                if Instant::now() >= deadline {
                    return Ok(());
                }
            }
            Some(st) => {
                return Err(st.note.unwrap_or_else(|| format!("状态为 {}", phase_str(st.phase))));
            }
            None => return Err("没有运行记录".to_string()),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn checks_from_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    items: &[plan::Resolved],
) -> Vec<ServiceCheck> {
    items
        .iter()
        .map(|r| {
            let status = supervisor::state(app)
                .status_of(&supervisor::key_of(r.project_id, &r.spec.name));
            ServiceCheck {
                name: r.spec.name.clone(),
                ok: false,
                phase: status.as_ref().map(|s| phase_str(s.phase)).unwrap_or("idle").to_string(),
                port: status.as_ref().and_then(|s| s.port),
                message: "步骤失败，未继续".to_string(),
            }
        })
        .collect()
}

fn failure_json<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: i64,
    item: &plan::Resolved,
    check: &ServiceCheck,
) -> Value {
    let status = supervisor::state(app)
        .status_of(&supervisor::key_of(project_id, &item.spec.name));
    json!({
        "name": item.spec.name,
        "phase": check.phase,
        "exit_code": status.as_ref().and_then(|s| s.exit_code),
        "note": status.as_ref().and_then(|s| s.note.clone()),
        "preflight": commands::preflight_problems(item, &item.spec.run_argv()),
        "logs": last_lines(app, project_id, &item.spec.name, 150),
    })
}

fn last_lines<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: i64,
    name: &str,
    n: usize,
) -> Vec<String> {
    let key = supervisor::key_of(project_id, name);
    let mut lines = supervisor::state(app).snapshot_logs(&key).lines;
    if lines.len() > n {
        lines = lines.split_off(lines.len() - n);
    }
    lines
}

/* ------------------------------------------------------------------------- *
 * 校验循环的入口
 * ------------------------------------------------------------------------- */

fn insert_loop(
    session_id: &str,
    project_id: i64,
    run_steps: bool,
) -> Result<Arc<Mutex<VerifyLoop>>, String> {
    let mut loops = lock_loops();
    if loops.contains_key(session_id) {
        return Err("该会话已有进行中的校验".to_string());
    }
    let base = commands::existing_project_path(project_id)?;
    let layer = if descriptor::exists(&base) {
        Layer::Descriptor
    } else {
        Layer::Db
    };
    let snapshot = capture_snapshot(&base, layer, project_id)?;
    {
        let mut busy = lock_busy();
        if !busy.insert(project_id) {
            return Err("该项目已有一个校验在进行中，请等它结束".to_string());
        }
    }
    let mut l = VerifyLoop::fresh(project_id);
    l.layer = layer;
    l.attempt = 1;
    l.run_steps = run_steps;
    l.snapshot = snapshot;
    l.holds_busy = true;
    let lp = Arc::new(Mutex::new(l));
    loops.insert(session_id.to_string(), Arc::clone(&lp));
    Ok(lp)
}

/// 取（或新建）本会话的校验循环。返回 `(循环, 是否首次)`。
fn begin_loop(session_id: &str, project_id: i64) -> Result<(Arc<Mutex<VerifyLoop>>, bool), String> {
    if let Some(lp) = get_loop(session_id) {
        with_loop(&lp, begin_attempt)?;
        return Ok((lp, false));
    }
    Ok((insert_loop(session_id, project_id, false)?, true))
}

/// 中止：停掉已启动的服务、按快照回滚、卡片标 aborted、释放项目锁，最后返回哨兵错误。
fn abort_loop<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    handle: &SessionHandle,
    lp: &Arc<Mutex<VerifyLoop>>,
    card_id: Option<&str>,
) -> Result<Value, String> {
    let _ = stop_and_rollback(app, lp);
    if let Some(id) = card_id {
        patch_card(handle, id, json!({"state": "aborted", "can_undo": false}));
    }
    drop_loop(&handle.session_id, lp);
    Err(ABORTED.to_string())
}

fn stop_and_rollback<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lp: &Arc<Mutex<VerifyLoop>>,
) -> Result<(), String> {
    let (project_id, snapshot) = with_loop(lp, |l| (l.project_id, l.snapshot.clone()));
    for name in take_started(lp) {
        let _ = commands::stop_service(app, project_id, &name);
    }
    let base = commands::existing_project_path(project_id)?;
    rollback(&base, project_id, &snapshot)
}

/// `pc_propose_services` 的实现体：步骤 ①–⑭。
fn propose_services<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    handle: &SessionHandle,
    params: &Value,
) -> Result<Value, String> {
    let session_id = handle.session_id.as_str();
    let project_id = handle.project_id;

    // ① 严格校验（不消耗尝试次数）。
    let args: ProposeArgs = parse(params)?;
    validate_set(&args.services)?;

    // ② 次数上限 + 项目锁 + 快照（首次提交时抓）。
    let (lp, first) = begin_loop(session_id, project_id)?;

    let prepared = (|| -> Result<(Proposal, u32, bool, bool), String> {
        let (base, before_specs) = effective_specs(project_id)?;
        let (attempt, layer, auto, prev_steps) =
            with_loop(&lp, |l| (l.attempt, l.layer, l.auto_approve, l.run_steps));
        let plan = build_proposal(&base, layer, &before_specs, &args.services, attempt, &args.notes)?;
        Ok((plan, attempt, auto, prev_steps))
    })();
    let (proposal, attempt, auto_approve, prev_steps) = match prepared {
        Ok(v) => v,
        Err(e) => {
            if first {
                drop_loop(session_id, &lp);
            }
            return Err(e);
        }
    };

    // ③④ 提案卡：首次弹窗等确认，之后（失败重试）自动应用。
    let card_id = match handle.push_card(proposal.card.clone()) {
        Ok(id) => id,
        Err(e) => {
            drop_loop(session_id, &lp);
            return Err(e);
        }
    };
    let mut run_steps = prev_steps;
    if auto_approve {
        patch_card(handle, &card_id, json!({"state": "auto"}));
    } else {
        let mgr = manager(app)?;
        mgr.set_awaiting(session_id, true);
        let decision = handle.await_decision(&card_id, PROPOSAL_TIMEOUT);
        mgr.set_awaiting(session_id, false);
        match decision {
            Ok(v) => {
                if v.get("action").and_then(Value::as_str) == Some("allow") {
                    run_steps = v
                        .get("run_steps")
                        .and_then(Value::as_bool)
                        .unwrap_or(proposal.run_steps_default);
                    patch_card(handle, &card_id, json!({"state": "approved"}));
                } else {
                    let feedback = v.get("feedback").cloned().unwrap_or(Value::Null);
                    patch_card(handle, &card_id, json!({"state": "rejected"}));
                    drop_loop(session_id, &lp);
                    return Ok(json!({ "approved": false, "feedback": feedback }));
                }
            }
            Err(e) => {
                patch_card(handle, &card_id, json!({"state": "expired"}));
                drop_loop(session_id, &lp);
                return Err(e);
            }
        }
    }
    with_loop(&lp, |l| {
        l.last = args.services.clone();
        l.run_steps = run_steps;
    });

    // 验证卡先以 running 落地，前端能立刻显示进度。
    let verify_id = new_card_id("verify");
    let verify_card = Card::Verify {
        id: verify_id.clone(),
        attempt,
        state: "running".to_string(),
        services: Vec::new(),
        can_undo: false,
        can_adopt_last: false,
    };
    if let Err(e) = push_card(handle, &verify_card) {
        drop_loop(session_id, &lp);
        return Err(e);
    }

    // ⑤–⑩ 应用 + 步骤 + 启动 + 健康检查。
    let outcome = match execute_plan(app, &lp, &args.services, run_steps) {
        Ok(o) => o,
        Err(e) if e == ABORTED => return abort_loop(app, handle, &lp, Some(&verify_id)),
        Err(e) => {
            let _ = stop_and_rollback(app, &lp);
            patch_card(handle, &verify_id, json!({"state": "aborted", "can_undo": false}));
            drop_loop(session_id, &lp);
            return Err(e);
        }
    };
    if abort_requested(&lp) {
        return abort_loop(app, handle, &lp, Some(&verify_id));
    }

    if outcome.ok {
        // ⑪ 通过：保留快照，撤销仍可用；项目锁释放，别的会话可以开始新的校验。
        patch_card(
            handle,
            &verify_id,
            json!({"state": "passed", "services": outcome.checks, "can_undo": true}),
        );
        release_busy(&lp);
        return Ok(json!({
            "verified": true,
            "services": outcome.checks,
            "instruction": "验证通过，请用两三句话总结配置后结束",
        }));
    }

    if attempt < MAX_ATTEMPTS {
        // ⑫ 失败但还有次数：停掉本轮启动的东西，下一轮自动应用。
        for name in take_started(&lp) {
            let _ = commands::stop_service(app, project_id, &name);
        }
        with_loop(&lp, |l| l.auto_approve = true);
        patch_card(
            handle,
            &verify_id,
            json!({"state": "failed", "services": outcome.checks, "can_undo": true}),
        );
        return Ok(json!({
            "verified": false,
            "attempt": attempt,
            "remaining": MAX_ATTEMPTS - attempt,
            "failures": outcome.failures,
            "instruction": "请分析原因并再次调用 pc_propose_services 提交修正后的完整配方；修正会自动应用",
        }));
    }

    // ⑬ 次数用尽：停掉本轮启动的、按快照回滚，保留 last 供「采纳」。
    for name in take_started(&lp) {
        let _ = commands::stop_service(app, project_id, &name);
    }
    let snapshot = with_loop(&lp, |l| l.snapshot.clone());
    if let Ok(base) = commands::existing_project_path(project_id) {
        let _ = rollback(&base, project_id, &snapshot);
    }
    patch_card(
        handle,
        &verify_id,
        json!({"state": "rolled_back", "services": outcome.checks, "can_adopt_last": true}),
    );
    release_busy(&lp);
    Ok(json!({
        "verified": false,
        "exhausted": true,
        "instruction": "不要再提交，请总结尝试过的方案与最可能的原因",
    }))
}

/* ------------------------------------------------------------------------- *
 * 其余宿主方法
 * ------------------------------------------------------------------------- */

fn parse<T: for<'de> Deserialize<'de>>(v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("参数不合法：{e}"))
}

#[derive(Deserialize)]
struct ProposeArgs {
    services: Vec<ServiceSpec>,
    #[serde(default)]
    notes: String,
}

#[derive(Deserialize)]
struct DiagnosisArgs {
    service: String,
    summary: String,
    cause: String,
    category: String,
    #[serde(default)]
    evidence: Vec<Evidence>,
    #[serde(default)]
    suggestion: String,
    #[serde(default)]
    config_fix: Option<Vec<ServiceSpec>>,
}

#[derive(Deserialize)]
struct HealthArgs {
    summary: String,
    #[serde(default)]
    items: Vec<HealthItem>,
}

#[derive(Deserialize)]
struct ProfileArgs {
    summary: String,
    #[serde(default)]
    stack: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct ReadmeArgs {
    content: String,
}

#[derive(Deserialize)]
struct ControlArgs {
    service: String,
    action: String,
    #[serde(default)]
    reason: String,
}

/// §3a：`diagnosis` 的校验比提案更宽松——`config_fix` 只要求「服务名唯一 + 各自合法 +
/// 依赖指向项目里真实存在的服务（含本次提交集合）」。
fn validate_fix(fix: &[ServiceSpec], project_names: &HashSet<String>) -> Result<(), String> {
    if fix.is_empty() {
        return Err("config_fix 不能为空".to_string());
    }
    let mut problems = Vec::new();
    let mut names = HashSet::new();
    for s in fix {
        problems.extend(s.validate());
        if !names.insert(s.name.as_str()) {
            problems.push(format!("config_fix 里服务名重复：{}", s.name));
        }
    }
    for s in fix {
        for d in &s.depends_on {
            if !names.contains(d.as_str()) && !project_names.contains(d) {
                problems.push(format!("{}：depends_on 指向不存在的服务 {}", s.name, d));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("；"))
    }
}

fn report_diagnosis(
    handle: &SessionHandle,
    params: &Value,
) -> Result<Value, String> {
    let args: DiagnosisArgs = parse(params)?;
    if !["config", "code", "env", "external", "unknown"].contains(&args.category.as_str()) {
        return Err(format!("未知的诊断归类：{}", args.category));
    }
    let (_, current) = effective_specs(handle.project_id)?;
    let names: HashSet<String> = current.iter().map(|s| s.name.clone()).collect();
    if !names.contains(&args.service) {
        return Err(format!("该项目没有名为 {} 的服务", args.service));
    }
    if let Some(fix) = &args.config_fix {
        validate_fix(fix, &names)?;
    }
    let card = Card::Diagnosis {
        id: new_card_id("diag"),
        service: args.service.clone(),
        summary: args.summary,
        cause: args.cause,
        category: args.category,
        evidence: args.evidence,
        suggestion: args.suggestion,
        config_fix: args.config_fix.clone(),
        rounds: 0,
        state: "open".to_string(),
    };
    let card_id = push_card(handle, &card)?;
    DIAGNOSES.lock().unwrap_or_else(|e| e.into_inner()).insert(
        rec_key(&handle.session_id, &card_id),
        DiagnosisRec {
            service: args.service,
            config_fix: args.config_fix,
            rounds: 0,
        },
    );
    Ok(json!({ "shown": true }))
}

fn check_text(field: &str, text: &str, cap: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{field} 不能为空"));
    }
    if text.chars().count() > cap {
        return Err(format!("{field} 超过 {cap} 字"));
    }
    Ok(())
}

fn report_health(handle: &SessionHandle, params: &Value) -> Result<Value, String> {
    let args: HealthArgs = parse(params)?;
    check_text("summary", &args.summary, 120)?;
    if args.items.len() > 100 {
        return Err("items 过多（最多 100 条）".to_string());
    }
    for item in &args.items {
        if !["error", "warn", "info"].contains(&item.severity.as_str()) {
            return Err(format!("未知的严重程度：{}", item.severity));
        }
        check_text("title", &item.title, 120)?;
        check_text("detail", &item.detail, 4000)?;
    }
    let card = Card::Health {
        id: new_card_id("health"),
        summary: args.summary,
        items: args.items,
    };
    push_card(handle, &card)?;
    Ok(json!({ "shown": true }))
}

fn report_profile(handle: &SessionHandle, params: &Value) -> Result<Value, String> {
    let args: ProfileArgs = parse(params)?;
    check_text("summary", &args.summary, 120)?;
    if args.stack.len() > 12 {
        return Err("stack 最多 12 项".to_string());
    }
    if args.tags.len() > 6 {
        return Err("tags 最多 6 个".to_string());
    }
    let card = Card::Profile {
        id: new_card_id("profile"),
        summary: args.summary.clone(),
        stack: args.stack.clone(),
        tags: args.tags.clone(),
        state: "pending".to_string(),
    };
    let card_id = push_card(handle, &card)?;
    PROFILES.lock().unwrap_or_else(|e| e.into_inner()).insert(
        rec_key(&handle.session_id, &card_id),
        ProfileRec { summary: args.summary, stack: args.stack },
    );
    Ok(json!({ "shown": true }))
}

fn report_readme(handle: &SessionHandle, params: &Value) -> Result<Value, String> {
    let args: ReadmeArgs = parse(params)?;
    check_text("content", &args.content, 60_000)?;
    let card = Card::Readme {
        id: new_card_id("readme"),
        content: args.content,
        state: "pending".to_string(),
    };
    let card_id = push_card(handle, &card)?;
    READMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(rec_key(&handle.session_id, &card_id));
    Ok(json!({ "shown": true }))
}

/// `pc_service_control`：推卡片 → 等用户确认 → 执行 → 报状态与日志尾巴。
fn service_control<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    handle: &SessionHandle,
    params: &Value,
) -> Result<Value, String> {
    let args: ControlArgs = parse(params)?;
    if !["start", "stop", "restart"].contains(&args.action.as_str()) {
        return Err(format!("不支持的动作：{}", args.action));
    }
    let (_, specs) = effective_specs(handle.project_id)?;
    if !specs.iter().any(|s| s.name == args.service) {
        return Err(format!("该项目没有名为 {} 的服务", args.service));
    }

    let card = Card::ServiceControl {
        id: new_card_id("control"),
        service: args.service.clone(),
        action: args.action.clone(),
        reason: args.reason,
        state: "pending".to_string(),
        result: None,
    };
    let card_id = push_card(handle, &card)?;
    let mgr = manager(app)?;
    mgr.set_awaiting(&handle.session_id, true);
    let decision = handle.await_decision(&card_id, CONTROL_TIMEOUT);
    mgr.set_awaiting(&handle.session_id, false);

    let decision = match decision {
        Ok(v) => v,
        Err(e) => {
            patch_card(handle, &card_id, json!({"state": "expired"}));
            return Err(e);
        }
    };
    if decision.get("action").and_then(Value::as_str) != Some("allow") {
        patch_card(handle, &card_id, json!({"state": "rejected"}));
        return Ok(json!({ "approved": false }));
    }

    let action = args.action.as_str();
    let mut error: Option<String> = None;
    if action == "stop" || action == "restart" {
        if let Err(e) = commands::stop_service(app, handle.project_id, &args.service) {
            error = Some(e);
        }
    }
    if error.is_none() && (action == "start" || action == "restart") {
        match commands::start_service(app, handle.project_id, &args.service) {
            Ok(_) => {
                if let Ok((_, items, _)) = commands::resolve_all_or_err(handle.project_id) {
                    if let Some(item) = items.iter().find(|i| i.spec.name == args.service) {
                        let status = supervisor::state(app)
                            .status_of(&supervisor::key_of(handle.project_id, &args.service));
                        let port = status.as_ref().and_then(|s| s.port);
                        if item.spec.is_longrun() {
                            let timeout = item
                                .spec
                                .health
                                .as_ref()
                                .and_then(|h| h.timeout_ms)
                                .map(Duration::from_millis)
                                .unwrap_or(Duration::from_secs(30));
                            plan::wait_ready(port, item.spec.health.as_ref(), timeout);
                        }
                    }
                }
            }
            Err(e) => error = Some(e),
        }
    }

    let status = supervisor::state(app)
        .status_of(&supervisor::key_of(handle.project_id, &args.service))
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or(Value::Null);
    let tail = last_lines(app, handle.project_id, &args.service, 60).join("\n");
    match &error {
        Some(e) => patch_card(
            handle,
            &card_id,
            json!({"state": "failed", "result": e}),
        ),
        None => patch_card(handle, &card_id, json!({"state": "done"})),
    }
    let mut out = json!({ "approved": true, "status": status, "log_tail": tail });
    if let Some(e) = error {
        out["error"] = json!(e);
    }
    Ok(out)
}

/* ------------------------------------------------------------------------- *
 * 宿主钩子
 * ------------------------------------------------------------------------- */

pub fn host_handler<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> HostHandler {
    Arc::new(move |handle, method, params| dispatch(&app, handle, method, params))
}

pub(crate) fn dispatch<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    handle: &SessionHandle,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    // 方法名逐字对齐 `src-tauri/src/ai_ext.ts` 里的 `hostCall(ctx, "<method>", …)`。
    match method {
        "context" => ai_context::project_context(app, handle.project_id),
        "logs" => {
            let service = params
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| "缺少 service 参数".to_string())?;
            let lines = params
                .get("lines")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(150)
                .clamp(1, 400);
            Ok(json!({
                "service": service,
                "lines": last_lines(app, handle.project_id, service, lines),
            }))
        }
        "propose_services" => propose_services(app, handle, params),
        "diagnosis" => report_diagnosis(handle, params),
        "health" => report_health(handle, params),
        "profile" => report_profile(handle, params),
        "readme" => report_readme(handle, params),
        "service_control" => service_control(app, handle, params),
        "shutdown" => {
            shutdown_session(app, handle);
            Ok(json!({ "ok": true }))
        }
        other => Err(format!("未知的宿主方法：{other}")),
    }
}

/// 退出：让本会话的校验循环尽快回滚（等一小会儿，超时就强制按快照还原）。
fn shutdown_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, handle: &SessionHandle) {
    let session_id = handle.session_id.as_str();
    let Some(lp) = get_loop(session_id) else { return };
    // 通过/已回滚的循环还会留在 LOOPS 里给「撤销 / 采纳」用；那些绝不能回滚。
    if !with_loop(&lp, |l| l.holds_busy) {
        return;
    }
    with_loop(&lp, |l| l.abort.store(true, Ordering::SeqCst));

    let deadline = Instant::now() + SHUTDOWN_WAIT;
    while Instant::now() < deadline {
        if get_loop(session_id).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // 循环还卡在别处（多半在等用户确认）：直接按快照还原，随后移除登记。
    let (project_id, snapshot, started) =
        with_loop(&lp, |l| (l.project_id, l.snapshot.clone(), std::mem::take(&mut l.started)));
    for name in started {
        let _ = commands::stop_service(app, project_id, &name);
    }
    if let Ok(base) = commands::existing_project_path(project_id) {
        let _ = rollback(&base, project_id, &snapshot);
    }
    drop_loop(session_id, &lp);
}

/* ------------------------------------------------------------------------- *
 * 卡片动作
 * ------------------------------------------------------------------------- */

fn manager<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<ai_session::AiManager, String> {
    app.try_state::<ai_session::AiManager>()
        .map(|s| ai_session::AiManager::clone(&s))
        .ok_or_else(|| "AI 会话管理器未初始化".to_string())
}

fn undo<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
    card_id: &str,
) -> Result<(), String> {
    let lp = get_loop(session_id).ok_or_else(|| "没有可撤销的校验".to_string())?;
    let (project_id, snapshot) = with_loop(&lp, |l| (l.project_id, l.snapshot.clone()));
    for name in take_started(&lp) {
        let _ = commands::stop_service(app, project_id, &name);
    }
    let base = commands::existing_project_path(project_id)?;
    rollback(&base, project_id, &snapshot)?;
    if let Ok(handle) = manager(app)?.handle(session_id) {
        patch_card(&handle, card_id, json!({"state": "undone", "can_undo": false}));
    }
    drop_loop(session_id, &lp);
    Ok(())
}

fn stop_services<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
) -> Result<(), String> {
    let lp = get_loop(session_id).ok_or_else(|| "没有进行中的校验".to_string())?;
    let project_id = with_loop(&lp, |l| l.project_id);
    for name in take_started(&lp) {
        let _ = commands::stop_service(app, project_id, &name);
    }
    Ok(())
}

fn adopt_last<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
    card_id: &str,
) -> Result<(), String> {
    let lp = get_loop(session_id).ok_or_else(|| "没有可采纳的配方".to_string())?;
    let (project_id, layer, last) = with_loop(&lp, |l| (l.project_id, l.layer, l.last.clone()));
    if last.is_empty() {
        return Err("没有可采纳的配方".to_string());
    }
    let base = commands::existing_project_path(project_id)?;
    apply_specs(project_id, &base, &last, layer)?;
    if let Ok(handle) = manager(app)?.handle(session_id) {
        patch_card(
            &handle,
            card_id,
            json!({"state": "passed", "can_adopt_last": false, "can_undo": true}),
        );
    }
    Ok(())
}

fn merge_fix(current: &[ServiceSpec], fix: &[ServiceSpec]) -> Vec<ServiceSpec> {
    let mut out: Vec<ServiceSpec> = current.to_vec();
    for spec in fix {
        match out.iter_mut().find(|s| s.name == spec.name) {
            Some(slot) => *slot = spec.clone(),
            None => out.push(spec.clone()),
        }
    }
    out
}

/// 按诊断的 `config_fix` 应用并验证（视为「已批准的第 1 次」）。
fn apply_fix<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
    card_id: &str,
    run_steps: bool,
) -> Result<(), String> {
    let key = rec_key(session_id, card_id);
    let (fix, service_name) = {
        let m = DIAGNOSES.lock().unwrap_or_else(|e| e.into_inner());
        let rec = m.get(&key).ok_or_else(|| "这条诊断已失效".to_string())?;
        let fix = rec.config_fix.clone().ok_or_else(|| "这条诊断没有配置修复".to_string())?;
        (fix, rec.service.clone())
    };
    let handle = manager(app)?.handle(session_id)?;
    let project_id = handle.project_id;

    let (_, current) = effective_specs(project_id)?;
    let merged = merge_fix(&current, &fix);
    validate_set(&merged)?;

    if get_loop(session_id).is_some() {
        return Err("该会话已有进行中的校验".to_string());
    }
    let lp = insert_loop(session_id, project_id, run_steps)?;
    DIAGNOSES.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
    with_loop(&lp, |l| l.last = merged.clone());

    let verify_id = new_card_id("verify");
    let verify_card = Card::Verify {
        id: verify_id.clone(),
        attempt: 1,
        state: "running".to_string(),
        services: Vec::new(),
        can_undo: false,
        can_adopt_last: false,
    };
    if let Err(e) = push_card(&handle, &verify_card) {
        drop_loop(session_id, &lp);
        return Err(e);
    }

    let app2 = app.clone();
    let mgr = manager(app)?;
    let sid = session_id.to_string();
    let vid = verify_id.clone();
    let cid = card_id.to_string();
    let service = service_name;
    let specs = merged;
    let cleanup = Arc::clone(&lp);
    let spawned = std::thread::Builder::new()
        .name(format!("ai-apply-{sid}"))
        .spawn(move || {
            let outcome = execute_plan(&app2, &lp, &specs, run_steps);
            let Ok(h) = mgr.handle(&sid) else {
                // 会话已经关了：把项目锁还回去，别让同项目后续提交被卡死。
                drop_loop(&sid, &lp);
                return;
            };
            match outcome {
                Ok(o) if o.ok => {
                    patch_card(
                        &h,
                        &vid,
                        json!({"state": "passed", "services": o.checks, "can_undo": true}),
                    );
                    patch_card(&h, &cid, json!({"state": "passed"}));
                    release_busy(&lp);
                    let body = json!({"verified": true, "services": o.checks});
                    let text = format!(
                        "[系统] 已按诊断应用配置并验证：{body}\n验证通过，请用两三句话总结这次修复。"
                    );
                    if let Err(e) = mgr.send_prompt(&sid, &text, Some("诊断".to_string())) {
                        eprintln!("应用诊断后发消息失败：{e}");
                    }
                }
                Ok(o) => {
                    with_loop(&lp, |l| l.auto_approve = true);
                    patch_card(
                        &h,
                        &vid,
                        json!({"state": "failed", "services": o.checks, "can_undo": true}),
                    );
                    patch_card(&h, &cid, json!({"state": "failed"}));
                    let body = json!({"verified": false, "services": o.checks, "failures": o.failures});
                    let text = format!(
                        "[系统] 已按诊断应用配置并验证：{body}\n验证失败。若仍需改配置，调用 pc_propose_services 提交修正后的完整配方（会自动应用，剩余次数 2）。"
                    );
                    if let Err(e) = mgr.send_prompt(&sid, &text, Some("诊断".to_string())) {
                        eprintln!("应用诊断后发消息失败：{e}");
                    }
                }
                Err(e) => {
                    let _ = stop_and_rollback(&app2, &lp);
                    patch_card(&h, &vid, json!({"state": "aborted", "can_undo": false}));
                    patch_card(&h, &cid, json!({"state": "failed"}));
                    drop_loop(&sid, &lp);
                    eprintln!("应用诊断失败（已回滚）：{e}（服务 {service}）");
                }
            }
        });
    if let Err(e) = spawned {
        drop_loop(session_id, &cleanup);
        return Err(format!("启动应用线程失败：{e}"));
    }
    Ok(())
}

fn reverify<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
    card_id: &str,
) -> Result<(), String> {
    let key = rec_key(session_id, card_id);
    let service = {
        let m = DIAGNOSES.lock().unwrap_or_else(|e| e.into_inner());
        m.get(&key).map(|r| r.service.clone()).ok_or_else(|| "这条诊断已失效".to_string())?
    };
    let handle = manager(app)?.handle(session_id)?;
    let project_id = handle.project_id;

    let (_, items, _) = commands::resolve_all_or_err(project_id)?;
    let item = items
        .iter()
        .find(|i| i.spec.name == service)
        .ok_or_else(|| format!("该项目没有名为 {service} 的服务"))?
        .clone();
    let _ = commands::stop_service(app, project_id, &service);
    let check = match commands::start_service(app, project_id, &service) {
        Ok(_) => check_one(app, &item),
        Err(e) => ServiceCheck {
            name: service.clone(),
            ok: false,
            phase: "failed".to_string(),
            port: None,
            message: e,
        },
    };

    patch_card(
        &handle,
        card_id,
        json!({"state": if check.ok { "passed" } else { "failed" }}),
    );
    if !check.ok {
        if let Some(rec) = DIAGNOSES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&key)
        {
            rec.rounds += 1;
        }
    }
    Ok(())
}

fn continue_diagnose<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
    card_id: &str,
) -> Result<(), String> {
    let key = rec_key(session_id, card_id);
    let (service, rounds) = {
        let m = DIAGNOSES.lock().unwrap_or_else(|e| e.into_inner());
        let rec = m.get(&key).ok_or_else(|| "这条诊断已失效".to_string())?;
        (rec.service.clone(), rec.rounds)
    };
    if rounds >= 3 {
        return Err("诊断轮次已用尽".to_string());
    }
    let handle = manager(app)?.handle(session_id)?;
    let status = supervisor::state(app)
        .status_of(&supervisor::key_of(handle.project_id, &service))
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or(Value::Null);
    let body = json!({
        "service": service,
        "status": status,
        "logs": last_lines(app, handle.project_id, &service, 150),
    });
    let text = format!(
        "[系统] 重新验证仍失败（第 {rounds} 轮）：{body}\n请重新诊断并调用 pc_report_diagnosis"
    );
    manager(app)?.send_prompt(session_id, &text, Some("诊断".to_string()))
}

fn adopt_profile<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
    card_id: &str,
    action: &CardAction,
) -> Result<(), String> {
    let CardAction::AdoptProfile { summary, stack, tags } = action else {
        return Err("卡片动作不匹配".to_string());
    };
    let key = rec_key(session_id, card_id);
    let rec = PROFILES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key)
        .ok_or_else(|| "这条画像已失效".to_string())?;
    let handle = manager(app)?.handle(session_id)?;
    let project_id = handle.project_id;
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let current = db::all_projects(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| "项目不存在".to_string())?;
    let new_summary = if *summary { Some(rec.summary) } else { current.summary };
    let new_stack = if *stack { rec.stack } else { current.stack };
    db::set_project_profile(&conn, project_id, new_summary.as_deref(), &new_stack)
        .map_err(|e| e.to_string())?;

    // 现有标签（含手工标签）与所选标签取并集。
    let mut merged = db::tags_of(&conn, project_id).map_err(|e| e.to_string())?;
    for t in tags {
        let t = t.trim();
        if !t.is_empty() && !merged.iter().any(|x| x == t) {
            merged.push(t.to_string());
        }
    }
    db::set_project_tags(&conn, project_id, &merged).map_err(|e| e.to_string())?;
    patch_card(&handle, card_id, json!({"state": "adopted"}));
    Ok(())
}

fn dismiss<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session_id: &str,
    card_id: &str,
) -> Result<(), String> {
    let key = rec_key(session_id, card_id);
    let removed = PROFILES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key)
        .is_some()
        || READMES.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
    if removed {
        if let Ok(handle) = manager(app)?.handle(session_id) {
            patch_card(&handle, card_id, json!({"state": "dismissed"}));
        }
    }
    Ok(())
}

#[tauri::command]
pub fn ai_card_action<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
    card_id: String,
    action: CardAction,
) -> Result<(), String> {
    match &action {
        CardAction::Approve { run_steps } => manager(&app)?.decide(
            &session_id,
            &card_id,
            json!({ "action": "allow", "run_steps": run_steps }),
        ),
        CardAction::Reject { feedback } => manager(&app)?.decide(
            &session_id,
            &card_id,
            json!({ "action": "deny", "feedback": feedback }),
        ),
        CardAction::Allow => {
            manager(&app)?.decide(&session_id, &card_id, json!({ "action": "allow" }))
        }
        CardAction::Deny => {
            manager(&app)?.decide(&session_id, &card_id, json!({ "action": "deny" }))
        }
        CardAction::Undo => undo(&app, &session_id, &card_id),
        CardAction::StopServices => stop_services(&app, &session_id),
        CardAction::AdoptLast => adopt_last(&app, &session_id, &card_id),
        CardAction::ApplyFix { run_steps } => apply_fix(&app, &session_id, &card_id, *run_steps),
        CardAction::Reverify => reverify(&app, &session_id, &card_id),
        CardAction::ContinueDiagnose => continue_diagnose(&app, &session_id, &card_id),
        CardAction::AdoptProfile { .. } => adopt_profile(&app, &session_id, &card_id, &action),
        CardAction::Dismiss => dismiss(&app, &session_id, &card_id),
    }
}

/* ------------------------------------------------------------------------- *
 * 任务命令
 * ------------------------------------------------------------------------- */

/// 给某个服务补上「现状」：配方、运行状态、预检问题与最近 200 行日志。
fn diagnosis_context<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: i64,
    service: &str,
) -> Result<Value, String> {
    let (_, items, _) = commands::resolve_all_or_err(project_id)?;
    let r = items
        .iter()
        .find(|i| i.spec.name == service)
        .ok_or_else(|| format!("该项目没有名为 {service} 的服务"))?;
    let status = supervisor::state(app)
        .status_of(&supervisor::key_of(project_id, service))
        .and_then(|s| serde_json::to_value(s).ok());
    Ok(json!({
        "service": service,
        "spec": serde_json::to_value(&r.spec).unwrap_or(Value::Null),
        "status": status,
        "preflight": commands::preflight_problems(r, &r.spec.run_argv()),
        "logs": last_lines(app, project_id, service, 200),
    }))
}

#[tauri::command]
pub fn ai_run_task<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
    task: AiTask,
) -> Result<(), String> {
    let mgr = manager(&app)?;
    let handle = mgr.handle(&session_id)?;
    let mut ctx = ai_context::project_context(&app, handle.project_id)?;
    if let AiTask::Diagnose { service } = &task {
        let detail = diagnosis_context(&app, handle.project_id, service)?;
        if let Some(obj) = ctx.as_object_mut() {
            obj.insert("diagnosis".to_string(), detail);
        }
    }

    // 任务级别决定用主模型还是轻量模型；未设置时交给 pi 的默认值。
    let settings = ai_settings::load();
    let (model, thinking) = match task.role() {
        Role::Main => (settings.main_model, settings.main_thinking),
        Role::Light => (settings.light_model, settings.light_thinking),
    };
    let thinking = if thinking.is_empty() { None } else { Some(thinking) };
    mgr.set_model(&session_id, model, thinking)?;

    let prompt = task.prompt(&ctx);
    mgr.send_prompt(&session_id, &prompt, Some(task.label()))
}

/* ------------------------------------------------------------------------- *
 * 批量画像（并发 2）
 *
 * 一次给多个项目生成「简介 / 技术栈 / 标签」。与单项目任务不同，这里**不建会话**：
 * 每个项目起一个短命的 pi（`--no-session`），只读跑一轮，拿到 `pc_propose_profile`
 * 就收工。两个 worker 共用一个队列，`MAX_LIVE` 满时等 1s 再抢。
 *
 * 事件契约（前端按此渲染，不可偏离）：`ai:batch`，载荷是**完整**的 `Vec<BatchItem>`，
 * 字段 `{project_id, name, state, profile, error}`；`state ∈
 * queued|running|done|failed|adopted|dismissed`；`profile = {summary, stack, tags} | null`。
 * ------------------------------------------------------------------------- */

/// 同时跑几个项目。
const BATCH_WORKERS: usize = 2;
/// 单个项目画像的硬超时。
const PROFILE_TIMEOUT: Duration = Duration::from_secs(300);
/// 收尾宽限：关掉 stdin 后给它这么久自己退出。
const PROFILE_REAP: Duration = Duration::from_secs(3);

struct BatchState {
    /// 按提交顺序排好的条目（也是事件载荷）。
    items: Vec<BatchItem>,
    /// 还没被 worker 领走的项目 id。
    queue: VecDeque<i64>,
    /// 正在跑的项目数（0..=BATCH_WORKERS）。
    running: usize,
    cancel: Arc<AtomicBool>,
    active: bool,
}

impl BatchState {
    fn fresh() -> Self {
        Self {
            items: Vec::new(),
            queue: VecDeque::new(),
            running: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            active: false,
        }
    }
}

static BATCH: LazyLock<Mutex<BatchState>> = LazyLock::new(|| Mutex::new(BatchState::fresh()));

fn lock_batch() -> std::sync::MutexGuard<'static, BatchState> {
    BATCH.lock().unwrap_or_else(|e| e.into_inner())
}

fn batch_snapshot() -> Vec<BatchItem> {
    lock_batch().items.clone()
}

/// 把完整快照推给前端（没有管理器就静默跳过：测试里不装出口）。
fn emit_batch<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let items = batch_snapshot();
    if let Ok(mgr) = manager(app) {
        mgr.emit_batch(&items);
    }
}

/// 组一批：校验选择、按提交顺序建条目、入队。不 spawn 任何东西。
fn prepare_batch(ids: &[i64]) -> Result<Vec<BatchItem>, String> {
    if ids.is_empty() {
        return Err("请先选择项目".to_string());
    }
    // 先查库（不持 BATCH 锁）：路径可能已经被删掉，这类项目直接跳过。
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let all = db::all_projects(&conn).map_err(|e| e.to_string())?;
    let mut items: Vec<BatchItem> = Vec::new();
    for id in ids {
        if items.iter().any(|i| i.project_id == *id) {
            continue;
        }
        if !all.iter().any(|p| p.id == *id) {
            continue;
        }
        let Some(path) = db::project_path(&conn, *id).map_err(|e| e.to_string())? else {
            continue;
        };
        items.push(BatchItem {
            project_id: *id,
            name: crate::logic::project_name_of(Path::new(&path)),
            state: "queued".to_string(),
            profile: None,
            error: None,
        });
    }
    drop(conn);
    if items.is_empty() {
        return Err("请先选择项目".to_string());
    }

    let mut st = lock_batch();
    if st.active {
        return Err("已有批量任务在运行".to_string());
    }
    st.items = items.clone();
    st.queue = items.iter().map(|i| i.project_id).collect();
    st.running = 0;
    st.cancel = Arc::new(AtomicBool::new(false));
    st.active = true;
    Ok(items)
}

fn batch_has_work() -> bool {
    let st = lock_batch();
    !st.cancel.load(Ordering::SeqCst) && !st.queue.is_empty()
}

/// 领一个项目并标 running。返回该条目的快照。
fn batch_claim() -> Option<BatchItem> {
    let mut st = lock_batch();
    if st.cancel.load(Ordering::SeqCst) {
        return None;
    }
    let id = st.queue.pop_front()?;
    let pos = st.items.iter().position(|i| i.project_id == id)?;
    st.items[pos].state = "running".to_string();
    st.running += 1;
    Some(st.items[pos].clone())
}

/// 落一个结果；返回最新快照。队列空了且没人跑时收尾（`active = false`）。
fn batch_finish(project_id: i64, result: Result<Value, String>) -> Vec<BatchItem> {
    let mut st = lock_batch();
    st.running = st.running.saturating_sub(1);
    if let Some(item) = st.items.iter_mut().find(|i| i.project_id == project_id) {
        match result {
            Ok(profile) => {
                item.state = "done".to_string();
                item.profile = Some(profile);
                item.error = None;
            }
            Err(e) => {
                item.state = "failed".to_string();
                item.error = Some(e);
            }
        }
    }
    if st.running == 0 && st.queue.is_empty() {
        st.active = false;
    }
    st.items.clone()
}

/// worker 退出时调用：队列空了且没有在跑的，就收尾。返回是否发生了状态变化。
fn batch_worker_exit() -> bool {
    let mut st = lock_batch();
    if st.running == 0 && st.queue.is_empty() {
        let changed = st.active;
        st.active = false;
        return changed;
    }
    false
}

fn batch_cancel_flag() -> Arc<AtomicBool> {
    Arc::clone(&lock_batch().cancel)
}

/// 取消：置标志、把还在排队的标 failed，其余交给 worker 自己退出。
fn batch_cancel_core() -> Vec<BatchItem> {
    let mut st = lock_batch();
    st.cancel.store(true, Ordering::SeqCst);
    let queued: Vec<i64> = st.queue.drain(..).collect();
    for id in queued {
        if let Some(item) = st.items.iter_mut().find(|i| i.project_id == id) {
            item.state = "failed".to_string();
            item.error = Some("已取消".to_string());
        }
    }
    if st.running == 0 {
        st.active = false;
    }
    st.items.clone()
}

fn batch_adopt_core(
    project_id: i64,
    summary: bool,
    stack: bool,
    tags: &[String],
) -> Result<Vec<BatchItem>, String> {
    let profile = {
        let st = lock_batch();
        let item = st
            .items
            .iter()
            .find(|i| i.project_id == project_id)
            .ok_or_else(|| "批量任务里没有该项目".to_string())?;
        if item.state != "done" {
            return Err("该项目的画像还没生成".to_string());
        }
        item.profile.clone().ok_or_else(|| "该项目的画像为空".to_string())?
    };

    let conn = db::open_db().map_err(|e| e.to_string())?;
    let current = db::all_projects(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| "项目不存在".to_string())?;
    // 没勾的字段保持库里的现值；勾了的取这次画像。
    let new_summary = if summary {
        profile.get("summary").and_then(Value::as_str).map(str::to_string)
    } else {
        current.summary
    };
    let new_stack = if stack {
        profile
            .get("stack")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    } else {
        current.stack
    };
    db::set_project_profile(&conn, project_id, new_summary.as_deref(), &new_stack)
        .map_err(|e| e.to_string())?;

    if !tags.is_empty() {
        let mut merged = db::tags_of(&conn, project_id).map_err(|e| e.to_string())?;
        for t in tags {
            let t = t.trim();
            if !t.is_empty() && !merged.iter().any(|x| x == t) {
                merged.push(t.to_string());
            }
        }
        db::set_project_tags(&conn, project_id, &merged).map_err(|e| e.to_string())?;
    }
    drop(conn);

    let mut st = lock_batch();
    if let Some(item) = st.items.iter_mut().find(|i| i.project_id == project_id) {
        item.state = "adopted".to_string();
    }
    Ok(st.items.clone())
}

fn batch_dismiss_core(project_id: i64) -> Result<Vec<BatchItem>, String> {
    let mut st = lock_batch();
    let item = st
        .items
        .iter_mut()
        .find(|i| i.project_id == project_id)
        .ok_or_else(|| "批量任务里没有该项目".to_string())?;
    item.state = "dismissed".to_string();
    Ok(st.items.clone())
}

/// 画像结果归一：保证事件里的 `profile` 严格是 `{summary, stack, tags}`。
fn normalize_profile(v: &Value) -> Result<Value, String> {
    let summary = v
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "模型没有提交画像".to_string())?;
    let list = |key: &str, cap: usize| -> Vec<String> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .take(cap)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(json!({
        "summary": summary.chars().take(120).collect::<String>(),
        "stack": list("stack", 12),
        "tags": list("tags", 6),
    }))
}

/* ----------------------------- 子进程驱动 ----------------------------- */

/// 一次画像任务的输入。
struct ProfileReq {
    project_id: i64,
    root: PathBuf,
    prompt: String,
    model: Option<ai_settings::ModelRef>,
    thinking: Option<String>,
    cancel: Arc<AtomicBool>,
}

/// 跑一次画像。抽成 trait 是为了测试能换成假驱动，不 spawn 任何进程。
trait ProfileDriver {
    fn drive(&self, req: ProfileReq) -> Result<Value, String>;
}

/// 真实驱动：起一个短命的 pi，自己当 `pc-gate` / `pc-host` 的应答方。
struct PiProfileDriver<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
    bin: ai_settings::PiBin,
    ext: PathBuf,
    system: PathBuf,
}

/// 资源目录与 `ai_session::assets_dir()` 同源（HOME 下的 `.project-ctrl/ai`）。
fn ai_assets_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".project-ctrl")
        .join("ai")
}

fn default_driver<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<PiProfileDriver<R>, String> {
    let dir = ai_assets_dir();
    let ext = dir.join("pc-extension.ts");
    if !ext.is_file() {
        // 启动时写过一次；这里兜底（例如用户清过缓存）。
        let _ = ai_session::install_assets();
    }
    if !ext.is_file() {
        return Err("AI 扩展资源缺失，请重装或重新登录".to_string());
    }
    let bin = ai_settings::resolve_pi(&ai_settings::load())?;
    Ok(PiProfileDriver { app: app.clone(), bin, ext, system: dir.join("system.md") })
}

fn write_frame(stdin: &mut impl Write, frame: &str) -> Result<(), String> {
    stdin.write_all(frame.as_bytes()).map_err(|e| format!("写入 pi 失败：{e}"))?;
    stdin.flush().map_err(|e| format!("写入 pi 失败：{e}"))
}

fn next_req_id(seq: &mut u64) -> String {
    *seq += 1;
    format!("b{seq}")
}

/// 关掉 stdin 后等 3s；还不走就按进程组 SIGKILL（pi 会 fork 工具子进程）。
fn reap_child(child: &mut Child) {
    let deadline = Instant::now() + PROFILE_REAP;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        // SAFETY：只是发信号；`process_group(0)` 保证 pgid == 子进程 pid。
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl<R: tauri::Runtime> ProfileDriver for PiProfileDriver<R> {
    fn drive(&self, req: ProfileReq) -> Result<Value, String> {
        let mut cmd = Command::new(&self.bin.program);
        let mut args: Vec<String> = self.bin.prefix_args.clone();
        args.extend([
            "--mode".to_string(),
            "rpc".to_string(),
            "-ne".to_string(),
            "-e".to_string(),
            self.ext.display().to_string(),
            "-ns".to_string(),
            "-np".to_string(),
            "--no-themes".to_string(),
            "--no-mcp".to_string(),
            "-na".to_string(),
            "--offline".to_string(),
            "--no-session".to_string(),
            "--tools".to_string(),
            "read,grep,find,ls,bash,pc_*".to_string(),
            "--append-system-prompt".to_string(),
            self.system.display().to_string(),
        ]);
        if let Some(m) = &req.model {
            if !m.id.is_empty() {
                args.push("--model".to_string());
                args.push(format!("{}/{}", m.provider, m.id));
            }
        }
        if let Some(t) = &req.thinking {
            if !t.is_empty() {
                args.push("--thinking".to_string());
                args.push(t.clone());
            }
        }
        cmd.args(&args);
        cmd.current_dir(&req.root);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.env_clear();
        let extra = [("PI_TELEMETRY".to_string(), "0".to_string())];
        cmd.envs(crate::execenv::env_for(None, &std::collections::BTreeMap::new(), &extra));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| format!("启动 pi 失败：{e}"))?;
        let mut stdin = child.stdin.take().ok_or_else(|| "pi 的 stdin 不可用".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "pi 的 stdout 不可用".to_string())?;
        let stderr = child.stderr.take();

        // stderr 必须读走：管道写满会把子进程卡住。
        if let Some(err) = stderr {
            std::thread::spawn(move || {
                for _ in BufReader::new(err).lines() {}
            });
        }

        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut splitter = ai_proto::LineSplitter::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for l in splitter.push(&buf[..n]).into_iter().flatten() {
                            if tx.send(l).is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let result = drive_once(&mut stdin, &rx, &req, &self.app);
        drop(stdin);
        reap_child(&mut child);
        result
    }
}

/// 驱动一轮 JSONL 对话：设模型 → 提问 → 应答闸门/宿主 → 等 `agent_settled`。
fn drive_once<R: tauri::Runtime>(
    stdin: &mut impl Write,
    rx: &mpsc::Receiver<String>,
    req: &ProfileReq,
    app: &tauri::AppHandle<R>,
) -> Result<Value, String> {
    let mut seq: u64 = 0;
    if let Some(m) = &req.model {
        if !m.id.is_empty() {
            let frame = ai_proto::cmd(
                &next_req_id(&mut seq),
                "set_model",
                json!({ "provider": m.provider, "modelId": m.id }),
            );
            write_frame(stdin, &frame)?;
        }
    }
    if let Some(level) = &req.thinking {
        if !level.is_empty() {
            let frame = ai_proto::cmd(
                &next_req_id(&mut seq),
                "set_thinking_level",
                json!({ "level": level }),
            );
            write_frame(stdin, &frame)?;
        }
    }
    let prompt_id = next_req_id(&mut seq);
    write_frame(stdin, &ai_proto::cmd(&prompt_id, "prompt", json!({ "message": req.prompt })))?;

    let deadline = Instant::now() + PROFILE_TIMEOUT;
    let mut profile: Option<Value> = None;
    loop {
        if req.cancel.load(Ordering::SeqCst) {
            return Err("已取消".to_string());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("画像超时".to_string());
        }
        let line = match rx.recv_timeout(remaining) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) => return Err("画像超时".to_string()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("pi 意外退出".to_string());
            }
        };
        match ai_proto::parse(&line) {
            Ok(ai_proto::Incoming::Response { id, success, error, .. }) => {
                if id.as_deref() == Some(prompt_id.as_str()) && !success {
                    return Err(error.unwrap_or_else(|| "AI 拒绝了这个请求".to_string()));
                }
            }
            Ok(ai_proto::Incoming::UiRequest { id, method, title, placeholder, .. }) => {
                if method != "input" {
                    let _ = write_frame(stdin, &ai_proto::ui_cancelled(&id));
                    continue;
                }
                match title.as_deref() {
                    Some("pc-gate") => {
                        let payload = placeholder
                            .as_deref()
                            .and_then(|p| serde_json::from_str::<Value>(p).ok());
                        let reply = match payload
                            .as_ref()
                            .and_then(|v| v.get("tool").and_then(Value::as_str))
                        {
                            Some(tool) => {
                                let input = payload
                                    .as_ref()
                                    .and_then(|v| v.get("input").cloned())
                                    .unwrap_or(Value::Null);
                                let d = ai_gate::decide(&req.root, tool, &input);
                                json!({ "allow": d.allow, "reason": d.reason, "patch": d.patch })
                            }
                            None => json!({ "allow": false, "reason": "权限请求格式错误，已拦截" }),
                        };
                        let _ = write_frame(stdin, &ai_proto::ui_value(&id, &reply.to_string()));
                    }
                    Some("pc-host") => {
                        let parsed = placeholder
                            .as_deref()
                            .and_then(|p| serde_json::from_str::<Value>(p).ok());
                        let method = parsed
                            .as_ref()
                            .and_then(|v| v.get("method"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let params = parsed
                            .as_ref()
                            .and_then(|v| v.get("params"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let body = match method.as_str() {
                            "context" => match ai_context::project_context(app, req.project_id) {
                                Ok(v) => json!({ "ok": true, "result": v }),
                                Err(e) => json!({ "ok": false, "error": e }),
                            },
                            "profile" => {
                                profile = Some(params);
                                json!({ "ok": true, "result": { "shown": true } })
                            }
                            _ => json!({ "ok": false, "error": "批量画像不支持该能力" }),
                        };
                        let _ = write_frame(stdin, &ai_proto::ui_value(&id, &body.to_string()));
                    }
                    _ => {
                        let _ = write_frame(stdin, &ai_proto::ui_cancelled(&id));
                    }
                }
            }
            Ok(ai_proto::Incoming::Event { kind, .. }) => {
                if kind == "agent_settled" {
                    break;
                }
            }
            Err(_) => {}
        }
    }
    let raw = profile.ok_or_else(|| "模型没有提交画像".to_string())?;
    normalize_profile(&raw)
}

/* ----------------------------- 任务入口 ----------------------------- */

fn project_root(project_id: i64) -> Result<PathBuf, String> {
    let conn = db::open_db().map_err(|e| e.to_string())?;
    let path = db::project_path(&conn, project_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "项目不存在".to_string())?;
    let root = PathBuf::from(path);
    if !root.is_dir() {
        return Err("项目路径不存在".to_string());
    }
    Ok(root)
}

/// 跑一个项目的画像：拼提示词（复用 Profile 任务的措辞），再交给驱动。
fn run_profile_job<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: i64,
    name: &str,
    root: &Path,
    cancel: &Arc<AtomicBool>,
) -> Result<Value, String> {
    let ctx = ai_context::project_context(app, project_id).map_err(|e| format!("{name}：{e}"))?;
    let prompt = AiTask::Profile.prompt(&ctx);
    let settings = ai_settings::load();
    let thinking = Some(settings.light_thinking).filter(|t| !t.is_empty());
    let req = ProfileReq {
        project_id,
        root: root.to_path_buf(),
        prompt,
        model: settings.light_model,
        thinking,
        cancel: Arc::clone(cancel),
    };
    default_driver(app)?.drive(req)
}

/// 起 worker；返回实际起来的线程数（0 表示整批都跑不了）。
fn spawn_batch_workers<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> usize {
    let mut spawned = 0;
    for n in 0..BATCH_WORKERS {
        let app = app.clone();
        if std::thread::Builder::new()
            .name(format!("ai-batch-{n}"))
            .spawn(move || batch_worker(app))
            .is_ok()
        {
            spawned += 1;
        }
    }
    spawned
}

/// 一个 worker 都没起来：把还在排队的标失败，别让界面永远停在 queued。
fn batch_fail_all(error: &str) -> Vec<BatchItem> {
    let mut st = lock_batch();
    st.queue.clear();
    for item in st.items.iter_mut() {
        if item.state == "queued" {
            item.state = "failed".to_string();
            item.error = Some(error.to_string());
        }
    }
    if st.running == 0 {
        st.active = false;
    }
    st.items.clone()
}

fn batch_worker<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    let mgr = manager(&app).ok();
    loop {
        if !batch_has_work() {
            break;
        }
        // 会话进程已经占满：让一让，别把 pi 的总数顶穿。
        if let Some(m) = &mgr {
            if m.live() >= ai_session::MAX_LIVE {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }
        let Some(item) = batch_claim() else { break };
        emit_batch(&app);
        let result = match project_root(item.project_id) {
            Ok(root) => {
                let cancel = batch_cancel_flag();
                run_profile_job(&app, item.project_id, &item.name, &root, &cancel)
            }
            Err(e) => Err(e),
        };
        batch_finish(item.project_id, result);
        emit_batch(&app);
    }
    if batch_worker_exit() {
        emit_batch(&app);
    }
}

/* ----------------------------- 命令 ----------------------------- */

#[tauri::command]
pub fn ai_batch_profile<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    ids: Vec<i64>,
) -> Result<(), String> {
    let items = prepare_batch(&ids)?;
    // 先把 queued 推出去，再开 worker。
    if let Ok(mgr) = manager(&app) {
        mgr.emit_batch(&items);
    }
    if spawn_batch_workers(&app) == 0 {
        let items = batch_fail_all("无法启动批量任务");
        if let Ok(mgr) = manager(&app) {
            mgr.emit_batch(&items);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn ai_batch_status() -> Result<Vec<BatchItem>, String> {
    Ok(batch_snapshot())
}

#[tauri::command]
pub fn ai_batch_adopt<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: i64,
    summary: bool,
    stack: bool,
    tags: Vec<String>,
) -> Result<(), String> {
    let items = batch_adopt_core(project_id, summary, stack, &tags)?;
    if let Ok(mgr) = manager(&app) {
        mgr.emit_batch(&items);
    }
    Ok(())
}

#[tauri::command]
pub fn ai_batch_dismiss<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: i64,
) -> Result<(), String> {
    let items = batch_dismiss_core(project_id)?;
    if let Ok(mgr) = manager(&app) {
        mgr.emit_batch(&items);
    }
    Ok(())
}

#[tauri::command]
pub fn ai_batch_cancel<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    let items = batch_cancel_core();
    if let Ok(mgr) = manager(&app) {
        mgr.emit_batch(&items);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScopedDataDir;
    use std::collections::BTreeMap;

    fn spec(name: &str) -> ServiceSpec {
        ServiceSpec::from_json(&format!(r#"{{"name":"{name}","run":["pnpm","dev"]}}"#)).unwrap()
    }

    #[test]
    fn proposal_card_renders_before_after_and_env_keys() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("package.json"), "{}").unwrap();

        let mut env = BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        let mut api = spec("api");
        api.env = env;
        let mut web = spec("web");
        web.steps = vec![crate::services::Step {
            name: "install".to_string(),
            argv: vec!["pnpm".to_string(), "install".to_string()],
            timeout_s: 60,
        }];
        let specs = vec![api, web];

        let p = build_proposal(d.path(), Layer::Db, &[], &specs, 1, "补齐两个服务").unwrap();
        assert_eq!(p.card["kind"], "proposal");
        assert_eq!(p.card["state"], "pending");
        assert_eq!(p.card["attempt"], 1);
        assert_eq!(p.card["layer"], "db");
        assert_eq!(p.card["run_steps_default"], true);
        assert!(p.card["id"].as_str().unwrap().starts_with("prop-"));

        let after: Descriptor = serde_json::from_str(p.card["after"].as_str().unwrap()).unwrap();
        assert_eq!(after, Descriptor::new(specs.clone()));

        let keys: Vec<&str> = p.card["env_keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(keys.contains(&"DATABASE_URL"), "env_keys 应含键名：{keys:?}");
        assert!(!keys.contains(&"postgres://secret"), "env_keys 绝不能含值：{keys:?}");
    }

    #[test]
    fn replace_services_applies_whole_set_atomically() {
        let data = tempfile::tempdir().unwrap();
        let _guard = ScopedDataDir::new(data.path());
        let project = tempfile::tempdir().unwrap();

        let conn = db::open_db().unwrap();
        let pid = db::add_project(&conn, "/tmp/project-ctrl-atomic-fixture").unwrap();
        db::insert_service(&conn, pid, &spec("a"), &Derived::manual()).unwrap();
        db::insert_service(&conn, pid, &spec("b"), &Derived::manual()).unwrap();
        assert_eq!(db::services_of(&conn, pid).unwrap().len(), 2);

        apply_specs(pid, project.path(), &[spec("c")], Layer::Db).unwrap();

        let conn = db::open_db().unwrap();
        let rows = db::services_of(&conn, pid).unwrap();
        assert_eq!(rows.len(), 1, "整表替换后只应剩一行");
        assert_eq!(ServiceSpec::from_json(&rows[0].config).unwrap().name, "c");
    }

    #[test]
    fn rollback_restores_descriptor_bytes() {
        let d = tempfile::tempdir().unwrap();
        let old = Descriptor::new(vec![spec("old")]);
        descriptor::write(d.path(), &old, false).unwrap();
        let original = std::fs::read(descriptor::path_of(d.path())).unwrap();

        let snap = capture_snapshot(d.path(), Layer::Descriptor, 1).unwrap();
        descriptor::write(d.path(), &Descriptor::new(vec![spec("new")]), true).unwrap();
        assert_ne!(std::fs::read(descriptor::path_of(d.path())).unwrap(), original);

        rollback(d.path(), 1, &snap).unwrap();
        assert_eq!(std::fs::read(descriptor::path_of(d.path())).unwrap(), original);
    }

    #[test]
    fn rollback_deletes_descriptor_when_absent_before() {
        let d = tempfile::tempdir().unwrap();
        let snap = capture_snapshot(d.path(), Layer::Descriptor, 1).unwrap();
        assert!(matches!(snap, Snapshot::Descriptor(None)));

        descriptor::write(d.path(), &Descriptor::new(vec![spec("new")]), true).unwrap();
        assert!(descriptor::exists(d.path()));

        rollback(d.path(), 1, &snap).unwrap();
        assert!(!descriptor::exists(d.path()), "快照里没有文件，回滚就该删掉");
    }

    #[test]
    fn validation_rejects_duplicate_names_and_dangling_depends_on() {
        let dangling = ServiceSpec::from_json(
            r#"{"name":"a","run":["x"],"depends_on":["b"]}"#,
        )
        .unwrap();
        let cases: Vec<(&str, Vec<ServiceSpec>, bool)> = vec![
            ("重复服务名", vec![spec("a"), spec("a")], true),
            ("依赖指向集合外", vec![dangling], true),
            ("空集合", vec![], true),
            ("合法集合", vec![spec("a")], false),
        ];
        for (label, set, should_err) in cases {
            let got = validate_set(&set);
            assert_eq!(got.is_err(), should_err, "{label}：{got:?}");
        }
    }

    #[test]
    fn attempt_cap_rejects_fourth_submission() {
        let mut lp = VerifyLoop::fresh(1);
        assert_eq!(begin_attempt(&mut lp).unwrap(), 1);
        assert_eq!(begin_attempt(&mut lp).unwrap(), 2);
        assert_eq!(begin_attempt(&mut lp).unwrap(), 3);
        let err = begin_attempt(&mut lp).unwrap_err();
        assert!(err.contains("已用尽"), "{err}");
    }

    #[test]
    fn card_action_serde() {
        let cases = vec![
            (CardAction::Approve { run_steps: true }, json!({"action": "approve", "run_steps": true})),
            (CardAction::Reject { feedback: None }, json!({"action": "reject", "feedback": null})),
            (CardAction::Undo, json!({"action": "undo"})),
            (CardAction::StopServices, json!({"action": "stop_services"})),
            (CardAction::AdoptLast, json!({"action": "adopt_last"})),
            (CardAction::ApplyFix { run_steps: false }, json!({"action": "apply_fix", "run_steps": false})),
            (CardAction::Reverify, json!({"action": "reverify"})),
            (CardAction::ContinueDiagnose, json!({"action": "continue_diagnose"})),
            (
                CardAction::AdoptProfile { summary: true, stack: false, tags: vec!["web".to_string()] },
                json!({"action": "adopt_profile", "summary": true, "stack": false, "tags": ["web"]}),
            ),
            (CardAction::Dismiss, json!({"action": "dismiss"})),
            (CardAction::Allow, json!({"action": "allow"})),
            (CardAction::Deny, json!({"action": "deny"})),
        ];
        for (action, want) in cases {
            let text = serde_json::to_string(&action).unwrap();
            let back: CardAction = serde_json::from_str(&text).unwrap();
            assert_eq!(back, action);
            assert_eq!(serde_json::to_value(&back).unwrap(), want, "{text}");
        }
    }

    /* ------------------------- 批量画像 ------------------------- */

    /// 批量状态是进程级全局量：批量测试必须串行，并在开头重置。
    static BATCH_TEST: Mutex<()> = Mutex::new(());

    fn batch_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = BATCH_TEST.lock().unwrap_or_else(|e| e.into_inner());
        *lock_batch() = BatchState::fresh();
        guard
    }

    struct FakeDriver {
        reply: Result<Value, String>,
        seen: Mutex<Vec<ProfileReq>>,
    }

    impl FakeDriver {
        fn ok(profile: Value) -> Self {
            Self { reply: Ok(profile), seen: Mutex::new(Vec::new()) }
        }
    }

    impl ProfileDriver for FakeDriver {
        fn drive(&self, req: ProfileReq) -> Result<Value, String> {
            self.seen.lock().unwrap_or_else(|e| e.into_inner()).push(req);
            self.reply.clone()
        }
    }

    fn batch_project(conn: &rusqlite::Connection, path: &str) -> i64 {
        db::add_project(conn, path).unwrap()
    }

    #[test]
    fn batch_state_transitions() {
        let data = tempfile::tempdir().unwrap();
        let _dir = ScopedDataDir::new(data.path());
        let _guard = batch_guard();

        let conn = db::open_db().unwrap();
        let p1 = batch_project(&conn, "/tmp/pc-batch-transitions-a");
        let p2 = batch_project(&conn, "/tmp/pc-batch-transitions-b");
        drop(conn);

        let items = prepare_batch(&[p1, p2]).unwrap();
        assert_eq!(
            items.iter().map(|i| i.project_id).collect::<Vec<_>>(),
            vec![p1, p2],
            "条目必须按提交顺序"
        );
        assert!(items.iter().all(|i| i.state == "queued"));
        assert!(items.iter().all(|i| i.profile.is_none() && i.error.is_none()));
        assert!(lock_batch().active);

        let first = batch_claim().expect("应领到第一个");
        assert_eq!(first.project_id, p1);
        assert_eq!(first.state, "running");
        let second = batch_claim().expect("应领到第二个");
        assert_eq!(second.project_id, p2, "必须按提交顺序");
        assert!(batch_claim().is_none(), "队列已空");

        let profile = json!({"summary": "演示项目", "stack": ["Rust"], "tags": ["demo"]});
        let driver = FakeDriver::ok(profile.clone());
        let got = driver
            .drive(ProfileReq {
                project_id: p1,
                root: std::path::PathBuf::from("/tmp"),
                prompt: "画像".to_string(),
                model: None,
                thinking: None,
                cancel: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        assert_eq!(got, profile, "假驱动应原样返回");
        assert_eq!(driver.seen.lock().unwrap().len(), 1);

        let items = batch_finish(p1, Ok(profile.clone()));
        assert_eq!(items[0].state, "done");
        assert_eq!(items[0].profile.as_ref(), Some(&profile));
        assert!(lock_batch().active, "还有一个在跑，不能收尾");

        let items = batch_finish(p2, Ok(profile));
        assert!(items.iter().all(|i| i.state == "done"));
        assert!(!lock_batch().active, "队列空了且没人跑，应收尾");

        let items = batch_adopt_core(p1, true, true, &["ai".to_string()]).unwrap();
        assert_eq!(items[0].state, "adopted");
        let conn = db::open_db().unwrap();
        let row = db::all_projects(&conn)
            .unwrap()
            .into_iter()
            .find(|p| p.id == p1)
            .expect("项目应存在");
        assert_eq!(row.summary.as_deref(), Some("演示项目"));
        assert_eq!(row.stack, vec!["Rust".to_string()]);
        assert_eq!(db::tags_of(&conn, p1).unwrap(), vec!["ai".to_string()]);
    }

    #[test]
    fn batch_rejects_second_run_and_empty_selection() {
        let data = tempfile::tempdir().unwrap();
        let _dir = ScopedDataDir::new(data.path());
        let _guard = batch_guard();

        assert_eq!(prepare_batch(&[]).unwrap_err(), "请先选择项目");

        let conn = db::open_db().unwrap();
        let pid = batch_project(&conn, "/tmp/pc-batch-reject");
        drop(conn);
        prepare_batch(&[pid]).unwrap();
        assert_eq!(prepare_batch(&[pid]).unwrap_err(), "已有批量任务在运行");
        batch_cancel_core();
    }

    #[test]
    fn batch_cancel_marks_remaining_as_failed() {
        let data = tempfile::tempdir().unwrap();
        let _dir = ScopedDataDir::new(data.path());
        let _guard = batch_guard();

        let conn = db::open_db().unwrap();
        let p1 = batch_project(&conn, "/tmp/pc-batch-cancel-a");
        let p2 = batch_project(&conn, "/tmp/pc-batch-cancel-b");
        drop(conn);

        let items = prepare_batch(&[p1, p2]).unwrap();
        assert_eq!(items.len(), 2);
        let items = batch_cancel_core();
        assert!(items.iter().all(|i| i.state == "failed"));
        assert!(items.iter().all(|i| i.error.as_deref() == Some("已取消")));
        let st = lock_batch();
        assert!(st.queue.is_empty());
        assert!(!st.active, "没有在跑的，取消后应立刻收尾");
        assert!(st.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn batch_adopt_unchecked_fields_keep_current_values() {
        let data = tempfile::tempdir().unwrap();
        let _dir = ScopedDataDir::new(data.path());
        let _guard = batch_guard();

        let conn = db::open_db().unwrap();
        let pid = batch_project(&conn, "/tmp/pc-batch-adopt");
        db::set_project_profile(&conn, pid, Some("旧描述"), &["旧栈".to_string()]).unwrap();
        drop(conn);

        prepare_batch(&[pid]).unwrap();
        assert!(batch_claim().is_some());
        batch_finish(
            pid,
            Ok(json!({"summary": "新描述", "stack": ["新栈"], "tags": []})),
        );

        batch_adopt_core(pid, true, false, &[]).unwrap();
        let conn = db::open_db().unwrap();
        let row = db::all_projects(&conn)
            .unwrap()
            .into_iter()
            .find(|p| p.id == pid)
            .expect("项目应存在");
        assert_eq!(row.summary.as_deref(), Some("新描述"), "勾选的字段要更新");
        assert_eq!(row.stack, vec!["旧栈".to_string()], "没勾的字段必须保持原值");
    }

    #[test]
    fn profile_prompt_requires_pc_propose_profile() {
        let prompt = AiTask::Profile.prompt(&json!({"project": {"name": "demo"}}));
        assert!(prompt.contains("pc_propose_profile"), "{prompt}");
        assert!(prompt.contains("<project_context>") && prompt.contains("</project_context>"));
        assert!(prompt.contains(r#""name": "demo""#), "上下文应原样嵌入：{prompt}");
    }

    #[test]
    fn merge_fix_replaces_by_name_and_keeps_the_rest() {
        let current = vec![spec("api"), spec("web")];
        let mut fixed = spec("api");
        fixed.run = Some(vec!["pnpm".to_string(), "start".to_string()]);
        let merged = merge_fix(&current, &[fixed.clone(), spec("worker")]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0], fixed);
        assert_eq!(merged[1], spec("web"));
        assert_eq!(merged[2], spec("worker"));
    }
}
