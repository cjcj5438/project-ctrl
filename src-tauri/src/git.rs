//! 只读 git 命令（`status` / `log`）的执行与输出处理。
//!
//! 安全约定：只允许 status 与 log；由调用方给出项目目录（命令层按 id 取），
//! 一律用 `Command` 参数数组，绝不拼 shell；设置环境变量避免 git 写索引、弹终端、开分页器。

use serde::Serialize;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 超时上限。
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// 单路输出上限 1MB。
pub const MAX_OUTPUT: usize = 1024 * 1024;
const TRUNCATE_NOTE: &str = "…输出已截断";

/// 允许的 git 子命令白名单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitKind {
    Status,
    Log,
}

impl GitKind {
    /// 只解析白名单值，绝不把用户输入透传给 git。
    pub fn parse(s: &str) -> Option<GitKind> {
        match s {
            "status" => Some(GitKind::Status),
            "log" => Some(GitKind::Log),
            _ => None,
        }
    }

    fn args(self) -> Vec<&'static str> {
        let mut v = vec!["-c", "color.ui=false", "-c", "core.quotepath=false"];
        match self {
            GitKind::Status => v.push("status"),
            GitKind::Log => v.extend([
                "--no-pager",
                "log",
                "-n",
                "30",
                "--date=short",
                "--pretty=format:%h %ad %an %s",
            ]),
        }
        v
    }
}

#[derive(Debug, Serialize)]
pub struct GitResult {
    /// 面板要显示的文本（stdout / stderr / 超时说明，必要时附截断提示）。
    pub text: String,
    pub ok: bool,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
}

/// 子进程一次执行的结果（保留 pid 便于测试确认超时被真正回收）。
pub struct RunOutcome {
    /// 子进程 pid（仅测试使用：确认超时后进程已被真正回收）。
    #[cfg(test)]
    pub pid: u32,
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// 优先用绝对路径 `/usr/bin/git`（macOS GUI 应用 PATH 精简，CLT 的 shim 可执行），
/// 不存在则回退到 `git`。
pub fn git_program() -> &'static str {
    if Path::new("/usr/bin/git").exists() {
        "/usr/bin/git"
    } else {
        "git"
    }
}

