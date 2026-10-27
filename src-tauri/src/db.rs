//! SQLite 持久化：projects / tags / project_tags / services。
//!
//! schema 版本记在 `PRAGMA user_version`，迁移按顺序一次跑完。改 schema 只加一条新迁移，
//! 绝不删库——库里有用户的 tag 与服务声明。
//!
//! 注意：`services` 一个项目可以有任意多行（`UNIQUE(project_id, name)`）。v0.2.0 的旧表是
//! `project_id` 单主键，v1 迁移会整表重建并把旧配方转成新模型（`install`/`build` → `steps`）。

use crate::services::{self, Derived, ServiceSpec};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PROJECT_CTRL_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".project-ctrl")
}

/// 有序迁移列表。索引 i 表示「升到版本 i+1」。
const MIGRATIONS: &[fn(&Connection) -> rusqlite::Result<()>] = &[migrate_to_v1, migrate_to_v2];

/// 打开（必要时迁移）数据库。
pub fn open_db() -> rusqlite::Result<Connection> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    restrict_db_permissions(&dir.join("data.db"));
    let mut conn = Connection::open(dir.join("data.db"))?;
    restrict_db_permissions(&dir.join("data.db"));
    migrate(&mut conn)?;
    Ok(conn)
}

/// #26：data.db 含服务 env，必须 0600。新建/存量都在 open 路径修复。
fn restrict_db_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.permissions().mode() & 0o077 != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// 把连接升到最新 schema 版本（幂等，测试也用这个建库）。
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let latest = MIGRATIONS.len() as i64;
    if current > latest {
        // #25：库比 App 新（降级/旧版本打开新库）：拒绝而不是静默读写未知 schema。
        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
            format!("数据库版本（{current}）高于本 App 支持的版本（{latest}），请升级 App 后再打开"),
        ))));
    }
    for (idx, step) in MIGRATIONS.iter().enumerate() {
        let version = idx as i64 + 1;
        if current >= version {
            continue;
        }
        let tx = conn.transaction()?;
        step(&tx)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

/// v1：建基线表；若检测到 v0.2.0 的单配方 `services` 表则整表重建并转换。
/// 解析失败或 validate 非法的行 → 隔离表（#61），不产生幽灵服务、不丢原 JSON。
fn migrate_to_v1(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           path TEXT NOT NULL UNIQUE,
           added_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS tags (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           name TEXT NOT NULL UNIQUE
         );
         CREATE TABLE IF NOT EXISTS project_tags (
           project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
           tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
           PRIMARY KEY (project_id, tag_id)
         );",
    )?;

    if table_exists(conn, "services")? && is_legacy_services(conn)? {
        let rows: Vec<(i64, String)> = {
            let mut st = conn.prepare("SELECT project_id, config FROM services")?;
            let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        conn.execute_batch("DROP TABLE services;")?;
        create_services(conn)?;
        create_services_quarantine(conn)?;
        for (project_id, json) in rows {
            let parsed: Result<services::LegacyServiceConfig, _> = serde_json::from_str(&json);
            let legacy = match parsed {
                Ok(l) => l,
                Err(e) => {
                    quarantine(conn, project_id, &json, &format!("legacy JSON 解析失败：{e}"))?;
                    continue;
                }
            };
            let (spec, derived) = services::from_legacy(&legacy);
            let problems = spec.validate();
            if !problems.is_empty() {
                quarantine(conn, project_id, &json, &format!("转换后校验失败：{}", problems.join("；")))?;
                continue;
            }
            insert_service(conn, project_id, &spec, &derived)?;
        }
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM services_quarantine", [], |r| r.get(0))?;
        if n > 0 {
            eprintln!("db 迁移：{n} 行 legacy 配置已隔离到 services_quarantine（原 JSON 保留）");
        }
    } else {
        create_services(conn)?;
    }
    Ok(())
}

/// v2：隔离表。已升级过的库（user_version=1）走这条补上；新库经 v1→v2 链同样得到。
fn migrate_to_v2(conn: &Connection) -> rusqlite::Result<()> {
    create_services_quarantine(conn)
}

fn create_services_quarantine(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS services_quarantine (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           project_id INTEGER NOT NULL,
           raw TEXT NOT NULL,
           error TEXT NOT NULL,
           quarantined_at INTEGER NOT NULL
         );",
    )
}

/// 坏行进隔离表（原 JSON + 原因都保留，可手工恢复）。
fn quarantine(conn: &Connection, project_id: i64, raw: &str, error: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO services_quarantine (project_id, raw, error, quarantined_at) VALUES (?1, ?2, ?3, ?4)",
        params![project_id, raw, error, now_secs()],
    )?;
    Ok(())
}

