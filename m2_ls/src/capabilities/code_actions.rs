use std::collections::HashMap;

use tower_lsp::lsp_types::*;

use crate::document::DocumentSnapshot;
use crate::util::*;

pub(crate) fn available_code_actions(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
) -> Option<CodeActionResponse> {
    let mut actions = Vec::new();

    if let Some(action) = conditional_null_code_action(document, uri, position) {
        actions.push(CodeActionOrCommand::CodeAction(action));
    }
    if let Some(action) = simplify_try_code_action(document, uri, position) {
        actions.push(CodeActionOrCommand::CodeAction(action));
    }
    if let Some(action) = simplify_if_condition_code_action(document, uri, position) {
        actions.push(CodeActionOrCommand::CodeAction(action));
    }

    (!actions.is_empty()).then_some(actions)
}

pub(crate) fn conditional_null_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
) -> Option<CodeAction> {
    let text = document.text();
    let if_node = if_statement_at_position(document, position)?;
    let replacement = refactor_if_null_branch(if_node, text)?;

    Some(CodeAction {
        title: "Simplify unnecessary null branch".to_string(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri.clone(),
                vec![TextEdit {
                    range: document.range_for(if_node),
                    new_text: replacement,
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        }),
        command: None,
        is_preferred: Some(true),
        disabled: None,
        data: None,
    })
}

pub(crate) fn simplify_try_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
) -> Option<CodeAction> {
    let text = document.text();
    let try_node = try_statement_at_position(document, position)?;
    let replacement = refactor_try_statement(try_node, text)?;

    Some(CodeAction {
        title: "Simplify try".to_string(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri.clone(),
                vec![TextEdit {
                    range: document.range_for(try_node),
                    new_text: replacement,
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        }),
        command: None,
        is_preferred: Some(true),
        disabled: None,
        data: None,
    })
}

pub(crate) fn simplify_if_condition_code_action(
    document: &DocumentSnapshot,
    uri: &Url,
    position: Position,
) -> Option<CodeAction> {
    let text = document.text();
    let if_node = if_statement_at_position(document, position)?;
    let condition = if_node.child_by_field_name("condition")?;
    let simplified = simplify_condition(condition, text)?;

    let then_branch = expression_of_clause(clause_child(if_node, "then_clause")?)?;
    let else_clause = clause_child(if_node, "else_clause");

    let mut replacement = format!(
        "if {} then {}",
        simplified,
        &text[then_branch.start_byte()..then_branch.end_byte()]
    );
    if let Some(else_clause) = else_clause {
        replacement.push(' ');
        replacement.push_str(&text[else_clause.start_byte()..else_clause.end_byte()]);
    }

    Some(CodeAction {
        title: "Simplify if condition".to_string(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri.clone(),
                vec![TextEdit {
                    range: document.range_for(if_node),
                    new_text: replacement,
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        }),
        command: None,
        is_preferred: Some(true),
        disabled: None,
        data: None,
    })
}

fn simplify_condition(node: tree_sitter::Node, text: &str) -> Option<String> {
    let original = &text[node.start_byte()..node.end_byte()];

    if node.kind() == "prefix_expression" {
        if let Some(operator) = node.child_by_field_name("operator") {
            if &text[operator.start_byte()..operator.end_byte()] == "not" {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.id() != operator.id() {
                        let inner = unwrap_parentheses(child, text);
                        let simplified = negated_condition_text(inner, text);
                        if simplified != original {
                            return Some(simplified);
                        }
                    }
                }
            }
        }
    }

    if let Some((left_not, negated_op, right)) = not_dominated_binary(node, text) {
        let left_operand = expression_operand_of_not(left_not, text);
        let simplified = format!(
            "{} {} {}",
            &text[left_operand.start_byte()..left_operand.end_byte()],
            negated_op,
            &text[right.start_byte()..right.end_byte()],
        );
        if simplified != original {
            return Some(simplified);
        }
    }

    None
}

fn not_dominated_binary<'tree>(
    node: tree_sitter::Node<'tree>,
    text: &str,
) -> Option<(
    tree_sitter::Node<'tree>,
    &'static str,
    tree_sitter::Node<'tree>,
)> {
    let operator = binary_expression_operator(node, text)?;
    let negated_operator = negated_binary_operator(operator)?;
    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;

    if is_prefix_not(left, text) {
        return Some((left, negated_operator, right));
    }
    if is_prefix_not(right, text) {
        return Some((right, negated_operator, left));
    }
    None
}

