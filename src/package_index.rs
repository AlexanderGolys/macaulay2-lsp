//! Imported-package discovery and configured source-file resolution.

use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{Location, Position, Range, Url};

use crate::builtin_index::Record;
use crate::node_metadata::{M2Node, M2Parser, NodeKind};
use crate::record_lsp::record_source_file;

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
        if let Some(paths) = env::var_os("M2_LSP_SOURCE_PATH") {
            roots.extend(env::split_paths(&paths));
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

    /// The LSP location of a builtin/package record's source definition, when
    /// the resolver can find the file on disk. Shared by the hover/index views
    /// and type-hierarchy plumbing, so it lives on the resolver rather than the
    /// backend to keep `capabilities/type_hierarchy` self-contained.
    pub(crate) fn record_location(&self, record: &Record) -> Option<Location> {
        let source_file = record_source_file(record)?;
        let path = self.resolve_source_file(source_file)?;
        let uri = Url::from_file_path(path).ok()?;
        let position = Position::new(0, 0);
        Some(Location {
            uri,
            range: Range::new(position, position),
        })
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
        if parent.kind == NodeKind::Sequence && !parent.is_first_collection_element(node) {
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
    let Some(mut parser) = M2Parser::new() else {
        return Vec::new();
    };
    parser
        .parse(text)
        .map(collect_imported_packages_in_tree)
        .unwrap_or_default()
}

/// Walk an already-parsed tree for package-import calls, reusing the caller's
/// parse rather than spinning up a fresh parser. The snapshot keeps its tree
/// around, so per-version import collection costs one walk, not a re-parse.
pub(crate) fn collect_imported_packages_in_tree(root: M2Node<'_>) -> Vec<String> {
    let mut packages = Vec::new();
    let mut seen = HashSet::new();
    for node in root.descendants() {
        if node.kind == NodeKind::StringLiteral {
            if let Some(package_name) = package_source_string(node) {
                if seen.insert(package_name.to_string()) {
                    packages.push(package_name.to_string());
                }
            }
        }
    }

    packages
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
    use std::{env, fs};

    use super::{
        collect_imported_packages, package_source_string, M2Parser, NodeKind, SourceResolver,
    };

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

    #[test]
    fn import_argument_selection_skips_muted_parts_but_not_null_slots() {
        let pkgs = collect_imported_packages(
            "loadPackage(ignored;\"AfterMuted\")\nloadPackage(,\"AfterNull\")\nloadPackage(\"Muted\";)\n",
        );
        assert_eq!(pkgs, vec!["AfterMuted".to_string()]);
    }

    #[test]
    fn package_source_string_detects_import_like_calls() {
        let text =
            "needsPackage \"Graphs\"\nloadPackage(\"Normaliz\", Reload => true)\ndebug \"Core\"";
        let mut parser = M2Parser::new().expect("Macaulay2 parser should load");
        let root = parser.parse(text).expect("fixture should parse");
        let packages = root
            .descendants()
            .filter(|node| node.kind == NodeKind::StringLiteral)
            .filter_map(package_source_string)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["Graphs", "Normaliz", "Core"]);
    }

    #[test]
    fn imported_packages_are_deduplicated_in_source_order() {
        let text = "needsPackage \"Graphs\"\nloadPackage(\"Normaliz\")\nneedsPackage \"Graphs\"";

        assert_eq!(
            collect_imported_packages(text),
            vec!["Graphs".to_string(), "Normaliz".to_string()]
        );
    }

    #[test]
    fn source_resolver_finds_package_and_doc_files_from_configured_roots() {
        let root = env::temp_dir().join(format!("m2-lsp-source-resolver-{}", std::process::id()));
        let packages = root.join("Macaulay2").join("packages");
        let docs = packages.join("Macaulay2Doc");
        let core = root.join("Macaulay2").join("m2");
        fs::create_dir_all(&docs).expect("test docs dir should be created");
        fs::create_dir_all(&core).expect("test core dir should be created");
        fs::write(packages.join("Graphs.m2"), "").expect("package fixture should write");
        fs::write(docs.join("operators.m2"), "").expect("doc fixture should write");
        fs::write(core.join("option.m2"), "").expect("core fixture should write");

        let resolver = SourceResolver::new(vec![packages.clone()]);

        assert_eq!(
            resolver.resolve_package_file("Graphs"),
            Some(packages.join("Graphs.m2"))
        );
        assert_eq!(
            resolver.resolve_source_file("Macaulay2Doc/operators.m2"),
            Some(docs.join("operators.m2"))
        );
        assert_eq!(
            resolver.resolve_source_file("m2/option.m2"),
            Some(core.join("option.m2"))
        );

        let _ = fs::remove_dir_all(root);
    }
}
