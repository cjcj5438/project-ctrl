//! 服务声明：一个项目可以有多个服务，每个服务 = 有序一次性步骤 + 一个长驻命令。
//!
//! 两种来源共用同一份类型：
//! - 仓库内 `.project-ctrl.json`（可提交，严格校验，未知字段报错）；
//! - 数据库 `services.config`（同一份 JSON，只是多了一层 `derived`）。
//!
//! 硬规则：所有命令都以 argv 数组表达，app 永不拼 shell 字符串。要用 shell 特性必须显式写成
//! `["/bin/zsh","-lc","…"]`，让「看到的就是执行的」成立。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 描述文件格式版本。比本版本新的文件整体拒绝读取。
pub const SCHEMA_VERSION: u32 = 1;

fn default_step_timeout() -> u64 {
    1800
}

/// 一次性步骤（安装 / 迁移 / 构建…）。默认不随启动自动执行。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default = "default_step_timeout")]
    pub timeout_s: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HealthKind {
    /// 端口可连即为就绪。
    #[default]
    Port,
    /// HTTP GET 返回 2xx/3xx 即为就绪。
    Http,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Health {
    #[serde(rename = "type")]
    pub kind: HealthKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// 端口：`listen` 是服务自己监听的端口（被占则顺延），`requires` 是必须已监听的依赖。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<u16>,
}

impl Ports {
    pub fn is_empty(&self) -> bool {
        self.listen.is_none() && self.requires.is_empty()
    }
}

/// 一个 compose 引用即一个服务：多个 `-f` 叠加，只操作列出的子服务。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DockerSpec {
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    Never,
    /// 非零退出才重启（默认）。
    #[default]
    OnFailure,
    /// 任何退出都重启。
    Always,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub name: String,
    /// 相对项目根的启动目录（monorepo 子包）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 长驻命令。为 None 时该服务只是一组可手动执行的步骤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// 相对项目根的 env 文件（只记名字，值留在文件里）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_files: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Ports::is_empty")]
    pub ports: Ports,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub restart: Restart,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerSpec>,
}

impl ServiceSpec {
    /// 是否为长驻服务（有 run）。否则是纯步骤集合。
    pub fn is_longrun(&self) -> bool {
        self.run.as_ref().map(|r| !r.is_empty()).unwrap_or(false)
    }

    pub fn run_argv(&self) -> Vec<String> {
        self.run.clone().unwrap_or_default()
    }

    pub fn step(&self, name: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.name == name)
    }

    /// 校验声明本身的自洽性（不触碰文件系统）。空数组 = 合法。
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.name.trim().is_empty() {
            out.push("服务名不能为空".to_string());
        }
        if self.name.contains('/') || self.name.contains('\\') {
            out.push(format!("服务名不能包含路径分隔符：{}", self.name));
        }
        if self.run.as_ref().map(|r| r.is_empty()).unwrap_or(false) {
            out.push(format!("{}：run 为空数组（要么给命令，要么整段省略）", self.name));
        }
        if !self.is_longrun() && self.steps.is_empty() && self.docker.is_none() {
            out.push(format!("{}：既没有 run、也没有 steps 或 docker", self.name));
        }
        // clippy 单元素循环改写：直接判 cwd（行为零差异）。
        if let Some(v) = &self.cwd {
            if !is_project_relative(v) {
                out.push(format!("{}：cwd 必须是项目内的相对路径（{v}）", self.name));
            }
        }
        for f in &self.env_files {
            if !is_project_relative(f) {
                out.push(format!("{}：env_files 必须是项目内的相对路径（{f}）", self.name));
            }
        }
        if self.ports.listen == Some(0) {
            out.push(format!("{}：ports.listen 不能为 0（0 不是可监听的固定端口）", self.name));
        }
        if let Some(h) = &self.health {
            if let Some(p) = &h.path {
                if !p.starts_with('/') || p.chars().any(|c| c.is_whitespace()) {
                    out.push(format!("{}：health.path 必须形如 /… 且不含空白（{p:?}）", self.name));
                }
            }
            if let Some(t) = h.timeout_ms {
                if !(100..=120_000).contains(&t) {
                    out.push(format!("{}：health.timeout_ms 必须在 100–120000 之间（{t}）", self.name));
                }
            }
        }
        if let Some(d) = &self.docker {
            if d.files.is_empty() {
                out.push(format!("{}：docker.files 不能为空", self.name));
            }
            for f in &d.files {
                if !is_project_relative(f) {
                    out.push(format!("{}：docker 文件必须是项目内的相对路径（{f}）", self.name));
                }
            }
            if !self.is_longrun() && d.services.is_empty() {
                out.push(format!("{}：docker 服务未指定具体子服务", self.name));
            }
        }
        for s in &self.steps {
            if s.name.trim().is_empty() {
                out.push("步骤名不能为空".to_string());
            }
            if s.argv.is_empty() {
                out.push(format!("步骤 {} 的命令为空", s.name));
            }
        }
        if self.depends_on.iter().any(|d| d == &self.name) {
            out.push(format!("{}：depends_on 不能指向自己", self.name));
        }
        out
    }
}

