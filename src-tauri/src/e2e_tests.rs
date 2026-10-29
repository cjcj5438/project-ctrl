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
        spawn_cmd: None,
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

/* ========================================================================= *
 * AI 服务配置校验循环：真 node 服务 / 真端口 / 真宿主分发入口
 *
 * 全部用例都从 `ai_tasks::dispatch` 进来——那是 pi 的 `pc-host` 通道打到的那同一个
 * 入口，所以「先停旧 → 写层 → 跑步骤 → 启动 → 等就绪 → 失败回灌 → 自动应用下一轮
 * → 次数用尽回滚 → 采纳/撤销」这条主路径的排序与加锁被真实覆盖。
 * ========================================================================= */
mod verify_loop_e2e {
    use super::{http_get, scratch, wait_until};
    use crate::ai_session::{AiManager, AiSnapshot, ItemBody, SessionHandle};
    use crate::ai_tasks::CardAction;
    use crate::services::{Derived, Descriptor, ServiceSpec};
    use crate::supervisor::{key_of, Phase};
    use serde_json::{json, Value};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::MutexGuard;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use tauri::Manager;

    type MockApp = tauri::App<tauri::test::MockRuntime>;
    type MockHandle = tauri::AppHandle<tauri::test::MockRuntime>;

    /// 每个用例都会写的真服务：`/ok` 200，其余 404。端口由 plan 注入的 `PORT` 决定。
    const SERVER_MJS: &str = r#"// 最小 HTTP 服务：/ok 返回 200，其余 404。端口由 plan 注入的 PORT 或 --port 决定。
import { createServer } from 'node:http';
const i = process.argv.indexOf('--port');
const port = Number(i > 0 ? process.argv[i + 1] : process.env.PORT ?? 0);
createServer((req, res) => {
  if (req.url === '/ok') { res.writeHead(200); res.end('ok'); }
  else { res.writeHead(404); res.end('nope'); }
}).listen(port, '127.0.0.1');
"#;

    /// 每个用例：独占端口锁 + 独立数据目录 + 临时项目目录 + mock app。
    ///
    /// 字段声明顺序即 drop 顺序，所以 `_ports` 放最后：它要等 app/进程/数据目录都收完
    /// 才让出端口锁，否则下一个用例会在旧进程还没死透时抢到端口。
    struct LoopEnv {
        app: MockApp,
        mgr: AiManager,
        pid: i64,
        dir: PathBuf,
        /// 本环境独占的 scratch 根（`home`/`data`/`proj`/`fakepi` 都在它下面）。
        /// Drop 时整棵删掉，避免 `$TMPDIR` 里越积越多。
        root: PathBuf,
        sid: String,
        _data: crate::test_support::ScopedDataDir,
        _home: Option<crate::test_support::ScopedHome>,
        _ports: MutexGuard<'static, ()>,
    }

    impl LoopEnv {
        fn new(tag: &str) -> Self {
            Self::build(tag, false)
        }

        /// 需要 `ScopedHome` 的用例（会走到 `spawn_inner` → `install_assets`）用这个：
        /// 资源与 `~/.project-ctrl` 必须落在临时 HOME 里，绝不碰真机。
        fn new_with_home(tag: &str) -> Self {
            Self::build(tag, true)
        }