fn create_services(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS services (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
           name TEXT NOT NULL,
           config TEXT NOT NULL,
           derived TEXT NOT NULL DEFAULT '{}',
           managed_at INTEGER NOT NULL,
           UNIQUE (project_id, name)
         );",
    )
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |r| r.get(0),
    )?;
    Ok(n == 1)
}

/// 旧表特征：有 `project_id` 列但没有 `name` 列（新表两者都有）。
fn is_legacy_services(conn: &Connection) -> rusqlite::Result<bool> {
    let mut has_project_id = false;
    let mut has_name = false;
    let mut st = conn.prepare("PRAGMA table_info(services)")?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let col: String = r.get(1)?;
        if col == "project_id" {
            has_project_id = true;
        }
        if col == "name" {
            has_name = true;
        }
    }
    Ok(has_project_id && !has_name)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn add_project(conn: &Connection, path: &str) -> rusqlite::Result<i64> {
    let path = crate::logic::normalize_path(path);
    conn.execute("INSERT OR IGNORE INTO projects (path, added_at) VALUES (?1, ?2)", params![path, now_secs()])?;
    let id: i64 = conn.query_row("SELECT id FROM projects WHERE path = ?1", params![path], |r| r.get(0))?;
    Ok(id)
}

pub fn all_project_paths(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut st = conn.prepare("SELECT path FROM projects")?;
    let rows = st.query_map([], |r| r.get(0))?;
    rows.collect()
}

pub fn project_path(conn: &Connection, id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT path FROM projects WHERE id = ?1", params![id], |r| r.get(0))
        .optional()
}

pub struct ProjectRow {
    pub id: i64,
    pub path: String,
}

pub fn all_projects(conn: &Connection) -> rusqlite::Result<Vec<ProjectRow>> {
    let mut st = conn.prepare("SELECT id, path FROM projects ORDER BY id ASC")?;
    let rows = st.query_map([], |r| Ok(ProjectRow { id: r.get(0)?, path: r.get(1)? }))?;
    rows.collect()
}

pub fn remove_project(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM project_tags WHERE tag_id = tags.id)", [])?;
    Ok(())
}

pub fn update_project_path(conn: &Connection, id: i64, new_path: &str) -> rusqlite::Result<()> {
    let new_path = crate::logic::normalize_path(new_path);
    conn.execute("UPDATE projects SET path = ?1 WHERE id = ?2", params![new_path, id])?;
    Ok(())
}

pub fn ensure_tag(conn: &Connection, name: &str) -> rusqlite::Result<i64> {
    conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", params![name])?;
    let id: i64 = conn.query_row("SELECT id FROM tags WHERE name = ?1", params![name], |r| r.get(0))?;
    Ok(id)
}

pub fn set_project_tags(conn: &Connection, project_id: i64, tags: &[String]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM project_tags WHERE project_id = ?1", params![project_id])?;
    for t in tags {
        let name = t.trim();
        if name.is_empty() {
            continue;
        }
        let tag_id = ensure_tag(conn, name)?;
        conn.execute(
            "INSERT OR IGNORE INTO project_tags (project_id, tag_id) VALUES (?1, ?2)",
            params![project_id, tag_id],
        )?;
    }
    conn.execute("DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM project_tags WHERE tag_id = tags.id)", [])?;
    Ok(())
}

pub fn tags_of(conn: &Connection, project_id: i64) -> rusqlite::Result<Vec<String>> {
    let mut st = conn.prepare(
        "SELECT t.name FROM tags t JOIN project_tags pt ON pt.tag_id = t.id WHERE pt.project_id = ?1 ORDER BY t.name ASC",
    )?;
    let rows = st.query_map(params![project_id], |r| r.get(0))?;
    rows.collect()
}

pub fn all_tags(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut st = conn.prepare("SELECT name FROM tags ORDER BY name ASC")?;
    let rows = st.query_map([], |r| r.get(0))?;
    rows.collect()
}

