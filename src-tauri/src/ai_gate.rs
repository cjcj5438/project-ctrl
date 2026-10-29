//! AI 工具调用的只读权限闸门。
//!
//! 纯逻辑：给定项目根目录、工具名、工具入参，判断是否放行。不碰文件系统之外的任何状态，
//! 也不执行命令——真正执行在别处。所有拒绝理由都是给用户看的中文。
//!
//! 三条主线：
//! - 读类工具（read/grep/find/ls）只允许项目目录内、且不是密钥文件；
//! - bash 只允许白名单里的只读命令（版本查询、依赖信息、只读 git、端口查询等）；
//! - 其余工具（edit/write/powershell/未知）一律拒绝——只读模式。

// 本模块是分阶段接入的：闸门要在后续步骤挂到 AI 工具调用链路上，现在先由单元测试保证行为正确。
#![allow(dead_code)]

use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// 闸门结论。`patch` 用于给工具加约束（目前只有 bash 的超时上限）。
#[derive(Debug, serde::Serialize)]
pub struct GateDecision {
    pub allow: bool,
    pub reason: Option<String>,
    pub patch: Option<Value>,
}

fn allow() -> GateDecision {
    GateDecision { allow: true, reason: None, patch: None }
}

fn deny(reason: String) -> GateDecision {
    GateDecision { allow: false, reason: Some(reason), patch: None }
}

/// 词法归一：去掉 `.`，`..` 弹一层。弹到顶（越过根）返回 `None`——那就是出界。
fn lexical(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// 归一 + 解析软链接后判断 `p` 是否落在 `root` 之内，落在里面则给出解析后的路径。
///
/// 只对「存在的最长前缀」调 `canonicalize`：用户可能指向一个还没创建的文件，
/// 整条路径 canonicalize 会失败；而软链接逃逸恰恰出现在已存在的那段前缀上。
fn contained(root: &Path, p: &Path) -> Option<PathBuf> {
    let root_norm = lexical(root).unwrap_or_else(|| root.to_path_buf());
    let joined = if p.is_absolute() { p.to_path_buf() } else { root_norm.join(p) };
    let norm = lexical(&joined)?;

    let mut prefix = norm.as_path();
    let mut suffix: Vec<&OsStr> = Vec::new();
    let resolved = loop {
        if let Ok(mut real) = prefix.canonicalize() {
            for s in suffix.iter().rev() {
                real.push(s);
            }
            break real;
        }
        suffix.push(prefix.file_name()?);
        prefix = prefix.parent()?;
    };

    let real_root = root.canonicalize().unwrap_or(root_norm);
    resolved.starts_with(&real_root).then_some(resolved)
}

/// 密钥类文件名：这类文件即便是项目内的也拒绝读取。
fn is_secret(name: &str) -> bool {
    let env_like = name == ".env" || name.starts_with(".env.");
    if env_like && !matches!(name, ".env.example" | ".env.sample" | ".env.template" | ".env.dist") {
        return true;
    }
    if matches!(name, ".npmrc" | ".netrc" | ".pypirc") {
        return true;
    }
    let by_ext = Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "pem" | "key" | "p12"))
        .unwrap_or(false);
    by_ext || name.starts_with("id_rsa") || name.starts_with("id_ed25519")
}

fn secret_reason(name: &str) -> String {
    format!("出于安全，AI 不能读取 {name}（可能含密钥）；变量名可通过 pc_project_context 获取")
}

