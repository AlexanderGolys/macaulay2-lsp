//! A workspace-wide index of top-level (global) definitions across every `.m2`
//! file under the project roots, so go-to-definition can jump into files the
//! editor has not opened.
//!
//! M2's top-level symbols are global once loaded, so a name-keyed index of every
//! file's global definitions is a faithful model: a name used in one file and
//! defined at the top level of another resolves across the project.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use dashmap::DashMap;
use tower_lsp::lsp_types::{Location, Range, Url};

use crate::document::DocumentSnapshot;
use crate::typesystem::BuiltinData;

#[derive(Debug, Clone)]
struct DefLocation {
    uri: Url,
    range: Range,
}

/// Global definition index, keyed by symbol name, kept in sync with edits and
/// on-disk changes. Open documents are indexed from their live text; unopened
/// files from disk.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceIndex {
    /// name -> every place it is defined at top level across the workspace.
    by_name: DashMap<String, Vec<DefLocation>>,
    /// file -> the names it contributes, so a re-index can drop the old set.
    by_file: DashMap<Url, Vec<String>>,
    roots: RwLock<Vec<PathBuf>>,
}

impl WorkspaceIndex {
    pub(crate) fn set_roots(&self, roots: Vec<PathBuf>) {
        *self.roots.write().expect("workspace roots lock poisoned") = roots;
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .read()
            .expect("workspace roots lock poisoned")
            .clone()
    }

    /// Walk every root and index all `.m2` files found. Intended to run off the
    /// request path (e.g. `spawn_blocking`) since it touches the filesystem.
    pub(crate) fn scan(&self, builtins: &BuiltinData) {
        for root in self.roots() {
            let mut files = Vec::new();
            collect_m2_files(&root, &mut files);
            for path in files {
                let Ok(uri) = Url::from_file_path(&path) else {
                    continue;
                };
                // A file already in the index is owned by an open buffer (indexed
                // live) or was already visited this scan; don't overwrite it with
                // potentially stale disk content.
                if self.by_file.contains_key(&uri) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                self.index_file(&uri, &text, builtins);
            }
        }
    }

    /// Replace the definitions recorded for `uri` with those parsed from `text`.
    pub(crate) fn index_file(&self, uri: &Url, text: &str, builtins: &BuiltinData) {
        self.remove_file(uri);
        let definitions = top_level_definitions(text, builtins);
        if definitions.is_empty() {
            return;
        }
        let mut names = Vec::with_capacity(definitions.len());
        for (name, range) in definitions {
            self.by_name
                .entry(name.clone())
                .or_default()
                .push(DefLocation {
                    uri: uri.clone(),
                    range,
                });
            names.push(name);
        }
        self.by_file.insert(uri.clone(), names);
    }

    pub(crate) fn remove_file(&self, uri: &Url) {
        let Some((_, names)) = self.by_file.remove(uri) else {
            return;
        };
        for name in names {
            let now_empty = if let Some(mut locations) = self.by_name.get_mut(&name) {
                locations.retain(|location| &location.uri != uri);
                locations.is_empty()
            } else {
                false
            };
            if now_empty {
                self.by_name.remove(&name);
            }
        }
    }

    /// Every workspace definition of `name`, excluding `exclude` — the current
    /// document, whose live analysis is the authoritative source for its own
    /// definitions.
    pub(crate) fn lookup(&self, name: &str, exclude: &Url) -> Vec<Location> {
        self.by_name
            .get(name)
            .map(|locations| {
                locations
                    .iter()
                    .filter(|location| &location.uri != exclude)
                    .map(|location| Location {
                        uri: location.uri.clone(),
                        range: location.range,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `name` is defined at top level anywhere in the workspace. Used to
    /// recognise a global symbol whose definition lives in another file.
    pub(crate) fn is_defined(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Every `.m2` file under the project roots, as URIs. Walks the filesystem,
    /// so callers should keep it off the hot path.
    pub(crate) fn workspace_file_uris(&self) -> Vec<Url> {
        let mut uris = Vec::new();
        for root in self.roots() {
            let mut files = Vec::new();
            collect_m2_files(&root, &mut files);
            uris.extend(
                files
                    .iter()
                    .filter_map(|path| Url::from_file_path(path).ok()),
            );
        }
        uris
    }
}

/// Recursively collect `.m2` files, skipping hidden, build, and vendored dirs.
fn collect_m2_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') || matches!(name.as_ref(), "target" | "node_modules") {
                continue;
            }
            collect_m2_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("m2") {
            out.push(path);
        }
    }
}

/// Parse `text` and return its global-scope definitions as `(name, range)`,
/// where `range` is the definition site — the same range go-to-definition
/// returns for an in-file symbol.
fn top_level_definitions(text: &str, builtins: &BuiltinData) -> Vec<(String, Range)> {
    let Some(snapshot) = DocumentSnapshot::from_text(text.to_string(), builtins) else {
        return Vec::new();
    };
    let Some(global_scope) = snapshot.analysis().scopes.first() else {
        return Vec::new();
    };
    global_scope
        .symbols
        .iter()
        .flat_map(|(name, infos)| infos.iter().map(move |info| (name.clone(), info.range)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typesystem::BuiltinData;

    #[test]
    fn indexes_and_looks_up_cross_file_definitions() {
        let builtins = BuiltinData::load_from_split("", "");
        let index = WorkspaceIndex::default();
        let defs = Url::parse("file:///defs.m2").expect("uri");
        let main = Url::parse("file:///main.m2").expect("uri");

        index.index_file(
            &defs,
            "myHelper = x -> x + 1\nGreeting = new Type of HashTable\n",
            &builtins,
        );

        let found = index.lookup("myHelper", &main);
        assert_eq!(found.len(), 1, "myHelper should be found in defs.m2");
        assert_eq!(found[0].uri, defs);
        assert!(
            !index.lookup("Greeting", &main).is_empty(),
            "Greeting type should be indexed"
        );
        // The current document is excluded (its live analysis is authoritative).
        assert!(index.lookup("myHelper", &defs).is_empty());

        // Re-indexing replaces; removal clears.
        index.index_file(&defs, "renamed = 1\n", &builtins);
        assert!(index.lookup("myHelper", &main).is_empty());
        assert_eq!(index.lookup("renamed", &main).len(), 1);
        index.remove_file(&defs);
        assert!(index.lookup("renamed", &main).is_empty());
    }
}
