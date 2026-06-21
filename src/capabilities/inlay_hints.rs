use tower_lsp::lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintServerCapabilities, OneOf, Range,
};

use crate::document::DocumentSnapshot;

pub(crate) fn inlay_hint_provider_capability() -> Option<OneOf<bool, InlayHintServerCapabilities>> {
    Some(OneOf::Left(true))
}

pub(crate) fn inlay_hints_response(document: &DocumentSnapshot, range: Range) -> Vec<InlayHint> {
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