fn is_prefix_not(node: tree_sitter::Node, text: &str) -> bool {
    node.kind() == "prefix_expression"
        && node
            .child_by_field_name("operator")
            .is_some_and(|op| &text[op.start_byte()..op.end_byte()] == "not")
}

fn expression_operand_of_not<'tree>(
    not_node: tree_sitter::Node<'tree>,
    text: &str,
) -> tree_sitter::Node<'tree> {
    let mut cursor = not_node.walk();
    let operator = not_node
        .child_by_field_name("operator")
        .expect("prefix expression should have operator");
    for child in not_node.named_children(&mut cursor) {
        if child.id() != operator.id() {
            return unwrap_parentheses(child, text);
        }
    }
    unreachable!("not prefix expression should have an operand")
}

fn unwrap_parentheses<'a>(node: tree_sitter::Node<'a>, text: &str) -> tree_sitter::Node<'a> {
    if node.kind() == "parenthesized_expression" || node.child_count() == 3 {
        let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            if children.len() == 3 {
                if let (Some(first), Some(middle), Some(last)) =
                    (children.first(), children.get(1), children.get(2))
                {
                    let first_text = &text[first.start_byte()..first.end_byte()];
                    let last_text = &text[last.start_byte()..last.end_byte()];
                if first_text == "(" && last_text == ")" {
                    return *middle;
                }
            }
        }
    }
    node
}

fn clause_child<'tree>(
    parent: tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = parent.walk();
    let mut children = parent.children(&mut cursor);
    children.find(|child| child.kind() == kind)
}

fn expression_of_clause<'tree>(
    clause: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = clause.walk();
    let mut children = clause.named_children(&mut cursor);
    children.next()
}

fn if_statement_at_position<'tree>(
    document: &'tree DocumentSnapshot,
    position: Position,
) -> Option<tree_sitter::Node<'tree>> {
    let node = document.node_at_position_minimal(position)?;
    document.enclosing_node_of_kind(node, "if_statement")
}

fn try_statement_at_position<'tree>(
    document: &'tree DocumentSnapshot,
    position: Position,
) -> Option<tree_sitter::Node<'tree>> {
    let node = document.node_at_position_minimal(position)?;
    document.enclosing_node_of_kind(node, "try_statement")
}

fn is_null_literal(node: tree_sitter::Node, text: &str) -> bool {
    matches!(node.kind(), "symbol") && &text[node.start_byte()..node.end_byte()] == "null"
}

fn not_condition_needs_parentheses(node: tree_sitter::Node) -> bool {
    matches!(node.kind(), "binary_expression")
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

fn negated_condition_text(node: tree_sitter::Node, text: &str) -> String {
    if node.kind() == "prefix_expression" {
        if let Some(operator) = node.child_by_field_name("operator") {
            if &text[operator.start_byte()..operator.end_byte()] == "not" {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.id() != operator.id() {
                        return text[child.start_byte()..child.end_byte()].to_string();
                    }
                }
            }
        }
    }

    if let Some(operator) = binary_expression_operator(node, text) {
        if let Some(negated_operator) = negated_binary_operator(operator) {
            let left = node
                .child_by_field_name("left")
                .expect("binary expressions should have a left operand");
            let right = node
                .child_by_field_name("right")
                .expect("binary expressions should have a right operand");
            return format!(
                "{} {} {}",
                &text[left.start_byte()..left.end_byte()],
                negated_operator,
                &text[right.start_byte()..right.end_byte()],
            );
        }
    }

    let condition_text = &text[node.start_byte()..node.end_byte()];
    if not_condition_needs_parentheses(node) {
        format!("not ({condition_text})")
    } else {
        format!("not {condition_text}")
    }
}

