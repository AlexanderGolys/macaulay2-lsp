//! Completion and navigation features: definition, references, rename, and
//! workspace-symbol queries.

use std::collections::HashMap;

use tower_lsp::lsp_types::*;

use crate::document::{DocumentSnapshot, TargetSymbol};
use crate::node_metadata::{NodeKind, NodeKindMetadata};
use crate::package_index::SourceResolver;
use crate::record_lsp::record_symbol_kind;
use crate::typesystem::{InstanceID, LspKnowledge, Record};
use crate::util::*;
use crate::workspace_index::WorkspaceDefinitionKnowledge;

/// The M2 keywords offered as completions — the control-flow, declaration, and
/// value keywords a user types (a subset of all reserved words: the ones worth
/// completing, not internal debug operators).
const COMPLETION_KEYWORDS: &[&str] = &[
    "if",
    "then",
    "else",
    "for",
    "from",
    "to",
    "do",
    "list",
    "while",
    "when",
    "in",
    "of",
    "break",
    "continue",
    "return",
    "try",
    "catch",
    "throw",
    "new",
    "and",
    "or",
    "not",
    "method",
    "true",
    "false",
    "null",
    "symbol",
    "local",
    "global",
    "threadLocal",
];

pub(crate) fn completion_response(
    text: &str,
    position: Position,
    analysis: &crate::analysis::Analysis,
    knowledge: &(impl LspKnowledge + ?Sized),
) -> Option<CompletionResponse> {
    let prefix = symbol_prefix_at(text, position)?;
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Local in-scope symbols first — a user binding shadows a builtin of the
    // same name, so it wins the de-dup.
    for (name, kind) in analysis.in_scope_symbols(&prefix, position) {
        if seen.insert(name.clone()) {
            items.push(CompletionItem {
                label: name,
                kind: Some(completion_item_kind(kind)),
                ..Default::default()
            });
        }
    }

    // Keywords.
    for keyword in COMPLETION_KEYWORDS {
        if keyword.starts_with(&prefix) && seen.insert((*keyword).to_string()) {
            items.push(CompletionItem {
                label: (*keyword).to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
    }

    // Builtin / imported package names from the scoped index.
    for (package, name) in knowledge.names_with_prefix(&prefix, 80) {
        if seen.insert(name.clone()) {
            items.push(CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::FUNCTION),
                // Label provenance only for non-baseline packages, matching prior UX.
                detail: (package != "Core").then(|| format!("Package: {package}")),
                ..Default::default()
            });
        }
    }

    Some(CompletionResponse::Array(items))
}

/// Map an analysis symbol kind to the completion-item kind shown in the editor.
fn completion_item_kind(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::FUNCTION => CompletionItemKind::FUNCTION,
        _ => CompletionItemKind::VARIABLE,
    }
}

/// The in-file references of the symbol at `position` as LSP locations.
pub(crate) fn references_response(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    collect_reference_ranges(document, position, include_declaration)
        .into_iter()
        .map(|range| Location {
            uri: uri.clone(),
            range,
        })
        .collect()
}

#[allow(deprecated)]
pub(crate) fn workspace_symbols_response(
    query: &str,
    knowledge: &(impl LspKnowledge + ?Sized),
    record_location: impl Fn(&Record) -> Option<Location>,
) -> Vec<SymbolInformation> {
    let mut symbols = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (package, name) in knowledge.matching_names(query, 120) {
        if !should_include_workspace_symbol(&package, &name) {
            continue;
        }
        let Some(record) = knowledge.get_record(&InstanceID::new(&name)) else {
            continue;
        };
        let Some(location) = record_location(&record) else {
            continue;
        };
        if seen.insert(workspace_symbol_dedupe_key(&package, &name)) {
            symbols.push(SymbolInformation {
                name,
                kind: record_symbol_kind(&record),
                tags: None,
                deprecated: None,
                location,
                container_name: Some(package),
            });
        }
    }

    symbols
}

