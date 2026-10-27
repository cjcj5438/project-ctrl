//! 端口探测与顺延。
//!
//! 两类端口语义相反，不可混用：
//! - `dev_port`（服务自己要监听的端口）必须**空闲**，被占就顺延；
//! - `requires_ports`（服务要连的依赖，如容器里的 pg）必须**正在监听**，不监听只警告。

use std::collections::HashSet;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// 顺延尝试的上限个数（wanted..=wanted+PICK_ATTEMPTS-1）。
pub const PICK_ATTEMPTS: u16 = 10;
/// 实测校准时向后观察的端口个数（比分配范围宽，因为框架可能自己继续 +1）。
#[cfg(test)]
pub const OBSERVE_SPAN: u16 = 40;

const LSOF: &str = "/usr/sbin/lsof";

/// 端口是否空闲。
///
/// 三个条件都满足才算空闲：
/// 1. 能在 127.0.0.1 上 bind；
/// 2. 能在 ::1 上 bind（IPv6 不可用不算占用）；
/// 3. `lsof` 没有报告该端口上的监听者。
///
/// 第 3 条是必要的：实测本机 docker 只在 IPv6 `*:8080` 上监听，而 macOS 允许我们再 bind
/// `127.0.0.1:8080`——只看 bind 会把「已被别人占用」误判成空闲，导致两个服务抢同一端口号。
///
/// 单次查询用这个函数；需要反复判断时用 `pick` / `listening_in` / `missing_requires`
/// （它们只扫一次 `lsof`，而不是每个端口扫一次）。
pub fn is_free(port: u16) -> bool {
    // 0 不是可分配的固定端口：bind(0) 语义是“随机分配”，判空闲只会让调用方恒超时。
    if port == 0 {
        return false;
    }
    bind_free(port) && !listen_set().contains(&port)
}

/// 只靠 bind 判断（快，但看不到「别的地址族上有人监听」）。
fn bind_free(port: u16) -> bool {
    let v4 = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(_) => return false,
    };
    drop(v4);
    match TcpListener::bind(("::1", port)) {
        Ok(l) => drop(l),
        Err(e) => {
            // 机器没开 IPv6 时不该误判为「被占用」
            if !matches!(
                e.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) {
                return false;
            }
        }
    }
    true
}

/// 一次 `lsof` 拿到全部监听端口（`-Fn` 只出机器可读字段）。
///
/// 早先每个端口跑一次 `lsof`，40 个端口要十几秒；合并成一次扫描后整轮观测只付一次
/// 进程启动成本。命令不可用时返回空集合（退化为只看 bind）。
fn listen_set() -> HashSet<u16> {
    let out = match Command::new(LSOF)
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fn"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return HashSet::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut set = HashSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('n') else { continue };
        // n127.0.0.1:8080 / n*:8080 / n[::1]:1420
        if let Some((_, port)) = rest.rsplit_once(':') {
            if let Ok(p) = port.parse::<u16>() {
                set.insert(p);
            }
        }
    }
    set
}

/// 占用该端口的进程名（尽力而为；任何失败都返回 None）。
pub fn holder(port: u16) -> Option<String> {
    // lsof 要求 `-iTCP:<port>` 是单个参数；拆成两个 token 会被当成文件名。
    let filter = format!("-iTCP:{port}");
    let out = Command::new(LSOF)
        .args(["-nP", &filter, "-sTCP:LISTEN", "-Fc"])
        .output()
        .ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix('c'))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// 只在测试里使用：产品代码按「上次端口优先」自行用 `is_free` 逐个试（见 `plan::plan_port`）。
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub port: u16,
    pub shifted: bool,
    pub wanted: u16,
}

/// 从 `port` 起找一个空闲端口，最多试 `PICK_ATTEMPTS` 个。全占返回 None。
#[cfg(test)]
pub fn pick(port: u16) -> Option<Pick> {
    let listening = listen_set();
    for offset in 0..PICK_ATTEMPTS {
        let candidate = port.saturating_add(offset);
        if bind_free(candidate) && !listening.contains(&candidate) {
            return Some(Pick { port: candidate, shifted: offset != 0, wanted: port });
        }
        if candidate == u16::MAX {
            break;
        }
    }
    None
}

