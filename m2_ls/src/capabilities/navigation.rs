use std::collections::HashMap;

use tower_lsp::lsp_types::*;

use crate::document::DocumentSnapshot;
use crate::package_index::SourceResolver;
use crate::record_lsp::record_symbol_kind;
use crate::typesystem::{BuiltinData, InstanceID, Record};
use crate::util::*;

pub(crate) fn completion_response(
    text: &str,
    position: Position,
    builtins: &BuiltinData,
    active_package_indexes: &[(String, BuiltinData)],
) -> Option<CompletionResponse> {
    let prefix = symbol_prefix_at(text, position)?;

    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();

    for (package, package_index) in active_package_indexes {
        for name in package_index.names_with_prefix(&prefix, 40) {
            if seen.insert(name.to_string()) {
                items.push(CompletionItem {
                    label: name.to_string(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    detail: Some(format!("Package: {package}")),
                    ..Default::default()
                });
            }
        }
    }

    items.extend(
        builtins
            .names_with_prefix(&prefix, 80usize.saturating_sub(items.len()))
            .into_iter()
            .filter(|name| seen.insert((*name).to_string()))
            .map(|name| CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                ..Default::default()
            }),
    );

    Some(CompletionResponse::Array(items))
}

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
    loaded_package_indexes: &[(String, BuiltinData)],
    builtins: &BuiltinData,
    record_location: impl Fn(&Record) -> Option<Location>,
) -> Vec<SymbolInformation> {
    let mut symbols = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (package, package_index) in loaded_package_indexes {
        for name in package_index.matching_names(query, 80) {
            let Some(record) = package_index.get_record(&InstanceID(name.to_string())) else {
                continue;
            };
            let Some(location) = record_location(&record) else {
                continue;
            };
            if seen.insert(workspace_symbol_dedupe_key(package, name)) {
                symbols.push(SymbolInformation {
                    name: name.to_string(),
                    kind: record_symbol_kind(&record),
                    tags: None,
                    deprecated: None,
                    location,
                    container_name: Some(package.clone()),
                });
            }
        }
    }

    for name in builtins.matching_names(query, 120usize.saturating_sub(symbols.len())) {
        if !should_include_workspace_symbol("Core", name) {
            continue;
        }
        let Some(record) = builtins.get_record(&InstanceID(name.to_string())) else {
            continue;
        };
        let Some(location) = record_location(&record) else {
            continue;
        };
        if seen.insert(workspace_symbol_dedupe_key("Core", name)) {
            symbols.push(SymbolInformation {
                name: name.to_string(),
                kind: record_symbol_kind(&record),
                tags: None,
                deprecated: None,
                location,
                container_name: Some("Core".to_string()),
            });
        }
    }

    symbols
}

pub(crate) fn goto_definition_response(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
    builtins: &BuiltinData,
    active_package_indexes: &[(String, BuiltinData)],
    source_resolver: &SourceResolver,
    workspace_index: &crate::workspace_index::WorkspaceIndex,
    record_location: impl Fn(&Record) -> Option<Location>,
) -> Option<GotoDefinitionResponse> {
    let text = document.text();
    let analysis = document.analysis();
    let node = document.node_at_position_minimal(position)?;

    if let Some(string_node) = document.enclosing_node_of_kind(node, "string_literal") {
        if let Some(package_name) = crate::package_index::package_source_string(text, string_node) {
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

    let kind = node.kind();
    if kind != "symbol" {
        return None;
    }

    let node_text = &text[node.start_byte()..node.end_byte()];

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

    for (_, package_index) in active_package_indexes {
        if let Some(record) = package_index.get_record(&InstanceID(node_text.to_string())) {
            if let Some(location) = record_location(&record) {
                return Some(GotoDefinitionResponse::Scalar(location));
            }
        }
    }

    if let Some(record) = builtins.get_record(&InstanceID(node_text.to_string())) {
        if let Some(location) = record_location(&record) {
            return Some(GotoDefinitionResponse::Scalar(location));
        }
    }

    None
}

pub(crate) fn collect_reference_ranges(
    document: &DocumentSnapshot,
    position: Position,
    include_declaration: bool,
) -> Vec<Range> {
    let text = document.text();
    let analysis = document.analysis();
    let root_node = document.root_node();
    let Some(target_node) = document.symbol_node_at_position(position) else {
        return Vec::new();
    };
    let target_name = &text[target_node.start_byte()..target_node.end_byte()];
    let Some(target_symbol) = analysis.get_symbol_at(target_name, position) else {
        return Vec::new();
    };
    let target_range = target_symbol.range;

    let mut references = Vec::new();
    let mut cursor = root_node.walk();
    let mut reached_root = false;
    while !reached_root {
        let node = cursor.node();
        if matches!(node.kind(), "symbol" | "identifier" | "resolved_symbol") {
            let node_text = &text[node.start_byte()..node.end_byte()];
            if node_text == target_name {
                let position = document.range_for(node).start;
                if let Some(symbol) = analysis.get_symbol_at(node_text, position) {
                    let range = document.range_for(node);
                    if symbol.range == target_range
                        && (include_declaration || range != target_range)
                    {
                        references.push(range);
                    }
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
    let node = document.symbol_node_at_position(position)?;
    let name = document.text_for(node);
    document.analysis().get_symbol_at(name, position)?;
    Some(document.range_for(node))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentSnapshot;
    use tower_lsp::lsp_types::{Position, Range};

    fn document(text: &str) -> DocumentSnapshot {
        DocumentSnapshot::from_text(
            text.to_string(),
            &crate::typesystem::BuiltinData::load_from_split("", ""),
        )
        .expect("fixture should parse")
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
}