/// Go-to-definition at `position`: package-source jump for an import string,
/// else local binding, else a cross-file workspace definition, else the
/// builtin/package record's source location.
pub(crate) fn goto_definition_response(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
    knowledge: &(impl LspKnowledge + ?Sized),
    source_resolver: &SourceResolver,
    workspace_index: &(impl WorkspaceDefinitionKnowledge + ?Sized),
    record_location: impl Fn(&Record) -> Option<Location>,
) -> Option<GotoDefinitionResponse> {
    let analysis = document.analysis();
    let node = document.node_at_position_minimal(position)?;
    let documentation_reference = document.documentation_reference_at(position);

    if let Some(string_node) = document.enclosing_node_of_kind(node, NodeKind::StringLiteral) {
        if let Some(package_name) = crate::package_index::package_source_string(string_node) {
            if let Some(path) = source_resolver.resolve_package_file(package_name) {
                if let Ok(uri) = Url::from_file_path(path) {
                    return Some(GotoDefinitionResponse::Scalar(Location {
                        uri,
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    }));
                }
            }
        }
    }

    let node_text = if let Some(reference) = documentation_reference {
        reference.name(document.text())
    } else {
        if node.kind != NodeKind::Symbol {
            return None;
        }
        node.text()
    };

    if let Some(range) = analysis.find_definition(node_text, position) {
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range,
        }));
    }

    // Cross-file: a top-level definition of this name in another workspace file.
    // User code outranks installed packages and builtins.
    let workspace_locations = workspace_index.lookup(node_text, uri);
    if !workspace_locations.is_empty() {
        return Some(GotoDefinitionResponse::Array(workspace_locations));
    }

    if let Some(record) = knowledge.get_record(&InstanceID(node_text.to_string())) {
        if let Some(location) = record_location(&record) {
            return Some(GotoDefinitionResponse::Scalar(location));
        }
    }

    None
}

/// Every in-file range referring to the same binding as the symbol at
/// `position` (scope-aware: shadowed names in other scopes are excluded).
pub(crate) fn collect_reference_ranges(
    document: &DocumentSnapshot,
    position: Position,
    include_declaration: bool,
) -> Vec<Range> {
    let Some(target) = document.target_symbol_at(position) else {
        return Vec::new();
    };
    reference_ranges_resolved(target, document, include_declaration)
}

/// The reference-range collection step, given a pre-resolved target. Split out
/// of [`collect_reference_ranges`] so a caller that has already resolved the
/// target (e.g. document highlight, which also needs the declaration range) can
/// reuse it instead of resolving a second time.
pub(crate) fn reference_ranges_resolved(
    target: TargetSymbol<'_>,
    document: &DocumentSnapshot,
    include_declaration: bool,
) -> Vec<Range> {
    let analysis = document.analysis();
    let root_node = document.root_node();
    let target_name = target.name;
    let target_range = target.symbol.range;

    let mut references = Vec::new();
    for node in root_node.descendants() {
        if node.kind.is_symbol_like() {
            let node_text = node.text();
            if node_text == target_name {
                let range = document.range_for(node);
                if let Some(symbol) = analysis.get_symbol_at(node_text, range.start) {
                    if symbol.range == target_range
                        && (include_declaration || range != target_range)
                    {
                        references.push(range);
                    }
                }
            }
        }
    }
    for reference in document.documentation_references() {
        if reference.name(document.text()) != target_name {
            continue;
        }
        let range = reference.range();
        if let Some(symbol) = analysis.get_symbol_at(target_name, range.start) {
            if symbol.range == target_range && (include_declaration || range != target_range) {
                references.push(range);
            }
        }
    }

    references.sort_by_key(|range| (range.start, range.end));
    references.dedup();
    references
}

/// The range of the symbol that would be renamed at `position`, or `None` if the
/// position is not on a renameable symbol. Only user symbols with a resolvable
/// binding qualify — builtins and package symbols are excluded, since renaming
/// them in this file alone would silently break the calls they stand for.
pub(crate) fn prepare_rename_range(
    document: &DocumentSnapshot,
    position: Position,
) -> Option<Range> {
    let target = document.target_symbol_at(position)?;
    Some(target.range)
}