/// 区间内当前处于「被监听」状态的端口集合。
///
/// 只返回**区间内**的端口（全局 `lsof` 快照仅用于判定，不能直接当结果——
/// `wait_listening` 靠 `difference` 找新端口，返回全局集合会把区间外的端口也算进去）。
#[cfg(test)]
pub fn listening_in(range: std::ops::RangeInclusive<u16>) -> HashSet<u16> {
    let all = listen_set();
    range.filter(|p| all.contains(p) || !bind_free(*p)).collect()
}

/// `requires_ports` 里那些「应当被监听、实际却空闲」的端口（升序）。
pub fn missing_requires(ports: &[u16]) -> Vec<u16> {
    let listening = listen_set();
    let mut v: Vec<u16> = ports
        .iter()
        .copied()
        .filter(|p| bind_free(*p) && !listening.contains(p))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// 把顺延后的端口写进命令行。
///
/// 返回 `(argv, extra_env)`。规则按序命中第一条，且不写正则：
/// 1. 末尾是 `--port N` / `-p N` → 替换 N；
/// 2. argv 里任一处是 `--port N` / `-p N` → 替换其后一个元素；
/// 3. 存在 vite 配置 → 追加 `--port N`（npm/yarn 前插 `--`）；
/// 4. 存在 next 配置 → 追加 `-p N`（同上插 `--`）；
/// 5. 都不命中 → argv 原样，注入 `PORT=N`。
pub fn plan_argv(
    _wanted: u16,
    actual: u16,
    argv: &[String],
    base: &Path,
) -> (Vec<String>, Vec<(String, String)>) {
    let actual_s = actual.to_string();
    let is_flag = |s: &String| s == "--port" || s == "-p";
    let is_num = |s: &String| s.parse::<u16>().is_ok();

    // 规则 1：末尾
    let n = argv.len();
    if n >= 2 && is_flag(&argv[n - 2]) && is_num(&argv[n - 1]) {
        let mut out = argv.to_vec();
        out[n - 1] = actual_s;
        return (out, Vec::new());
    }
    // 规则 2：任一处
    if let Some(i) = argv.iter().position(is_flag) {
        if i + 1 < argv.len() && is_num(&argv[i + 1]) {
            let mut out = argv.to_vec();
            out[i + 1] = actual_s;
            return (out, Vec::new());
        }
    }

    let has = |names: &[&str]| names.iter().any(|f| base.join(f).is_file());
    let vite = has(&["vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts"]);
    let next = has(&["next.config.js", "next.config.mjs", "next.config.ts"]);

    if vite || next {
        let mut out = argv.to_vec();
        // npm / yarn 需要 `--` 把参数传给底层脚本；pnpm / bun 也接受这个分隔符
        let needs_sep = matches!(argv.first().map(|s| s.as_str()), Some("npm") | Some("yarn"))
            && matches!(argv.get(1).map(|s| s.as_str()), Some("run") | Some("start"));
        if needs_sep {
            out.push("--".to_string());
        }
        out.push(if vite { "--port".to_string() } else { "-p".to_string() });
        out.push(actual_s);
        return (out, Vec::new());
    }

    (argv.to_vec(), vec![("PORT".to_string(), actual_s)])
}

/// spawn 后等待「某个端口从空闲变为被占」。
///
/// `before` 是 spawn 前的监听快照；返回观察区间内第一个新出现在监听状态的端口。
/// 只用于**报告真相**，不影响分配决策。
#[cfg(test)]
pub fn wait_listening(
    before: &HashSet<u16>,
    range: std::ops::RangeInclusive<u16>,
    timeout: Duration,
) -> Option<u16> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let now = listening_in(range.clone());
        let mut new_ports: Vec<u16> = now.difference(before).copied().collect();
        new_ports.sort_unstable();
        if let Some(p) = new_ports.first() {
            return Some(*p);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    None
}

/// 停止后等待端口释放（供 supervisor 用）。
///
/// 仅覆盖 TCP（`is_free` 只 bind/观测 TCP 监听）：UDP 独占的端口这里看不见，
/// 若服务是纯 UDP，返回 true 不代表端口真释放——调用方不要据此做强假设。
pub fn wait_released(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if is_free(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    is_free(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn is_free_zero_is_always_false() {
        // PORT=0 会恒超时：0 不是可分配的固定端口，判空闲只会误导调用方去 bind(0)。
        assert!(!is_free(0));
    }

    #[test]
    fn pick_skips_busy() {
        // 连续占用一段端口，验证 pick 会跳过它们并报 shifted。
        let base = 46000u16;
        let mut held = Vec::new();
        for p in base..base + 3 {
            if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
                held.push(l);
            }
        }
        if held.len() != 3 {
            return; // 该段被系统占用，跳过（等价于环境不满足前置）
        }
        let got = pick(base).expect("应能在附近找到空闲端口");
        assert!(got.shifted);
        assert_eq!(got.wanted, base);
        assert!(got.port >= base + 3);
        drop(held);
    }

    #[test]
    fn plan_argv_replaces_existing_port_flag() {
        let dir = tempfile::tempdir().unwrap();
        let (out, env) =
            plan_argv(5190, 5191, &argv(&["vite", "--port", "5190"]), dir.path());
        assert_eq!(out, argv(&["vite", "--port", "5191"]));
        assert!(env.is_empty());
    }

    #[test]
    fn plan_argv_replaces_mid_argv_port_flag() {
        let dir = tempfile::tempdir().unwrap();
        let (out, _) =
            plan_argv(5190, 5191, &argv(&["vite", "-p", "5190", "--strictPort"]), dir.path());
        assert_eq!(out, argv(&["vite", "-p", "5191", "--strictPort"]));
    }

    #[test]
    fn plan_argv_appends_after_run_separator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vite.config.ts"), "").unwrap();
        let (out, env) = plan_argv(5190, 5191, &argv(&["npm", "run", "dev"]), dir.path());
        assert_eq!(out, argv(&["npm", "run", "dev", "--", "--port", "5191"]));
        assert!(env.is_empty());
    }

    #[test]
    fn plan_argv_appends_without_separator_for_pnpm() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vite.config.ts"), "").unwrap();
        let (out, _) = plan_argv(5190, 5191, &argv(&["pnpm", "dev"]), dir.path());
        assert_eq!(out, argv(&["pnpm", "dev", "--port", "5191"]));
    }

    #[test]
    fn plan_argv_uses_next_dash_p() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("next.config.mjs"), "").unwrap();
        let (out, _) = plan_argv(3000, 3001, &argv(&["npm", "run", "dev"]), dir.path());
        assert_eq!(out, argv(&["npm", "run", "dev", "--", "-p", "3001"]));
    }

    #[test]
    fn plan_argv_falls_back_to_port_env() {
        let dir = tempfile::tempdir().unwrap();
        let (out, env) = plan_argv(8080, 8081, &argv(&["node", "server.js"]), dir.path());
        assert_eq!(out, argv(&["node", "server.js"]));
        assert_eq!(env, vec![("PORT".to_string(), "8081".to_string())]);
    }

    #[test]
    fn missing_requires_reports_idle_only() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = l.local_addr().unwrap().port();
        // 找一个确定空闲的端口：绑 0 再释放
        let l2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let idle = l2.local_addr().unwrap().port();
        drop(l2);
        let missing = missing_requires(&[busy, idle]);
        if is_free(idle) {
            assert!(missing.contains(&idle));
        }
        assert!(!missing.contains(&busy));
        drop(l);
    }

    #[test]
    fn holder_is_none_for_free_port() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let free = l.local_addr().unwrap().port();
        drop(l);
        if is_free(free) {
            assert_eq!(holder(free), None);
        }
    }

    #[test]
    fn holder_names_the_listening_process() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        // lsof 需要一点时间把新 socket 反映出来
        let got = (0..40).find_map(|_| {
            std::thread::sleep(Duration::from_millis(25));
            holder(port)
        });
        assert!(got.is_some(), "应能取到占用 {port} 的进程名");
        drop(l);
    }
}
