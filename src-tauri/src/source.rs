//! 源码更新：`fetch`（只读）、`pull --ff-only`（有前置条件）、`clone`。
//!
//! 安全约定：绝不提供 `reset --hard` / `stash` / `checkout`；`pull` 只在
//! 「有 upstream + 工作区干净 + 无未推送提交 + 落后 > 0」时才允许。
//! 所有 git 调用都走参数数组，不拼 shell。

use crate::git::{self, GitResult};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// fetch / pull / clone 的超时（网络操作，比只读命令宽松）。
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

/// fetch / pull 用的环境变量：**不设** `GIT_OPTIONAL_LOCKS=0`（需要写 `.git`）。
const SYNC_ENVS: &[(&str, &str)] = &[("GIT_TERMINAL_PROMPT", "0"), ("GIT_PAGER", "cat")];

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct SourceInfo {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub detached: bool,
    pub upstream: Option<String>,
    pub dirty: usize,
    pub ahead: u32,
    pub behind: u32,
    pub can_fetch: bool,
    pub can_pull: bool,
    /// 不可 pull 的唯一原因（可 pull 时为 None）。
    pub pull_blocked_by: Option<String>,
}

impl SourceInfo {
    fn not_repo() -> Self {
        Self {
            is_repo: false,
            branch: None,
            detached: false,
            upstream: None,
            dirty: 0,
            ahead: 0,
            behind: 0,
            can_fetch: false,
            can_pull: false,
            pull_blocked_by: Some("不是 git 仓库".to_string()),
        }
    }
}

fn run(project: &Path, args: &[&str]) -> Option<git::RunOutcome> {
    git::run_capture_env(git::git_program(), args, project, SYNC_TIMEOUT, SYNC_ENVS).ok()
}

