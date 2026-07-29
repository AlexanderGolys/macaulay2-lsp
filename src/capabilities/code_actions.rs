//! Diagnostic quick fixes and syntax-preserving refactors offered by the LSP.

use std::collections::HashMap;

use tower_lsp::lsp_types::*;

use crate::capabilities::diagnostics::ambiguous_float_member_access_rewrite;
use crate::diagnostic_registry::{diagnostic_has_kind, M2Diagnostic};
use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind};
use crate::source::SourceNavigation;
use crate::util::position_in_range;

struct CodeActionContext<'tree, 'request> {
    document: &'tree DocumentSnapshot,
    uri: &'request Url,
    position: Position,
    cursor: M2Node<'tree>,
    diagnostics: &'request [Diagnostic],
}

trait CodeActionRule: Sync {
    fn action(&self, context: &CodeActionContext<'_, '_>) -> Option<CodeAction>;
}

struct AmbiguousFloatMemberAccess;
struct ConvertToRawString;
struct ConditionalNull;
struct SimplifyTry;
struct SimplifyIfCondition;
struct FlattenElseIf;

const ACTION_RULES: &[&dyn CodeActionRule] = &[
    &AmbiguousFloatMemberAccess,
    &ConvertToRawString,
    &ConditionalNull,
    &SimplifyTry,
    &SimplifyIfCondition,
    &FlattenElseIf,
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
    let context = CodeActionContext {
        document,
        uri,
        position,
        cursor,
        diagnostics,
    };
    let actions = actions_from_rules(ACTION_RULES, &context);
    (!actions.is_empty()).then_some(actions)
}

fn actions_from_rules(
    rules: &[&dyn CodeActionRule],
    context: &CodeActionContext<'_, '_>,
) -> CodeActionResponse {
    rules
        .iter()
        .filter_map(|rule| rule.action(context))
        .map(CodeActionOrCommand::CodeAction)
        .collect()
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
impl CodeActionRule for ConvertToRawString {
    fn action(&self, context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
        let string_node = context
            .document
            .enclosing_node_of_kind(context.cursor, NodeKind::StringLiteral)?;
        let replacement = raw_string_replacement(string_node)?;

        Some(
            CodeActionSpec {
                title: "Convert to raw string",
                kind: CodeActionKind::REFACTOR_REWRITE,
                is_preferred: None,
                diagnostics: None,
            }
            .build(
                context.uri,
                context.document.range_for_node(string_node),
                replacement,
            ),
        )
    }
}

/// Quickfix for the ambiguous-float diagnostic (`x.3` parses as `x SPACE .3`):
/// rewrite to the member access the user almost certainly meant (`x#3`).
impl CodeActionRule for AmbiguousFloatMemberAccess {
    fn action(&self, context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
        let diagnostic = context
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic_has_kind(diagnostic, M2Diagnostic::AmbiguousFloatMemberAccess)
                    && position_in_range(context.position, diagnostic.range)
            })?
            .clone();
        let expression = context
            .document
            .enclosing_node_of_kind(context.cursor, NodeKind::BinaryExpression)?;
        let replacement = ambiguous_float_member_access_rewrite(expression)?;

        Some(
            CodeActionSpec {
                title: "Rewrite as member access",
                kind: CodeActionKind::QUICKFIX,
                is_preferred: Some(true),
                diagnostics: Some(vec![diagnostic]),
            }
            .build(
                context.uri,
                context.document.range_for_node(expression),
                replacement,
            ),
        )
    }
}

/// Refactor: drop a redundant `else null` (or `then null`, negating the
/// condition) from an `if` statement.
impl CodeActionRule for ConditionalNull {
    fn action(&self, context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
        let if_node = context
            .document
            .enclosing_node_of_kind(context.cursor, NodeKind::IfStatement)?;
        let replacement = refactor_if_null_branch(if_node)?;

        Some(
            CodeActionSpec {
                title: "Simplify unnecessary null branch",
                kind: CodeActionKind::REFACTOR_REWRITE,
                is_preferred: None,
                diagnostics: None,
            }
            .build(
                context.uri,
                context.document.range_for_node(if_node),
                replacement,
            ),
        )
    }
}

/// Refactor: simplify a `try` statement — drop a redundant `then` echo or a
/// redundant `else null`.
impl CodeActionRule for SimplifyTry {
    fn action(&self, context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
        let try_node = context
            .document
            .enclosing_node_of_kind(context.cursor, NodeKind::TryStatement)?;
        let replacement = refactor_try_statement(try_node)?;

        Some(
            CodeActionSpec {
                title: "Simplify try",
                kind: CodeActionKind::REFACTOR_REWRITE,
                is_preferred: None,
                diagnostics: None,
            }
            .build(
                context.uri,
                context.document.range_for_node(try_node),
                replacement,
            ),
        )
    }
}

