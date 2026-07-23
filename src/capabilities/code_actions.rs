//! Diagnostic quick fixes and syntax-preserving refactors offered by the LSP.

use std::collections::HashMap;

use tower_lsp::lsp_types::*;

use crate::capabilities::diagnostics::ambiguous_float_member_access_rewrite;
use crate::diagnostic_registry::{diagnostic_has_kind, M2Diagnostic};
use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind};
use crate::util::position_in_range;

/// A code action producer. Every action is funneled through this signature so
/// the dispatcher can iterate a single registry instead of listing each by
/// hand. `cursor` is the deepest CST node covering `position`, precomputed
/// once by `available_code_actions` so the tree-sitter descent is not repeated
/// per producer. Producers that do not consult diagnostics simply ignore the
/// slice.
type ActionProducer =
    fn(&DocumentSnapshot, &Url, Position, M2Node<'_>, &[Diagnostic]) -> Option<CodeAction>;

/// The action registry: ordered as quickfixes first, then refactors. A new
/// action is appended here and `available_code_actions` picks it up with no
/// further wiring.
const ACTION_PRODUCERS: &[ActionProducer] = &[
    ambiguous_float_member_access_code_action,
    convert_to_raw_string_code_action,
    conditional_null_code_action,
    simplify_try_code_action,
    simplify_if_condition_code_action,
];

/// The code actions offered at `position`: every action from the registry
/// whose producer returns `Some`. The deepest CST node covering `position` is
/// resolved a single time here and threaded through the registry, so the
/// tree-sitter descent happens once per request instead of once per producer.
pub(crate) fn available_code_actions(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
    diagnostics: &[Diagnostic],
) -> Option<CodeActionResponse> {
    let cursor = document.node_at_position_minimal(position)?;
    let actions: Vec<_> = ACTION_PRODUCERS
        .iter()
        .filter_map(|producer| producer(document, uri, position, cursor, diagnostics))
        .map(CodeActionOrCommand::CodeAction)
        .collect();
    (!actions.is_empty()).then_some(actions)
}

/// The shared shape of every action this module emits: a title, a kind, and
/// the optional LSP flags (`is_preferred`, `diagnostics`) — plus a single
/// text edit applied to `uri`. Bundling them here collapses the ~15-line
/// `WorkspaceEdit`/`CodeAction` boilerplate that every per-action producer
/// would otherwise repeat.
struct CodeActionSpec {
    title: &'static str,
    kind: CodeActionKind,
    is_preferred: Option<bool>,
    diagnostics: Option<Vec<Diagnostic>>,
}

impl CodeActionSpec {
    fn build(self, uri: &Url, range: Range, new_text: String) -> CodeAction {
        CodeAction {
            title: self.title.to_string(),
            kind: Some(self.kind),
            diagnostics: self.diagnostics,
            edit: Some(WorkspaceEdit {
                changes: Some(HashMap::from([(
                    uri.clone(),
                    vec![TextEdit { range, new_text }],
                )])),
                document_changes: None,
                change_annotations: None,
            }),
            command: None,
            is_preferred: self.is_preferred,
            disabled: None,
            data: None,
        }
    }
}

/// Refactor: rewrite a heavily-escaped string literal as a raw `///…///` string
/// when the value survives verbatim (no unsupported escapes, no `///` inside).
pub(crate) fn convert_to_raw_string_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    _position: Position,
    cursor: M2Node<'_>,
    _diagnostics: &[Diagnostic],
) -> Option<CodeAction> {
    let string_node = document.enclosing_node_of_kind(cursor, NodeKind::StringLiteral)?;
    let replacement = raw_string_replacement(string_node)?;

    Some(
        CodeActionSpec {
            title: "Convert to raw string",
            kind: CodeActionKind::REFACTOR_REWRITE,
            is_preferred: None,
            diagnostics: None,
        }
        .build(uri, document.range_for(string_node), replacement),
    )
}

