//! 给 AI 看的「项目事实」快照：只读、只陈述观测到的事实，不做任何推断或建议。
//!
//! 这里刻意只做组装：路径、描述文件、服务声明、env 的**键名**、工具链、端口、git 状态。
//! 密钥值永不出现在载荷里——env 只报键名，AI 要看值必须自己读文件（且受只读闸门约束）。
//! 载荷总量封顶 24KB：超限时按固定顺序裁剪列表（`TRIM_ORDER`），绝不裁剪 `services`
//! 本身，否则 AI 会以为项目没有服务。

// 本模块是分阶段接入的：快照要由后续的 AI 会话入口挂上去，
// 现在先由单元测试保证行为正确。
#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 载荷总量上限（序列化后的字节数）。
pub const CONTEXT_CAP: usize = 24 * 1024;

const LSOF: &str = "/usr/sbin/lsof";
const VERSION_TIMEOUT: Duration = Duration::from_secs(3);
const TOOLCHAIN_TTL: Duration = Duration::from_secs(600);

/// 探测版本的工具名单（顺序即输出顺序）。
const TOOLS: &[&str] = &["node", "npm", "pnpm", "yarn", "bun", "go", "cargo", "python3", "docker"];

/// env 示例文件后缀：只用来算 `examples` 与 `missing_keys`。
const EXAMPLE_SUFFIXES: &[&str] = &[".example", ".sample", ".template", ".dist"];

/// 超限时的裁剪顺序（显式列表，从最不重要的开始）。`services` 不在列表里——永不裁剪。
///
/// 顺序理由：探测出的候选只是「可参考」，工具链/端口可以重新查，env 键名是用户直接关心的，
/// 所以先剪候选、再剪工具链、再剪端口、最后才剪 env 列表。
const TRIM_ORDER: &[&[&str]] = &[
    &["detected", "start_candidates"],
    &["detected", "compose_stacks"],
    &["detected", "script_candidates"],
    &["detected", "make_targets"],
    &["toolchain"],
    &["listening_ports"],
    &["env", "files"],
    &["env", "examples"],
];

/// 工具链探测缓存：`(写入时刻, PATH 键, 结果)`。PATH 变了就重新探测（pin 切换）。
static TOOLCHAIN_CACHE: Mutex<Option<(Instant, String, Vec<Value>)>> = Mutex::new(None);

