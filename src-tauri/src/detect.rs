//! 技术栈探测：只产出**候选**，不与数据库里已有配方做任何合并。
//!
//! 全部是文件读取 + 字符串匹配，不执行任何命令。`toolchain` 用来让 preflight 提前发现
//! 「按钮可点但必然失败」的情况（例如 `sub2api` 的 `make` 存在、它调用的 `go` 不存在）。

use serde::Serialize;
use std::path::{Path, PathBuf};

/// 一个 compose 文件里能作为「一键采用」的服务条目。
#[derive(Debug, Serialize, Clone, Default, PartialEq)]
pub struct ComposeCandidate {
    /// 相对项目根的 compose 文件路径。
    pub file: String,
    /// 该文件里声明的服务名（`services:` 下的键）。
    pub services: Vec<String>,
    /// 该文件里出现的宿主侧端口。
    pub ports: Vec<u16>,
    /// 已存在的 override 兄弟文件（如 `docker-compose.gpu.yml`），供多 `-f` 叠加。
    pub overrides: Vec<String>,
}

#[derive(Debug, Serialize, Clone, Default, PartialEq)]
pub struct DetectResult {
    pub package_manager: Option<String>,
    pub install: Option<Vec<String>>,
    pub build: Option<Vec<String>>,
    pub start_candidates: Vec<Vec<String>>,
    pub dev_port: Option<u16>,
    pub requires_ports: Vec<u16>,
    pub node_pin: Option<String>,
    pub toolchain: Vec<String>,
    pub problems: Vec<String>,
    /// 发现的 compose 栈（含子服务名），供「临时运行 / 一键采用」直接用。
    pub compose_stacks: Vec<ComposeCandidate>,
    /// `Makefile` 里的可执行目标名。
    pub make_targets: Vec<String>,
    /// 根目录下看起来能直接跑的脚本（`bin/*.sh`、`start*.sh`、`scripts/*.sh`）。
    pub script_candidates: Vec<String>,
}

fn read(base: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(base.join(name)).ok()
}

fn exists(base: &Path, name: &str) -> bool {
    base.join(name).exists()
}

/// 包管理器：锁文件优先，都没有则 npm。
fn package_manager(base: &Path) -> Option<String> {
    if !exists(base, "package.json") {
        return None;
    }
    if exists(base, "pnpm-lock.yaml") || exists(base, "pnpm-workspace.yaml") {
        return Some("pnpm".into());
    }
    if exists(base, "bun.lock") || exists(base, "bun.lockb") {
        return Some("bun".into());
    }
    if exists(base, "yarn.lock") {
        return Some("yarn".into());
    }
    Some("npm".into())
}

fn scripts_of(base: &Path) -> Vec<(String, String)> {
    let Some(text) = read(base, "package.json") else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let Some(obj) = v.get("scripts").and_then(|s| s.as_object()) else { return Vec::new() };
    obj.iter()
        .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

fn script_of(base: &Path, key: &str) -> Option<String> {
    scripts_of(base).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// 从脚本串里提取 `--port N` / `-p N`。
fn port_in_script(s: &str) -> Option<u16> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    for (i, t) in toks.iter().enumerate() {
        if (*t == "--port" || *t == "-p") && i + 1 < toks.len() {
            if let Ok(p) = toks[i + 1].parse::<u16>() {
                return Some(p);
            }
        }
        // 支持 --port=5190
        if let Some(rest) = t.strip_prefix("--port=") {
            if let Ok(p) = rest.parse::<u16>() {
                return Some(p);
            }
        }
    }
    None
}

/// 取形如 `port: 5190` / `port = 5190` / `"port": "5190"` 的第一个数字（用于 vite / next 配置）。
/// 必须匹配 `port` 前面的字符不是字母数字，否则 `export` 里的 "port" 会误命中。
fn port_in_config(text: &str) -> Option<u16> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find("port") {
        let at = from + rel;
        let prev_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        if prev_ok {
            let rest = &text[at + 4..];
            let trimmed = rest.trim_start_matches([':', '=', ' ', '\t', '"', '\'']);
            let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse::<u16>() {
                return Some(p);
            }
        }
        from = at + 4;
    }
    None
}

