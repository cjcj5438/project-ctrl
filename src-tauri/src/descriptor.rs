//! 仓库内描述文件 `.project-ctrl.json`：只读为主，写入必须显式触发。
//!
//! 这一层是**可提交**的共享声明。所有未知字段直接报错（`services::parse_descriptor`），
//! 因为静默忽略拼错的键会让「服务能跑但 env 没生效」变成不可见的故障。

use crate::services::{self, Descriptor};
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
    // 先写临时文件再 rename：中途失败不会留下半个 JSON
    let tmp = base.join(format!(".{FILE_NAME}.tmp"));
    std::fs::write(&tmp, json).map_err(|e| format!("写入 {FILE_NAME} 失败：{e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("写入 {FILE_NAME} 失败：{e}")
    })
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
}
