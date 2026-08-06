//! Transient static type inference for a completed semantic snapshot.

use std::ops::Deref;

use super::*;
use crate::object_registry::TypeStore;

/// Partial-order view combining source and external type edges without copying.
struct SourceTypeOrder<'analysis, Knowledge: ?Sized> {
    source: &'analysis SourceTypeFacts,
    external: &'analysis Knowledge,
}

impl<Knowledge: TypeKnowledge + ?Sized> TypeStore for SourceTypeOrder<'_, Knowledge> {
    fn parent_type_id(&self, type_id: &TypeId) -> Option<TypeId> {
        self.source
            .data
            .get(type_id)
            .and_then(|data| data.parent.clone())
            .or_else(|| {
                self.external
                    .object(type_id.object())?
                    .type_info()
                    .and_then(|data| data.parent.clone())
            })
    }
}

pub struct TypeChecker<'analysis> {
    analysis: &'analysis Analysis,
}

enum TypeSubstitution<'tree> {
    Exact(ObjectName),
    Follow(M2Node<'tree>),
    Union(Vec<Self>),
    Symbol(M2Node<'tree>),
    Dispatch(M2Node<'tree>),
    Unknown,
}

/// One transient operator-inference request passed to specialized rule sets.
#[derive(Debug, Clone, Copy)]
pub struct OperatorTypeQuery<'tree> {
    pub operator: &'tree str,
    pub left: M2Node<'tree>,
    pub right: Option<M2Node<'tree>>,
    pub scope_idx: usize,
}

impl<'analysis> TypeChecker<'analysis> {
    pub fn new(analysis: &'analysis Analysis) -> Self {
        Self { analysis }
    }
}

impl Deref for TypeChecker<'_> {
    type Target = Analysis;

    fn deref(&self) -> &Self::Target {
        self.analysis
    }
}

impl Analysis {
    pub(super) fn enrich_types(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        current_scope_idx: usize,
        assignment_scope_idx: usize,
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        let knowledge = knowledge_provider.at_position(source.position_for_node(node));
        let mut next_scope_idx = current_scope_idx;
        let mut next_assignment_scope_idx = assignment_scope_idx;

        match node.kind {
            NodeKind::LambdaExpression => {
                next_scope_idx = self.registry.scopes.owned_by(node, source);
                next_assignment_scope_idx = next_scope_idx;
                if let Some(parameters) = node.child_by_field_name("parameters") {
                    let parameter_types = method_installation_parameter_types_for_function(node);
                    self.enrich_parameters(parameters, source, parameter_types.as_deref());
                }
            }
            NodeKind::ForStatement => {
                next_scope_idx = self.registry.scopes.owned_by(node, source);
            }
            _ if node.is_assignment() => {
                let left = node.child_by_field_name("left");
                let operator = node.child_by_field_name("operator");
                let right = node.child_by_field_name("right");
                if let (Some(left), Some(operator)) = (left, operator) {
                    self.record_method_installation(node, source, &knowledge);
                    let inferred_type = right.map(|right| {
                        if method_declaration_typical_value(right).is_some()
                            || is_method_call(right)
                        {
                            InferredType::exact_from_id(TypeRole::MethodFunction.object_name())
                        } else {
                            TypeChecker::new(self).type_of(
                                right,
                                source,
                                current_scope_idx,
                                &knowledge,
                            )
                        }
                    });
                    let type_name = inferred_type.as_ref().and_then(InferredType::single);
                    let parent_type = right.and_then(|right| {
                        self.declared_type_parent(
                            right,
                            type_name,
                            source.position_for_node(right),
                            &knowledge,
                        )
                    });
                    let presentation_kind = if right.is_some_and(|right| {
                        right.kind == NodeKind::LambdaExpression
                            || method_declaration_typical_value(right).is_some()
                            || is_method_call(right)
                    }) || type_name.is_some_and(|type_name| {
                        knowledge.has_type_role(type_name, TypeRole::Function)
                    }) {
                        SymbolKind::FUNCTION
                    } else if type_name.is_some_and(|type_name| {
                        TypeChecker::new(self).is_subtype(
                            type_name,
                            &TypeRole::Type.object_name(),
                            source.position_for_node(left),
                            &knowledge,
                        )
                    }) {
                        SymbolKind::CLASS
                    } else {
                        SymbolKind::VARIABLE
                    };
                    let object_id =
                        right
                            .zip(single_symbol_assignment_target(left))
                            .and_then(|(right, _)| {
                                self.callable_object_for_value(
                                    right,
                                    source,
                                    current_scope_idx,
                                    &knowledge,
                                )
                            });

                    if matches!(operator.text(), ":=" | "=") {
                        self.enrich_definitions(
                            left,
                            source,
                            BindingEnrichment {
                                presentation_kind,
                                inferred_type: inferred_type.clone(),
                                object_id,
                                indexed_element_type: None,
                                parent_type,
                            },
                        );
                    }

                    if let (Some(right), Some(type_name), Some(ring_name)) =
                        (right, type_name, single_symbol_assignment_target(left))
                    {
                        if knowledge.has_type_role(type_name, TypeRole::Ring) {
                            self.collect_ring_generator_bindings(
                                ring_name,
                                right,
                                left,
                                current_scope_idx,
                                source,
                                &knowledge,
                            );
                        }
                    }
                }
            }
            _ => {}
        }

        for child in node.children() {
            let (child_scope_idx, child_assignment_scope_idx) =
                match control_flow_scope(node, child) {
                    Some(scope_kind) => {
                        let scope_idx = self.registry.scopes.owned_by(child, source);
                        let assignment_scope_idx =
                            scope_kind.assignment_scope(scope_idx, next_assignment_scope_idx);
                        (scope_idx, assignment_scope_idx)
                    }
                    None => (next_scope_idx, next_assignment_scope_idx),
                };
            self.enrich_types(
                child,
                source,
                child_scope_idx,
                child_assignment_scope_idx,
                knowledge_provider,
            );
        }
    }
}