/// 组装给 AI 看的项目事实快照。总量上限 24KB（超出截断列表并置 truncated）。
pub fn project_context<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    id: i64,
) -> Result<Value, String> {
    let base = crate::commands::existing_project_path(id)?;
    let conn = crate::db::open_db().map_err(|e| e.to_string())?;
    let sup = crate::supervisor::state(app);

    let summary = crate::db::all_projects(&conn)
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.id == id);
    let tags = crate::db::tags_of(&conn, id).unwrap_or_default();

    // 描述文件：坏文件既是一个错误，也意味着「数据库里的旧配方不再是真相」。
    let descriptor_error = crate::descriptor::read(&base).err();
    let descriptor_exists = crate::descriptor::exists(&base);
    let detected = crate::detect::detect(&base);

    let (items, services_error) = match descriptor_error.clone() {
        Some(e) => (Vec::new(), Some(e)),
        None => match crate::plan::resolve_all(&conn, id, &base) {
            Ok((items, _overrides)) => (items, None),
            Err(e) => (Vec::new(), Some(e)),
        },
    };

    let services: Vec<Value> = items
        .iter()
        .map(|r| {
            let key = crate::supervisor::key_of(id, &r.spec.name);
            json!({
                "spec": serde_json::to_value(&r.spec).unwrap_or(Value::Null),
                "layer": layer_str(r.layer),
                "status": sup
                    .status_of(&key)
                    .and_then(|s| serde_json::to_value(s).ok())
                    .unwrap_or(Value::Null),
                "preflight": crate::commands::preflight_problems(r, &r.spec.run_argv()),
            })
        })
        .collect();

    // pin：服务声明优先，其次探测结果（决定工具链用哪个 PATH）。
    let pin = items
        .first()
        .and_then(|r| r.derived.node_pin.clone())
        .or_else(|| detected.node_pin.clone());
    let node_pin = json!({
        "pin": pin,
        "installed": match pin.as_deref() {
            None => true,
            Some(p) => crate::execenv::node_bin_dir(p).is_some(),
        },
    });

    let (env_files, env_examples, missing_keys) = scan_env_files(&base);
    let env_json = json!({
        "files": env_files.iter().map(|(f, keys)| json!({"file": f, "keys": keys})).collect::<Vec<_>>(),
        "examples": env_examples.iter().map(|(f, keys)| json!({"file": f, "keys": keys})).collect::<Vec<_>>(),
        "missing_keys": missing_keys,
    });

    let toolchain = toolchain_entries(&crate::execenv::path_for(pin.as_deref()));
    let ports = listening_ports();

    let src = crate::source::info(&base);
    let source = json!({
        "branch": src.branch,
        "dirty": src.dirty,
        "ahead": src.ahead,
        "behind": src.behind,
        "is_repo": src.is_repo,
    });

    let mut out = json!({
        "project": {
            "name": crate::logic::project_name_of(&base),
            "path": base.display().to_string(),
            "has_git": base.join(".git").exists(),
            "tags": tags,
            "summary": summary.as_ref().and_then(|p| p.summary.clone()),
            "stack": summary.map(|p| p.stack).unwrap_or_default(),
        },
        "detected": serde_json::to_value(&detected).unwrap_or(Value::Null),
        "descriptor": {"exists": descriptor_exists, "error": descriptor_error},
        "services": services,
        "services_error": services_error,
        "env": env_json,
        "toolchain": toolchain,
        "node_pin": node_pin,
        "listening_ports": ports,
        "source": source,
        "truncated": false,
    });

    trim_to_cap(&mut out, CONTEXT_CAP);
    Ok(out)
}

fn layer_str(layer: crate::plan::Layer) -> &'static str {
    match layer {
        crate::plan::Layer::Descriptor => "descriptor",
        crate::plan::Layer::Db => "db",
    }
}

// ---------------------------------------------------------------------------
// env 扫描（纯函数，可单测）
// ---------------------------------------------------------------------------

fn is_example(name: &str) -> bool {
    EXAMPLE_SUFFIXES.iter().any(|s| name.ends_with(s))
}

fn env_keys(text: &str) -> Vec<String> {
    let mut keys: Vec<String> = crate::execenv::parse_env_file(text)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// 一个 env 文件与它声明过的键名（按名排序、去重）。
pub(crate) type EnvKeyFile = (String, Vec<String>);

/// 扫项目根下的 env 文件，**只返回键名**（值留在文件里，绝不进载荷）。
///
/// 返回 `(files, examples, missing_keys)`：
/// - `files`：`.env` 或以 `.env.` 开头的真实 env 文件（`.env.local` 等），按文件名排序；
/// - `examples`：其中带示例后缀的（`.env.example` / `.sample` / `.template` / `.dist`）；
/// - `missing_keys`：示例里有、但所有实际 env 文件里都没有的键。
pub(crate) fn scan_env_files(
    base: &Path,
) -> (Vec<EnvKeyFile>, Vec<EnvKeyFile>, Vec<String>) {
    let mut files: Vec<EnvKeyFile> = Vec::new();
    let mut examples: Vec<EnvKeyFile> = Vec::new();
    let Ok(rd) = std::fs::read_dir(base) else {
        return (files, examples, Vec::new());
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n == ".env" || n.starts_with(".env."))
        .collect();
    names.sort();
    for name in names {
        let keys = std::fs::read_to_string(base.join(&name))
            .map(|t| env_keys(&t))
            .unwrap_or_default();
        if is_example(&name) {
            examples.push((name, keys));
        } else {
            files.push((name, keys));
        }
    }

    let present: BTreeSet<&str> = files
        .iter()
        .flat_map(|(_, ks)| ks.iter().map(|k| k.as_str()))
        .collect();
    let missing: Vec<String> = examples
        .iter()
        .flat_map(|(_, ks)| ks.iter())
        .filter(|k| !present.contains(k.as_str()))
        .cloned()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();

    (files, examples, missing)
}

// ---------------------------------------------------------------------------
// 端口（纯解析 + 一次性 lsof）
// ---------------------------------------------------------------------------

/// 解析 `lsof -Fn` 输出里以 `n` 开头的行（`n127.0.0.1:8080` / `n*:8080` / `n[::1]:5432`）。
///
/// 只认最后一段冒号后的合法 u16；非 `n` 行、无法解析、越界的一律跳过。结果去重升序。
pub(crate) fn parse_lsof_ports(text: &str) -> Vec<u16> {
    let mut set: BTreeSet<u16> = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('n') else { continue };
        if let Some((_, port)) = rest.rsplit_once(':') {
            if let Ok(p) = port.trim().parse::<u16>() {
                set.insert(p);
            }
        }
    }
    set.into_iter().collect()
}

