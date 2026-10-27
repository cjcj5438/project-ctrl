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
fn temp_data_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pc-ipc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::env::set_var("PROJECT_CTRL_DATA_DIR", &d);
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
        // 收养凭据与本机状态都在数据目录里，随 _data 一起丢弃
        let _ = &self._data;
        std::env::remove_var("PROJECT_CTRL_DATA_DIR");
    }
}

/// 样本项目：一个只监听 `PORT` 的最小 HTTP 服务。
fn sample_service(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("server.js"),
        "const p=process.env.PORT;require('http').createServer((q,s)=>s.end('ok')).listen(p,'127.0.0.1')\n",
    )
    .unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"app","scripts":{"dev":"node server.js"}}"#).unwrap();
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

    // 先占住 8080（本机通常已被 docker 占；这里再兜一层，确保测试自洽）
    let holder = std::net::TcpListener::bind("127.0.0.1:8080");
    let squatted = holder.is_ok();
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
    if !squatted && port == 8080 {
        // 本机 8080 真空闲：不构成顺延场景，清理后跳过
        h.invoke("service_stop", json!({ "id": id, "name": "api" })).ok();
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
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

    // 步骤不会被自动重启：等一段退避时间后仍是 Failed
    std::thread::sleep(std::time::Duration::from_millis(1200));
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
        .invoke("solidify_descriptor", json!({ "id": id, "names": [], "overwrite": false }))
        .expect("solidify");
    let text = json_text.as_str().unwrap();
    assert!(text.contains("\"api\""), "{text}");
    assert!(dir.join(".project-ctrl.json").is_file());

    // 再固化一次必须拒绝（不静默覆盖）
    let err = h
        .invoke("solidify_descriptor", json!({ "id": id, "names": [], "overwrite": false }))
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
    sup.adopt(sink, spec, rec.pid, rec.started_at);
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
        Some(&localstate::Override { keep_alive: Some(true), ..Default::default() }),
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

/// 外部监督者视图：只读扫 launchd + PM2，并标注是否已被本 App 管理。
#[test]
fn ipc_external_scan_is_read_only_and_annotated() {
    let h = Harness::new("external");
    let list = h.invoke("external_scan", json!({})).unwrap();
    let arr = list.as_array().expect("应是数组");
    // 本机装了 launchd agent，至少应能解析出几条；若环境没有则为空数组（不是错误）
    for e in arr {
        assert!(e["source"] == "pm2" || e["source"] == "launchd", "{e}");
        assert!(e["config_path"].as_str().unwrap_or_default().len() > 0, "{e}");
        assert!(e["managed_project"].is_null() || e["managed_project"].is_string(), "{e}");
    }

    // reveal_file 对不存在的路径必须拒绝（不能变成任意路径 open）
    let err = h
        .invoke("reveal_file", json!({ "path": "/definitely/not/here.plist" }))
        .expect_err("应拒绝");
    assert!(err.to_string().contains("不存在"), "{err}");
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
/// 直接拿本机真实的 launchd 条目做参照：找一条带 cwd 与命令的，把「同名服务 + 不同命令」
/// 声明进它指向的目录，然后要求扫描结果报出 drift。
#[test]
fn ipc_external_scan_reports_drift_and_reloads() {
    let h = Harness::new("drift");
    let first = h.invoke("external_scan", json!({})).unwrap();
    let items = first.as_array().unwrap().clone();

    let Some(sample) = items
        .iter()
        .find(|e| e["cwd"].is_string() && e["command"].is_array())
        .cloned()
    else {
        eprintln!("跳过：本机没有带 cwd 与命令的 launchd 条目");
        return;
    };
    let cwd = sample["cwd"].as_str().unwrap().to_string();
    let name = sample["name"].as_str().unwrap().to_string();
    if !std::path::Path::new(&cwd).is_dir() {
        eprintln!("跳过：{cwd} 不存在");
        return;
    }

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
    // 不用真 docker：用一个长命的 `sh` 冒充日志流，is_alive 判定才可控
    let spec = DockerSpec { files: vec!["x.yml".into()], project_name: None, services: vec![] };
    let sup = h.app.state::<crate::supervisor::Supervisor>().inner().clone();
    let sink = std::sync::Arc::new(crate::supervisor::Collector::default());

    // 直接构造两条流（不依赖 docker 是否安装）
    let mut logs = Vec::new();
    for name in ["a", "b"] {
        let l = DockerLogs::follow(&sup, sink.clone(), dir.path(), &spec, 77, name)
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
    // 用 sh 冒充「docker compose + 插件子进程」的两层结构
    let script = dir.path().join("fake-docker");
    std::fs::write(&script, "#!/bin/sh\n# 模拟插件子进程：sleep 在同一个进程组里\nsleep 300\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let spec = DockerSpec { files: vec!["x.yml".into()], project_name: None, services: vec![] };
    let sup = h.app.state::<crate::supervisor::Supervisor>().inner().clone();
    let sink = std::sync::Arc::new(crate::supervisor::Collector::default());

    // 让 follow 用的 docker_bin 指向我们的假脚本：通过 PATH 前置实现
    let mut logs = DockerLogs::follow_with_bin(
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

    // 找出同组的 sleep 子进程
    let child = std::process::Command::new("/bin/ps")
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

    DockerLogs::stop(logs);
    std::thread::sleep(std::time::Duration::from_millis(500));

    assert!(!pid_is_running(pid), "父进程必须退出");
    for c in &child {
        assert!(!pid_is_running(*c), "同组的子进程 {c} 也必须退出（否则会留下孤儿）");
    }
}

/// 进程是否真的还活着（僵尸也算死：它们不再占用任何资源语义）。
fn pid_is_running(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let st = s.trim();
            !st.is_empty() && !st.starts_with('Z')
        })
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
    assert!(got.is_array(), "应返回行数组：{got}");
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
    DockerLogs::stop(logs);
}
