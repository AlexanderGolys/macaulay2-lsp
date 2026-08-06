//! Diagnostic quick fixes and syntax-preserving refactors offered by the LSP.

use std::collections::HashMap;

use tower_lsp::lsp_types::Range as TextRange;
use tower_lsp::lsp_types::*;

use crate::analysis::{
    ambiguous_float_member_access_rewrite, coalescence_rewrite,
    redundant_control_parentheses_inner, MethodCodomainEdit,
};
use crate::diagnostic_declarations;
use crate::diagnostic_registry::{diagnostic_has_kind, DiagnosticKind};
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

macro_rules! push_declared_action {
    (none, $context:ident, $actions:ident) => {};
    ((|$action_context:ident| $action:expr), $context:ident, $actions:ident) => {
        let $action_context = $context;
        if let Some(action) = $action {
            $actions.push(CodeActionOrCommand::CodeAction(action));
        }
    };
}

macro_rules! declared_code_actions {
    (diagnostics { $($kind:ident {
        code: $code:literal, name: $name:literal, severity: $severity:ident,
        legacy: [$($legacy:literal),* $(,)?],
        check: $phase:ident => |$check_context:ident| $check:expr, action: $action:tt,
    }),+ $(,)? } standalone_actions {
        $($standalone:ident => |$action_context:ident| $standalone_action:expr),* $(,)?
    }) => {
        |context: &CodeActionContext<'_, '_>| {
            let mut actions = CodeActionResponse::new();
            $(push_declared_action!($action, context, actions);)+
            $(
                let $action_context = context;
                if let Some(action) = $standalone_action {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }
            )*
            actions
        }
    };
}

fn diagnostic_at(context: &CodeActionContext<'_, '_>, kind: DiagnosticKind) -> Option<Diagnostic> {
    context
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic_has_kind(diagnostic, kind)
                && position_in_range(context.position, diagnostic.range)
        })
        .cloned()
}

fn method_codomain_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    let diagnostic = diagnostic_at(context, DiagnosticKind::InstallCodomainMissing)?;
    let mut current = Some(context.cursor);
    while let Some(node) = current {
        if node.is_assignment() {
            let knowledge = context
                .document
                .object_registry()
                .at(context.document.position_for_node(node));
            if let Some(deduction) = context.document.analysis().method_codomain_deduction(
                node,
                context.document,
                &knowledge,
            ) {
                if deduction.diagnostic_range == diagnostic.range {
                    let MethodCodomainEdit::Add(edit_range) = deduction.edit else {
                        return None;
                    };
                    return Some(
                        CodeActionSpec {
                            title: "Add codomain annotation",
                            kind: CodeActionKind::QUICKFIX,
                            is_preferred: Some(true),
                            diagnostics: Some(vec![diagnostic]),
                        }
                        .build(
                            context.uri,
                            edit_range,
                            format!("{} => ", deduction.codomain),
                        ),
                    );
                }
            }
        }
        current = node.parent();
    }
    None
}

fn colon_equal_part_assignment_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    assignment_operator_action(
        context,
        DiagnosticKind::ColonEqualPartAssignment,
        ":=",
        "=",
        "Use `=` for part assignment",
    )
}

fn install_needs_colon_equals_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    assignment_operator_action(
        context,
        DiagnosticKind::InstallNeedsColonEquals,
        "=",
        ":=",
        "Use `:=` for method installation",
    )
}

fn assignment_operator_action(
    context: &CodeActionContext<'_, '_>,
    kind: DiagnosticKind,
    current_operator: &str,
    replacement: &str,
    title: &'static str,
) -> Option<CodeAction> {
    let diagnostic = diagnostic_at(context, kind)?;
    let mut current = Some(context.cursor);
    while let Some(node) = current {
        if node.binary_operator() == Some(current_operator) {
            let operator = node.child_by_field_name("operator")?;
            return Some(
                CodeActionSpec {
                    title,
                    kind: CodeActionKind::QUICKFIX,
                    is_preferred: Some(true),
                    diagnostics: Some(vec![diagnostic]),
                }
                .build(
                    context.uri,
                    context.document.range_for_node(operator),
                    replacement.to_string(),
                ),
            );
        }
        current = node.parent();
    }
    None
}

