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
use std::path::PathBuf;

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
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Override {
    /// 优先使用的端口（与声明里的 `ports.listen` 冲突时以本机为准），
    /// 以及上次被顺延后的稳定偏移。
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

/// `overrides.json` 的整份内容：`{ "<project path>": { "<service name>": {...} } }`。
///
/// 值一律保留原始 JSON：认不出的顶层键、认不出的服务键、服务覆盖里的未知字段
/// 都原样写回，保证 app 不会毁掉用户手写的内容（`Override` 只是其中的已知子集）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(transparent)]
pub struct Overrides {
    pub projects: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

/// 写入一个服务的覆盖（`None` = 删除该键）。已知字段逐项合并，未知字段原样保留。
pub fn set_override(
    o: &mut Overrides,
    project_path: &str,
    service: &str,
    patch: Option<&Override>,
) -> Result<(), String> {
    match patch {
        Some(ov) if !ov.is_empty() => {
            let new = serde_json::to_value(ov).map_err(|e| format!("序列化覆盖失败：{e}"))?;
            let entry = o
                .projects
                .entry(project_path.to_string())
                .or_default()
                .entry(service.to_string());
            match (entry, new) {
                (std::collections::btree_map::Entry::Occupied(mut occ), serde_json::Value::Object(new)) => {
                    if let serde_json::Value::Object(old) = occ.get_mut() {
                        for (k, v) in new {
                            old.insert(k, v);
                        }
                    } else {
                        occ.insert(serde_json::Value::Object(new));
                    }
                }
                (std::collections::btree_map::Entry::Vacant(vac), v) => {
                    vac.insert(v);
                }
                (std::collections::btree_map::Entry::Occupied(mut occ), v) => {
                    occ.insert(v);
                }
            }
        }
        _ => {
            if let Some(services) = o.projects.get_mut(project_path) {
                services.remove(service);
            }
            o.projects.retain(|_, v| !v.is_empty());
        }
    }
    Ok(())
}

pub fn load_overrides() -> Result<Overrides, String> {
    let p = overrides_path();
    match std::fs::read_to_string(&p) {
        Ok(t) if t.trim().is_empty() => Ok(Overrides::default()),
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("overrides.json 解析失败：{e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Overrides::default()),
        Err(e) => Err(format!("读取 overrides.json 失败：{e}")),
    }
}

pub fn save_overrides(o: &Overrides) -> Result<(), String> {
    let p = overrides_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建数据目录失败：{e}"))?;
    }
    let json = serde_json::to_string_pretty(o).map_err(|e| format!("序列化 overrides 失败：{e}"))?;
    std::fs::write(&p, format!("{json}\n")).map_err(|e| format!("写入 overrides.json 失败：{e}"))
}

/// 端口观测值：`{ "<project path>#<service>": port }`。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(transparent)]
pub struct ObservedPorts {
    pub map: BTreeMap<String, u16>,
}

fn observed_key(project_path: &str, service: &str) -> String {
    format!("{project_path}#{service}")
}

pub fn load_ports() -> ObservedPorts {
    std::fs::read_to_string(ports_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn observed_port(project_path: &str, service: &str) -> Option<u16> {
    load_ports().map.get(&observed_key(project_path, service)).copied()
}

pub fn remember_port(project_path: &str, service: &str, port: u16) {
    let mut ports = load_ports();
    if ports.map.get(&observed_key(project_path, service)) == Some(&port) {
        return;
    }
    ports.map.insert(observed_key(project_path, service), port);
    let _ = write_ports(&ports);
}

fn write_ports(ports: &ObservedPorts) -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(state_dir()) {
        return Err(format!("创建状态目录失败：{e}"));
    }
    let json = serde_json::to_string_pretty(ports).map_err(|e| e.to_string())?;
    std::fs::write(ports_path(), format!("{json}\n")).map_err(|e| format!("写入 ports.json 失败：{e}"))
}

/// 收养凭据落盘 / 读取 / 清除。
pub fn write_adopt(rec: &crate::supervisor::AdoptRecord) -> Result<(), String> {
    std::fs::create_dir_all(run_dir()).map_err(|e| format!("创建 run 目录失败：{e}"))?;
    let json = serde_json::to_string_pretty(rec).map_err(|e| e.to_string())?;
    std::fs::write(adopt_path(&rec.key), json).map_err(|e| format!("写入收养凭据失败：{e}"))
}

pub fn read_adopts() -> Vec<crate::supervisor::AdoptRecord> {
    let Ok(rd) = std::fs::read_dir(run_dir()) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str::<crate::supervisor::AdoptRecord>(&t).ok())
        .collect()
}

pub fn clear_adopt(key: &str) {
    let _ = std::fs::remove_file(adopt_path(key));
}

fn adopt_path(key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    run_dir().join(format!("{safe}.json"))
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
        let patch = Override { keep_alive: Some(true), ..Default::default() };
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
        let patch = Override { keep_alive: Some(true), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&patch)).unwrap();
        assert!(o.projects.contains_key("/x/a"));
        set_override(&mut o, "/x/a", "api", None).unwrap();
        assert!(o.projects.is_empty(), "{:?}", o.projects);
    }

    #[test]
    fn empty_patch_is_a_delete() {
        let mut o = Overrides::default();
        let patch = Override { keep_alive: Some(true), ..Default::default() };
        set_override(&mut o, "/x/a", "api", Some(&patch)).unwrap();
        set_override(&mut o, "/x/a", "api", Some(&Override::default())).unwrap();
        assert!(service_override(&o, "/x/a", "api").is_none());
    }

    #[test]
    fn adopt_path_is_filesystem_safe() {
        let p = adopt_path("12/my service: with spaces");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains('/') && !name.contains(' ') && !name.contains(':'), "{name}");
        assert!(name.ends_with(".json"));
    }
}
