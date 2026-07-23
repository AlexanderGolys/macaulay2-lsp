//! Conversion of document analysis facts into published LSP diagnostics.

use std::collections::HashSet;

use tower_lsp::lsp_types::Url;
use tower_lsp::lsp_types::{Position, Range as LspRange, SymbolKind};
use tower_lsp::Client;

use crate::analysis::{symbol_node_text, Analysis, BindingRole};
use crate::diagnostic_registry::M2Diagnostic;
use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind};
use crate::util::{node_position, to_lsp_range, utf16_len_for_byte_span};

pub(crate) const AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE: &str =
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
pub(crate) async fn publish_diagnostics(client: &Client, uri: Url, document: &DocumentSnapshot) {
    let diagnostics = document.diagnostics().to_vec();
    client.publish_diagnostics(uri, diagnostics, None).await;
}

impl Analysis {
    /// Collect the structural diagnostics for the whole tree: syntax/missing-node
    /// errors, ambiguous float member access, assignment-form validation, and the
    /// option-key convention hint. Runs after the semantic passes, which it consumes.
    pub(crate) fn collect_diagnostics(&mut self, node: M2Node, text: &str) {
        if node.is_error() {
            self.diagnostics.push(M2Diagnostic::SyntaxError.at(
                single_line_range(text, node.start_position(), node.start_byte()),
                "Syntax error",
            ));
        } else if node.is_missing() {
            self.diagnostics.push(M2Diagnostic::MissingNode.at(
                to_lsp_range(text, node.range()),
                format!("Missing: {}", node.syntax_label()),
            ));
        } else if let Some(range) = ambiguous_float_member_access_range(node, text) {
            self.diagnostics.push(
                M2Diagnostic::AmbiguousFloatMemberAccess
                    .at(range, AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE),
            );
        } else if node.is_assignment() {
            self.validate_assignment_form(node, text);
        }

        // Runs independently of the chain above: any node may be an option pair,
        // and an option `=>` is never an error/missing/assignment node.
        self.diagnose_option_key_convention(node, text);

        for child in node.children() {
            self.collect_diagnostics(child, text);
        }
    }

    /// Macaulay2 convention capitalizes option names (`Strategy`, `DegreeLimit`).
    /// Flag a lowercase-initial key on an `=>` pair with a gentle Hint — but only
    /// when the pair is a function option, not a hashtable entry (see the context
    /// predicate, where lowercase keys are legitimate).
    fn diagnose_option_key_convention(&mut self, node: M2Node, text: &str) {
        if node.binary_operator() != Some("=>") {
            return;
        }
        let Some(key) = node.child_by_field_name("left") else {
            return;
        };
        if key.kind != NodeKind::Symbol {
            return;
        }
        let key_text = key.text();
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
        self.diagnostics.push(M2Diagnostic::OptionKeyConvention.at(
            to_lsp_range(text, key.range()),
            format!("Option key `{key_text}` should be capitalized by Macaulay2 convention"),
        ));
    }

    fn validate_assignment_form(&mut self, node: M2Node, text: &str) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let Some(operator) = node.child_by_field_name("operator") else {
            return;
        };
        let op_text = operator.text();

        // Consume the installation fact characterized during analysis (which had
        // the type registry) rather than re-deciding install-vs-call here.
        let is_method_installation = self.installation_for(node, text).is_some();

        if matches!(op_text, "=" | ":=")
            && !is_method_installation
            && !multiple_assignment_targets_are_symbols(left)
        {
            self.diagnostics
                .push(M2Diagnostic::MultipleAssignmentTargets.at(
                    to_lsp_range(text, left.range()),
                    format!("{op_text} multiple assignment targets must be symbols"),
                ));
        }

        if op_text == ":=" && left.binary_operator() == Some("#") {
            self.diagnostics
                .push(M2Diagnostic::ColonEqualPartAssignment.at(
                    to_lsp_range(text, left.range()),
                    "`:=` cannot assign to parts; use `=` for part assignment",
                ));
        }