/// 仓库内描述文件。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub schema_version: u32,
    pub services: Vec<ServiceSpec>,
}

impl Descriptor {
    pub fn new(services: Vec<ServiceSpec>) -> Self {
        Self { schema_version: SCHEMA_VERSION, services }
    }
}

/// 严格解析描述文件：未知字段报错，版本过新整体拒绝。
pub fn parse_descriptor(text: &str) -> Result<Descriptor, String> {
    #[derive(Deserialize)]
    struct VersionProbe {
        schema_version: u32,
    }
    let probe: VersionProbe = serde_json::from_str(text).map_err(|e| format!("描述文件解析失败：{e}"))?;
    if probe.schema_version > SCHEMA_VERSION {
        return Err(format!(
            "此文件由更新版本的 project-ctrl 写入（schema_version {}，本版本支持 {}）",
            probe.schema_version, SCHEMA_VERSION
        ));
    }
    let d: Descriptor = serde_json::from_str(text).map_err(|e| {
        format!("描述文件解析失败：{e}（未知字段会直接报错，请检查拼写）")
    })?;
    let mut problems = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        for s in &d.services {
            if !seen.insert(s.name.clone()) {
                problems.push(format!("描述文件里服务名重复：{}", s.name));
            }
        }
    }
    for s in &d.services {
        problems.extend(s.validate());
    }
    if !problems.is_empty() {
        return Err(problems.join("；"));
    }
    Ok(d)
}

/// 项目内相对路径：非空、不以 `/` 开头、不含 `..` 路径段。
/// 描述文件（cwd/env_files/docker.files）与本机覆盖层（env_files）共用同一规则。
pub fn is_project_relative(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && !p.split('/').any(|seg| seg == "..")
}

/// 执行前终检（#18）：`base.join(rel)` 经 canonicalize 后必须仍位于 base 内。
/// 只做词法校验挡不住 `cwd: "link"`（→/etc）这类符号链接穿越；`readme::resolve_within`
/// 要求目标是文件（图片场景），这里 cwd 须是目录、env/docker 文件须存在，各自另判。
pub fn resolve_within(base: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("路径不能为空".to_string());
    }
    if rel.starts_with('/') {
        return Err(format!("路径必须是项目内的相对路径（{rel}）"));
    }
    let base_canon =
        base.canonicalize().map_err(|e| format!("项目目录不可用（{}）：{e}", base.display()))?;
    let canon = base
        .join(rel)
        .canonicalize()
        .map_err(|_| format!("路径不存在（{rel}）"))?;
    if canon.starts_with(&base_canon) {
        Ok(canon)
    } else {
        Err(format!("路径越界：{rel} 指向项目之外"))
    }
}

pub fn to_descriptor_json(d: &Descriptor) -> Result<String, String> {
    let mut s = serde_json::to_string_pretty(d).map_err(|e| format!("描述文件序列化失败：{e}"))?;
    s.push('\n');
    Ok(s)
}

/// 由 app 维护、不进描述文件的派生信息。
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Derived {
    pub origin: String,
    pub node_pin: Option<String>,
    pub toolchain: Vec<String>,
}

impl Derived {
    pub fn manual() -> Self {
        Self { origin: "manual".into(), node_pin: None, toolchain: Vec::new() }
    }

    pub fn from_json(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

impl ServiceSpec {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| format!("服务声明序列化失败：{e}"))
    }