        fn build(tag: &str, with_home: bool) -> Self {
            // 锁顺序跟 ipc Harness 一致：HOME → 端口 → 数据目录。
            // 反过来说，先拿端口锁再等 HOME 锁会和 `ipc_external_scan_*`
            // （先 ScopedHome 再 Harness 里的端口锁）构成 ABBA 死锁。
            let root = scratch(tag);
            let home_guard = if with_home {
                let h = root.join("home");
                std::fs::create_dir_all(&h).expect("建临时 HOME");
                // HOME 是进程级环境变量：换掉它会让同时跑的 docker 用例找不到
                // Docker Desktop 的 compose 插件（在 `~/.docker/cli-plugins` 下），
                // `docker compose` 直接变成「unknown command」。把真 HOME 里只读的
                // 相关路径链进来，隔离 pi 资源的同时不误伤并发用例。
                if let Some(real) = std::env::var_os("HOME").map(PathBuf::from) {
                    for dot in [".docker", ".gitconfig"] {
                        let src = real.join(dot);
                        let dst = h.join(dot);
                        if src.exists() && !dst.exists() {
                            let _ = std::os::unix::fs::symlink(&src, &dst);
                        }
                    }
                }
                Some(crate::test_support::ScopedHome::new(&h))
            } else {
                None
            };
            let ports = crate::test_support::exclusive_ports();
            let data = root.join("data");
            std::fs::create_dir_all(&data).expect("建临时数据目录");
            let data_guard = crate::test_support::ScopedDataDir::new(&data);

            let dir = root.join("proj");
            std::fs::create_dir_all(&dir).expect("建项目目录");
            std::fs::write(dir.join("package.json"), "{\"name\":\"x\"}\n").expect("写 package.json");
            std::fs::write(dir.join("server.mjs"), SERVER_MJS).expect("写 server.mjs");

            let app = crate::test_app();
            let mgr = app
                .try_state::<AiManager>()
                .expect("AiManager 应已装配")
                .inner()
                .clone();
            let path = dir.to_string_lossy().into_owned();
            let conn = crate::db::open_db().expect("库应能打开");
            let pid = crate::db::add_project(&conn, &path).expect("项目应能入库");
            drop(conn);
            let snap = mgr.open(pid, &path, "e2e").expect("会话应能打开");
            let sid = snap.session.session_id.clone();
            Self {
                app,
                mgr,
                pid,
                dir,
                root,
                sid,
                _data: data_guard,
                _home: home_guard,
                _ports: ports,
            }
        }

        fn app_handle(&self) -> MockHandle {
            self.app.handle().clone()
        }

        fn sup(&self) -> tauri::State<'_, crate::supervisor::Supervisor> {
            crate::supervisor::state(self.app.handle())
        }

        /// 往 DB 层种一条旧配方（用例的「第一个提案之前的真相」）。
        fn seed(&self, spec: &ServiceSpec) {
            let conn = crate::db::open_db().expect("库应能打开");
            crate::db::insert_service(&conn, self.pid, spec, &Derived::manual())
                .expect("种子服务应能入库");
        }

        fn store_specs(&self) -> Vec<ServiceSpec> {
            let conn = crate::db::open_db().expect("库应能打开");
            crate::db::services_of(&conn, self.pid)
                .expect("读服务表")
                .iter()
                .map(|r| ServiceSpec::from_json(&r.config).expect("库里配方应可解析"))
                .collect()
        }

        fn db_names(&self) -> Vec<String> {
            self.store_specs().iter().map(|s| s.name.clone()).collect()
        }

        /// 在独立线程里走宿主分发入口（`pc-host` 调的就是它），不阻塞驱动线程。
        fn call(&self, method: &'static str, params: Value) -> JoinHandle<Result<Value, String>> {
            let handle = self.mgr.handle(&self.sid).expect("会话句柄");
            host_call(self.app_handle(), handle, method, params)
        }

        fn approve(&self, run_steps: bool) -> JoinHandle<Result<String, String>> {
            approve_when_ready(self.app_handle(), self.sid.clone(), run_steps)
        }

        fn reject(&self, kind: &'static str) -> JoinHandle<Result<String, String>> {
            reject_when_ready(self.app_handle(), self.sid.clone(), kind)
        }

        fn cards(&self, kind: &str) -> Vec<Value> {
            let snap = self.mgr.snapshot(&self.sid).expect("会话快照");
            cards_of(&snap, kind)
        }

        fn last_card(&self, kind: &str) -> Option<Value> {
            self.cards(kind).pop()
        }

        /// 按 `attempt` 找提案/验证卡（这两类卡才有该字段）。
        fn card(&self, kind: &str, attempt: u32) -> Option<Value> {
            self.cards(kind)
                .into_iter()
                .find(|c| c["attempt"].as_u64() == Some(u64::from(attempt)))
        }

