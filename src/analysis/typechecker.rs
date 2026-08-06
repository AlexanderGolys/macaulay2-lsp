//! Transient static type inference for a completed semantic snapshot.

use std::cell::RefCell;
use std::collections::HashMap;
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

/// Identity of one expression within a transient inference run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NodeFactId(usize);

/// Type inference state borrowed from an immutable analysis snapshot.
///
/// The cache belongs only to this checker and is discarded with it; no
/// expression-level intermediate facts are retained in the semantic registry.
pub struct TypeChecker<'analysis> {
    analysis: &'analysis Analysis,
    type_cache: RefCell<HashMap<NodeFactId, InferredType>>,
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
        Self {
            analysis,
            type_cache: RefCell::new(HashMap::new()),
        }
    }
}

impl Deref for TypeChecker<'_> {
    type Target = Analysis;

    fn deref(&self) -> &Self::Target {
        self.analysis
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
        let principal = inferred.principal()?;
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
            .map_or_else(InferredType::unknown, InferredType::from_id)
    }

    /// The inferred type of the value `node` evaluates to — see [`InferredType`].
    /// Every value-producing node has a type; control-flow and unhandled forms
    /// fall to `Unknown`. The bound is a lower bound (a `typicalValue`), never
    /// asserted exact.
    pub fn type_of(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let node_id = NodeFactId(node.id());
        if let Some(inferred) = self.type_cache.borrow().get(&node_id) {
            return inferred.clone();
        }

        let inferred = self.compute_type_of(node, source, scope_idx, knowledge);
        self.type_cache
            .borrow_mut()
            .insert(node_id, inferred.clone());
        inferred
    }

    fn compute_type_of(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        match node.kind {
            // A lambda's class is the concrete `FunctionClosure`, not the abstract
            // `Function` — this distinction drives the method-install no-effect rule
            // (a `FunctionClosure` is not a method function).
            NodeKind::LambdaExpression => InferredType::of("FunctionClosure"),
            NodeKind::BinaryExpression
                if method_declaration_typical_value(node).is_some() || is_method_call(node) =>
            {
                InferredType::from_id(TypeRole::MethodFunction.object_name())
            }
            NodeKind::List => InferredType::of("List"),
            NodeKind::Array => InferredType::of("Array"),
            NodeKind::AngleBarList => InferredType::of("AngleBarList"),
            kind if kind.is_sequence() => InferredType::of("Sequence"),
            kind if kind.is_nothing_value() => InferredType::of("Nothing"),
            // A parenthesized expression is its final unmuted value: `(1)` is
            // `ZZ`, `(a;b)` is the type of `b`. With no final value (`(a;)`) it
            // evaluates to `null`, whose class is `Nothing`.
            NodeKind::ParenthesizedExpression => match parenthesized_value(node) {
                Some(inner) => self.type_of(inner, source, scope_idx, knowledge),
                None => InferredType::of("Nothing"),
            },
            NodeKind::StringLiteral | NodeKind::RawStringLiteral => {
                InferredType::from_id(TypeRole::String.object_name())
            }
            NodeKind::IntegerLiteral => InferredType::of("ZZ"),
            NodeKind::FloatLiteral => InferredType::of("RR"),
            // A quote expression (`symbol +`, `local x`, `global y`,
            // `threadLocal z`) evaluates to the Symbol it names.
            NodeKind::QuoteExpression => InferredType::of("Symbol"),
            NodeKind::Symbol => self.symbol_type(node, source, scope_idx, knowledge),
            // An assignment evaluates to its right-hand side: `a = b` / `a := b`
            // (and destructuring `{x,y} := …`) take the type of the RHS.
            _ if node.is_assignment() => match node.child_by_field_name("right") {
                Some(right) => self.type_of(right, source, scope_idx, knowledge),
                None => InferredType::unknown(),
            },
            // `x => y` builds an `Option` object, whatever the operand types.
            _ if node.is_option_assignment() => InferredType::of("Option"),
            NodeKind::BinaryExpression => {
                self.binary_expression_type(node, source, scope_idx, knowledge)
            }
            NodeKind::PrefixExpression | NodeKind::PostfixExpression => {
                self.unary_operator_type(node, source, scope_idx, knowledge)
            }
            NodeKind::NewStatement => node
                .child_by_field_name("type")
                .filter(|type_node| type_node.kind == NodeKind::Symbol)
                .map(|type_node| InferredType::of(type_node.text()))
                .unwrap_or_else(InferredType::unknown),
            // `if c then A [else B]` is whichever branch runs; with no `else`,
            // a false condition yields `null` (`Nothing`). The static type is the
            // join of the reachable branch types.
            NodeKind::IfStatement => self.if_statement_type(node, source, scope_idx, knowledge),
            // `try E [then A] [else B | except e do B]` is the success value
            // (`then A`, else `E`) joined with the failure value (`else`/`do B`,
            // else `null` since an unhandled error makes `try` evaluate to null).
            NodeKind::TryStatement => self.try_statement_type(node, source, scope_idx, knowledge),
            // A `for … list …` collects a `List`; a `for … do …` loop (and every
            // `while` loop) evaluates to `null` (`Nothing`).
            NodeKind::ForStatement => {
                if node
                    .named_children()
                    .any(|child| child.kind == NodeKind::ListClause)
                {
                    InferredType::of("List")
                } else {
                    InferredType::of("Nothing")
                }
            }
            NodeKind::WhileStatement => InferredType::of("Nothing"),
            // A control transfer evaluates (for the loop/function it escapes) to
            // its operand, or `null` (`Nothing`) when bare.
            kind if kind.is_control_transfer() => {
                self.control_transfer_type(node, source, scope_idx, knowledge)
            }
            // A debug clause (`time E`, `break v`, …) passes through to the value
            // of its inner statement/expression.
            NodeKind::DebugClause => node
                .named_children()
                .next()
                .map(|inner| self.type_of(inner, source, scope_idx, knowledge))
                .unwrap_or_else(InferredType::unknown),
            _ => InferredType::unknown(),
        }
    }

    /// The type of an `if` statement: the join of its `then` branch with its
    /// `else` branch, where a missing `else` contributes `Nothing` (a false
    /// condition makes the whole expression `null`).
    fn if_statement_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let then_type = clause_of(node, NodeKind::ThenClause)
            .and_then(clause_value)
            .map(|value| self.type_of(value, source, scope_idx, knowledge))
            .unwrap_or_else(InferredType::unknown);
        let else_type = match clause_of(node, NodeKind::ElseClause).and_then(clause_value) {
            Some(value) => self.type_of(value, source, scope_idx, knowledge),
            None => InferredType::of("Nothing"),
        };
        then_type.join(else_type, knowledge)
    }

    /// The type of a `try` statement: the success value (`then` clause if present,
    /// else the guarded body) joined with the failure value (`else`/`do` clause if
    /// present, else `Nothing` since an unhandled error makes `try` yield `null`).
    fn try_statement_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let body = node
            .named_children()
            .find(|child| !is_try_clause(child.kind));
        let success_value = clause_of(node, NodeKind::ThenClause)
            .and_then(clause_value)
            .or(body);
        let success = success_value
            .map(|value| self.type_of(value, source, scope_idx, knowledge))
            .unwrap_or_else(InferredType::unknown);
        let failure_value = clause_of(node, NodeKind::ElseClause)
            .or_else(|| clause_of(node, NodeKind::DoClause))
            .and_then(clause_value);
        let failure = match failure_value {
            Some(value) => self.type_of(value, source, scope_idx, knowledge),
            None => InferredType::of("Nothing"),
        };
        success.join(failure, knowledge)
    }

    /// The type of a control transfer (`return e` / `break e` / `continue e`):
    /// its operand's type, or `Nothing` when the transfer is bare.
    fn control_transfer_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        match node.named_children().next() {
            Some(operand) => self.type_of(operand, source, scope_idx, knowledge),
            None => InferredType::of("Nothing"),
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
                .type_name
                .as_ref()
                .map_or_else(InferredType::unknown, |type_name| {
                    InferredType::from_id(type_name.clone())
                });
        }

        if let Some(reference) = OutputReference::parse(name) {
            return self.output_reference_type(node, reference, source, knowledge);
        }

        if let Some(record) = knowledge.get_record(&ObjectName::new(name)) {
            return InferredType::from_id(record.class.clone());
        }

        InferredType::of("Symbol")
    }

    fn output_reference_type(
        &self,
        node: M2Node,
        reference: OutputReference,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let Some(output) = reference.referenced_value(node) else {
            return InferredType::of("Symbol");
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
                .unwrap_or_else(|| InferredType::from_id(TypeRole::Boolean.object_name()));
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
        let left_name = left_type.principal()?;

        if operator == "_" && left_name.as_ref() == "Symbol" {
            return Some(InferredType::of("IndexedVariable"));
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
            return Some(InferredType::of("FunctionClosure"));
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
                    .map_or_else(|| InferredType::of("Thing"), InferredType::from_id);
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
            .principal()
            .is_some_and(|head| knowledge.has_type_role(head, TypeRole::Function));
        if head_is_function {
            if let Some(callable) = callable_name {
                if let Some(return_type) = knowledge.resolve_call_return_type_with_options(
                    &ObjectName::new(callable),
                    &self.dispatch_argument_ids(
                        &call_facts,
                        source.position_for_node(node),
                        knowledge,
                    ),
                    &call_facts.literal_options,
                ) {
                    return self.inferred_external_type(return_type, knowledge);
                }
            }
            // Applying a function yields at least a Thing.
            return InferredType::of("Thing");
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
            return InferredType::of("Thing");
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
                return Some(InferredType::from_id(return_type));
            }
        }
        if let Some(return_type) = knowledge.resolve_call_return_type_with_options(
            &operator.token,
            &args
                .iter()
                .map(|argument| self.dispatch_object_id(argument, position, knowledge))
                .collect::<Vec<_>>(),
            options,
        ) {
            return Some(self.inferred_external_type(return_type, knowledge));
        }
        None
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
    ) -> Option<ObjectName> {
        let matching_codomains = self
            .methods_for_at(function, position)
            .into_iter()
            .filter(|signature| {
                self.signature_matches(signature, argument_types, position, knowledge)
            })
            .filter_map(|signature| signature.codomain.as_ref())
            .cloned()
            .collect::<HashSet<_>>();

        if matching_codomains.len() == 1 {
            return matching_codomains.into_iter().next();
        }

        function.typical_value.clone()
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
                let candidates = self
                    .methods_for_at(function, position)
                    .into_iter()
                    .filter(|method| {
                        self.signature_matches(method, argument_types, position, knowledge)
                    })
                    .collect::<Vec<_>>();
                let dispatched = candidates
                    .iter()
                    .copied()
                    .filter(|candidate| {
                        !candidates.iter().copied().any(|other| {
                            self.method_domain_strictly_smaller(
                                &other.domain,
                                &candidate.domain,
                                position,
                                knowledge,
                            )
                        })
                    })
                    .collect::<Vec<_>>();
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
                    actual.principal().is_some_and(|actual| {
                        self.is_subtype(actual, expected, position, knowledge)
                    })
                })
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