pub fn rename_tag(conn: &Connection, old: &str, new: &str) -> rusqlite::Result<()> {
    let new = new.trim();
    if new.is_empty() || old == new {
        return Ok(());
    }
    // 合并语义：new 已存在则把关联搬过去再删 old
    let new_id: Option<i64> =
        conn.query_row("SELECT id FROM tags WHERE name = ?1", params![new], |r| r.get(0)).ok();
    let old_id: Option<i64> =
        conn.query_row("SELECT id FROM tags WHERE name = ?1", params![old], |r| r.get(0)).ok();
    let old_id = match old_id {
        Some(id) => id,
        None => return Ok(()),
    };
    match new_id {
        Some(nid) => {
            conn.execute(
                "UPDATE OR IGNORE project_tags SET tag_id = ?1 WHERE tag_id = ?2",
                params![nid, old_id],
            )?;
            conn.execute("DELETE FROM tags WHERE id = ?1", params![old_id])?;
        }
        None => {
            conn.execute("UPDATE tags SET name = ?1 WHERE id = ?2", params![new, old_id])?;
        }
    }
    Ok(())
}

pub fn delete_tag(conn: &Connection, name: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM tags WHERE name = ?1", params![name])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 服务声明（每项目多行）
// ---------------------------------------------------------------------------

pub struct ServiceRow {
    pub config: String,
    pub derived: String,
}

fn row_service(r: &rusqlite::Row) -> rusqlite::Result<ServiceRow> {
    Ok(ServiceRow { config: r.get(0)?, derived: r.get(1)? })
}

pub fn services_of(conn: &Connection, project_id: i64) -> rusqlite::Result<Vec<ServiceRow>> {
    let mut st = conn
        .prepare("SELECT config, derived FROM services WHERE project_id = ?1 ORDER BY id ASC")?;
    let it = st.query_map(params![project_id], row_service)?;
    it.collect()
}

pub fn insert_service(conn: &Connection, project_id: i64, spec: &ServiceSpec, derived: &Derived) -> rusqlite::Result<i64> {
    let json = spec.to_json().unwrap_or_default();
    conn.execute(
        "INSERT INTO services (project_id, name, config, derived, managed_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(project_id, name) DO UPDATE SET
           config = excluded.config, derived = excluded.derived, managed_at = excluded.managed_at",
        params![project_id, spec.name, json, derived.to_json(), now_secs()],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM services WHERE project_id = ?1 AND name = ?2",
        params![project_id, spec.name],
        |r| r.get(0),
    )?;
    Ok(id)
}

pub fn delete_service(conn: &Connection, project_id: i64, name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM services WHERE project_id = ?1 AND name = ?2",
        params![project_id, name],
    )?;
    Ok(())
}