/// Quickfix for the ambiguous-float diagnostic (`x.3` parses as `x SPACE .3`):
/// rewrite to the member access the user almost certainly meant (`x#3`).
pub(crate) fn ambiguous_float_member_access_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
    cursor: M2Node<'_>,
    diagnostics: &[Diagnostic],
) -> Option<CodeAction> {
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic_has_kind(diagnostic, M2Diagnostic::AmbiguousFloatMemberAccess)
                && position_in_range(position, diagnostic.range)
        })?
        .clone();
    let expression = document.enclosing_node_of_kind(cursor, NodeKind::BinaryExpression)?;
    let replacement = ambiguous_float_member_access_rewrite(expression)?;

    Some(
        CodeActionSpec {
            title: "Rewrite as member access",
            kind: CodeActionKind::QUICKFIX,
            is_preferred: Some(true),
            diagnostics: Some(vec![diagnostic]),
        }
        .build(uri, document.range_for(expression), replacement),
    )
}

/// Refactor: drop a redundant `else null` (or `then null`, negating the
/// condition) from an `if` statement.
pub(crate) fn conditional_null_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    _position: Position,
    cursor: M2Node<'_>,
    _diagnostics: &[Diagnostic],
) -> Option<CodeAction> {
    let if_node = document.enclosing_node_of_kind(cursor, NodeKind::IfStatement)?;
    let replacement = refactor_if_null_branch(if_node)?;

    Some(
        CodeActionSpec {
            title: "Simplify unnecessary null branch",
            kind: CodeActionKind::REFACTOR_REWRITE,
            is_preferred: None,
            diagnostics: None,
        }
        .build(uri, document.range_for(if_node), replacement),
    )
}

/// Refactor: simplify a `try` statement — drop a redundant `then` echo or a
/// redundant `else null`.
pub(crate) fn simplify_try_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    _position: Position,
    cursor: M2Node<'_>,
    _diagnostics: &[Diagnostic],
) -> Option<CodeAction> {
    let try_node = document.enclosing_node_of_kind(cursor, NodeKind::TryStatement)?;
    let replacement = refactor_try_statement(try_node)?;

    Some(
        CodeActionSpec {
            title: "Simplify try",
            kind: CodeActionKind::REFACTOR_REWRITE,
            is_preferred: None,
            diagnostics: None,
        }
        .build(uri, document.range_for(try_node), replacement),
    )
}

/// Refactor: push a leading `not` through a parenthesized comparison
/// (`if not (a == b) then x` → `if a != b then x`).
pub(crate) fn simplify_if_condition_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    _position: Position,
    cursor: M2Node<'_>,
    _diagnostics: &[Diagnostic],
) -> Option<CodeAction> {
    let if_node = document.enclosing_node_of_kind(cursor, NodeKind::IfStatement)?;
    let condition = if_node.child_by_field_name("condition")?;
    let simplified = simplify_condition(condition)?;

    let then_branch = expression_of_clause(clause_child(if_node, NodeKind::ThenClause)?)?;
    let else_clause = clause_child(if_node, NodeKind::ElseClause);

    let mut replacement = format!("if {} then {}", simplified, then_branch.text());
    if let Some(else_clause) = else_clause {
        replacement.push(' ');
        replacement.push_str(else_clause.text());
    }

    Some(
        CodeActionSpec {
            title: "Simplify if condition",
            kind: CodeActionKind::REFACTOR_REWRITE,
            is_preferred: None,
            diagnostics: None,
        }
        .build(uri, document.range_for(if_node), replacement),
    )
}

fn simplify_condition(node: M2Node<'_>) -> Option<String> {
    let original = node.text();

    if node.kind == NodeKind::PrefixExpression {
        if let Some(operator) = node.child_by_field_name("operator") {
            if operator.text() == "not" {
                for child in node.named_children() {
                    if child.id() != operator.id() {
                        let inner = unwrap_parentheses(child);
                        let simplified = negated_condition_text(inner);
                        if simplified != original {
                            return Some(simplified);
                        }
                    }
                }
            }
        }
    }

    // Note: there is deliberately no `(not a) <op> b` => `a <neg-op> b` rule.
    // M2's `not` binds looser than every comparison, so `not a == b` parses as
    // `not (a == b)` (handled above), and the only way to get a comparison whose
    // operand is a `not` is to parenthesize it — `(not a) == b` — which the
    // grammar wraps in a `parenthesized_expression`, not a bare prefix. That
    // rewrite would also be unsound in general (it holds only for the boolean XOR
    // case), so it is intentionally absent rather than dead/unsound code.
    None
}

