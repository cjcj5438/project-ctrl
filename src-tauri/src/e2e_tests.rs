//! 真实端到端：真进程、真端口、真仓库、真外网无关。
//!
//! 这些测试不是纯函数单测，会 spawn 子进程并占用/释放真实端口，所以全部用
//! 「端口已被占用就跳过」的方式表达前置条件，避免在别的机器上误报。

#![cfg(test)]

use crate::ports;
use crate::services::Restart;
use crate::supervisor::{key_of, Phase, SpawnSpec, Supervisor};
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
        // 测试里不保留任何进程：显式传空集合
        self.0.shutdown_all(&std::collections::HashSet::new());
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

/// 构造一份最小 SpawnSpec（env 只有 PATH，避免继承本进程环境）。
fn spec_for(
    project_id: i64,
    name: &str,
    base: &Path,
    argv: Vec<String>,
    port: Option<u16>,
    extra_env: &[(String, String)],
) -> SpawnSpec {
    let mut env = vec![("PATH".to_string(), crate::execenv::base_path())];
    env.extend(extra_env.iter().cloned());
    SpawnSpec {
        key: key_of(project_id, name),
        project_id,
        name: name.to_string(),
        base: base.to_path_buf(),
        argv,
        env,
        phase: Phase::Running,
        step: None,
        port,
        note: None,
        restart: Restart::Never,
        keep_alive: false,
    }
}