/// A workspace edit renaming every in-file reference of the symbol at `position`
/// (including its declaration) to `new_name`, or `None` if there is nothing to
/// rename. References are resolved scope-aware via [`collect_reference_ranges`],
/// so shadowed names in other scopes are left untouched.
pub(crate) fn rename_edits(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    if new_name.trim().is_empty() {
        return None;
    }
    let ranges = collect_reference_ranges(document, position, true);
    if ranges.is_empty() {
        return None;
    }
    let edits = ranges
        .into_iter()
        .map(|range| TextEdit {
            range,
            new_text: new_name.to_string(),
        })
        .collect();
    Some(WorkspaceEdit {
        changes: Some(HashMap::from([(uri.clone(), edits)])),
        document_changes: None,
        change_annotations: None,
    })
}

/// What a references request at `position` targets. A local binding's references
/// stay in the document; a global (top-level / workspace) symbol's references
/// span every file.
pub(crate) enum ReferenceTarget {
    Local,
    Global(String),
}

/// Classify the symbol at `position`: a binding in an inner scope is `Local`; a
/// top-level binding, or an undefined-here name that is defined at top level
/// elsewhere in the workspace, is `Global`.
pub(crate) fn reference_target(
    document: &DocumentSnapshot,
    position: Position,
    workspace_index: &(impl WorkspaceDefinitionKnowledge + ?Sized),
) -> Option<ReferenceTarget> {
    let (name, _) = document.symbol_occurrence_at(position)?;
    let name = name.to_string();
    match document.analysis().get_binding_at(&name, position) {
        Some(binding) if binding.scope_idx != 0 => Some(ReferenceTarget::Local),
        Some(_) => Some(ReferenceTarget::Global(name)),
        None => workspace_index
            .is_defined(&name)
            .then_some(ReferenceTarget::Global(name)),
    }
}

/// Every occurrence of `name` in `document` that refers to a global (top-level)
/// definition — i.e. one not shadowed by a local binding at that point. Used to
/// gather a workspace-global symbol's references file by file.
pub(crate) fn global_reference_ranges(document: &DocumentSnapshot, name: &str) -> Vec<Range> {
    let analysis = document.analysis();
    let root_node = document.root_node();
    let mut references = Vec::new();
    for node in root_node.descendants() {
        if node.kind.is_symbol_like() && node.text() == name {
            let position = document.range_for(node).start;
            // A use is global unless a local binding shadows the name here.
            let shadowed = analysis
                .get_binding_at(name, position)
                .is_some_and(|binding| binding.scope_idx != 0);
            if !shadowed {
                references.push(document.range_for(node));
            }
        }
    }
    for reference in document.documentation_references() {
        if reference.name(document.text()) != name {
            continue;
        }
        let range = reference.range();
        let shadowed = analysis
            .get_binding_at(name, range.start)
            .is_some_and(|binding| binding.scope_idx != 0);
        if !shadowed {
            references.push(range);
        }
    }
    references.sort_by_key(|range| (range.start, range.end));
    references.dedup();
    references
}

/// Every occurrence of `name` that is not bound by user code at that source
/// position. Used for in-document builtin highlighting: local shadows must not
/// light up with the library object they replace.
pub(crate) fn unbound_reference_ranges(document: &DocumentSnapshot, name: &str) -> Vec<Range> {
    let analysis = document.analysis();
    let mut references = document
        .root_node()
        .descendants()
        .filter(|node| node.kind.is_symbol_like() && node.text() == name)
        .filter_map(|node| {
            let range = document.range_for(node);
            analysis
                .get_binding_at(name, range.start)
                .is_none()
                .then_some(range)
        })
        .collect::<Vec<_>>();

    references.extend(
        document
            .documentation_references()
            .iter()
            .filter(|reference| reference.name(document.text()) == name)
            .filter_map(|reference| {
                let range = reference.range();
                analysis
                    .get_binding_at(name, range.start)
                    .is_none()
                    .then_some(range)
            }),
    );
    references.sort_by_key(|range| (range.start, range.end));
    references.dedup();
    references
}

pub(crate) fn symbol_prefix_at(text: &str, position: Position) -> Option<String> {
    let line = text.lines().nth(position.line as usize)?;
    let cursor = utf16_col_to_byte(line, position.character);
    let start = line[..cursor]
        .char_indices()
        .rev()
        .find_map(|(index, ch)| {
            if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                None
            } else {
                Some(index + ch.len_utf8())
            }
        })
        .unwrap_or(0);
    let prefix = &line[start..cursor];
    (!prefix.is_empty()).then(|| prefix.to_string())
}

