//! 纯函数：扫描、自动 tag、remote 解析、日期格式化。全部可单元测试。

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 是否隐藏目录（名称以 `.` 开头）
pub fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

/// 项目显示名：文件夹名
pub fn project_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// 扫描 root 的一级子目录：跳过隐藏、跳过非目录、跳过已加入。
/// `already` 为已入库的路径集合（做尾部 `/` 归一化后比较）。
/// 返回按名称排序的 (name, path)。
pub fn scan_candidates(
    entries: Vec<(String, String, bool)>,
    already: &HashSet<String>,
) -> Vec<(String, String)> {
    let normalized: HashSet<String> = already.iter().map(|p| normalize_path(p)).collect();
    let mut out: Vec<(String, String)> = entries
        .into_iter()
        .filter(|(name, _, is_dir)| *is_dir && !is_hidden_name(name))
        .filter(|(_, path, _)| !normalized.contains(&normalize_path(path)))
        .map(|(name, path, _)| (name, path))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn normalize_path(p: &str) -> String {
    p.trim_end_matches('/').to_string()
}

/// birthtime 秒 -> `YYYY-MM-DD`（本机时区的日期）
pub fn format_date_ymd(secs: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// 解析 `.git/config` 中各 remote 的 URL：`{ remote名: url }`
pub fn extract_remote_urls(config_text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut current: Option<String> = None;
    for raw in config_text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            current = parse_remote_header(line);
        } else if let Some(name) = current.clone() {
            if let Some(url) = parse_url_line(line) {
                map.entry(name).or_insert(url);
            }
        }
    }
    map
}

fn parse_remote_header(line: &str) -> Option<String> {
    // [remote "origin"] / [remote "upstream"]
    let rest = line.strip_prefix("[remote \"")?;
    let name = rest.strip_suffix("\"]")?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn parse_url_line(line: &str) -> Option<String> {
    let (k, v) = line.split_once('=')?;
    if k.trim() == "url" {
        let url = v.trim().to_string();
        if url.is_empty() { None } else { Some(url) }
    } else {
        None
    }
}

/// GitHub remote URL -> 规范仓库页 `https://github.com/owner/repo`；非 github 返回 None。
/// 支持 `git@github.com:owner/repo(.git)`、`ssh://git@github.com/owner/repo(.git)`、
/// `https?://github.com/owner/repo(.git)(/)`。
pub fn github_repo_url(url: &str) -> Option<String> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        return Some(format!("https://github.com/{}", strip_git_suffix(rest)));
    }
    if let Some(rest) = strip_prefix_any(url, &["ssh://git@github.com/", "ssh://git@github.com:"]) {
        return Some(format!("https://github.com/{}", strip_git_suffix(rest)));
    }
    if let Some(rest) = strip_prefix_any(url, &["https://github.com/", "http://github.com/"]) {
        let path = strip_git_suffix(rest.trim_end_matches('/'));
        // 只接受 owner/repo 形状
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some(format!("https://github.com/{path}"));
        }
        return None;
    }
    None
}

fn strip_prefix_any<'a>(s: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| s.strip_prefix(p))
}

fn strip_git_suffix(s: &str) -> &str {
    s.trim_end_matches('/').strip_suffix(".git").unwrap_or(s.trim_end_matches('/'))
}

/// 取 GitHub 跳转 URL：优先 origin，否则第一个含 github.com 的 remote。
pub fn pick_github_url(remotes: &HashMap<String, String>) -> Option<String> {
    if let Some(origin) = remotes.get("origin") {
        if let Some(u) = github_repo_url(origin) {
            return Some(u);
        }
    }
    let mut names: Vec<&String> = remotes.keys().collect();
    names.sort();
    for n in names {
        if n == "origin" {
            continue;
        }
        if let Some(u) = github_repo_url(&remotes[n]) {
            return Some(u);
        }
    }
    None
}

