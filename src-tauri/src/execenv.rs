//! 子进程执行环境：显式 PATH，不经过任何 shell。
//!
//! 为什么不用 `zsh -lc` / `zsh -ic`：`zsh -lc` 只读 `.zshenv` / `.zprofile`，拿不到
//! `~/.bun/bin`（bun 项目会直接 MISSING）；`zsh -ic` 会读 `.zshrc`，而 `.zshrc` 里有明文密钥，
//! 会让每个子进程都继承它。这里改用固定 PATH 名单，并在需要时把 nvm 版本目录前置。
//!
//! 子进程环境用 `env_clear()` 后重建：唯一环境来源是声明的 `env`、`env_files` 解析结果与本模块常量。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 收到 TERM 后等待优雅退出的时长。
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// 固定 PATH 前缀（顺序即优先级）。`/usr/bin` 与 `/bin` 覆盖系统自带工具。
const PATH_PREFIX: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// 基础 PATH：系统目录 + `~/.bun/bin` + `~/.cargo/bin`。
/// 不存在的目录也保留（无害），避免每次调用都做 stat。
pub fn base_path() -> String {
    match home() {
        Some(h) => format!("{PATH_PREFIX}:{}/.bun/bin:{}/.cargo/bin", h.display(), h.display()),
        None => PATH_PREFIX.to_string(),
    }
}

/// `~/.nvm/versions/node/v<pin>/bin`，目录存在才返回。
pub fn node_bin_dir(pin: &str) -> Option<PathBuf> {
    let pin = pin.trim().trim_start_matches('v');
    if pin.is_empty() {
        return None;
    }
    let dir = home()?.join(".nvm/versions/node").join(format!("v{pin}")).join("bin");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// 该服务的 PATH：pin 可解析则把对应 bin 目录前置，否则用基础 PATH。
pub fn path_for(pin: Option<&str>) -> String {
    match pin.and_then(node_bin_dir) {
        Some(dir) => format!("{}:{}", dir.display(), base_path()),
        None => base_path(),
    }
}

/// 在给定 PATH 里找可执行文件（等价于 `which`，但不依赖外部命令）。
/// `program` 含 `/` 时按路径直接判断。
pub fn resolve(program: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| -> bool {
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    };
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let p = PathBuf::from(program);
        return if is_exec(&p) { Some(p) } else { None };
    }
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(program);
        if is_exec(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 子进程的完整环境变量集合。调用方必须先 `env_clear()`。
/// 优先级（低→高）：模块常量 < 声明的 `env` < `extra`（端口注入等）。
pub fn env_for(
    pin: Option<&str>,
    env: &BTreeMap<String, String>,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![
        ("PATH".into(), path_for(pin)),
        ("TERM".into(), "dumb".into()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("NO_COLOR".into(), "1".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ];
    if let Some(h) = home() {
        out.push(("HOME".into(), h.display().to_string()));
    }
    let upsert = |out: &mut Vec<(String, String)>, k: &str, v: &str| match
        out.iter_mut().find(|(ek, _)| ek == k)
    {
        Some(slot) => slot.1 = v.to_string(),
        None => out.push((k.to_string(), v.to_string())),
    };
    for (k, v) in env {
        upsert(&mut out, k, v);
    }
    for (k, v) in extra {
        upsert(&mut out, k, v);
    }
    out
}

/// 从 env 文件里读出键值（只做最小解析：`KEY=VALUE`、忽略空行与 `#` 注释、
/// 去除成对的引号、支持 `export ` 前缀）。
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let v = v.trim();
        let v = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
        {
            &v[1..v.len() - 1]
        } else {
            // 去掉行尾注释（仅当 # 前有空白，避免破坏 URL 里的 #）
            v.split_once(" #").map(|(a, _)| a.trim_end()).unwrap_or(v)
        };
        out.push((k.to_string(), v.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_finds_system_binary() {
        let p = base_path();
        assert!(resolve("ls", &p).is_some(), "PATH 里应该有 ls");
        assert!(resolve("/nonexistent/xyz", &p).is_none());
    }

    #[test]
    fn env_priority_extra_wins_over_declared() {
        let mut env = BTreeMap::new();
        env.insert("PORT".to_string(), "1111".to_string());
        env.insert("MODE".to_string(), "dev".to_string());
        let out = env_for(None, &env, &[("PORT".to_string(), "2222".to_string())]);
        let get = |k: &str| out.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PORT"), Some("2222"));
        assert_eq!(get("MODE"), Some("dev"));
        assert_eq!(get("NO_COLOR"), Some("1"));
    }

    #[test]
    fn parse_env_file_handles_quotes_comments_and_export() {
        let parsed = parse_env_file(
            "# comment\nexport FOO=bar\nQUOTED=\"a b\"\nURL=https://x/#frag\nEMPTY=\nBROKEN\n",
        );
        let get = |k: &str| parsed.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get("FOO"), Some("bar"));
        assert_eq!(get("QUOTED"), Some("a b"));
        assert_eq!(get("URL"), Some("https://x/#frag"));
        assert_eq!(get("EMPTY"), Some(""));
        assert_eq!(get("BROKEN"), None);
    }
}
