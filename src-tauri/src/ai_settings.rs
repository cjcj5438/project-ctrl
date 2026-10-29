//! AI 设置持久化 + pi 二进制发现 + 模型列表。
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `~/.project-ctrl/ai/settings.json` | 用户选的 pi 路径、主/轻量模型与思考档位 |
//!
//! 子进程一律走 [`crate::execenv`] 的固定 PATH（`env_clear()` 后重建），不经过 shell；
//! `PI_TELEMETRY=0` 关掉 pi 自己的遥测。目录与文件权限交给 `localstate::atomic_write`
//! （0600 + tmp/rename），与其它本机状态文件保持同一套写法。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 允许的思考档位（顺序即由弱到强，前端直接按此顺序渲染）。
pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `pi --version` 的超时。
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// `resolve_pi` 结果缓存时长。
const PI_CACHE_TTL: Duration = Duration::from_secs(60);
/// 模型列表缓存时长。
const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);
/// 等 pi 回 `get_available_models` 应答的超时。
const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
/// 收到应答后等进程自行退出的宽限；超时则杀进程组。
const STOP_GRACE: Duration = Duration::from_secs(3);
/// 轮询 `try_wait()` 的步长（项目没有 `wait_timeout` 依赖）。
const POLL: Duration = Duration::from_millis(50);

/// 最低可用 pi 版本。
const MIN_VERSION: (u64, u64, u64) = (1, 1, 0);

/// 模型引用：provider 与模型 id 一起才唯一。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

/// `settings.json` 的整份内容。缺字段一律回落默认值（`#[serde(default)]`），
/// 旧版本写下的文件不会因为新增字段而读不出来。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AiSettings {
    /// 用户显式指定的 pi 可执行文件；`None` 表示按 PATH 解析。
    pub pi_path: Option<String>,
    pub main_model: Option<ModelRef>,
    pub main_thinking: String,
    pub light_model: Option<ModelRef>,
    pub light_thinking: String,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            pi_path: None,
            main_model: None,
            main_thinking: "medium".to_string(),
            light_model: None,
            light_thinking: "low".to_string(),
        }
    }
}

/// 设置文件位置：`<data_dir>/ai/settings.json`。
pub fn settings_path() -> PathBuf {
    crate::db::data_dir().join("ai").join("settings.json")
}

/// 读设置。文件缺失或损坏都返回默认值（损坏只打日志，不阻断启动）。
pub fn load() -> AiSettings {
    let path = settings_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AiSettings::default(),
        Err(e) => {
            eprintln!("读取 AI 设置失败（使用默认值）：{e}");
            return AiSettings::default();
        }
    };
    match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("AI 设置解析失败（使用默认值）：{e}");
            AiSettings::default()
        }
    }
}

/// 思考档位是否合法。
pub fn thinking_ok(v: &str) -> bool {
    THINKING_LEVELS.contains(&v)
}

/// pi 路径校验：绝对路径 + 存在的可执行普通文件。
fn check_pi_path(p: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let path = Path::new(p);
    let ok = path.is_absolute()
        && std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(format!("pi 路径无效：{p}（需要是绝对路径且可执行）"))
    }
}

/// 写设置。校验通过才落盘；成功后清掉 `resolve_pi` 缓存（路径可能变了）。
pub fn save(s: &AiSettings) -> Result<(), String> {
    if !thinking_ok(&s.main_thinking) {
        return Err(format!(
            "思考档位无效：{}（可选 off/minimal/low/medium/high/xhigh/max）",
            s.main_thinking
        ));
    }
    if !thinking_ok(&s.light_thinking) {
        return Err(format!(
            "思考档位无效：{}（可选 off/minimal/low/medium/high/xhigh/max）",
            s.light_thinking
        ));
    }
    if let Some(p) = &s.pi_path {
        check_pi_path(p)?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| format!("序列化 AI 设置失败：{e}"))?;
    crate::localstate::atomic_write(&settings_path(), format!("{json}\n").as_bytes())?;
    clear_cache();
    Ok(())
}

/// 解析出的 pi 可执行文件。
#[derive(Clone, Debug, PartialEq)]
pub struct PiBin {
    pub program: PathBuf,
    /// 传给 pi 的前置参数（当前一律为空，留着以支持 `node .../pi.js` 这类启动方式）。
    pub prefix_args: Vec<String>,
    pub version: String,
}

