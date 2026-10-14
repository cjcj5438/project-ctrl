//! 目录磁盘占用统计（与 `du` 同口径）。
//!
//! 口径：`MetadataExt::blocks() * 512` 累加；递归全部子项（含 `node_modules` 与 `.git`）；
//! 同一 `(dev, ino)` 的硬链接只算一次；不跟随符号链接（符号链接只按自身 lstat 计入）；
//! 读不了的子目录跳过，继续统计其余部分，不报错。

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// 统计目录的磁盘占用（字节）。路径不存在或不可访问时返回 0。
pub fn dir_size(root: &Path) -> u64 {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(path) = stack.pop() {
        // symlink_metadata：不跟随符号链接
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !seen.insert((meta.dev(), meta.ino())) {
            continue; // 硬链接（同一 inode）只算一次
        }
        total += meta.blocks() * 512;
        if meta.is_dir() {
            match fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        stack.push(entry.path());
                    }
                }
                Err(_) => continue, // 无权限等：跳过该子目录，继续其余部分
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::SystemTime;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pc-size-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// `du -s`（默认 512 字节块）的块数，与 `dir_size/512` 同口径。
    fn du_blocks(path: &Path) -> u64 {
        let out = Command::new("/usr/bin/du").arg("-s").arg(path).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace().next().unwrap().parse().unwrap()
    }

    fn blocks_of(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks()
    }

    fn write_file(path: &Path, len: usize) {
        fs::write(path, vec![b'x'; len]).unwrap();
    }

    #[test]
    fn nested_dirs_match_du() {
        let d = tmp();
        fs::create_dir_all(d.join("a/b/c")).unwrap();
        write_file(&d.join("top.bin"), 5000);
        write_file(&d.join("a/one.bin"), 70000);
        write_file(&d.join("a/b/c/two.bin"), 300000);
        assert_eq!(
            dir_size(&d) / 512,
            du_blocks(&d),
            "与 du -s 的 512 字节块数应完全一致"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_content_not_counted() {
        let d = tmp();
        let big = d.join("big");
        fs::create_dir_all(&big).unwrap();
        write_file(&big.join("huge.bin"), 2_000_000);
        let proj = d.join("proj");
        fs::create_dir_all(&proj).unwrap();
        write_file(&proj.join("small.bin"), 10);
        std::os::unix::fs::symlink(&big, proj.join("link-to-big")).unwrap();

        let size = dir_size(&proj);
        assert!(
            size < blocks_of(&big.join("huge.bin")) * 512,
            "符号链接指向的外部大目录不应被计入（size={size}）"
        );
        assert_eq!(size / 512, du_blocks(&proj), "应与 du -s 一致（du 同样不跟随符号链接）");
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_counted_once() {
        let d = tmp();
        fs::create_dir_all(&d).unwrap();
        write_file(&d.join("origin.bin"), 1_000_000);
        fs::hard_link(d.join("origin.bin"), d.join("alias.bin")).unwrap();
        let one = blocks_of(&d.join("origin.bin")) * 512;

        let size = dir_size(&d);
        assert!(size < one * 2, "硬链接不应重复计数（size={size}, one={one}）");
        assert_eq!(size / 512, du_blocks(&d), "应与 du -s 一致（du 默认对硬链接去重）");
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subdir_is_skipped_without_panic() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let proj = d.join("proj");
        fs::create_dir_all(proj.join("locked")).unwrap();
        write_file(&proj.join("visible.bin"), 200_000);
        write_file(&proj.join("locked/hidden.bin"), 500_000);

        let before = dir_size(&proj);

        // 锁住子目录
        fs::set_permissions(proj.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let after = dir_size(&proj); // 不应 panic
        assert!(
            after >= blocks_of(&proj.join("visible.bin")) * 512,
            "仍应统计可读部分（after={after}）"
        );
        assert!(after < before, "无权限子目录的内容应被跳过（after={after} before={before}）");

        // 恢复权限后清理
        fs::set_permissions(proj.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_path_is_zero() {
        let d = tmp();
        assert_eq!(dir_size(&d.join("does-not-exist")), 0);
        fs::remove_dir_all(&d).ok();
    }
}
