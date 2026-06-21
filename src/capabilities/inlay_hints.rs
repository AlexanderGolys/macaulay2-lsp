use std::collections::HashSet;

use tower_lsp::lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintServerCapabilities, OneOf, Range,
};

use crate::document::DocumentSnapshot;

pub(crate) fn inlay_hint_provider_capability() -> Option<OneOf<bool, InlayHintServerCapabilities>> {
    Some(OneOf::Left(true))
}

pub(crate) fn inlay_hints_response(
    document: &DocumentSnapshot,
    range: Range,
    expression_types: bool,
) -> Vec<InlayHint> {
    let mut hints = Vec::new();

    // Calm default: a single type hint per binding. The maximal per-expression
    // readout (every sub-expression's inferred type, useful for debugging the
    // inference but noisy and prone to overlapping at shared end positions) is
    // opt-in via `initializationOptions.inlayHints.expressionTypes`.
    hints.extend(binding_type_hints(document, &range));
    if expression_types {
        hints.extend(expression_type_hints(document, &range));
    }
    hints.sort_by_key(|hint| {
        (
            hint.position.line,
            hint.position.character,
            label_text(&hint.label).to_string(),
        )
    });

    hints
}

fn label_text(label: &InlayHintLabel) -> &str {
    match label {
        InlayHintLabel::String(text) => text,
        InlayHintLabel::LabelParts(parts) => parts
            .first()
            .map(|part| part.value.as_str())
            .expect("inlay hint labels should not be empty"),
    }
}

fn binding_type_hints(document: &DocumentSnapshot, range: &Range) -> Vec<InlayHint> {
    document
        .analysis()
        .typed_bindings_in_range(*range)
        .into_iter()
        .filter_map(|binding| {
            let type_name = binding.type_name.as_ref()?;
            // Place the hint after the value expression that evaluates to this
            // type (`x = expr : T`), not after the bound name — M2 has no
            // `x : T =` declaration syntax, so a trailing value-type annotation
            // reads more naturally. Falls back to the name's end when there is no
            // value range (e.g. a destructuring target).
            let position = binding
                .value_range
                .map(|value_range| value_range.end)
                .unwrap_or(binding.range.end);
            Some(InlayHint {
                position,
                label: InlayHintLabel::from(format!(": {type_name}")),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: None,
                data: None,
            })
        })
        .collect()
}

fn expression_type_hints(document: &DocumentSnapshot, range: &Range) -> Vec<InlayHint> {
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
            let end = binding.value_range?.end;
            Some((end.line, end.character, binding.type_name.clone()?))
        })
        .collect();
    analysis
        .typed_expression_facts_in_range(*range)
        .into_iter()
        .filter_map(|fact| {
            let type_name = fact.result_type.label()?;
            let end = fact.span.range.end;
            if binding_value_types.contains(&(end.line, end.character, type_name.clone())) {
                return None;
            }
            Some(InlayHint {
                position: fact.span.range.end,
                label: InlayHintLabel::from(format!(": {type_name}")),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: None,
                data: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typesystem::BuiltinData;
    use tower_lsp::lsp_types::Position;

    fn hints(text: &str, expression_types: bool) -> Vec<InlayHint> {
        let document = DocumentSnapshot::from_text(text.to_string(), &BuiltinData::empty())
            .expect("fixture should parse");
        let range = Range::new(Position::new(0, 0), Position::new(u32::MAX, 0));
        inlay_hints_response(&document, range, expression_types)
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
