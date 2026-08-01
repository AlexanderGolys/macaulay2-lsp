//! Ring construction, generator rebinding, and ring-specific type rules.

use super::typechecker::{OperatorTypeQuery, TypeChecker};
use super::*;

/// Runtime binding shape produced for a ring generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingGeneratorKind {
    Direct,
    IndexedTable,
}

/// A ring-generator name and source node extracted from constructor syntax.
#[derive(Debug, Clone)]
struct RingGeneratorBinding<'tree> {
    name: String,
    kind: RingGeneratorKind,
    node: M2Node<'tree>,
}

/// Compact reference retained for later ring-generator rebinding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingGenerator {
    name: ObjectName,
    kind: RingGeneratorKind,
}

/// Hardcoded static semantics that supplement indexed ring dispatch facts.
pub struct RingSemantics;

impl RingSemantics {
    /// Return the instance parent introduced by a source-defined ring value.
    pub fn value_parent(
        type_name: Option<&ObjectName>,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<ObjectName> {
        type_name
            .is_some_and(|type_name| knowledge.has_type_role(type_name, TypeRole::Ring))
            .then(|| ObjectName::new("RingElement"))
    }
}

impl Analysis {
    pub fn collect_ring_generator_bindings(
        &mut self,
        ring_name: &str,
        expression: M2Node,
        rebind_node: M2Node,
        scope_idx: usize,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        let containers = expression
            .descendants()
            .filter(|node| node.is_space_application())
            .filter_map(|node| {
                let head = node.child_by_field_name("left")?;
                let variables = RingGeneratorBinding::constructor_variables(node)?;
                TypeChecker::new(self)
                    .type_of(head, source, scope_idx, knowledge)
                    .principal()
                    .is_some_and(|head_type| knowledge.has_type_role(head_type, TypeRole::Ring))
                    .then_some(variables)
            })
            .collect::<Vec<_>>();

        let mut generators = Vec::new();
        for container in containers {
            for generator in RingGeneratorBinding::collect(container) {
                generators.push(RingGenerator {
                    name: ObjectName::new(&generator.name),
                    kind: generator.kind,
                });
                self.register_ring_generator(
                    ring_name,
                    &generator.name,
                    generator.kind,
                    generator.node,
                    source,
                );
            }
        }

        if generators.is_empty() {
            generators = self
                .ring_source_symbol(expression)
                .and_then(|source| self.registry.ring_generators.get(source).cloned())
                .unwrap_or_default();
            for generator in &generators {
                self.register_ring_generator(
                    ring_name,
                    generator.name.name(),
                    generator.kind,
                    rebind_node,
                    source,
                );
            }
        }

        self.registry
            .ring_generators
            .insert(ObjectName::new(ring_name), generators);
    }

    fn ring_source_symbol<'tree>(&self, expression: M2Node<'tree>) -> Option<&'tree str> {
        let expression = parenthesized_value(expression).unwrap_or(expression);
        if expression.kind.is_symbol_like() {
            return Some(expression.text());
        }
        if expression.binary_operator() == Some("/") {
            return expression
                .child_by_field_name("left")
                .filter(|left| left.kind.is_symbol_like())
                .map(|left| left.text());
        }
        None
    }

    fn register_ring_generator(
        &mut self,
        ring_name: &str,
        generator_name: &str,
        kind: RingGeneratorKind,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        match kind {
            RingGeneratorKind::Direct => {
                self.register_dynamic_global(
                    generator_name,
                    node,
                    ObjectName::new(ring_name),
                    None,
                    source,
                );
            }
            RingGeneratorKind::IndexedTable => {
                self.register_dynamic_global(
                    generator_name,
                    node,
                    ObjectName::new("IndexedVariableTable"),
                    Some(ObjectName::new(ring_name)),
                    source,
                );
            }
        }
    }

    fn register_dynamic_global(
        &mut self,
        name: &str,
        node: M2Node,
        type_name: ObjectName,
        indexed_element_type: Option<ObjectName>,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let position = source.position_for_node(node);
        let binding_id = self
            .binding_id_from_scope(name, 0, position)
            .filter(|binding_id| {
                self.binding_anchor(*binding_id)
                    .is_some_and(|binding| binding.scope_idx == 0)
            });
        let registration = SymbolRegistration {
            presentation_kind: SymbolKind::VARIABLE,
            role: BindingRole::Ordinary,
            type_name: Some(type_name),
            object_id: None,
            indexed_element_type,
            parent_type: None,
            scope_idx: 0,
            potential_export: true,
        };
        if let Some(binding_id) = binding_id {
            self.add_binding_state(binding_id, node, None, registration, source);
        } else {
            self.add_symbol(name, node, None, registration, source);
        }
    }
}