fn unwrap_parentheses(node: M2Node<'_>) -> M2Node<'_> {
    if node.kind == NodeKind::ParenthesizedExpression && node.child_count() == 3 {
        if let Some(inner) = node.child(1) {
            return inner;
        }
    }
    node
}

fn clause_child<'tree>(parent: M2Node<'tree>, kind: NodeKind) -> Option<M2Node<'tree>> {
    parent.children().find(|child| child.kind == kind)
}

fn expression_of_clause(clause: M2Node<'_>) -> Option<M2Node<'_>> {
    clause.named_children().next()
}

fn raw_string_replacement(string_node: M2Node<'_>) -> Option<String> {
    let content = string_node.string_literal_inner_text()?;
    let escape_count = count_string_escapes(content);
    if escape_count <= 2 {
        return None;
    }

    let unescaped = unescape_string_literal_content(content)?;
    if unescaped.contains("///") {
        return None;
    }

    Some(format!("///{unescaped}///"))
}

fn count_string_escapes(content: &str) -> usize {
    let mut chars = content.chars().peekable();
    let mut count = 0;
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some() {
            count += 1;
            chars.next();
        }
    }
    count
}

fn unescape_string_literal_content(content: &str) -> Option<String> {
    let mut result = String::new();
    let mut chars = content.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }

        let escaped = chars.next()?;
        match escaped {
            '\\' => result.push('\\'),
            '"' => result.push('"'),
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            // Any escape we do not faithfully decode (octal \NNN, hex \xNN,
            // \a \b \f \v, ...) aborts the conversion. A raw string ///...///
            // is verbatim, so pushing the bare trailing character would silently
            // change the string's value (e.g. "\134" is one backslash, not "134").
            _ => return None,
        }
    }
    Some(result)
}

fn is_null_literal(node: M2Node<'_>) -> bool {
    node.kind == NodeKind::Symbol && node.text() == "null"
}

fn not_condition_needs_parentheses(node: M2Node<'_>) -> bool {
    node.kind == NodeKind::BinaryExpression
}

fn negated_binary_operator(operator: &str) -> Option<&'static str> {
    match operator {
        "==" => Some("!="),
        "!=" => Some("=="),
        "===" => Some("=!="),
        "=!=" => Some("==="),
        "<" => Some(">="),
        "<=" => Some(">"),
        ">" => Some("<="),
        ">=" => Some("<"),
        _ => None,
    }
}

fn negated_condition_text(node: M2Node<'_>) -> String {
    if node.kind == NodeKind::PrefixExpression {
        if let Some(operator) = node.child_by_field_name("operator") {
            if operator.text() == "not" {
                for child in node.named_children() {
                    if child.id() != operator.id() {
                        return child.text().to_string();
                    }
                }
            }
        }
    }

    if let Some(operator) = node.binary_operator() {
        if let Some(negated_operator) = negated_binary_operator(operator) {
            // A malformed binary expression (a MISSING operand in broken code)
            // cannot be negated; fall through to the `not …` wrap instead.
            if let (Some(left), Some(right)) = (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ) {
                return format!("{} {} {}", left.text(), negated_operator, right.text());
            }
        }
    }

    let condition_text = node.text();
    if not_condition_needs_parentheses(node) {
        format!("not ({condition_text})")
    } else {
        format!("not {condition_text}")
    }
}

pub(crate) fn refactor_if_null_branch(if_node: M2Node<'_>) -> Option<String> {
    let condition = if_node.child_by_field_name("condition")?;
    let then_branch = expression_of_clause(clause_child(if_node, NodeKind::ThenClause)?)?;
    let else_branch = expression_of_clause(clause_child(if_node, NodeKind::ElseClause)?)?;

    if is_null_literal(else_branch) {
        return Some(format!(
            "if {} then {}",
            condition.text(),
            then_branch.text(),
        ));
    }

    if is_null_literal(then_branch) && !is_null_literal(else_branch) {
        return Some(format!(
            "if {} then {}",
            negated_condition_text(condition),
            else_branch.text(),
        ));
    }

    None
}