/// Refactor: push a leading `not` through a parenthesized comparison
/// (`if not (a == b) then x` → `if a != b then x`).
impl CodeActionRule for SimplifyIfCondition {
    fn action(&self, context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
        let if_node = context
            .document
            .enclosing_node_of_kind(context.cursor, NodeKind::IfStatement)?;
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
            .build(
                context.uri,
                context.document.range_for_node(if_node),
                replacement,
            ),
        )
    }
}

impl CodeActionRule for FlattenElseIf {
    fn action(&self, context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
        let mut current = Some(context.cursor);
        let mut candidate = None;
        while let Some(node) = current {
            if node.kind == NodeKind::IfStatement {
                if let Some(replacement) = flatten_else_if_chain(node) {
                    candidate = Some((node, replacement));
                }
            }
            current = node.parent();
        }
        let (node, replacement) = candidate?;
        Some(
            CodeActionSpec {
                title: "Flatten nested if into else-if chain",
                kind: CodeActionKind::REFACTOR_REWRITE,
                is_preferred: None,
                diagnostics: None,
            }
            .build(
                context.uri,
                context.document.range_for_node(node),
                replacement,
            ),
        )
    }
}

fn flatten_else_if_chain(if_node: M2Node<'_>) -> Option<String> {
    flatten_then_if_chain(if_node).or_else(|| flatten_parenthesized_else_if_chain(if_node))
}

fn flatten_then_if_chain(if_node: M2Node<'_>) -> Option<String> {
    let condition = if_node.child_by_field_name("condition")?;
    let then_clause = clause_child(if_node, NodeKind::ThenClause)?;
    let then_branch = expression_of_clause(then_clause)?;
    let nested_if = unwrap_parentheses(then_branch);
    if nested_if.kind != NodeKind::IfStatement {
        return None;
    }
    let else_branch = expression_of_clause(clause_child(if_node, NodeKind::ElseClause)?)?;
    let nested_replacement =
        flatten_else_if_chain(nested_if).unwrap_or_else(|| nested_if.text().to_string());

    Some(format!(
        "if {} then {} else {}",
        negated_condition_text(condition),
        else_branch.text(),
        nested_replacement
    ))
}