/// 读类工具：路径必须在项目内，且不能是密钥文件。
fn decide_read(root: &Path, tool: &str, input: &Value) -> GateDecision {
    let raw = input.get("path").and_then(Value::as_str);
    let raw = match (tool, raw) {
        ("read", None) => return deny("read 需要字符串参数 path".to_string()),
        ("read", Some(p)) => p,
        // grep/find/ls 不传 path 时从项目根开始。
        (_, None) => "",
        (_, Some(p)) => p,
    };

    let p = if raw.is_empty() { root } else { Path::new(raw) };
    let Some(resolved) = contained(root, p) else {
        let shown = if raw.is_empty() { root.display().to_string() } else { raw.to_string() };
        return deny(format!("只能访问项目目录内的文件：{shown}"));
    };

    let mut names: Vec<String> = Vec::new();
    if let Some(n) = resolved.file_name().and_then(OsStr::to_str) {
        names.push(n.to_string());
    }
    // grep 的 glob 也会命中同类文件，取它最后一段一起判。
    if tool == "grep" {
        if let Some(g) = input.get("glob").and_then(Value::as_str) {
            if let Some(last) = g.rsplit('/').next().filter(|s| !s.is_empty()) {
                names.push(last.to_string());
            }
        }
    }
    if let Some(bad) = names.iter().find(|n| is_secret(n)) {
        return deny(secret_reason(bad));
    }
    allow()
}

