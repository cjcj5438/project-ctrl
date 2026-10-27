//! 本机状态层：`~/.project-ctrl/` 下的三类文件职责不重叠。
//!
//! | 文件 | 谁写 | 内容 |
//! |---|---|---|
//! | `overrides.json` | 只由用户（含 UI 里用户点的编辑） | 端口偏移意愿、绝对路径、env、`keep_alive`、`restart` 覆盖 |
//! | `state/ports.json` | app 自动 | 上次运行时实际分配到的端口（观测值，下次启动沿用） |
//! | `state/run/<safe-key>.json` | app 自动 | 收养凭据（pid/pgid/argv/cwd/started_at） |
//!
//! 合并顺序：仓库内声明 → `overrides.json`（意图）→ `state/ports.json`（观测）。
//!
//! `overrides.json` 里认不出的键**原样保留**（不报错、不丢弃）：它是用户手写的本机状态，
//! 与严格校验的仓库声明方向相反。

use crate::services::{Restart, ServiceSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 进程内写串行锁：`load→改→write` 三段必须原子，否则并发写丢键（#16）。
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 原子写：tmp + fsync + rename（#16），新建文件 0600（#26）。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
        f.write_all(bytes).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
        f.sync_all().map_err(|e| format!("落盘 {} 失败：{e}", path.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    restrict_permissions(path);
    Ok(())
}

/// 存量修复：本 app 的状态文件若 group/other 可读 → 改 0600（#26）。
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.permissions().mode() & 0o077 != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// 文件名/键转义（#15、#50）：`[A-Za-z0-9_-]` 原样，其余字节转 `%XX`（大写十六进制）。
/// 可逆：读路径按文件名反推 key 时必须能还原，所以不用哈希。
fn escape_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// [`escape_key`] 的逆操作：`%XX` 还原字节（非法序列原样保留，不 panic）。
/// 目前只有测试用它做 roundtrip 断言；读路径按收养凭据 JSON 里的 key 直接定位文件，无需反推。
#[cfg(test)]
fn unescape_filename(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let (h, l) = (bytes[i + 1], bytes[i + 2]);
            if let (Some(h), Some(l)) = (hex_val(h), hex_val(l)) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}
pub fn root() -> PathBuf {
    crate::db::data_dir()
}

pub fn state_dir() -> PathBuf {
    root().join("state")
}

pub fn run_dir() -> PathBuf {
    state_dir().join("run")
}

fn overrides_path() -> PathBuf {
    root().join("overrides.json")
}

fn ports_path() -> PathBuf {
    state_dir().join("ports.json")
}

/// 单服务的本机覆盖（用户意图）。
///
/// `port_offset` 是**加法语义**：期望端口 = `ports.listen + port_offset`（#53）。
/// 不是绝对端口——旧注释写错了，这里以实现与 plan.rs 为准。
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Override {
    /// 加到声明 `ports.listen` 上的偏移量（0 = 不偏移）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_offset: Option<u16>,
    /// 退出 App 时是否保留进程（下次打开重新收养）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<Restart>,
    /// 追加/覆盖的环境变量。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// 追加的 env 文件（相对项目根）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_files: Vec<String>,
}

impl Override {
    pub fn is_empty(&self) -> bool {
        self.port_offset.is_none()
            && self.keep_alive.is_none()
            && self.restart.is_none()
            && self.env.is_empty()
            && self.env_files.is_empty()
    }
}

/// `set_service_override` 的 patch 形状（#22 三态语义）。
///
/// 三态字段的反序列化：serde 默认把 JSON `null` 反成外层 `None`（= 不变），
/// 与三态语义（`null` = 删键）相悖。这里显式包一层 `Some`，使字段缺席 → `None`、
/// `null` → `Some(None)`、值 → `Some(Some(v))`（等价于 `serde_with::double_option`，免引依赖）。
fn deserialize_double_option<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

/// 每个字段三态：absent（字段缺席）= 不变；`null` = 删除该键；值 = 设置。
/// `env` 逐键三态：缺席的键不变，`null` 值的键删除，有值的键设置/覆盖。
/// `env_files` 整体三态：absent 不变，`null` 清空，数组整体替换。
/// 外层 `patch: null`（`set_override` 的 `None`）= 删除整个服务的覆盖。
///
/// JSON 示例：`{"port_offset": null}` 只清偏移；`{"keep_alive": true}` 只设保留；
/// `{}` 什么都不改；`{"env": {"A": "1", "B": null}}` 设 A 删 B。
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct OverridePatch {
    #[serde(deserialize_with = "deserialize_double_option")]
    pub port_offset: Option<Option<u16>>,
    #[serde(deserialize_with = "deserialize_double_option")]
    pub keep_alive: Option<Option<bool>>,
    #[serde(deserialize_with = "deserialize_double_option")]
    pub restart: Option<Option<Restart>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, Option<String>>>,
    #[serde(deserialize_with = "deserialize_double_option")]
    pub env_files: Option<Option<Vec<String>>>,
}

/// `overrides.json` 的整份内容：`{ "<project path>": { "<service name>": {...} } }`。
///
/// 值一律保留原始 JSON：认不出的顶层键、认不出的服务键、服务覆盖里的未知字段
/// 都原样写回，保证 app 不会毁掉用户手写的内容（`Override` 只是其中的已知子集）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(transparent)]
pub struct Overrides {
    pub projects: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

/// 写入一个服务的覆盖（#22 三态，见 [`OverridePatch`]）。
///
/// `None` = 删除整个服务的覆盖；`Some(patch)` 按字段三态合并：
/// absent 不变、`null` 删键、值设置。未知字段原样保留。
/// 合并后已知字段全空且无未知字段 → 删整条。
pub fn set_override(
    o: &mut Overrides,
    project_path: &str,
    service: &str,
    patch: Option<&OverridePatch>,
) -> Result<(), String> {
    let Some(patch) = patch else {
        if let Some(services) = o.projects.get_mut(project_path) {
            services.remove(service);
        }
        o.projects.retain(|_, v| !v.is_empty());
        return Ok(());
    };
    let services = o.projects.entry(project_path.to_string()).or_default();
    let entry = services.entry(service.to_string()).or_insert_with(|| serde_json::Value::Object(Default::default()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(Default::default());
    }
    let obj = entry.as_object_mut().expect("刚保证是 object");
    apply_scalar(obj, "port_offset", &patch.port_offset)?;
    apply_scalar(obj, "keep_alive", &patch.keep_alive)?;
    apply_scalar(obj, "restart", &patch.restart)?;
    apply_scalar(obj, "env_files", &patch.env_files)?;
    if let Some(env_patch) = &patch.env {
        let env_obj = obj
            .entry("env".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if !env_obj.is_object() {
            *env_obj = serde_json::Value::Object(Default::default());
        }
        let map = env_obj.as_object_mut().expect("刚保证是 object");
        for (k, v) in env_patch {
            match v {
                Some(val) => {
                    map.insert(k.clone(), serde_json::Value::String(val.clone()));
                }
                None => {
                    map.remove(k);
                }
            }
        }
        if map.is_empty() {
            obj.remove("env");
        }
    }
    let known_gone = ["port_offset", "keep_alive", "restart", "env", "env_files"]
        .iter()
        .all(|k| !obj.contains_key(*k));
    if known_gone && obj.is_empty() {
        services.remove(service);
    }
    o.projects.retain(|_, v| !v.is_empty());
    Ok(())
}

/// 单字段三态合并：absent 不变，`None` 删键，`Some` 设置。
fn apply_scalar<T: Serialize>(obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, patch: &Option<Option<T>>) -> Result<(), String> {
    match patch {
        None => {}
        Some(None) => {
            obj.remove(key);
        }
        Some(Some(v)) => {
            obj.insert(key.to_string(), serde_json::to_value(v).map_err(|e| format!("序列化覆盖失败：{e}"))?);
        }
    }
    Ok(())
}

pub fn load_overrides() -> Result<Overrides, String> {
    let p = overrides_path();
    restrict_permissions(&p);
    match std::fs::read_to_string(&p) {
        Ok(t) if t.trim().is_empty() => Ok(Overrides::default()),
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("overrides.json 解析失败：{e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Overrides::default()),
        Err(e) => Err(format!("读取 overrides.json 失败：{e}")),
    }
}

#[cfg(test)]
pub fn save_overrides(o: &Overrides) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    save_overrides_locked(o)
}

/// 锁内原子修改：`load→f→save` 全程持有 `WRITE_LOCK`（#16），多线程并发不丢键。
/// commands 层与测试里的 load-modify-save 都应走这里，不要手写三段。
pub fn modify_overrides(f: impl FnOnce(&mut Overrides) -> Result<(), String>) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut o = load_overrides()?;
    f(&mut o)?;
    save_overrides_locked(&o)
}

/// `WRITE_LOCK` 已持有时调用。
fn save_overrides_locked(o: &Overrides) -> Result<(), String> {
    let json = serde_json::to_string_pretty(o).map_err(|e| format!("序列化 overrides 失败：{e}"))?;
    atomic_write(&overrides_path(), format!("{json}\n").as_bytes())
}

/// 端口观测值：`{ "<project path>#<service>": port }`。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(transparent)]
pub struct ObservedPorts {
    pub map: BTreeMap<String, u16>,
}

fn observed_key(project_path: &str, service: &str) -> String {
    format!("{}#{}", escape_key(project_path), escape_key(service))
}

pub fn load_ports() -> ObservedPorts {
    let p = ports_path();
    restrict_permissions(&p);
    match std::fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
            eprintln!("ports.json 解析失败，已按空处理：{e}");
            ObservedPorts::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ObservedPorts::default(),
        Err(e) => {
            eprintln!("ports.json 读取失败，已按空处理：{e}");
            ObservedPorts::default()
        }
    }
}

pub fn observed_port(project_path: &str, service: &str) -> Option<u16> {
    load_ports().map.get(&observed_key(project_path, service)).copied()
}

pub fn remember_port(project_path: &str, service: &str, port: u16) {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut ports = load_ports();
    if ports.map.get(&observed_key(project_path, service)) == Some(&port) {
        return;
    }
    ports.map.insert(observed_key(project_path, service), port);
    let _ = write_ports_locked(&ports);
}

/// `WRITE_LOCK` 已持有时调用（#16：load-modify-write 全程串行）。
fn write_ports_locked(ports: &ObservedPorts) -> Result<(), String> {
    let json = serde_json::to_string_pretty(ports).map_err(|e| e.to_string())?;
    atomic_write(&ports_path(), format!("{json}\n").as_bytes())
}

/// 收养凭据落盘 / 读取 / 清除。
pub fn write_adopt(rec: &crate::supervisor::AdoptRecord) -> Result<(), String> {
    let json = serde_json::to_string_pretty(rec).map_err(|e| e.to_string())?;
    atomic_write(&adopt_path(&rec.key), json.as_bytes())
}

pub fn read_adopts() -> Vec<crate::supervisor::AdoptRecord> {
    let Ok(rd) = std::fs::read_dir(run_dir()) else { return Vec::new() };
    // #15 命名迁移：同一 key 可能同时有新旧两个文件（旧 `_` 命名 + 新 `%XX` 命名）。
    // 同 key 只保留一条，并删掉非新命名路径的文件，避免调用方 verify 旧记录失败后
    // clear_adopt(key) 误删同名新凭据。
    let mut by_key: BTreeMap<String, (std::path::PathBuf, crate::supervisor::AdoptRecord)> = BTreeMap::new();
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().map(|x| x != "json").unwrap_or(true) {
            continue;
        }
        restrict_permissions(&path);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(rec) = serde_json::from_str::<crate::supervisor::AdoptRecord>(&text) else { continue };
        let canonical = adopt_path(&rec.key);
        match by_key.get(&rec.key) {
            None => {
                by_key.insert(rec.key.clone(), (path, rec));
            }
            Some((prev_path, prev)) => {
                // 新命名路径优先；都不/都新则 started_at 大者胜。
                let cur_canon = path == canonical;
                let prev_canon = *prev_path == canonical;
                let cur_wins = match (cur_canon, prev_canon) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => rec.started_at >= prev.started_at,
                };
                if cur_wins {
                    let (loser_path, _) = by_key.insert(rec.key.clone(), (path.clone(), rec)).unwrap();
                    let _ = std::fs::remove_file(loser_path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    // 单文件但路径非新命名（如仅剩旧 1_keep.json）：搬到新命名，避免下次写出双份。
    let mut out = Vec::with_capacity(by_key.len());
    for (key, (path, rec)) in by_key {
        let canonical = adopt_path(&key);
        if path != canonical
            && atomic_write(&canonical, serde_json::to_string_pretty(&rec).unwrap_or_default().as_bytes()).is_ok()
        {
            let _ = std::fs::remove_file(&path);
        }
        out.push(rec);
    }
    out
}

pub fn clear_adopt(key: &str) {
    let _ = std::fs::remove_file(adopt_path(key));
}

fn adopt_path(key: &str) -> PathBuf {
    run_dir().join(format!("{}.json", escape_key(key)))
}

/// 把声明与该服务的本机覆盖合成「实际执行用的声明」。
pub fn apply_override(spec: &ServiceSpec, ov: Option<&Override>) -> (ServiceSpec, bool) {
    let Some(ov) = ov else { return (spec.clone(), false) };
    let mut out = spec.clone();
    if let Some(r) = ov.restart {
        out.restart = r;
    }
    for (k, v) in &ov.env {
        out.env.insert(k.clone(), v.clone());
    }
    for f in &ov.env_files {
        if !out.env_files.contains(f) {
            out.env_files.push(f.clone());
        }
    }
    // port_offset 由调用方按「带偏移的期望端口」处理，这里只记在返回值里
    (out, ov.keep_alive.unwrap_or(false))
}

/// 期望端口 = `listen + offset`（加法语义，#53）。溢出返回 Err 交给调用方处理，
/// 不再 saturating 钳到 65535（钳位会让多个服务撞到同一端口还静默通过）。
///
/// 注意：plan.rs 的 `wanted_port` 目前仍是旧 saturating 路径（另一 slice 的文件，
/// 本 slice 不能改）：集成者需在 plan.rs 里把
/// `r.spec.ports.listen.map(|p| p.saturating_add(r.port_offset))`
/// 切换为 `r.spec.ports.listen.map(|p| checked_apply_offset(p, r.port_offset)).transpose()?`。
pub fn checked_apply_offset(listen: u16, offset: u16) -> Result<u16, String> {
    listen.checked_add(offset).ok_or_else(|| format!("端口溢出：{listen} + 偏移 {offset} 超出 65535"))
}

/// 读单个服务的覆盖。未知字段不影响解析；解析不了就当没有覆盖。
pub fn service_override(o: &Overrides, project_path: &str, service: &str) -> Option<Override> {
    let raw = o.projects.get(project_path)?.get(service)?;
    serde_json::from_value::<Override>(raw.clone()).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec::from_json(r#"{"name":"api","run":["pnpm","dev"]}"#).unwrap()
    }

    #[test]
    fn unknown_project_and_service_keys_are_preserved() {
        let text = r#"{"\/x\/a":{"api":{"keep_alive":true},
                        "unrelated_service":{"port_offset":1234}},
                       "future_top_level":{"whatever":1}}"#;
        let o: Overrides = serde_json::from_str(text).unwrap();
        let back = serde_json::to_string(&o).unwrap();
        assert!(back.contains("unrelated_service"), "{back}");
        assert!(back.contains("future_top_level"), "{back}");
        assert_eq!(
            service_override(&o, "/x/a", "api").and_then(|v| v.keep_alive),
            Some(true)
        );
    }

    #[test]
    fn override_merges_env_restart_and_keep_alive() {
        let mut ov = Override::default();
        ov.env.insert("NODE_ENV".into(), "test".into());
        ov.restart = Some(Restart::Never);
        ov.keep_alive = Some(true);
        let (merged, keep) = apply_override(&spec(), Some(&ov));
        assert_eq!(merged.env.get("NODE_ENV").map(String::as_str), Some("test"));
        assert_eq!(merged.restart, Restart::Never);
        assert!(keep);
    }

    #[test]
    fn missing_override_is_noop() {
        let (merged, keep) = apply_override(&spec(), None);
        assert_eq!(merged, spec());
        assert!(!keep);
    }

    #[test]
    fn set_override_keeps_unknown_keys_of_the_same_service() {
        let mut o: Overrides =
            serde_json::from_str(r#"{"/x/a":{"api":{"port_offset":1,"hand_written":"keep me"}}}"#)
                .unwrap();
        let patch = OverridePatch { keep_alive: Some(Some(true)), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&patch)).unwrap();
        let back = serde_json::to_string(&o).unwrap();
        assert!(back.contains("hand_written"), "{back}");
        let merged = service_override(&o, "/x/a", "api").unwrap();
        assert_eq!(merged.port_offset, Some(1), "已有字段不应被抹掉");
        assert_eq!(merged.keep_alive, Some(true));
    }

    #[test]
    fn removing_last_override_drops_the_project_entry() {
        let mut o = Overrides::default();
        let patch = OverridePatch { keep_alive: Some(Some(true)), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&patch)).unwrap();
        assert!(o.projects.contains_key("/x/a"));
        set_override(&mut o, "/x/a", "api", None).unwrap();
        assert!(o.projects.is_empty(), "{:?}", o.projects);
    }

    #[test]
    fn empty_patch_is_a_noop() {
        let mut o = Overrides::default();
        let patch = OverridePatch { keep_alive: Some(Some(true)), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&patch)).unwrap();
        // 空 patch（全 absent）= 不变，不再是删除（#22 语义变更）
        set_override(&mut o, "/x/a", "api", Some(&OverridePatch::default())).unwrap();
        assert_eq!(
            service_override(&o, "/x/a", "api").and_then(|v| v.keep_alive),
            Some(true)
        );
    }

    #[test]
    fn adopt_path_collision_avoided_and_roundtrip() {
        let a = adopt_path("api v2");
        let b = adopt_path("api.v2");
        assert_ne!(a, b, "不同 key 必须落不同文件");
        for key in ["api v2", "api.v2", "12/my service: with spaces", "100%/确"] {
            let p = adopt_path(key);
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            assert!(!name.contains('/'), "{name}");
            assert!(name.ends_with(".json"));
            let stem = name.strip_suffix(".json").unwrap();
            assert_eq!(unescape_filename(stem), key, "文件名必须可逆回 key");
        }
    }

    #[test]
    fn overlapping_writes_do_not_lose_keys() {
        let dir = tempfile::tempdir().unwrap();
        let _data = crate::test_support::ScopedDataDir::new(dir.path());
        // ports：多线程并发 remember_port 不丢键
        let n_threads = 8usize;
        let n_each = 25usize;
        std::thread::scope(|s| {
            for t in 0..n_threads {
                s.spawn(move || {
                    for i in 0..n_each {
                        remember_port("/x/concurrent", &format!("svc-{t}-{i}"), 5000 + t as u16);
                    }
                });
            }
        });
        let ports = load_ports();
        assert_eq!(ports.map.len(), n_threads * n_each, "{:?}", ports.map.keys().take(5));
        // overrides：多线程并发 modify_overrides 不丢服务
        std::thread::scope(|s| {
            for t in 0..n_threads {
                s.spawn(move || {
                    for i in 0..n_each {
                        let name = format!("svc-{t}-{i}");
                        modify_overrides(|o| {
                            let patch = OverridePatch {
                                keep_alive: Some(Some(true)),
                                ..Default::default()
                            };
                            set_override(o, "/x/concurrent", &name, Some(&patch))
                        })
                        .unwrap();
                    }
                });
            }
        });
        let o = load_overrides().unwrap();
        assert_eq!(o.projects["/x/concurrent"].len(), n_threads * n_each);
    }

    #[test]
    fn override_patch_tristate_merge() {
        // 设值
        let mut o = Overrides::default();
        let patch = OverridePatch { port_offset: Some(Some(5)), keep_alive: Some(Some(true)), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&patch)).unwrap();
        let got = service_override(&o, "/x/a", "api").unwrap();
        assert_eq!(got.port_offset, Some(5));
        assert_eq!(got.keep_alive, Some(true));
        // 单字段删除：port_offset=null 只删该键，不动 keep_alive
        let del = OverridePatch { port_offset: Some(None), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&del)).unwrap();
        let got = service_override(&o, "/x/a", "api").unwrap();
        assert_eq!(got.port_offset, None);
        assert_eq!(got.keep_alive, Some(true));
        // absent=不变：空 patch 不动已有值
        set_override(&mut o, "/x/a", "api", Some(&OverridePatch::default())).unwrap();
        let got = service_override(&o, "/x/a", "api").unwrap();
        assert_eq!(got.keep_alive, Some(true));
        // null patch=整服务删除
        set_override(&mut o, "/x/a", "api", None).unwrap();
        assert!(service_override(&o, "/x/a", "api").is_none());
        // env 三态：设值 / 删键（null 值） / 不变
        let mut o = Overrides::default();
        let patch = OverridePatch { env: Some([("A".into(), Some("1".into())), ("B".into(), Some("2".into()))].into_iter().collect()), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&patch)).unwrap();
        let m = BTreeMap::from([("B".to_string(), None)]);
        set_override(&mut o, "/x/a", "api", Some(&OverridePatch { env: Some(m), ..Default::default() })).unwrap();
        let got = service_override(&o, "/x/a", "api").unwrap();
        assert_eq!(got.env.get("A").map(String::as_str), Some("1"));
        assert!(!got.env.contains_key("B"));
    }

    #[test]
    fn ports_key_with_hash_roundtrips() {
        // "#/x#y" 类 key 不得与分隔符串扰：读写必须回到同一条目
        let dir = tempfile::tempdir().unwrap();
        let _data = crate::test_support::ScopedDataDir::new(dir.path());
        remember_port("/x#proj", "a#b", 5199);
        assert_eq!(observed_port("/x#proj", "a#b"), Some(5199));
        remember_port("/x", "proj#a#b", 5200);
        assert_eq!(observed_port("/x#proj", "a#b"), Some(5199), "含 # 的 key 不得串扰");
        assert_eq!(observed_port("/x", "proj#a#b"), Some(5200));
    }

    #[test]
    fn checked_apply_offset_rejects_overflow() {
        assert_eq!(checked_apply_offset(5000, 10).unwrap(), 5010);
        assert!(checked_apply_offset(65530, 10).is_err(), "溢出必须报错而不是钳到 65535");
    }

    #[test]
    fn state_files_are_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let _data = crate::test_support::ScopedDataDir::new(dir.path());
        save_overrides(&Overrides::default()).unwrap();
        remember_port("/x/a", "api", 5000);
        let rec = crate::supervisor::AdoptRecord {
            key: "1/api".into(), project_id: 1, name: "api".into(), pid: 1, pgid: 1,
            argv: vec!["x".into()], cwd: "/tmp".into(), started_at: 1, port: None, keep_alive: false, spawn_cmd: None,
        };
        write_adopt(&rec).unwrap();
        for p in [overrides_path(), ports_path(), adopt_path(&rec.key)] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} 权限应为 0600", p.display());
        }
        // 存量 0644 → 下次 open/load 后修复为 0600
        for p in [overrides_path(), ports_path(), adopt_path(&rec.key)] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        load_overrides().unwrap();
        load_ports();
        read_adopts();
        for p in [overrides_path(), ports_path(), adopt_path(&rec.key)] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "存量 {} 应被修复为 0600", p.display());
        }
    }

    #[test]
    fn stale_old_naming_adopt_files_are_deduped_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let _data = crate::test_support::ScopedDataDir::new(dir.path());
        std::fs::create_dir_all(run_dir()).unwrap();
        // 新旧两个文件同 key：新命名 1%2Fkeep.json + 旧命名 1_keep.json
        let new_rec = crate::supervisor::AdoptRecord {
            key: "1/keep".into(), project_id: 1, name: "keep".into(), pid: 37881, pgid: 37881,
            argv: vec!["sleep".into(), "30".into()], cwd: "/tmp".into(), started_at: 1793129285,
            port: None, keep_alive: true, spawn_cmd: None,
        };
        let mut old_rec = new_rec.clone();
        old_rec.pid = 27881;
        old_rec.started_at = 1793128207;
        std::fs::write(adopt_path("1/keep"), serde_json::to_string_pretty(&new_rec).unwrap()).unwrap();
        std::fs::write(run_dir().join("1_keep.json"), serde_json::to_string_pretty(&old_rec).unwrap()).unwrap();
        // 读出只保留一条（started_at 较大者），旧命名文件被顺手删掉
        let recs = read_adopts();
        let keep: Vec<_> = recs.iter().filter(|r| r.key == "1/keep").collect();
        assert_eq!(keep.len(), 1, "{recs:?}");
        assert_eq!(keep[0].started_at, 1793129285);
        assert!(!run_dir().join("1_keep.json").exists(), "旧命名文件应被清理");
        assert!(adopt_path("1/keep").exists(), "新命名文件必须保留");
    }

    #[test]
    fn adopt_path_is_filesystem_safe() {
        let p = adopt_path("12/my service: with spaces");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains('/') && !name.contains(' ') && !name.contains(':'), "{name}");
        assert!(name.ends_with(".json"));
    }
}
