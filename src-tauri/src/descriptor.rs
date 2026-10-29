//! 仓库内描述文件 `.project-ctrl.json`：只读为主，写入必须显式触发。
//!
//! 这一层是**可提交**的共享声明。所有未知字段直接报错（`services::parse_descriptor`），
//! 因为静默忽略拼错的键会让「服务能跑但 env 没生效」变成不可见的故障。

use crate::services::{self, Descriptor};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = ".project-ctrl.json";

pub fn path_of(base: &Path) -> PathBuf {
    base.join(FILE_NAME)
}

pub fn exists(base: &Path) -> bool {
    path_of(base).is_file()
}

/// 读并严格校验。文件不存在返回 `Ok(None)`（不是错误）。
pub fn read(base: &Path) -> Result<Option<Descriptor>, String> {
    let p = path_of(base);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取 {FILE_NAME} 失败：{e}")),
    };
    services::parse_descriptor(&text).map(Some)
}

/// 写描述文件。已存在时默认拒绝（绝不静默覆盖用户/团队的文件）。
pub fn write(base: &Path, d: &Descriptor, overwrite: bool) -> Result<(), String> {
    let p = path_of(base);
    if p.exists() && !overwrite {
        return Err(format!("{FILE_NAME} 已存在；请手工编辑或确认覆盖"));
    }
    let json = services::to_descriptor_json(d)?;
    // 先写临时文件再 rename：中途失败不会留下半个 JSON。
    // 名字只保留一个点（`FILE_NAME` 自带前导点，再拼 `.` 会变成 `..xxx`）：仍是隐藏的 dotfile。
    let tmp = base.join(format!(".{}.tmp", FILE_NAME.trim_start_matches('.')));
    // 任何一步失败都要清掉 tmp，否则会在用户的项目根目录（常见是 git worktree）留下垃圾。
    let res = write_tmp_and_rename(&tmp, &p, json.as_bytes());
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 落盘 + 原子上线。0600 + `sync_all` 与 `localstate::atomic_write` 保持同一套持久性保证。
fn write_tmp_and_rename(tmp: &Path, dest: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)
            .map_err(|e| format!("写入 {FILE_NAME} 失败：{e}"))?;
        f.write_all(bytes).map_err(|e| format!("写入 {FILE_NAME} 失败：{e}"))?;
        f.sync_all().map_err(|e| format!("写入 {FILE_NAME} 失败：{e}"))?;
    }
    std::fs::rename(tmp, dest).map_err(|e| format!("写入 {FILE_NAME} 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::ServiceSpec;

    fn spec(name: &str) -> ServiceSpec {
        ServiceSpec::from_json(&format!(r#"{{"name":"{name}","run":["pnpm","dev"]}}"#)).unwrap()
    }

    #[test]
    fn missing_file_reads_as_none() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read(d.path()).unwrap(), None);
        assert!(!exists(d.path()));
    }

    #[test]
    fn roundtrip_through_disk() {
        let d = tempfile::tempdir().unwrap();
        let desc = Descriptor::new(vec![spec("api")]);
        write(d.path(), &desc, false).unwrap();
        assert_eq!(read(d.path()).unwrap(), Some(desc));
        assert_eq!(
            std::fs::read_to_string(d.path().join(FILE_NAME)).unwrap().matches("\"api\"").count(),
            1
        );
    }

    #[test]
    fn refuses_to_overwrite_without_flag() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), &Descriptor::new(vec![spec("api")]), false).unwrap();
        let err = write(d.path(), &Descriptor::new(vec![spec("web")]), false).unwrap_err();
        assert!(err.contains("已存在"), "{err}");
        // 原内容保持不变
        assert_eq!(read(d.path()).unwrap().unwrap().services[0].name, "api");
    }

    #[test]
    fn overwrite_with_flag_replaces_content() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), &Descriptor::new(vec![spec("api")]), false).unwrap();
        write(d.path(), &Descriptor::new(vec![spec("web")]), true).unwrap();
        assert_eq!(read(d.path()).unwrap().unwrap().services[0].name, "web");
    }

    #[test]
    fn invalid_json_is_reported_not_ignored() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(FILE_NAME), "{ not json").unwrap();
        assert!(read(d.path()).unwrap_err().contains("解析失败"));
    }

    #[test]
    fn unknown_field_names_the_offender() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(FILE_NAME),
            r#"{"schema_version":1,"services":[{"name":"a","run":["x"],"enviroment":{"A":"1"}}]}"#,
        )
        .unwrap();
        let err = read(d.path()).unwrap_err();
        assert!(err.contains("enviroment"), "{err}");
    }

    #[test]
    fn write_failure_leaves_no_tmp() {
        let d = tempfile::tempdir().unwrap();
        // 把 tmp 路径先建成目录：`open` 会以 EISDIR 失败（不需要 root / 磁盘映像）。
        let tmp = d.path().join(".project-ctrl.json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        let err = write(d.path(), &Descriptor::new(vec![spec("api")]), false).unwrap_err();
        assert!(err.contains("写入"), "{err}");
        // 失败路径不得留下 tmp 文件，也不得创建目标描述文件。
        assert!(!tmp.is_file(), "tmp 文件不应存在：{}", tmp.display());
        assert!(!exists(d.path()));
        // 移走挡路的目录后，正常写入必须成功且不残留 tmp。
        std::fs::remove_dir(&tmp).unwrap();
        write(d.path(), &Descriptor::new(vec![spec("api")]), false).unwrap();
        assert!(!tmp.exists(), "正常写入后不应残留 tmp");
        assert!(path_of(d.path()).is_file());
    }

    #[test]
    fn write_uses_single_dot_tmp_name() {
        let d = tempfile::tempdir().unwrap();
        // 目标是目录 → rename 必失败，从而把 tmp 的真实名字暴露出来。
        std::fs::create_dir(path_of(d.path())).unwrap();
        let err = write(d.path(), &Descriptor::new(vec![spec("api")]), true).unwrap_err();
        assert!(err.contains("写入"), "{err}");
        let names: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n == "..project-ctrl.json.tmp"), "{names:?}");
        // 修复后清理路径会删掉 tmp，所以这里也不该看到单个点的 tmp。
        assert!(!names.iter().any(|n| n == ".project-ctrl.json.tmp"), "{names:?}");
    }
}