pub(crate) fn workspace_symbol_dedupe_key(package: &str, name: &str) -> String {
    format!("{package}:{name}")
}

pub(crate) fn should_include_workspace_symbol(package: &str, name: &str) -> bool {
    !(package == "Core" && name.starts_with("Core$"))
}

/// Whether `name` is a valid M2 identifier (a letter followed by letters and
/// digits). A rename target failing this would silently produce unparsable
/// code, so the rename request must reject it instead of editing.
pub(crate) fn is_valid_m2_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentSnapshot;
    use crate::workspace_index::WorkspaceIndex;
    use tower_lsp::lsp_types::{Position, Range};

    fn document(text: &str) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), &crate::typesystem::BuiltinData::empty())
            .expect("fixture should parse")
    }

    #[test]
    fn m2_identifier_validation_accepts_plain_identifiers_only() {
        assert!(is_valid_m2_identifier("foo"));
        assert!(is_valid_m2_identifier("foo42"));
        assert!(!is_valid_m2_identifier("42foo"));
        assert!(!is_valid_m2_identifier("a b"));
        assert!(!is_valid_m2_identifier(""));
    }

    fn completion_labels(text: &str, position: Position) -> Vec<String> {
        use crate::partitioned_index::{LoadedPackages, PackagePartitionedIndex};
        let document = document(text);
        let index = PackagePartitionedIndex::from_corpus(include_str!("../data/m2-index.jsonl"));
        let loaded = LoadedPackages::resolve(index.default_loaded(), text);
        let scoped = index.scoped(&loaded);
        match completion_response(text, position, document.analysis(), &scoped) {
            Some(CompletionResponse::Array(items)) => {
                items.into_iter().map(|item| item.label).collect()
            }
            other => panic!("expected an array completion response, got {other:?}"),
        }
    }

    #[test]
    fn completion_merges_locals_keywords_and_builtins() {
        // Local bindings appear for a matching prefix.
        let locals = completion_labels("myvar = 1\nmyfun = x -> x\nmy\n", Position::new(2, 2));
        assert!(locals.contains(&"myvar".to_string()), "got {locals:?}");
        assert!(locals.contains(&"myfun".to_string()), "got {locals:?}");

        // Keywords appear for a matching prefix.
        let keywords = completion_labels("wh\n", Position::new(0, 2));
        assert!(keywords.contains(&"while".to_string()), "got {keywords:?}");
        assert!(keywords.contains(&"when".to_string()), "got {keywords:?}");

        // Builtin index names still appear.
        let builtins = completion_labels("ZZ\n", Position::new(0, 2));
        assert!(builtins.contains(&"ZZ".to_string()), "got {builtins:?}");
    }

    #[test]
    fn symbol_prefix_uses_lsp_utf16_columns() {
        assert_eq!(
            symbol_prefix_at("éideal", Position::new(0, 3)).as_deref(),
            Some("éid")
        );
        assert_eq!(
            symbol_prefix_at("😀 ideal", Position::new(0, 7)).as_deref(),
            Some("idea")
        );
    }

    #[test]
    fn collect_reference_ranges_finds_same_file_local_symbols() {
        let text = "f := x -> (y := x + x; y)\nf 1";
        let document = document(text);

        let with_declaration = collect_reference_ranges(&document, Position::new(0, 16), true);
        let without_declaration = collect_reference_ranges(&document, Position::new(0, 16), false);

        assert_eq!(
            with_declaration,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(0, 16), Position::new(0, 17)),
                Range::new(Position::new(0, 20), Position::new(0, 21)),
            ]
        );
        assert_eq!(
            without_declaration,
            vec![
                Range::new(Position::new(0, 16), Position::new(0, 17)),
                Range::new(Position::new(0, 20), Position::new(0, 21)),
            ]
        );
    }

    #[test]
    fn collect_reference_ranges_finds_forward_references_through_closures() {
        // `h` is used in `g`'s body before `h` is defined. M2 closures are
        // late-bound, so the use binds to the later top-level `h`; rename and
        // highlight must include it.
        let text = "g := x -> h x\nh := y -> y\n";
        let document = document(text);

        // Resolve from the declaration of `h` (line 1).
        let ranges = collect_reference_ranges(&document, Position::new(1, 0), true);

        assert!(
            ranges.contains(&Range::new(Position::new(0, 10), Position::new(0, 11))),
            "forward reference to `h` in g's body must be collected, got {ranges:?}"
        );
        assert!(
            ranges.contains(&Range::new(Position::new(1, 0), Position::new(1, 1))),
            "the declaration of `h` must be collected, got {ranges:?}"
        );
    }

    #[test]
    fn backtick_documentation_mentions_are_scope_aware_references() {
        let text = "x := 1\n-- use `x`\nx\n";
        let document = document(text);
        let ranges = collect_reference_ranges(&document, Position::new(1, 8), true);

        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 0), Position::new(0, 1)),
                Range::new(Position::new(1, 8), Position::new(1, 9)),
                Range::new(Position::new(2, 0), Position::new(2, 1)),
            ]
        );
        assert!(matches!(
            reference_target(
                &document,
                Position::new(1, 8),
                &WorkspaceIndex::default()
            ),
            Some(ReferenceTarget::Global(name)) if name == "x"
        ));
    }

    #[test]
    fn goto_definition_resolves_from_a_backtick_documentation_mention() {
        let text = "x := 1\n-- use `x`\n";
        let document = document(text);
        let index = crate::partitioned_index::PackagePartitionedIndex::from_corpus(include_str!(
            "../data/m2-index.jsonl"
        ));
        let loaded =
            crate::partitioned_index::LoadedPackages::resolve(index.default_loaded(), text);
        let scoped = index.scoped(&loaded);
        let uri = Url::parse("file:///t.m2").expect("uri");

        assert_eq!(
            goto_definition_response(
                &document,
                &uri,
                Position::new(1, 8),
                &scoped,
                &SourceResolver::new(Vec::new()),
                &WorkspaceIndex::default(),
                |_| None,
            ),
            Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            }))
        );
    }

    #[test]
    fn rename_from_code_updates_backtick_documentation_mentions() {
        let text = "f := x -> (\n-- return `x`\nx + x)\n";
        let document = document(text);
        let uri = Url::parse("file:///t.m2").expect("uri");
        let edits = rename_edits(&document, &uri, Position::new(0, 5), "value")
            .expect("parameter should be renameable")
            .changes
            .expect("simple changes")[&uri]
            .iter()
            .map(|edit| edit.range)
            .collect::<Vec<_>>();

        assert_eq!(
            edits,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(1, 11), Position::new(1, 12)),
                Range::new(Position::new(2, 0), Position::new(2, 1)),
                Range::new(Position::new(2, 4), Position::new(2, 5)),
            ]
        );
    }

    #[test]
    fn reference_ranges_use_lsp_utf16_columns() {
        let text = "f := x -> (\"😀\"; x + x)";
        let document = document(text);

        let ranges = collect_reference_ranges(&document, Position::new(0, 17), true);

        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(0, 17), Position::new(0, 18)),
                Range::new(Position::new(0, 21), Position::new(0, 22)),
            ]
        );
    }

    #[test]
    fn workspace_symbols_omit_core_qualified_twins() {
        assert!(!should_include_workspace_symbol("Core", "Core$name"));
        assert!(should_include_workspace_symbol("Core", "name"));
        assert!(should_include_workspace_symbol("SomePackage", "Core$name"));
    }

    #[test]
    fn rename_edits_replace_every_in_scope_reference() {
        let text = "f := x -> (y := x + x; y)\nf 1";
        let document = document(text);
        let uri = Url::parse("file:///test.m2").expect("uri");

        // Cursor on a use of `x`; rename to `z`.
        let edit = rename_edits(&document, &uri, Position::new(0, 16), "z")
            .expect("local symbol should be renameable");
        let edits = &edit.changes.expect("simple changes")[&uri];

        // Declaration + both uses, all rewritten to the new name.
        assert_eq!(
            edits.iter().map(|e| e.range).collect::<Vec<_>>(),
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(0, 16), Position::new(0, 17)),
                Range::new(Position::new(0, 20), Position::new(0, 21)),
            ]
        );
        assert!(edits.iter().all(|e| e.new_text == "z"));
    }

    #[test]
    fn prepare_rename_accepts_user_symbols_and_rejects_others() {
        let text = "f := x -> (x + x)";
        let document = document(text);
        // On the parameter `x`.
        assert!(prepare_rename_range(&document, Position::new(0, 5)).is_some());
        // Empty new name is refused.
        let uri = Url::parse("file:///t.m2").expect("uri");
        assert!(rename_edits(&document, &uri, Position::new(0, 5), "  ").is_none());
    }

    #[test]
    fn rename_includes_quoted_symbol_reference() {
        // `symbol M` names the identifier `M` (a plain `symbol` node); renaming the
        // user-defined `M` must rewrite that occurrence too.
        let text = "f := M -> (symbol M; M + 1)";
        let document = document(text);
        let uri = Url::parse("file:///t.m2").expect("uri");
        let edits = rename_edits(&document, &uri, Position::new(0, 5), "N")
            .expect("user symbol should be renameable")
            .changes
            .expect("simple changes")[&uri]
            .iter()
            .map(|edit| edit.range.start.character)
            .collect::<Vec<_>>();
        // parameter `M`, the `M` in `symbol M`, and the `M` in `M + 1`.
        assert_eq!(edits, vec![5, 18, 21]);
    }

    #[test]
    fn rename_rejects_symbols_not_defined_by_the_user() {
        let uri = Url::parse("file:///t.m2").expect("uri");
        // `Algorithm` is a global reserved option key, not a user definition.
        let opt = document("g := gens gb(I, Algorithm => Homogeneous)");
        assert!(rename_edits(&opt, &uri, Position::new(0, 16), "Z").is_none());
        assert!(prepare_rename_range(&opt, Position::new(0, 16)).is_none());
        // Keywords and punctuation that the grammar resolves into symbols.
        let kw = document("z = a and b");
        assert!(rename_edits(&kw, &uri, Position::new(0, 6), "Z").is_none());
        let brace = document("x = {1, 2}");
        assert!(rename_edits(&brace, &uri, Position::new(0, 4), "Z").is_none());
    }

    #[test]
    fn reference_target_classifies_local_versus_global() {
        struct KnownGlobal;

        impl WorkspaceDefinitionKnowledge for KnownGlobal {
            fn lookup(&self, _name: &str, _exclude: &Url) -> Vec<Location> {
                Vec::new()
            }

            fn is_defined(&self, name: &str) -> bool {
                name == "shared"
            }

            fn semantic_token_type(
                &self,
                _name: &str,
                _exclude: &Url,
            ) -> Option<crate::typesystem::M2SemanticTokenType> {
                None
            }
        }

        let index = crate::workspace_index::WorkspaceIndex::default();
        // Lambda parameter -> local (references stay in-file).
        let local = document("f := x -> (x + x)");
        assert!(matches!(
            reference_target(&local, Position::new(0, 5), &index),
            Some(ReferenceTarget::Local)
        ));
        // Top-level binding -> global.
        let global = document("y = 5\nz = y");
        assert!(matches!(
            reference_target(&global, Position::new(0, 0), &index),
            Some(ReferenceTarget::Global(name)) if name == "y"
        ));
        let external = document("shared");
        assert!(matches!(
            reference_target(&external, Position::new(0, 0), &KnownGlobal),
            Some(ReferenceTarget::Global(name)) if name == "shared"
        ));
    }

    #[test]
    fn global_reference_ranges_skip_local_shadows() {
        // `g` is a top-level global; `f`'s parameter `g` shadows it inside `f`.
        let document = document("g = 1\nf := g -> (g + g)\nh = g + g");
        let ranges = global_reference_ranges(&document, "g");
        // The global definition and its top-level uses, not the shadowed uses.
        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 0), Position::new(0, 1)),
                Range::new(Position::new(2, 4), Position::new(2, 5)),
                Range::new(Position::new(2, 8), Position::new(2, 9)),
            ]
        );
    }
}
