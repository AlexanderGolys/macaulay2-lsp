use tower_lsp::lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintServerCapabilities, OneOf, Range,
};

use crate::document::DocumentSnapshot;

pub(crate) fn inlay_hint_provider_capability() -> Option<OneOf<bool, InlayHintServerCapabilities>> {
    None
}

pub(crate) fn inlay_hints_response(document: &DocumentSnapshot, range: Range) -> Vec<InlayHint> {
    let _ = (document, range);
    Vec::new()
}

#[allow(dead_code)]
fn disabled_inlay_hints_response(document: &DocumentSnapshot, range: Range) -> Vec<InlayHint> {
    let mut hints = Vec::new();

    hints.extend(binding_type_hints(document, &range));
    hints.extend(expression_type_hints(document, &range));
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
            Some(InlayHint {
                position: binding.range.end,
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
    document
        .analysis()
        .typed_expression_facts_in_range(*range)
        .into_iter()
        .filter_map(|fact| {
            let crate::analysis::ExpressionType::Known(type_name) = &fact.result_type else {
                return None;
            };
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
    use crate::document::DocumentSnapshot;
    use crate::typesystem::BuiltinData;
    use tower_lsp::lsp_types::Position;

    fn builtins() -> BuiltinData {
        BuiltinData::load_from_split_with_type_facts(
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
            include_str!("../data/type_facts.jsonl"),
        )
    }

    fn document(text: &str) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), &builtins()).expect("fixture should parse")
    }

    #[test]
    fn inlay_hints_include_tracked_local_binding_types() {
        let document = document("Doc := Macaulay2Doc\nx := 1\n");

        let hints = inlay_hints_response(
            &document,
            Range::new(Position::new(0, 0), Position::new(2, 0)),
        );

        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].position, Position::new(0, 3));
        assert_eq!(label_text(&hints[0].label), ": Package");
        assert_eq!(hints[0].kind, Some(InlayHintKind::TYPE));
        assert_eq!(hints[1].position, Position::new(1, 1));
        assert_eq!(label_text(&hints[1].label), ": ZZ");
    }

    #[test]
    fn inlay_hints_include_typed_method_parameters() {
        let document = document("f ZZ := x -> x\n");

        let hints = inlay_hints_response(
            &document,
            Range::new(Position::new(0, 0), Position::new(1, 0)),
        );

        assert!(
            hints.iter().any(|hint| {
                hint.position == Position::new(0, 9) && label_text(&hint.label) == ": ZZ"
            }),
            "method-installation parameter should receive its tracked domain type"
        );
    }

    #[test]
    fn inlay_hints_show_expression_result_types() {
        let document = document("x := 1 + 2\n");

        let hints = inlay_hints_response(
            &document,
            Range::new(Position::new(0, 0), Position::new(1, 0)),
        );

        assert!(
            hints.iter().any(|hint| {
                hint.position == Position::new(0, 10) && label_text(&hint.label) == ": ZZ"
            }),
            "binary expression should show its result type"
        );
    }
}
