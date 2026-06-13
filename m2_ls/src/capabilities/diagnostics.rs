use std::collections::HashSet;

use tower_lsp::lsp_types::Url;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range as LspRange, SymbolKind,
};
use tower_lsp::Client;
use tree_sitter::Node;

use crate::analysis::{
    binary_expression_operator, is_assignment_expression, is_space_operator_expression,
    method_installation_signature, node_position, symbol_node_text, to_lsp_range,
    utf16_len_for_byte_span, Analysis, BindingRole,
};
use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind};

pub const ORPHAN_ELSE_DIAGNOSTIC_MESSAGE: &str =
    "An else clause is optional and can be separated from the statement by a linebreak 
    only in non-global scope.";
pub const AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE: &str =
    "This is parsed like function call: dot followed immediately by digits are always parsed 
    as literal floats with the high precedence, following only cobinding specifiers (like `symbol`) 
    and merging dots into range operators. This makes the member access `.` operator 
    impossible to use on literal number: 
        - `x.2` =>  `x SPACE .2`
        - `x.2.` => `x SPACE .2 . MISSING`
        - `x.2.2` => `x SPACE .2 SPACE .2`
        - `x..2` => `x .. 2`
        - `x...2` => `x .. .2`
        - `symbol.....2` => `symbol.. .. .2`";
pub const UNUSED_BINDING_DIAGNOSTIC_CODE: &str = "unused-binding";
pub const OPTION_KEY_CONVENTION_DIAGNOSTIC_CODE: &str = "option-key-convention";

pub(crate) async fn publish_diagnostics(client: &Client, uri: Url, document: &DocumentSnapshot) {
    let diagnostics = document.diagnostics().to_vec();
    client.publish_diagnostics(uri, diagnostics, None).await;
}