/// 执行外部程序并带超时；stdout/stderr 各用一个线程读，避免管道写满导致假死。
pub fn run_capture(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> std::io::Result<RunOutcome> {
    run_capture_env(
        program,
        args,
        cwd,
        timeout,
        &[
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_PAGER", "cat"),
        ],
    )
}

/// 同 `run_capture`，但由调用方决定附加的环境变量（不设置 `GIT_OPTIONAL_LOCKS` 之外的默认值）。
/// 用于需要写 `.git` 的命令（fetch / pull / clone）。
pub fn run_capture_env(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
    envs: &[(&str, &str)],
) -> std::io::Result<RunOutcome> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    #[cfg(not(test))]
    let _ = pid;
    let mut so = child.stdout.take().expect("stdout piped");
    let mut se = child.stderr.take().expect("stderr piped");
    let th_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let th_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });

    let start = Instant::now();
    let mut timed_out = false;
    let mut status = None;
    loop {
        match child.try_wait()? {
            Some(s) => {
                status = Some(s);
                break;
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait(); // 回收，确保进程真的没了
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let stdout = th_out.join().unwrap_or_default();
    let stderr = th_err.join().unwrap_or_default();
    Ok(RunOutcome {
        #[cfg(test)]
        pid,
        success: status.map(|s| s.success()).unwrap_or(false),
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
    })
}

/// 截到 UTF-8 字符边界，返回 (截断后文本, 是否发生截断)。
fn truncate_utf8(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// 把一次执行结果整理成面板文本。
pub(crate) fn build_result(out: RunOutcome) -> GitResult {
    let (stdout, t1) = truncate_utf8(String::from_utf8_lossy(&out.stdout).into_owned(), MAX_OUTPUT);
    let (stderr, t2) = truncate_utf8(String::from_utf8_lossy(&out.stderr).into_owned(), MAX_OUTPUT);
    let truncated = t1 || t2;

    let mut text = if out.timed_out {
        format!("命令超时（{} 秒），已终止进程。\n{stdout}", TIMEOUT.as_secs())
    } else if !out.success {
        if stderr.trim().is_empty() {
            stdout.clone()
        } else {
            stderr.clone()
        }
    } else {
        stdout.clone()
    };
    if out.success && !stderr.trim().is_empty() {
        text = format!("{text}\n{stderr}");
    }
    if truncated {
        text.push_str(&format!("\n{TRUNCATE_NOTE}\n"));
    }
    GitResult { text, ok: out.success, code: out.code, timed_out: out.timed_out, truncated }
}

/// 对项目目录执行只读 git 命令；任何失败都体现在返回文本里（不 panic）。
pub fn run(project: &Path, kind: GitKind) -> GitResult {
    let args = kind.args();
    match run_capture(git_program(), &args, project, TIMEOUT) {
        Err(e) => GitResult {
            text: format!("无法执行 git：{e}"),
            ok: false,
            code: None,
            timed_out: false,
            truncated: false,
        },
        Ok(out) => build_result(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn tmp() -> PathBuf {
        // macOS 的 `SystemTime::now()` 精度可能只有微秒级：并行测试会拿到相同的时间戳，
        // 于是两个测试共用同一个目录、`git init` 互相踩。计数器保证唯一。
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!(
            "pc-git-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos(),
            seq
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 用固定身份、禁用用户全局/系统配置执行 git（建仓、提交用）。
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

    fn init(dir: &Path) {
        git(dir, &["init", "-q"]);
    }

    #[test]
    fn kind_parse_is_whitelist_only() {
        assert_eq!(GitKind::parse("status"), Some(GitKind::Status));
        assert_eq!(GitKind::parse("log"), Some(GitKind::Log));
        assert_eq!(GitKind::parse("push"), None);
        assert_eq!(GitKind::parse("status; rm -rf /"), None);
        assert_eq!(GitKind::parse(""), None);
    }

    #[test]
    fn status_keeps_chinese_unquoted() {
        let d = tmp();
        init(&d);
        fs::write(d.join("中文 文件.txt"), "x").unwrap();
        let r = run(&d, GitKind::Status);
        assert!(r.ok, "status failed: {}", r.text);
        assert!(r.text.contains("中文 文件.txt"), "got: {}", r.text);
        assert!(!r.text.contains("\\344"), "八进制转义未避免: {}", r.text);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn log_format_and_limit() {
        let d = tmp();
        init(&d);
        fs::write(d.join("a.txt"), "1").unwrap();
        git(&d, &["add", "."]);
        for i in 0..33 {
            git(&d, &["commit", "-q", "--allow-empty", "-m", &format!("标题 {i}")]);
        }
        let r = run(&d, GitKind::Log);
        assert!(r.ok, "log failed: {}", r.text);
        let lines: Vec<&str> = r.text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 30, "应最多 30 条");
        // 格式：短hash 日期 作者 标题
        for l in &lines {
            let mut it = l.splitn(4, ' ');
            let hash = it.next().unwrap();
            let date = it.next().unwrap();
            let author = it.next().unwrap();
            let subject = it.next().unwrap();
            assert!(hash.len() >= 7 && hash.chars().all(|c| c.is_ascii_hexdigit()), "hash: {l}");
            assert_eq!(date.len(), 10, "date: {l}");
            assert!(!author.is_empty(), "author: {l}");
            assert!(subject.starts_with("标题"), "subject: {l}");
        }
        // 最新一条在首行
        assert!(lines[0].contains("标题 32"), "首行应为最新: {}", lines[0]);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn log_on_repo_without_commits_reports_error() {
        let d = tmp();
        init(&d);
        let r = run(&d, GitKind::Log);
        assert!(!r.ok, "无提交的 log 应失败");
        assert!(!r.text.trim().is_empty(), "错误说明不应为空");
        assert!(r.text.to_lowercase().contains("commit"), "got: {}", r.text);
        // status 仍正常
        let s = run(&d, GitKind::Status);
        assert!(s.ok, "status 应正常: {}", s.text);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn status_works_in_worktree_git_file() {
        let d = tmp();
        let repo = d.join("repo");
        fs::create_dir_all(&repo).unwrap();
        init(&repo);
        fs::write(repo.join("a.txt"), "1").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let wt = d.join("wt");
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        // worktree 的 .git 是文件
        assert!(wt.join(".git").is_file());
        let r = run(&wt, GitKind::Status);
        assert!(r.ok, "worktree status failed: {}", r.text);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn timeout_kills_and_reaps_child() {
        let d = tmp();
        let start = Instant::now();
        let out = run_capture("/bin/sleep", &["5"], &d, Duration::from_millis(100)).unwrap();
        assert!(out.timed_out, "应超时");
        assert!(start.elapsed() < Duration::from_secs(3), "应被立即杀掉而非等 5 秒");
        // 确认 pid 已不存在（kill -0 失败）
        let alive = Command::new("/bin/kill")
            .args(["-0", &out.pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "子进程 {} 应已被回收", out.pid);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn large_output_is_truncated_with_note() {
        let d = tmp();
        let out = run_capture("/usr/bin/seq", &["1", "200000"], &d, Duration::from_secs(20)).unwrap();
        assert!(out.success);
        assert!(out.stdout.len() > MAX_OUTPUT, "seq 输出应超过 1MB");
        let r = build_result(out);
        assert!(r.truncated);
        assert!(r.text.ends_with(&format!("{TRUNCATE_NOTE}\n")), "应追加截断提示");
        assert!(r.text.len() <= MAX_OUTPUT + TRUNCATE_NOTE.len() + 4);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn status_does_not_write_index() {
        let d = tmp();
        init(&d);
        fs::write(d.join("a.txt"), "1").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-q", "-m", "init"]);
        // 让工作区文件 mtime 与 index 记录不一致，制造 status 需要刷新 stat 的场景
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(d.join("a.txt"), "1").unwrap();

        let index = d.join(".git").join("index");
        let before = fs::metadata(&index).unwrap().modified().unwrap();
        let r = run(&d, GitKind::Status);
        assert!(r.ok, "{}", r.text);
        let after = fs::metadata(&index).unwrap().modified().unwrap();
        assert_eq!(before, after, "status 不应改写 .git/index（GIT_OPTIONAL_LOCKS=0）");
        assert!(!d.join(".git").join("index.lock").exists(), "不应留下 index.lock");
        fs::remove_dir_all(&d).ok();
    }
}