/// 从 `.env*` 文件里读 `PORT=NNNN`。
fn port_in_env_files(base: &Path) -> Option<u16> {
    let mut names: Vec<String> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".env"))
        .collect();
    names.sort();
    for n in names {
        let Some(text) = read(base, &n) else { continue };
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("PORT=") {
                if let Ok(p) = v.trim().trim_matches('"').parse::<u16>() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// 常见 compose 文件名（顶层与 `infra/`、`deploy/` 一层子目录）。
const COMPOSE_NAMES: &[&str] = &[
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yaml",
    "compose.yml",
    "docker-compose.dev.yml",
    "docker-compose.prod.yml",
];

/// 扫描 compose 文件：顶层 + 一层子目录（monorepo 常见 `infra/docker-compose.yml`）。
fn compose_candidates(base: &Path) -> Vec<ComposeCandidate> {
    let mut dirs: Vec<PathBuf> = vec![base.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(base) {
        let mut subs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .map(|n| n.to_string_lossy().starts_with('.'))
                        .unwrap_or(false)
            })
            .collect();
        subs.sort();
        dirs.extend(subs);
    }
    let mut found: Vec<(String, PathBuf, String)> = Vec::new();
    for dir in &dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let name = e.file_name().to_string_lossy().into_owned();
                let is_compose = COMPOSE_NAMES.contains(&name.as_str())
                    || (name.starts_with("docker-compose.") && (name.ends_with(".yml") || name.ends_with(".yaml")));
                if !is_compose {
                    continue;
                }
                let rel = match dir.strip_prefix(base) {
                    Ok(p) if p.as_os_str().is_empty() => name.clone(),
                    Ok(p) => format!("{}/{}", p.display(), name),
                    Err(_) => name.clone(),
                };
                found.push((rel, e.path(), name));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out: Vec<ComposeCandidate> = Vec::new();
    for (rel, path, name) in &found {
        let Some(text) = std::fs::read_to_string(path).ok() else { continue };
        let services = compose_service_names(&text);
        let ports = compose_ports_in(&text);
        // 兄弟 override（base = docker-compose.yml，override = docker-compose.gpu.yml）
        let dir_rel = Path::new(rel).parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let overrides = found
            .iter()
            .filter(|(r2, _, n2)| {
                n2 != name
                    && Path::new(r2).parent().map(|p| p.to_path_buf()).unwrap_or_default() == dir_rel
                    && !COMPOSE_NAMES.contains(&n2.as_str())
            })
            .map(|(r2, _, _)| r2.clone())
            .collect();
        out.push(ComposeCandidate { file: rel.clone(), services, ports, overrides });
    }
    out
}

/// 取 `services:` 下的第一层键。
fn compose_service_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_services = false;
    let mut service_indent: Option<usize> = None;
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        if !in_services {
            if trimmed.starts_with("services:") {
                in_services = true;
                service_indent = None;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        // 回到顶层（新的顶级键）就结束
        if indent == 0 {
            break;
        }
        match service_indent {
            None => {
                // 第一个服务名的缩进就是基准
                service_indent = Some(indent);
                if let Some(name) = trimmed.strip_suffix(':') {
                    out.push(name.trim().to_string());
                }
            }
            Some(base_indent) if indent == base_indent => {
                if let Some(name) = trimmed.strip_suffix(':') {
                    out.push(name.trim().to_string());
                }
            }
            _ => {}
        }
    }
    out
}

fn compose_ports_in(text: &str) -> Vec<u16> {
    let mut out = Vec::new();
    let mut in_ports = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("ports:") {
            in_ports = true;
            continue;
        }
        if in_ports {
            if !(trimmed.starts_with('-') || trimmed.starts_with('"') || trimmed.starts_with('\'') || trimmed.is_empty()) {
                in_ports = false;
                continue;
            }
            if let Some(p) = first_host_port(trimmed) {
                out.push(p);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// `Makefile` 里的目标名（行首非空、以 `:` 结尾、非变量赋值）。
fn make_targets(base: &Path) -> Vec<String> {
    let Some(text) = read(base, "Makefile") else { return Vec::new() };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('\t') || line.starts_with('#') || line.starts_with('.') {
            continue;
        }
        let Some((name, _)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name.is_empty() || name.contains('=') || name.contains(' ') {
            continue;
        }
        if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
            out.push(name.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 根目录/一层子目录里能直接跑起来的脚本。
fn script_candidates(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for sub in ["", "bin", "scripts"] {
        let dir = if sub.is_empty() { base.to_path_buf() } else { base.join(sub) };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if !(name.ends_with(".sh") || name == "start-all.sh") {
                continue;
            }
            if sub.is_empty() && !(name.starts_with("start") || name.starts_with("serve")) {
                continue;
            }
            let rel = if sub.is_empty() { name } else { format!("{sub}/{name}") };
            out.push(rel);
        }
    }
    out.sort();
    out
}

/// 收集 compose 文件里 `ports:` 段的宿主侧端口。
fn compose_ports(base: &Path) -> Vec<u16> {
    let mut files: Vec<String> = ["docker-compose.yml", "docker-compose.yaml", "compose.yaml", "compose.yml", "docker-compose.dev.yml"]
        .iter()
        .filter(|n| exists(base, n))
        .map(|s| s.to_string())
        .collect();
    files.sort();
    files.dedup();
    let mut out: Vec<u16> = Vec::new();
    for f in files {
        let Some(text) = read(base, &f) else { continue };
        let mut in_ports = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("ports:") {
                in_ports = true;
                continue;
            }
            if in_ports {
                if !(trimmed.starts_with('-') || trimmed.starts_with('"') || trimmed.starts_with('\'') || trimmed.is_empty()) {
                    in_ports = false;
                    continue;
                }
                // 形如 "- 127.0.0.1:5432:5432" / "- \"3000:3000\"" / "- ${PORT:-8080}:80"
                if let Some(p) = first_host_port(trimmed) {
                    out.push(p);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// 从一行 compose 端口映射里取宿主侧端口。
/// `127.0.0.1:5432:5432` → 5432；`3000:3000` → 3000；`${PORT:-8080}:80` → 8080。
fn first_host_port(line: &str) -> Option<u16> {
    let s = line.trim_start_matches('-').trim().trim_matches('"').trim_matches('\'');
    if s.is_empty() || s.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    // 宿主侧总是倒数第二段
    let host = parts[parts.len() - 2].trim();
    let host = match host.rsplit_once(":-") {
        // ${PORT:-8080}
        Some((_, default)) => default.trim_end_matches('}').trim(),
        None => host,
    };
    host.parse::<u16>().ok()
}

fn node_pin(base: &Path) -> (Option<String>, Vec<String>) {
    let mut problems = Vec::new();
    if let Some(text) = read(base, ".nvmrc") {
        let pin = text.trim().trim_start_matches('v').to_string();
        if !pin.is_empty() {
            return (Some(pin), problems);
        }
    }
    if let Some(text) = read(base, "package.json") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(expr) = v.get("engines").and_then(|e| e.get("node")).and_then(|n| n.as_str()) {
                if let Some(exact) = first_semver(expr) {
                    return (Some(exact), problems);
                }
                problems.push(format!("engines 要求 node {expr}（非精确版本，未自动选择）"));
            }
        }
    }
    (None, problems)
}

/// 从 `^22.18.0` / `>=24.16.0` 里取第一个 `X.Y.Z`。
fn first_semver(s: &str) -> Option<String> {
    let is_version = |t: &str| {
        let parts: Vec<&str> = t.split('.').collect();
        parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.parse::<u32>().is_ok())
    };
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            cur.push(c);
            continue;
        }
        if is_version(&cur) {
            return Some(std::mem::take(&mut cur));
        }
        cur.clear();
    }
    if is_version(&cur) {
        return Some(cur);
    }
    None
}

pub fn detect(base: &Path) -> DetectResult {
    let mut r = DetectResult::default();
    if !base.is_dir() {
        r.problems.push("项目路径不存在".into());
        return r;
    }

    let pm = package_manager(base);
    r.package_manager = pm.clone();

    // install / build
    if let Some(pm) = &pm {
        r.install = Some(vec![pm.clone(), "install".into()]);
        if script_of(base, "build").is_some() {
            r.build = Some(vec![pm.clone(), "run".into(), "build".into()]);
        }
    } else if exists(base, "uv.lock") || exists(base, "pyproject.toml") {
        r.install = Some(vec!["uv".into(), "sync".into()]);
    }

    // start 候选
    if let Some(pm) = &pm {
        let scripts = scripts_of(base);
        for key in ["dev", "start", "serve"] {
            if scripts.iter().any(|(k, _)| k == key) {
                let cand = if key == "start" {
                    vec![pm.clone(), "start".into()]
                } else {
                    vec![pm.clone(), "run".into(), key.to_string()]
                };
                if !r.start_candidates.contains(&cand) {
                    r.start_candidates.push(cand);
                }
            }
        }
        if r.start_candidates.is_empty() {
            r.problems.push("package.json 里没有 dev / start / serve 脚本".into());
        }
    }

    // dev_port
    let mut port = None;
    if let Some(pm) = &pm {
        let _ = pm;
        for key in ["dev", "start", "serve"] {
            if let Some(s) = script_of(base, key) {
                if let Some(p) = port_in_script(&s) {
                    port = Some(p);
                    break;
                }
            }
        }
    }
    if port.is_none() {
        for f in ["vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts"] {
            if let Some(text) = read(base, f) {
                if let Some(p) = port_in_config(&text) {
                    port = Some(p);
                    break;
                }
            }
        }
    }
    if port.is_none() {
        port = port_in_env_files(base);
    }
    let comp = compose_ports(base);
    if port.is_none() {
        port = comp.first().copied();
    }
    r.dev_port = port;
    r.requires_ports = comp;

    // node pin
    let (pin, mut pin_problems) = node_pin(base);
    r.node_pin = pin;
    r.problems.append(&mut pin_problems);

    // toolchain：根目录 + 一级子目录（monorepo 常见形态，如 sub2api/backend/go.mod）
    let mut tools: Vec<String> = Vec::new();
    let mut marker_dirs: Vec<PathBuf> = vec![base.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(base) {
        let mut subs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .map(|n| n.to_string_lossy().starts_with('.'))
                        .unwrap_or(false)
            })
            .collect();
        subs.sort();
        marker_dirs.extend(subs);
    }
    for d in &marker_dirs {
        if d.join("go.mod").is_file() {
            tools.push("go".into());
        }
        if d.join("Cargo.toml").is_file() {
            tools.push("cargo".into());
        }
        if d.join("pyproject.toml").is_file() {
            tools.push("uv".into());
        }
    }
    if let Some(pm) = &pm {
        tools.push(pm.clone());
    }
    tools.sort();
    tools.dedup();
    r.toolchain = tools;

    r.compose_stacks = compose_candidates(base);
    r.make_targets = make_targets(base);
    r.script_candidates = script_candidates(base);

    if pm.is_none() && r.start_candidates.is_empty() && r.compose_stacks.is_empty() {
        r.problems.push("未识别的技术栈：请手工填写启动命令".into());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn pnpm_lock_wins_over_package_lock() {
        let d = tmp();
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).unwrap();
        std::fs::write(d.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(d.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(detect(d.path()).package_manager, Some("pnpm".to_string()));
    }

    #[test]
    fn bun_lock_detected() {
        let d = tmp();
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"start":"bun run src/index.ts"}}"#).unwrap();
        std::fs::write(d.path().join("bun.lock"), "").unwrap();
        let r = detect(d.path());
        assert_eq!(r.package_manager, Some("bun".to_string()));
        assert_eq!(r.start_candidates, vec![vec!["bun".to_string(), "start".to_string()]]);
    }

    #[test]
    fn dev_port_from_script_flag() {
        let d = tmp();
        std::fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"dev":"vite --port 5190 --strictPort"}}"#,
        )
        .unwrap();
        let r = detect(d.path());
        assert_eq!(r.dev_port, Some(5190));
        assert_eq!(r.start_candidates, vec![vec!["npm".to_string(), "run".to_string(), "dev".to_string()]]);
    }

    #[test]
    fn dev_port_from_vite_config() {
        let d = tmp();
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).unwrap();
        std::fs::write(d.path().join("vite.config.ts"), "export default { server: { port: 5178 } }").unwrap();
        assert_eq!(detect(d.path()).dev_port, Some(5178));
    }

    #[test]
    fn dev_port_from_env_fallback() {
        let d = tmp();
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"start":"node x.js"}}"#).unwrap();
        std::fs::write(d.path().join(".env.example"), "FOO=1\nPORT=3333\n").unwrap();
        assert_eq!(detect(d.path()).dev_port, Some(3333));
    }

    #[test]
    fn compose_ports_become_requires_ports() {
        let d = tmp();
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).unwrap();
        std::fs::write(
            d.path().join("docker-compose.yml"),
            "services:\n  db:\n    image: pg\n    ports:\n      - 127.0.0.1:55432:5432\n      - \"59000:9000\"\n",
        )
        .unwrap();
        let r = detect(d.path());
        assert_eq!(r.requires_ports, vec![55432, 59000]);
        // dev_port 落到 compose 首个宿主端口
        assert_eq!(r.dev_port, Some(55432));
    }

    #[test]
    fn go_mod_adds_go_toolchain() {
        let d = tmp();
        std::fs::write(d.path().join("go.mod"), "module x\n").unwrap();
        let r = detect(d.path());
        assert!(r.toolchain.contains(&"go".to_string()));
        assert!(r.start_candidates.is_empty());
        assert!(r.problems.iter().any(|p| p.contains("未识别的技术栈")));
    }

    #[test]
    fn nvmrc_wins_over_engines() {
        let d = tmp();
        std::fs::write(d.path().join(".nvmrc"), "24.16.0\n").unwrap();
        std::fs::write(d.path().join("package.json"), r#"{"engines":{"node":">=22"},"scripts":{"dev":"vite"}}"#).unwrap();
        assert_eq!(detect(d.path()).node_pin, Some("24.16.0".to_string()));
    }

    #[test]
    fn engines_exact_version_used() {
        let d = tmp();
        std::fs::write(d.path().join("package.json"), r#"{"engines":{"node":"^22.18.0"},"scripts":{"dev":"vite"}}"#).unwrap();
        assert_eq!(detect(d.path()).node_pin, Some("22.18.0".to_string()));
    }

    #[test]
    fn engines_range_is_reported_not_guessed() {
        let d = tmp();
        std::fs::write(d.path().join("package.json"), r#"{"engines":{"node":">=23"},"scripts":{"dev":"vite"}}"#).unwrap();
        let r = detect(d.path());
        assert_eq!(r.node_pin, None);
        assert!(r.problems.iter().any(|p| p.contains("engines")));
    }

    #[test]
    fn nested_go_mod_adds_go_toolchain() {
        // sub2api 的真实形态：Makefile 在根，go.mod 在 backend/
        let d = tmp();
        std::fs::write(d.path().join("Makefile"), "build:\n\tmake -C backend build\n").unwrap();
        std::fs::create_dir_all(d.path().join("backend")).unwrap();
        std::fs::write(d.path().join("backend/go.mod"), "module x\n").unwrap();
        let r = detect(d.path());
        assert!(r.toolchain.contains(&"go".to_string()), "应识别子目录的 go.mod: {:?}", r.toolchain);
    }

    #[test]
    fn missing_dir_reports_problem() {
        let r = detect(Path::new("/definitely/not/here/xyz"));
        assert!(r.problems.iter().any(|p| p.contains("路径不存在")));
    }

    #[test]
    fn compose_services_and_overrides_are_detected() {
        let d = tmp();
        std::fs::create_dir_all(d.path().join("infra")).unwrap();
        std::fs::write(
            d.path().join("infra/docker-compose.yml"),
            "services:\n  postgres:\n    image: pg\n    ports:\n      - 127.0.0.1:55432:5432\n  minio:\n    image: minio\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("infra/docker-compose.gpu.yml"),
            "services:\n  postgres:\n    deploy:\n      resources: {}\n",
        )
        .unwrap();
        let r = detect(d.path());
        let base = r
            .compose_stacks
            .iter()
            .find(|c| c.file == "infra/docker-compose.yml")
            .expect("应识别 infra 下的 compose");
        assert_eq!(base.services, vec!["postgres".to_string(), "minio".to_string()]);
        assert_eq!(base.ports, vec![55432]);
        assert_eq!(base.overrides, vec!["infra/docker-compose.gpu.yml".to_string()]);
    }

    #[test]
    fn make_targets_and_scripts_are_listed() {
        let d = tmp();
        std::fs::write(
            d.path().join("Makefile"),
            "build:\n\tgo build\n.deploy:\n\t@echo\nVAR=1\ntest-unit:\n\techo\n",
        )
        .unwrap();
        std::fs::create_dir_all(d.path().join("bin")).unwrap();
        std::fs::write(d.path().join("bin/serve.sh"), "").unwrap();
        std::fs::write(d.path().join("start-all.sh"), "").unwrap();
        std::fs::write(d.path().join("unrelated.sh"), "").unwrap();
        let r = detect(d.path());
        assert_eq!(r.make_targets, vec!["build".to_string(), "test-unit".to_string()]);
        assert_eq!(r.script_candidates, vec!["bin/serve.sh".to_string(), "start-all.sh".to_string()]);
    }

    #[test]
    fn uv_project_gets_uv_sync() {
        let d = tmp();
        std::fs::write(d.path().join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        std::fs::write(d.path().join("uv.lock"), "").unwrap();
        let r = detect(d.path());
        assert_eq!(r.install, Some(vec!["uv".to_string(), "sync".to_string()]));
        assert!(r.toolchain.contains(&"uv".to_string()));
    }
}