        if matches!(op_text, "=" | ":=") && !is_method_installation {
            if let Some(right) = node.child_by_field_name("right") {
                self.validate_parallel_assignment_arity(left, right, text);
            }
        }
    }

    /// A destructuring assignment whose right-hand side is itself a fixed-length
    /// collection literal must match arity: `[x, y] = [a, b, c]` and
    /// `[x, y] = {a}` are always errors, while `[x, y] = a` and `[x, y] = (a)`
    /// are runtime-checked (the right side's length is not known statically) and
    /// left alone. Recurses so nested targets like `[x, [y, z]] = [1, {2, 3, 4}]`
    /// are checked at every level where both sides are collection literals.
    fn validate_parallel_assignment_arity(&mut self, left: M2Node, right: M2Node, text: &str) {
        if !is_fixed_length_collection(left) || !is_fixed_length_collection(right) {
            return;
        }

        let target_nodes = left.named_children().collect::<Vec<_>>();
        let value_nodes = right.named_children().collect::<Vec<_>>();
        if target_nodes.len() != value_nodes.len() {
            self.diagnostics.push(M2Diagnostic::ParallelAssignmentArity.at(
                to_lsp_range(text, right.range()),
                format!(
                    "parallel assignment binds {} targets but the right-hand side lists {}; their lengths must match",
                    target_nodes.len(),
                    value_nodes.len()
                ),
            ));
            return;
        }

        for (target, value) in target_nodes.iter().zip(value_nodes.iter()) {
            self.validate_parallel_assignment_arity(*target, *value, text);
        }
    }

    /// Warn about non-global bindings (variables and functions) that are never
    /// referenced outside their own definition site. Top-level bindings are
    /// potential exports and stay silent, as do `_`-prefixed names.
    pub(crate) fn collect_unused_binding_diagnostics(&mut self, root: M2Node, text: &str) {
        let mut used_bindings = HashSet::new();
        for node in root.descendants() {
            if node.kind.is_symbol_like() {
                let name = node.text();
                let position = node_position(text, node);
                if let Some(binding_idx) = self.binding_idx_at(name, position) {
                    if let Some(binding) = self.registry.bindings.get(binding_idx) {
                        let node_range = to_lsp_range(text, node.range());
                        if node_range != binding.range {
                            used_bindings.insert(binding_idx);
                        }
                    }
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
            self.diagnostics.push(
                M2Diagnostic::UnusedBinding.at(binding.range, format!("Unused {noun} `{name}`")),
            );
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
fn is_function_option_context(option: M2Node<'_>) -> bool {
    // `option` is the `=>` `binary_expression`. It is a function/method option —
    // where the uppercase-key convention applies — when its nearest enclosing
    // collection is a call's argument `Sequence`, e.g. `gb(I, Strategy => 4)`. It
    // is an ordinary dictionary/list entry — where lowercase keys are legitimate
    // and the hint must stay silent — when that collection is a `List`/`Array`
    // literal, e.g. `hashTable {a => 1, b => 2}`.
    //
    let mut current = option;
    while let Some(parent) = current.parent() {
        match parent.kind {
            NodeKind::Sequence => return true,
            NodeKind::List | NodeKind::Array | NodeKind::AngleBarList => return false,
            _ => current = parent,
        }
    }
    false
}

/// A genuine fixed-length collection literal, whose arity is known statically.
/// `List`/`Array`/`AngleBarList` of any length qualify, including length 0 and 1
/// (`{a}` is a real one-element list). A `Sequence` qualifies at every length
/// except 1: the current grammar represents a parenthesized expression `(a)` as
/// a length-1 `Sequence`, so that single case is runtime-checked, not static.
/// The empty sequence `()` (length 0) is a real value and stays in scope.
fn is_fixed_length_collection(node: M2Node) -> bool {
    if !node.kind.is_collection_expression() {
        return false;
    }
    node.kind != NodeKind::Sequence || node.named_children().count() != 1
}

fn multiple_assignment_targets_are_symbols(node: M2Node) -> bool {
    if !node.kind.is_collection_expression() {
        return true;
    }

    // A target element is valid when it is a plain symbol or itself a nested
    // target collection whose elements are all valid (`[x, [y, z]] = ...`). The
    // recursion is gated on the child being a collection because the early
    // return above yields `true` for non-collections -- a bare recursive call
    // would otherwise wrongly accept `[x + 1, y]`.
    node.named_children().all(|child| {
        child.kind == NodeKind::Symbol
            || (child.kind.is_collection_expression()
                && multiple_assignment_targets_are_symbols(child))
    })
}

pub(crate) fn ambiguous_float_member_access_rewrite(node: M2Node<'_>) -> Option<String> {
    if !node.is_space_application() {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    if symbol_node_text(left).is_none()
        || right.kind != NodeKind::FloatLiteral
        || left.end_byte() != right.start_byte()
    {
        return None;
    }

    let right_text = right.text();
    let member_index = member_index_for_ambiguous_float_literal(right_text)?;
    Some(format!("{}#{member_index}", left.text()))
}

pub(crate) fn member_index_for_ambiguous_float_literal(float_text: &str) -> Option<String> {
    if !float_text.starts_with('.') {
        return None;
    }

    let fractional_part = &float_text[1..];
    (!fractional_part.is_empty() && fractional_part.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| fractional_part.to_string())
}

fn ambiguous_float_member_access_range(node: M2Node<'_>, text: &str) -> Option<LspRange> {
    ambiguous_float_member_access_rewrite(node).map(|_| to_lsp_range(text, node.range()))
}