fn listening_ports() -> Vec<u16> {
    for prog in [LSOF, "lsof"] {
        // lsof 在没有任何匹配时也返回非 0，退出码不重要；拿不到 stdout 就换下一个。
        if let Ok(o) = Command::new(prog)
            .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fn"])
            .output()
        {
            return parse_lsof_ports(&String::from_utf8_lossy(&o.stdout));
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// 工具链（带 10 分钟缓存）
// ---------------------------------------------------------------------------

fn toolchain_entries(path: &str) -> Vec<Value> {
    let now = Instant::now();
    {
        let guard = TOOLCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, key, cached)) = guard.as_ref() {
            if key == path && now.duration_since(*at) < TOOLCHAIN_TTL {
                return cached.clone();
            }
        }
    }
    // 探测会 spawn 子进程并等待，绝不持锁进行。
    let fresh = probe_toolchain(path);
    *TOOLCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((now, path.to_string(), fresh.clone()));
    fresh
}

fn probe_toolchain(path: &str) -> Vec<Value> {
    TOOLS
        .iter()
        .map(|name| match crate::execenv::resolve(name, path) {
            Some(p) => json!({
                "name": name,
                "path": p.display().to_string(),
                "version": tool_version(&p),
            }),
            // 缺失的工具也保留条目：AI 需要知道「本该有但没有」。
            None => json!({"name": name, "path": null, "version": null}),
        })
        .collect()
}

/// `<path> --version`，3 秒硬超时。
///
/// 输出必须在进程退出后才读：先起线程 `read_to_end`（管道被写满时进程才不会卡住），
/// 主线程只轮询 `try_wait`；超时就 kill 并丢弃输出（半截版本号没有意义）。
fn tool_version(program: &Path) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;

    let mut child = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    // 读线程：进程退出或被杀后管道关闭，read_to_end 才返回。读线程只是搬运，
    // 是否采信其输出由主线程按「是否在超时前退出」决定。
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + VERSION_TIMEOUT;
    let mut exited = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }

    let buf = rx.recv_timeout(Duration::from_millis(500)).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let first = text.lines().next()?.trim();
    // 只剥「v22.19.0」这种前缀的 v；「version 24」不能剥。
    let v = match first.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => first,
    }
    .trim();
    if v.is_empty() {
        None
    } else {
        Some(v.chars().take(80).collect())
    }
}

// ---------------------------------------------------------------------------
// 24KB 封顶
// ---------------------------------------------------------------------------

fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0)
}

fn array_at_mut<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Vec<Value>> {
    let (last, parents) = path.split_last()?;
    let mut cur = v;
    for k in parents {
        cur = cur.get_mut(*k)?;
    }
    cur.get_mut(*last)?.as_array_mut()
}

