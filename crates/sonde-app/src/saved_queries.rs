//! Saved queries: `.sql` files, by default in the `queries` folder beside
//! Sonde's other files.
//!
//! A file Sonde writes starts with a comment that names the connection the
//! query ran on, so reopening it opens a tab on that connection:
//!
//! ```sql
//! -- sonde-connection: 6f1c…
//! select * from orders;
//! ```
//!
//! Any other `.sql` file opens too; it just has no connection of its own.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

const HEADER: &str = "-- sonde-connection: ";

/// A query in the saved-queries folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedQuery {
    /// Its file.
    pub path: PathBuf,
    /// What the sidebar shows: the file name without `.sql`.
    pub name: String,
}

/// A query file's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFile {
    /// The saved connection it ran on, when the file names one.
    pub connection_id: Option<String>,
    /// The SQL, without the header.
    pub sql: String,
}

/// The name a query file is shown under: its file name without `.sql`.
pub fn name_of(path: &Path) -> String {
    path.file_stem().map_or_else(
        || "Query".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

/// The saved queries in `dir`, sorted by name. A missing folder has none.
pub fn list(dir: &Path) -> Vec<SavedQuery> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut queries: Vec<SavedQuery> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "sql"))
        .map(|path| SavedQuery {
            name: name_of(&path),
            path,
        })
        .collect();
    queries.sort_by_key(|query| query.name.to_lowercase());
    queries
}

/// The file in `dir` for a query called `name`. Characters a file name
/// cannot hold become `_`.
///
/// # Errors
///
/// A message when the name is empty.
pub fn path_for(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let cleaned: String = name
        .trim()
        .trim_end_matches(".sql")
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.');
    if cleaned.is_empty() {
        return Err("Give the query a name.".into());
    }
    Ok(dir.join(format!("{cleaned}.sql")))
}

/// Read a query file.
///
/// # Errors
///
/// Any error reading the file.
pub fn read(path: &Path) -> io::Result<QueryFile> {
    let text = fs::read_to_string(path)?;
    let (connection_id, sql) = match text.strip_prefix(HEADER) {
        Some(rest) => {
            let (id, sql) = rest.split_once('\n').unwrap_or((rest, ""));
            let id = id.trim();
            ((!id.is_empty()).then(|| id.to_owned()), sql.to_owned())
        }
        None => (None, text),
    };
    Ok(QueryFile { connection_id, sql })
}

/// Write a query file, through a temporary file so a crash never leaves
/// half a query behind.
///
/// # Errors
///
/// Any error creating the folder or writing the file.
pub fn write(path: &Path, connection_id: Option<&str>, sql: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut text = String::new();
    if let Some(id) = connection_id {
        text.push_str(HEADER);
        text.push_str(id);
        text.push('\n');
    }
    text.push_str(sql);
    let staged = path.with_extension("sql.tmp");
    fs::write(&staged, text)?;
    fs::rename(&staged, path).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sonde-queries-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn write_then_read_should_keep_the_connection_and_the_sql() {
        let dir = dir("roundtrip");
        let path = path_for(&dir, "Monthly totals").unwrap();

        write(&path, Some("abc-123"), "select 1;\n-- done\n").unwrap();

        assert_eq!(
            read(&path).unwrap(),
            QueryFile {
                connection_id: Some("abc-123".into()),
                sql: "select 1;\n-- done\n".into()
            }
        );
        assert_eq!(
            list(&dir),
            [SavedQuery {
                path,
                name: "Monthly totals".into()
            }]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_sql_file_should_open_without_a_connection() {
        let dir = dir("plain");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("other.sql");
        fs::write(&path, "select 2").unwrap();

        assert_eq!(read(&path).unwrap().connection_id, None);
        assert_eq!(read(&path).unwrap().sql, "select 2");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_for_should_clean_the_name() {
        let dir = Path::new("/q");

        assert_eq!(
            path_for(dir, "a/b: c?").unwrap(),
            Path::new("/q/a_b_ c_.sql")
        );
        assert_eq!(
            path_for(dir, "report.sql").unwrap(),
            Path::new("/q/report.sql")
        );
        assert!(path_for(dir, "  ").is_err());
    }

    #[test]
    fn list_should_sort_by_name_and_skip_other_files() {
        let dir = dir("list");
        fs::create_dir_all(&dir).unwrap();
        for name in ["b.sql", "A.sql", "notes.txt"] {
            fs::write(dir.join(name), "").unwrap();
        }

        let names: Vec<String> = list(&dir).into_iter().map(|q| q.name).collect();

        assert_eq!(names, ["A", "b"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
