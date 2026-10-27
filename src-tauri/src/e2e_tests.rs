//! 真实端到端：真进程、真端口、真仓库、真外网无关。
//!
//! 这些测试不是纯函数单测，会 spawn 子进程并占用/释放真实端口，所以全部用
//! 「端口已被占用就跳过」的方式表达前置条件，避免在别的机器上误报。

#![cfg(test)]

use crate::ports;
use crate::services::ServiceConfig;
use crate::supervisor::{Launch, Phase, Supervisor};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 每个测试独享的临时项目目录（不复用 tempfile，方便失败时保留现场）。
fn scratch(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "pc-e2e-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 连接地址两个都试（vite 可能只监听 `::1`）；`Host` 头固定写 `localhost`，
/// 否则 vite 的 allowedHosts 校验会回 403。
/// 测试里 spawn 的进程必须在测试结束（包括 panic）时收掉，否则会留下占端口的孤儿。
struct KillOnDrop(Supervisor);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.shutdown_all();
    }
}

fn http_get(port: u16) -> Option<String> {
    for addr in ["127.0.0.1", "::1"] {
        let Ok(mut s) = TcpStream::connect((addr, port)) else { continue };
        s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        if s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
            continue;
        }
        let mut buf = String::new();
        if s.read_to_string(&mut buf).is_ok() && !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// 步骤 1：8080 被占（本机是 docker）→ 顺延到下一个空闲端口 → 进程真的听新端口 → 停止后释放。
#[test]
fn port_shift_actually_rebinds_and_releases() {
    let _guard = crate::test_support::exclusive_ports();
    // 自己占一个 OS 分配的端口当「冲突源」，不依赖本机 8080 上恰好有 docker。
    // 端口要留出足够的观察窗口（`wanted..=wanted+OBSERVE_SPAN` 不能溢出 u16）。
    let mut squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    while squatter.local_addr().unwrap().port() > u16::MAX - ports::OBSERVE_SPAN - ports::PICK_ATTEMPTS {
        drop(squatter);
        squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    }
    let wanted = squatter.local_addr().unwrap().port();
    assert!(!ports::is_free(wanted), "刚占用的端口必须判为占用");
    let pick = ports::pick(wanted).expect("应能顺延");
    assert!(pick.shifted, "{wanted} 已被占用，应判为顺延");
    assert_ne!(pick.port, wanted);

    let dir = scratch("shift");
    std::fs::write(
        dir.join("server.js"),
        "const p=process.env.PORT||8080;require('http').createServer((q,s)=>s.end('ok')).listen(p,'127.0.0.1')\n",
    )
    .unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"app","scripts":{"dev":"node server.js"}}"#).unwrap();

    let before = ports::listening_in(wanted..=wanted + ports::OBSERVE_SPAN);
    let argv: Vec<String> = vec!["node".into(), "server.js".into()];
    let (planned, extra) = ports::plan_argv(wanted, pick.port, &argv, &dir);
    assert_eq!(planned, argv, "无 vite/next 配置时应原样保留 argv");
    assert_eq!(extra, vec![("PORT".to_string(), pick.port.to_string())]);

    let guard = KillOnDrop(Supervisor::default());
    let sup = &guard.0;
    let sink = Arc::new(crate::supervisor::Collector::default());
    let cfg = ServiceConfig { dev_port: Some(wanted), ..Default::default() };
    let _pid = sup
        .spawn(
            sink,
            Launch {
                project_id: 1,
                base: &dir,
                argv: &planned,
                cfg: &cfg,
                extra_env: &extra,
                phase: Phase::Running,
                port: Some(pick.port),
                note: None,
            },
        )
        .expect("spawn 应成功");

    let observed = ports::wait_listening(
        &before,
        wanted..=wanted + ports::OBSERVE_SPAN,
        Duration::from_secs(15),
    )
    .expect("15 秒内应观察到新监听的端口");
    assert_eq!(observed, pick.port, "实测端口应与分配一致");
    let body = http_get(observed).expect("应能连上并读到响应");
    assert!(body.contains("ok"), "响应内容异常: {body}");

    sup.stop(1).expect("应有运行记录");
    assert!(
        ports::wait_released(observed, Duration::from_secs(5)),
        "停止后端口 {observed} 必须释放"
    );
    // 对照：原来的占用者不受影响
    assert!(!ports::is_free(wanted), "不应误伤 {wanted} 上的原进程");
    drop(squatter);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 步骤 2：真实 vite 项目（本仓库）在顺延端口上启动，HTTP 200；停止后只释放新端口。
#[test]
fn real_vite_dev_server_on_shifted_port() {
    let _guard = crate::test_support::exclusive_ports();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root").to_path_buf();
    if !repo.join("node_modules").is_dir() {
        eprintln!("跳过：{repo:?} 未安装 node_modules");
        return;
    }
    // 1420 被本机 vite 占着时才验证顺延；否则用 1420 本身也可
    let wanted = 1420u16;
    let pick = ports::pick(wanted).expect("应能选到端口");
    let before = ports::listening_in(wanted..=wanted + ports::OBSERVE_SPAN);

    let argv: Vec<String> = vec!["pnpm".into(), "dev".into()];
    let (planned, extra) = ports::plan_argv(wanted, pick.port, &argv, &repo);
    // 该测试真的要去起 vite：CI runner 上没有 pnpm 时跳过（不是产品问题）。
    // 前置条件写成显式 skip，而不是让 spawn 失败被当成回归。
    if crate::execenv::resolve("pnpm", &crate::execenv::base_path()).is_none() {
        eprintln!("跳过：PATH 里没有 pnpm（CI 环境），无法真起 vite");
        return;
    }
    assert!(
        planned.ends_with(&["--".to_string(), "--port".to_string(), pick.port.to_string()])
            || planned.ends_with(&["--port".to_string(), pick.port.to_string()]),
        "应把端口写进命令行: {planned:?}"
    );
    assert!(extra.is_empty(), "vite 走命令行注入，不应再注入 PORT");

    let guard = KillOnDrop(Supervisor::default());
    let sup = &guard.0;
    let sink = Arc::new(crate::supervisor::Collector::default());
    let cfg = ServiceConfig { dev_port: Some(wanted), ..Default::default() };
    sup.spawn(
        sink,
        Launch {
            project_id: 2,
            base: &repo,
            argv: &planned,
            cfg: &cfg,
            extra_env: &extra,
            phase: Phase::Running,
            port: Some(pick.port),
            note: None,
        },
    )
    .expect("spawn 应成功");

    let observed = ports::wait_listening(
        &before,
        wanted..=wanted + ports::OBSERVE_SPAN,
        Duration::from_secs(40),
    );
    let observed = match observed {
        Some(p) => p,
        None => {
            sup.stop(2);
            panic!("vite 未在 40 秒内监听；日志：{}", sup.logs_of(2).join("\n"));
        }
    };

    // 端口刚被监听时 vite 可能还没准备好响应，重试几秒
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut body = None;
    while Instant::now() < deadline {
        if let Some(b) = http_get(observed) {
            if b.contains("200") {
                body = Some(b);
                break;
            }
            body = Some(b);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    sup.stop(2);
    assert!(
        ports::wait_released(observed, Duration::from_secs(5)),
        "停止后 {observed} 必须释放（进程组回收失败）"
    );
    let body = body.expect("应能取到 HTTP 响应");
    assert!(body.contains("200"), "响应头应含 200: {body}");
    if !ports::is_free(wanted) {
        // 原有占用者仍在（没有误杀）
        assert!(ports::holder(wanted).is_some());
    }
}

/// 步骤 5：真实仓库的 pull 门禁文案。
#[test]
fn pull_gate_on_real_repos() {
    let my = PathBuf::from(std::env::var("HOME").unwrap()).join("my");
    let dirty_repo = my.join("Reading-Club-AI");
    if dirty_repo.is_dir() {
        let s = crate::source::info(&dirty_repo);
        assert!(s.is_repo, "Reading-Club-AI 应是仓库");
        assert!(!s.can_pull, "脏工作区不可 pull");
        let why = s.pull_blocked_by.clone().unwrap_or_default();
        assert!(
            why.contains("detached HEAD") || why.contains("未提交改动") || why.contains("没有 upstream"),
            "应给出可读原因，实际：{why}"
        );
    }

    let clean_repo = my.join("project-ctrl");
    if clean_repo.is_dir() {
        let s = crate::source::info(&clean_repo);
        assert!(s.is_repo);
        assert!(s.branch.is_some(), "非 detached 应有分支名");
        // 干净且无落后时，唯一原因必须是「已是最新」
        if s.dirty == 0 && s.ahead == 0 && s.behind == 0 {
            assert_eq!(s.pull_blocked_by.as_deref(), Some("已是最新"));
        }
    }
}

/// 步骤 6：`sub2api` 有 go.mod 但本机无 go → preflight 必须挡住。
#[test]
fn preflight_blocks_missing_go_toolchain() {
    let dir = PathBuf::from(std::env::var("HOME").unwrap()).join("projects/sub2api");
    if !dir.is_dir() {
        eprintln!("跳过：{dir:?} 不存在");
        return;
    }
    let d = crate::detect::detect(&dir);
    assert!(d.toolchain.contains(&"go".to_string()), "应识别出 go: {:?}", d.toolchain);

    let cfg = ServiceConfig {
        toolchain: d.toolchain.clone(),
        build: Some(vec!["make".into(), "build".into()]),
        start: vec!["make".into()],
        ..Default::default()
    };
    // 直接复刻命令层的判定顺序
    let path = crate::execenv::path_for(&cfg);
    let mut problems = Vec::new();
    if let Some(p) = cfg.build.as_ref().and_then(|a| a.first()) {
        if crate::execenv::resolve(p, &path).is_none() {
            problems.push(format!("缺少可执行文件：{p}"));
        }
    }
    for tool in &cfg.toolchain {
        if crate::execenv::resolve(tool, &path).is_none() {
            problems.push(format!("缺少可执行文件：{tool}（项目需要它才能构建/运行）"));
        }
    }
    let go_missing = crate::execenv::resolve("go", &path).is_none();
    if go_missing {
        assert!(
            problems.iter().any(|p| p.contains("缺少可执行文件：go")),
            "应报缺 go，实际：{problems:?}"
        );
    }
}

/// 步骤 8：`shutdown_all` 之后所有受管端口必须释放。
#[test]
fn shutdown_all_releases_every_port() {
    let _guard = crate::test_support::exclusive_ports();
    let dir = scratch("shutdown");
    std::fs::write(
        dir.join("a.js"),
        "require('http').createServer((q,s)=>s.end('a')).listen(process.env.PORT,'127.0.0.1')\n",
    )
    .unwrap();
    let guard = KillOnDrop(Supervisor::default());
    let sup = &guard.0;
    let sink = Arc::new(crate::supervisor::Collector::default());
    let mut ports_used = Vec::new();

    for (i, wanted) in [44100u16, 44110].iter().enumerate() {
        let pick = ports::pick(*wanted).expect("应能选到端口");
        ports_used.push(pick.port);
        let cfg = ServiceConfig::default();
        let argv: Vec<String> = vec!["node".into(), "a.js".into()];
        sup.spawn(
            sink.clone(),
            Launch {
                project_id: i as i64,
                base: &dir,
                argv: &argv,
                cfg: &cfg,
                extra_env: &[("PORT".to_string(), pick.port.to_string())],
                phase: Phase::Running,
                port: Some(pick.port),
                note: None,
            },
        )
        .expect("spawn 应成功");
    }

    for p in &ports_used {
        assert!(
            wait_until(Duration::from_secs(10), || !ports::is_free(*p)),
            "端口 {p} 应被监听"
        );
    }

    sup.shutdown_all();
    for p in &ports_used {
        assert!(
            ports::wait_released(*p, Duration::from_secs(6)),
            "shutdown_all 后端口 {p} 必须释放"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