/// `resolve_pi` 结果缓存：60s 内重复调用（每次 `ai_status` 都会调）不再 fork 子进程。
static CACHE: Mutex<Option<(Instant, Result<PiBin, String>)>> = Mutex::new(None);

/// 清空缓存（`save` 与测试用）。
pub fn clear_cache() {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// 子进程环境：清空后只保留 [`execenv::env_for`] 声明的变量，并关掉 pi 遥测。
fn apply_env(cmd: &mut Command) {
    cmd.env_clear();
    let extra = [("PI_TELEMETRY".to_string(), "0".to_string())];
    cmd.envs(crate::execenv::env_for(None, &BTreeMap::new(), &extra));
}

/// 找 pi，并确认 `node` 也在同一套 PATH 上、版本够新。
pub fn resolve_pi(s: &AiSettings) -> Result<PiBin, String> {
    {
        let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, cached)) = guard.as_ref() {
            if at.elapsed() < PI_CACHE_TTL {
                return cached.clone();
            }
        }
    }
    let result = resolve_pi_uncached(s);
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((Instant::now(), result.clone()));
    result
}

fn resolve_pi_uncached(s: &AiSettings) -> Result<PiBin, String> {
    let path = crate::execenv::path_for(None);
    let program = match &s.pi_path {
        Some(p) => {
            check_pi_path(p)?;
            PathBuf::from(p)
        }
        None => crate::execenv::resolve("pi", &path).ok_or_else(|| {
            "未找到 pi：请执行 npm i -g @earendil-works/pi-coding-agent，或在设置中填写 pi 路径"
                .to_string()
        })?,
    };
    // pi 是 node 程序：解析不到 node 时 `--version` 也会失败，提前给出可读的原因。
    if crate::execenv::resolve("node", &path).is_none() {
        return Err("找不到 node：pi 需要 node ≥ 22.19".to_string());
    }
    let prefix_args = Vec::new();
    let version = run_version(&program, &prefix_args)?;
    if !version_ok(&version) {
        return Err(format!(
            "pi 版本过低（{version}），需要 ≥ 1.1.0：npm i -g @earendil-works/pi-coding-agent"
        ));
    }
    Ok(PiBin { program, prefix_args, version })
}

/// 跑 `<program> --version`，10s 超时；返回解析出的版本号。
fn run_version(program: &Path, prefix_args: &[String]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(prefix_args).arg("--version");
    apply_env(&mut cmd);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 pi 失败：{e}"))?;

    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(format!("等待 pi 失败：{e}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("pi --version 超时".to_string());
        }
        std::thread::sleep(POLL);
    }

    let out = child.wait_with_output().map_err(|e| format!("读取 pi 输出失败：{e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    parse_version(&text).ok_or_else(|| "无法解析 pi 版本".to_string())
}

/// 从任意文本里取第一个 `x.y.z`（前后不能紧邻数字或 `.`，避免把 `1.1.0.5` 截成 `1.1.0`）。
pub fn parse_version(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let prev_ok = i == 0 || (!b[i - 1].is_ascii_digit() && b[i - 1] != b'.');
        if prev_ok {
            let mut j = i;
            let mut parts = 0;
            let mut end = i;
            let mut shape_ok = true;
            while parts < 3 {
                let start = j;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j == start {
                    shape_ok = false;
                    break;
                }
                parts += 1;
                end = j;
                if parts < 3 {
                    if j < b.len() && b[j] == b'.' {
                        j += 1;
                    } else {
                        shape_ok = false;
                        break;
                    }
                }
            }
            let tail_ok = end >= b.len() || b[end] != b'.';
            if shape_ok && parts == 3 && tail_ok {
                return Some(text[i..end].to_string());
            }
        }
        i += 1;
    }
    None
}

/// 版本是否 ≥ [`MIN_VERSION`]。解析不出三段数字一律否。
pub fn version_ok(v: &str) -> bool {
    let Some(parsed) = parse_version(v) else { return false };
    let nums: Vec<u64> = parsed.split('.').filter_map(|p| p.parse().ok()).collect();
    if nums.len() < 3 {
        return false;
    }
    (nums[0], nums[1], nums[2]) >= MIN_VERSION
}

/// 前端用的模型条目（pi `data.models[]` 的投影）。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct AiModel {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: u64,
}