fn option_key_convention_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    let diagnostic = diagnostic_at(context, DiagnosticKind::OptionKeyConvention)?;
    let key = enclosing_node_with_range(context, NodeKind::Symbol, diagnostic.range)?;
    let mut replacement = key.text().to_string();
    replacement.get_mut(..1)?.make_ascii_uppercase();
    Some(
        CodeActionSpec {
            title: "Capitalize option key",
            kind: CodeActionKind::QUICKFIX,
            is_preferred: Some(true),
            diagnostics: Some(vec![diagnostic]),
        }
        .build(
            context.uri,
            context.document.range_for_node(key),
            replacement,
        ),
    )
}

fn redundant_control_parentheses_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    let diagnostic = diagnostic_at(context, DiagnosticKind::RedundantControlParentheses)?;
    let parentheses =
        enclosing_node_with_range(context, NodeKind::ParenthesizedExpression, diagnostic.range)?;
    let inner = redundant_control_parentheses_inner(parentheses)?;
    let range = diagnostic.range;
    Some(
        CodeActionSpec {
            title: "Remove redundant parentheses",
            kind: CodeActionKind::QUICKFIX,
            is_preferred: Some(true),
            diagnostics: Some(vec![diagnostic]),
        }
        .build(context.uri, range, inner.text().to_string()),
    )
}

fn coalescence_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    let diagnostic = diagnostic_at(context, DiagnosticKind::PreferCoalescence)?;
    let mut current = Some(context.cursor);
    while let Some(node) = current {
        if context.document.range_for_node(node) == diagnostic.range {
            let replacement = coalescence_rewrite(node)?;
            let range = diagnostic.range;
            let title = if node.is_assignment() {
                "Use `??=` coalescing assignment"
            } else {
                "Use `??` coalescence"
            };
            return Some(
                CodeActionSpec {
                    title,
                    kind: CodeActionKind::QUICKFIX,
                    is_preferred: Some(true),
                    diagnostics: Some(vec![diagnostic]),
                }
                .build(context.uri, range, replacement),
            );
        }
        current = node.parent();
    }
    None
}

fn protect_assigned_symbol_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    let diagnostic = diagnostic_at(context, DiagnosticKind::ProtectAssignedSymbol)?;
    let symbol = enclosing_node_with_range(context, NodeKind::Symbol, diagnostic.range)?;
    let start = context.document.position_for_node(symbol);
    Some(
        CodeActionSpec {
            title: "Protect the symbol itself",
            kind: CodeActionKind::QUICKFIX,
            is_preferred: Some(true),
            diagnostics: Some(vec![diagnostic]),
        }
        .build(
            context.uri,
            TextRange::new(start, start),
            "symbol ".to_string(),
        ),
    )
}

fn enclosing_node_with_range<'tree>(
    context: &CodeActionContext<'tree, '_>,
    kind: NodeKind,
    range: TextRange,
) -> Option<M2Node<'tree>> {
    if kind == NodeKind::Symbol {
        if let Some(symbol) = context.document.symbol_node_at_position(range.start) {
            if context.document.range_for_node(symbol) == range {
                return Some(symbol);
            }
        }
    }
    let mut current = Some(context.cursor);
    while let Some(node) = current {
        if node.kind == kind && context.document.range_for_node(node) == range {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

/// The code actions offered at `position`: every action from the registry
/// whose producer returns `Some`. The deepest CST node covering `position` is
/// resolved a single time here and threaded through the registry, so the
/// tree-sitter descent happens once per request instead of once per producer.
pub fn available_code_actions(
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
    let actions = diagnostic_declarations!(declared_code_actions)(&context);
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
    fn build(self, uri: &Url, range: TextRange, new_text: String) -> CodeAction {
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
fn convert_to_raw_string_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
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

/// Quickfix for the ambiguous-float diagnostic (`x.3` parses as `x SPACE .3`):
/// rewrite to the member access the user almost certainly meant (`x#3`).
fn ambiguous_float_member_access_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
    let diagnostic = diagnostic_at(context, DiagnosticKind::AmbiguousFloatMemberAccess)?;
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

/// Refactor: drop a redundant `else null` (or `then null`, negating the
/// condition) from an `if` statement.
fn conditional_null_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
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

/// Refactor: simplify a `try` statement — drop a redundant `then` echo or a
/// redundant `else null`.
fn simplify_try_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
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

/// Refactor: push a leading `not` through a parenthesized comparison
/// (`if not (a == b) then x` → `if a != b then x`).
fn simplify_if_condition_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
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

fn flatten_else_if_action(context: &CodeActionContext<'_, '_>) -> Option<CodeAction> {
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

pub fn refactor_if_null_branch(if_node: M2Node<'_>) -> Option<String> {
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

pub fn refactor_try_statement(try_node: M2Node<'_>) -> Option<String> {
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
