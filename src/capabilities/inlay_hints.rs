//! Type inlay hints derived from static document analysis.

use tower_lsp::lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintServerCapabilities, OneOf, Position,
    Range as TextRange,
};

use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::object_registry::{ObjectName, ObjectRegistry};
use crate::source::SourceNavigation;
use crate::util::position_in_range;

pub fn inlay_hint_provider_capability() -> Option<OneOf<bool, InlayHintServerCapabilities>> {
    Some(OneOf::Left(true))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InlayHintOptions {
    pub expression_types: bool,
    pub all_known_types: bool,
}

/// The inlay type hints for `range`: one per typed binding by default, plus
/// per-expression hints when `expression_types` is opted in.
pub fn inlay_hints_response(
    document: &DocumentSnapshot,
    range: TextRange,
    options: InlayHintOptions,
    knowledge: &ObjectRegistry,
) -> Vec<InlayHint> {
    let binding_hints = binding_type_hints(document, &range, options.all_known_types, knowledge);
    let mut hints = binding_hints.clone();
    hints.extend(lambda_return_type_hints(
        document,
        &range,
        options.all_known_types,
        knowledge,
    ));

    if options.expression_types || options.all_known_types {
        hints.extend(expression_type_hints(
            document,
            &range,
            options.all_known_types,
            knowledge,
            &binding_hints,
        ));
    }
    hints.extend(parameter_name_hints(document, &range, knowledge));
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
    hints.dedup_by(|left, right| {
        left.position == right.position
            && left.kind == right.kind
            && label_text(&left.label) == label_text(&right.label)
    });

    hints
}

fn lambda_return_type_hints(
    document: &DocumentSnapshot,
    range: &TextRange,
    all_known_types: bool,
    knowledge: &ObjectRegistry,
) -> Vec<InlayHint> {
    let analysis = document.analysis();
    document
        .root_node()
        .descendants()
        .filter_map(|node| match node.kind {
            NodeKind::ReturnStatement => {
                let value = node
                    .named_children()
                    .find(|child| child.kind.is_value_expression())?;
                let view = knowledge.at(document.position_for_node(value));
                analysis.control_transfer_target(node, document, &view)?;
                Some(value)
            }
            NodeKind::LambdaExpression => lambda_final_value(node),
            _ => None,
        })
        .filter(|value| all_known_types || !is_self_describing_value(*value))
        .filter_map(|value| {
            let value_range = document.range_for_node(value);
            if !position_in_range(value_range.end, *range) {
                return None;
            }
            let view = knowledge.at(value_range.start);
            let type_name =
                inlay_type_label(analysis.infer_expression_type_label(value, document, &view)?);
            (type_name != "Thing").then(|| type_hint(value_range.end, &type_name))
        })
        .collect()
}

fn lambda_final_value(lambda: M2Node<'_>) -> Option<M2Node<'_>> {
    let mut value = lambda.child_by_field_name("body")?;
    while value.kind == NodeKind::ParenthesizedExpression {
        value = value.final_value_child()?;
    }
    (value.kind != NodeKind::ReturnStatement).then_some(value)
}

fn label_text(label: &InlayHintLabel) -> &str {
    match label {
        InlayHintLabel::String(text) => text,
        InlayHintLabel::LabelParts(parts) => {
            parts.first().map(|part| part.value.as_str()).unwrap_or("")
        }
    }
}

fn inlay_type_label(type_name: String) -> String {
    let members = type_name.split(" | ").collect::<Vec<_>>();
    match members.as_slice() {
        ["Nothing", value] | [value, "Nothing"] => format!("{value}?"),
        _ => type_name,
    }
}

fn type_hint(position: Position, type_name: &str) -> InlayHint {
    InlayHint {
        position,
        label: InlayHintLabel::from(type_name.to_string()),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: None,
        padding_left: Some(true),
        padding_right: None,
        data: None,
    }
}

fn parameter_hint(position: Position, parameter_name: &str) -> InlayHint {
    InlayHint {
        position,
        label: InlayHintLabel::from(format!("{parameter_name}:")),
        kind: Some(InlayHintKind::PARAMETER),
        text_edits: None,
        tooltip: None,
        padding_left: None,
        padding_right: Some(true),
        data: None,
    }
}

struct AssignedValue {
    target_range: TextRange,
    value_range: TextRange,
    type_name: String,
    destructured: bool,
}

#[derive(PartialEq, Eq)]
struct TypeHintIdentity {
    position: Position,
    label: String,
}

fn binding_type_hints(
    document: &DocumentSnapshot,
    range: &TextRange,
    all_known_types: bool,
    knowledge: &ObjectRegistry,
) -> Vec<InlayHint> {
    let assigned_values = assigned_values(document, knowledge);
    let self_describing_values = self_describing_assignment_values(document);
    let mut hints = Vec::new();

    for binding in document.analysis().bindings() {
        let mut previous_type: Option<String> = None;
        for state in &binding.states {
            let assigned = assigned_values
                .iter()
                .find(|value| value.target_range == state.span);
            let type_name = assigned.map(|value| value.type_name.clone()).or_else(|| {
                state
                    .type_name
                    .as_ref()
                    .map(ObjectName::name)
                    .map(ToString::to_string)
            });
            let Some(type_name) = type_name else {
                previous_type = None;
                continue;
            };
            let changed = previous_type.as_ref() != Some(&type_name);
            previous_type = Some(type_name.clone());
            if (!all_known_types && !changed && assigned.is_none_or(|value| !value.destructured))
                || type_name == "Thing"
            {
                continue;
            }
            let value_range = assigned
                .map(|value| value.value_range)
                .or(state.value_range);
            if !all_known_types
                && value_range.is_some_and(|value_range| {
                    self_describing_values.contains(&value_range)
                        && assigned.is_none_or(|value| !value.destructured)
                })
            {
                continue;
            }
            let position = state.span.end;
            if position_in_range(position, *range) {
                hints.push(type_hint(position, &type_name));
            }
        }
    }

    hints
}

fn expression_type_hints(
    document: &DocumentSnapshot,
    range: &TextRange,
    all_known_types: bool,
    knowledge: &ObjectRegistry,
    binding_hints: &[InlayHint],
) -> Vec<InlayHint> {
    let analysis = document.analysis();
    let binding_value_types: Vec<TypeHintIdentity> = binding_hints
        .iter()
        .filter(|hint| hint.kind == Some(InlayHintKind::TYPE))
        .map(|hint| TypeHintIdentity {
            position: hint.position,
            label: label_text(&hint.label).to_string(),
        })
        .collect();
    let self_describing_values = self_describing_assignment_values(document);
    let assignment_parts = assignment_parts(document);
    document
        .root_node()
        .descendants()
        .filter(|node| node.kind.is_value_expression())
        .filter_map(|node| {
            let expression_range = document.range_for_node(node);
            if !range_contains(*range, expression_range)
                || (!all_known_types
                    && (assignment_parts.suppresses(expression_range)
                        || self_describing_values
                            .iter()
                            .any(|value_range| range_contains(*value_range, expression_range))))
            {
                return None;
            }
            let view = knowledge.at(expression_range.start);
            let type_name =
                inlay_type_label(analysis.infer_expression_type_label(node, document, &view)?);
            if type_name == "Thing" {
                return None;
            }
            let end = expression_range.end;
            if binding_value_types.contains(&TypeHintIdentity {
                position: end,
                label: type_name.clone(),
            }) {
                return None;
            }
            Some(type_hint(expression_range.end, &type_name))
        })
        .collect()
}

struct AssignmentParts {
    assignments: Vec<TextRange>,
    targets: Vec<TextRange>,
    values: Vec<TextRange>,
}

impl AssignmentParts {
    fn suppresses(&self, expression: TextRange) -> bool {
        self.assignments.contains(&expression)
            || self.values.contains(&expression)
            || self
                .targets
                .iter()
                .any(|target| range_contains(*target, expression))
    }
}

fn assignment_parts(document: &DocumentSnapshot) -> AssignmentParts {
    let mut parts = AssignmentParts {
        assignments: Vec::new(),
        targets: Vec::new(),
        values: Vec::new(),
    };
    for assignment in document
        .root_node()
        .descendants()
        .filter(|node| node.is_assignment())
    {
        parts.assignments.push(document.range_for_node(assignment));
        if let Some(target) = assignment.child_by_field_name("left") {
            parts.targets.push(document.range_for_node(target));
        }
        if let Some(value) = assignment.child_by_field_name("right") {
            parts.values.push(document.range_for_node(value));
        }
    }
    parts
}

fn parameter_name_hints(
    document: &DocumentSnapshot,
    range: &TextRange,
    knowledge: &ObjectRegistry,
) -> Vec<InlayHint> {
    document
        .root_node()
        .descendants()
        .filter(|node| node.is_space_application())
        .flat_map(|call| {
            let Some(arguments) = call.child_by_field_name("right") else {
                return Vec::new();
            };
            if !matches!(
                arguments.kind,
                NodeKind::ParenthesizedExpression | NodeKind::Sequence
            ) {
                return Vec::new();
            }
            let Some(callable) = call.child_by_field_name("left") else {
                return Vec::new();
            };
            if document
                .analysis()
                .local_method_installation_signature_at(callable, document)
                .is_some()
            {
                return Vec::new();
            }
            let view = knowledge.at(document.position_for_node(call));
            let Some(parameter_names) = document
                .analysis()
                .call_parameter_names(call, document, &view)
            else {
                return Vec::new();
            };
            let arguments = call_arguments(arguments)
                .into_iter()
                .filter(|argument| !argument.is_option_assignment())
                .collect::<Vec<_>>();
            if parameter_names.len() != arguments.len() {
                return Vec::new();
            }
            parameter_names
                .into_iter()
                .zip(arguments)
                .filter_map(|(name, argument)| {
                    let position = document.range_for_node(argument).start;
                    if name.name() == "_" || !position_in_range(position, *range) {
                        return None;
                    }
                    Some(parameter_hint(position, name.name()))
                })
                .collect()
        })
        .collect()
}

fn call_arguments(arguments: M2Node<'_>) -> Vec<M2Node<'_>> {
    let arguments = if arguments.kind == NodeKind::ParenthesizedExpression {
        match arguments.final_value_child() {
            Some(value) => value,
            None => return Vec::new(),
        }
    } else {
        arguments
    };
    if arguments.kind == NodeKind::Sequence {
        arguments.collection_elements().collect()
    } else {
        vec![arguments]
    }
}

fn assigned_values(document: &DocumentSnapshot, knowledge: &ObjectRegistry) -> Vec<AssignedValue> {
    let mut values = Vec::new();
    for assignment in document.root_node().descendants().filter(|node| {
        node.is_assignment() && matches!(node.binary_operator(), Some("=") | Some(":="))
    }) {
        let (Some(left), Some(right)) = (
            assignment.child_by_field_name("left"),
            assignment.child_by_field_name("right"),
        ) else {
            continue;
        };
        let destructured = left.kind.is_collection_expression();
        let pairs = if destructured {
            paired_assignment_values(left, right)
        } else if left.kind == NodeKind::Symbol {
            vec![(left, right)]
        } else {
            Vec::new()
        };
        for (target, value) in pairs {
            let value_range = document.range_for_node(value);
            let view = knowledge.at(value_range.start);
            let Some(type_name) = document
                .analysis()
                .infer_expression_type_label(value, document, &view)
                .map(inlay_type_label)
            else {
                continue;
            };
            values.push(AssignedValue {
                target_range: document.range_for_node(target),
                value_range,
                type_name,
                destructured,
            });
        }
    }
    values
}

fn paired_assignment_values<'tree>(
    target: M2Node<'tree>,
    value: M2Node<'tree>,
) -> Vec<(M2Node<'tree>, M2Node<'tree>)> {
    if target.kind == NodeKind::Symbol {
        return vec![(target, value)];
    }
    if !target.kind.is_collection_expression() {
        return Vec::new();
    }
    let targets = target.collection_elements().collect::<Vec<_>>();
    let value = parenthesized_value(value);
    if value.kind.is_collection_expression() {
        let values = value.collection_elements().collect::<Vec<_>>();
        if targets.len() != values.len() {
            return Vec::new();
        }
        return targets
            .into_iter()
            .zip(values)
            .flat_map(|(target, value)| paired_assignment_values(target, value))
            .collect();
    }
    match targets.as_slice() {
        [target] => paired_assignment_values(*target, value),
        _ => Vec::new(),
    }
}

fn self_describing_assignment_values(document: &DocumentSnapshot) -> Vec<TextRange> {
    document
        .root_node()
        .descendants()
        .filter(|node| node.is_assignment())
        .filter_map(|assignment| {
            let left = assignment.child_by_field_name("left")?;
            let right = assignment.child_by_field_name("right")?;
            (left.kind == NodeKind::Symbol && is_self_describing_value(right))
                .then(|| document.range_for_node(right))
        })
        .collect()
}

fn is_self_describing_value(node: M2Node<'_>) -> bool {
    let node = parenthesized_value(node);
    node.kind.is_literal()
        || node.kind.is_nothing_value()
        || (node.kind == NodeKind::Symbol && matches!(node.text(), "true" | "false" | "null"))
        || node.kind == NodeKind::LambdaExpression
        || node.kind == NodeKind::NewStatement
        || ((node.kind.is_collection_expression() || node.kind.is_sequence())
            && node.collection_elements().all(is_self_describing_value))
}

fn parenthesized_value(mut node: M2Node<'_>) -> M2Node<'_> {
    while node.kind == NodeKind::ParenthesizedExpression {
        let Some(value) = node.final_value_child() else {
            break;
        };
        node = value;
    }
    node
}

fn range_contains(outer: TextRange, inner: TextRange) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_registry::ObjectRegistry;

    fn hints(text: &str, expression_types: bool) -> Vec<InlayHint> {
        let registry = ObjectRegistry::default();
        let document =
            DocumentSnapshot::from_text(text.to_string(), &registry).expect("fixture should parse");
        let range = TextRange::new(pos!(), pos!(u32::MAX, 0));
        inlay_hints_response(
            &document,
            range,
            InlayHintOptions {
                expression_types,
                all_known_types: false,
            },
            &registry,
        )
    }

    fn labels(hints: &[InlayHint]) -> Vec<String> {
        hints
            .iter()
            .map(|hint| label_text(&hint.label).to_string())
            .collect()
    }

    #[test]
    fn calm_default_shows_informative_computed_assignment_types() {
        let computed = hints("x = if condition then 1 else 2\n", false);
        assert_eq!(labels(&computed), vec!["ZZ".to_string()]);
        assert_eq!(computed[0].position, pos!(0, 1));

        let self_describing = concat!(
            "x = 1\n",
            "x = (2)\n",
            "x = [1]\n",
            "f = x -> x\n",
            "y = new MutableList from {1, 2}\n",
            "truth = true\n",
            "falsity = false\n",
            "nothing = null\n",
        );
        let calm = hints(self_describing, false);
        assert!(calm.is_empty(), "got {calm:?}");
        let verbose = hints(self_describing, true);
        assert!(verbose.is_empty(), "got {:?}", labels(&verbose));
    }

    #[test]
    fn expression_types_opt_in_adds_sub_expression_hints() {
        // The maximal readout is opt-in and yields strictly more hints than the
        // calm default (it annotates sub-expressions too).
        let source = "x = if condition then 1 else 2\n";
        let calm = hints(source, false);
        let verbose = hints(source, true);
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