fn flatten_parenthesized_else_if_chain(if_node: M2Node<'_>) -> Option<String> {
    let else_clause = clause_child(if_node, NodeKind::ElseClause)?;
    let else_branch = expression_of_clause(else_clause)?;
    let nested_if = unwrap_parentheses(else_branch);
    if nested_if.kind != NodeKind::IfStatement {
        return None;
    }

    let nested_replacement = flatten_else_if_chain(nested_if);
    let removes_parentheses = nested_if.id() != else_branch.id();
    if !removes_parentheses && nested_replacement.is_none() {
        return None;
    }

    let replacement = nested_replacement.unwrap_or_else(|| nested_if.text().to_string());
    let start = else_branch.start_byte() - if_node.start_byte();
    let end = else_branch.end_byte() - if_node.start_byte();
    let mut flattened = if_node.text().to_string();
    flattened.replace_range(start..end, &replacement);
    Some(flattened)
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
    use crate::object_registry::ObjectRegistry;

    fn document(text: &str) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), &ObjectRegistry::default())
            .expect("fixture should parse")
    }

    fn cursor_at(document: &DocumentSnapshot, position: Position) -> M2Node<'_> {
        document
            .node_at_position_minimal(position)
            .expect("cursor position should resolve to a node")
    }

    fn action_for<'tree>(
        rule: &dyn CodeActionRule,
        document: &'tree DocumentSnapshot,
        uri: &Url,
        position: Position,
        cursor: M2Node<'tree>,
        diagnostics: &[Diagnostic],
    ) -> Option<CodeAction> {
        rule.action(&CodeActionContext {
            document,
            uri,
            position,
            cursor,
            diagnostics,
        })
    }

    fn convert_to_raw_string_code_action<'tree>(
        document: &'tree DocumentSnapshot,
        uri: &Url,
        position: Position,
        cursor: M2Node<'tree>,
        diagnostics: &[Diagnostic],
    ) -> Option<CodeAction> {
        action_for(
            &ConvertToRawString,
            document,
            uri,
            position,
            cursor,
            diagnostics,
        )
    }

    fn ambiguous_float_member_access_code_action<'tree>(
        document: &'tree DocumentSnapshot,
        uri: &Url,
        position: Position,
        cursor: M2Node<'tree>,
        diagnostics: &[Diagnostic],
    ) -> Option<CodeAction> {
        action_for(
            &AmbiguousFloatMemberAccess,
            document,
            uri,
            position,
            cursor,
            diagnostics,
        )
    }

    fn conditional_null_code_action<'tree>(
        document: &'tree DocumentSnapshot,
        uri: &Url,
        position: Position,
        cursor: M2Node<'tree>,
        diagnostics: &[Diagnostic],
    ) -> Option<CodeAction> {
        action_for(
            &ConditionalNull,
            document,
            uri,
            position,
            cursor,
            diagnostics,
        )
    }

    fn simplify_try_code_action<'tree>(
        document: &'tree DocumentSnapshot,
        uri: &Url,
        position: Position,
        cursor: M2Node<'tree>,
        diagnostics: &[Diagnostic],
    ) -> Option<CodeAction> {
        action_for(&SimplifyTry, document, uri, position, cursor, diagnostics)
    }

    fn simplify_if_condition_code_action<'tree>(
        document: &'tree DocumentSnapshot,
        uri: &Url,
        position: Position,
        cursor: M2Node<'tree>,
        diagnostics: &[Diagnostic],
    ) -> Option<CodeAction> {
        action_for(
            &SimplifyIfCondition,
            document,
            uri,
            position,
            cursor,
            diagnostics,
        )
    }

    fn flatten_else_if_code_action<'tree>(
        document: &'tree DocumentSnapshot,
        uri: &Url,
        position: Position,
        cursor: M2Node<'tree>,
        diagnostics: &[Diagnostic],
    ) -> Option<CodeAction> {
        action_for(&FlattenElseIf, document, uri, position, cursor, diagnostics)
    }

    #[test]
    fn action_dispatch_accepts_stateful_rules_and_preserves_order() {
        struct NamedAction(&'static str);

        impl CodeActionRule for NamedAction {
            fn action(&self, _context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
                Some(CodeAction {
                    title: self.0.to_string(),
                    ..Default::default()
                })
            }
        }

        let document = document("x");
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let position = Position::new(0, 0);
        let context = CodeActionContext {
            document: &document,
            uri: &uri,
            position,
            cursor: cursor_at(&document, position),
            diagnostics: &[],
        };
        let first = NamedAction("first");
        let second = NamedAction("second");
        let rules: &[&dyn CodeActionRule] = &[&first, &second];
        let actions = actions_from_rules(rules, &context);
        let titles: Vec<_> = actions
            .into_iter()
            .map(|action| match action {
                CodeActionOrCommand::CodeAction(action) => action.title,
                CodeActionOrCommand::Command(_) => unreachable!("rules only emit code actions"),
            })
            .collect();

        assert_eq!(titles, ["first", "second"]);
    }

    #[test]
    fn flattens_parenthesized_nested_ifs_into_one_else_if_chain() {
        let text = "if a then one else (if b then two else (if c then three else four))";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);
        let position = Position::new(0, text.find('c').unwrap() as u32);

        let action = flatten_else_if_code_action(
            &document,
            &uri,
            position,
            cursor_at(&document, position),
            &[],
        )
        .expect("nested else-if chain should be flattenable");
        let change = &action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes")[&uri][0];

        assert_eq!(
            change.new_text,
            "if a then one else if b then two else if c then three else four"
        );
    }

    #[test]
    fn flattens_then_nested_if_by_negating_the_outer_condition_once() {
        let text = "if xywzx then (if xxx then yyyxyz else xuuu) else xuu";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);
        let position = Position::new(0, text.find("xxx").unwrap() as u32);

        let action = flatten_else_if_code_action(
            &document,
            &uri,
            position,
            cursor_at(&document, position),
            &[],
        )
        .expect("a nested then-if should be flattenable");
        let change = &action
            .edit
            .expect("code action should carry an edit")
            .changes
            .expect("edit should use simple changes")[&uri][0];

        assert_eq!(
            change.new_text,
            "if not xywzx then xuu else if xxx then yyyxyz else xuuu"
        );
    }

    #[test]
    fn does_not_offer_else_if_flattening_for_an_existing_chain() {
        let text = "if a then one else if b then two else three";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);
        let position = Position::new(0, 1);

        assert!(flatten_else_if_code_action(
            &document,
            &uri,
            position,
            cursor_at(&document, position),
            &[],
        )
        .is_none());
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