/// 单引号/双引号成对剥离，按空白切词；引号不闭合视为可疑输入。
fn tokenize(cmd: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    for ch in cmd.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    cur.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    in_token = true;
                }
                c if c.is_whitespace() => {
                    if in_token {
                        out.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                }
                c => {
                    cur.push(c);
                    in_token = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_token {
        out.push(cur);
    }
    Some(out)
}

const VERSION_PROGRAMS: &[&str] = &[
    "node", "npm", "pnpm", "yarn", "bun", "deno", "go", "cargo", "rustc", "python", "python3",
    "pip", "pip3", "uv", "java", "mvn", "gradle", "ruby", "php", "docker", "git",
];

const GIT_READ_ONLY: &[&str] = &[
    "status", "log", "diff", "show", "rev-parse", "ls-files", "describe", "blame", "shortlog",
];

const GIT_BRANCH_FLAGS: &[&str] =
    &["-a", "-r", "-v", "-vv", "--list", "--show-current", "--verbose", "--all"];

/// ① 版本查询。必须先于 ② 判断：`pnpm`/`npm` 等同时属于两类，`pnpm outdated` 不能被
/// 「version 查询」那条提前截断。
fn is_version_query(argv: &[String]) -> bool {
    argv.len() == 2
        && VERSION_PROGRAMS.contains(&argv[0].as_str())
        && matches!(argv[1].as_str(), "--version" | "-v" | "-V" | "version")
}

/// 是不是一条只读的白名单命令。
fn bash_allowed(argv: &[String]) -> bool {
    let Some(prog) = argv.first() else { return false };
    let rest = &argv[1..];
    let word = |i: usize| rest.get(i).map(String::as_str);

    if is_version_query(argv) {
        return true;
    }
    match prog.as_str() {
        // ② 依赖 / 包信息
        "npm" | "pnpm" | "yarn" | "bun" => {
            matches!(word(0), Some("outdated" | "ls" | "list" | "why" | "view" | "info"))
        }
        "pip" | "pip3" => word(0) == Some("list"),
        "uv" => word(0) == Some("pip") && word(1) == Some("list"),
        "cargo" => word(0) == Some("tree"),
        "go" => matches!(word(0), Some("list" | "env")),
        // ③ 只读 git
        "git" => match word(0) {
            Some(sub) if GIT_READ_ONLY.contains(&sub) => true,
            Some("branch" | "remote") => rest[1..].iter().all(|a| GIT_BRANCH_FLAGS.contains(&a.as_str())),
            _ => false,
        },
        // ④ 只读 docker
        "docker" => match word(0) {
            Some("ps" | "images" | "version" | "info") => true,
            Some("compose") => matches!(word(1), Some("ps" | "config" | "ls")),
            _ => false,
        },
        // ⑤ 端口查询
        "lsof" => {
            argv.len() == 4
                && argv[1] == "-nP"
                && argv[3] == "-sTCP:LISTEN"
                && (argv[2] == "-iTCP"
                    || argv[2]
                        .strip_prefix("-iTCP:")
                        .map(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
                        .unwrap_or(false))
        }
        // ⑥ 杂项
        "which" => argv.len() == 2 && !rest[0].starts_with('-'),
        "uname" => argv.len() == 2 && argv[1] == "-a",
        "sw_vers" => argv.len() == 1,
        _ => false,
    }
}

fn decide_bash(input: &Value) -> GateDecision {
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return deny("bash 需要字符串参数 command".to_string());
    };
    let reject = || {
        deny(format!(
            "bash 只允许只读白名单命令（版本/依赖查询、只读 git、端口查询）；已拦截：{command}"
        ))
    };

    // shell 元字符一律拒绝：没有它们就拼不出重定向、管道、子命令。
    if command.chars().any(|c| matches!(c, ';' | '&' | '|' | '<' | '>' | '$' | '`' | '(' | ')' | '{' | '}' | '\\' | '\n' | '\r')) {
        return reject();
    }
    let Some(argv) = tokenize(command) else { return reject() };
    let Some(prog) = argv.first() else { return reject() };
    if prog.is_empty() || prog.contains('/') {
        return reject();
    }
    if argv.iter().any(|a| a.starts_with("--output")) {
        return reject();
    }
    if !bash_allowed(&argv) {
        return reject();
    }

    let timeout = input
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(120)
        .min(120);
    GateDecision { allow: true, reason: None, patch: Some(json!({ "timeout": timeout })) }
}

/// 判断一次工具调用是否放行。
pub fn decide(root: &Path, tool: &str, input: &Value) -> GateDecision {
    if tool.starts_with("pc_") {
        return allow();
    }
    match tool {
        "read" | "grep" | "find" | "ls" => decide_read(root, tool, input),
        "bash" => decide_bash(input),
        other => deny(format!("只读模式：AI 不能修改文件或执行该工具（{other}）")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn root() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(d.path().join(".env"), "SECRET=1\n").unwrap();
        fs::write(d.path().join(".env.example"), "SECRET=\n").unwrap();
        fs::write(d.path().join(".env.local"), "SECRET=2\n").unwrap();
        fs::write(d.path().join("key.pem"), "x\n").unwrap();
        d
    }

    fn reason(d: &GateDecision) -> String {
        d.reason.clone().unwrap_or_default()
    }

    #[test]
    fn bash_白名单表() {
        let root = root();
        let cases: &[(&str, bool)] = &[
            ("pnpm outdated", true),
            ("npm outdated", true),
            ("npm ls --depth=0", true),
            ("git status", true),
            ("git log --oneline -5", true),
            ("git diff", true),
            ("git branch -a", true),
            ("git remote -v", true),
            ("node -v", true),
            ("node --version", true),
            ("python3 --version", true),
            ("cargo --version", true),
            ("lsof -nP -iTCP -sTCP:LISTEN", true),
            ("lsof -nP -iTCP:3000 -sTCP:LISTEN", true),
            ("docker compose ps", true),
            ("docker ps -a", true),
            ("which node", true),
            ("uname -a", true),
            ("sw_vers", true),
            ("go list ./...", true),
            ("pip list", true),
            ("uv pip list", true),
            ("cargo tree", true),
            ("rm -rf x", false),
            ("git commit -m x", false),
            ("git push", false),
            ("git add .", false),
            ("git fetch", false),
            ("git tag", false),
            ("pnpm install", false),
            ("npm run build", false),
            ("cat .env", false),
            ("ls | head", false),
            ("echo $HOME", false),
            ("git diff --output=x", false),
            ("git status --output x", false),
            ("git branch -D main", false),
            ("git branch -d main", false),
            ("node --version; rm x", false),
            ("./script.sh", false),
            ("node --version && rm x", false),
            ("node --version > out", false),
            ("uname", false),
            ("uname -r", false),
            ("which", false),
            ("which -a node", false),
            ("lsof -i :3000", false),
            ("docker rm x", false),
            ("docker compose down", false),
            ("", false),
            ("/bin/ls", false),
            ("node 'unterminated", false),
            ("go env --output=t", false),
        ];
        for (cmd, expect) in cases {
            let d = decide(root.path(), "bash", &json!({ "command": cmd }));
            assert_eq!(d.allow, *expect, "命令 `{cmd}` 期望 allow={expect}，实际：{d:?}");
            if !expect {
                assert!(reason(&d).contains("bash 只允许只读白名单命令"), "命令 `{cmd}` 理由：{d:?}");
            }
        }
    }

    #[test]
    fn bash_超时补丁封顶_120() {
        let root = root();
        let d = decide(root.path(), "bash", &json!({ "command": "pnpm outdated" }));
        assert_eq!(d.patch.as_ref().unwrap()["timeout"], json!(120));

        let d = decide(root.path(), "bash", &json!({ "command": "pnpm outdated", "timeout": 5000 }));
        assert_eq!(d.patch.as_ref().unwrap()["timeout"], json!(120));

        let d = decide(root.path(), "bash", &json!({ "command": "pnpm outdated", "timeout": 30 }));
        assert_eq!(d.patch.as_ref().unwrap()["timeout"], json!(30));
    }

    #[test]
    fn 读类工具的路径与密钥规则() {
        let dir = root();
        let root = dir.path();
        // 项目内软链接指向项目外：必须拦截（词法上没出界，解析后才出界）。
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "x\n").unwrap();
        symlink(outside.path(), root.join("escape")).unwrap();

        let cases: Vec<(&str, Value, bool)> = vec![
            ("read", json!({"path": "src/main.rs"}), true),
            ("read", json!({"path": "src"}), true),
            ("read", json!({"path": "."}), true),
            ("grep", json!({"pattern": "main"}), true),
            ("grep", json!({"pattern": "main", "path": "src"}), true),
            ("find", json!({"name": "*.rs"}), true),
            ("ls", json!({"path": ""}), true),
            ("read", json!({"path": ".env"}), false),
            ("read", json!({"path": ".env.local"}), false),
            ("read", json!({"path": ".env.example"}), true),
            ("read", json!({"path": "key.pem"}), false),
            ("grep", json!({"pattern": "x", "path": ".env"}), false),
            ("grep", json!({"pattern": "x", "path": "src", "glob": "**/*.pem"}), false),
            ("read", json!({"path": "../x"}), false),
            ("read", json!({"path": "/etc/hosts"}), false),
            ("grep", json!({"pattern": "x", "path": "../../.."}), false),
            ("read", json!({"path": "escape/secret.txt"}), false),
            ("read", json!({"path": "escape"}), false),
            ("read", json!({}), false),
            ("read", json!({"path": 42}), false),
        ];
        for (tool, input, expect) in cases {
            let d = decide(root, tool, &input);
            assert_eq!(d.allow, expect, "{tool} {input} 期望 allow={expect}，实际：{d:?}");
        }

        let d = decide(root, "read", &json!({"path": ".env"}));
        assert_eq!(
            reason(&d),
            "出于安全，AI 不能读取 .env（可能含密钥）；变量名可通过 pc_project_context 获取"
        );
        let d = decide(root, "read", &json!({"path": "/etc/hosts"}));
        assert_eq!(reason(&d), "只能访问项目目录内的文件：/etc/hosts");
    }

    #[test]
    fn 其余工具与_pc_工具() {
        let dir = root();
        let root = dir.path();
        for tool in ["edit", "write", "powershell", "some_unknown_tool"] {
            let d = decide(root, tool, &json!({}));
            assert!(!d.allow, "{tool} 应被拒绝");
            assert_eq!(reason(&d), format!("只读模式：AI 不能修改文件或执行该工具（{tool}）"));
        }
        let d = decide(root, "pc_propose_services", &json!({}));
        assert!(d.allow);
        assert!(d.reason.is_none() && d.patch.is_none());
    }
}
