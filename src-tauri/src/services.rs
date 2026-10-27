//! 服务配方类型：一个项目可选地拥有一份「安装 / 构建 / 启动」配方。
//!
//! 存储形态是 `services.config` 列里的 JSON 字符串（见 `db::set_service_config`），
//! 所以这里只管序列化与默认值，不做任何 IO。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 一个项目的运行配方。所有命令都以 argv 数组表达，绝不拼 shell 字符串。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ServiceConfig {
    #[serde(default)]
    pub install: Option<Vec<String>>,
    #[serde(default)]
    pub build: Option<Vec<String>>,
    #[serde(default)]
    pub start: Vec<String>,
    #[serde(default)]
    pub dev_port: Option<u16>,
    #[serde(default)]
    pub requires_ports: Vec<u16>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub node_pin: Option<String>,
    #[serde(default)]
    pub toolchain: Vec<String>,
    #[serde(default = "default_origin")]
    pub origin: String,
}

fn default_origin() -> String {
    "manual".to_string()
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            install: None,
            build: None,
            start: Vec::new(),
            dev_port: None,
            requires_ports: Vec::new(),
            env: BTreeMap::new(),
            node_pin: None,
            toolchain: Vec::new(),
            origin: "manual".to_string(),
        }
    }
}

impl ServiceConfig {
    pub fn from_json(s: &str) -> Result<ServiceConfig, String> {
        serde_json::from_str(s).map_err(|e| format!("配方解析失败：{e}"))
    }

    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| format!("配方序列化失败：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_origin_is_manual() {
        assert_eq!(ServiceConfig::default().origin, "manual");
        // 缺字段也能反序列化，origin 落到默认值
        let c: ServiceConfig = serde_json::from_str(r#"{"start":["node","a.js"]}"#).unwrap();
        assert_eq!(c.origin, "manual");
        assert_eq!(c.start, vec!["node".to_string(), "a.js".to_string()]);
        assert!(!c.start.is_empty());
    }

    #[test]
    fn empty_start_counts_as_no_recipe() {
        let c: ServiceConfig = serde_json::from_str("{}").unwrap();
        assert!(c.start.is_empty());
        assert!(c.dev_port.is_none());
        assert!(c.requires_ports.is_empty());
    }

    #[test]
    fn json_roundtrip() {
        let c = ServiceConfig {
            install: Some(vec!["pnpm".into(), "install".into()]),
            build: None,
            start: vec!["pnpm".into(), "dev".into()],
            dev_port: Some(5190),
            requires_ports: vec![5432],
            env: BTreeMap::from([("NODE_ENV".to_string(), "development".to_string())]),
            node_pin: Some("24.16.0".into()),
            toolchain: vec!["pnpm".into()],
            origin: "auto".into(),
        };
        let back = ServiceConfig::from_json(&c.to_json().unwrap()).unwrap();
        assert_eq!(c, back);
    }
}
