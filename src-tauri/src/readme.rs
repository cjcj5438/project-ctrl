//! README 读写、生成与相对路径图片解析。
//!
//! 所有函数以「项目目录」为作用域，不依赖数据库，便于单元测试；命令层负责由 id 取目录。
//! 硬约束：对项目目录只写 README 文件本身，绝不创建临时/备份文件。

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 图片大小上限 10MB。
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Serialize, PartialEq)]
pub struct ReadmeInfo {
    pub exists: bool,
    pub filename: Option<String>,
    pub content: Option<String>,
    pub mtime: Option<String>,
}

/// 命令错误：`conflict` 可被前端识别（保存时磁盘 mtime 已变），其余为普通错误消息。
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadmeError {
    Conflict { current_mtime: String },
    Message { message: String },
}

fn msg(m: impl Into<String>) -> ReadmeError {
    ReadmeError::Message { message: m.into() }
}

impl ReadmeError {
    /// 汇总为普通错误字符串（`Conflict` 场景由命令层按需单独处理）。
    pub fn into_message(self) -> String {
        match self {
            ReadmeError::Message { message } => message,
            ReadmeError::Conflict { current_mtime } => {
                format!("文件已被外部修改（mtime={current_mtime}）")
            }
        }
    }
}

/// 在一批文件名中精确挑出 README：优先 `README.md`，其次 `readme.md`，否则 None。
/// 大小写敏感——APFS 默认大小写不敏感，不能用 `exists()` 判断，否则会把 `Readme.md` 混进来。
pub fn pick_readme_name(names: &[String]) -> Option<String> {
    if names.iter().any(|n| n == "README.md") {
        return Some("README.md".to_string());
    }
    if names.iter().any(|n| n == "readme.md") {
        return Some("readme.md".to_string());
    }
    None
}

/// 读取项目一级目录，精确查找 README 文件名。
pub fn find_readme(base: &Path) -> std::io::Result<Option<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        if let Some(n) = entry.file_name().to_str() {
            names.push(n.to_string());
        }
    }
    Ok(pick_readme_name(&names))
}

/// `SystemTime` -> 纳秒精度整数字符串（避免 JS number 精度丢失与毫秒级误判）。
pub fn mtime_string(t: SystemTime) -> Option<String> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some((d.as_secs() as u128 * 1_000_000_000 + d.subsec_nanos() as u128).to_string())
}

fn file_mtime(path: &Path) -> Option<String> {
    fs::metadata(path).ok()?.modified().ok().and_then(mtime_string)
}

/// 生成的 README 模板（末尾保留一个换行）。
pub fn readme_template(name: &str, date: &str) -> String {
    format!("# {name}\n\n创建时间：{date}\n\n## 简介\n")
}

/// 把 base 与相对路径拼起来并 canonicalize，要求结果仍位于 base 内。
/// 拒绝绝对路径、`..` 穿越、以及指向 base 之外的符号链接。
pub fn resolve_within(base: &Path, rel: &str) -> Result<PathBuf, ReadmeError> {
    let base_canon = base.canonicalize().map_err(|e| msg(e.to_string()))?;
    let target = base.join(rel);
    let canon = target.canonicalize().map_err(|_| msg("文件不存在"))?;
    if canon.starts_with(&base_canon) {
        Ok(canon)
    } else {
        Err(msg("路径越界"))
    }
}