        /// 把 pi 指到一个必然失败的桩：`apply_fix` 成功后会 `send_prompt`，那里会
        /// `install_assets()` + `resolve_pi()`；不指桩就会起真 pi 进程（真机装了 pi）。
        fn install_pi_stub(&self) {
            let stub = self.dir.parent().expect("项目目录应有父目录").join("fakepi");
            std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("写桩");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod 桩");
            let settings = crate::ai_settings::settings_path();
            std::fs::create_dir_all(settings.parent().expect("设置目录")).expect("建设置目录");
            std::fs::write(&settings, format!("{{\"pi_path\":\"{}\"}}\n", stub.display()))
                .expect("写 AI 设置");
            crate::ai_settings::clear_cache();
        }
    }

    impl Drop for LoopEnv {
        fn drop(&mut self) {
            // 先收真进程，再让宿主钩子回滚在飞的循环，最后清进程级全局登记。
            crate::supervisor::state(self.app.handle()).shutdown_all(&HashSet::new());
            self.mgr.shutdown_all();
            crate::ai_tasks::forget_loop_for_test(&self.sid);
            crate::ai_settings::clear_cache();

            // 最后删 scratch 根目录。
            // Drop 体在执行时所有字段都还活着（真正析构在体结束之后），所以体内删目录
            // 是安全的；`_ports` 最后声明 → 最后释放，端口锁会在删目录之后才让出，
            // 下一个用例不会在旧目录还没删干净时就抢到端口。
            // 失败时保留现场，方便排查。
            if std::thread::panicking() {
                // 保留现场，方便排查
            } else {
                // 只删本 harness 自己建的目录，别误伤别处。
                let ours = self
                    .root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.starts_with("pc-e2e-"));
                if ours {
                    let _ = std::fs::remove_dir_all(&self.root);
                }
            }
        }
    }

    fn cards_of(snap: &AiSnapshot, kind: &str) -> Vec<Value> {
        snap.items
            .iter()
            .filter_map(|it| match &it.body {
                ItemBody::Card { card } => Some(card.clone()),
                _ => None,
            })
            .filter(|c| c["kind"] == kind)
            .collect()
    }

    fn host_call(
        app: MockHandle,
        handle: SessionHandle,
        method: &'static str,
        params: Value,
    ) -> JoinHandle<Result<Value, String>> {
        std::thread::spawn(move || crate::ai_tasks::dispatch(&app, &handle, method, &params))
    }

    /// 等线程收工（超时即失败），再取它的返回值；线程 panic 原样上抛。
    fn join_within<T>(h: JoinHandle<Result<T, String>>, secs: u64, what: &str) -> Result<T, String> {
        assert!(
            wait_until(Duration::from_secs(secs), || h.is_finished()),
            "{what} 未在 {secs}s 内结束（校验循环可能卡住）"
        );
        h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
    }

    /// 轮询快照，找到 `kind` 的 pending 卡片就投递决定；返回卡片 id。
    fn decide_when_pending(
        app: MockHandle,
        sid: String,
        kind: &'static str,
        decision: Value,
    ) -> JoinHandle<Result<String, String>> {
        std::thread::spawn(move || {
            let mgr = app
                .try_state::<AiManager>()
                .ok_or_else(|| "AiManager 未装配".to_string())?
                .inner()
                .clone();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let snap = mgr.snapshot(&sid)?;
                if let Some(card) = cards_of(&snap, kind)
                    .into_iter()
                    .find(|c| c["state"] == "pending")
                {
                    let id = card["id"]
                        .as_str()
                        .ok_or_else(|| format!("{kind} 卡片缺 id：{card}"))?
                        .to_string();
                    if kind == "proposal" {
                        // 前端直接渲染 before/after 的 JSON 文本：必须是非空且可解析的 JSON。
                        for key in ["before", "after"] {
                            let text = card[key]
                                .as_str()
                                .ok_or_else(|| format!("提案卡缺 {key}：{card}"))?;
                            assert!(!text.trim().is_empty(), "提案卡 {key} 不能为空");
                            serde_json::from_str::<Value>(text)
                                .map_err(|e| format!("提案卡 {key} 不是合法 JSON：{e}"))?;
                        }
                    }
                    match mgr.decide(&sid, &id, decision.clone()) {
                        Ok(()) => return Ok(id),
                        // 卡片已推送、`await_decision` 还没登记等待者：同一张卡重试即可。
                        Err(e) if e.contains("已失效") && Instant::now() < deadline => {}
                        Err(e) => return Err(format!("投递决定失败：{e}")),
                    }
                }
                if Instant::now() >= deadline {
                    return Err(format!("20s 内没有出现待确认的 {kind} 卡片"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    }

    fn approve_when_ready(
        app: MockHandle,
        sid: String,
        run_steps: bool,
    ) -> JoinHandle<Result<String, String>> {
        decide_when_pending(
            app,
            sid,
            "proposal",
            json!({"action": "allow", "run_steps": run_steps}),
        )
    }

    fn reject_when_ready(
        app: MockHandle,
        sid: String,
        kind: &'static str,
    ) -> JoinHandle<Result<String, String>> {
        decide_when_pending(
            app,
            sid,
            kind,
            json!({"action": "deny", "feedback": "换一个"}),
        )
    }

    /// 独占端口锁下挑一个真正空闲的固定端口。
    fn free_port() -> u16 {
        (41_000..42_000)
            .find(|p| crate::ports::is_free(*p))
            .expect("41000–42000 内应能找到空闲端口")
    }

    /// 带路径的 GET（`http_get` 固定打 `/`，要从 `/ok` 取真实响应体）。
    fn http_get_path(port: u16, path: &str) -> Option<String> {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).ok()?;
        if buf.is_empty() {
            None
        } else {
            Some(buf)
        }
    }

    /// 校验循环用的服务规格：真 node 服务 + 固定端口 + 指定健康检查路径。
    fn svc(name: &str, listen: u16, health_path: &str, timeout_ms: u64) -> ServiceSpec {
        ServiceSpec::from_json(&format!(
            r#"{{"name":"{name}","run":["node","server.mjs"],"ports":{{"listen":{listen}}},"health":{{"type":"http","path":"{health_path}","timeout_ms":{timeout_ms}}},"restart":"never"}}"#
        ))
        .unwrap_or_else(|e| panic!("规格应合法：{e}"))
    }

    fn spec_json(spec: &ServiceSpec) -> Value {
        serde_json::to_value(spec).expect("规格应可序列化")
    }

    fn ok_body(port: u16) -> bool {
        http_get_path(port, "/ok").is_some_and(|b| b.contains("ok"))
    }

    /* --------------------------- T1：三次失败 → 回滚 → 采纳 --------------------------- */

    #[test]
    fn verify_loop_fails_three_times_then_rolls_back_to_old_recipe() {
        let env = LoopEnv::new("vloop-rollback");
        env.seed(&svc("legacy", free_port(), "/ok", 1500));
        let api_port = free_port();
        let params = json!({
            "services": [spec_json(&svc("api", api_port, "/nope", 1500))],
            "notes": "先试 api",
        });

        // ① 第 1 次：人工批准。
        let t1 = Instant::now();
        let call = env.call("propose_services", params.clone());
        let approver = env.approve(false);
        let card_id = join_within(approver, 20, "第 1 次批准").expect("应能找到并批准提案卡");
        assert!(card_id.starts_with("prop-"), "提案卡 id 应带 prop- 前缀：{card_id}");
        let first = join_within(call, 30, "第 1 次校验").expect("第 1 次提交应返回 Ok（失败不是错误）");
        let d1 = t1.elapsed();
        assert!(d1 < Duration::from_secs(30), "单次尝试必须 < 30s，实际 {d1:?}");

        assert_eq!(first["verified"], json!(false), "健康检查写 /nope 必须判失败：{first}");
        assert_eq!(first["attempt"], json!(1), "第 1 次应是 attempt=1：{first}");
        assert_eq!(first["remaining"], json!(2), "第 1 次后应剩 2 次：{first}");
        let fails = first["failures"].as_array().expect("failures 必须是数组");
        assert!(!fails.is_empty(), "失败必须带至少一条 failures：{first}");
        assert_eq!(fails[0]["name"], json!("api"), "失败项应是 api：{}", fails[0]);
        for key in ["phase", "note", "preflight", "exit_code"] {
            assert!(fails[0].get(key).is_some(), "失败项必须带 {key}：{}", fails[0]);
        }
        assert!(fails[0]["logs"].is_array(), "失败项 logs 必须是数组：{}", fails[0]);
        assert_eq!(
            env.card("verify", 1).expect("第 1 次应有验证卡")["state"],
            json!("failed"),
            "第 1 次失败后验证卡必须是 failed"
        );

        // ② 第 2 次：失败后自动应用——没有批准线程，也必须在 30s 内自己返回。
        let t2 = Instant::now();
        let second = join_within(env.call("propose_services", params.clone()), 30, "第 2 次校验")
            .expect("第 2 次应自动应用并返回");
        let d2 = t2.elapsed();
        assert!(d2 < Duration::from_secs(30), "第 2 次必须 < 30s（不该等用户确认），实际 {d2:?}");
        assert_eq!(second["verified"], json!(false), "第 2 次仍应失败：{second}");
        assert_eq!(second["attempt"], json!(2));
        assert_eq!(second["remaining"], json!(1));
        assert_eq!(
            env.card("proposal", 2).expect("第 2 次应有提案卡")["state"],
            json!("auto"),
            "第 2 次的提案卡必须是 auto（无需用户确认）"
        );

        // ③ 第 3 次：次数用尽 → 回滚。
        let t3 = Instant::now();
        let third = join_within(env.call("propose_services", params.clone()), 30, "第 3 次校验")
            .expect("第 3 次应自动应用并返回");
        let d3 = t3.elapsed();
        assert!(d3 < Duration::from_secs(30), "第 3 次必须 < 30s，实际 {d3:?}");
        assert_eq!(third["verified"], json!(false));
        assert_eq!(third["exhausted"], json!(true), "第 3 次必须报次数用尽：{third}");
        assert_eq!(
            env.card("proposal", 3).expect("第 3 次应有提案卡")["state"],
            json!("auto")
        );

        assert_eq!(env.db_names(), vec!["legacy".to_string()], "次数用尽必须把旧配方整体还原");
        let key = key_of(env.pid, "api");
        for st in env.sup().statuses() {
            assert!(
                !(st.key == key && matches!(st.phase, Phase::Running | Phase::Crashed)),
                "回滚后 api 不能还在运行/崩溃态：{:?}",
                st.phase
            );
        }
        let rolled = env.card("verify", 3).expect("第 3 次应有验证卡");
        assert_eq!(rolled["state"], json!("rolled_back"), "次数用尽后验证卡必须是 rolled_back");
        assert_eq!(rolled["can_adopt_last"], json!(true), "回滚后必须允许采纳最后一次配方");
        let vid = rolled["id"].as_str().expect("验证卡应有 id").to_string();

        // ④ 采纳最后一次：把失败配方写回生效层（不再启动）。
        crate::ai_tasks::ai_card_action(env.app_handle(), env.sid.clone(), vid, CardAction::AdoptLast)
            .expect("采纳最后一次应成功");
        assert_eq!(env.db_names(), vec!["api".to_string()], "采纳后库里应只剩 api");
        assert_eq!(
            env.card("verify", 3).expect("第 3 次应有验证卡")["state"],
            json!("passed"),
            "采纳后验证卡应转 passed"
        );
        eprintln!("[T1] 三次尝试耗时：{d1:?} / {d2:?} / {d3:?}");
    }

    /* --------------------------- T2：通过 → 撤销 --------------------------- */

    #[test]
    fn verify_loop_pass_writes_db_then_undo_restores() {
        let env = LoopEnv::new("vloop-pass");
        env.seed(&svc("legacy", free_port(), "/ok", 1500));
        let api_port = free_port();
        let call = env.call(
            "propose_services",
            json!({"services": [spec_json(&svc("api", api_port, "/ok", 2000))]}),
        );
        let approver = env.approve(false);
        let prop_id = join_within(approver, 20, "批准").expect("应能批准提案卡");
        assert!(prop_id.starts_with("prop-"));
        let ok = join_within(call, 40, "通过校验").expect("提交应成功");

        assert_eq!(ok["verified"], json!(true), "健康检查 /ok 应通过：{ok}");
        assert_eq!(ok["services"][0]["ok"], json!(true), "services[0] 必须 ok：{ok}");
        let port = ok["services"][0]["port"]
            .as_u64()
            .expect("就绪服务必须带端口") as u16;
        // 真进程真端口：这是「REAL server 真的起来了」的证据。
        assert!(
            wait_until(Duration::from_secs(5), || ok_body(port)),
            "真实服务应在 {port} 上接受连接并回 ok"
        );

        assert_eq!(env.db_names(), vec!["api".to_string()], "通过后库里只剩 api（整组覆盖语义）");
        let v = env.card("verify", 1).expect("应有验证卡");
        assert_eq!(v["state"], json!("passed"));
        assert_eq!(v["can_undo"], json!(true), "通过后必须允许撤销");
        let vid = v["id"].as_str().expect("验证卡 id").to_string();

        crate::ai_tasks::ai_card_action(env.app_handle(), env.sid.clone(), vid, CardAction::Undo)
            .expect("撤销应成功");
        assert_eq!(env.db_names(), vec!["legacy".to_string()], "撤销必须还原旧配方");
        assert!(
            wait_until(Duration::from_secs(5), || http_get(port).is_none()),
            "撤销后 {port} 不应再接受连接"
        );
        assert_eq!(
            env.card("verify", 1).expect("应有验证卡")["state"],
            json!("undone"),
            "撤销后验证卡应是 undone"
        );
    }

    /* --------------------------- T3：描述文件层 --------------------------- */

    #[test]
    fn verify_loop_descriptor_layer_writes_file_not_db() {
        let env = LoopEnv::new("vloop-descriptor");
        let old = svc("oldfile", free_port(), "/ok", 1500);
        crate::descriptor::write(&env.dir, &Descriptor::new(vec![old]), false)
            .expect("应能写描述文件");
        let file = crate::descriptor::path_of(&env.dir);
        let original = std::fs::read(&file).expect("读原始描述文件");
        // 库里放一条完全不同的种子：用来证明描述文件层绝不写库。
        env.seed(&svc("dbseed", free_port(), "/ok", 1500));

        let api_port = free_port();
        let call = env.call(
            "propose_services",
            json!({"services": [spec_json(&svc("api", api_port, "/ok", 2000))]}),
        );
        let approver = env.approve(false);
        join_within(approver, 20, "批准").expect("应能批准提案卡");
        let ok = join_within(call, 40, "通过校验").expect("提交应成功");
        assert_eq!(ok["verified"], json!(true), "描述文件层也应真起服务并通过：{ok}");

        let d = crate::descriptor::read(&env.dir)
            .expect("读描述文件")
            .expect("描述文件应存在");
        assert_eq!(
            d.services.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            vec!["api".to_string()],
            "描述文件必须被整份覆盖成 api"
        );
        assert_eq!(env.db_names(), vec!["dbseed".to_string()], "描述文件层绝不能写数据库");
        assert_eq!(
            env.card("proposal", 1).expect("应有提案卡")["layer"],
            json!("descriptor"),
            "提案卡必须标出 descriptor 层"
        );

        let vid = env.card("verify", 1).expect("应有验证卡")["id"]
            .as_str()
            .expect("验证卡 id")
            .to_string();
        crate::ai_tasks::ai_card_action(env.app_handle(), env.sid.clone(), vid, CardAction::Undo)
            .expect("撤销应成功");
        assert_eq!(
            std::fs::read(&file).expect("读回描述文件"),
            original,
            "撤销必须逐字节还原描述文件"
        );
    }

    /* --------------------------- T4：中止 --------------------------- */

    #[test]
    fn verify_loop_abort_rolls_back_and_stops() {
        let env = LoopEnv::new("vloop-abort");
        env.seed(&svc("legacy", free_port(), "/ok", 1500));
        let api_port = free_port();
        let call = env.call(
            "propose_services",
            json!({"services": [spec_json(&svc("api", api_port, "/nope", 2000))]}),
        );
        let approver = env.approve(false);
        join_within(approver, 20, "批准").expect("应能批准提案卡");

        // 等 api 真的起来（此刻 proposer 正在 wait_ready 里转），再走 shutdown——
        // `AiManager::shutdown_all` 就是用这个方法给在飞的循环封顶的。
        let key = key_of(env.pid, "api");
        assert!(
            wait_until(Duration::from_secs(20), || env
                .sup()
                .status_of(&key)
                .is_some_and(|s| s.phase == Phase::Running)),
            "api 应已启动（说明 proposer 已进入健康检查阶段）"
        );
        let handle = env.mgr.handle(&env.sid).expect("会话句柄");
        crate::ai_tasks::dispatch(env.app.handle(), &handle, "shutdown", &Value::Null)
            .expect("shutdown 应返回 ok");

        let err = join_within(call, 30, "中止后的提交").expect_err("中止必须让提案返回错误");
        // ai_tasks.rs:59 `const ABORTED: &str = "用户已停止"`，abort_loop 原样返回它。
        assert!(err.contains("用户已停止"), "中止的错误必须是 ABORTED：{err}");
        assert_eq!(env.db_names(), vec!["legacy".to_string()], "中止必须按快照回滚到旧配方");
        for st in env.sup().statuses() {
            assert!(
                !(st.key == key && matches!(st.phase, Phase::Running | Phase::Crashed)),
                "中止后 api 不应还在运行/崩溃态：{:?}",
                st.phase
            );
        }
        // abort_loop 把 verify 卡打成 aborted（proposal 卡停在 approved）。
        assert_eq!(
            env.card("verify", 1).expect("应有验证卡")["state"],
            json!("aborted"),
            "中止后验证卡必须是 aborted"
        );
    }

    /* ------------------- T5：诊断 → 应用修复 → 重新验证 ------------------- */

    #[test]
    fn diagnosis_apply_fix_runs_verify_loop_and_reverify_restarts() {
        let env = LoopEnv::new_with_home("vloop-diagnosis");
        env.install_pi_stub();
        let legacy_port = free_port();
        let api_port = free_port();
        env.seed(&svc("legacy", legacy_port, "/ok", 1500));
        env.seed(&svc("api", api_port, "/nope", 1500));
        // 现场：api 真起着但健康检查一直不就绪（进程在跑、就绪超时）。
        crate::commands::start_service(env.app.handle(), env.pid, "api")
            .expect("api 应能启动（health 稍后失败）");
        let key = key_of(env.pid, "api");
        assert!(
            wait_until(Duration::from_secs(10), || env.sup().status_of(&key).is_some()),
            "api 应出现在运行表里"
        );

        let diag = env.call(
            "diagnosis",
            json!({
                "service": "api",
                "summary": "健康检查路径写错",
                "cause": "server.mjs 只对 /ok 回 200，配方里写的是 /nope",
                "category": "config",
                "evidence": [{"file": "server.mjs", "line": 1, "excerpt": "if (req.url === '/ok')"}],
                "suggestion": "把 health.path 改成 /ok",
                "config_fix": [spec_json(&svc("api", api_port, "/ok", 2000))],
            }),
        );
        join_within(diag, 20, "诊断上报").expect("诊断应成功");
        let card = env.last_card("diagnosis").expect("应有诊断卡");
        assert_eq!(card["state"], json!("open"), "新诊断卡必须是 open：{card}");
        assert!(card["config_fix"].is_array(), "config_fix 必须带上：{card}");
        let cid = card["id"].as_str().expect("诊断卡 id").to_string();

        // ApplyFix 在 worker 线程（`ai-apply-{sid}`）里跑完整校验循环。
        crate::ai_tasks::ai_card_action(
            env.app_handle(),
            env.sid.clone(),
            cid.clone(),
            CardAction::ApplyFix { run_steps: false },
        )
        .expect("ApplyFix 应受理");
        assert!(
            wait_until(Duration::from_secs(40), || env
                .last_card("verify")
                .is_some_and(|c| c["state"] == json!("passed"))),
            "ApplyFix 应在 40s 内把 verify 卡推到 passed"
        );
        let specs = env.store_specs();
        let api = specs.iter().find(|s| s.name == "api").expect("库里应有 api");
        assert_eq!(
            api.health.as_ref().and_then(|h| h.path.clone()).as_deref(),
            Some("/ok"),
            "ApplyFix 必须把 /ok 写进库：{specs:?}"
        );
        let port = env.last_card("verify").expect("验证卡")["services"][0]["port"]
            .as_u64()
            .expect("通过的服务应带端口") as u16;
        assert!(wait_until(Duration::from_secs(5), || ok_body(port)), "修复后真实服务应在 {port} 回 ok");
        assert_eq!(
            env.last_card("diagnosis").expect("诊断卡")["state"],
            json!("passed"),
            "成功的修复要把诊断卡标 passed"
        );

        // `apply_fix` 成功时把该诊断记录从 DIAGNOSES 里删掉（ai_tasks.rs:1706），
        // 所以同一张卡再点「重新验证」必然失效——先把真实行为钉住。
        let stale = crate::ai_tasks::ai_card_action(
            env.app_handle(),
            env.sid.clone(),
            cid,
            CardAction::Reverify,
        );
        assert_eq!(
            stale.err().as_deref(),
            Some("这条诊断已失效"),
            "apply_fix 已消费掉这条诊断记录"
        );

        // 让它再次不就绪，并重新诊断（Reverify 需要一条仍然存活的诊断记录）。
        env.seed(&svc("api", api_port, "/nope", 1500));
        crate::commands::stop_service(env.app.handle(), env.pid, "api").expect("停 api");
        let again = env.call(
            "diagnosis",
            json!({
                "service": "api",
                "summary": "仍未就绪",
                "cause": "路径又被改回 /nope",
                "category": "config",
                "evidence": [],
                "suggestion": "改回 /ok",
            }),
        );
        join_within(again, 20, "第二次诊断").expect("第二次诊断应成功");
        let cid2 = env.last_card("diagnosis").expect("第二张诊断卡")["id"]
            .as_str()
            .expect("诊断卡 id")
            .to_string();

        crate::ai_tasks::ai_card_action(
            env.app_handle(),
            env.sid.clone(),
            cid2.clone(),
            CardAction::Reverify,
        )
        .expect("Reverify 应受理");
        assert!(
            wait_until(Duration::from_secs(30), || env
                .last_card("diagnosis")
                .is_some_and(|c| c["state"] == json!("failed"))),
            "重新验证失败后诊断卡必须转 failed"
        );
        // 卡片上的 `rounds` 恒为 0：`reverify` 只把内部 `DiagnosisRec.rounds` 加一
        // （ai_tasks.rs:1822 `rec.rounds += 1`），从不 patch 卡片字段。所以「rounds==1」
        // 在卡片上不可观察，这里断言真实值，并用公共入口证明计数确实涨了。
        assert_eq!(
            env.last_card("diagnosis").expect("诊断卡")["rounds"],
            json!(0),
            "卡片 rounds 不会随 reverify 变化（计数只存在 DiagnosisRec 里）"
        );
        for _ in 0..2 {
            crate::ai_tasks::ai_card_action(
                env.app_handle(),
                env.sid.clone(),
                cid2.clone(),
                CardAction::Reverify,
            )
            .expect("Reverify 应受理");
            assert_eq!(
                env.last_card("diagnosis").expect("诊断卡")["state"],
                json!("failed")
            );
        }
        let err = crate::ai_tasks::ai_card_action(
            env.app_handle(),
            env.sid.clone(),
            cid2,
            CardAction::ContinueDiagnose,
        )
        .expect_err("轮次用尽后必须拒绝继续诊断");
        // ai_tasks.rs:1839 `if rounds >= 3 { return Err("诊断轮次已用尽") }`
        assert!(err.contains("诊断轮次已用尽"), "三轮失败后 rounds 必须是 3：{err}");
    }

    /* ------------------- T6：服务控制卡片必须经用户批准 ------------------- */

    #[test]
    fn service_control_card_requires_approval() {
        let env = LoopEnv::new("vloop-control");
        env.seed(&svc("api", free_port(), "/ok", 2000));
        let key = key_of(env.pid, "api");

        // ① 拒绝：卡片 rejected，服务绝不能被拉起。
        let call = env.call(
            "service_control",
            json!({"service": "api", "action": "restart", "reason": "e2e"}),
        );
        let deny = env.reject("service_control");
        join_within(deny, 20, "拒绝服务控制卡").expect("应能找到并拒绝卡片");
        let out = join_within(call, 20, "拒绝后的 service_control").expect("拒绝应返回 Ok");
        assert_eq!(out["approved"], json!(false), "拒绝必须回 approved=false：{out}");
        assert_eq!(
            env.last_card("service_control").expect("应有控制卡")["state"],
            json!("rejected"),
            "拒绝后控制卡必须是 rejected"
        );
        assert!(
            env.sup().status_of(&key).is_none(),
            "被拒绝时绝不能启动/重启服务（{key}）"
        );

        // ② 允许：卡片 done，服务真的起来了。
        let call = env.call(
            "service_control",
            json!({"service": "api", "action": "restart", "reason": "e2e"}),
        );
        let allow = decide_when_pending(
            env.app_handle(),
            env.sid.clone(),
            "service_control",
            json!({"action": "allow"}),
        );
        join_within(allow, 20, "允许服务控制卡").expect("应能找到并允许卡片");
        let out = join_within(call, 40, "允许后的 service_control").expect("允许应返回 Ok");
        assert_eq!(out["approved"], json!(true), "允许必须回 approved=true：{out}");
        assert!(out["status"].is_object(), "允许后必须带 supervisor 状态：{out}");
        assert!(
            env.sup().status_of(&key).is_some_and(|s| s.phase == Phase::Running),
            "允许后 api 应真的在跑"
        );
    }
}