impl TypeChecker<'_> {
    /// Project call arguments into the validated identities used by dispatch.
    pub fn dispatch_argument_ids(
        &self,
        facts: &CallStaticFacts,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Vec<Option<ObjectId>> {
        facts
            .argument_types
            .iter()
            .map(|inferred| self.dispatch_object_id(inferred, position, knowledge))
            .collect()
    }

    /// Project an inferred source type into the external dispatch hierarchy.
    pub fn dispatch_object_id(
        &self,
        inferred: &InferredType,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<ObjectId> {
        let principal = inferred.single()?;
        let Some(mut current) = self.resolve_source_type_id(principal, position) else {
            return knowledge
                .resolve_type_id(principal)
                .map(|type_id| type_id.object().clone());
        };
        let mut visited = HashSet::new();

        loop {
            if !visited.insert(current.clone()) {
                return None;
            }
            let Some(data) = self.registry.source_types.data.get(&current) else {
                return Some(current.object().clone());
            };
            current.clone_from(data.parent.as_ref()?);
        }
    }

    pub fn inferred_external_type(
        &self,
        type_id: TypeId,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        knowledge
            .type_name(&type_id)
            .cloned()
            .map_or_else(InferredType::unknown, InferredType::upward_from_id)
    }

    pub fn type_of(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        self.evaluate_type_substitution(Self::type_substitution(node), source, scope_idx, knowledge)
    }

    fn type_substitution(node: M2Node<'_>) -> TypeSubstitution<'_> {
        use TypeSubstitution::{Dispatch, Exact, Follow, Symbol, Union, Unknown};

        let exact = |name| Exact(ObjectName::new(name));

        match node.kind {
            NodeKind::LambdaExpression => exact("FunctionClosure"),
            NodeKind::BinaryExpression
                if method_declaration_typical_value(node).is_some() || is_method_call(node) =>
            {
                Exact(TypeRole::MethodFunction.object_name())
            }
            NodeKind::List => exact("List"),
            NodeKind::Array => exact("Array"),
            NodeKind::AngleBarList => exact("AngleBarList"),
            kind if kind.is_sequence() => exact("Sequence"),
            kind if kind.is_nothing_value() => exact("Nothing"),
            NodeKind::ParenthesizedExpression => parenthesized_value(node)
                .map(Follow)
                .unwrap_or_else(|| exact("Nothing")),
            NodeKind::StringLiteral | NodeKind::RawStringLiteral => {
                Exact(TypeRole::String.object_name())
            }
            NodeKind::IntegerLiteral => exact("ZZ"),
            NodeKind::FloatLiteral => exact("RR"),
            NodeKind::QuoteExpression => exact("Symbol"),
            NodeKind::Symbol => Symbol(node),
            _ if node.is_assignment() => node
                .child_by_field_name("right")
                .map(Follow)
                .unwrap_or(Unknown),
            _ if node.is_option_assignment() => exact("Option"),
            NodeKind::BinaryExpression
            | NodeKind::PrefixExpression
            | NodeKind::PostfixExpression => Dispatch(node),
            NodeKind::NewStatement => node
                .child_by_field_name("type")
                .filter(|type_node| type_node.kind == NodeKind::Symbol)
                .map(|type_node| Exact(ObjectName::new(type_node.text())))
                .unwrap_or(Unknown),
            NodeKind::IfStatement => {
                let then_value = clause_of(node, NodeKind::ThenClause)
                    .and_then(clause_value)
                    .map(Follow)
                    .unwrap_or(Unknown);
                let else_value = clause_of(node, NodeKind::ElseClause)
                    .and_then(clause_value)
                    .map(Follow)
                    .unwrap_or_else(|| exact("Nothing"));
                Union(vec![then_value, else_value])
            }
            NodeKind::TryStatement => {
                let body = node
                    .named_children()
                    .find(|child| !child.kind.is_try_clause());
                let success = clause_of(node, NodeKind::ThenClause)
                    .and_then(clause_value)
                    .or(body)
                    .map(Follow)
                    .unwrap_or(Unknown);
                let failure = clause_of(node, NodeKind::ElseClause)
                    .or_else(|| clause_of(node, NodeKind::DoClause))
                    .and_then(clause_value)
                    .map(Follow)
                    .unwrap_or_else(|| exact("Nothing"));
                Union(vec![success, failure])
            }
            NodeKind::ForStatement => {
                if node
                    .named_children()
                    .any(|child| child.kind == NodeKind::ListClause)
                {
                    exact("List")
                } else {
                    exact("Nothing")
                }
            }
            NodeKind::WhileStatement => exact("Nothing"),
            kind if kind.is_control_transfer() => node
                .named_children()
                .next()
                .map(Follow)
                .unwrap_or_else(|| exact("Nothing")),
            NodeKind::DebugClause => node.named_children().next().map(Follow).unwrap_or(Unknown),
            _ => Unknown,
        }
    }

    fn evaluate_type_substitution(
        &self,
        substitution: TypeSubstitution<'_>,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        match substitution {
            TypeSubstitution::Exact(type_name) => InferredType::exact_from_id(type_name),
            TypeSubstitution::Follow(node) => self.type_of(node, source, scope_idx, knowledge),
            TypeSubstitution::Union(substitutions) => substitutions.into_iter().fold(
                InferredType::diverges(),
                |inferred, substitution| {
                    inferred.join(
                        self.evaluate_type_substitution(substitution, source, scope_idx, knowledge),
                        knowledge,
                    )
                },
            ),
            TypeSubstitution::Symbol(node) => self.symbol_type(node, source, scope_idx, knowledge),
            TypeSubstitution::Dispatch(node) => match node.kind {
                NodeKind::BinaryExpression => {
                    self.binary_expression_type(node, source, scope_idx, knowledge)
                }
                NodeKind::PrefixExpression | NodeKind::PostfixExpression => {
                    self.unary_operator_type(node, source, scope_idx, knowledge)
                }
                _ => InferredType::unknown(),
            },
            TypeSubstitution::Unknown => InferredType::unknown(),
        }
    }

    /// A symbol's type, in precedence order: an in-scope user binding (which
    /// overrides an earlier visible unprotected name), then the visible object's
    /// recorded class, then `Symbol` (an
    /// unbound name evaluates to its own `Symbol` in M2).
    fn symbol_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let name = node.text();
        if let Some(binding) = self.visible_source_binding_from_scope(
            name,
            scope_idx,
            source.position_for_node(node),
            knowledge,
        ) {
            return binding
                .state
                .inferred_type
                .as_ref()
                .cloned()
                .unwrap_or_else(InferredType::unknown);
        }

        if let Some(reference) = OutputReference::parse(name) {
            return self.output_reference_type(node, reference, source, knowledge);
        }

        if let Some(record) = knowledge.get_record(&ObjectName::new(name)) {
            return InferredType::exact_from_id(record.class.clone());
        }

        InferredType::exact("Symbol")
    }

    fn output_reference_type(
        &self,
        node: M2Node,
        reference: OutputReference,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let Some(output) = reference.referenced_value(node) else {
            return InferredType::exact("Symbol");
        };
        let output_scope = self
            .find_scope_at(source.position_for_node(output))
            .unwrap_or(0);
        self.type_of(output, source, output_scope, knowledge)
    }

    /// A binary expression's type. Juxtaposition `a SPACE b` is application
    /// (handled by [`Self::application_type`]); the function-dependent operators
    /// `_` (currying) and `@@` (composition) are computed here when their
    /// function-position operand is a `Function`; everything else dispatches
    /// through the M2 type table.
    fn binary_expression_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        if node.is_space_application() {
            return self.application_type(node, source, scope_idx, knowledge);
        }

        let operator = node.binary_operator();
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");

        if let Some(operator) = operator {
            if let Some(result) =
                self.special_operator_type(operator, left, right, source, scope_idx, knowledge)
            {
                return result;
            }
        }

        let (Some(left), Some(right), Some(operator)) =
            (left, right, Operator::from_expression(node))
        else {
            return InferredType::unknown();
        };
        let left_type = self.type_of(left, source, scope_idx, knowledge);
        let right_type = self.type_of(right, source, scope_idx, knowledge);
        let position = source.position_for_node(node);
        let arguments = [left_type, right_type];
        if is_comparison_operator(operator.token.name()) {
            return self
                .resolved_dispatch_codomain(knowledge, &operator, &arguments, &[], position)
                .unwrap_or_else(|| InferredType::exact_from_id(TypeRole::Boolean.object_name()));
        }
        self.dispatch_codomain(knowledge, &operator, &arguments, &[], position)
    }

    /// The function-dependent operators, whose result depends on the specific
    /// function value (M2 has no dependent types, so the M2 table cannot express
    /// them): currying `f _ x` (`f_x(y) := f(x, y)`) and composition `f @@ g`
    /// both yield a `FunctionClosure` when the function-position operand is a
    /// `Function`. `None` falls through to ordinary dispatch (so `M_i`, `L_i`,
    /// … keep their table behavior).
    fn special_operator_type(
        &self,
        operator: &str,
        left: Option<M2Node>,
        right: Option<M2Node>,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let left = left?;
        let left_type = self.type_of(left, source, scope_idx, knowledge);
        let left_name = left_type.single()?;

        if operator == "_" && left_name.as_ref() == "Symbol" {
            return Some(InferredType::exact("IndexedVariable"));
        }

        let query = OperatorTypeQuery {
            operator,
            left,
            right,
            scope_idx,
        };
        if let Some(result) = self.ring_operator_type(query, left_name, source, knowledge) {
            return Some(result);
        }

        if matches!(operator, "_" | "@@") && knowledge.has_type_role(left_name, TypeRole::Function)
        {
            return Some(InferredType::exact("FunctionClosure"));
        }

        None
    }

    /// Application `f SPACE x`. A `Function` head delegates to the head's own
    /// signatures available in the current object environment,
    /// stepping beyond the M2 table whose `(Function, Thing)` row only yields
    /// `Thing`. A non-`Function` head dispatches `SPACE` through the table
    /// (`Ring × Array → PolynomialRing`).
    fn application_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let (Some(callable_node), Some(argument_node)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) else {
            return InferredType::unknown();
        };
        let Some(operator) = Operator::from_expression(node) else {
            return InferredType::unknown();
        };
        let callable_name = symbol_node_text(callable_node);
        let call_facts = self.infer_call_facts_for_callable(
            argument_node,
            source,
            scope_idx,
            callable_name,
            knowledge,
        );

        // A locally-defined function is known to be a function from the registry
        // alone, so its application resolves without external type facts: its
        // signatures give the codomain, and an undocumented one yields `Thing`
        // (applying a function gives at least a Thing).
        if let Some(callable) = callable_name {
            let position = source.position_for_node(callable_node);
            if let Some(function) = self.local_function_at(callable, scope_idx, position, knowledge)
            {
                return self
                    .resolve_local_call_return_type(
                        function,
                        &call_facts.argument_types,
                        position,
                        knowledge,
                    )
                    .unwrap_or_else(InferredType::unknown);
            }
            if callable == "error"
                && self
                    .visible_source_binding_from_scope(callable, scope_idx, position, knowledge)
                    .is_none()
                && knowledge.get_record(&ObjectName::new(callable)).is_some()
            {
                return InferredType::diverges();
            }
        }

        // Otherwise the lattice decides whether the head is a function (delegating
        // to its signatures) or another SPACE method (`Ring × Array →
        // PolynomialRing`).
        let head = self.type_of(callable_node, source, scope_idx, knowledge);
        let head_is_function = head
            .single()
            .is_some_and(|head| knowledge.has_type_role(head, TypeRole::Function));
        if head_is_function {
            if let Some(callable) = callable_name {
                if let Some(return_type) = self.resolve_external_call_return_type(
                    &ObjectName::new(callable),
                    &call_facts.argument_types,
                    &call_facts.literal_options,
                    source.position_for_node(node),
                    knowledge,
                ) {
                    return return_type;
                }
            }
            // Applying a function yields at least a Thing.
            return InferredType::unknown();
        }

        if let Some(result) = self.ring_application_with_trailing_operator_type(
            &operator,
            &head,
            argument_node,
            source,
            scope_idx,
            knowledge,
        ) {
            return result;
        }

        let argument_type = self.type_of(argument_node, source, scope_idx, knowledge);
        self.dispatch_codomain(
            knowledge,
            &operator,
            &[head, argument_type],
            &call_facts.literal_options,
            source.position_for_node(node),
        )
    }

    /// Whether `name` resolves to a function tracked in the local registry — a
    /// lambda binding or a local method declaration. Such a head is known to be a
    /// function without consulting external type facts.
    fn local_function_at(
        &self,
        name: &str,
        scope_idx: usize,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<&FunctionInfo> {
        let binding = self.get_binding_from_scope(name, scope_idx, position)?;
        if binding.scope_idx == 0
            && knowledge.shadows_source(&binding.name, binding.state.span.start)
        {
            return None;
        }
        self.function_for_binding(binding)
    }

    /// A prefix/postfix operator's type: `typicalValue(op, operand)`.
    fn unary_operator_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let (Some(operator), Some(operand)) = (
            Operator::from_expression(node),
            node.child_by_field_name("operand"),
        ) else {
            return InferredType::unknown();
        };
        let operand_type = self.type_of(operand, source, scope_idx, knowledge);
        self.dispatch_codomain(
            knowledge,
            &operator,
            &[operand_type],
            &[],
            source.position_for_node(node),
        )
    }

    /// Dispatch `callable` on `args` through the M2 type table. A matched but
    /// undocumented codomain is `Thing` (≡ a null `typicalValue` under the
    /// lower-bound reading) — approximated by "the callable/operator resolves to
    /// a known object, so it dispatches"; an unidentifiable head stays `Unknown`.
    pub fn dispatch_codomain(
        &self,
        knowledge: &(impl TypeKnowledge + ?Sized),
        operator: &Operator,
        args: &[InferredType],
        options: &[LiteralOption],
        position: Position,
    ) -> InferredType {
        if let Some(return_type) =
            self.resolved_dispatch_codomain(knowledge, operator, args, options, position)
        {
            return return_type;
        }
        if knowledge.get_record(&operator.token).is_some() {
            return InferredType::unknown();
        }
        InferredType::unknown()
    }

    fn resolved_dispatch_codomain(
        &self,
        knowledge: &(impl TypeKnowledge + ?Sized),
        operator: &Operator,
        args: &[InferredType],
        options: &[LiteralOption],
        position: Position,
    ) -> Option<InferredType> {
        if let Some(function) = self.registry.operator_functions.get(operator) {
            if let Some(return_type) =
                self.resolve_local_call_return_type(function, args, position, knowledge)
            {
                return Some(return_type);
            }
        }
        self.resolve_external_call_return_type(&operator.token, args, options, position, knowledge)
    }

    fn resolve_external_call_return_type(
        &self,
        callable: &ObjectName,
        argument_types: &[InferredType],
        _options: &[LiteralOption],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let callable = knowledge.get_record(callable)?.callable()?;
        let mut result = InferredType::diverges();
        let mut matched = false;

        if let Some(products) = exact_type_products(argument_types) {
            for arguments in products {
                let candidates = minimal_candidates(
                    callable
                        .methods
                        .iter()
                        .filter(|method| {
                            self.external_domain_matches_exact(
                                &method.domain,
                                &arguments,
                                position,
                                knowledge,
                            )
                        })
                        .collect(),
                    |other, candidate| {
                        knowledge.domain_strictly_smaller(&other.domain, &candidate.domain)
                    },
                );
                if !candidates.is_empty() {
                    matched = true;
                    result = result.join(
                        self.external_candidates_return_type(callable, &candidates, knowledge),
                        knowledge,
                    );
                }
            }
        } else {
            let candidates = minimal_candidates(
                callable
                    .methods
                    .iter()
                    .filter(|method| {
                        self.external_signature_matches(method, argument_types, position, knowledge)
                    })
                    .collect(),
                |other, candidate| {
                    knowledge.domain_strictly_smaller(&other.domain, &candidate.domain)
                },
            );
            if !candidates.is_empty() {
                matched = true;
                result = self.external_candidates_return_type(callable, &candidates, knowledge);
            }
        }

        if matched {
            Some(result)
        } else {
            callable
                .typical_value
                .clone()
                .map(|codomain| self.inferred_external_type(codomain, knowledge))
        }
    }

    fn external_candidates_return_type(
        &self,
        callable: &crate::builtin_index::CallableInfo,
        candidates: &[&crate::builtin_index::MethodSignature],
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let codomains = candidates
            .iter()
            .filter_map(|method| callable.effective_codomain(method))
            .collect::<Vec<_>>();
        let has_specialized = codomains.iter().any(|(_, specialized)| *specialized);
        let mut selected = codomains
            .into_iter()
            .filter(|(_, specialized)| !has_specialized || *specialized)
            .peekable();
        if selected.peek().is_none() {
            return InferredType::unknown();
        }
        selected.fold(InferredType::diverges(), |result, (codomain, _)| {
            result.join(
                self.inferred_external_type(codomain.clone(), knowledge),
                knowledge,
            )
        })
    }

    fn external_signature_matches(
        &self,
        signature: &crate::builtin_index::MethodSignature,
        argument_types: &[InferredType],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        signature.domain.len() == argument_types.len()
            && signature
                .domain
                .iter()
                .zip(argument_types)
                .all(|(expected, actual)| {
                    actual
                        .exact_points()
                        .chain(actual.upward_generators())
                        .any(|actual| {
                            self.external_dispatch_matches(actual, expected, position, knowledge)
                        })
                })
    }

    fn external_domain_matches_exact(
        &self,
        expected_domain: &[ObjectId],
        actual_domain: &[&ObjectName],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        expected_domain.len() == actual_domain.len()
            && expected_domain
                .iter()
                .zip(actual_domain)
                .all(|(expected, actual)| {
                    self.external_dispatch_matches(actual, expected, position, knowledge)
                })
    }

    fn external_dispatch_matches(
        &self,
        actual: &ObjectName,
        expected: &ObjectId,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        let Some(actual) = self.resolve_type_id_at(actual, position, knowledge) else {
            return false;
        };
        if actual.object() == expected {
            return true;
        }
        let Some(expected) = knowledge.type_id(expected) else {
            return false;
        };
        SourceTypeOrder {
            source: &self.registry.source_types,
            external: knowledge,
        }
        .is_subtype_id(&actual, &expected)
    }

    pub fn infer_call_facts(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> CallStaticFacts {
        self.infer_call_facts_for_callable(node, source, scope_idx, None, knowledge)
    }

    fn infer_call_facts_for_callable(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        callable: Option<&str>,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> CallStaticFacts {
        // A single parenthesized argument `f(x)` / `f(opt => v)` denotes its inner
        // value; peel it so the argument is classified like a bare argument.
        let node = parenthesized_value(node).unwrap_or(node);
        let receives_sequence = callable.is_some_and(|name| {
            self.callable_receives_sequence(
                name,
                scope_idx,
                source.position_for_node(node),
                knowledge,
            )
        });
        if node.kind == NodeKind::Sequence && !receives_sequence {
            let mut facts = CallStaticFacts::default();
            for child in node.collection_elements() {
                if let Some(option) = literal_option_assignment(child) {
                    facts.literal_options.push(option);
                } else {
                    facts
                        .argument_types
                        .push(self.type_of(child, source, scope_idx, knowledge));
                }
            }
            return facts;
        }

        if let Some(option) = literal_option_assignment(node) {
            return CallStaticFacts {
                argument_types: Vec::new(),
                literal_options: vec![option],
            };
        }

        CallStaticFacts {
            argument_types: vec![self.type_of(node, source, scope_idx, knowledge)],
            literal_options: Vec::new(),
        }
    }

    fn callable_receives_sequence(
        &self,
        name: &str,
        scope_idx: usize,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        let local_dispatch = self
            .local_function_at(name, scope_idx, position, knowledge)
            .and_then(|function| function.dispatch);
        if local_dispatch == Some(Dispatch::Variadic) {
            return true;
        }

        knowledge
            .get_record(&ObjectName::new(name))
            .and_then(|record| record.callable())
            .is_some_and(|callable| callable.receives_sequence)
    }

    fn resolve_local_call_return_type(
        &self,
        function: &FunctionInfo,
        argument_types: &[InferredType],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let methods = self.methods_for_at(function, position);
        let mut result = InferredType::diverges();
        let mut matched = false;

        if let Some(products) = exact_type_products(argument_types) {
            for arguments in products {
                let candidates = minimal_candidates(
                    methods
                        .iter()
                        .copied()
                        .filter(|method| {
                            self.domain_matches_exact(
                                &method.domain,
                                &arguments,
                                position,
                                knowledge,
                            )
                        })
                        .collect(),
                    |other, candidate| {
                        self.method_domain_strictly_smaller(
                            &other.domain,
                            &candidate.domain,
                            position,
                            knowledge,
                        )
                    },
                );
                for method in candidates {
                    matched = true;
                    let codomain = method.codomain.as_ref().or(function.typical_value.as_ref());
                    result = result.join(
                        codomain
                            .cloned()
                            .map_or_else(InferredType::unknown, InferredType::upward_from_id),
                        knowledge,
                    );
                }
            }
        } else {
            let candidates = minimal_candidates(
                methods
                    .into_iter()
                    .filter(|method| {
                        self.signature_matches(method, argument_types, position, knowledge)
                    })
                    .collect(),
                |other, candidate| {
                    self.method_domain_strictly_smaller(
                        &other.domain,
                        &candidate.domain,
                        position,
                        knowledge,
                    )
                },
            );
            for method in candidates {
                matched = true;
                let codomain = method.codomain.as_ref().or(function.typical_value.as_ref());
                result = result.join(
                    codomain
                        .cloned()
                        .map_or_else(InferredType::unknown, InferredType::upward_from_id),
                    knowledge,
                );
            }
        }

        if matched {
            Some(result)
        } else {
            function
                .typical_value
                .clone()
                .map(InferredType::upward_from_id)
        }
    }

    pub fn local_call_parameter_names(
        &self,
        function: &FunctionInfo,
        argument_types: &[InferredType],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<Vec<ObjectName>> {
        match function.kind {
            LocalFunctionKind::Plain => {
                let Dispatch::Fixed(arity) = function.dispatch? else {
                    return None;
                };
                (arity == argument_types.len())
                    .then(|| function.parameter_names.clone())
                    .flatten()
            }
            LocalFunctionKind::Method => {
                let dispatched = minimal_candidates(
                    self.methods_for_at(function, position)
                        .into_iter()
                        .filter(|method| {
                            self.signature_matches(method, argument_types, position, knowledge)
                        })
                        .collect(),
                    |other, candidate| {
                        self.method_domain_strictly_smaller(
                            &other.domain,
                            &candidate.domain,
                            position,
                            knowledge,
                        )
                    },
                );
                let [method] = dispatched.as_slice() else {
                    return None;
                };
                method
                    .parameter_names
                    .as_ref()
                    .filter(|names| names.len() == argument_types.len())
                    .cloned()
            }
        }
    }

    fn method_domain_strictly_smaller(
        &self,
        smaller: &[ObjectName],
        bigger: &[ObjectName],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        if smaller.len() != bigger.len() {
            return false;
        }
        let mut strict = false;
        for (smaller, bigger) in smaller.iter().zip(bigger) {
            if smaller == bigger {
                continue;
            }
            if !self.is_subtype(smaller, bigger, position, knowledge) {
                return false;
            }
            strict = true;
        }
        strict
    }

    fn signature_matches(
        &self,
        signature: &Method,
        argument_types: &[InferredType],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        self.signature_matches_domain(&signature.domain, argument_types, position, knowledge)
    }

    fn signature_matches_domain(
        &self,
        expected_domain: &[ObjectName],
        argument_types: &[InferredType],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        expected_domain.len() == argument_types.len()
            && expected_domain
                .iter()
                .zip(argument_types)
                .all(|(expected, actual)| {
                    actual
                        .exact_points()
                        .chain(actual.upward_generators())
                        .any(|actual| self.is_subtype(actual, expected, position, knowledge))
                })
    }

    fn domain_matches_exact(
        &self,
        expected_domain: &[ObjectName],
        actual_domain: &[&ObjectName],
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        expected_domain.len() == actual_domain.len()
            && expected_domain
                .iter()
                .zip(actual_domain)
                .all(|(expected, actual)| self.is_subtype(actual, expected, position, knowledge))
    }

    pub fn is_subtype(
        &self,
        actual: &ObjectName,
        expected: &ObjectName,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        self.subtype_evidence(actual, expected, position, knowledge) == SubtypeEvidence::Proven
    }

    pub fn subtype_evidence(
        &self,
        actual: &ObjectName,
        expected: &ObjectName,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> SubtypeEvidence {
        let order = SourceTypeOrder {
            source: &self.registry.source_types,
            external: knowledge,
        };
        let Some((actual, expected)) = self
            .resolve_type_id_at(actual, position, knowledge)
            .zip(self.resolve_type_id_at(expected, position, knowledge))
        else {
            return SubtypeEvidence::Unknown;
        };
        if order.is_subtype_id(&actual, &expected) {
            SubtypeEvidence::Proven
        } else {
            SubtypeEvidence::Disproven
        }
    }

    fn resolve_source_type_id(&self, name: &ObjectName, position: Position) -> Option<TypeId> {
        self.get_binding_at(name.name(), position)?
            .state
            .source_type
            .clone()
    }

    pub fn resolve_type_id_at(
        &self,
        name: &ObjectName,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<TypeId> {
        self.resolve_source_type_id(name, position)
            .or_else(|| knowledge.resolve_type_id(name))
    }
}

fn is_comparison_operator(operator: &str) -> bool {
    matches!(
        operator,
        "==" | "===" | "!=" | "=!=" | "<" | "<=" | ">" | ">="
    )
}

fn minimal_candidates<T: Copy>(
    mut candidates: Vec<T>,
    strictly_smaller: impl Fn(T, T) -> bool,
) -> Vec<T> {
    let originals = candidates.clone();
    candidates.retain(|candidate| {
        !originals
            .iter()
            .copied()
            .any(|other| strictly_smaller(other, *candidate))
    });
    candidates
}

const MAX_EXACT_TYPE_PRODUCTS: usize = 256;

fn exact_type_products(arguments: &[InferredType]) -> Option<Vec<Vec<&ObjectName>>> {
    let mut products = vec![Vec::new()];
    for argument in arguments {
        if argument.upward_generators().next().is_some() {
            return None;
        }
        let points = argument.exact_points().collect::<Vec<_>>();
        if points.is_empty() {
            return Some(Vec::new());
        }
        let product_count = products.len().checked_mul(points.len())?;
        if product_count > MAX_EXACT_TYPE_PRODUCTS {
            return None;
        }
        products = products
            .into_iter()
            .flat_map(|product| {
                points.iter().map(move |point| {
                    let mut product = product.clone();
                    product.push(*point);
                    product
                })
            })
            .collect();
    }
    Some(products)
}
