//! IPC 级端到端：用 Tauri 的 mock runtime 起真实的命令表，走 `invoke` 通道断言结果。
//!
//! 这里验证的是「前端能不能真的调用到后端并拿到正确形状」——纯 Rust 单测覆盖不到
//! serde 字段名、命令名拼写、`AppHandle` 注入这几类只在 IPC 边界才会暴露的错误。

#![cfg(test)]

use serde_json::{json, Value};
use tauri::test::{get_ipc_response, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewWindowBuilder};

/// 数据库位置是进程级环境变量，并行测试必须串行化，否则彼此会读到对方的库。
static DB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 每个测试用独立的数据目录，避免污染真实的 `~/.project-ctrl`。
///
/// 只负责建目录，不碰环境变量：`set_var` 必须在拿住锁之后做（见 `Harness::new`），
/// 否则等锁期间其他持锁测试会读到这个还没生效完的目录（#74 串库偶发）。
fn temp_data_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pc-ipc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Harness {
    app: tauri::App<tauri::test::MockRuntime>,
    webview: tauri::WebviewWindow<tauri::test::MockRuntime>,
    _data: std::path::PathBuf,
    _db_guard: std::sync::MutexGuard<'static, ()>,
    _port_guard: std::sync::MutexGuard<'static, ()>,
    _data_guard: crate::test_support::ScopedDataDir,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let port_guard = crate::test_support::exclusive_ports();
        let db_guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 先拿锁、再 set：锁内设置保证等锁期间没有中间状态外露。
        // `ScopedDataDir::new` 内部再拿 `DATA_DIR_LOCK`（固定顺序 port→db→data，不死锁），
        // 并在 drop 时恢复环境——所以建目录与 set 必须都在这里按此顺序完成。
        let data = temp_data_dir(tag);
        // 同一把锁保证不与 e2e 里改数据目录的测试并行
        let data_guard = crate::test_support::ScopedDataDir::new(&data);
        let app = crate::test_app();
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("webview 应能创建");
        Self {
            app,
            webview,
            _data: data,
            _db_guard: db_guard,
            _port_guard: port_guard,
            _data_guard: data_guard,
        }
    }

    /// 走真实 IPC 调一个命令。
    fn invoke(&self, cmd: &str, args: Value) -> Result<Value, Value> {
        let req = InvokeRequest {
            cmd: cmd.to_string(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        let raw = get_ipc_response(&self.webview, req)?;
        raw.deserialize::<Value>().map_err(|e| json!(format!("响应反序列化失败：{e}")))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(sup) = self.app.try_state::<crate::supervisor::Supervisor>() {
            sup.shutdown_all(&std::collections::HashSet::new());
        }
        // 收养凭据与本机状态都在数据目录里，随 _data 一起丢弃。
        // 环境恢复由 `_data_guard`（ScopedDataDir 的 Drop）负责：它会恢复旧值
        // 而不是粗暴 remove——这里不再动环境，避免与 guard 的恢复打架。
        let _ = &self._data;
    }
}

/// 样本项目：一个只监听 `PORT` 的最小 HTTP 服务。
fn sample_service(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("server.js"),
        "console.log('booting');const p=process.env.PORT;require('http').createServer((q,s)=>s.end('ok')).listen(p,'127.0.0.1')\n",
    )
    .unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"app","scripts":{"dev":"node server.js"}}"#).unwrap();
}