impl Analysis {
    pub(crate) fn collect_diagnostics(&mut self, node: Node, text: &str) {
        let m2_node = M2Node::new(node);

        if node.is_error() {
            self.diagnostics.push(Diagnostic {
                range: single_line_range(text, node.start_position(), node.start_byte()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Syntax error".to_string(),
                ..Default::default()
            });
        } else if node.is_missing() {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(text, node.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("Missing: {}", m2_node.raw_kind()),
                ..Default::default()
            });
        } else if let Some(range) = ambiguous_float_member_access_range(node, text) {
            self.diagnostics.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                message: AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE.to_string(),
                ..Default::default()
            });
        } else if is_assignment_expression(m2_node, text) {
            self.validate_assignment_form(node, text);
        } else if m2_node.kind == NodeKind::Cell {
            self.diagnose_leading_else(node, text);
        }

        // Runs independently of the chain above: any node may be an option pair,
        // and an option `=>` is never an error/missing/assignment node.
        self.diagnose_option_key_convention(node, text);

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_diagnostics(child, text);
        }
    }

    /// Macaulay2 convention capitalizes option names (`Strategy`, `DegreeLimit`).
    /// Flag a lowercase-initial key on an `=>` pair with a gentle Hint — but only
    /// when the pair is a function option, not a hashtable entry (see the context
    /// predicate, where lowercase keys are legitimate).
    fn diagnose_option_key_convention(&mut self, node: Node, text: &str) {
        let m2_node = M2Node::new(node);
        if binary_expression_operator(m2_node, text) != Some("=>") {
            return;
        }
        let Some(key) = m2_node.child_by_field_name("left") else {
            return;
        };
        if key.kind != NodeKind::Symbol {
            return;
        }
        let key_text = &text[key.inner().start_byte()..key.inner().end_byte()];
        let starts_lowercase = key_text
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase());
        if !starts_lowercase {
            return;
        }
        if !is_function_option_context(node) {
            return;
        }
        self.diagnostics.push(Diagnostic {
            range: to_lsp_range(text, key.inner().range()),
            severity: Some(DiagnosticSeverity::HINT),
            code: Some(NumberOrString::String(
                OPTION_KEY_CONVENTION_DIAGNOSTIC_CODE.to_string(),
            )),
            message: format!(
                "Option key `{key_text}` should be capitalized by Macaulay2 convention"
            ),
            ..Default::default()
        });
    }

    fn diagnose_leading_else(&mut self, cell: Node, text: &str) {
        let cell_text = &text[cell.start_byte()..cell.end_byte()];
        if !cell_text.trim_start().starts_with("else") {
            return;
        }

        let Some(symbol) = find_first_else_symbol(cell, text) else {
            return;
        };
        self.diagnostics.push(Diagnostic {
            range: to_lsp_range(text, symbol.range()),
            severity: Some(DiagnosticSeverity::ERROR),
            message: ORPHAN_ELSE_DIAGNOSTIC_MESSAGE.to_string(),
            ..Default::default()
        });
    }

    fn validate_assignment_form(&mut self, node: Node, text: &str) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let Some(operator) = node.child_by_field_name("operator") else {
            return;
        };
        let op_text = &text[operator.start_byte()..operator.end_byte()];

        let is_method_installation =
            op_text == ":=" && method_installation_signature(M2Node::new(left), text).is_some();

        if matches!(op_text, "=" | ":=")
            && !is_method_installation
            && !multiple_assignment_targets_are_symbols(left)
        {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(text, left.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("{op_text} multiple assignment targets must be symbols"),
                ..Default::default()
            });
        }

        if op_text == ":="
            && M2Node::new(left).is(NodeKind::BinaryExpression)
            && binary_expression_operator(M2Node::new(left), text) == Some("#")
        {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(text, left.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "`:=` cannot assign to parts; use `=` for part assignment".to_string(),
                ..Default::default()
            });
        }
    }

    pub(crate) fn collect_unused_binding_diagnostics(&mut self, root: Node, text: &str) {
        let mut used_bindings = HashSet::new();
        let mut cursor = root.walk();
        let mut reached_root = false;
        while !reached_root {
            let node = cursor.node();
            if M2Node::new(node).kind.is_symbol_like() {
                let name = &text[node.start_byte()..node.end_byte()];
                let position = node_position(text, M2Node::new(node));
                if let Some(binding_idx) = self.binding_idx_at(name, position) {
                    if let Some(binding) = self.registry.bindings.get(binding_idx) {
                        let node_range = to_lsp_range(text, node.range());
                        if node_range != binding.range {
                            used_bindings.insert(binding_idx);
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

        for (binding_idx, binding) in self.registry.bindings.iter().enumerate() {
            if binding.role != BindingRole::Ordinary {
                continue;
            }
            if binding.scope_idx == 0 {
                continue;
            }
            if !matches!(binding.kind, SymbolKind::VARIABLE | SymbolKind::FUNCTION) {
                continue;
            }
            if used_bindings.contains(&binding_idx) {
                continue;
            }
            let name = self.symbol_name(binding.symbol);
            if name.starts_with('_') {
                continue;
            }
            let noun = if binding.kind == SymbolKind::FUNCTION {
                "function"
            } else {
                "variable"
            };
            self.diagnostics.push(Diagnostic {
                range: binding.range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String(
                    UNUSED_BINDING_DIAGNOSTIC_CODE.to_string(),
                )),
                message: format!("Unused {noun} `{name}`"),
                ..Default::default()
            });
        }
    }
}

fn single_line_range(text: &str, start: tree_sitter::Point, start_byte: usize) -> LspRange {
    let start_line_byte = start_byte.saturating_sub(start.column);
    let line_end_byte = text[start_byte..]
        .find('\n')
        .map(|i| start_byte + i)
        .unwrap_or(text.len());

    LspRange::new(
        Position::new(
            start.row as u32,
            utf16_len_for_byte_span(text, start_line_byte, start_byte),
        ),
        Position::new(
            start.row as u32,
            utf16_len_for_byte_span(text, start_line_byte, line_end_byte),
        ),
    )
}

/// Decide whether an `=>` pair is a *function/method option* (convention applies)
/// versus a *hashtable / dictionary entry* (lowercase keys are legitimate).
fn is_function_option_context(option: Node<'_>) -> bool {
    // PENDING (parked — resume as a learn-by-doing): return true only when this
    // `=>` pair is a function option, and false when it is a hashtable/list entry.
    //
    // `option` is the `binary_expression` whose operator is `=>`. Walk its
    // ancestors via `M2Node::new(option).parent()` and inspect `.kind`:
    //   - call arguments / option lists live in a `NodeKind::Sequence` (the `(...)`
    //     after a function), e.g. `gb(I, Strategy => 4)`
    //   - hashtable & list literals are `NodeKind::List` (`{...}`) or
    //     `NodeKind::Array` (`[...]`), e.g. `hashTable {a => 1, b => 2}`
    // Decide which enclosing collection the pair belongs to. Returning `false`
    // here (the current default) makes the hint fire on nothing — safe but inert.
    let _ = option;
    false
}

fn multiple_assignment_targets_are_symbols(node: Node) -> bool {
    let m2_node = M2Node::new(node);
    if !matches!(m2_node.kind, NodeKind::Sequence | NodeKind::List) {
        return true;
    }

    let mut cursor = node.walk();
    let all_targets_are_symbols = node
        .named_children(&mut cursor)
        .all(|child| M2Node::new(child).kind == NodeKind::Symbol);
    all_targets_are_symbols
}

fn find_first_else_symbol<'tree>(node: Node<'tree>, text: &str) -> Option<Node<'tree>> {
    if M2Node::new(node).kind == NodeKind::Symbol
        && &text[node.start_byte()..node.end_byte()] == "else"
    {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(result) = find_first_else_symbol(child, text) {
            return Some(result);
        }
    }
    None
}

pub(crate) fn ambiguous_float_member_access_rewrite(node: Node<'_>, text: &str) -> Option<String> {
    if !is_space_operator_expression(M2Node::new(node)) {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    if symbol_node_text(M2Node::new(left), text).is_none()
        || M2Node::new(right).kind != NodeKind::FloatLiteral
        || left.end_byte() != right.start_byte()
    {
        return None;
    }

    let right_text = &text[right.start_byte()..right.end_byte()];
    let member_index = member_index_for_ambiguous_float_literal(right_text)?;
    Some(format!(
        "{}#{member_index}",
        &text[left.start_byte()..left.end_byte()]
    ))
}

pub(crate) fn member_index_for_ambiguous_float_literal(float_text: &str) -> Option<String> {
    if !float_text.starts_with('.') {
        return None;
    }

    let fractional_part = &float_text[1..];
    (!fractional_part.is_empty() && fractional_part.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| "0".to_string())
}

fn ambiguous_float_member_access_range(node: Node<'_>, text: &str) -> Option<LspRange> {
    ambiguous_float_member_access_rewrite(node, text).map(|_| to_lsp_range(text, node.range()))
}
