//! A workspace-wide index of top-level (global) definitions across every `.m2`
//! file under the project roots, so go-to-definition can jump into files the
//! editor has not opened.
//!
//! M2's top-level symbols are global once loaded, so a name-keyed index of every
//! file's global definitions is a faithful model: a name used in one file and
//! defined at the top level of another resolves across the project.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use tower_lsp::lsp_types::{Location, Range, Url};

use crate::capabilities::semantic_tokens::local_symbol_semantic_token_type;
use crate::document::DocumentSnapshot;
use crate::object_registry::{ObjectName, ObjectRegistry};
use crate::semantic_token::M2SemanticTokenType;

#[derive(Debug, Clone)]
struct DefLocation {
    uri: Url,
    range: Range,
    semantic_token_type: M2SemanticTokenType,
}

pub(crate) trait WorkspaceDefinitionKnowledge {
    fn lookup(&self, name: &str, exclude: &Url) -> Vec<Location>;
    fn is_defined(&self, name: &str) -> bool;
    fn semantic_token_type(&self, name: &str, exclude: &Url) -> Option<M2SemanticTokenType>;
}

impl<T: WorkspaceDefinitionKnowledge + ?Sized> WorkspaceDefinitionKnowledge for Arc<T> {
    fn lookup(&self, name: &str, exclude: &Url) -> Vec<Location> {
        self.as_ref().lookup(name, exclude)
    }

    fn is_defined(&self, name: &str) -> bool {
        self.as_ref().is_defined(name)
    }

    fn semantic_token_type(&self, name: &str, exclude: &Url) -> Option<M2SemanticTokenType> {
        self.as_ref().semantic_token_type(name, exclude)
    }
}

/// Global definition index, keyed by symbol name, kept in sync with edits and
/// on-disk changes. Open documents are indexed from their live text; unopened
/// files from disk.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceIndex {
    definitions_by_name: DashMap<ObjectName, Vec<DefLocation>>,
    names_by_file: DashMap<Url, Vec<ObjectName>>,
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
    pub(crate) fn scan(&self, knowledge_provider: &ObjectRegistry) {
        for root in self.roots() {
            let mut files = Vec::new();
            collect_m2_files(&root, &mut files);
            for path in files {
                let Ok(uri) = Url::from_file_path(&path) else {
                    continue;
                };
                if self.names_by_file.contains_key(&uri) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                self.index_file(&uri, &text, knowledge_provider);
            }
        }
    }

    /// Replace the definitions recorded for `uri` with those parsed from `text`.
    pub(crate) fn index_file(&self, uri: &Url, text: &str, knowledge_provider: &ObjectRegistry) {
        self.remove_file(uri);
        let definitions = top_level_definitions(text, knowledge_provider);
        if definitions.is_empty() {
            return;
        }
        let mut names = Vec::with_capacity(definitions.len());
        for (name, range, semantic_token_type) in definitions {
            let name = ObjectName::new(name);
            self.definitions_by_name
                .entry(name.clone())
                .or_default()
                .push(DefLocation {
                    uri: uri.clone(),
                    range,
                    semantic_token_type,
                });
            names.push(name);
        }
        self.names_by_file.insert(uri.clone(), names);
    }

    pub(crate) fn remove_file(&self, uri: &Url) {
        let Some((_, names)) = self.names_by_file.remove(uri) else {
            return;
        };
        for name in names {
            let now_empty = if let Some(mut locations) = self.definitions_by_name.get_mut(&name) {
                locations.retain(|location| &location.uri != uri);
                locations.is_empty()
            } else {
                false
            };
            if now_empty {
                self.definitions_by_name.remove(&name);
            }
        }
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

impl WorkspaceDefinitionKnowledge for WorkspaceIndex {
    fn lookup(&self, name: &str, exclude: &Url) -> Vec<Location> {
        self.definitions_by_name
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

    fn is_defined(&self, name: &str) -> bool {
        self.definitions_by_name.contains_key(name)
    }

    fn semantic_token_type(&self, name: &str, exclude: &Url) -> Option<M2SemanticTokenType> {
        self.definitions_by_name.get(name).and_then(|locations| {
            locations
                .iter()
                .find(|location| &location.uri != exclude)
                .map(|location| location.semantic_token_type)
        })
    }
}

/// Recursively collect `.m2` files, skipping hidden, build, and vendored dirs.
fn collect_m2_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
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
fn top_level_definitions(
    text: &str,
    knowledge_provider: &ObjectRegistry,
) -> Vec<(String, Range, M2SemanticTokenType)> {
    let Some(snapshot) = DocumentSnapshot::from_text(text.to_string(), knowledge_provider) else {
        return Vec::new();
    };
    let knowledge = snapshot.object_registry();
    let analysis = snapshot.analysis();
    analysis
        .bindings_in_scope(0)
        .map(|binding| {
            let token_type = local_symbol_semantic_token_type(&binding, &knowledge);
            (binding.name.name().to_string(), binding.range, token_type)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_registry::ObjectRegistry;

    #[test]
    fn indexes_and_looks_up_cross_file_definitions() {
        let builtins = ObjectRegistry::default();
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

    #[test]
    fn imported_package_knowledge_classifies_workspace_definitions() {
        let corpus = concat!(
            "{\"kind\":\"meta\",\"default_loaded\":[\"Core\"]}\n",
            "{\"kind\":\"type\",\"name\":\"Type\",",
            "\"package\":\"$Core$Core\",\"class\":\"$Core$Type\",",
            "\"parent\":\"$Core$Thing\",\"ancestors\":[\"$Core$Thing\"]}\n",
            "{\"kind\":\"type\",\"name\":\"MethodFunction\",",
            "\"package\":\"$Core$Core\",\"class\":\"$Core$Type\",",
            "\"parent\":\"$Core$Function\",\"ancestors\":[\"$Core$Function\",\"$Core$Thing\"]}\n",
            "{\"kind\":\"methodFunction\",\"name\":\"packageType\",",
            "\"package\":\"$Pkg$Pkg\",\"class\":\"$Core$MethodFunction\",",
            "\"methods\":[{\"domain\":[\"$Core$ZZ\"],\"typicalValue\":\"$Core$Type\"}]}\n",
        );
        let knowledge = ObjectRegistry::load(corpus);
        let index = WorkspaceIndex::default();
        let definitions = Url::parse("file:///definitions.m2").unwrap();
        let reference = Url::parse("file:///reference.m2").unwrap();

        index.index_file(
            &definitions,
            "needsPackage \"Pkg\"\nGreeting = packageType 1\n",
            &knowledge,
        );

        assert_eq!(
            index.semantic_token_type("Greeting", &reference),
            Some(M2SemanticTokenType::Class)
        );
    }
}
