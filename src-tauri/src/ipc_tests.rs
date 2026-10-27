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
}

impl Harness {
    fn new(tag: &str) -> Self {
        let port_guard = crate::test_support::exclusive_ports();
        let db_guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = temp_data_dir(tag);
        let app = crate::test_app();
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("webview 应能创建");
        Self { app, webview, _data: data, _db_guard: db_guard, _port_guard: port_guard }
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
            sup.shutdown_all();
        }
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

    // detect_service：字段名必须与前端 `DetectResult` 一致
    let d = h.invoke("detect_service", json!({ "id": id })).expect("detect_service");
    assert_eq!(d["package_manager"], "npm");
    assert_eq!(d["start_candidates"][0], json!(["npm", "run", "dev"]));
    assert_eq!(d["dev_port"], Value::Null, "脚本没有 --port 时应为 null");
    assert!(d["toolchain"].as_array().unwrap().contains(&json!("npm")));

    // service_config 初始为 null
    assert_eq!(h.invoke("service_config", json!({ "id": id })).unwrap(), Value::Null);

    // 保存配方（字段名用 snake_case，与 Rust 结构一致）
    let cfg = json!({
        "install": null,
        "build": null,
        "start": ["node", "server.js"],
        "dev_port": 45200,
        "requires_ports": [],
        "env": {},
        "node_pin": null,
        "toolchain": ["npm"],
        "origin": "manual"
    });
    h.invoke("save_service_config", json!({ "id": id, "config": cfg })).expect("save_service_config");
    let back = h.invoke("service_config", json!({ "id": id })).expect("读回配方");
    assert_eq!(back["start"], json!(["node", "server.js"]));
    assert_eq!(back["dev_port"], 45200);

    // preflight：本机 npm 存在，应返回空数组
    let pf = h.invoke("service_preflight", json!({ "id": id, "step": "start" })).expect("preflight");
    assert_eq!(pf, json!([]));

    // source_info：非仓库
    let si = h.invoke("source_info", json!({ "id": id })).expect("source_info");
    assert_eq!(si["is_repo"], false);
    assert_eq!(si["pull_blocked_by"], "不是 git 仓库");

    // service_status 初始为空数组
    assert_eq!(h.invoke("service_status", json!({})).unwrap(), json!([]));

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
        "save_service_config",
        json!({
            "id": id,
            "config": {
                "start": ["node", "server.js"],
                "dev_port": 45210,
                "requires_ports": [],
                "env": {},
                "toolchain": [],
                "origin": "manual"
            }
        }),
    )
    .expect("save");

    let st = h.invoke("service_start", json!({ "id": id })).expect("service_start");
    assert_eq!(st["phase"], "Running");
    assert_eq!(st["port"], 45210);

    // 等端口真的起来
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut up = false;
    while std::time::Instant::now() < deadline {
        if !crate::ports::is_free(45210) {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(up, "服务应在 10 秒内监听 45210；日志：{:?}", h.invoke("service_logs", json!({ "id": id })));

    // 状态列表里能看到它
    let all = h.invoke("service_status", json!({})).unwrap();
    assert!(all.as_array().unwrap().iter().any(|s| s["project_id"] == id && s["phase"] == "Running"));

    h.invoke("service_stop", json!({ "id": id })).expect("service_stop");
    assert!(
        crate::ports::wait_released(45210, std::time::Duration::from_secs(5)),
        "停止后 45210 必须释放"
    );

    // 停止后状态为 Stopped
    let after = h.invoke("service_status", json!({})).unwrap();
    let row = after.as_array().unwrap().iter().find(|s| s["project_id"] == id).cloned().unwrap();
    assert_eq!(row["phase"], "Stopped");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `dev_port` 被占时必须顺延，并把真相写进 port 字段。
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
    h.invoke(
        "save_service_config",
        json!({
            "id": id,
            "config": {
                "start": ["node", "server.js"],
                "dev_port": 8080,
                "requires_ports": [],
                "env": {},
                "toolchain": [],
                "origin": "manual"
            }
        }),
    )
    .expect("save");

    let st = h.invoke("service_start", json!({ "id": id })).expect("service_start");
    let port = st["port"].as_u64().expect("应有端口") as u16;
    assert_ne!(port, 8080, "8080 被占时必须顺延");
    let note = st["note"].as_str().unwrap_or_default().to_string();
    assert!(note.contains("8080"), "note 应说明 8080 被占：{note}");
    assert!(
        note.contains("已改用") && note.contains(&port.to_string()),
        "note 应说明改用了哪个端口：{note}"
    );

    // 服务必须真的听在被报告的那个端口上
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

    // 实测校准会把真相写回状态（allocated 与 observed 应一致）
    let later = h.invoke("service_status", json!({})).unwrap();
    let row = later.as_array().unwrap().iter().find(|s| s["project_id"] == id).cloned().unwrap();
    assert_eq!(row["port"].as_u64().map(|p| p as u16), Some(port));

    h.invoke("service_stop", json!({ "id": id })).expect("stop");
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
        "save_service_config",
        json!({
            "id": id,
            "config": {
                "build": ["definitely-not-a-real-binary-xyz", "build"],
                "start": ["node", "server.js"],
                "dev_port": null,
                "requires_ports": [],
                "env": {},
                "toolchain": [],
                "origin": "manual"
            }
        }),
    )
    .expect("save");

    let pf = h.invoke("service_preflight", json!({ "id": id, "step": "build" })).expect("preflight");
    assert!(
        pf.as_array().unwrap().iter().any(|v| v.as_str().unwrap_or_default().contains("definitely-not-a-real-binary-xyz")),
        "应报缺少该命令：{pf}"
    );

    // 直接 start 缺的可执行文件同样要拒绝
    h.invoke(
        "save_service_config",
        json!({
            "id": id,
            "config": {
                "start": ["definitely-not-a-real-binary-xyz"],
                "dev_port": null,
                "requires_ports": [],
                "env": {},
                "toolchain": [],
                "origin": "manual"
            }
        }),
    )
    .expect("save");
    let err = h.invoke("service_start", json!({ "id": id })).expect_err("应拒绝启动");
    assert!(err.to_string().contains("缺少可执行文件"), "错误文案：{err}");

    let _ = std::fs::remove_dir_all(&dir);
}