    pub fn from_json(s: &str) -> Result<ServiceSpec, String> {
        serde_json::from_str(s).map_err(|e| format!("服务声明解析失败：{e}"))
    }
}

// ---------------------------------------------------------------------------
// v0.2.0 旧配方 → 新模型（迁移用）
// ---------------------------------------------------------------------------

/// 旧版单配方 `ServiceConfig` 的镜像，只用于从老库读出来。
#[derive(Deserialize, Default)]
pub struct LegacyServiceConfig {
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
    #[serde(default = "legacy_origin")]
    pub origin: String,
}

fn legacy_origin() -> String {
    "manual".to_string()
}

/// 旧配方 → (新声明, 派生信息)。install/build 变成有序步骤，start 变成 run，零丢失。
pub fn from_legacy(l: &LegacyServiceConfig) -> (ServiceSpec, Derived) {
    let mut steps = Vec::new();
    if let Some(argv) = &l.install {
        if !argv.is_empty() {
            steps.push(Step { name: "install".into(), argv: argv.clone(), timeout_s: default_step_timeout() });
        }
    }
    if let Some(argv) = &l.build {
        if !argv.is_empty() {
            steps.push(Step { name: "build".into(), argv: argv.clone(), timeout_s: default_step_timeout() });
        }
    }
    let spec = ServiceSpec {
        name: "default".into(),
        cwd: None,
        run: if l.start.is_empty() { None } else { Some(l.start.clone()) },
        steps,
        env_files: Vec::new(),
        env: l.env.clone(),
        ports: Ports { listen: l.dev_port, requires: l.requires_ports.clone() },
        health: None,
        depends_on: Vec::new(),
        restart: Restart::OnFailure,
        docker: None,
    };
    let derived = Derived {
        origin: l.origin.clone(),
        node_pin: l.node_pin.clone(),
        toolchain: l.toolchain.clone(),
    };
    (spec, derived)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> ServiceSpec {
        ServiceSpec {
            name: "api".into(),
            cwd: None,
            run: Some(vec!["pnpm".into(), "dev".into()]),
            steps: vec![],
            env_files: vec![],
            env: BTreeMap::new(),
            ports: Ports::default(),
            health: None,
            depends_on: vec![],
            restart: Restart::OnFailure,
            docker: None,
        }
    }

    #[test]
    fn json_roundtrip_keeps_optional_fields_absent() {
        let s = minimal();
        let json = s.to_json().unwrap();
        assert!(!json.contains("cwd"), "None 字段不应出现在 JSON 里：{json}");
        assert_eq!(ServiceSpec::from_json(&json).unwrap(), s);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse_descriptor(
            r#"{"schema_version":1,"services":[{"name":"a","run":["x"],"restartt":"always"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("restartt"), "错误信息应点名拼错的字段：{err}");
    }

    #[test]
    fn newer_schema_version_is_rejected_with_hint() {
        let err = parse_descriptor(r#"{"schema_version":2,"services":[]}"#).unwrap_err();
        assert!(err.contains("更新版本"), "{err}");
    }

    #[test]
    fn service_without_run_steps_or_docker_fails_validation() {
        let s = ServiceSpec { run: None, ..minimal() };
        assert!(s.validate().iter().any(|m| m.contains("既没有 run")));
    }

    #[test]
    fn steps_only_service_is_valid() {
        let s = ServiceSpec {
            run: None,
            steps: vec![Step { name: "build".into(), argv: vec!["make".into()], timeout_s: 60 }],
            ..minimal()
        };
        assert!(s.validate().is_empty(), "{:?}", s.validate());
        assert!(!s.is_longrun());
    }

    #[test]
    fn legacy_install_build_become_ordered_steps() {
        let legacy: LegacyServiceConfig = serde_json::from_str(
            r#"{"install":["pnpm","install"],"build":["pnpm","run","build"],
                "start":["pnpm","dev"],"dev_port":5190,"requires_ports":[5432],
                "env":{"NODE_ENV":"development"},"node_pin":"24.16.0",
                "toolchain":["pnpm"],"origin":"pm2"}"#,
        )
        .unwrap();
        let (spec, derived) = from_legacy(&legacy);
        assert_eq!(
            spec.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["install", "build"]
        );
        assert_eq!(spec.run.unwrap(), vec!["pnpm", "dev"]);
        assert_eq!(spec.ports.listen, Some(5190));
        assert_eq!(spec.ports.requires, vec![5432]);
        assert_eq!(spec.env.get("NODE_ENV").map(String::as_str), Some("development"));
        assert_eq!(spec.name, "default");
        assert_eq!(derived.origin, "pm2");
        assert_eq!(derived.node_pin.as_deref(), Some("24.16.0"));
    }

    #[test]
    fn legacy_empty_start_becomes_steps_only_service() {
        let legacy: LegacyServiceConfig =
            serde_json::from_str(r#"{"install":["pnpm","i"],"start":[]}"#).unwrap();
        let (spec, _) = from_legacy(&legacy);
        assert!(spec.run.is_none());
        assert_eq!(spec.steps.len(), 1);
        assert!(spec.validate().is_empty());
    }

    #[test]
    fn restart_serializes_kebab_case() {
        assert_eq!(serde_json::to_string(&Restart::OnFailure).unwrap(), "\"on-failure\"");
        assert_eq!(serde_json::to_string(&Restart::Never).unwrap(), "\"never\"");
    }

    #[test]
    fn duplicate_service_names_are_rejected_with_the_name() {
        let err = parse_descriptor(
            r#"{"schema_version":1,"services":[{"name":"api","run":["a"]},{"name":"api","run":["b"]}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("api"), "重名错误应点名：{err}");
    }
    #[test]
    fn listen_zero_is_rejected() {
        let s = ServiceSpec { ports: Ports { listen: Some(0), requires: vec![] }, ..minimal() };
        assert!(s.validate().iter().any(|m| m.contains('0')), "{:?}", s.validate());
    }

    #[test]
    fn health_path_must_be_absolute_without_whitespace() {
        for p in ["relative", "/has space", "/has\ttab", "/crlf\r\n"] {
            let s = ServiceSpec {
                health: Some(Health { kind: HealthKind::Http, path: Some(p.into()), timeout_ms: None }),
                ..minimal()
            };
            assert!(!s.validate().is_empty(), "应拒绝 {p:?}");
        }
        let ok = ServiceSpec {
            health: Some(Health { kind: HealthKind::Http, path: Some("/healthz".into()), timeout_ms: None }),
            ..minimal()
        };
        assert!(ok.validate().is_empty(), "{:?}", ok.validate());
    }

    #[test]
    fn health_timeout_is_bounded_but_optional() {
        for t in [99u64, 120_001] {
            let s = ServiceSpec {
                health: Some(Health { kind: HealthKind::Port, path: None, timeout_ms: Some(t) }),
                ..minimal()
            };
            assert!(!s.validate().is_empty(), "应拒绝 timeout {t}");
        }
        for t in [None, Some(100u64), Some(30_000), Some(120_000)] {
            let s = ServiceSpec {
                health: Some(Health { kind: HealthKind::Port, path: None, timeout_ms: t }),
                ..minimal()
            };
            assert!(s.validate().is_empty(), "应接受 timeout {t:?}：{:?}", s.validate());
        }
    }


    #[test]
    fn absolute_paths_are_rejected() {
        let s = ServiceSpec { cwd: Some("/etc".into()), ..minimal() };
        assert!(!s.validate().is_empty());
        let s = ServiceSpec { env_files: vec!["../secret".into()], ..minimal() };
        assert!(!s.validate().is_empty());
    }

    #[test]
    fn resolve_within_blocks_symlink_dotdot_and_absolute() {
        // #18 终检：链接穿越 / .. / 绝对路径 / 不存在全部拒绝，项目内放行。
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.env"), "A=1\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", dir.path().join("link")).unwrap();
        assert!(super::resolve_within(dir.path(), "ok.env").is_ok());
        assert!(super::resolve_within(dir.path(), "link").unwrap_err().contains("越界"));
        assert!(super::resolve_within(dir.path(), "../x").is_err());
        assert!(super::resolve_within(dir.path(), "/etc/hostname").unwrap_err().contains("相对路径"));
        assert!(super::resolve_within(dir.path(), "missing.env").unwrap_err().contains("不存在"));
        let _ = outside;
    }
}
