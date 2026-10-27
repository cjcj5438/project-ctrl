//! 子进程执行环境：显式 PATH，不经过任何 shell。
//!
//! 为什么不用 `zsh -lc` / `zsh -ic`：`zsh -lc` 只读 `.zshenv` / `.zprofile`，拿不到
//! `~/.bun/bin`（bun 项目会直接 MISSING）；`zsh -ic` 会读 `.zshrc`，而 `.zshrc` 里有明文密钥，
//! 会让每个子进程都继承它。这里改用固定 PATH 名单，并在需要时把 nvm 版本目录前置。
//!
//! 子进程环境用 `env_clear()` 后重建：唯一的环境变量来源是 `ServiceConfig::env` 与本模块的常量。

use crate::services::ServiceConfig;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 收到 TERM 后等待优雅退出的时长。
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// 安装 / 构建步骤的总超时（30 分钟）。
pub const STEP_TIMEOUT: Duration = Duration::from_secs(1800);

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

/// 该配方实际使用的 PATH：pin 可解析则把对应 bin 目录前置，否则用基础 PATH。
pub fn path_for(cfg: &ServiceConfig) -> String {
    match cfg.node_pin.as_deref().and_then(node_bin_dir) {
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
/// `extra` 通常来自 `ports::plan_argv` 的端口注入，优先级高于配方自报的 `env`。
pub fn env_for(cfg: &ServiceConfig, extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![
        ("PATH".into(), path_for(cfg)),
        ("TERM".into(), "dumb".into()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("NO_COLOR".into(), "1".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ];
    if let Some(h) = home() {
        out.push(("HOME".into(), h.display().to_string()));
    }
    for (k, v) in &cfg.env {
        out.push((k.clone(), v.clone()));
    }
    for (k, v) in extra {
        match out.iter_mut().find(|(ek, _)| ek == k) {
            Some(slot) => slot.1 = v.clone(),
            None => out.push((k.clone(), v.clone())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn path_has_bun_and_cargo() {
        let p = base_path();
        assert!(p.contains("/opt/homebrew/bin"), "{p}");
        if let Some(h) = home() {
            assert!(p.contains(&format!("{}/.bun/bin", h.display())), "{p}");
            assert!(p.contains(&format!("{}/.cargo/bin", h.display())), "{p}");
        }
    }

    #[test]
    fn node_bin_dir_prefers_pin() {
        // 本机实测存在 v22.22.0；不存在的版本必须返回 None
        if node_bin_dir("22.22.0").is_some() {
            assert!(node_bin_dir("99.0.0").is_none());
            assert!(node_bin_dir("v22.22.0").is_some());
        }
        assert!(node_bin_dir("").is_none());
    }

    #[test]
    fn env_for_clears_inherited_secrets() {
        std::env::set_var("PROJECT_CTRL_TEST_KEY", "secret");
        let env = env_for(&ServiceConfig::default(), &[]);
        assert!(!env.iter().any(|(k, _)| k == "PROJECT_CTRL_TEST_KEY"));
        assert!(env.iter().any(|(k, v)| k == "PATH" && !v.is_empty()));
        std::env::remove_var("PROJECT_CTRL_TEST_KEY");
    }

    #[test]
    fn extra_env_overrides_recipe_env() {
        let mut env = BTreeMap::new();
        env.insert("PORT".to_string(), "8080".to_string());
        env.insert("NODE_ENV".to_string(), "development".to_string());
        let cfg = ServiceConfig { env, ..Default::default() };
        let got = env_for(&cfg, &[("PORT".to_string(), "8081".to_string())]);
        assert_eq!(got.iter().filter(|(k, _)| k == "PORT").count(), 1);
        assert!(got.iter().any(|(k, v)| k == "PORT" && v == "8081"));
        assert!(got.iter().any(|(k, v)| k == "NODE_ENV" && v == "development"));
    }

    #[test]
    fn resolve_finds_system_binaries() {
        let path = base_path();
        assert!(resolve("sh", &path).is_some() || resolve("ls", &path).is_some());
        assert!(resolve("definitely-not-a-real-binary-xyz", &path).is_none());
        assert!(resolve("", &path).is_none());
    }
}
