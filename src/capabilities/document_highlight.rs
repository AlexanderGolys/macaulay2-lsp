use crate::capabilities::navigation::collect_reference_ranges;
use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind};
use tower_lsp::lsp_types::{
    DocumentHighlight, DocumentHighlightKind, DocumentHighlightOptions, OneOf, Position,
};

pub(crate) fn document_highlight_provider_capability(
) -> Option<OneOf<bool, DocumentHighlightOptions>> {
    Some(OneOf::Left(true))
}

/// Highlight occurrences related to the cursor. A symbol under the cursor lights
/// up all its in-file references; a keyword lights up its compound statement's
/// keyword sequence. The two are mutually exclusive, so references take priority
/// and keyword highlighting is the fallback.
pub(crate) fn document_highlights(
    document: &DocumentSnapshot,
    position: Position,
) -> Option<Vec<DocumentHighlight>> {
    if let Some(highlights) = symbol_reference_highlights(document, position) {
        return Some(highlights);
    }
    keyword_sequence_highlights(document, position)
}

/// Highlight every in-file occurrence of the symbol under the cursor — the same
/// scope-aware set a references request resolves. `None` when the cursor is not
/// on a resolvable symbol, so the caller falls back to keyword highlighting.
fn symbol_reference_highlights(
    document: &DocumentSnapshot,
    position: Position,
) -> Option<Vec<DocumentHighlight>> {
    let target_node = document.symbol_node_at_position(position)?;
    let target_name = document.text_for(target_node);
    let declaration = document
        .analysis()
        .get_symbol_at(target_name, position)?
        .range;
    let ranges = collect_reference_ranges(document, position, true);
    if ranges.is_empty() {
        return None;
    }

    Some(
        ranges
            .into_iter()
            .map(|range| {
                // The binding site is a write; every other occurrence reads it.
                let kind = if range == declaration {
                    DocumentHighlightKind::WRITE
                } else {
                    DocumentHighlightKind::READ
                };
                DocumentHighlight {
                    range,
                    kind: Some(kind),
                }
            })
            .collect(),
    )
}

/// Highlight the keyword sequence of the compound statement under the cursor:
/// resting on any one keyword lights up its siblings (`if`/`then`/`else`,
/// `for`/`in`/`list`/`do`, …). Triggers only when the cursor is on a keyword,
/// not when it is inside the statement's expressions.
fn keyword_sequence_highlights(
    document: &DocumentSnapshot,
    position: Position,
) -> Option<Vec<DocumentHighlight>> {
    let node = M2Node::new(document.node_at_position_minimal(position)?);
    let statement = enclosing_keyword_statement(node)?;
    let keywords = statement_keyword_tokens(statement);

    if !keywords.iter().any(|keyword| node_contains(*keyword, node)) {
        return None;
    }

    Some(
        keywords
            .into_iter()
            .map(|keyword| DocumentHighlight {
                range: document.range_for(keyword.inner()),
                kind: Some(DocumentHighlightKind::TEXT),
            })
            .collect(),
    )
}

/// The compound statements whose leading keyword opens a keyword sequence.
/// Excludes single-keyword statements (`break`, `return`, `step`, …).
fn is_keyword_statement(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::IfStatement
            | NodeKind::ForStatement
            | NodeKind::WhileStatement
            | NodeKind::NewStatement
            | NodeKind::TryStatement
    )
}

/// The clause nodes whose leading keyword belongs to a statement's sequence.
fn is_keyword_clause(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::FromClause
            | NodeKind::ToClause
            | NodeKind::OfClause
            | NodeKind::InClause
            | NodeKind::WhenClause
            | NodeKind::ListClause
            | NodeKind::DoClause
            | NodeKind::ThenClause
            | NodeKind::ElseClause
            | NodeKind::ExceptClause
    )
}