/// 步骤 1：8080 被占（本机是 docker）→ 顺延到下一个空闲端口 → 进程真的听新端口 → 停止后释放。
#[test]
fn port_shift_actually_rebinds_and_releases() {
    let _guard = crate::test_support::exclusive_ports();
    // 自己占一个 OS 分配的端口当「冲突源」，不依赖本机 8080 上恰好有 docker。
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

    let before = ports::listening_in(wanted..=wanted + ports::OBSERVE_SPAN);
    let argv: Vec<String> = vec!["node".into(), "server.js".into()];
    let (planned, extra) = ports::plan_argv(wanted, pick.port, &argv, &dir);
    assert_eq!(planned, argv, "无 vite/next 配置时应原样保留 argv");
    assert_eq!(extra, vec![("PORT".to_string(), pick.port.to_string())]);

    let guard = KillOnDrop(Supervisor::default());
    let sup = &guard.0;
    let sink = Arc::new(crate::supervisor::Collector::default());
    sup.spawn(
        sink,
        spec_for(1, "dev", &dir, planned, Some(pick.port), &extra),
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

    sup.stop(&key_of(1, "dev")).expect("应有运行记录");
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
    let wanted = 1420u16;
    let pick = ports::pick(wanted).expect("应能选到端口");
    let before = ports::listening_in(wanted..=wanted + ports::OBSERVE_SPAN);

    let argv: Vec<String> = vec!["pnpm".into(), "dev".into()];
    let (planned, extra) = ports::plan_argv(wanted, pick.port, &argv, &repo);
    // 该测试真的要去起 vite：CI runner 上没有 pnpm 时跳过（不是产品问题）。
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
    let key = key_of(2, "web");
    sup.spawn(sink, spec_for(2, "web", &repo, planned, Some(pick.port), &extra))
        .expect("spawn 应成功");

    let observed = ports::wait_listening(
        &before,
        wanted..=wanted + ports::OBSERVE_SPAN,
        Duration::from_secs(40),
    );
    let observed = match observed {
        Some(p) => p,
        None => {
            sup.stop(&key);
            panic!("vite 未在 40 秒内监听；日志：{}", sup.logs_of(&key).join("\n"));
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
    sup.stop(&key);
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

/// #39：temp 工程 + 真实 `service_preflight` IPC，缺 toolchain 必须挡住。
///
/// 旧写法手拼 problems（自写判定，不调产品代码），本机有 go 时还零断言通过。
/// hermetic 方案：fixture 工程放 `go.mod` + `Makefile`（模拟 sub2api 的根 Makefile），
/// 服务 run 用一个**必然不存在**的二进制（`definitely-not-a-real-binary-xyz`）——
/// 不依赖本机是否装 go：run 缺失恒报；go 缺失与否则按本机实际分叉断言，
/// 两个分支都有非空断言，没有 return 跳过。
#[test]
fn preflight_blocks_missing_go_toolchain() {
    use serde_json::json;
    use tauri::test::{get_ipc_response, INVOKE_KEY};
    use tauri::webview::InvokeRequest;
    use tauri::WebviewWindowBuilder;

    let data = tempfile::tempdir().unwrap();
    let _data_dir = crate::test_support::ScopedDataDir::new(data.path());
    let app = crate::test_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview 应能创建");
    let invoke = |cmd: &str, args: serde_json::Value| {
        let req = InvokeRequest {
            cmd: cmd.to_string(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        let raw = get_ipc_response(&webview, req).expect("ipc 应有响应");
        raw.deserialize::<serde_json::Value>().expect("响应应可反序列化")
    };

    // temp 工程：go.mod（一级子目录也行，这里放根）+ Makefile（模拟 sub2api 形态）。
    let dir = scratch("preflight-go");
    std::fs::write(dir.join("go.mod"), "module example.com/demo\n\ngo 1.22\n").unwrap();
    std::fs::write(dir.join("Makefile"), "build:\n\tgo build ./...\n").unwrap();
    let d = crate::detect::detect(&dir);
    assert!(d.toolchain.contains(&"go".to_string()), "应识别出 go: {:?}", d.toolchain);

    let path = dir.to_string_lossy().into_owned();
    let ids = invoke("add_projects", json!({ "paths": [path], "tags": [] }));
    let id = ids[0].as_i64().unwrap();
    // run 用必然不存在的二进制：preflight 恒报「缺 run」，不依赖本机装没装 go。
    invoke(
        "save_service",
        json!({ "id": id, "spec": { "name": "api", "run": ["definitely-not-a-real-binary-xyz", "build"] } }),
    );

    // 调真实 preflight（产品代码），不是手拼判定。
    let pf = invoke("service_preflight", json!({ "id": id, "name": "api" }));
    let arr = pf.as_array().expect("preflight 应返回数组");
    assert!(
        arr.iter().any(|v| v.as_str().unwrap_or_default().contains("definitely-not-a-real-binary-xyz")),
        "应报缺 run 命令：{pf}"
    );
    // go 分支：按本机实际分叉，两个分支都有断言（无跳过）。
    let go_missing = crate::execenv::resolve("go", &crate::execenv::path_for(None)).is_none();
    if go_missing {
        assert!(
            arr.iter().any(|v| v.as_str().unwrap_or_default().contains("缺少可执行文件：go")),
            "本机无 go 时应报缺 go toolchain：{pf}"
        );
    } else {
        assert!(
            !arr.iter().any(|v| v.as_str().unwrap_or_default().contains("缺少可执行文件：go")),
            "本机有 go 时不应报缺 go：{pf}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 步骤 8：`shutdown_all` 之后所有受管端口必须释放（多服务同时跑）。
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
        let argv: Vec<String> = vec!["node".into(), "a.js".into()];
        sup.spawn(
            sink.clone(),
            spec_for(
                i as i64 + 1,
                "a",
                &dir,
                argv,
                Some(pick.port),
                &[("PORT".to_string(), pick.port.to_string())],
            ),
        )
        .expect("spawn 应成功");
    }

    for p in &ports_used {
        assert!(
            wait_until(Duration::from_secs(10), || !ports::is_free(*p)),
            "端口 {p} 应被监听"
        );
    }

    sup.shutdown_all(&std::collections::HashSet::new());
    for p in &ports_used {
        assert!(
            ports::wait_released(*p, Duration::from_secs(6)),
            "shutdown_all 后端口 {p} 必须释放"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 步骤 9：同一项目的两个服务并行运行，互不影响。
#[test]
fn two_services_of_one_project_run_together() {
    let _guard = crate::test_support::exclusive_ports();
    let dir = scratch("multi");
    std::fs::write(
        dir.join("srv.js"),
        "require('http').createServer((q,s)=>s.end(process.env.WHO||'x')).listen(process.env.PORT,'127.0.0.1')\n",
    )
    .unwrap();
    let guard = KillOnDrop(Supervisor::default());
    let sup = &guard.0;
    let sink = Arc::new(crate::supervisor::Collector::default());

    let mut used = Vec::new();
    for (name, wanted) in [("api", 44200u16), ("worker", 44210)] {
        let pick = ports::pick(wanted).expect("应能选到端口");
        used.push(pick.port);
        sup.spawn(
            sink.clone(),
            spec_for(
                9,
                name,
                &dir,
                vec!["node".into(), "srv.js".into()],
                Some(pick.port),
                &[
                    ("PORT".to_string(), pick.port.to_string()),
                    ("WHO".to_string(), name.to_string()),
                ],
            ),
        )
        .expect("spawn 应成功");
    }
    for (name, (_, port)) in ["api", "worker"].iter().zip([(44200u16, used[0]), (44210, used[1])]) {
        let _ = name;
        assert!(wait_until(Duration::from_secs(10), || !ports::is_free(port)), "端口 {port} 应被监听");
    }
    assert_eq!(sup.active_keys_of_project(9).len(), 2, "两个服务应同时活跃");
    // 各自的日志/端口互不串扰
    assert_eq!(sup.port_of(&key_of(9, "api")), Some(used[0]));
    assert_eq!(sup.port_of(&key_of(9, "worker")), Some(used[1]));

    sup.stop(&key_of(9, "api"));
    assert!(ports::wait_released(used[0], Duration::from_secs(5)));
    // 停掉一个不该影响另一个
    assert!(!ports::is_free(used[1]), "worker 应仍在监听");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 用 temp 工程走一遍「探测 → 计划 → 真起 → 真停」，确认产品路径而不只是内部 API。
///
/// #41(c)：hermetic——旧写法依赖 `~/my/G6-oht-3D` 真实项目与 infra compose；
/// 改为 temp 工程自写 `infra/docker-compose.yml`（postgres+minio）与 pnpm 锁文件。
/// 端口分配不用 `bind(:0)` 释放重抢（TOCTOU：释放后可能被别人抢走），
/// 改用 `ports::pick` 在期望值附近选空闲端口（独占端口锁下无竞争）。
#[test]
fn real_project_detect_plan_start_stop() {
    let _guard = crate::test_support::exclusive_ports();
    // 1) temp 工程模拟 monorepo：infra compose + pnpm 锁文件。
    let base = scratch("realplan-base");
    std::fs::create_dir_all(base.join("infra")).unwrap();
    std::fs::write(
        base.join("infra/docker-compose.yml"),
        "services:\n  postgres:\n    image: postgres:16-alpine\n    ports:\n      - \"5432:5432\"\n  minio:\n    image: minio/minio:latest\n    ports:\n      - \"9000:9000\"\n",
    )
    .unwrap();
    std::fs::write(base.join("package.json"), r#"{"name":"demo","private":true}"#).unwrap();
    std::fs::write(base.join("pnpm-workspace.yaml"), "packages:\n  - apps/*\n").unwrap();
    // 探测必须识别出 compose 栈与 infra 目录
    let d = crate::detect::detect(&base);
    let infra = d
        .compose_stacks
        .iter()
        .find(|c| c.file == "infra/docker-compose.yml")
        .expect("应识别 infra/docker-compose.yml");
    assert!(infra.services.contains(&"postgres".to_string()), "{:?}", infra.services);
    assert!(infra.services.contains(&"minio".to_string()), "{:?}", infra.services);
    assert!(d.toolchain.contains(&"pnpm".to_string()), "monorepo 应识别 pnpm");

    // 2) 计划阶段：compose 服务不参与端口分配，但必须能解析出文件路径
    let spec = crate::services::ServiceSpec::from_json(
        &serde_json::json!({
            "name": "infra",
            "docker": { "files": ["infra/docker-compose.yml"], "services": ["postgres", "minio"] },
            "ports": { "requires": [5432] }
        })
        .to_string(),
    )
    .expect("声明应合法");
    assert!(spec.validate().is_empty(), "{:?}", spec.validate());

    // 3) 真起真停：用一个确定的 node 服务，断言端口、日志、停止后释放
    let dir = scratch("realplan");
    std::fs::write(
        dir.join("srv.js"),
        "console.log('booting');require('http').createServer((q,s)=>s.end('ok')).listen(process.env.PORT,'127.0.0.1')\n",
    )
    .unwrap();
    let guard = KillOnDrop(Supervisor::default());
    let sup = &guard.0;
    // 动态端口直连：直接用 pick 选一个空闲端口并立即 spawn，不释放重抢。
    let port = ports::pick(44600).expect("应能选到空闲端口").port;
    let sink = Arc::new(crate::supervisor::Collector::default());
    sup.spawn(
        sink.clone(),
        spec_for(
            77,
            "real",
            &dir,
            vec!["node".into(), "srv.js".into()],
            Some(port),
            &[("PORT".to_string(), port.to_string())],
        ),
    )
    .expect("spawn");
    let key = key_of(77, "real");
    assert!(wait_until(Duration::from_secs(10), || !ports::is_free(port)), "端口应被监听");
    assert!(wait_until(Duration::from_secs(5), || !sup.logs_of(&key).is_empty()), "应收到日志");
    assert_eq!(sup.logs_of(&key).first().map(String::as_str), Some("booting"));
    sup.stop(&key);
    assert!(ports::wait_released(port, Duration::from_secs(5)), "停止后必须释放端口");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&base);
}

/// 崩溃安全的凭据：spawn 时就落盘，正常停止后清掉；被强杀留下的孤儿在下次启动时**回收**而非接管。
#[test]
fn adopt_credentials_are_written_on_spawn_and_cleared_on_stop() {
    let _guard = crate::test_support::exclusive_ports();
    let data = tempfile::tempdir().unwrap();
    let _data_dir = crate::test_support::ScopedDataDir::new(data.path());

    let dir = scratch("adoptcred");
    std::fs::write(dir.join("a.js"), "setInterval(()=>{},1000)\n").unwrap();
    let guard = KillOnDrop(Supervisor::default());
    let sup = &guard.0;
    let sink = Arc::new(crate::supervisor::Collector::default());
    let pid = sup
        .spawn(sink, spec_for(31, "svc", &dir, vec!["node".into(), "a.js".into()], None, &[]))
        .expect("spawn");

    // spawn 即落盘：App 被 kill -9 时没有退出钩子，只有这份记录能救场
    let recs = crate::localstate::read_adopts();
    assert_eq!(recs.len(), 1, "spawn 后应立即有凭据：{recs:?}");
    assert_eq!(recs[0].pid, pid);
    assert_eq!(recs[0].name, "svc");
    assert!(!recs[0].keep_alive, "默认不保留（此时还不知道用户的决定）");

    // 正常停止后凭据必须消失
    sup.stop(&key_of(31, "svc"));
    assert!(crate::localstate::read_adopts().is_empty(), "停止后凭据应清掉");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 孤儿回收：keep_alive=false 的凭据指向仍存活的进程 → 启动时杀掉并清记录。
#[test]
fn orphan_without_keep_alive_is_reclaimed_not_adopted() {
    let data = tempfile::tempdir().unwrap();
    let _data_dir = crate::test_support::ScopedDataDir::new(data.path());
    let dir = scratch("orphan");
    std::fs::write(dir.join("a.js"), "setInterval(()=>{},1000)\n").unwrap();

    let mut child = std::process::Command::new("node")
        .arg("a.js")
        .current_dir(&dir)
        .spawn()
        .expect("spawn node");
    let pid = child.id();
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let rec = crate::supervisor::AdoptRecord {
        key: "31/svc".into(),
        project_id: 31,
        name: "svc".into(),
        pid,
        pgid: pid as i32,
        argv: vec!["node".into(), "a.js".into()],
        cwd: dir.to_string_lossy().into_owned(),
        started_at,
        port: None,
        keep_alive: false,
    };
    crate::supervisor::verify_adopt(&rec).expect("活进程应通过校验");
    crate::localstate::write_adopt(&rec).unwrap();

    // 走真实的启动回收路径
    let app = crate::test_app();
    crate::commands::adopt_existing(app.handle());

    assert!(crate::localstate::read_adopts().is_empty(), "孤儿凭据应被清掉");
    // 进程应被回收。注意：被杀的**子进程**在 wait 之前是僵尸，`kill -0` 仍返回成功，
    // 所以必须用 `try_wait` 收尸作为「已死」的判据。
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut dead = false;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            dead = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 无论断言成败都先收尸：失败路径直接 panic 会跳过 wait 留下僵尸。
    if !dead {
        let _ = child.kill();
    }
    let _ = child.wait();
    assert!(dead, "孤儿进程应被回收");

    let _ = std::fs::remove_dir_all(&dir);
}

/// #41(d)：temp compose + 本地镜像验证 `ps` 解析（只读被测函数，不启停外部栈）。
///
/// PATH 结论：`execenv::base_path()` 是固定前缀 + HOME 派生，**不 honor 进程 PATH**，
/// 且 `docker::docker_bin()` 经 base_path 解析——本机装了真 docker 时任何 PATH 前置
/// 的假脚本都会被真实现遮蔽（resolve 按前缀顺序命中 /usr/local/bin/docker）。
/// 所以假脚本回放不可行，退到 temp compose + 本地镜像 + 真 daemon。
/// daemon 不可用时测试**明确失败**（不 return 跳过），并在消息里说明原因。
#[test]
fn real_compose_ps_reports_services() {
    // daemon 可用性是硬前置：不可用则明确失败，不要跳过。
    let daemon_ok = std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(daemon_ok, "docker daemon 不可用：本测试需要真 daemon（假脚本被 base_path 遮蔽，见注释）");

    // temp 工程：只用本地已有镜像（postgres:16-alpine），不 pull。
    // ports 用宿主侧临时端口（`127.0.0.1::5432` 由 daemon 随机分配）：既 hermetic
    // （无固定端口冲突），又能覆盖 PublishedPort → ports 字段的解析路径。
    let base = scratch("compose-ps");
    std::fs::write(
        base.join("docker-compose.yml"),
        "services:\n  postgres:\n    image: postgres:16-alpine\n    environment:\n      POSTGRES_PASSWORD: pw\n    ports:\n      - \"127.0.0.1::5432\"\n",
    )
    .unwrap();
    // 探测应识别该 compose 栈（产品路径不断）。
    let d = crate::detect::detect(&base);
    assert!(
        d.compose_stacks.iter().any(|c| c.file == "docker-compose.yml"),
        "应识别 temp compose：{:?}",
        d.compose_stacks
    );

    // 起一个一次性的 postgres（项目名唯一，不污染别的栈）。
    let proj = format!("pc-test-ps-{}", std::process::id());
    let up = std::process::Command::new("docker")
        .args(["compose", "-f", "docker-compose.yml", "-p", &proj, "up", "-d", "postgres"])
        .current_dir(&base)
        .output()
        .expect("应能执行 docker compose up");
    assert!(up.status.success(), "compose up 应成功：{}", String::from_utf8_lossy(&up.stderr));
    // 等容器进入 running（最多 60s：首次解压镜像慢）。
    let spec = crate::services::DockerSpec {
        files: vec!["docker-compose.yml".into()],
        project_name: Some(proj.clone()),
        services: vec!["postgres".into()],
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let ps = loop {
        let ps = crate::docker::ps(&base, &spec).expect("ps 应可执行");
        if ps.iter().any(|c| c.service == "postgres" && c.state == "running") {
            break ps;
        }
        assert!(Instant::now() < deadline, "postgres 应在 60s 内 running（当前：{ps:?}）");
        std::thread::sleep(Duration::from_millis(500));
    };
    let pg = ps.iter().find(|c| c.service == "postgres").expect("应解析出 postgres");
    assert_eq!(pg.state, "running", "{:?}", pg);
    assert!(!pg.ports.is_empty(), "PublishedPort 应被解析进 ports：{pg:?}");
    // 过滤：只要 postgres 时不应返回别的服务
    let other = crate::services::DockerSpec {
        files: vec!["docker-compose.yml".into()],
        project_name: Some(proj.clone()),
        services: vec!["nonexistent-svc".into()],
    };
    assert!(crate::docker::ps(&base, &other).unwrap().is_empty(), "服务名过滤必须生效");

    // 清理：down 掉一次性栈（-v 清掉匿名卷）。
    let _ = std::process::Command::new("docker")
        .args(["compose", "-p", &proj, "down", "-v"])
        .current_dir(&base)
        .output();
    let _ = std::fs::remove_dir_all(&base);
}

/// #41(d)：temp compose（postgres 本地镜像）验证 `logs -f` 进共享 sink。
///
/// 同 ps 测试：daemon 是硬前置，不可用则明确失败，不跳过。
#[test]
fn real_compose_logs_stream_into_shared_sink() {
    let daemon_ok = std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(daemon_ok, "docker daemon 不可用：本测试需要真 daemon（见 ps 测试注释）");

    let base = scratch("compose-logs");
    std::fs::write(
        base.join("docker-compose.yml"),
        "services:\n  postgres:\n    image: postgres:16-alpine\n    environment:\n      POSTGRES_PASSWORD: pw\n",
    )
    .unwrap();
    let proj = format!("pc-test-logs-{}", std::process::id());
    let up = std::process::Command::new("docker")
        .args(["compose", "-f", "docker-compose.yml", "-p", &proj, "up", "-d", "postgres"])
        .current_dir(&base)
        .output()
        .expect("应能执行 docker compose up");
    assert!(up.status.success(), "compose up 应成功：{}", String::from_utf8_lossy(&up.stderr));

    let spec = crate::services::DockerSpec {
        files: vec!["docker-compose.yml".into()],
        project_name: Some(proj.clone()),
        services: vec!["postgres".into()],
    };
    // 等 running 再跟随（logs 对未运行容器会立刻失败）。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ps = crate::docker::ps(&base, &spec).expect("ps 应可执行");
        if ps.iter().any(|c| c.service == "postgres" && c.state == "running") {
            break;
        }
        assert!(Instant::now() < deadline, "postgres 应在 60s 内 running（当前：{ps:?}）");
        std::thread::sleep(Duration::from_millis(500));
    }

    let sup = Supervisor::default();
    let sink = Arc::new(crate::supervisor::Collector::default());
    let logs = crate::docker::DockerLogs::follow(&sup, sink.clone(), &base, &spec, 91, "db")
        .expect("应能开始跟随");
    let key = key_of(91, "db");

    // postgres 启动期日志量足够大，`--tail LOG_CAP` 保证立刻有内容
    let got = wait_until(Duration::from_secs(25), || !sup.logs_of(&key).is_empty());
    let lines = sup.logs_of(&key);
    logs.stop();
    assert!(got, "应收到 compose 日志；当前 {} 行", lines.len());
    let joined = lines.join("\n");
    assert!(
        joined.to_lowercase().contains("postgres") || joined.contains("database"),
        "日志内容应来自 postgres 容器：{}",
        joined.chars().take(400).collect::<String>()
    );
    // 事件通道也要有（前端靠它增量追加，不轮询）。冲刷是按时间的（200ms 一批），
    // stop 后读线程退出、冲刷线程推完尾批才结束，这里等它一下。
    assert!(
        wait_until(Duration::from_secs(3), || !sink.logs.lock().unwrap().is_empty()),
        "应通过 Sink 推送过日志批次"
    );
    let events = sink.logs.lock().unwrap();
    assert!(events.iter().all(|p| p.key == key), "事件必须带正确的服务 key");
    drop(events);

    let _ = std::process::Command::new("docker")
        .args(["compose", "-p", &proj, "down", "-v"])
        .current_dir(&base)
        .output();
    let _ = std::fs::remove_dir_all(&base);
}