/// 模型列表缓存：键是「程序路径 + 版本」，换 pi 或升级内核后自动失效。
static MODELS: Mutex<Option<(String, Instant, Vec<AiModel>)>> = Mutex::new(None);

fn cache_key(bin: &PiBin) -> String {
    format!("{}|{}", bin.program.display(), bin.version)
}

/// 读模型列表（`get_available_models`）。`refresh` 跳过 5 分钟缓存。
pub fn list_models(bin: &PiBin, refresh: bool) -> Result<Vec<AiModel>, String> {
    let key = cache_key(bin);
    if !refresh {
        let guard = MODELS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_key, at, models)) = guard.as_ref() {
            if *cached_key == key && at.elapsed() < MODELS_CACHE_TTL {
                return Ok(models.clone());
            }
        }
    }
    let models = fetch_models(bin)?;
    let mut guard = MODELS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((key, Instant::now(), models.clone()));
    Ok(models)
}

/// 起一个 RPC 模式的 pi 子进程问一次模型清单，然后无条件收干净。
fn fetch_models(bin: &PiBin) -> Result<Vec<AiModel>, String> {
    let mut cmd = Command::new(&bin.program);
    cmd.args(&bin.prefix_args).args([
        "--mode",
        "rpc",
        "-ne",
        "-ns",
        "-np",
        "--no-themes",
        "--no-mcp",
        "-na",
        "--offline",
        "--no-session",
    ]);
    cmd.current_dir(cwd());
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // 独立进程组：pi 会派生工具子进程，收尾时按组杀掉，避免漏进程。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    apply_env(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| format!("启动 pi 失败：{e}"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| "pi 的 stdin 不可用".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "pi 的 stdout 不可用".to_string())?;
    let stderr = child.stderr.take();

    // `ai_proto::cmd` 已带行尾换行（JSONL 分帧）。
    let frame = crate::ai_proto::cmd("m1", "get_available_models", serde_json::Value::Null);
    let _ = stdin.write_all(frame.as_bytes());
    let _ = stdin.flush();

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    // stderr 也要读走：管道写满会让子进程卡住。
    if let Some(err) = stderr {
        std::thread::spawn(move || {
            for _ in BufReader::new(err).lines() {}
        });
    }

    let result = wait_models(&rx);
    drop(stdin);
    reap(&mut child);
    result
}

/// 在超时内等 `id == "m1"` 的 `response`。
fn wait_models(rx: &mpsc::Receiver<String>) -> Result<Vec<AiModel>, String> {
    let deadline = Instant::now() + MODELS_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("读取模型列表超时".to_string());
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let Ok(msg) = crate::ai_proto::parse(&line) else { continue };
                let crate::ai_proto::Incoming::Response { id, success, data, error, .. } = msg else {
                    continue;
                };
                if id.as_deref() != Some("m1") {
                    continue;
                }
                if !success {
                    return Err(error.unwrap_or_else(|| "pi 返回失败：get_available_models".to_string()));
                }
                return Ok(parse_models(&data));
            }
            Err(RecvTimeoutError::Timeout) => return Err("读取模型列表超时".to_string()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("pi 未返回模型列表（进程已退出）".to_string())
            }
        }
    }
}

/// `data.models[]` → [`AiModel`]：缺 name 回落 id，缺 reasoning/contextWindow 取零值。
/// 没有 id 的条目无法引用，直接跳过。
fn parse_models(data: &serde_json::Value) -> Vec<AiModel> {
    let Some(arr) = data.get("models").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    arr.iter().filter_map(model_from).collect()
}

fn model_from(v: &serde_json::Value) -> Option<AiModel> {
    let id = v.get("id").and_then(serde_json::Value::as_str)?.to_string();
    let provider = v
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let name = v
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    let reasoning = v
        .get("reasoning")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let context_window = v
        .get("contextWindow")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Some(AiModel { provider, id, name, reasoning, context_window })
}