fn refactor_if_null_branch(if_node: tree_sitter::Node, text: &str) -> Option<String> {
    let condition = if_node.child_by_field_name("condition")?;
    let then_branch = expression_of_clause(clause_child(if_node, "then_clause")?)?;
    let else_branch = expression_of_clause(clause_child(if_node, "else_clause")?)?;

    if is_null_literal(else_branch, text) && !is_null_literal(then_branch, text) {
        return Some(format!(
            "if {} then {}",
            &text[condition.start_byte()..condition.end_byte()],
            &text[then_branch.start_byte()..then_branch.end_byte()],
        ));
    }

    if is_null_literal(then_branch, text) && !is_null_literal(else_branch, text) {
        return Some(format!(
            "if {} then {}",
            negated_condition_text(condition, text),
            &text[else_branch.start_byte()..else_branch.end_byte()],
        ));
    }

    None
}

fn try_alternative_is_else(try_node: tree_sitter::Node, _text: &str) -> bool {
    clause_child(try_node, "else_clause").is_some()
}

fn try_condition<'tree>(try_node: tree_sitter::Node<'tree>) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = try_node.walk();
    let mut children = try_node.named_children(&mut cursor);
    children.find(|child| {
        !matches!(
            child.kind(),
            "then_clause" | "else_clause" | "except_clause" | "do_clause"
        )
    })
}

fn refactor_try_statement(try_node: tree_sitter::Node, text: &str) -> Option<String> {
    let condition = try_condition(try_node)?;
    let consequence = clause_child(try_node, "then_clause").and_then(expression_of_clause);
    let else_clause = clause_child(try_node, "else_clause");

    let condition_text = &text[condition.start_byte()..condition.end_byte()];
    let consequence_text = consequence.map(|node| &text[node.start_byte()..node.end_byte()]);

    if let Some(consequence_text) = consequence_text {
        if consequence_text == condition_text && else_clause.is_none() {
            return Some(format!("try {condition_text}"));
        }
    }

    if let Some(else_clause) = else_clause {
        let alternative = expression_of_clause(else_clause)?;
        if is_null_literal(alternative, text) && try_alternative_is_else(try_node, text) {
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
        DocumentSnapshot::from_text(text.to_string(), &BuiltinData::load_from_split("", ""))
            .expect("fixture should parse")
    }

    #[test]
    fn conditional_null_else_refactor_drops_else_branch() {
        let text = "if ready then value else null";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(&document, &uri, Position::new(0, 4))
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
    fn conditional_null_then_refactor_negates_simple_condition() {
        let text = "if ready then null else value";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = conditional_null_code_action(&document, &uri, Position::new(0, 4))
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

        let action = conditional_null_code_action(&document, &uri, Position::new(0, 4))
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

        let action = conditional_null_code_action(&document, &uri, Position::new(0, 4))
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

        let action = conditional_null_code_action(&document, &uri, Position::new(0, 4))
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

        let action = conditional_null_code_action(&document, &uri, Position::new(0, 4))
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

        let action = simplify_try_code_action(&document, &uri, Position::new(0, 4))
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

        let action = simplify_try_code_action(&document, &uri, Position::new(0, 4))
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

        let action = simplify_try_code_action(&document, &uri, Position::new(0, 4))
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
            simplify_try_code_action(&document, &uri, Position::new(0, 4)).is_none(),
            "except branches should not be simplified by the else-null rewrite"
        );
    }

    #[test]
    fn simplify_if_condition_negates_equality() {
        let text = "if not (a == b) then x";
        let uri = Url::parse("file:///test.m2").expect("test uri should parse");
        let document = document(text);

        let action = simplify_if_condition_code_action(&document, &uri, Position::new(0, 4))
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

        let action = simplify_if_condition_code_action(&document, &uri, Position::new(0, 4))
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

        let action = simplify_if_condition_code_action(&document, &uri, Position::new(0, 4))
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

        let action = simplify_if_condition_code_action(&document, &uri, Position::new(0, 4))
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
            simplify_if_condition_code_action(&document, &uri, Position::new(0, 4)).is_none(),
            "simple conditions should not offer simplification"
        );
    }
}