fn enclosing_keyword_statement(mut node: M2Node<'_>) -> Option<M2Node<'_>> {
    loop {
        if is_keyword_statement(node.kind) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Collect the keyword tokens of a compound statement: its own leading keyword
/// plus the leading keyword of each direct clause child. Nested statements are
/// not descended into — they own their own sequence.
fn statement_keyword_tokens(statement: M2Node<'_>) -> Vec<M2Node<'_>> {
    let mut keywords = Vec::new();
    if is_keyword_statement(statement.kind) {
        let Some(kw) = statement.child(0) else {
            return Vec::new();
        };
        keywords.push(kw);
        for child in statement.named_children() {
            let Some(kw_child) = child.child(0) else {
                continue;
            };
            if is_keyword_clause(child.kind) {
                keywords.push(kw_child);
            };
        }
        return keywords;
    }
    if is_keyword_clause(statement.kind) {
        let Some(st) = enclosing_keyword_statement(statement) else {
            return Vec::new();
        };
        return statement_keyword_tokens(st);
    }
    Vec::new()
}

fn node_contains(outer: M2Node<'_>, inner: M2Node<'_>) -> bool {
    inner.start_byte() >= outer.start_byte() && inner.end_byte() <= outer.end_byte()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typesystem::BuiltinData;

    fn document(text: &str) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), &BuiltinData::load_from_split("", ""))
            .expect("fixture should parse")
    }

    fn highlighted_words(text: &str, line: u32, character: u32) -> Vec<String> {
        let document = document(text);
        document_highlights(&document, Position::new(line, character))
            .unwrap_or_default()
            .into_iter()
            .map(|highlight| {
                let range = highlight.range;
                let line_text = text.lines().nth(range.start.line as usize).unwrap_or("");
                line_text[range.start.character as usize..range.end.character as usize].to_string()
            })
            .collect()
    }

    fn highlight_kinds(text: &str, line: u32, character: u32) -> Vec<DocumentHighlightKind> {
        let document = document(text);
        document_highlights(&document, Position::new(line, character))
            .unwrap_or_default()
            .into_iter()
            .map(|highlight| highlight.kind.expect("symbol highlights carry a kind"))
            .collect()
    }

    #[test]
    fn highlights_if_then_else_keywords_from_any_keyword() {
        let text = "if a then b else c\n";
        for character in [0, 5, 12] {
            assert_eq!(
                highlighted_words(text, 0, character),
                vec!["if", "then", "else"],
                "cursor at column {character} should highlight the if-keyword sequence",
            );
        }
    }

    #[test]
    fn highlights_for_in_list_keywords() {
        assert_eq!(
            highlighted_words("for i in L list f i\n", 0, 1),
            vec!["for", "in", "list"]
        );
    }

    #[test]
    fn highlights_for_from_to_do_keywords() {
        assert_eq!(
            highlighted_words("for i from 1 to 10 do g i\n", 0, 7),
            vec!["for", "from", "to", "do"]
        );
    }

    #[test]
    fn does_not_highlight_when_cursor_is_inside_an_expression() {
        assert!(highlighted_words("if a then b else c\n", 0, 3).is_empty());
    }

    #[test]
    fn highlights_all_in_file_references_of_the_symbol_under_cursor() {
        // `x` is bound by `:=` then used twice; resting on any occurrence lights
        // up all three.
        let text = "x := 1\ny := x + x\n";
        for (line, character) in [(0, 0), (1, 5), (1, 9)] {
            assert_eq!(
                highlighted_words(text, line, character),
                vec!["x", "x", "x"],
                "occurrence at {line}:{character} should highlight every use of x",
            );
        }
    }

    #[test]
    fn distinguishes_the_binding_write_from_use_reads() {
        // Declaration is a WRITE; the two uses are READs.
        assert_eq!(
            highlight_kinds("x := 1\ny := x + x\n", 0, 0),
            vec![
                DocumentHighlightKind::WRITE,
                DocumentHighlightKind::READ,
                DocumentHighlightKind::READ,
            ],
        );
    }

    #[test]
    fn loop_variables_are_not_highlighted() {
        // For-loop iterator variables are not registered as bindings (see
        // `analysis::build_scopes`), so they have no references to resolve and
        // fall through to keyword highlighting, which a non-keyword cursor misses.
        // This documents the known gap: closing it belongs in the symbol registry,
        // not here, so highlight/references/rename all gain loop vars together.
        assert!(highlighted_words("for i in L do f i\n", 0, 4).is_empty());
    }

    #[test]
    fn nested_if_highlights_only_the_innermost_statement() {
        // `if a then if b then c` — cursor on the inner `if` lights up only the
        // inner sequence, not the outer.
        let text = "if a then if b then c\n";
        assert_eq!(highlighted_words(text, 0, 10), vec!["if", "then"]);
    }
}