impl<'tree> RingGeneratorBinding<'tree> {
    fn constructor_variables(application: M2Node<'tree>) -> Option<M2Node<'tree>> {
        let argument = application.child_by_field_name("right")?;
        if argument.kind.is_collection_expression() {
            return Some(argument);
        }
        argument
            .child_by_field_name("left")
            .filter(|left| left.kind.is_collection_expression())
    }

    fn collect(container: M2Node<'tree>) -> Vec<Self> {
        let elements = container.collection_elements().collect::<Vec<_>>();
        let variable_base = elements
            .iter()
            .find_map(|element| Self::option_value(*element, "VariableBaseName"))
            .and_then(Self::base_name);
        let mut bindings = Vec::new();

        for element in elements {
            if element.is_option_assignment() {
                if let Some(variables) = Self::option_value(element, "Variables") {
                    if variables.kind == NodeKind::IntegerLiteral {
                        let name = variable_base.clone().unwrap_or_else(|| "p".to_string());
                        Self::push(
                            &mut bindings,
                            RingGeneratorBinding {
                                name,
                                kind: RingGeneratorKind::IndexedTable,
                                node: variables,
                            },
                        );
                    } else {
                        Self::collect_spec(variables, &mut bindings);
                    }
                }
                continue;
            }
            Self::collect_spec(element, &mut bindings);
        }

        bindings
    }

    fn option_value(node: M2Node<'tree>, key: &str) -> Option<M2Node<'tree>> {
        if !node.is_option_assignment() {
            return None;
        }
        node.child_by_field_name("left")
            .filter(|left| left.kind == NodeKind::Symbol && left.text() == key)?;
        node.child_by_field_name("right")
    }

    fn base_name(node: M2Node<'tree>) -> Option<String> {
        match node.kind {
            NodeKind::Symbol => Some(node.text().to_string()),
            NodeKind::StringLiteral => node.string_literal_inner_text().map(ToString::to_string),
            _ => None,
        }
    }

    fn collect_spec(node: M2Node<'tree>, bindings: &mut Vec<RingGeneratorBinding<'tree>>) {
        if node.kind == NodeKind::Symbol {
            Self::push(
                bindings,
                RingGeneratorBinding {
                    name: node.text().to_string(),
                    kind: RingGeneratorKind::Direct,
                    node,
                },
            );
            return;
        }

        if node.binary_operator() == Some("_") {
            if let Some(base) = node
                .child_by_field_name("left")
                .filter(|base| base.kind == NodeKind::Symbol)
            {
                Self::push(
                    bindings,
                    RingGeneratorBinding {
                        name: base.text().to_string(),
                        kind: RingGeneratorKind::IndexedTable,
                        node: base,
                    },
                );
            }
            return;
        }

        if matches!(node.binary_operator(), Some("..") | Some("..<")) {
            let left = node.child_by_field_name("left");
            let right = node.child_by_field_name("right");
            if let (Some(left), Some(right)) = (left, right) {
                if let (Some(left_base), Some(right_base)) =
                    (Self::indexed_base(left), Self::indexed_base(right))
                {
                    if left_base.text() == right_base.text() {
                        Self::push(
                            bindings,
                            RingGeneratorBinding {
                                name: left_base.text().to_string(),
                                kind: RingGeneratorKind::IndexedTable,
                                node,
                            },
                        );
                        return;
                    }
                }

                if let Some(names) = Self::symbol_range(node, left, right) {
                    for name in names {
                        Self::push(
                            bindings,
                            RingGeneratorBinding {
                                name,
                                kind: RingGeneratorKind::Direct,
                                node,
                            },
                        );
                    }
                    return;
                }
            }
        }

        if node.kind.is_collection_expression() {
            for element in node.collection_elements() {
                Self::collect_spec(element, bindings);
            }
        }
    }

    fn indexed_base(node: M2Node<'tree>) -> Option<M2Node<'tree>> {
        (node.binary_operator() == Some("_"))
            .then(|| node.child_by_field_name("left"))
            .flatten()
            .filter(|base| base.kind == NodeKind::Symbol)
    }

    fn symbol_range(
        range: M2Node<'tree>,
        left: M2Node<'tree>,
        right: M2Node<'tree>,
    ) -> Option<Vec<String>> {
        if left.kind != NodeKind::Symbol || right.kind != NodeKind::Symbol {
            return None;
        }
        let [start] = left.text().as_bytes() else {
            return None;
        };
        let [end] = right.text().as_bytes() else {
            return None;
        };
        if !start.is_ascii_alphabetic()
            || !end.is_ascii_alphabetic()
            || start.is_ascii_lowercase() != end.is_ascii_lowercase()
            || start > end
        {
            return None;
        }

        let exclusive = range.binary_operator() == Some("..<");
        let stop = if exclusive {
            *end
        } else {
            end.saturating_add(1)
        };
        Some(
            (*start..stop)
                .map(|letter| (letter as char).to_string())
                .collect(),
        )
    }

    fn push(bindings: &mut Vec<RingGeneratorBinding<'tree>>, binding: RingGeneratorBinding<'tree>) {
        if !bindings
            .iter()
            .any(|existing| existing.name == binding.name)
        {
            bindings.push(binding);
        }
    }
}

impl TypeChecker<'_> {
    /// Infer the ring-specific indexed-variable and quotient operator cases.
    pub fn ring_operator_type(
        &self,
        query: OperatorTypeQuery<'_>,
        left_name: &ObjectName,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let promoted_value = parenthesized_value(query.left)?;
        if query.operator == "_"
            && matches!(
                promoted_value.kind,
                NodeKind::IntegerLiteral | NodeKind::FloatLiteral
            )
        {
            let ring = parenthesized_value(query.right?)?;
            let ring_type = self.type_of(ring, source, query.scope_idx, knowledge);
            if knowledge.has_type_role(ring_type.principal()?, TypeRole::Ring) {
                let element_type = self
                    .resolved_ring_element_type(
                        ring,
                        source,
                        query.scope_idx,
                        knowledge,
                        &mut HashSet::new(),
                    )
                    .unwrap_or_else(|| ObjectName::new("RingElement"));
                return Some(InferredType::from_id(element_type));
            }
        }

        if query.operator == "_" && left_name.as_ref() == "IndexedVariableTable" {
            if let Some(element_type) = self
                .get_binding_from_scope(
                    query.left.text(),
                    query.scope_idx,
                    source.position_for_node(query.left),
                )
                .and_then(|binding| binding.state.indexed_element_type.as_ref())
            {
                return Some(InferredType::of(element_type.name()));
            }
            return Some(InferredType::of("RingElement"));
        }

        if query.operator == "/" && knowledge.has_type_role(left_name, TypeRole::Ring) {
            let right_type = self.type_of(query.right?, source, query.scope_idx, knowledge);
            if right_type.principal()?.as_ref() == "ZZ" {
                return Some(InferredType::of("QuotientRing"));
            }
        }

        None
    }

    fn resolved_ring_element_type(
        &self,
        ring: M2Node<'_>,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
        visited: &mut HashSet<BindingId>,
    ) -> Option<ObjectName> {
        let ring = parenthesized_value(ring)?;
        let name = symbol_node_text(ring)?;
        let position = source.position_for_node(ring);
        let Some(binding) =
            self.visible_source_binding_from_scope(name, scope_idx, position, knowledge)
        else {
            let name = ObjectName::new(name);
            self.resolve_type_id_at(&name, position, knowledge)?;
            return Some(name);
        };
        if !visited.insert(binding.binding_id) {
            return None;
        }

        let value_range = binding.state.value_range?;
        let mut root = ring;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let value = root.descendant_for_point_range(
            source.point_for_position(value_range.start)?,
            source.point_for_position(value_range.end)?,
        )?;
        if source.range_for_node(value) != value_range {
            return None;
        }
        let value_scope_idx = self
            .find_scope_at(source.position_for_node(value))
            .unwrap_or(binding.state.scope_idx);
        if symbol_node_text(parenthesized_value(value)?).is_some() {
            return self.resolved_ring_element_type(
                value,
                source,
                value_scope_idx,
                knowledge,
                visited,
            );
        }

        (binding.scope_idx == 0
            && self.expression_introduces_ring_identity(value, source, value_scope_idx, knowledge))
        .then(|| binding.name.clone())
        .filter(|_| binding.state.source_type.is_some())
    }

    fn expression_introduces_ring_identity(
        &self,
        expression: M2Node<'_>,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        let Some(expression) = parenthesized_value(expression) else {
            return false;
        };
        if expression.kind == NodeKind::NewStatement {
            return true;
        }
        let introduces_identity =
            expression.is_space_application() || expression.binary_operator() == Some("/");
        let operand = introduces_identity
            .then(|| expression.child_by_field_name("left"))
            .flatten();
        operand.is_some_and(|operand| {
            self.type_of(operand, source, scope_idx, knowledge)
                .principal()
                .is_some_and(|type_name| knowledge.has_type_role(type_name, TypeRole::Ring))
        })
    }

    /// Square-bracket ring construction binds specially in Macaulay2 source:
    /// `R[x]/I` has a CST shaped like `R SPACE ([x] / I)`, while evaluation
    /// constructs `R[x]` before applying `/ I`. Preserve the parser's grouping,
    /// but lower that one type-directed application chain through the same
    /// dispatch table used for ordinary operators.
    pub fn ring_application_with_trailing_operator_type(
        &self,
        application_operator: &Operator,
        head: &InferredType,
        argument: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let head_name = head.principal()?;
        if !knowledge.has_type_role(head_name, TypeRole::Ring) {
            return None;
        }

        let operator = argument.binary_operator()?;
        let variables = argument.child_by_field_name("left")?;
        if !variables.kind.is_collection_expression() {
            return None;
        }
        let trailing_operand = argument.child_by_field_name("right")?;

        let variables_type = self.type_of(variables, source, scope_idx, knowledge);
        let ring_type = self.dispatch_codomain(
            knowledge,
            application_operator,
            &[head.clone(), variables_type],
            &[],
            source.position_for_node(argument),
        );
        let ring_name = ring_type.principal()?;
        if !knowledge.has_type_role(ring_name, TypeRole::Ring) {
            return None;
        }

        let trailing_type = self.type_of(trailing_operand, source, scope_idx, knowledge);
        let result = knowledge.resolve_call_return_type_with_options(
            &ObjectName::new(operator),
            &[
                self.dispatch_object_id(&ring_type, source.position_for_node(argument), knowledge),
                self.dispatch_object_id(
                    &trailing_type,
                    source.position_for_node(argument),
                    knowledge,
                ),
            ],
            &[],
        )?;
        Some(self.inferred_external_type(result, knowledge))
    }
}
