use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tree_sitter::Parser;

use crate::node_metadata::{M2Node, NodeKind};

#[derive(Debug, Clone)]
pub(crate) struct SourceResolver {
    roots: Vec<PathBuf>,
}

impl SourceResolver {
    /// Source roots are configuration-driven only: the LSP never invokes M2 to
    /// discover them (no runtime dependency on M2 existing, nor on how it is
    /// launched). Sourced from `M2_LSP_SOURCE_PATH`; package-source jumps simply
    /// degrade to nothing when it is unset.
    pub(crate) fn from_environment() -> Self {
        let mut roots = Vec::new();
        if let Some(paths) = std::env::var_os("M2_LSP_SOURCE_PATH") {
            roots.extend(std::env::split_paths(&paths));
        }
        Self::new(roots)
    }

    pub(crate) fn new(roots: Vec<PathBuf>) -> Self {
        let mut deduped = Vec::new();
        for root in roots {
            Self::push_root(&mut deduped, root.clone());
            if root.file_name().is_some_and(|name| name == "packages") {
                if let Some(parent) = root.parent() {
                    Self::push_root(&mut deduped, parent.to_path_buf());
                }
            }
        }
        SourceResolver { roots: deduped }
    }

    fn push_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
        if !roots.iter().any(|existing| existing == &root) {
            roots.push(root);
        }
    }

    pub(crate) fn resolve_source_file(&self, source_file: &str) -> Option<PathBuf> {
        let source = Path::new(source_file);
        if source.is_absolute() && source.exists() {
            return Some(source.to_path_buf());
        }

        self.roots
            .iter()
            .map(|root| root.join(source))
            .find(|candidate| candidate.exists())
    }

    pub(crate) fn resolve_package_file(&self, package_name: &str) -> Option<PathBuf> {
        let source_file = format!("{package_name}.m2");
        self.resolve_source_file(&source_file)
    }
}

/// The function names whose string argument names a package to import.
fn is_package_import_trigger(name: &str) -> bool {
    matches!(
        name,
        "needsPackage" | "loadPackage" | "debug" | "importFrom"
    )
}

pub(crate) fn package_source_string(node: M2Node<'_>) -> Option<&str> {
    let package_name = node.string_literal_inner_text()?;
    let parent = node.parent()?;

    // A single parenthesized argument `loadPackage("Pkg")` is a
    // `parenthesized_expression`; a multi-argument call wraps them in a
    // `sequence`. Both sit between the string and the `callee ARG` application.
    if matches!(
        parent.kind,
        NodeKind::Sequence | NodeKind::ParenthesizedExpression
    ) {
        if parent.kind == NodeKind::Sequence && !is_first_named_child(parent, node) {
            return None;
        }

        return parent
            .parent()
            .and_then(binary_expression_left_symbol)
            .filter(|name| is_package_import_trigger(name))
            .map(|_| package_name);
    }

    binary_expression_left_symbol(parent)
        .filter(|name| is_package_import_trigger(name))
        .map(|_| package_name)
}

pub(crate) fn collect_imported_packages(text: &str) -> Vec<String> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_macaulay2::language())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    collect_imported_packages_in_tree(text, &tree)
}

/// Walk an already-parsed tree for package-import calls, reusing the caller's
/// parse rather than spinning up a fresh parser. The snapshot keeps its tree
/// around, so per-version import collection costs one walk, not a re-parse.
pub(crate) fn collect_imported_packages_in_tree(
    text: &str,
    tree: &tree_sitter::Tree,
) -> Vec<String> {
    let root = tree.root_node();
    let mut packages = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = root.walk();
    let mut reached_root = false;
    while !reached_root {
        let node = M2Node::new(cursor.node(), text);
        if node.kind == NodeKind::StringLiteral {
            if let Some(package_name) = package_source_string(node) {
                if seen.insert(package_name.to_string()) {
                    packages.push(package_name.to_string());
                }
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                reached_root = true;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }

    packages
}

fn is_first_named_child(parent: M2Node<'_>, child: M2Node<'_>) -> bool {
    parent
        .named_child(0)
        .is_some_and(|first| first.id() == child.id())
}

/// The left symbol of an application `callee ARG` (`needsPackage "Pkg"`): the
/// callee name, when `node` is a `SPACE` application whose left operand is a
/// bare symbol.
fn binary_expression_left_symbol(node: M2Node<'_>) -> Option<&str> {
    if !node.is_space_application() {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if left.kind != NodeKind::Symbol {
        return None;
    }

    Some(left.text())
}

#[cfg(test)]
mod import_trigger_tests {
    use super::collect_imported_packages;

    #[test]
    fn import_from_string_form_adds_the_package() {
        let pkgs = collect_imported_packages(r#"importFrom("FooPkg", {"barSym", "bazSym"})"#);
        assert_eq!(pkgs, vec!["FooPkg".to_string()]);
    }

    #[test]
    fn import_from_does_not_capture_symbol_name_strings() {
        // The second-argument symbol strings must NOT be treated as packages.
        let pkgs = collect_imported_packages(r#"importFrom("FooPkg", "barSym")"#);
        assert_eq!(pkgs, vec!["FooPkg".to_string()]);
    }

    #[test]
    fn import_from_package_object_form_adds_nothing() {
        // `importFrom_Core {...}` / `importFrom(Core, ...)` take a Package object,
        // not a string — no package name to detect.
        let pkgs = collect_imported_packages("importFrom_Core {\"raw\"}");
        assert!(
            pkgs.is_empty(),
            "Package-object form must add nothing, got {pkgs:?}"
        );
    }

    #[test]
    fn existing_triggers_still_detected() {
        let pkgs = collect_imported_packages("needsPackage \"A\"\nloadPackage \"B\"\ndebug \"C\"");
        assert_eq!(
            pkgs,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }
}