/// 自动 tag。
/// - `git_kind`: None=无 .git；Some(true)=`.git` 是目录；Some(false)=`.git` 是文件（worktree/submodule）
/// - 有 .git 一定有 `git`；只有 `.git` 为目录时才读 config 找 `github`。
pub fn auto_tags(git_kind: Option<bool>, config_text: Option<&str>) -> Vec<String> {
    match git_kind {
        None => vec!["无git".to_string()],
        Some(false) => vec!["git".to_string()],
        Some(true) => {
            let mut tags = vec!["git".to_string()];
            if let Some(text) = config_text {
                let remotes = extract_remote_urls(text);
                if pick_github_url(&remotes).is_some() {
                    tags.push("github".to_string());
                }
            }
            tags
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scan_skips_hidden_non_dir_and_joined() {
        let entries = vec![
            ("a".into(), "/root/a".into(), true),
            (".hidden".into(), "/root/.hidden".into(), true),
            ("file.txt".into(), "/root/file.txt".into(), false),
            ("joined".into(), "/root/joined".into(), true),
            ("spaced name".into(), "/root/spaced name".into(), true),
        ];
        let got = scan_candidates(entries, &set(&["/root/joined/"]));
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "/root/a".to_string()),
                ("spaced name".to_string(), "/root/spaced name".to_string()),
            ]
        );
    }

    #[test]
    fn scan_empty_root_gives_empty() {
        let got = scan_candidates(vec![], &set(&[]));
        assert!(got.is_empty());
    }

    #[test]
    fn auto_tag_no_git() {
        assert_eq!(auto_tags(None, None), vec!["无git".to_string()]);
    }

    #[test]
    fn auto_tag_git_dir_without_remote() {
        assert_eq!(auto_tags(Some(true), Some("[core]\n\trepositoryformatversion = 0\n")), vec!["git".to_string()]);
    }

    #[test]
    fn auto_tag_git_dir_with_ssh_github_origin() {
        let cfg = "[remote \"origin\"]\n\turl = git@github.com:owner/repo.git\n";
        assert_eq!(
            auto_tags(Some(true), Some(cfg)),
            vec!["git".to_string(), "github".to_string()]
        );
    }

    #[test]
    fn auto_tag_git_dir_with_https_github_origin() {
        let cfg = "[remote \"origin\"]\n\turl = https://github.com/owner/repo.git\n";
        assert_eq!(
            auto_tags(Some(true), Some(cfg)),
            vec!["git".to_string(), "github".to_string()]
        );
    }

    #[test]
    fn auto_tag_git_dir_with_non_github_remote() {
        let cfg = "[remote \"origin\"]\n\turl = git@gitee.com:owner/repo.git\n";
        assert_eq!(auto_tags(Some(true), Some(cfg)), vec!["git".to_string()]);
    }

    #[test]
    fn auto_tag_git_file_never_github() {
        let cfg = "[remote \"origin\"]\n\turl = git@github.com:owner/repo.git\n";
        assert_eq!(auto_tags(Some(false), Some(cfg)), vec!["git".to_string()]);
    }

    #[test]
    fn github_url_shapes() {
        assert_eq!(
            github_repo_url("git@github.com:owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
        assert_eq!(
            github_repo_url("https://github.com/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
        assert_eq!(
            github_repo_url("https://github.com/owner/repo").as_deref(),
            Some("https://github.com/owner/repo")
        );
        assert_eq!(github_repo_url("git@gitee.com:owner/repo.git"), None);
        assert_eq!(github_repo_url("https://gitlab.com/owner/repo.git"), None);
    }

    #[test]
    fn pick_prefers_origin_then_first_github() {
        let mut m = HashMap::new();
        m.insert("aaa".into(), "git@github.com:a/first.git".into());
        m.insert("origin".into(), "git@github.com:o/primary.git".into());
        assert_eq!(
            pick_github_url(&m).as_deref(),
            Some("https://github.com/o/primary")
        );
        m.remove("origin");
        assert_eq!(
            pick_github_url(&m).as_deref(),
            Some("https://github.com/a/first")
        );
    }

    #[test]
    fn pick_ignores_non_github_origin() {
        let mut m = HashMap::new();
        m.insert("origin".into(), "git@gitee.com:o/x.git".into());
        m.insert("mirror".into(), "https://github.com/o/x.git".into());
        assert_eq!(
            pick_github_url(&m).as_deref(),
            Some("https://github.com/o/x")
        );
    }

    #[test]
    fn date_format_uses_local_date() {
        use chrono::{Local, TimeZone};
        let secs = 1704153600;
        let expected = Local
            .timestamp_opt(secs, 0)
            .single()
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(format_date_ymd(secs), expected);
        assert_eq!(format_date_ymd(secs).len(), 10);
    }

    #[test]
    fn project_name_is_folder_name() {
        assert_eq!(project_name_of(Path::new("/a/b/my-proj")), "my-proj");
        assert_eq!(project_name_of(Path::new("/a/未命名文件夹")), "未命名文件夹");
    }
}
