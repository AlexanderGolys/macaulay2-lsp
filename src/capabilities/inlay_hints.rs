//! Type inlay hints derived from static document analysis.

use std::collections::HashSet;

use tower_lsp::lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintServerCapabilities, OneOf, Position, Range,
};

use crate::document::DocumentSnapshot;
use crate::node_metadata::NodeKindMetadata;
use crate::object_registry::ObjectRegistry;
use crate::source::SourceNavigation;

pub(crate) fn inlay_hint_provider_capability() -> Option<OneOf<bool, InlayHintServerCapabilities>> {
    Some(OneOf::Left(true))
}

/// The inlay type hints for `range`: one per typed binding by default, plus
/// per-expression hints when `expression_types` is opted in.
pub(crate) fn inlay_hints_response(
    document: &DocumentSnapshot,
    range: Range,
    expression_types: bool,
    knowledge: &ObjectRegistry,
) -> Vec<InlayHint> {
    let mut hints = Vec::new();

    // Calm default: a single type hint per binding. The maximal per-expression
    // readout (every sub-expression's inferred type, useful for debugging the
    // inference but noisy and prone to overlapping at shared end positions) is
    // opt-in via `initializationOptions.inlayHints.expressionTypes`.
    hints.extend(binding_type_hints(document, &range));
    if expression_types {
        hints.extend(expression_type_hints(document, &range, knowledge));
    }
    hints.sort_by(|left, right| {
        (
            left.position.line,
            left.position.character,
            label_text(&left.label),
        )
            .cmp(&(
                right.position.line,
                right.position.character,
                label_text(&right.label),
            ))
    });

    hints
}

fn label_text(label: &InlayHintLabel) -> &str {
    match label {
        InlayHintLabel::String(text) => text,
        InlayHintLabel::LabelParts(parts) => {
            parts.first().map(|part| part.value.as_str()).unwrap_or("")
        }
    }
}

fn type_hint(position: Position, type_name: &str) -> InlayHint {
    InlayHint {
        position,
        label: InlayHintLabel::from(format!(": {type_name}")),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: None,
        padding_left: Some(true),
        padding_right: None,
        data: None,
    }
}

fn binding_type_hints(document: &DocumentSnapshot, range: &Range) -> Vec<InlayHint> {
    document
        .analysis()
        .typed_bindings_in_range(*range)
        .into_iter()
        .filter_map(|binding| {
            let type_name = binding.state.type_name.as_ref()?;
            // Place the hint after the value expression that evaluates to this
            // type (`x = expr : T`), not after the bound name — M2 has no
            // `x : T =` declaration syntax, so a trailing value-type annotation
            // reads more naturally. Falls back to the name's end when there is no
            // value range (e.g. a destructuring target).
            let position = binding
                .state
                .value_range
                .map(|value_range| value_range.end)
                .unwrap_or(binding.range.end);
            Some(type_hint(position, type_name.name()))
        })
        .collect()
}

fn expression_type_hints(
    document: &DocumentSnapshot,
    range: &Range,
    knowledge: &ObjectRegistry,
) -> Vec<InlayHint> {
    let analysis = document.analysis();
    // A binding already shows its type on the variable, so drop the RHS /
    // whole-assignment expression hint that would repeat the same type on the
    // value side (`x : T = expr : T` → keep only `x : T`). Keyed by the binding's
    // value-range end plus type, which both the RHS expression fact and the
    // assignment fact share.
    let binding_value_types: HashSet<(u32, u32, String)> = analysis
        .typed_bindings_in_range(*range)
        .into_iter()
        .filter_map(|binding| {
            let end = binding.state.value_range?.end;
            Some((
                end.line,
                end.character,
                binding.state.type_name.as_ref()?.name().to_string(),
            ))
        })
        .collect();
    document
        .root_node()
        .descendants()
        .filter(|node| node.kind.is_value_expression())
        .filter_map(|node| {
            let expression_range = document.range_for_node(node);
            if !range_contains(*range, expression_range) {
                return None;
            }
            let view = knowledge.at(expression_range.start);
            let type_name = analysis.infer_expression_type_label(node, document, &view)?;
            let end = expression_range.end;
            if binding_value_types.contains(&(end.line, end.character, type_name.clone())) {
                return None;
            }
            Some(type_hint(expression_range.end, &type_name))
        })
        .collect()
}

fn range_contains(outer: Range, inner: Range) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_registry::ObjectRegistry;
    use tower_lsp::lsp_types::Position;

    fn hints(text: &str, expression_types: bool) -> Vec<InlayHint> {
        let registry = ObjectRegistry::default();
        let document =
            DocumentSnapshot::from_text(text.to_string(), &registry).expect("fixture should parse");
        let range = Range::new(Position::new(0, 0), Position::new(u32::MAX, 0));
        inlay_hints_response(&document, range, expression_types, &registry)
    }

    fn labels(hints: &[InlayHint]) -> Vec<String> {
        hints
            .iter()
            .map(|hint| label_text(&hint.label).to_string())
            .collect()
    }

    #[test]
    fn calm_default_shows_one_binding_hint_per_binding() {
        // The Image #12 case: a single binding produces exactly one hint, on the
        // bound variable — not a doubled hint plus stray sub-expression hints.
        let hints = hints("Comment = new SelfInitializingType of TokenTree\n", false);
        assert_eq!(labels(&hints), vec![": SelfInitializingType".to_string()]);
        // The single hint sits after the value expression (end of the line, the
        // end of `TokenTree`), not after the bound name and not doubled.
        assert_eq!(hints[0].position, Position::new(0, 47));
    }

    #[test]
    fn expression_types_opt_in_adds_sub_expression_hints() {
        // The maximal readout is opt-in and yields strictly more hints than the
        // calm default (it annotates sub-expressions too).
        let calm = hints("Comment = new SelfInitializingType of TokenTree\n", false);
        let verbose = hints("Comment = new SelfInitializingType of TokenTree\n", true);
        assert!(
            verbose.len() > calm.len(),
            "opt-in readout should add hints: calm={}, verbose={}",
            calm.len(),
            verbose.len()
        );
    }

    #[test]
    fn calm_default_does_not_annotate_bare_expressions() {
        // A bare expression statement (not a binding) gets no hint in calm mode.
        let hints = hints("f 1\n", false);
        assert!(hints.is_empty(), "got {:?}", labels(&hints));
    }
}