/// 去掉 `?query` / `#fragment`，并做百分号解码（`%20`、中文等 UTF-8 序列），
/// 未转义的字符（含中文）原样保留。
pub fn decode_rel_path(rel: &str) -> String {
    let rel = rel.split(['?', '#']).next().unwrap_or("");
    let bytes = rel.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 允许的图片扩展名 -> MIME。
pub fn image_mime(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "svg" => Some("image/svg+xml"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}

/// 读取相对路径图片，返回 `data:` URL。含越界、格式、大小校验。
pub fn read_image_data_url(base: &Path, rel: &str) -> Result<String, ReadmeError> {
    let decoded = decode_rel_path(rel);
    let path = resolve_within(base, &decoded)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime = image_mime(ext).ok_or_else(|| msg("不支持的图片格式"))?;
    let meta = fs::metadata(&path).map_err(|e| msg(e.to_string()))?;
    if !meta.is_file() {
        return Err(msg("不是文件"));
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(msg("图片超过 10MB 限制"));
    }
    let bytes = fs::read(&path).map_err(|e| msg(e.to_string()))?;
    Ok(format!("data:{mime};base64,{}", B64.encode(bytes)))
}

/// 只允许 http/https/mailto。
pub fn is_allowed_external(url: &str) -> bool {
    let l = url.trim().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:")
}

/// 基于已解析出的 README 文件名读取内容。
pub fn read_info(base: &Path, name: Option<String>) -> Result<ReadmeInfo, ReadmeError> {
    match name {
        None => Ok(ReadmeInfo { exists: false, filename: None, content: None, mtime: None }),
        Some(name) => {
            let path = base.join(&name);
            let content = fs::read_to_string(&path).map_err(|e| msg(e.to_string()))?;
            let mtime = file_mtime(&path);
            Ok(ReadmeInfo { exists: true, filename: Some(name), content: Some(content), mtime })
        }
    }
}

/// 保存 README。`expected_mtime` 与磁盘不一致时（且未强制）返回 `Conflict`，不写入。
pub fn save(
    base: &Path,
    name: &str,
    content: &str,
    expected_mtime: Option<&str>,
    force: bool,
) -> Result<String, ReadmeError> {
    let path = resolve_within(base, name)?;
    if !force {
        if let Some(cur) = file_mtime(&path) {
            if Some(cur.as_str()) != expected_mtime {
                return Err(ReadmeError::Conflict { current_mtime: cur });
            }
        }
    }
    fs::write(&path, content).map_err(|e| msg(e.to_string()))?;
    file_mtime(&path).ok_or_else(|| msg("无法读取写入后的修改时间"))
}

/// 仅当目录里没有 README.md/readme.md 时才创建 `README.md`（`create_new` 保证不覆盖）。
pub fn generate(base: &Path, title: &str, date: &str) -> Result<ReadmeInfo, ReadmeError> {
    if find_readme(base).map_err(|e| msg(e.to_string()))?.is_some() {
        return Err(msg("已存在 README"));
    }
    let path = base.join("README.md");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| msg(e.to_string()))?;
    let content = readme_template(title, date);
    file.write_all(content.as_bytes()).map_err(|e| msg(e.to_string()))?;
    drop(file);
    Ok(ReadmeInfo {
        exists: true,
        filename: Some("README.md".to_string()),
        content: Some(content),
        mtime: file_mtime(&path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pc-readme-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_readme_prefers_uppercase() {
        assert_eq!(pick_readme_name(&names(&["README.md"])).as_deref(), Some("README.md"));
        assert_eq!(pick_readme_name(&names(&["readme.md"])).as_deref(), Some("readme.md"));
        assert_eq!(
            pick_readme_name(&names(&["readme.md", "README.md"])).as_deref(),
            Some("README.md")
        );
        assert_eq!(pick_readme_name(&names(&["a.md", "src"])), None);
        // 其他大小写不算
        assert_eq!(pick_readme_name(&names(&["Readme.md", "READme.MD"])), None);
    }

    #[test]
    fn find_readme_uses_exact_names() {
        // 每个场景用独立目录：APFS 默认大小写不敏感，同一目录里 Readme.md 与 readme.md 会是同一个文件。
        let d = tmp();
        fs::write(d.join("other.md"), "x").unwrap();
        assert_eq!(find_readme(&d).unwrap(), None);
        fs::remove_dir_all(&d).ok();

        let d = tmp();
        fs::write(d.join("Readme.md"), "x").unwrap();
        assert_eq!(find_readme(&d).unwrap(), None);
        fs::remove_dir_all(&d).ok();

        let d = tmp();
        fs::write(d.join("readme.md"), "lower").unwrap();
        assert_eq!(find_readme(&d).unwrap().as_deref(), Some("readme.md"));
        fs::remove_dir_all(&d).ok();

        let d = tmp();
        fs::write(d.join("README.md"), "upper").unwrap();
        assert_eq!(find_readme(&d).unwrap().as_deref(), Some("README.md"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn save_detects_conflict_without_writing() {
        let d = tmp();
        let p = d.join("README.md");
        fs::write(&p, "v1").unwrap();
        let m1 = file_mtime(&p).unwrap();
        // 期望 mtime 错误 -> 冲突，文件不变
        let err = save(&d, "README.md", "v2", Some("123"), false).unwrap_err();
        assert!(matches!(err, ReadmeError::Conflict { .. }));
        assert_eq!(fs::read_to_string(&p).unwrap(), "v1");
        // 期望 mtime 正确 -> 写入
        let m2 = save(&d, "README.md", "v2", Some(&m1), false).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "v2");
        assert_ne!(m1, m2);
        // force -> 即使 expected 错误也写入
        save(&d, "README.md", "v3", Some("123"), true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "v3");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn generate_refuses_when_readme_exists() {
        let d = tmp();
        fs::write(d.join("readme.md"), "keep-me").unwrap();
        let err = generate(&d, "proj", "2024-01-02").unwrap_err();
        assert!(matches!(err, ReadmeError::Message { .. }));
        // 未覆盖：大小写不敏感，读回同一文件内容仍为 keep-me
        assert_eq!(fs::read_to_string(d.join("README.md")).unwrap(), "keep-me");
        assert_eq!(find_readme(&d).unwrap().as_deref(), Some("readme.md"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn generate_writes_template() {
        let d = tmp();
        let info = generate(&d, "我的项目", "2024-01-02").unwrap();
        let expected = "# 我的项目\n\n创建时间：2024-01-02\n\n## 简介\n";
        assert_eq!(info.content.as_deref(), Some(expected));
        assert_eq!(fs::read_to_string(d.join("README.md")).unwrap(), expected);
        assert!(info.mtime.is_some());
        assert_eq!(info.filename.as_deref(), Some("README.md"));
        // 再生成应失败
        assert!(generate(&d, "我的项目", "2024-01-02").is_err());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn resolve_within_rejects_escape() {
        let d = tmp();
        let outside = d.parent().unwrap().join(format!("pc-out-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x.png"), "no").unwrap();
        let base = d.join("proj");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("ok.png"), "ok").unwrap();
        assert!(resolve_within(&base, "ok.png").is_ok());
        assert!(resolve_within(&base, "../x.png").is_err());
        assert!(resolve_within(&base, outside.join("x.png").to_str().unwrap()).is_err());
        fs::remove_dir_all(&d).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_within_rejects_symlink_escape() {
        let d = tmp();
        let outside = d.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        let base = d.join("proj");
        fs::create_dir_all(&base).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), base.join("link.txt")).unwrap();
        assert!(resolve_within(&base, "link.txt").is_err());
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_readme_symlinked_outside() {
        let d = tmp();
        let outside = d.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim.md");
        fs::write(&victim, "original").unwrap();
        let base = d.join("proj");
        fs::create_dir_all(&base).unwrap();
        std::os::unix::fs::symlink(&victim, base.join("README.md")).unwrap();
        let err = save(&base, "README.md", "hacked", None, true).unwrap_err();
        assert!(matches!(err, ReadmeError::Message { .. }));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn image_mime_whitelist() {
        assert_eq!(image_mime("PNG"), Some("image/png"));
        assert_eq!(image_mime("jpeg"), Some("image/jpeg"));
        assert_eq!(image_mime("svg"), Some("image/svg+xml"));
        assert_eq!(image_mime("gif"), Some("image/gif"));
        assert_eq!(image_mime("txt"), None);
        assert_eq!(image_mime(""), None);
    }

    #[test]
    fn decode_rel_path_handles_encoding_and_fragments() {
        assert_eq!(decode_rel_path("a%20b.png"), "a b.png");
        assert_eq!(decode_rel_path("%E4%B8%AD%E6%96%87.png"), "中文.png");
        assert_eq!(decode_rel_path("中文 图.png"), "中文 图.png");
        assert_eq!(decode_rel_path("a.png?raw=1"), "a.png");
        assert_eq!(decode_rel_path("a.png#frag"), "a.png");
        assert_eq!(decode_rel_path("50%25.png"), "50%.png");
    }

    #[test]
    fn read_image_data_url_checks_ext_and_size() {
        let d = tmp();
        fs::write(d.join("p.png"), [0x89u8, 0x50, 0x4e, 0x47]).unwrap();
        let url = read_image_data_url(&d, "p.png").unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        fs::write(d.join("bad.txt"), "x").unwrap();
        assert!(read_image_data_url(&d, "bad.txt").is_err());
        // 相对路径经 URL 解码后仍指向真实存在的图
        fs::write(d.join("我 的.png"), [1u8, 2, 3]).unwrap();
        assert!(read_image_data_url(&d, "%E6%88%91%20%E7%9A%84.png").is_ok());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn external_scheme_whitelist() {
        assert!(is_allowed_external("https://github.com/a/b"));
        assert!(is_allowed_external("http://example.com"));
        assert!(is_allowed_external("mailto:a@b.com"));
        assert!(!is_allowed_external("javascript:alert(1)"));
        assert!(!is_allowed_external("file:///etc/passwd"));
        assert!(!is_allowed_external("data:text/html,x"));
        assert!(!is_allowed_external("ftp://x"));
    }
}