/// 轮询 `service_logs` 直到非空（5s 上限）：进程输出需要经过读线程→环形缓冲，
/// 起后立刻读可能是空——单次判定会把时序抖动误判成失败。
fn wait_logs_nonempty(h: &Harness, id: i64, name: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let got = h.invoke("service_logs", json!({ "id": id, "name": name })).unwrap();
        let nonempty = got.get("lines").and_then(|a| a.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
        if nonempty || std::time::Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn add_project(h: &Harness, path: &str) -> i64 {
    let ids = h
        .invoke("add_projects", json!({ "paths": [path], "tags": [] }))
        .expect("add_projects 应成功");
    ids[0].as_i64().expect("项目 id")
}

/// 命令名/参数名拼错、字段名漏 serde 转换，都会在这里暴露。
#[test]
fn ipc_contract_roundtrips_all_new_commands() {
    let h = Harness::new("contract");
    let dir = std::env::temp_dir().join(format!("pc-ipc-sample-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    // project_services：字段名必须与前端一致，且探测结果一起返回
    let ps = h.invoke("project_services", json!({ "id": id })).expect("project_services");
    assert_eq!(ps["services"], json!([]), "初始没有服务");
    assert_eq!(ps["descriptor_exists"], false);
    assert_eq!(ps["descriptor_error"], Value::Null);
    assert_eq!(ps["detected"]["package_manager"], "npm");
    assert_eq!(ps["detected"]["start_candidates"][0], json!(["npm", "run", "dev"]));
    assert_eq!(ps["detected"]["dev_port"], Value::Null, "脚本没有 --port 时应为 null");

    // save_service：新模型字段（run / ports.listen）
    let spec = json!({
        "name": "api",
        "run": ["node", "server.js"],
        "ports": { "listen": 45200 }
    });
    h.invoke("save_service", json!({ "id": id, "spec": spec })).expect("save_service");

    let ps = h.invoke("project_services", json!({ "id": id })).expect("读回服务");
    assert_eq!(ps["services"][0]["spec"]["name"], "api");
    assert_eq!(ps["services"][0]["spec"]["run"], json!(["node", "server.js"]));
    assert_eq!(ps["services"][0]["spec"]["ports"]["listen"], 45200);
    assert_eq!(ps["services"][0]["layer"], "db");
    assert_eq!(ps["services"][0]["status"], Value::Null);

    // preflight：本机 npm/node 存在，应返回空数组
    let pf = h
        .invoke("service_preflight", json!({ "id": id, "name": "api" }))
        .expect("preflight");
    assert_eq!(pf, json!([]));

    // 未知字段必须被拒绝（严格 schema）
    let bad = json!({ "name": "bad", "run": ["true"], "restartt": "always" });
    let err = h.invoke("save_service", json!({ "id": id, "spec": bad })).expect_err("应拒绝未知字段");
    assert!(err.to_string().contains("restartt"), "错误应点名字段：{err}");

    // 空服务必须被拒绝
    let empty = json!({ "name": "empty" });
    assert!(h.invoke("save_service", json!({ "id": id, "spec": empty })).is_err());

    // source_info：非仓库
    let si = h.invoke("source_info", json!({ "id": id })).expect("source_info");
    assert_eq!(si["is_repo"], false);
    assert_eq!(si["pull_blocked_by"], "不是 git 仓库");

    // service_status 初始为空数组
    assert_eq!(h.invoke("service_status", json!({})).unwrap(), json!([]));

    // 本机覆盖：设置后读回一致，空覆盖等于删除
    let ov = json!({ "port_offset": 10, "keep_alive": true });
    h.invoke("set_service_override", json!({ "id": id, "name": "api", "patch": ov }))
        .expect("set_service_override");
    let back = h.invoke("service_override", json!({ "id": id, "name": "api" })).unwrap();
    assert_eq!(back["port_offset"], 10);
    assert_eq!(back["keep_alive"], true);
    // 覆盖生效后端口应当是 listen + offset
    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    assert_eq!(ps["services"][0]["port_offset"], 10);
    h.invoke("set_service_override", json!({ "id": id, "name": "api", "patch": null }))
        .expect("清空覆盖");
    assert_eq!(
        h.invoke("service_override", json!({ "id": id, "name": "api" })).unwrap(),
        Value::Null
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 完整启停链路：start → 事件驱动状态 → 端口可用 → logs 有内容 → stop 释放端口。
#[test]
fn ipc_start_stop_and_logs() {
    let h = Harness::new("lifecycle");
    let dir = std::env::temp_dir().join(format!("pc-ipc-life-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    h.invoke(
        "save_service",
        json!({
            "id": id,
            "spec": { "name": "api", "run": ["node", "server.js"], "ports": { "listen": 45210 } }
        }),
    )
    .expect("save");

    let st = h.invoke("service_start", json!({ "id": id, "name": "api" })).expect("service_start");
    assert_eq!(st["phase"], "Running");
    assert_eq!(st["port"], 45210);
    assert_eq!(st["key"], format!("{id}/api"));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut up = false;
    while std::time::Instant::now() < deadline {
        if !crate::ports::is_free(45210) {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        up,
        "服务应在 10 秒内监听 45210；日志：{:?}",
        h.invoke("service_logs", json!({ "id": id, "name": "api" }))
    );

    // #40：logs 必须真有内容（不只是出现在失败信息里）。
    // sample 的 server.js 首行打印 booting（加这一行对其他测试无害：
    // 它们不断言日志内容，logsargs 那个 save 后不起服务、读的是空缓冲）。
    let logs = wait_logs_nonempty(&h, id, "api");
    let lines = logs.get("lines").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    assert!(!lines.is_empty(), "启动后 5 秒内 service_logs 应非空：{logs}");
    assert!(
        lines.iter().any(|l| l.as_str().unwrap_or_default().contains("booting")),
        "应收到启动行：{logs}"
    );

    let all = h.invoke("service_status", json!({})).unwrap();
    assert!(all
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["project_id"] == id && s["phase"] == "Running" && s["name"] == "api"));

    h.invoke("service_stop", json!({ "id": id, "name": "api" })).expect("service_stop");
    assert!(
        crate::ports::wait_released(45210, std::time::Duration::from_secs(5)),
        "停止后 45210 必须释放"
    );

    let after = h.invoke("service_status", json!({})).unwrap();
    let row = after
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["project_id"] == id)
        .cloned()
        .unwrap();
    assert_eq!(row["phase"], "Stopped");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 同一项目的两个服务通过 IPC 同时启动、各自有端口、可分别停止。
#[test]
fn ipc_two_services_same_project() {
    let h = Harness::new("multi");
    let dir = std::env::temp_dir().join(format!("pc-ipc-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    for (name, port) in [("api", 45220), ("worker", 45230)] {
        h.invoke(
            "save_service",
            json!({
                "id": id,
                "spec": { "name": name, "run": ["node", "server.js"], "ports": { "listen": port } }
            }),
        )
        .expect("save");
    }

    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    assert_eq!(ps["services"].as_array().unwrap().len(), 2);

    let a = h.invoke("service_start", json!({ "id": id, "name": "api" })).expect("start api");
    assert_eq!(a["port"], 45220);
    let b = h.invoke("service_start", json!({ "id": id, "name": "worker" })).expect("start worker");
    assert_eq!(b["port"], 45230);

    let all = h.invoke("service_status", json!({})).unwrap();
    let running = all
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["project_id"] == id && s["phase"] == "Running")
        .count();
    assert_eq!(running, 2, "两个服务应同时运行");

    // 只停一个，另一个不受影响
    h.invoke("service_stop", json!({ "id": id, "name": "api" })).expect("stop api");
    assert!(crate::ports::wait_released(45220, std::time::Duration::from_secs(5)));
    assert!(!crate::ports::is_free(45230), "worker 应仍在监听");

    h.invoke("service_stop_all", json!({ "id": id })).expect("stop_all");
    assert!(crate::ports::wait_released(45230, std::time::Duration::from_secs(5)));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `ports.listen` 被占时必须顺延，并把真相写进 port 字段。
#[test]
fn ipc_port_shift_reported() {
    let h = Harness::new("shift");
    let dir = std::env::temp_dir().join(format!("pc-ipc-shift-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    // 先占住 8080：测试自带冲突源，不依赖本机 8080 上恰好有什么。
    // 若 bind 失败，说明本机另有进程占着——顺延场景依然成立（is_free 看 bind+lsof）。
    let holder = std::net::TcpListener::bind("127.0.0.1:8080").ok();
    assert!(
        !crate::ports::is_free(8080),
        "前置条件：8080 必须处于被占状态，否则顺延场景不成立"
    );
    h.invoke(
        "save_service",
        json!({
            "id": id,
            "spec": { "name": "api", "run": ["node", "server.js"], "ports": { "listen": 8080 } }
        }),
    )
    .expect("save");

    let st = h.invoke("service_start", json!({ "id": id, "name": "api" })).expect("service_start");
    let port = st["port"].as_u64().expect("应有端口") as u16;
    // 恒断言顺延：8080 已被占（上面已断言），返回 8080 就是产品 bug。
    assert_ne!(port, 8080, "8080 被占时必须顺延");
    let note = st["note"].as_str().unwrap_or_default().to_string();
    assert!(note.contains("8080"), "note 应说明 8080 被占：{note}");
    assert!(
        note.contains("已改用") && note.contains(&port.to_string()),
        "note 应说明改用了哪个端口：{note}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut listening = false;
    while std::time::Instant::now() < deadline {
        if !crate::ports::is_free(port) {
            listening = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(listening, "报告端口 {port} 必须真的被监听");

    h.invoke("service_stop", json!({ "id": id, "name": "api" })).expect("stop");
    assert!(
        crate::ports::wait_released(port, std::time::Duration::from_secs(5)),
        "停止后 {port} 必须释放"
    );
    drop(holder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 缺工具链时 preflight 必须挡住（用不可能存在的命令模拟）。
#[test]
fn ipc_preflight_blocks_missing_binary() {
    let h = Harness::new("preflight");
    let dir = std::env::temp_dir().join(format!("pc-ipc-pf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    h.invoke(
        "save_service",
        json!({
            "id": id,
            "spec": {
                "name": "api",
                "run": ["node", "server.js"],
                "steps": [{ "name": "build", "argv": ["definitely-not-a-real-binary-xyz", "build"] }]
            }
        }),
    )
    .expect("save");

    let pf = h
        .invoke("service_preflight", json!({ "id": id, "name": "api", "step": "build" }))
        .expect("preflight");
    assert!(
        pf.as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap_or_default().contains("definitely-not-a-real-binary-xyz")),
        "应报缺少该命令：{pf}"
    );

    // 直接 start 缺的可执行文件同样要拒绝
    h.invoke(
        "save_service",
        json!({
            "id": id,
            "spec": { "name": "broken", "run": ["definitely-not-a-real-binary-xyz"] }
        }),
    )
    .expect("save");
    let err = h
        .invoke("service_start", json!({ "id": id, "name": "broken" }))
        .expect_err("应拒绝启动");
    assert!(err.to_string().contains("缺少可执行文件"), "错误文案：{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 步骤只跑一次就结束，phase 从 Step 变 Failed，且不会被自动重启。
#[test]
fn ipc_step_runs_once_and_stops() {
    let h = Harness::new("step");
    let dir = std::env::temp_dir().join(format!("pc-ipc-step-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    h.invoke(
        "save_service",
        json!({
            "id": id,
            "spec": {
                "name": "api",
                "run": ["node", "server.js"],
                "restart": "always",
                "steps": [{ "name": "migrate", "argv": ["node", "-e", "console.log(42)"], "timeout_s": 30 }]
            }
        }),
    )
    .expect("save");

    h.invoke("service_step", json!({ "id": id, "name": "api", "step": "migrate" }))
        .expect("service_step");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut done = false;
    while std::time::Instant::now() < deadline {
        let all = h.invoke("service_status", json!({})).unwrap();
        if let Some(row) = all.as_array().unwrap().iter().find(|s| s["project_id"] == id) {
            if row["phase"] == "Failed" {
                assert_eq!(row["step"], "migrate");
                assert_eq!(row["exit_code"], 0);
                done = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(done, "步骤应在 10 秒内结束并进入 Failed（一次性步骤不再运行）");

    // 步骤不会被自动重启：等超过一次退避窗口后仍是 Failed。
    // 用产品常量表达（backoff(0)=BASE，BASE+BASE 覆盖首个重启窗口）。
    std::thread::sleep(crate::supervisor::BACKOFF_BASE + crate::supervisor::BACKOFF_BASE);
    let all = h.invoke("service_status", json!({})).unwrap();
    let row = all.as_array().unwrap().iter().find(|s| s["project_id"] == id).cloned().unwrap();
    assert_eq!(row["phase"], "Failed");
    assert_eq!(row["restarts"], 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 描述文件优先于数据库：写了 `.project-ctrl.json` 就以文件为准，坏文件要报错而不是静默忽略。
#[test]
fn ipc_descriptor_wins_and_errors_are_visible() {
    let h = Harness::new("descriptor");
    let dir = std::env::temp_dir().join(format!("pc-ipc-desc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    // 数据库里先存一个
    h.invoke(
        "save_service",
        json!({ "id": id, "spec": { "name": "from-db", "run": ["node", "server.js"] } }),
    )
    .expect("save");

    // 写入描述文件后，数据库里的服务不再生效
    std::fs::write(
        dir.join(".project-ctrl.json"),
        r#"{"schema_version":1,"services":[{"name":"from-file","run":["node","server.js"],"ports":{"listen":45300}}]}"#,
    )
    .unwrap();
    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    assert_eq!(ps["descriptor_exists"], true);
    assert_eq!(ps["services"][0]["spec"]["name"], "from-file");
    assert_eq!(ps["services"][0]["layer"], "descriptor");

    // 坏文件必须在 UI 里可见，并提供原文
    std::fs::write(dir.join(".project-ctrl.json"), r#"{"schema_version":1,"services":[{"name":"x","run":["a"],"unknown_key":1}]}"#).unwrap();
    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    let err = ps["descriptor_error"].as_str().unwrap_or_default().to_string();
    assert!(err.contains("unknown_key"), "应点名坏字段：{err}");
    let raw = h.invoke("read_descriptor", json!({ "id": id })).unwrap();
    assert!(raw.as_str().unwrap().contains("unknown_key"));

    // 有描述文件时不允许删数据库里的服务（避免误导）
    let e = h
        .invoke("delete_service", json!({ "id": id, "name": "from-db" }))
        .expect_err("应拒绝");
    assert!(e.to_string().contains(".project-ctrl.json"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 固化为声明：从数据库写进 `.project-ctrl.json`，已存在时不覆盖。
#[test]
fn ipc_solidify_descriptor() {
    let h = Harness::new("solidify");
    let dir = std::env::temp_dir().join(format!("pc-ipc-solid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    h.invoke(
        "save_service",
        json!({ "id": id, "spec": { "name": "api", "run": ["node", "server.js"], "ports": { "listen": 45310 } } }),
    )
    .expect("save");

    let json_text = h
        .invoke("solidify_descriptor", json!({ "id": id, "names": [], "overwrite": false, "confirm": true }))
        .expect("solidify");
    let text = json_text.as_str().unwrap();
    assert!(text.contains("\"api\""), "{text}");
    assert!(dir.join(".project-ctrl.json").is_file());

    // 再固化一次必须拒绝（不静默覆盖）
    let err = h
        .invoke("solidify_descriptor", json!({ "id": id, "names": [], "overwrite": false, "confirm": true }))
        .expect_err("应拒绝覆盖");
    assert!(err.to_string().contains("已存在"), "{err}");

    // 固化后由描述文件接管
    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    assert_eq!(ps["services"][0]["layer"], "descriptor");
    assert_eq!(ps["services"][0]["spec"]["ports"]["listen"], 45310);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 收养链路：退出前写凭据 → 下次启动校验并接管 → 进程已死则清掉记录。
///
/// 这里刻意用**描述文件**声明服务：`adopt_existing` 走 `resolve_all`，
/// 若只查数据库会找不到服务并把凭据当垃圾清掉（真实踩过的 bug）。
///
/// 这是 `keep_alive` 的全部价值所在：关掉 App 后服务继续跑、下次打开接着显示。
#[test]
fn ipc_adopt_roundtrip_after_restart() {
    use crate::localstate;
    use crate::supervisor::{AdoptRecord, SpawnSpec};

    let h = Harness::new("adopt");
    let dir = std::env::temp_dir().join(format!("pc-ipc-adopt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);
    std::fs::write(
        dir.join(".project-ctrl.json"),
        r#"{"schema_version":1,"services":[{"name":"api","run":["node","server.js"]}]}"#,
    )
    .unwrap();

    // 一个真的活着的进程，冒充「上次没被收掉的进程」
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .current_dir(&dir)
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    let rec = AdoptRecord {
        key: format!("{id}/api"),
        project_id: id,
        name: "api".into(),
        pid,
        pgid: pid as i32,
        argv: vec!["sleep".into(), "60".into()],
        cwd: path.clone(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
        port: None,
        keep_alive: true,
        spawn_cmd: None,
    };
    localstate::write_adopt(&rec).expect("写凭据");
    assert_eq!(localstate::read_adopts().len(), 1);

    // 校验通过 → 接管后出现在状态表里
    crate::supervisor::verify_adopt(&rec).expect("刚起的进程应通过校验");
    let sup = h.app.state::<crate::supervisor::Supervisor>().inner().clone();
    let sink = std::sync::Arc::new(crate::supervisor::Collector::default());
    let spec = SpawnSpec {
        key: rec.key.clone(),
        project_id: id,
        name: "api".into(),
        base: dir.clone(),
        argv: rec.argv.clone(),
        env: Vec::new(),
        phase: crate::supervisor::Phase::Running,
        step: None,
        port: None,
        note: Some("已接管".into()),
        restart: crate::services::Restart::Never,
        keep_alive: true,
    };
    sup.adopt(sink, spec, rec.pid, rec.started_at).expect("收养应当成功");
    assert_eq!(
        sup.status_of(&rec.key).map(|s| s.phase),
        Some(crate::supervisor::Phase::Running)
    );

    // 退出登记：本机层勾了 keep_alive 的服务要写盘而不是被杀。
    // 注意保留决定在退出这一刻才判定，所以必须先写 override。
    let mut overrides = localstate::Overrides::default();
    localstate::set_override(
        &mut overrides,
        &path,
        "api",
        Some(&localstate::OverridePatch { keep_alive: Some(Some(true)), ..Default::default() }),
    )
    .unwrap();
    localstate::save_overrides(&overrides).unwrap();
    crate::commands::register_adoptable(&sup);
    let after = localstate::read_adopts();
    assert!(
        after.iter().any(|r| r.key == rec.key && r.keep_alive),
        "勾了保留的服务应在退出后仍然存在凭据：{after:?}"
    );

    // 业务进程死掉后，凭据必须被清掉（不能留下指错进程的记录）
    let mut dead = rec.clone();
    dead.pid = 999_999;
    dead.pgid = 999_999;
    localstate::write_adopt(&dead).unwrap();
    crate::commands::adopt_existing(h.app.handle());
    let left = localstate::read_adopts();
    assert!(left.iter().all(|r| r.pid != 999_999), "死进程的凭据必须清掉：{left:?}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// #38：live 收养路径——手写凭据 + 真进程，走 `adopt_existing` 整条启动路径。
///
/// 与上面的 roundtrip 不同：这里不直调 `sup.adopt`，而是调产品在启动时调的
/// `adopt_existing`——它要过 verify→keep_alive 分支→resolve_all→adopt 全链路。
/// 用普通二进制 sleep（不依赖脚本归一化，与 #2 无关）。
#[test]
fn ipc_adopt_existing_takes_over_live_process() {
    use crate::localstate;
    use crate::supervisor::AdoptRecord;
    use tauri::Manager;

    let h = Harness::new("adoptlive");
    let dir = std::env::temp_dir().join(format!("pc-ipc-adoptlive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);
    // 数据库声明即可（adopt_existing 走 resolve_all，db 层也认）。
    h.invoke("save_service", json!({ "id": id, "spec": { "name": "api", "run": ["sleep", "300"] } }))
        .expect("save");

    // 真进程 + 手写凭据（keep_alive=true 才走收养分支，否则走孤儿回收）。
    // 独立进程组：收养后 Harness::drop 会 shutdown_all（TERM 进程组），
    // 若 sleep 与测试进程同组，TERM 会误杀测试进程自身导致静默死亡。
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("300").current_dir(&dir).process_group(0);
        cmd.spawn().expect("spawn sleep")
    };
    let pid = child.id();
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let rec = AdoptRecord {
        key: format!("{id}/api"),
        project_id: id,
        name: "api".into(),
        pid,
        pgid: pid as i32,
        argv: vec!["sleep".into(), "300".into()],
        cwd: path.clone(),
        started_at,
        port: None,
        keep_alive: true,
        spawn_cmd: None,
    };
    localstate::write_adopt(&rec).expect("写凭据");

    // 走真实启动路径
    crate::commands::adopt_existing(h.app.handle());

    // 收养后：状态 Running + pid 一致 + 凭据仍在（收养成功不删凭据）。
    let sup = h.app.state::<crate::supervisor::Supervisor>().inner().clone();
    let st = sup.status_of(&rec.key).expect("收养后状态必须存在");
    assert_eq!(st.phase, crate::supervisor::Phase::Running, "{st:?}");
    assert_eq!(st.pid, Some(pid), "收养的 pid 必须与凭据一致：{st:?}");
    assert_eq!(st.project_id, id);
    assert_eq!(st.name, "api");
    let left = localstate::read_adopts();
    assert!(
        left.iter().any(|r| r.key == rec.key && r.pid == pid && r.keep_alive),
        "收养成功后凭据应保留：{left:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 外部监督者视图：只读扫 launchd + PM2，并标注是否已被本 App 管理。
///
/// #41(a)：hermetic——用 temp HOME + 自写 plist/PM2 fixture，不读真机；
/// 断言条目非空且 changed_at 与文件 mtime 一致（只读未验 mtime 的旧写法会漏过）。
#[test]
fn ipc_external_scan_is_read_only_and_annotated() {
    // 先拿 HOME 锁，再建 Harness（Harness 拿 port→db→data 锁，固定顺序不死锁）。
    let fake_home = std::env::temp_dir().join(format!("pc-ipc-fakehome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&fake_home);
    let agents = fake_home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    let _home = crate::test_support::ScopedHome::new(&fake_home);
    let h = Harness::new("external");

    // 被管项目：plist 的 cwd 指向它，scan 后应标 managed_project。
    let proj = std::env::temp_dir().join(format!("pc-ipc-extproj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&proj);
    sample_service(&proj);
    let proj_path = proj.to_string_lossy().into_owned();
    let id = add_project(&h, &proj_path);
    h.invoke("save_service", json!({ "id": id, "spec": { "name": "svc", "run": ["node", "x.js"] } }))
        .expect("save");

    // fixture 1：launchd plist（带 Label/cwd/命令）。
    let plist = agents.join("com.test.demo.plist");
    let label = "com.test.demo";
    std::fs::write(
        &plist,
        format!(
            r#"<?xml version="1.0"?><plist version="1.0"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>node</string><string>server.js</string></array><key>WorkingDirectory</key><string>{proj_path}</string><key>KeepAlive</key><true/></dict></plist>"#
        ),
    )
    .unwrap();
    let expect_mtime = std::fs::metadata(&plist)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // fixture 2：PM2 ecosystem（node 求值，cwd 指向另一个 temp 目录）。
    let pm2proj = std::env::temp_dir().join(format!("pc-ipc-pm2proj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&pm2proj);
    std::fs::create_dir_all(&pm2proj).unwrap();
    let eco_dir = fake_home.join("my/PM2");
    std::fs::create_dir_all(&eco_dir).unwrap();
    std::fs::write(
        eco_dir.join("ecosystem.config.js"),
        format!("module.exports={{apps:[{{name:'pm2svc',cwd:'{}',script:'app.js',args:'--x 1'}}]}}", pm2proj.display()),
    )
    .unwrap();

    let list = h.invoke("external_scan", json!({})).unwrap();
    let arr = list.as_array().expect("应是数组");
    // 非空断言：两个 fixture 都必须被扫出来（空数组零断言的旧写法到此为止）。
    assert_eq!(arr.len(), 2, "temp HOME 下应恰好扫出 2 条：{list}");

    let launchd = arr.iter().find(|e| e["source"] == "launchd").expect("应有 launchd 条目");
    assert_eq!(launchd["name"], label);
    assert_eq!(launchd["cwd"], proj_path);
    assert_eq!(launchd["command"], json!(["node", "server.js"]));
    assert_eq!(launchd["managed_project"], proj_path, "cwd 匹配的项目应被标注：{launchd}");
    assert_eq!(launchd["declared"], false, "同名服务不存在时 declared 应为 false：{launchd}");
    assert_eq!(
        launchd["changed_at"], expect_mtime,
        "changed_at 必须等于 plist 文件 mtime：{launchd}"
    );
    assert!(launchd["config_path"].as_str().unwrap_or_default().ends_with("com.test.demo.plist"));

    let pm2 = arr.iter().find(|e| e["source"] == "pm2").expect("应有 pm2 条目");
    assert_eq!(pm2["name"], "pm2svc");
    assert_eq!(pm2["command"], json!(["app.js", "--x", "1"]));
    assert!(pm2["managed_project"].is_null(), "未加入管理的目录不应标注：{pm2}");

    // 只读：扫完 plist 与 ecosystem 原样不动。
    let plist_after = std::fs::read_to_string(&plist).unwrap();
    assert!(plist_after.contains(label), "扫描不应改写 plist");

    // reveal_file 对不存在的路径必须拒绝（不能变成任意路径 open）
    let err = h
        .invoke("reveal_file", json!({ "path": "/definitely/not/here.plist" }))
        .expect_err("应拒绝");
    assert!(err.to_string().contains("不存在"), "{err}");

    let _ = std::fs::remove_dir_all(&fake_home);
    let _ = std::fs::remove_dir_all(&proj);
    let _ = std::fs::remove_dir_all(&pm2proj);
}

/// 批量探测 + 批量采纳 + 临时运行：三条「快速关联」路径的行为契约。
#[test]
fn ipc_probe_adopt_and_adhoc_paths() {
    let h = Harness::new("probe");
    let dir = std::env::temp_dir().join(format!("pc-ipc-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);

    // 1) 批量探测：只读，不写库
    let probes = h.invoke("probe_all", json!({})).unwrap();
    let mine = probes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["project_id"] == id)
        .cloned()
        .expect("应包含该项目");
    assert_eq!(mine["has_services"], false);
    assert_eq!(mine["candidates"][0]["kind"], "script");
    assert_eq!(mine["candidates"][0]["spec"]["run"], json!(["npm", "run", "dev"]));
    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    assert_eq!(ps["services"], json!([]), "探测本身不应写库");

    // 2) 批量采纳到本机层
    let spec = mine["candidates"][0]["spec"].clone();
    let saved = h
        .invoke("adopt_candidates", json!({ "items": [[id, spec]] }))
        .unwrap();
    assert_eq!(saved, json!([spec["name"].clone()]));
    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    assert_eq!(ps["services"].as_array().unwrap().len(), 1);
    assert_eq!(ps["services"][0]["layer"], "db");
    assert!(!dir.join(".project-ctrl.json").exists(), "批量采纳不得写仓库文件");

    // 3) 临时运行：不写库，直接起进程
    let adhoc = json!({
        "name": "adhoc",
        "run": ["node", "server.js"],
        "ports": { "listen": 45411 }
    });
    let st = h.invoke("run_adhoc", json!({ "id": id, "spec": adhoc })).expect("run_adhoc");
    assert_eq!(st["phase"], "Running");
    assert_eq!(st["name"], "adhoc");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut up = false;
    while std::time::Instant::now() < deadline {
        if !crate::ports::is_free(45411) {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(up, "临时运行也应真的监听端口");
    // 数据库里并没有它
    let ps = h.invoke("project_services", json!({ "id": id })).unwrap();
    assert!(
        !ps["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["spec"]["name"] == "adhoc"),
        "临时运行不得落库"
    );
    // 但没有 run 声明的不能临时运行
    let steps_only = json!({ "name": "x", "steps": [{ "name": "a", "argv": ["true"] }] });
    assert!(h.invoke("run_adhoc", json!({ "id": id, "spec": steps_only })).is_err());

    h.invoke("service_stop", json!({ "id": id, "name": "adhoc" })).ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 外部视图的「同步」语义：每次调用都重读文件，且能报出与声明的命令漂移。
///
/// #41(b)：hermetic——temp HOME + 自写 plist fixture，不依赖真机 launchd 条目；
/// 逻辑与旧版一致（声明不同命令→报 drift，改一致→drift 消失），不断言跳过。
#[test]
fn ipc_external_scan_reports_drift_and_reloads() {
    let fake_home = std::env::temp_dir().join(format!("pc-ipc-dhome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&fake_home);
    let agents = fake_home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    let _home = crate::test_support::ScopedHome::new(&fake_home);
    let h = Harness::new("drift");

    // 自写 plist：cwd 指向 temp 项目，命令固定。
    let proj = std::env::temp_dir().join(format!("pc-ipc-dproj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&proj);
    sample_service(&proj);
    let cwd = proj.to_string_lossy().into_owned();
    let name = "com.test.drift";
    std::fs::write(
        agents.join("com.test.drift.plist"),
        format!(
            r#"<?xml version="1.0"?><plist version="1.0"><dict><key>Label</key><string>{name}</string><key>ProgramArguments</key><array><string>node</string><string>server.js</string></array><key>WorkingDirectory</key><string>{cwd}</string></dict></plist>"#
        ),
    )
    .unwrap();

    // 首扫：fixture 必须在，且此时无声明、无 drift。
    let first = h.invoke("external_scan", json!({})).unwrap();
    let sample = first
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == name && e["source"] == "launchd")
        .cloned()
        .expect("fixture plist 应被扫出");
    assert_eq!(sample["declared"], false);
    assert_eq!(sample["drift"], Value::Null);

    // 把该目录收进管理器，并声明一个与外部命令不同的同名服务
    let id = add_project(&h, &cwd);
    h.invoke(
        "save_service",
        json!({ "id": id, "spec": { "name": name, "run": ["node", "totally-different.js"] } }),
    )
    .expect("save");

    let second = h.invoke("external_scan", json!({})).unwrap();
    let hit = second
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == json!(name) && e["source"] == "launchd")
        .cloned()
        .expect("同一 label 应仍在结果里");
    assert_eq!(hit["declared"], true, "同名且 cwd 匹配应标为已声明：{hit}");
    assert_eq!(hit["drift"]["kind"], "command-mismatch", "{hit}");
    assert!(
        hit["drift"]["detail"].as_str().unwrap_or_default().contains("totally-different.js"),
        "{hit}"
    );

    // 不缓存：把声明改成与外部一致后，drift 必须消失
    let external_cmd: Vec<String> = hit["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    h.invoke(
        "save_service",
        json!({ "id": id, "spec": { "name": name, "run": external_cmd } }),
    )
    .expect("save");
    let third = h.invoke("external_scan", json!({})).unwrap();
    let hit2 = third
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == json!(name) && e["source"] == "launchd")
        .cloned()
        .unwrap();
    assert_eq!(hit2["drift"], Value::Null, "一致后不该再报漂移：{hit2}");

    let _ = std::fs::remove_dir_all(&fake_home);
    let _ = std::fs::remove_dir_all(&proj);
}

/// 退出时必须停掉 compose 日志跟随，否则每次 Cmd+Q 都漏一个 `docker compose logs -f`。
///
/// 这是实测过的失败模式：父进程退出后子进程被 launchd 收养（PPID 变 1）继续跑。
#[test]
fn ipc_exit_stops_all_docker_log_followers() {
    use crate::docker::DockerLogs;
    use crate::services::DockerSpec;
    use tauri::Manager;

    let h = Harness::new("dockerlogs");
    let dir = tempfile::tempdir().unwrap();
    // #10：不用真 docker——长命假脚本冒充日志流，is_alive 判定才可控。
    // 旧写法注释说不用真 docker、实际调 follow（真 docker_bin），名实不符。
    let script = dir.path().join("fake-docker");
    std::fs::write(&script, "#!/bin/sh\nsleep 300\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let bin = script.to_string_lossy().into_owned();
    // #18 终检要求 -f 文件真实存在：假脚本不读它，建空文件过检即可。
    std::fs::write(dir.path().join("x.yml"), "services: {}\n").unwrap();
    let spec = DockerSpec { files: vec!["x.yml".into()], project_name: None, services: vec![] };
    let sup = h.app.state::<crate::supervisor::Supervisor>().inner().clone();
    let sink = std::sync::Arc::new(crate::supervisor::Collector::default());

    // 直接构造两条流（不依赖 docker 是否安装）
    let mut logs = Vec::new();
    for name in ["a", "b"] {
        let l = DockerLogs::follow_with_bin(&sup, sink.clone(), dir.path(), &spec, 77, name, bin.clone())
            .expect("应能开始跟随");
        assert!(l.is_alive(), "刚起的跟随进程应存活");
        logs.push(l);
    }

    // 登记进 registry（模拟 UI 展开时的行为）
    {
        let reg = h.app.state::<crate::commands::DockerLogsRegistry>();
        let mut g = reg.0.lock().unwrap();
        for (i, l) in logs.into_iter().enumerate() {
            let pid = l.pid();
            g.insert(format!("77/{}", i), l);
            assert!(pid_is_running(pid), "登记后进程应还在");
        }
        assert_eq!(g.len(), 2);
    }

    // 记下 pid 以便断言「进程真的死了」，而不只是登记表被清空
    let mut pids = Vec::new();
    {
        let reg = h.app.state::<crate::commands::DockerLogsRegistry>();
        let g = reg.0.lock().unwrap();
        for l in g.values() {
            pids.push(l.pid());
        }
    }
    assert_eq!(pids.len(), 2);

    // 模拟退出钩子里的那一步
    h.app.state::<crate::commands::DockerLogsRegistry>().stop_all();

    let reg = h.app.state::<crate::commands::DockerLogsRegistry>();
    assert_eq!(reg.len(), 0, "退出后登记表必须清空");
    for pid in pids {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut dead = false;
        while std::time::Instant::now() < deadline {
            if !pid_is_running(pid) {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(dead, "退出后日志跟随进程 {pid} 必须被回收，不能留给 launchd");
    }
}

/// 关键细节：`docker compose` 的日志跟随有两层进程（CLI + 插件）。
/// 只 kill 第一层会留下插件继续跟随，所以必须按**进程组**停。
#[test]
fn ipc_docker_log_group_stops_child_process_too() {
    use crate::docker::DockerLogs;
    use crate::services::DockerSpec;
    use tauri::Manager;

    let h = Harness::new("logptree");
    let dir = tempfile::tempdir().unwrap();
    // #37：`sleep 300 & wait` 保证父子两层都稳定存在（旧 `sleep 300` 是竞态：
    // sh 启动 sleep 需要时间，单次 ps 快照若在 fork 前抓拍就抓不到子进程，
    // for 零断言、测试空测通过）。
    let script = dir.path().join("fake-docker");
    std::fs::write(&script, "#!/bin/sh\nsleep 300 & wait\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    // #18 终检要求 -f 文件真实存在：假脚本不读它，建空文件过检即可。
    std::fs::write(dir.path().join("x.yml"), "services: {}\n").unwrap();
    let spec = DockerSpec { files: vec!["x.yml".into()], project_name: None, services: vec![] };
    let sup = h.app.state::<crate::supervisor::Supervisor>().inner().clone();
    let sink = std::sync::Arc::new(crate::supervisor::Collector::default());

    let logs = DockerLogs::follow_with_bin(
        &sup,
        sink,
        dir.path(),
        &spec,
        88,
        "grp",
        script.to_string_lossy().into_owned(),
    )
    .expect("应能开始跟随");
    let pid = logs.pid();
    assert!(pid_is_running(pid), "跟随进程应存活");

    // 轮询等子进程出现（2s 上限）：fork 需要时间，单次快照会抓空。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let child: Vec<u32> = loop {
        let found = std::process::Command::new("/bin/ps")
            .args(["-o", "pid=,pgid=", "-A"])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| {
                        let mut it = l.split_whitespace();
                        let p: u32 = it.next()?.parse().ok()?;
                        let g: i32 = it.next()?.parse().ok()?;
                        (g == pid as i32 && p != pid).then_some(p)
                    })
                    .collect::<Vec<u32>>()
            })
            .unwrap_or_default();
        if !found.is_empty() || std::time::Instant::now() >= deadline {
            break found;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    // 非空断言：没抓到子进程就是前置失败，不能空测通过。
    assert!(!child.is_empty(), "假脚本应有 sleep 子进程（2s 内未出现）：跟随 pid={pid}");

    DockerLogs::stop(logs);

    // 父+子全死才算过（轮询代替固定 500ms）。
    for p in std::iter::once(pid).chain(child.iter().copied()) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut dead = false;
        while std::time::Instant::now() < deadline {
            if !pid_is_running(p) {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(dead, "stop 后进程 {p} 必须退出（否则会留下孤儿）");
    }
}

/// 进程是否真的还活着。
///
/// #78：僵尸（stat 以 Z 开头）必须视为**活着/收割中**，而不是死了——
/// 旧写法把 Z 视为死，父进程漏 wait 时测试会被蒙混过去（进程表里还有它）。
/// 要求 `ps -o stat=` 输出空（进程彻底没了）才算死；调用点一律轮询而非单次判定，
/// 给父进程留出 wait 收尸的时间（stop 后短暂 Z 是正常的，轮询等它消失）。
fn pid_is_running(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// 前端 `api.serviceLogs` 实际发的形状是 `{ id, name }`：改名后必须仍被接受。
///
/// 退化史：`service_logs` 的 Rust 参数曾叫 `project_id`（Tauri 按 camelCase 读成
/// `projectId`），前端发 `id` 导致缺参报错；Service.tsx 的 docker effect 里这行在
/// `dockerLogs` 之前，catch 又吞错，结果是跟随进程根本没起、面板永远 0 行。
#[test]
fn ipc_service_logs_accepts_frontend_arg_shape() {
    let h = Harness::new("logsargs");
    let dir = std::env::temp_dir().join(format!("pc-ipc-logsargs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);
    h.invoke(
        "save_service",
        json!({ "id": id, "spec": { "name": "api", "run": ["node", "server.js"] } }),
    )
    .expect("save");
    // exact shape sent by src/api.ts `serviceLogs(id, name)`
    let got = h
        .invoke("service_logs", json!({ "id": id, "name": "api" }))
        .expect("前端形状 { id, name } 必须被 service_logs 接受");
    // #75：save 后未启动，缓冲是空的——精确断言空数组（is_array 太弱，非空数组也能过）。
    assert_eq!(got.get("lines").and_then(|a| a.as_array()).cloned().unwrap_or_default(), Vec::<Value>::new(), "未启动的服务日志应为空：{got}");
    assert_eq!(got.get("next").and_then(|n| n.as_u64()), Some(0), "空快照 next 应为 0：{got}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// compose 跟随输出不足一批（<200 行）时也必须 promptly 送达 UI。
///
/// 退化史：docker pump 只在攒满 200 行或 EOF 时 emit；`logs -f` 在吐完历史后会
/// 阻塞等新行（不 EOF），输出少的容器（如 postgres 的 checkpoint 日志）第一批
/// 永远到不了前端，面板卡在 0 行。
#[test]
fn ipc_docker_logs_small_output_reaches_ui_promptly() {
    use crate::docker::DockerLogs;
    use crate::services::DockerSpec;
    use tauri::Manager;

    let h = Harness::new("logflush");
    let dir = tempfile::tempdir().unwrap();
    // 冒充 `docker compose logs -f`：吐 3 行后阻塞（等新日志，不退出）
    let script = dir.path().join("fake-docker");
    std::fs::write(&script, "#!/bin/sh\necho one\necho two\necho three\nsleep 300\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    // #18 终检要求 -f 文件真实存在：假脚本不读它，建空文件过检即可。
    std::fs::write(dir.path().join("x.yml"), "services: {}\n").unwrap();
    let spec = DockerSpec { files: vec!["x.yml".into()], project_name: None, services: vec![] };
    let sup = h.app.state::<crate::supervisor::Supervisor>().inner().clone();
    let sink = std::sync::Arc::new(crate::supervisor::Collector::default());
    let logs = DockerLogs::follow_with_bin(
        &sup,
        sink.clone(),
        dir.path(),
        &spec,
        88,
        "tiny",
        script.to_string_lossy().into_owned(),
    )
    .expect("应能开始跟随");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let got = loop {
        let n: usize = sink.logs.lock().unwrap().iter().map(|p| p.lines.len()).sum();
        if n >= 3 {
            break n;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "3 行输出应在 3 秒内送达 UI（实际只收到 {n} 行）：攒批不能等满 200 行"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(got, 3);
    // 内容必须有序到达（不丢不乱）
    let lines: Vec<String> =
        sink.logs.lock().unwrap().iter().flat_map(|p| p.lines.clone()).collect();
    assert_eq!(lines, vec!["one".to_string(), "two".to_string(), "three".to_string()]);
    // #75：stop 后跟随进程必须真死（轮询），不能只停了读线程留着 sleep 孤儿。
    let pid = logs.pid();
    DockerLogs::stop(logs);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut dead = false;
    while std::time::Instant::now() < deadline {
        if !pid_is_running(pid) {
            dead = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(dead, "stop 后跟随进程 {pid} 必须退出");
}

/// #22 三态经 IPC 必须成立：`{"port_offset": null}` 删键，不是不变。
///
/// 退化史：serde 默认把 JSON `null` 反成外层 `None`（= 不变），`Some(None)`
/// （= 删键）需要自定义 `deserialize_with`。单测只覆盖了 merge 层，IPC 层的
/// 反序列化断裂直到真机联调才暴露（偏移清零无效）。
#[test]
fn ipc_override_null_deletes_key() {
    let h = Harness::new("tmpnull");
    let dir = std::env::temp_dir().join(format!("pc-ipc-ovenull-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sample_service(&dir);
    let path = dir.to_string_lossy().into_owned();
    let id = add_project(&h, &path);
    h.invoke(
        "save_service",
        json!({ "id": id, "spec": { "name": "api", "run": ["node", "server.js"] } }),
    )
    .expect("save");
    // exact frontend shape: set then clear via explicit null
    h.invoke("set_service_override", json!({ "id": id, "name": "api", "patch": { "port_offset": 5 } }))
        .expect("set");
    let back = h.invoke("service_override", json!({ "id": id, "name": "api" })).unwrap();
    assert_eq!(back["port_offset"], 5);
    h.invoke(
        "set_service_override",
        json!({ "id": id, "name": "api", "patch": { "port_offset": null } }),
    )
    .expect("clear");
    let back2 = h.invoke("service_override", json!({ "id": id, "name": "api" })).unwrap();
    assert!(
        back2.get("port_offset").is_none() || back2["port_offset"].is_null(),
        "null must delete key: {back2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
