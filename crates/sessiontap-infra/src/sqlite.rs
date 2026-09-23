use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

/// Opens a SQLite database with owner-only permissions, WAL journaling, and
/// foreign keys enabled. A symlinked database path is rejected.
pub fn open_private_sqlite(path: &Path) -> Result<Connection> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("database path must not be a symlink");
    }
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_is_private_wal_with_foreign_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db.sqlite3");
        let conn = open_private_sqlite(&path).unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn symlinked_database_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.sqlite3");
        let link = temp.path().join("db.sqlite3");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = open_private_sqlite(&link).unwrap_err();
        assert!(error.to_string().contains("must not be a symlink"));
        assert!(!target.exists(), "symlink target must not be created");
    }
}