fn stdout_trim(project: &Path, args: &[&str]) -> Option<String> {
    let out = run(project, args)?;
    if !out.success {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 由已知事实算出 `can_pull` 与唯一的阻止原因。判定顺序即此函数的分支顺序。
fn verdict(
    is_repo: bool,
    detached: bool,
    upstream: Option<&str>,
    dirty: usize,
    ahead: u32,
    behind: u32,
) -> (bool, Option<String>) {
    if !is_repo {
        return (false, Some("不是 git 仓库".to_string()));
    }
    if detached {
        return (false, Some("detached HEAD，需先 checkout 分支".to_string()));
    }
    if upstream.is_none() {
        return (false, Some("没有 upstream".to_string()));
    }
    if dirty > 0 {
        return (false, Some(format!("工作区有 {dirty} 个未提交改动")));
    }
    if ahead > 0 {
        return (false, Some(format!("有 {ahead} 个提交未推送")));
    }
    if behind == 0 {
        return (false, Some("已是最新".to_string()));
    }
    (true, None)
}

pub fn info(project: &Path) -> SourceInfo {
    // 用 `rev-parse --is-inside-work-tree` 判仓库：`.git` 存在但还没有首个提交时
    // `rev-parse --abbrev-ref HEAD` 会失败（unborn branch），不能据此判成「不是仓库」。
    let inside = stdout_trim(project, &["rev-parse", "--is-inside-work-tree"]);
    if !project.join(".git").exists() || inside.as_deref() != Some("true") {
        return SourceInfo::not_repo();
    }

    // 未出生的分支上 `symbolic-ref --short HEAD` 仍能给出分支名；失败即 detached。
    let branch = stdout_trim(project, &["symbolic-ref", "--short", "HEAD"]).filter(|s| !s.is_empty());
    let detached = branch.is_none();

    let upstream = stdout_trim(project, &["rev-parse", "--abbrev-ref", "@{upstream}"])
        .filter(|s| !s.is_empty() && s != "@{upstream}");

    let dirty = run(project, &["status", "--porcelain"])
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);

    let (ahead, behind) = match (
        upstream.as_deref(),
        run(project, &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"]),
    ) {
        (Some(_), Some(o)) if o.success => {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut it = text.split_whitespace();
            let b = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            let a = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            (a, b)
        }
        _ => (0, 0),
    };

    let (can_pull, blocked) = verdict(true, detached, upstream.as_deref(), dirty, ahead, behind);
    SourceInfo {
        is_repo: true,
        branch,
        detached,
        upstream,
        dirty,
        ahead,
        behind,
        can_fetch: true,
        can_pull,
        pull_blocked_by: blocked,
    }
}

pub fn fetch(project: &Path) -> GitResult {
    match run(project, &["fetch", "--prune", "--no-tags"]) {
        Some(out) => git::build_result(out),
        None => err_result("无法执行 git fetch"),
    }
}

/// `pull --ff-only`。调用前会用 `info` 复核，不满足条件则返回错误结果而不执行。
pub fn pull(project: &Path) -> GitResult {
    let s = info(project);
    if !s.can_pull {
        return err_result(&s.pull_blocked_by.unwrap_or_else(|| "当前不可 pull".to_string()));
    }
    match run(project, &["pull", "--ff-only"]) {
        Some(out) => git::build_result(out),
        None => err_result("无法执行 git pull"),
    }
}

fn err_result(text: &str) -> GitResult {
    GitResult {
        text: text.to_string(),
        ok: false,
        code: None,
        timed_out: false,
        truncated: false,
    }
}

/// 从仓库 URL 取目录名：最后一段去掉 `.git`。
fn repo_name_of(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() || name.contains(char::is_whitespace) {
        None
    } else {
        Some(name.to_string())
    }
}

fn url_allowed(url: &str) -> bool {
    let l = url.trim();
    ["https://", "http://", "ssh://", "git@"].iter().any(|p| l.starts_with(p))
}

/// 克隆到 `parent/<repo名>`，返回 (绝对路径, git 结果)。已存在则不覆盖。
pub fn clone(url: &str, parent: &str) -> Result<(String, GitResult), String> {
    if !url_allowed(url) {
        return Err("只支持 http(s) 或 ssh 的仓库地址".to_string());
    }
    let name = repo_name_of(url).ok_or_else(|| "无法从地址推断仓库名".to_string())?;
    let parent_dir = Path::new(parent);
    if !parent_dir.is_dir() {
        return Err(format!("目标目录不存在：{parent}"));
    }
    let dest = parent_dir.join(&name);
    if dest.exists() {
        return Err(format!("目标已存在：{}", dest.display()));
    }
    let dest_s = dest.to_string_lossy().into_owned();
    let out = git::run_capture_env(git::git_program(), &["clone", "--", url, &dest_s], parent_dir, SYNC_TIMEOUT, SYNC_ENVS)
        .map_err(|e| format!("无法执行 git clone：{e}"))?;
    let result = git::build_result(out);
    if !result.ok {
        return Ok((dest_s, result));
    }
    Ok((dest_s, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pc-src-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed ({:?}) in {dir:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    #[test]
    fn blocked_reasons_priority() {
        // detached 优先于「没有 upstream」与「脏」
        let (ok, why) = verdict(true, true, None, 5, 0, 3);
        assert!(!ok);
        assert_eq!(why.unwrap(), "detached HEAD，需先 checkout 分支");
    }

    #[test]
    fn blocked_reasons_order() {
        assert_eq!(verdict(false, false, None, 0, 0, 0).1.unwrap(), "不是 git 仓库");
        assert_eq!(verdict(true, false, None, 0, 0, 0).1.unwrap(), "没有 upstream");
        assert_eq!(verdict(true, false, Some("o/m"), 7, 0, 3).1.unwrap(), "工作区有 7 个未提交改动");
        assert_eq!(verdict(true, false, Some("o/m"), 0, 2, 3).1.unwrap(), "有 2 个提交未推送");
        assert_eq!(verdict(true, false, Some("o/m"), 0, 0, 0).1.unwrap(), "已是最新");
        let (ok, why) = verdict(true, false, Some("o/m"), 0, 0, 4);
        assert!(ok);
        assert!(why.is_none());
    }

    #[test]
    fn non_repo_dir_reports_not_repo() {
        let d = tmp();
        let s = info(&d);
        assert!(!s.is_repo);
        assert!(!s.can_fetch);
        assert_eq!(s.pull_blocked_by.as_deref(), Some("不是 git 仓库"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn clean_repo_without_upstream_reports_no_upstream() {
        let d = tmp();
        git(&d, &["init", "-q"]);
        fs::write(d.join("a.txt"), "1").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-q", "-m", "init"]);
        let s = info(&d);
        assert!(s.is_repo);
        assert!(!s.detached);
        assert!(s.branch.is_some(), "非 detached 必须有分支名: {:?}", s.branch);
        assert!(s.upstream.is_none(), "本地 init 无 upstream: {:?}", s.upstream);
        assert_eq!(s.dirty, 0, "刚提交应干净");
        assert_eq!(s.pull_blocked_by.as_deref(), Some("没有 upstream"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn dirty_and_detached_detected() {
        let d = tmp();
        git(&d, &["init", "-q"]);
        fs::write(d.join("a.txt"), "1").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-q", "-m", "init"]);
        git(&d, &["commit", "-q", "--allow-empty", "-m", "second"]);
        git(&d, &["checkout", "-q", "--detach", "HEAD~1"]);
        fs::write(d.join("a.txt"), "2").unwrap();
        let s = info(&d);
        assert!(s.detached);
        assert_eq!(s.dirty, 1);
        assert_eq!(s.pull_blocked_by.as_deref(), Some("detached HEAD，需先 checkout 分支"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn pull_refuses_when_blocked() {
        let d = tmp();
        git(&d, &["init", "-q"]);
        let r = pull(&d);
        assert!(!r.ok);
        assert!(r.text.contains("没有 upstream"), "got: {}", r.text);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn repo_name_extraction() {
        assert_eq!(repo_name_of("https://github.com/a/b.git").as_deref(), Some("b"));
        assert_eq!(repo_name_of("git@github.com:a/b.git").as_deref(), Some("b"));
        assert_eq!(repo_name_of("https://github.com/a/b/").as_deref(), Some("b"));
        assert_eq!(repo_name_of(""), None);
    }

    #[test]
    fn url_whitelist() {
        assert!(url_allowed("https://x/y.git"));
        assert!(url_allowed("git@github.com:a/b.git"));
        assert!(!url_allowed("/etc/passwd"));
        assert!(!url_allowed("file:///tmp/x"));
    }

    #[test]
    fn clone_rejects_bad_inputs() {
        let parent = tempfile::tempdir().unwrap();
        let p = parent.path().to_string_lossy().into_owned();
        assert!(clone("/etc/passwd", &p).is_err());
        assert!(clone("https://x/y.git", "/definitely/not/here").is_err());
    }
}
