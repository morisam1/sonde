//! Saved views: a table with its filter, sort, columns and mode, kept as
//! `.toml` files in the `views` folder beside Sonde's other files, and
//! listed in the sidebar under the saved queries.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sonde_core::TableRef;

use crate::table_view::ViewState;

/// A view as its file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    /// The saved connection it opens on.
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    #[serde(flatten)]
    pub state: ViewState,
}

impl SavedView {
    pub fn table_ref(&self) -> TableRef {
        TableRef {
            schema: self.schema.clone(),
            name: self.table.clone(),
        }
    }
}

/// A view in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewFile {
    pub path: PathBuf,
    /// The file name without `.toml`.
    pub name: String,
}

pub fn name_of(path: &Path) -> String {
    path.file_stem().map_or_else(
        || "View".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

/// The views in `dir`, sorted by name. A missing folder has none.
pub fn list(dir: &Path) -> Vec<ViewFile> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut views: Vec<ViewFile> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "toml"))
        .map(|path| ViewFile {
            name: name_of(&path),
            path,
        })
        .collect();
    views.sort_by_key(|view| view.name.to_lowercase());
    views
}

/// The file in `dir` for a view called `name`.
///
/// # Errors
///
/// A message when the name is empty.
pub fn path_for(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let sql = crate::saved_queries::path_for(dir, name.trim().trim_end_matches(".toml"))
        .map_err(|_| "Give the view a name.".to_owned())?;
    Ok(sql.with_extension("toml"))
}

/// Read a view file.
///
/// # Errors
///
/// Any error reading or parsing the file.
pub fn read(path: &Path) -> io::Result<SavedView> {
    let text = fs::read_to_string(path)?;
    toml::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Write a view file through a temporary file.
///
/// # Errors
///
/// Any error creating the folder or writing the file.
pub fn write(path: &Path, view: &SavedView) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(view)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let staged = path.with_extension("toml.tmp");
    fs::write(&staged, text)?;
    fs::rename(&staged, path).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })
}

#[cfg(test)]
mod tests {
    use sonde_core::filter::{Filter, Op, Rule};

    use super::*;

    #[test]
    fn a_view_should_round_trip_through_its_file() {
        let dir = std::env::temp_dir().join(format!("sonde-views-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let view = SavedView {
            connection_id: "abc".into(),
            schema: Some("shop".into()),
            table: "orders".into(),
            state: ViewState {
                filter: Filter {
                    rules: vec![Rule::new("status", "text", Op::Is, "paid")],
                    any: false,
                },
                sort: Some(("placed_at".into(), false)),
                ..ViewState::default()
            },
        };
        let path = path_for(&dir, "Paid orders").unwrap();

        write(&path, &view).unwrap();

        assert_eq!(read(&path).unwrap(), view);
        assert_eq!(list(&dir)[0].name, "Paid orders");
        let _ = fs::remove_dir_all(&dir);
    }
}
