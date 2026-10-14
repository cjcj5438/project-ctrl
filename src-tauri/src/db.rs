//! SQLite 持久化：projects / tags / project_tags。

use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PROJECT_CTRL_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".project-ctrl")
}

pub fn open_db() -> rusqlite::Result<Connection> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let conn = Connection::open(dir.join("data.db"))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS projects (
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
    Ok(conn)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL);
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
             CREATE TABLE project_tags (project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE, PRIMARY KEY (project_id, tag_id));",
        )
        .unwrap();
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
        // t2 改名为 t1：合并，p1 去重后只有 t1
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
}