/// 按 `TRIM_ORDER` 从数组尾部裁剪，直到序列化后 ≤ `cap`；只要超限就置 `truncated: true`。
///
/// 返回「是否真的删除了条目」（超限但没有可裁剪的东西时返回 false，`truncated` 仍为 true）。
pub(crate) fn trim_to_cap(v: &mut Value, cap: usize) -> bool {
    if json_len(v) <= cap {
        return false;
    }
    let mut trimmed = false;
    for path in TRIM_ORDER {
        loop {
            if json_len(v) <= cap {
                return mark_truncated(v, trimmed);
            }
            // 每轮重新借一次：`json_len` 需要不可变借用，不能与 `arr` 的可变借用共存。
            let popped = match array_at_mut(v, path) {
                Some(arr) if !arr.is_empty() => {
                    arr.pop();
                    true
                }
                _ => false,
            };
            if !popped {
                break;
            }
            trimmed = true;
        }
    }
    mark_truncated(v, trimmed)
}

fn mark_truncated(v: &mut Value, trimmed: bool) -> bool {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("truncated".to_string(), Value::Bool(true));
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_env_files_splits_examples_and_ignores_plain_env_name() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path();
        std::fs::write(base.join(".env"), "FOO=1\nBAR=2\n# 注释\n").unwrap();
        std::fs::write(base.join(".env.local"), "BAZ=3\nFOO=override\n").unwrap();
        std::fs::write(base.join(".env.example"), "FOO=\nBAZ=\nQUX=\n").unwrap();
        // 名字不是 `.env` 也不是 `.env.` 开头：必须忽略。
        std::fs::write(base.join("env"), "NOPE=1\n").unwrap();
        std::fs::write(base.join(".envrc"), "NOPE=1\n").unwrap();

        let (files, examples, missing) = scan_env_files(base);
        assert_eq!(
            files,
            vec![
                (".env".to_string(), vec!["BAR".to_string(), "FOO".to_string()]),
                (".env.local".to_string(), vec!["BAZ".to_string(), "FOO".to_string()]),
            ]
        );
        assert_eq!(examples, vec![(".env.example".to_string(), vec!["BAZ".into(), "FOO".into(), "QUX".into()])]);
        // QUX 只出现在示例里；BAZ/FOO 在实际文件里都有。
        assert_eq!(missing, vec!["QUX".to_string()]);
    }

    #[test]
    fn scan_env_files_reports_missing_dir_as_empty() {
        let d = tempfile::tempdir().unwrap();
        let gone = d.path().join("does-not-exist");
        let (files, examples, missing) = scan_env_files(&gone);
        assert!(files.is_empty() && examples.is_empty() && missing.is_empty());
    }

    #[test]
    fn parse_lsof_ports_reads_only_names_and_sorts_dedup() {
        // 真实 lsof -nP -iTCP -sTCP:LISTEN -Fn 的形状：命令/进程/文件/名字交替出现。
        let text = "\
p1234
cnode
f12
n127.0.0.1:1420
p999
cdocker
f7
n*:8080
f8
n[::1]:5432
p1234
cnode
f12
n127.0.0.1:1420
n
nfoo:bar
nxx:99999
garbage line
n127.0.0.1:no-port
";
        assert_eq!(parse_lsof_ports(text), vec![1420, 5432, 8080]);
        assert!(parse_lsof_ports("").is_empty());
    }

    #[test]
    fn trim_to_cap_drops_lists_but_keeps_services() {
        let toolchain: Vec<Value> = (0..300)
            .map(|i| json!({"name": format!("tool-{i}"), "path": format!("/very/long/path/to/tool-{i}/bin/tool"), "version": "1.2.3"}))
            .collect();
        let ports: Vec<Value> = (0..200).map(|p| json!(30000 + p)).collect();
        let mut v = json!({
            "project": {"name": "demo"},
            "services": [
                {"spec": {"name": "api", "run": ["pnpm", "dev"]}, "layer": "descriptor"},
                {"spec": {"name": "web", "run": ["npm", "dev"]}, "layer": "db"},
            ],
            "toolchain": toolchain,
            "listening_ports": ports,
            "env": {"files": [], "examples": [], "missing_keys": []},
            "truncated": false,
        });
        let cap = 2048;
        assert!(json_len(&v) > cap, "fixture 必须超限");

        let trimmed = trim_to_cap(&mut v, cap);
        assert!(trimmed, "超限时应当真的删掉了条目");
        assert!(json_len(&v) <= cap, "裁剪后仍超限：{}", json_len(&v));
        assert_eq!(v["truncated"], Value::Bool(true));
        // services 永不被裁剪，即使超限。
        assert_eq!(v["services"].as_array().unwrap().len(), 2);
        assert_eq!(v["services"][0]["spec"]["name"], "api");
        // 先从列表尾部剪：只要达标就停在半途，不追求清空。
        assert!(v["toolchain"].as_array().unwrap().len() < 300);
    }

    #[test]
    fn trim_to_cap_is_a_noop_under_the_cap() {
        let mut v = json!({"services": [{"spec": {"name": "api"}}], "truncated": false});
        let before = json_len(&v);
        assert!(!trim_to_cap(&mut v, CONTEXT_CAP));
        assert_eq!(json_len(&v), before);
        assert_eq!(v["truncated"], Value::Bool(false));
        assert_eq!(v["services"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn trim_to_cap_prefers_detected_candidates_over_env_lists() {
        // 候选列表在最前面被剪：即使 env 列表也很长，候选先被清空。
        let mut v = json!({
            "detected": {
                "start_candidates": (0..40).map(|i| json!([format!("run-{i}-aaaaaaaaaaaaaaaaaaaaaaaa")])).collect::<Vec<_>>(),
            },
            "toolchain": [{"name": "node", "path": "/x/node", "version": "1.0.0"}],
            "env": {"files": [{"file": ".env", "keys": ["A"]}], "examples": [], "missing_keys": []},
            "services": [{"spec": {"name": "api"}}],
            "truncated": false,
        });
        assert!(json_len(&v) > 512);
        assert!(trim_to_cap(&mut v, 512));
        assert!(json_len(&v) <= 512);
        // 先剪候选（可能只剪一部分就够小），后面的列表保持原样。
        let cands = v["detected"]["start_candidates"].as_array().unwrap().len();
        assert!(cands < 40, "start_candidates 未被裁剪：{cands}");
        assert_eq!(v["toolchain"].as_array().unwrap().len(), 1);
        assert_eq!(v["env"]["files"].as_array().unwrap().len(), 1);
        assert_eq!(v["services"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_version_handles_missing_binary() {
        assert!(tool_version(Path::new("/nonexistent/definitely-not-here")).is_none());
    }

    #[test]
    fn toolchain_entries_marks_missing_tools_with_null_path() {
        // 给一个空 PATH：所有工具都应「缺失」，但条目齐全。
        let entries = probe_toolchain("/nonexistent-bin-dir");
        assert_eq!(entries.len(), TOOLS.len());
        for e in &entries {
            assert!(e["path"].is_null(), "{e}");
            assert!(e["version"].is_null(), "{e}");
        }
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert_eq!(names, TOOLS.to_vec());
    }

    /// 端到端：真 mock app + 临时数据目录 + 样本项目，断言载荷形状与截断上限。
    /// 这里同时打印一份实际载荷，方便人工核对字段名。
    ///
    /// 刻意**不动 HOME**：`project_context` 只需要真实 HOME 去解析 PATH/nvm（只读），
    /// 而改 HOME 是进程级副作用——并行跑着的 docker e2e 会因此找不到 compose 插件。
    #[test]
    fn project_context_emits_full_shape_for_real_project() {
        let data = tempfile::tempdir().unwrap();
        let _data = crate::test_support::ScopedDataDir::new(data.path());
        // 不取端口锁：本模块只读 lsof、从不 bind。反过来取会与 `ipc_tests::Harness`
        // 的「port → data」顺序形成锁反转（那里先 port 锁再 ScopedDataDir），并行即死锁。

        let proj = tempfile::tempdir().unwrap();
        let base = proj.path();
        std::fs::write(base.join("package.json"), r#"{"scripts":{"dev":"vite --port 1420"}}"#).unwrap();
        std::fs::write(base.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(base.join(".env"), "FOO=1\nSECRET=should-never-appear\n").unwrap();
        std::fs::write(base.join(".env.example"), "FOO=\nBAR=\n").unwrap();
        std::fs::write(
            base.join(crate::descriptor::FILE_NAME),
            r#"{"schema_version":1,"services":[{"name":"api","run":["pnpm","dev"],"ports":{"listen":1420},"env_files":[".env"]}]}"#,
        )
        .unwrap();

        let app = crate::test_app();
        let id = {
            let conn = crate::db::open_db().unwrap();
            let id = crate::db::add_project(&conn, &base.display().to_string()).unwrap();
            let spec = crate::services::ServiceSpec::from_json(
                r#"{"name":"api","run":["pnpm","dev"]}"#,
            )
            .unwrap();
            crate::db::insert_service(&conn, id, &spec, &crate::services::Derived::manual()).unwrap();
            id
        };

        let v = project_context(app.handle(), id).expect("应能组装载荷");

        // 顶层字段齐备（顺序无关）。
        for k in [
            "project", "detected", "descriptor", "services", "env", "toolchain",
            "node_pin", "listening_ports", "source", "truncated",
        ] {
            assert!(v.get(k).is_some(), "缺少顶层字段 {k}");
        }
        assert_eq!(v["project"]["name"], json!(base.file_name().unwrap().to_string_lossy()));
        assert_eq!(v["project"]["has_git"], json!(false));
        assert_eq!(v["descriptor"]["exists"], json!(true));
        assert!(v["descriptor"]["error"].is_null(), "{}", v["descriptor"]["error"]);
        assert_eq!(v["services"].as_array().unwrap().len(), 1);
        assert_eq!(v["services"][0]["layer"], json!("descriptor"));
        assert!(v["services"][0]["status"].is_null());
        assert_eq!(v["env"]["files"][0]["file"], json!(".env"));
        assert_eq!(v["env"]["files"][0]["keys"], json!(["FOO", "SECRET"]));
        assert_eq!(v["env"]["examples"][0]["keys"], json!(["BAR", "FOO"]));
        assert_eq!(v["env"]["missing_keys"], json!(["BAR"]));
        assert_eq!(v["node_pin"], json!({"pin": null, "installed": true}));
        assert_eq!(v["source"]["is_repo"], json!(false));
        assert_eq!(v["truncated"], json!(false));

        // 密钥值绝不出现在载荷里。
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.contains("should-never-appear"), "载荷泄露了 env 值");
        assert!(text.len() <= CONTEXT_CAP);

        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    }

    #[test]
    fn project_context_reports_broken_descriptor_without_services() {
        let data = tempfile::tempdir().unwrap();
        let _data = crate::test_support::ScopedDataDir::new(data.path());
        // 不取端口锁：本模块只读 lsof、从不 bind。反过来取会与 `ipc_tests::Harness`
        // 的「port → data」顺序形成锁反转（那里先 port 锁再 ScopedDataDir），并行即死锁。

        let proj = tempfile::tempdir().unwrap();
        let base = proj.path();
        std::fs::write(base.join(crate::descriptor::FILE_NAME), "{ not json").unwrap();
        let app = crate::test_app();
        let id = {
            let conn = crate::db::open_db().unwrap();
            crate::db::add_project(&conn, &base.display().to_string()).unwrap()
        };

        let v = project_context(app.handle(), id).expect("描述文件坏了也要出载荷");
        assert_eq!(v["descriptor"]["exists"], json!(true));
        assert!(v["descriptor"]["error"].as_str().unwrap().contains("解析失败"));
        // 坏文件时不假装数据库里的旧配方有效：服务为空，并给出 services_error。
        assert_eq!(v["services"], json!([]));
        assert!(v["services_error"].as_str().unwrap().contains("解析失败"));
    }
}