/// 项目下所有服务的名字（用于重名检查与 depends_on 校验）。
pub fn service_names(conn: &Connection, project_id: i64) -> rusqlite::Result<Vec<String>> {
    let mut st = conn.prepare("SELECT name FROM services WHERE project_id = ?1 ORDER BY id ASC")?;
    let it = st.query_map(params![project_id], |r| r.get(0))?;
    it.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn path_unique_returns_same_id() {
        let conn = mem();
        let a = add_project(&conn, "/x/a/").unwrap();
        let b = add_project(&conn, "/x/a").unwrap();
        assert_eq!(a, b);
        assert_eq!(all_project_paths(&conn).unwrap(), vec!["/x/a".to_string()]);
    }

    #[test]
    fn tag_name_unique_and_rename_merge() {
        let conn = mem();
        let p1 = add_project(&conn, "/x/a").unwrap();
        let p2 = add_project(&conn, "/x/b").unwrap();
        set_project_tags(&conn, p1, &["t1".into(), "t2".into()]).unwrap();
        set_project_tags(&conn, p2, &["t2".into()]).unwrap();
        rename_tag(&conn, "t2", "t1").unwrap();
        assert_eq!(tags_of(&conn, p1).unwrap(), vec!["t1".to_string()]);
        assert_eq!(tags_of(&conn, p2).unwrap(), vec!["t1".to_string()]);
        assert_eq!(all_tags(&conn).unwrap(), vec!["t1".to_string()]);
    }

    #[test]
    fn rename_tag_simple() {
        let conn = mem();
        let p = add_project(&conn, "/x/a").unwrap();
        set_project_tags(&conn, p, &["old".into()]).unwrap();
        rename_tag(&conn, "old", "new").unwrap();
        assert_eq!(tags_of(&conn, p).unwrap(), vec!["new".to_string()]);
    }

    #[test]
    fn delete_tag_cascades_from_projects() {
        let conn = mem();
        let p = add_project(&conn, "/x/a").unwrap();
        set_project_tags(&conn, p, &["k1".into(), "k2".into()]).unwrap();
        delete_tag(&conn, "k1").unwrap();
        assert_eq!(tags_of(&conn, p).unwrap(), vec!["k2".to_string()]);
    }

    #[test]
    fn remove_project_cleans_orphan_tags() {
        let conn = mem();
        let p = add_project(&conn, "/x/a").unwrap();
        set_project_tags(&conn, p, &["solo".into()]).unwrap();
        remove_project(&conn, p).unwrap();
        assert!(all_tags(&conn).unwrap().is_empty());
    }

    #[test]
    fn relink_path_conflict_errors() {
        let conn = mem();
        add_project(&conn, "/x/a").unwrap();
        let p2 = add_project(&conn, "/x/b").unwrap();
        assert!(update_project_path(&conn, p2, "/x/a").is_err());
    }

    fn spec(name: &str) -> ServiceSpec {
        services::ServiceSpec::from_json(&format!(r#"{{"name":"{name}","run":["echo","hi"]}}"#)).unwrap()
    }

    #[test]
    fn multiple_services_per_project() {
        let conn = mem();
        let p = add_project(&conn, "/x/a").unwrap();
        insert_service(&conn, p, &spec("api"), &Derived::manual()).unwrap();
        insert_service(&conn, p, &spec("worker"), &Derived::manual()).unwrap();
        assert_eq!(service_names(&conn, p).unwrap(), vec!["api", "worker"]);
    }

    #[test]
    fn same_name_upserts_instead_of_duplicating() {
        let conn = mem();
        let p = add_project(&conn, "/x/a").unwrap();
        let first = insert_service(&conn, p, &spec("api"), &Derived::manual()).unwrap();
        let mut changed = spec("api");
        changed.run = Some(vec!["echo".into(), "changed".into()]);
        let second = insert_service(&conn, p, &changed, &Derived::manual()).unwrap();
        assert_eq!(first, second);
        assert_eq!(services_of(&conn, p).unwrap().len(), 1);
        let rows = services_of(&conn, p).unwrap();
        let spec = ServiceSpec::from_json(&rows[0].config).unwrap();
        assert_eq!(spec.run.unwrap()[1], "changed");
    }

    #[test]
    fn derived_survives_roundtrip() {
        let conn = mem();
        let p = add_project(&conn, "/x/a").unwrap();
        let d = Derived { origin: "auto".into(), node_pin: Some("24.16.0".into()), toolchain: vec!["pnpm".into()] };
        insert_service(&conn, p, &spec("api"), &d).unwrap();
        let rows = services_of(&conn, p).unwrap();
        assert_eq!(Derived::from_json(&rows[0].derived), d);
    }

    #[test]
    fn removing_project_cascades_services() {
        let conn = mem();
        let p = add_project(&conn, "/x/a").unwrap();
        insert_service(&conn, p, &spec("api"), &Derived::manual()).unwrap();
        remove_project(&conn, p).unwrap();
        assert!(services_of(&conn, p).unwrap().is_empty());
    }

    #[test]
    fn migration_converts_legacy_single_recipe() {
        // 手工建 v0.2.0 的库形态，再跑迁移，断言零丢失
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL);
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
             CREATE TABLE project_tags (project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE, PRIMARY KEY (project_id, tag_id));
             CREATE TABLE services (project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE, config TEXT NOT NULL, managed_at INTEGER NOT NULL);
             INSERT INTO projects (id, path, added_at) VALUES (7, '/x/legacy', 1);
             INSERT INTO services (project_id, config, managed_at) VALUES (7, '{\"install\":[\"pnpm\",\"install\"],\"build\":[\"pnpm\",\"run\",\"build\"],\"start\":[\"pnpm\",\"dev\"],\"dev_port\":5190,\"requires_ports\":[5432],\"env\":{\"A\":\"1\"},\"node_pin\":null,\"toolchain\":[\"pnpm\"],\"origin\":\"pm2\"}', 2);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let rows = services_of(&conn, 7).unwrap();
        assert_eq!(rows.len(), 1);
        let spec = ServiceSpec::from_json(&rows[0].config).unwrap();
        assert_eq!(spec.name, "default");
        assert_eq!(spec.steps.len(), 2, "install 与 build 都保留成步骤");
        assert_eq!(spec.run.unwrap(), vec!["pnpm", "dev"]);
        assert_eq!(spec.ports.listen, Some(5190));
        assert_eq!(Derived::from_json(&rows[0].derived).origin, "pm2");
        // 幂等：再跑一次不应重复转换
        migrate(&mut conn).unwrap();
        assert_eq!(services_of(&conn, 7).unwrap().len(), 1);
    }

    #[test]
    fn migrate_is_idempotent_on_fresh_db() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    #[test]
    fn newer_schema_is_rejected_with_upgrade_hint() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 5).unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("升级"), "{err}");
    }

    #[test]
    fn busy_timeout_is_configured() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let ms: i64 = conn.pragma_query_value(None, "busy_timeout", |r| r.get(0)).unwrap();
        assert_eq!(ms, 5000, "并发写不应立刻 database is locked");
    }

    #[test]
    fn new_database_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let _data = crate::test_support::ScopedDataDir::new(dir.path());
        let db_path = dir.path().join("data.db");
        // 预置 0644 存量 → open 后修复为 0600
        std::fs::write(&db_path, b"").unwrap();
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_db().unwrap();
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "存量 data.db 应被修复为 0600");
        // 新建库同样 0600
        std::fs::remove_file(&db_path).unwrap();
        open_db().unwrap();
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "新建 data.db 应为 0600");
    }

    #[test]
    fn corrupt_legacy_rows_are_quarantined_not_ghosted() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL);
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
             CREATE TABLE project_tags (project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE, PRIMARY KEY (project_id, tag_id));
             CREATE TABLE services (project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE, config TEXT NOT NULL, managed_at INTEGER NOT NULL);
             INSERT INTO projects (id, path, added_at) VALUES (7, '/x/good', 1), (8, '/x/bad', 1);
             INSERT INTO services (project_id, config, managed_at) VALUES
               (7, '{\"start\":[\"pnpm\",\"dev\"],\"dev_port\":5190}', 2),
               (8, 'NOT-JSON{{{', 2);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        // 好行正常
        let rows = services_of(&conn, 7).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(ServiceSpec::from_json(&rows[0].config).unwrap().run.unwrap(), vec!["pnpm", "dev"]);
        // 坏行：无幽灵服务，原 JSON 进隔离表
        assert!(services_of(&conn, 8).unwrap().is_empty(), "坏行不得产生幽灵服务");
        let q: Vec<(i64, String, String)> = {
            let mut st = conn.prepare("SELECT project_id, raw, error FROM services_quarantine").unwrap();
            st.query_map([], |r| Ok((r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, 8);
        assert!(q[0].1.contains("NOT-JSON"), "原 JSON 必须保留：{}", q[0].1);
        assert!(!q[0].2.is_empty());
        // 已升级过的库（无 legacy 表）同样有隔离表
        let fresh = mem();
        let n: i64 = fresh
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='services_quarantine'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}

#[cfg(test)]
mod real_db_smoke {
    //! 用**真实库的副本**验证迁移不丢东西（只在副本上跑，绝不碰 ~/.project-ctrl）。
    use super::*;

    #[test]
    fn migrates_a_copy_of_the_real_database() {
        let real = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".project-ctrl/data.db");
        if !real.is_file() {
            eprintln!("跳过：没有真实库");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let copy = tmp.path().join("data.db");
        std::fs::copy(&real, &copy).unwrap();

        // 副本如果还是旧 schema，先确认它确实是旧的
        let mut conn = Connection::open(&copy).unwrap();
        let before: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        let projects_before: i64 =
            conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0)).unwrap();
        let legacy_rows: Vec<(i64, String)> = {
            let mut st = conn.prepare("SELECT project_id, config FROM services").unwrap();
            let it = st.query_map([], |r| Ok((r.get(0).unwrap(), r.get(1).unwrap()))).unwrap();
            it.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };

        migrate(&mut conn).unwrap();

        let after: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert!(after >= 2, "应升到 v2（之前是 {before}）");
        let projects_after: i64 =
            conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0)).unwrap();
        assert_eq!(projects_before, projects_after, "项目数量不能变");

        for (project_id, old_json) in legacy_rows {
            let rows = services_of(&conn, project_id).unwrap();
            assert_eq!(rows.len(), 1, "旧配方应转成恰好一条服务");
            let spec = ServiceSpec::from_json(&rows[0].config).unwrap();
            // 零丢失：旧 JSON 里出现的每一段命令都还在
            let legacy: services::LegacyServiceConfig = serde_json::from_str(&old_json).unwrap();
            if let Some(install) = legacy.install {
                assert!(
                    spec.steps.iter().any(|s| s.argv == install),
                    "install 命令丢失：{install:?}"
                );
            }
            if let Some(build) = legacy.build {
                assert!(
                    spec.steps.iter().any(|s| s.argv == build),
                    "build 命令丢失：{build:?}"
                );
            }
            if !legacy.start.is_empty() {
                assert_eq!(spec.run_argv(), legacy.start, "start 命令丢失");
            }
            assert_eq!(spec.ports.listen, legacy.dev_port);
            assert_eq!(spec.ports.requires, legacy.requires_ports);
            assert_eq!(spec.env, legacy.env);
        }
    }
}