fn try_condition(try_node: M2Node<'_>) -> Option<M2Node<'_>> {
    try_node.named_children().find(|child| {
        !matches!(
            child.kind,
            NodeKind::ThenClause
                | NodeKind::ElseClause
                | NodeKind::ExceptClause
                | NodeKind::DoClause
        )
    })
}

pub(crate) fn refactor_try_statement(try_node: M2Node<'_>) -> Option<String> {
    let condition = try_condition(try_node)?;
    let consequence = clause_child(try_node, NodeKind::ThenClause).and_then(expression_of_clause);
    let else_clause = clause_child(try_node, NodeKind::ElseClause);

    let condition_text = condition.text();
    let consequence_text = consequence.map(|node| node.text());

    if let Some(consequence_text) = consequence_text {
        if consequence_text == condition_text && else_clause.is_none() {
            return Some(format!("try {condition_text}"));
        }
    }

    if let Some(else_clause) = else_clause {
        let alternative = expression_of_clause(else_clause)?;
        if is_null_literal(alternative) {
            let mut simplified = format!("try {condition_text}");
            if let Some(consequence_text) = consequence_text {
                simplified.push_str(" then ");
                simplified.push_str(consequence_text);
            }
            return Some(simplified);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentSnapshot;
    use crate::typesystem::BuiltinData;

    fn document(text: &str) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), &BuiltinData::empty())
            .expect("fixture should parse")
    }

    fn cursor_at(document: &DocumentSnapshot, position: Position) -> M2Node<'_> {
        document
            .node_at_position_minimal(position)
            .expect("cursor position should resolve to a node")
    }

    #[test]
    fn conditional_null_else_refactor_drops_else_branch() {
        let text = "if ready then value else null";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("conditional null refactor should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(
            change.range,
            Range::new(Position::new(0, 0), Position::new(0, 29))
        );
        assert_eq!(change.new_text, "if ready then value");
    }

    #[test]
    fn conditional_null_refactor_drops_else_when_both_branches_null() {
        // Generated placeholders like `... then null else null` should still
        // offer to drop the redundant `else null`.
        let text = "if member(\"Flexible\", attrStrings) then null else null";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("conditional null refactor should be available for both-null branches");
        let change = &action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes")[&uri][0];

        assert_eq!(
            change.new_text,
            "if member(\"Flexible\", attrStrings) then null"
        );
    }

    #[test]
    fn ambiguous_float_member_access_quickfix_rewrites_to_hash_member_access() {
        let text = "x.3\n";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);
        let diagnostic = M2Diagnostic::AmbiguousFloatMemberAccess.at(
            Range::new(Position::new(0, 0), Position::new(0, 3)),
            "ambiguous float member access",
        );

        let action = ambiguous_float_member_access_code_action(
            &document,
            &uri,
            Position::new(0, 1),
            cursor_at(&document, Position::new(0, 1)),
            std::slice::from_ref(&diagnostic),
        )
        .expect("ambiguous member access quickfix should be available");
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(action.diagnostics, Some(vec![diagnostic]));

        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(
            change.range,
            Range::new(Position::new(0, 0), Position::new(0, 3))
        );
        assert_eq!(change.new_text, "x#3");
    }

    #[test]
    fn convert_to_raw_string_rewrites_heavily_escaped_strings() {
        let text = "x := \"a\\nb\\tc\\\"\"\n";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = convert_to_raw_string_code_action(
            &document,
            &uri,
            Position::new(0, 7),
            cursor_at(&document, Position::new(0, 7)),
            &[],
        )
        .expect("raw string conversion should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "///a\nb\tc\"///");
    }

    #[test]
    fn convert_to_raw_string_requires_more_than_two_escapes() {
        let text = "x := \"a\\nb\"\n";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        assert!(convert_to_raw_string_code_action(
            &document,
            &uri,
            Position::new(0, 7),
            cursor_at(&document, Position::new(0, 7)),
            &[]
        )
        .is_none());
    }

    #[test]
    fn convert_to_raw_string_rejects_content_with_raw_delimiter() {
        let text = "x := \"a\\/\\/\\/b\"\n";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        assert!(convert_to_raw_string_code_action(
            &document,
            &uri,
            Position::new(0, 7),
            cursor_at(&document, Position::new(0, 7)),
            &[]
        )
        .is_none());
    }

    #[test]
    fn convert_to_raw_string_rejects_unsupported_escapes() {
        // Octal escapes (and hex / \a \b \f \v) are not faithfully reproducible
        // verbatim in a raw string. The action must NOT be offered rather than
        // silently dropping the backslash and corrupting the value: M2 source
        // `"\101\102\103"` is the string "ABC", not "101102103".
        let text = "x := \"\\101\\102\\103\"\n";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        assert!(
            convert_to_raw_string_code_action(
                &document,
                &uri,
                Position::new(0, 7),
                cursor_at(&document, Position::new(0, 7)),
                &[]
            )
            .is_none(),
            "raw-string conversion must not be offered for unsupported escapes"
        );
    }

    #[test]
    fn conditional_null_then_refactor_negates_simple_condition() {
        let text = "if ready then null else value";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("conditional null refactor should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if not ready then value");
    }

    #[test]
    fn conditional_null_then_refactor_parenthesizes_binary_condition() {
        let text = "if a < b then null else value";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("conditional null refactor should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if a >= b then value");
    }

    #[test]
    fn conditional_null_then_refactor_inverts_equality_condition() {
        let text = "if a == b then null else value";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("conditional null refactor should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if a != b then value");
    }

    #[test]
    fn conditional_null_then_refactor_inverts_strict_equality_condition() {
        let text = "if a === b then null else value";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("conditional null refactor should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if a =!= b then value");
    }

    #[test]
    fn conditional_null_then_refactor_cancels_not_condition() {
        let text = "if not ready then null else value";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("conditional null refactor should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if ready then value");
    }

    #[test]
    fn simplify_try_drops_redundant_then_branch() {
        let text = "try value then value";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_try_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("try simplification should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "try value");
    }

    #[test]
    fn simplify_try_drops_else_null_branch() {
        let text = "try value then result else null";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_try_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("try simplification should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "try value then result");
    }

    #[test]
    fn simplify_try_drops_else_null_without_then_branch() {
        let text = "try value else null";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_try_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("try simplification should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "try value");
    }

    #[test]
    fn simplify_try_does_not_touch_except_null_branch() {
        let text = "try value except err do null";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        assert!(
            simplify_try_code_action(
                &document,
                &uri,
                Position::new(0, 4),
                cursor_at(&document, Position::new(0, 4)),
                &[]
            )
            .is_none(),
            "except branches should not be simplified by the else-null rewrite"
        );
    }

    #[test]
    fn simplify_if_condition_negates_equality() {
        let text = "if not (a == b) then x";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_if_condition_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("simplify if condition should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if a != b then x");
    }

    #[test]
    fn simplify_if_condition_negates_inequality() {
        let text = "if not (a != b) then x else y";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_if_condition_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("simplify if condition should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if a == b then x else y");
    }

    #[test]
    fn simplify_if_condition_negates_less_than() {
        let text = "if not (a < b) then x";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_if_condition_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("simplify if condition should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if a >= b then x");
    }

    #[test]
    fn simplify_if_condition_cancels_double_not() {
        let text = "if not not x then y";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_if_condition_code_action(
            &document,
            &uri,
            Position::new(0, 4),
            cursor_at(&document, Position::new(0, 4)),
            &[],
        )
        .expect("simplify if condition should be available");
        let edit = action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes");
        let change = &edit[&uri][0];

        assert_eq!(change.new_text, "if x then y");
    }

    #[test]
    fn simplify_if_condition_not_available_for_simple_condition() {
        let text = "if x then y";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        assert!(
            simplify_if_condition_code_action(
                &document,
                &uri,
                Position::new(0, 4),
                cursor_at(&document, Position::new(0, 4)),
                &[]
            )
            .is_none(),
            "simple conditions should not offer simplification"
        );
    }
}