/// 收尾：先给 3s 优雅退出，再按进程组 SIGKILL。
fn reap(child: &mut std::process::Child) {
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }
    // 组内后续子进程可能还活着：按 pd=pid 的进程组整组杀掉（process_group(0) 保证 pgid=pid）。
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY：只是发信号；pid 来自刚 spawn 的 child，pgid 见上。
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// pi 的 cwd：家目录（拿不到就用 `/`）。
fn cwd() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScopedDataDir;

    /// 在临时数据目录里跑 `f`（`settings_path()` 依赖 `PROJECT_CTRL_DATA_DIR`）。
    fn with_data_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        let tmp = tempfile::tempdir().expect("临时目录应能创建");
        let _guard = ScopedDataDir::new(tmp.path());
        f(tmp.path())
    }

    fn model(provider: &str, id: &str) -> Option<ModelRef> {
        Some(ModelRef { provider: provider.into(), id: id.into() })
    }

    #[test]
    fn save_rejects_unknown_thinking_level() {
        with_data_dir(|_| {
            let s = AiSettings { main_thinking: "turbo".into(), ..AiSettings::default() };
            let err = save(&s).expect_err("非法思考档位应被拒绝");
            assert!(err.contains("思考档位无效：turbo"), "实际：{err}");
            assert!(!settings_path().exists(), "校验失败时不应写文件");

            let s = AiSettings { light_thinking: String::new(), ..AiSettings::default() };
            assert!(save(&s).is_err(), "空思考档位应被拒绝");
            assert!(!settings_path().exists());
        });
    }

    #[test]
    fn save_validates_pi_path_and_round_trips() {
        with_data_dir(|_| {
            let bad = AiSettings { pi_path: Some("bin/pi".into()), ..AiSettings::default() };
            let err = save(&bad).expect_err("相对路径应被拒绝");
            assert!(err.contains("pi 路径无效"), "实际：{err}");
            assert!(!settings_path().exists());

            let s = AiSettings {
                pi_path: Some("/bin/echo".into()),
                main_model: model("anthropic", "claude-sonnet-4-5"),
                main_thinking: "high".into(),
                light_model: model("anthropic", "claude-haiku-4-5"),
                light_thinking: "minimal".into(),
            };
            save(&s).expect("可执行文件应被接受");

            let got = load();
            assert_eq!(got, s, "load 应原样读回");
            assert_eq!(got.main_thinking, "high");
            assert_eq!(got.light_thinking, "minimal");
            assert_eq!(got.light_model, model("anthropic", "claude-haiku-4-5"));
        });
    }

    #[test]
    fn load_returns_defaults_on_corrupt_json() {
        with_data_dir(|_| {
            let path = settings_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"{ this is not json").unwrap();

            let got = load();
            assert_eq!(got, AiSettings::default());
            assert_eq!(got.pi_path, None);
            assert_eq!(got.main_thinking, "medium");
            assert_eq!(got.light_thinking, "low");
        });
    }

    #[test]
    fn resolve_pi_rejects_non_executable_path() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("pi");
        std::fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
        clear_cache();

        let s = AiSettings {
            pi_path: Some(script.display().to_string()),
            ..AiSettings::default()
        };
        let err = resolve_pi(&s).expect_err("不可执行文件应被拒绝");
        assert!(err.contains("pi 路径无效"), "实际：{err}");
        clear_cache();
    }

    #[test]
    fn version_parsing_and_comparison() {
        assert_eq!(parse_version("1.1.0").as_deref(), Some("1.1.0"));
        assert_eq!(parse_version("pi 1.2.3 (node 22)\n").as_deref(), Some("1.2.3"));
        assert_eq!(parse_version("v0.0.1").as_deref(), Some("0.0.1"));
        assert_eq!(parse_version("1.1").as_deref(), None);
        assert_eq!(parse_version("no version here").as_deref(), None);
        // 四段版本不能被截成前三段
        assert_eq!(parse_version("1.1.0.5").as_deref(), None);

        assert!(version_ok("1.1.0"));
        assert!(version_ok("1.2.0"));
        assert!(version_ok("2.0.0"));
        assert!(!version_ok("1.0.9"));
        assert!(!version_ok("0.9.9"));
        assert!(!version_ok("garbage"));
    }

    /// 在 `dir` 里写一个可执行的 sh 脚本（不依赖真 pi / node）。
    fn fake_pi(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("pi");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn pi_bin(program: &Path) -> PiBin {
        PiBin { program: program.to_path_buf(), prefix_args: Vec::new(), version: "1.2.0".into() }
    }

    #[test]
    fn run_version_parses_fake_pi_output() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = fake_pi(tmp.path(), "echo 'pi 1.2.0'");
        assert_eq!(run_version(&ok, &[]).unwrap(), "1.2.0");

        // 版本只出现在 stderr 也要认。
        let on_stderr = fake_pi(tmp.path(), "echo 'pi 2.3.4' >&2");
        assert_eq!(run_version(&on_stderr, &[]).unwrap(), "2.3.4");

        let no_version = fake_pi(tmp.path(), "echo hello");
        assert_eq!(run_version(&no_version, &[]).unwrap_err(), "无法解析 pi 版本");
    }

    #[test]
    fn list_models_over_rpc_returns_models() {
        let tmp = tempfile::tempdir().unwrap();
        let script = fake_pi(
            tmp.path(),
            "read line\n\
             printf '%s\\n' '{\"type\":\"response\",\"id\":\"m1\",\"success\":true,\
             \"data\":{\"models\":[{\"provider\":\"p\",\"id\":\"m\",\"contextWindow\":1000}]}}'",
        );
        let models = list_models(&pi_bin(&script), true).expect("应读到模型");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider, "p");
        assert_eq!(models[0].id, "m");
        assert_eq!(models[0].name, "m");
        assert!(!models[0].reasoning);
        assert_eq!(models[0].context_window, 1000);
    }

    #[test]
    fn list_models_propagates_pi_error() {
        let tmp = tempfile::tempdir().unwrap();
        let script = fake_pi(
            tmp.path(),
            "read line\n\
             printf '%s\\n' '{\"type\":\"response\",\"id\":\"m1\",\"success\":false,\"error\":\"no auth\"}'",
        );
        let err = list_models(&pi_bin(&script), true).expect_err("失败应答应变成 Err");
        assert_eq!(err, "no auth");
    }

    #[test]
    fn reap_kills_process_that_ignores_stdin_eof() {
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("gnu.pid");
        // 脚本自己再派生一个长睡子进程：收尾必须按进程组杀掉，只杀父进程会留下孤儿。
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!("sleep 30 & echo $! > {}; wait", pidfile.display()),
        ]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        cmd.process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id() as i32;

        let started = Instant::now();
        reap(&mut child);
        assert!(
            started.elapsed() < STOP_GRACE + Duration::from_secs(2),
            "收尾不应超过宽限期：{:?}",
            started.elapsed()
        );
        assert!(child.try_wait().unwrap().is_some(), "进程应已退出");
        // SAFETY：只是探测信号，pid 来自刚回收的 child。
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "不应留下孤儿进程");

        let gnu: i32 = std::fs::read_to_string(&pidfile)
            .expect("脚本应写下孙进程 pid")
            .trim()
            .parse()
            .expect("pid 应为数字");
        // 组内孙进程可能比父进程晚一步收到 SIGKILL，给最多 1s 消失。
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY：探测孙进程是否还在。
            if unsafe { libc::kill(gnu, 0) } == -1 {
                break;
            }
            assert!(Instant::now() < deadline, "组内子进程 {gnu} 应被一起杀掉");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn models_parse_missing_fields() {
        let data = serde_json::json!({
            "models": [
                {"provider": "anthropic", "id": "claude", "name": "Claude", "reasoning": true, "contextWindow": 200000},
                {"provider": "openai", "id": "gpt"},
                {"name": "没有 id 的条目"}
            ]
        });
        let models = parse_models(&data);
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0],
            AiModel {
                provider: "anthropic".into(),
                id: "claude".into(),
                name: "Claude".into(),
                reasoning: true,
                context_window: 200_000,
            }
        );
        // name 回落 id，reasoning/contextWindow 取零值
        assert_eq!(models[1].name, "gpt");
        assert!(!models[1].reasoning);
        assert_eq!(models[1].context_window, 0);
    }
}
