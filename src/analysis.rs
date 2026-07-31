//! Parse-tree analysis that records lexical bindings, static type facts, and
//! diagnostics for one document snapshot.

mod ring;
mod typechecker;

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use tower_lsp::lsp_types::{Diagnostic, Position, Range as TextRange, SymbolKind};

use crate::diagnostic_registry::M2Diagnostic;
use crate::meta::{BindingRole, Meta, Metadata};
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::object_registry::ObjectName;
use crate::object_registry::{ObjectId, OperatorForm, TypeData, TypeId};
use crate::source::SourceNavigation;
use crate::typesystem::{InferredType, LiteralOption, PositionedTypeKnowledge, TypeKnowledge};
use crate::util::position_in_range;
use typechecker::TypeChecker;

/// Identity of one lexical declaration within an immutable analysis snapshot.
///
/// Reassigning the declared name preserves this identity and adds a new
/// [`BindingStateId`]; declaring a shadowing name creates a new `BindingId`.
/// The identity indexes [`SemanticRegistry::bindings`] and connects the
/// declaration to all of its source-ordered states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingId(u32);

/// Identity of one source-ordered state of a binding within an analysis snapshot.
///
/// The declaration creates the first state, and each reassignment creates
/// another state containing the value, symbol kind, and inferred type effective
/// from that source position. The identity indexes
/// [`SemanticRegistry::binding_states`] and refers back to one [`BindingId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingStateId(u32);

/// An M2 operator — including `SPACE`, the juxtaposition operator (`X Y` is
/// `X SPACE Y`). Just another operator, not a special "adjacency" concept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operator {
    pub token: ObjectName,
    pub form: OperatorForm,
}

/// Juxtaposition's operator token, e.g. the `SPACE` in `(SPACE, Ring, Array)`.
pub const SPACE_OPERATOR: &str = "SPACE";

/// The callable or operator receiving an installed method.
/// Installation syntax affects the installed function's arity, not the
/// identity of this head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodHead {
    Function(ObjectName),
    Operator(Operator),
}

/// Identity of one method-installing assignment within an analysis snapshot.
///
/// The identity indexes [`SemanticRegistry::installations`], allowing callable
/// records to refer directly to the source fact without a second method index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodInstallationId(usize);

/// Source-characterized method signature.
///
/// Domain and codomain names remain nominal here because local bindings and
/// package inclusions make their resolution source-position dependent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub head: MethodHead,
    pub domain: Vec<ObjectName>,
    pub codomain: Option<ObjectName>,
}

impl Method {
    fn new(head: MethodHead, domain: Vec<ObjectName>, codomain: Option<ObjectName>) -> Self {
        Self {
            head,
            domain,
            codomain,
        }
    }

    fn has_same_dispatch_as(&self, other: &Self) -> bool {
        self.head == other.head && self.domain == other.domain
    }
}

/// A function's argument shape, read from its lambda parameter node — the arity
/// of its domain, independent of any installed methods (a function with no
/// methods can still be total).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Variadic,
    Fixed(usize),
}

/// The [`Dispatch`] shape of a lambda from its parameter node: a bare `Symbol`
/// parameter is variadic; a `Sequence` is fixed-arity with one slot per named
/// element.
fn function_dispatch(lambda: M2Node) -> Option<Dispatch> {
    let parameters = lambda.child_by_field_name("parameters")?;
    Some(match parameters.kind {
        NodeKind::ParenthesizedExpression => Dispatch::Fixed(1),
        kind if kind.is_collection_expression() => {
            Dispatch::Fixed(parameters.collection_elements().count())
        }
        // A single bare parameter binds the whole argument sequence — variadic.
        _ => Dispatch::Variadic,
    })
}

/// A characterized assignment that installs one named [`Method`].
///
/// It is produced once during analysis and consumed by every capability instead
/// of each re-deciding installation syntax. `span` covers the whole assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInstallation {
    pub id: MethodInstallationId,
    pub method: Method,
    pub codomain_span: Option<TextRange>,
    pub span: TextRange,
    pub target: TextRange,
    pub value: Option<TextRange>,
    /// Required arity of the installed function. Assignment handlers receive
    /// the assigned value in addition to the operands in `domain`.
    expected_rhs_arity: usize,
    pub rhs_lambda_dispatch: Option<Dispatch>,
}

#[derive(Debug)]
pub struct CompositeAssignmentDeclaration {
    pub span: TextRange,
    pub target: TextRange,
    pub value: Option<TextRange>,
    pub scope_idx: usize,
    pub operator: ObjectName,
}

impl MethodInstallation {
    /// The argument count the right-hand-side function must take.
    pub fn expected_rhs_arity(&self) -> usize {
        self.expected_rhs_arity
    }
}

/// How an installation head resolves with respect to method-function-ness — the
/// hinge of the no-effect rule. `Unknown` keeps the analysis monotone: we never
/// warn (nor suppress a record) on an unresolved head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadFunctionKind {
    MethodFunction,
    NonMethodFunction,
    Unknown,
}

/// Syntax-derived callable behavior used to decide whether method
/// installations take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalFunctionKind {
    Unknown,
    Plain,
    Method,
}

/// Semantic information about one locally defined callable.
///
/// Installed methods are referenced by identity in the snapshot's semantic
/// registry
/// so their source facts have one owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub name: ObjectName,
    pub typical_value: Option<ObjectName>,
    pub methods: Vec<MethodInstallationId>,
    pub dispatch: Option<Dispatch>,
    kind: LocalFunctionKind,
}

/// Static facts computed for one call after separating positional arguments
/// from literal option assignments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStaticFacts {
    pub argument_types: Vec<InferredType>,
    pub literal_options: Vec<LiteralOption>,
}

/// Source-independent identity and declaration properties of one lexical
/// binding.
///
/// Its value and inferred type at a particular point live in
/// [`BindingStateInfo`] records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingInfo {
    pub binding_id: BindingId,
    pub name: ObjectName,
    pub role: BindingRole,
    pub declaration_kind: SymbolKind,
    pub potential_export: bool,
    pub range: TextRange,
    pub scope_idx: usize,
    pub declaration_range: TextRange,
    pub definition_state: BindingStateId,
}

/// One source-ordered value, kind, and inferred-type state of a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingStateInfo {
    pub state_id: BindingStateId,
    pub binding_id: BindingId,
    pub kind: SymbolKind,
    pub type_name: Option<ObjectName>,
    /// For an `IndexedVariableTable` binding, the local ring type produced by
    /// subscripting the table after a ring constructor or `use`-style rebind.
    pub indexed_element_type: Option<ObjectName>,
    pub value_range: Option<TextRange>,
    pub span: TextRange,
    pub scope_idx: usize,
}

/// A binding declaration paired with the state effective at a query position.
#[derive(Debug, Clone, Copy)]
pub struct BindingView<'a> {
    pub binding: &'a BindingInfo,
    pub state: &'a BindingStateInfo,
}

impl Deref for BindingView<'_> {
    /// The binding declaration exposed by dereferencing the combined view.
    type Target = BindingInfo;

    fn deref(&self) -> &Self::Target {
        self.binding
    }
}

impl Metadata for BindingView<'_> {
    fn meta(&self) -> Meta<'_> {
        Meta {
            symbol_kind: Some(self.state.kind),
            binding_role: Some(self.role),
            type_name: self.state.type_name.as_ref().map(ObjectName::name),
        }
    }
}

/// One lexical scope and its relationship to the enclosing scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeInfo {
    pub range: TextRange,
    pub parent_idx: Option<usize>,
    /// `=` definitions in this statically isolated scope may still become
    /// visible outside it when the region executes at runtime.
    pub context_assignments_may_escape: bool,
}

/// Typed source-declared type edges and their source-symbol identities.
#[derive(Debug, Default)]
struct SourceTypeFacts {
    by_name: HashMap<ObjectName, TypeId>,
    data: HashMap<TypeId, TypeData>,
}

/// Canonical per-snapshot store of symbols, bindings, scopes, and their indexes.
#[derive(Debug, Default)]
pub struct SemanticRegistry {
    pub scopes: Vec<ScopeInfo>,
    pub bindings: Vec<BindingInfo>,
    pub binding_states: Vec<BindingStateInfo>,
    pub bindings_by_name: HashMap<ObjectName, Vec<BindingId>>,
    pub states_by_binding: HashMap<BindingId, Vec<BindingStateId>>,
    pub functions: HashMap<ObjectName, FunctionInfo>,
    installations: Vec<MethodInstallation>,
    composite_assignment_declarations: Vec<CompositeAssignmentDeclaration>,
    source_types: SourceTypeFacts,
    ring_generators: HashMap<ObjectName, Vec<ring::RingGenerator>>,
}

/// Binding-registration policy selected from the assignment operator.
#[derive(Debug, Clone, Copy)]
enum DefinitionScope {
    Local,
    Assign,
}

/// Complete input for creating a binding or adding a new state to one.
///
/// Keeping this packet typed ensures all registration paths pass through the
/// same bookkeeping code.
#[derive(Debug, Clone)]
struct SymbolRegistration<'a> {
    kind: SymbolKind,
    role: BindingRole,
    type_name: Option<ObjectName>,
    indexed_element_type: Option<ObjectName>,
    parent_type: Option<TypeId>,
    node: M2Node<'a>,
    value_node: Option<M2Node<'a>>,
    scope_idx: usize,
    potential_export: bool,
}

/// Scope behavior contributed by one control-flow child.
#[derive(Debug, Clone, Copy)]
struct ChildScopePolicy {
    assignments_are_local: bool,
    context_assignments_may_escape: bool,
}

impl ChildScopePolicy {
    const CONDITIONAL: Self = Self {
        assignments_are_local: true,
        context_assignments_may_escape: true,
    };

    const LOOP_CLAUSE: Self = Self {
        assignments_are_local: false,
        context_assignments_may_escape: false,
    };
}

/// Complete semantic analysis of one immutable document snapshot.
/// It owns source facts, characterized method installations, and diagnostics;
/// expression types are not retained after inference.
#[derive(Debug)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub registry: SemanticRegistry,
}

impl Analysis {
    pub fn find_definition(&self, name: &str, pos: Position) -> Option<TextRange> {
        self.get_symbol_at(name, pos).map(|symbol| symbol.range)
    }

    pub fn get_symbol_at(&self, name: &str, pos: Position) -> Option<BindingView<'_>> {
        let state = self.get_binding_at(name, pos)?;
        self.binding_definition(state.binding_id)
    }

    pub fn documentation_symbol_at(&self, name: &str, pos: Position) -> Option<BindingView<'_>> {
        self.get_symbol_at(name, pos).or_else(|| {
            let mut fallback = None;
            for binding_id in self.registry.bindings_by_name.get(name)? {
                let binding = self.binding_definition(*binding_id)?;
                if binding.scope_idx == 0 {
                    return Some(binding);
                }
                fallback.get_or_insert(binding);
            }
            fallback
        })
    }

    pub fn get_binding_at(&self, name: &str, pos: Position) -> Option<BindingView<'_>> {
        let scope_idx = self.find_scope_at(pos)?;
        self.get_binding_from_scope(name, scope_idx, pos)
    }

    fn get_binding_from_scope(
        &self,
        name: &str,
        scope_idx: usize,
        pos: Position,
    ) -> Option<BindingView<'_>> {
        let binding_id = self.binding_id_from_scope(name, scope_idx, pos)?;
        self.binding_state_from_scope(binding_id, scope_idx, pos)
    }

    fn binding_id_from_scope(
        &self,
        name: &str,
        scope_idx: usize,
        pos: Position,
    ) -> Option<BindingId> {
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            // In the use's own scope a binding governs only from its definition
            // onward: a use textually before a local `:=` sees the outer binding,
            // not the not-yet-declared local. Ancestor scopes are closures
            // evaluated at call time, after the file is fully read, so a forward
            // reference to an outer name defined later still resolves to it.
            let constrain_to_prior = idx == scope_idx;
            let binding_id = self
                .registry
                .bindings_by_name
                .get(name)
                .into_iter()
                .flatten()
                .filter_map(|binding_id| {
                    self.binding(*binding_id)
                        .map(|binding| (*binding_id, binding))
                })
                .filter(|binding| {
                    binding.1.scope_idx == idx
                        && (!constrain_to_prior || binding.1.range.start <= pos)
                })
                .max_by_key(|(_, binding)| {
                    (binding.range.start.line, binding.range.start.character)
                })
                .map(|(binding_id, _)| binding_id);
            if binding_id.is_some() {
                return binding_id;
            }
            curr = self.registry.scopes[idx].parent_idx;
        }
        None
    }

    fn binding(&self, binding_id: BindingId) -> Option<&BindingInfo> {
        self.registry.bindings.get(binding_id.0 as usize)
    }

    fn binding_definition(&self, binding_id: BindingId) -> Option<BindingView<'_>> {
        let binding = self.binding(binding_id)?;
        let state = self.binding_state(binding.definition_state)?;
        Some(BindingView { binding, state })
    }

    fn binding_state(&self, state_id: BindingStateId) -> Option<&BindingStateInfo> {
        self.registry.binding_states.get(state_id.0 as usize)
    }

    fn binding_view<'a>(&'a self, state: &'a BindingStateInfo) -> Option<BindingView<'a>> {
        Some(BindingView {
            binding: self.binding(state.binding_id)?,
            state,
        })
    }

    fn binding_state_from_scope(
        &self,
        binding_id: BindingId,
        scope_idx: usize,
        pos: Position,
    ) -> Option<BindingView<'_>> {
        let state_ids = self.registry.states_by_binding.get(&binding_id)?;
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            let constrain_to_prior = idx == scope_idx;
            let state = state_ids
                .iter()
                .filter_map(|state_id| self.binding_state(*state_id))
                .filter(|state| {
                    state.scope_idx == idx && (!constrain_to_prior || state.span.start <= pos)
                })
                .max_by_key(|state| (state.span.start.line, state.span.start.character));
            if state.is_some() {
                return self.binding_view(state?);
            }
            curr = self.registry.scopes[idx].parent_idx;
        }
        self.binding_definition(binding_id)
    }

    pub fn function(&self, name: &str) -> Option<&FunctionInfo> {
        self.registry.functions.get(name)
    }

    pub fn method_installation_codomain<'a>(
        &'a self,
        installation: &'a MethodInstallation,
    ) -> Option<&'a str> {
        installation.method.codomain.as_ref().map(ObjectName::name)
    }

    pub fn is_method_installation_codomain(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) -> bool {
        let span = source.range_for_node(node);
        self.registry
            .installations
            .iter()
            .any(|installation| installation.codomain_span.as_ref() == Some(&span))
    }

    pub fn function_for_binding(&self, binding: &BindingInfo) -> Option<&FunctionInfo> {
        self.registry.functions.get(&binding.name)
    }

    pub fn methods_for<'a>(
        &'a self,
        function: &'a FunctionInfo,
    ) -> impl Iterator<Item = &'a Method> + 'a {
        function
            .methods
            .iter()
            .filter_map(|id| self.method_installation(*id))
            .map(|installation| &installation.method)
    }

    /// Methods installed on `function` no later than `position`, with a later
    /// installation of the same method identity shadowing an earlier one.
    fn methods_for_at<'a>(
        &'a self,
        function: &'a FunctionInfo,
        position: Position,
    ) -> Vec<&'a Method> {
        let active_methods = function
            .methods
            .iter()
            .filter_map(|id| self.method_installation(*id))
            .map(|installation| &installation.method)
            .collect::<Vec<_>>();
        let mut seen = Vec::new();
        let mut methods = Vec::new();
        for method in self
            .registry
            .installations
            .iter()
            .rev()
            .filter(|installation| installation.span.start <= position)
            .map(|installation| &installation.method)
        {
            if active_methods
                .iter()
                .any(|active| active.has_same_dispatch_as(method))
                && !seen
                    .iter()
                    .any(|previous: &&Method| previous.has_same_dispatch_as(method))
            {
                seen.push(method);
                methods.push(method);
            }
        }
        methods
    }

    fn method_installation(&self, id: MethodInstallationId) -> Option<&MethodInstallation> {
        self.registry.installations.get(id.0)
    }

    /// Borrow every characterized method installation in source order.
    pub fn installations(&self) -> &[MethodInstallation] {
        &self.registry.installations
    }

    pub fn composite_assignment_declarations(&self) -> &[CompositeAssignmentDeclaration] {
        &self.registry.composite_assignment_declarations
    }

    pub fn scope_with_range(&self, range: TextRange) -> Option<usize> {
        self.registry
            .scopes
            .iter()
            .position(|scope| scope.range == range)
    }

    pub fn bindings_in_scope(&self, scope_idx: usize) -> impl Iterator<Item = BindingView<'_>> {
        self.bindings()
            .filter(move |binding| binding.scope_idx == scope_idx)
    }

    pub fn bindings(&self) -> impl Iterator<Item = BindingView<'_>> {
        self.registry
            .bindings
            .iter()
            .filter_map(|binding| self.binding_definition(binding.binding_id))
    }

    pub fn typed_bindings_in_range(&self, range: TextRange) -> Vec<BindingView<'_>> {
        self.registry
            .bindings
            .iter()
            .filter_map(|binding| self.binding_definition(binding.binding_id))
            .filter(|binding| binding.state.type_name.is_some())
            .filter(|binding| {
                matches!(
                    binding.state.kind,
                    SymbolKind::VARIABLE | SymbolKind::FUNCTION
                )
            })
            .filter(|binding| {
                let position = binding.range.end;
                position_in_range(position, range)
            })
            .collect()
    }

    /// Local symbol names visible at `pos` whose name starts with `prefix`, from
    /// the most-nested scope outward, de-duplicated (a nearer binding shadows an
    /// outer one). Drives local-symbol completion.
    pub fn in_scope_symbols(&self, prefix: &str, pos: Position) -> Vec<(String, SymbolKind)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut current = self.find_scope_at(pos);
        while let Some(idx) = current {
            for binding in self.bindings_in_scope(idx) {
                if binding.name.name().starts_with(prefix) && seen.insert(binding.name.clone()) {
                    out.push((binding.name.name().to_string(), binding.state.kind));
                }
            }
            current = self.registry.scopes[idx].parent_idx;
        }
        out
    }

    fn find_scope_at(&self, pos: Position) -> Option<usize> {
        let mut best_idx = None;
        let mut best_range: Option<TextRange> = None;

        for (idx, scope) in self.registry.scopes.iter().enumerate() {
            if position_in_range(pos, scope.range) {
                match best_range {
                    None => {
                        best_idx = Some(idx);
                        best_range = Some(scope.range);
                    }
                    Some(r) => {
                        // We want the smallest (most nested) scope
                        if is_range_smaller(scope.range, r) {
                            best_idx = Some(idx);
                            best_range = Some(scope.range);
                        }
                    }
                }
            }
        }
        best_idx
    }

    /// Return the most-nested lexical scope containing `position`.
    pub(crate) fn scope_at_position(&self, position: Position) -> Option<usize> {
        self.find_scope_at(position)
    }

    pub fn new_with_knowledge(
        root: M2Node<'_>,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl PositionedTypeKnowledge + ?Sized),
    ) -> Self {
        let mut analysis = Analysis {
            diagnostics: Vec::new(),
            registry: SemanticRegistry {
                scopes: vec![ScopeInfo {
                    range: TextRange::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                    parent_idx: None,
                    context_assignments_may_escape: false,
                }],
                ..Default::default()
            },
        };
        // Analysis-first: derive bindings, scopes, and method installations
        // before running diagnostics that consume those facts.
        analysis.build_scopes(root, source, 0, 0, knowledge);
        analysis.collect_installation_diagnostics(knowledge);
        analysis.collect_install_form_diagnostics(root, source, knowledge);
        analysis.collect_diagnostics(root, source, knowledge);
        analysis.collect_unused_binding_diagnostics(root, source);
        analysis
    }

    fn build_scopes(
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
                next_scope_idx = self.push_scope(node, source, Some(current_scope_idx), false);
                next_assignment_scope_idx = next_scope_idx;

                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let parameter_types = method_installation_parameter_types_for_function(node);
                    self.collect_parameters(
                        params_node,
                        source,
                        next_scope_idx,
                        parameter_types.as_deref(),
                    );
                }
            }
            _ if node.is_assignment() => {
                let left = node.child_by_field_name("left");
                let op = node.child_by_field_name("operator");
                let right = node.child_by_field_name("right");

                if let (Some(left), Some(op)) = (left, op) {
                    let op_text = op.text();
                    let installation = self.record_method_installation(node, source, &knowledge);
                    if installation.is_none() && op_text == "=" {
                        if let Some(operator) = left.binary_operator() {
                            self.registry.composite_assignment_declarations.push(
                                CompositeAssignmentDeclaration {
                                    span: source.range_for_node(node),
                                    target: source.range_for_node(left),
                                    value: right.map(|right| source.range_for_node(right)),
                                    scope_idx: current_scope_idx,
                                    operator: ObjectName::new(operator),
                                },
                            );
                        }
                    }
                    let symbol_kind = match right {
                        Some(right) if right.kind == NodeKind::LambdaExpression => {
                            SymbolKind::FUNCTION
                        }
                        Some(right)
                            if method_declaration_typical_value(right).is_some()
                                || is_method_call(right) =>
                        {
                            SymbolKind::FUNCTION
                        }
                        _ => SymbolKind::VARIABLE,
                    };
                    let type_name = right.and_then(|right| {
                        if method_declaration_typical_value(right).is_some()
                            || is_method_call(right)
                        {
                            Some(ObjectName::new("MethodFunction"))
                        } else {
                            TypeChecker::new(self)
                                .type_of(right, source, current_scope_idx, &knowledge)
                                .principal()
                                .cloned()
                        }
                    });
                    let parent_type = right
                        .and_then(|right| {
                            declared_type_parent(right, type_name.as_ref(), &knowledge)
                        })
                        .and_then(|parent| {
                            TypeChecker::new(self).resolve_type_id(&parent, &knowledge)
                        });

                    if let (Some(right), Some(name)) =
                        (right, single_symbol_assignment_target(left))
                    {
                        if let Some(typical_value) = method_declaration_typical_value(right) {
                            self.record_local_method_declaration(name, typical_value);
                        } else if right.kind == NodeKind::LambdaExpression {
                            if let Some(dispatch) = function_dispatch(right) {
                                self.record_local_function_dispatch(name, dispatch);
                            }
                        }
                    }

                    match op_text {
                        ":=" => self.collect_definitions(
                            left,
                            right,
                            source,
                            DefinitionScope::Local,
                            SymbolRegistration {
                                kind: symbol_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.clone(),
                                indexed_element_type: None,
                                parent_type: parent_type.clone(),
                                node: left,
                                value_node: right,
                                scope_idx: current_scope_idx,
                                potential_export: current_scope_idx == 0,
                            },
                        ),
                        // `=` writes the nearest enclosing binding of the name, or
                        // creates a global when none exists anywhere up the chain.
                        // The write becomes a new state of that binding rather than
                        // a second lexical definition.
                        "=" => self.collect_definitions(
                            left,
                            right,
                            source,
                            DefinitionScope::Assign,
                            SymbolRegistration {
                                kind: symbol_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.clone(),
                                indexed_element_type: None,
                                parent_type,
                                node: left,
                                value_node: right,
                                scope_idx: assignment_scope_idx,
                                potential_export: assignment_scope_idx == 0
                                    || self.registry.scopes[assignment_scope_idx]
                                        .context_assignments_may_escape,
                            },
                        ),
                        _ => {}
                    }

                    if let (Some(right), Some(type_name), Some(ring_name)) = (
                        right,
                        type_name.as_ref(),
                        single_symbol_assignment_target(left),
                    ) {
                        if type_name.name() == "Ring"
                            || knowledge.is_subtype(type_name, &ObjectName::new("Ring"))
                        {
                            self.collect_ring_generator_bindings(
                                ring_name, right, left, source, &knowledge,
                            );
                        }
                    }
                }
            }
            _ => {}
        }

        // Recurse into children
        for child in node.children() {
            let (child_scope_idx, child_assignment_scope_idx) =
                match child_scope_policy(node, child) {
                    Some(policy) => {
                        let scope_idx = self.push_scope(
                            child,
                            source,
                            Some(next_scope_idx),
                            policy.context_assignments_may_escape,
                        );
                        let assignment_scope_idx = if policy.assignments_are_local {
                            scope_idx
                        } else {
                            next_assignment_scope_idx
                        };
                        (scope_idx, assignment_scope_idx)
                    }
                    None => (next_scope_idx, next_assignment_scope_idx),
                };
            self.build_scopes(
                child,
                source,
                child_scope_idx,
                child_assignment_scope_idx,
                knowledge_provider,
            );
        }
    }

    /// The installation characterized for the assignment spanning `node`, if any.
    pub fn installation_for(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) -> Option<&MethodInstallation> {
        let span = source.range_for_node(node);
        self.registry
            .installations
            .iter()
            .find(|installation| installation.span == span)
    }

    /// Characterize an `=`/`:=` assignment as a method installation, or `None`
    /// when it is an ordinary assignment/call. The single source of truth for
    /// the install-vs-call distinction.
    ///
    /// - `:=` installs whenever the left side matches an installation shape (the
    ///   six forms: `f Type`, `f (T..)`, `T1 op T2`, `T1 T2` adjacency,
    ///   `prefix T`, `T postfix`).
    /// - `=` installs ONLY the assignment-operator form `T1 op T2 = f`, and only
    ///   when both operands are types — otherwise the identical syntax assigns
    ///   `f` to the lvalue `T1 op T2`, which is a call.
    fn classify_installation(
        &self,
        id: MethodInstallationId,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<MethodInstallation> {
        let operator = node.binary_operator()?;
        let left = node.child_by_field_name("left")?;
        let (head, domain) = self.installation_shape(left, knowledge)?;
        let operand_arity = domain.len();
        let span = source.range_for_node(node);
        let target = source.range_for_node(left);
        let right = node.child_by_field_name("right");
        let value = right.map(|right| source.range_for_node(right));
        let codomain_node = right
            .filter(|right| right.is_option_assignment())
            .and_then(|right| right.child_by_field_name("left"));
        let codomain = codomain_node
            .and_then(symbol_node_text)
            .map(ObjectName::new);
        let codomain_span = codomain_node.map(|node| source.range_for_node(node));
        // The RHS function shape, read once here so the arity diagnostic need not
        // re-walk the tree. Only a plain lambda RHS carries a checkable arity.
        let rhs_lambda_dispatch = node
            .child_by_field_name("right")
            .filter(|right| right.kind == NodeKind::LambdaExpression)
            .and_then(function_dispatch);

        match operator {
            // `:=` installs by shape alone — no type check on the operands.
            ":=" => Some(MethodInstallation {
                id,
                method: Method::new(head, domain, codomain),
                codomain_span,
                span,
                target,
                value,
                expected_rhs_arity: operand_arity,
                rhs_lambda_dispatch,
            }),
            // `=` installs only the assignment form of a BINARY operator (incl.
            // SPACE), and only when every operand is a type; otherwise the same
            // syntax assigns to the lvalue `X op Y`, which is a call.
            "=" => match head {
                MethodHead::Operator(op)
                    if op.form == OperatorForm::Binary
                        && domain
                            .iter()
                            .all(|operand| self.operand_is_type(operand.name(), knowledge)) =>
                {
                    Some(MethodInstallation {
                        id,
                        method: Method::new(MethodHead::Operator(op), domain, codomain),
                        codomain_span,
                        span,
                        target,
                        value,
                        expected_rhs_arity: operand_arity + 1,
                        rhs_lambda_dispatch,
                    })
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Classify the left side of an assignment into a `(MethodHead, domain)`
    /// pair (the bare, non-assignment head), or `None` if it is not an
    /// installation target at all. The `=`/`:=` rule is applied by the caller.
    fn installation_shape(
        &self,
        node: M2Node,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<(MethodHead, Vec<ObjectName>)> {
        // A parenthesized expression is identified with its final value, so
        // `(T op S) := f` installs exactly like `T op S := f`. A final `muted`
        // child means the group evaluates to null and is not an installation
        // target.
        if node.kind == NodeKind::ParenthesizedExpression {
            let inner = node.final_value_child()?;
            return self.installation_shape(inner, knowledge);
        }
        match node.kind {
            NodeKind::BinaryExpression => {
                let left = node.child_by_field_name("left")?;
                let right = node.child_by_field_name("right")?;
                if node.is_space_application() {
                    // `A B` (juxtaposition = the SPACE operator): a method on the
                    // named function `A` when `A` is a function, or a SPACE
                    // operator method on the type pair when `A` is a type.
                    let left_name = symbol_node_text(left)?;
                    if self.operand_is_type(left_name, knowledge) {
                        let right_name = symbol_node_text(right)?;
                        Some((
                            MethodHead::Operator(Operator {
                                token: ObjectName::new(SPACE_OPERATOR),
                                form: OperatorForm::Binary,
                            }),
                            vec![ObjectName::new(left_name), ObjectName::new(right_name)],
                        ))
                    } else {
                        Some((
                            MethodHead::Function(ObjectName::new(left_name)),
                            method_installation_domain(right)?,
                        ))
                    }
                } else {
                    // `X op Y`: an explicit binary-operator method.
                    let operator = node.binary_operator()?;
                    if matches!(operator, "=" | ":=" | "<-" | "=>") {
                        return None;
                    }
                    Some((
                        MethodHead::Operator(Operator {
                            token: ObjectName::new(operator),
                            form: OperatorForm::Binary,
                        }),
                        vec![
                            ObjectName::new(symbol_node_text(left)?),
                            ObjectName::new(symbol_node_text(right)?),
                        ],
                    ))
                }
            }
            NodeKind::PrefixExpression => Some((
                MethodHead::Operator(Operator {
                    token: ObjectName::new(operator_text(node)?),
                    form: OperatorForm::Prefix,
                }),
                vec![ObjectName::new(symbol_node_text(
                    node.child_by_field_name("operand")?,
                )?)],
            )),
            NodeKind::PostfixExpression => Some((
                MethodHead::Operator(Operator {
                    token: ObjectName::new(operator_text(node)?),
                    form: OperatorForm::Postfix,
                }),
                vec![ObjectName::new(symbol_node_text(
                    node.child_by_field_name("operand")?,
                )?)],
            )),
            _ => None,
        }
    }

    /// Whether `name` denotes a TYPE — the hinge of the installation rules. The
    /// type universe is layered:
    /// a local binding whose inferred class is `Type` (e.g. `X = new Type of …`)
    /// takes precedence over objects visible in the current environment.
    fn operand_is_type(&self, name: &str, knowledge: &(impl TypeKnowledge + ?Sized)) -> bool {
        self.local_binding_is_type(name, knowledge)
            || knowledge
                .get_record(&ObjectName::new(name))
                .is_some_and(|record| record.type_info().is_some())
    }

    /// Whether any local binding named `name` is a type — its inferred static
    /// class is `Type` or a `Type` descendant.
    fn local_binding_is_type(&self, name: &str, knowledge: &(impl TypeKnowledge + ?Sized)) -> bool {
        self.registry
            .bindings_by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|binding_id| self.binding_definition(*binding_id))
            .any(|binding| {
                binding
                    .state
                    .type_name
                    .as_ref()
                    .is_some_and(|type_name| type_name_denotes_type(type_name, knowledge))
            })
    }

    /// Emit a diagnostic for every stored installation that M2 would reject or
    /// silently ignore. Installation shapes were characterized during the
    /// source-ordered scope pass; this phase only consumes those facts.
    fn collect_installation_diagnostics(
        &mut self,
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        // Validity hinges on the type universe: adjacency `A B := …` is a SPACE
        // operator install when `A` is a type but a function-head install
        // otherwise, and the two have different domains (hence different arities).
        // Without external facts we cannot tell them apart, so stay monotone.
        if !knowledge_provider
            .at_position(Position::new(0, 0))
            .is_available()
        {
            return;
        }
        let mut diagnostics = Vec::new();
        for installation in &self.registry.installations {
            let knowledge = knowledge_provider.at_position(installation.span.start);
            self.installation_diagnostics(installation, &knowledge, &mut diagnostics);
        }
        self.diagnostics.extend(diagnostics);
    }

    /// Flag a method install written with `=` instead of `:=` on a method
    /// function head (`f Domain = fn`). M2 rejects this ("no method for storing
    /// values of function f") because the assignment-install form is reserved for
    /// operators; `:=` is the installation operator. A lambda RHS distinguishes an
    /// install attempt from a legitimate value store. Walks the tree itself
    /// because such an `=` is classified as a plain assignment (no install record).
    fn collect_install_form_diagnostics(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        let mut diagnostics = Vec::new();
        self.scan_install_form(node, source, knowledge, &mut diagnostics);
        self.diagnostics.extend(diagnostics);
    }

    fn scan_install_form(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
        out: &mut Vec<Diagnostic>,
    ) {
        let knowledge = knowledge_provider.at_position(source.position_for_node(node));
        if let Some(name) = self.illegal_equals_install_head(node, &knowledge) {
            out.push(M2Diagnostic::InstallNeedsColonEquals.at(
                source.range_for_node(node),
                format!(
                    "Installing a method on `{name}` must use `:=`, not `=`: M2 rejects this \
                     (\"no method for storing values of function {name}\"). Use `:=`."
                ),
            ));
        }
        for child in node.children() {
            self.scan_install_form(child, source, knowledge_provider, out);
        }
    }

    /// The function name when `node` is `f Domain = fn` — an `=` assignment whose
    /// left side is a function-head install shape, whose right side is a lambda
    /// (install intent, not a value store), and whose head resolves to a function.
    /// `None` otherwise.
    fn illegal_equals_install_head(
        &self,
        node: M2Node,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<String> {
        if !node.is_assignment() || node.binary_operator() != Some("=") {
            return None;
        }
        let right = node.child_by_field_name("right")?;
        if right.kind != NodeKind::LambdaExpression {
            return None;
        }
        let left = node.child_by_field_name("left")?;
        let (MethodHead::Function(name), _) = self.installation_shape(left, knowledge)? else {
            return None;
        };
        // M2 rejects `f Domain = fn` for ANY function head, method function or
        // not ("no method for storing values of function f"); verified against
        // v1.26.05. Stay silent only when `name` does not resolve to a function.
        (self.head_function_kind(name.name(), knowledge) != HeadFunctionKind::Unknown)
            .then(|| name.name().to_string())
    }

    /// The diagnostics for a single installation: a no-effect warning on a
    /// non-method-function head, a hard error on a non-flexible operator form, and
    /// a hard error when a fixed-arity RHS disagrees with the installed domain.
    fn installation_diagnostics(
        &self,
        installation: &MethodInstallation,
        knowledge: &(impl TypeKnowledge + ?Sized),
        out: &mut Vec<Diagnostic>,
    ) {
        match &installation.method.head {
            MethodHead::Function(name) => {
                if self.head_function_kind(name.name(), knowledge)
                    == HeadFunctionKind::NonMethodFunction
                {
                    out.push(M2Diagnostic::InstallNoEffect.at(
                        installation.span,
                        format!(
                            "Installing a method on `{name}` has no effect: `{name}` is not a \
                             method function. Define it with `{name} = method()` to make method \
                             installations take effect."
                        ),
                    ));
                }
            }
            MethodHead::Operator(operator) => {
                let form = operator.form;
                if self.operator_form_is_flexible(operator.token.name(), form, knowledge)
                    == Some(false)
                {
                    out.push(M2Diagnostic::OperatorNotFlexible.at(
                        installation.span,
                        format!(
                            "Cannot install a method on the {form} operator `{}`: it is not \
                             flexible, so M2 rejects the assignment.",
                            operator.token
                        ),
                    ));
                }
            }
        }

        // A variadic RHS (`x -> …`) binds the whole argument sequence and absorbs
        // any arity, so only a fixed-arity RHS can be wrong.
        if let Some(Dispatch::Fixed(actual)) = installation.rhs_lambda_dispatch {
            let expected = installation.expected_rhs_arity();
            if actual != expected {
                out.push(M2Diagnostic::InstallArity.at(
                    installation.span,
                    format!(
                        "This method's function takes {actual} argument(s) but the installation \
                         expects {expected}. Match the domain arity or use a variadic `x -> …`."
                    ),
                ));
            }
        }
    }

    /// Whether the named operator's given form is flexible (accepts a runtime
    /// method install). `None` when the operator or its attributes are unknown
    /// (e.g. `SPACE`), so the caller stays silent rather than guessing.
    fn operator_form_is_flexible(
        &self,
        token: &str,
        form: OperatorForm,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<bool> {
        let record = knowledge.get_record(&ObjectName::new(token))?;
        let operator_info = record.operator_info()?;
        Some(operator_info.is_flexible(form))
    }

    /// Classify an installation's function head by method-function-ness, querying
    /// the layered type universe (local bindings first, then knowledge).
    fn head_function_kind(
        &self,
        name: &str,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> HeadFunctionKind {
        // A local function binding shadows an earlier visible object. Its callable kind is
        // recorded from the defining syntax, without reverse-engineering the
        // behavior from a runtime class-name catalog.
        if let Some(kind) = self.local_function_kind(name) {
            return match kind {
                LocalFunctionKind::Method => HeadFunctionKind::MethodFunction,
                LocalFunctionKind::Plain => HeadFunctionKind::NonMethodFunction,
                LocalFunctionKind::Unknown => HeadFunctionKind::Unknown,
            };
        }
        // Otherwise use the callable kind of the object visible here.
        if let Some(record) = knowledge.get_record(&ObjectName::new(name)) {
            if let Some(callable) = record.callable() {
                return if callable.is_method_function() {
                    HeadFunctionKind::MethodFunction
                } else {
                    HeadFunctionKind::NonMethodFunction
                };
            }
        }
        // Not resolvable as a function — stay silent (monotone).
        HeadFunctionKind::Unknown
    }

    fn local_function_kind(&self, name: &str) -> Option<LocalFunctionKind> {
        self.registry
            .bindings_by_name
            .get(name)?
            .iter()
            .rev()
            .flat_map(|binding_id| {
                self.registry
                    .states_by_binding
                    .get(binding_id)
                    .into_iter()
                    .flatten()
                    .rev()
            })
            .filter_map(|state_id| self.binding_state(*state_id))
            .find(|binding| binding.kind == SymbolKind::FUNCTION)?;
        Some(
            self.registry
                .functions
                .get(name)
                .map_or(LocalFunctionKind::Unknown, |function| function.kind),
        )
    }

    fn collect_parameters(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        parameter_types: Option<&[ObjectName]>,
    ) {
        let mut parameter_nodes = Vec::new();
        collect_parameter_nodes(node, &mut parameter_nodes);
        let typed_parameters = parameter_types.filter(|types| types.len() == parameter_nodes.len());
        for (idx, parameter_node) in parameter_nodes.into_iter().enumerate() {
            let name = parameter_node.text();
            let type_name = typed_parameters.and_then(|types| types.get(idx)).cloned();
            self.add_symbol(
                name,
                SymbolRegistration {
                    kind: SymbolKind::VARIABLE,
                    role: BindingRole::Parameter,
                    type_name,
                    indexed_element_type: None,
                    parent_type: None,
                    node: parameter_node,
                    value_node: None,
                    scope_idx,
                    potential_export: false,
                },
                source,
            );
        }
    }

    fn collect_definitions(
        &mut self,
        node: M2Node,
        value_node: Option<M2Node>,
        source: &(impl SourceNavigation + ?Sized),
        definition_scope: DefinitionScope,
        registration: SymbolRegistration<'_>,
    ) {
        match node.kind {
            NodeKind::Symbol => {
                let name = node.text();
                match definition_scope {
                    DefinitionScope::Local => self.add_symbol(
                        name,
                        SymbolRegistration {
                            node,
                            value_node,
                            ..registration
                        },
                        source,
                    ),
                    DefinitionScope::Assign => {
                        let position = source.position_for_node(node);
                        let binding_id = self
                            .binding_id_from_scope(name, registration.scope_idx, position)
                            .filter(|binding_id| {
                                self.binding_definition(*binding_id).is_some_and(|binding| {
                                    binding.scope_idx == registration.scope_idx
                                })
                            });
                        if let Some(binding_id) = binding_id {
                            self.add_binding_state(
                                binding_id,
                                SymbolRegistration {
                                    node,
                                    value_node,
                                    ..registration
                                },
                                source,
                            );
                        } else {
                            self.add_symbol(
                                name,
                                SymbolRegistration {
                                    node,
                                    value_node,
                                    ..registration
                                },
                                source,
                            );
                        }
                    }
                }
            }
            _ if node.kind.is_collection_expression() => {
                // Recurse on every element so nested destructuring targets such
                // as `[x, [y, z]]` register their inner symbols too; non-symbol,
                // non-collection elements fall through to the `_` arm and are
                // ignored.
                for child in node.collection_elements() {
                    self.collect_definitions(
                        child,
                        value_node,
                        source,
                        definition_scope,
                        SymbolRegistration {
                            type_name: None,
                            ..registration.clone()
                        },
                    );
                }
            }
            _ => {}
        }
    }

    fn add_symbol(
        &mut self,
        name: &str,
        registration: SymbolRegistration<'_>,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let SymbolRegistration {
            kind,
            role,
            type_name,
            indexed_element_type,
            parent_type,
            node,
            value_node,
            scope_idx,
            potential_export,
        } = registration;
        let name = ObjectName::new(name);
        if let Some(parent_type) = parent_type {
            let type_id = TypeId::from_object(ObjectId::new(name.name()));
            self.registry
                .source_types
                .by_name
                .insert(name.clone(), type_id.clone());
            self.registry.source_types.data.insert(
                type_id,
                TypeData {
                    parent: parent_type,
                },
            );
        }
        let binding_id = BindingId(self.registry.bindings.len() as u32);
        let state_id = BindingStateId(self.registry.binding_states.len() as u32);
        let range = source.range_for_node(node);
        let binding = BindingInfo {
            binding_id,
            name: name.clone(),
            role,
            declaration_kind: declaration_symbol_kind(kind, value_node),
            potential_export,
            range,
            scope_idx,
            declaration_range: enclosing_definition_range(node, source),
            definition_state: state_id,
        };
        let state = BindingStateInfo {
            state_id,
            binding_id,
            kind,
            type_name,
            indexed_element_type,
            value_range: value_node.map(|value| source.range_for_node(value)),
            span: source.range_for_node(node),
            scope_idx,
        };
        self.registry.bindings.push(binding);
        self.registry.binding_states.push(state);
        self.registry
            .states_by_binding
            .entry(binding_id)
            .or_default()
            .push(state_id);
        self.registry
            .bindings_by_name
            .entry(name)
            .or_default()
            .push(binding_id);
    }

    fn add_binding_state(
        &mut self,
        binding_id: BindingId,
        registration: SymbolRegistration<'_>,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let Some(name) = self.binding(binding_id).map(|binding| binding.name.clone()) else {
            return;
        };
        match registration.parent_type {
            Some(parent_type) => {
                let type_id = TypeId::from_object(ObjectId::new(name.name()));
                self.registry
                    .source_types
                    .by_name
                    .insert(name.clone(), type_id.clone());
                self.registry.source_types.data.insert(
                    type_id,
                    TypeData {
                        parent: parent_type,
                    },
                );
            }
            None => {
                if let Some(type_id) = self.registry.source_types.by_name.remove(&name) {
                    self.registry.source_types.data.remove(&type_id);
                }
            }
        }
        let state_id = BindingStateId(self.registry.binding_states.len() as u32);
        self.registry.binding_states.push(BindingStateInfo {
            state_id,
            binding_id,
            kind: registration.kind,
            type_name: registration.type_name,
            indexed_element_type: registration.indexed_element_type,
            value_range: registration
                .value_node
                .map(|value| source.range_for_node(value)),
            span: source.range_for_node(registration.node),
            scope_idx: registration.scope_idx,
        });
        self.registry
            .states_by_binding
            .entry(binding_id)
            .or_default()
            .push(state_id);
    }

    pub fn local_method_installation_signature_at<'a>(
        &'a self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) -> Option<(&'a FunctionInfo, &'a MethodInstallation)> {
        let assignment = method_installation_assignment_for_callable_node(node)?;
        let installation = self.installation_for(assignment, source)?;
        let MethodHead::Function(name) = &installation.method.head else {
            return None;
        };
        let method = self.function(name.name())?;
        method
            .methods
            .iter()
            .filter_map(|id| self.method_installation(*id))
            .any(|current| current.method.has_same_dispatch_as(&installation.method))
            .then_some((method, installation))
    }

    pub fn infer_call_static_facts(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> CallStaticFacts {
        let scope_idx = self
            .find_scope_at(source.position_for_node(node))
            .unwrap_or(0);
        TypeChecker::new(self).infer_call_facts(node, source, scope_idx, knowledge)
    }

    pub fn infer_expression_static_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<ObjectName> {
        let scope_idx = self
            .find_scope_at(source.position_for_node(node))
            .unwrap_or(0);
        TypeChecker::new(self)
            .type_of(node, source, scope_idx, knowledge)
            .principal()
            .cloned()
    }

    /// Infer a display label for one expression without retaining the result in
    /// the analysis snapshot.
    pub fn infer_expression_type_label(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<String> {
        let scope_idx = self
            .find_scope_at(source.position_for_node(node))
            .unwrap_or(0);
        TypeChecker::new(self)
            .type_of(node, source, scope_idx, knowledge)
            .label()
    }

    /// Project inferred types into the validated identities used by method
    /// dispatch. Locally-created runtime types (most importantly a ring
    /// such as `R = QQ[x]`) walk through the local parent registry first, so an
    /// element whose exact class is `R` dispatches as a `RingElement`.
    pub fn dispatch_argument_ids(
        &self,
        facts: &CallStaticFacts,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Vec<Option<ObjectId>> {
        TypeChecker::new(self).dispatch_argument_ids(facts, knowledge)
    }

    /// Record the [`Dispatch`] shape of a lambda-defined local function on its
    /// function record, creating the record if this is its first mention.
    fn record_local_function_dispatch(&mut self, name: &str, dispatch: Dispatch) {
        let name = ObjectName::new(name);
        let function = self
            .registry
            .functions
            .entry(name.clone())
            .or_insert_with(|| FunctionInfo {
                name,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
                kind: LocalFunctionKind::Unknown,
            });
        function.dispatch = Some(dispatch);
        function.kind = LocalFunctionKind::Plain;
    }

    fn record_local_method_declaration(&mut self, name: &str, typical_value: Option<ObjectName>) {
        let name = ObjectName::new(name);
        let method = self
            .registry
            .functions
            .entry(name.clone())
            .or_insert_with(|| FunctionInfo {
                name,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
                kind: LocalFunctionKind::Unknown,
            });
        method.typical_value = typical_value;
        method.kind = LocalFunctionKind::Method;
    }

    /// Characterize an assignment once, retain its source fact, and attach that
    /// fact to the local callable registry when the installation takes effect.
    fn record_method_installation(
        &mut self,
        assignment: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<MethodInstallationId> {
        let id = MethodInstallationId(self.registry.installations.len());
        let mut installation = self.classify_installation(id, assignment, source, knowledge)?;

        // Preserve M2's distinct assignment-method form: only `:=` contributes
        // a callable signature here. `=` installations are retained for
        // diagnostics/document symbols but are not ordinary call methods.
        if assignment.binary_operator() == Some(":=") {
            self.attach_method_installation(&mut installation, knowledge);
        }

        debug_assert_eq!(installation.id.0, self.registry.installations.len());
        self.registry.installations.push(installation);
        Some(id)
    }

    fn attach_method_installation(
        &mut self,
        installation: &mut MethodInstallation,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        let name = match &installation.method.head {
            MethodHead::Function(name) => {
                // An install on a non-method-function compiles but has no effect,
                // so it creates no method record.
                if self.head_function_kind(name.name(), knowledge)
                    == HeadFunctionKind::NonMethodFunction
                {
                    return;
                }
                name.name()
            }
            MethodHead::Operator(operator) => operator.token.name(),
        };
        let name = ObjectName::new(name);
        let existing_method = self.registry.functions.get(&name).and_then(|function| {
            function.methods.iter().position(|id| {
                self.method_installation(*id).is_some_and(|existing| {
                    existing.method.has_same_dispatch_as(&installation.method)
                })
            })
        });
        let method = self
            .registry
            .functions
            .entry(name.clone())
            .or_insert_with(|| FunctionInfo {
                name,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
                kind: LocalFunctionKind::Unknown,
            });
        if installation.method.codomain.is_none() {
            installation
                .method
                .codomain
                .clone_from(&method.typical_value);
        }
        if let Some(index) = existing_method {
            method.methods[index] = installation.id;
        } else {
            method.methods.push(installation.id);
        }
    }

    fn push_scope(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        parent_idx: Option<usize>,
        context_assignments_may_escape: bool,
    ) -> usize {
        let range = source.range_for_node(node);
        let scope_idx = self.registry.scopes.len();
        self.registry.scopes.push(ScopeInfo {
            range,
            parent_idx,
            context_assignments_may_escape,
        });
        scope_idx
    }

    pub fn binding_id_at(&self, name: &str, pos: Position) -> Option<BindingId> {
        let scope_idx = self.find_scope_at(pos)?;
        self.binding_id_from_scope(name, scope_idx, pos)
    }
}

fn collect_parameter_nodes<'tree>(node: M2Node<'tree>, parameters: &mut Vec<M2Node<'tree>>) {
    match node.kind {
        NodeKind::Symbol => parameters.push(node),
        // `(x,y)` is a `sequence`; a single `(x)` is a `parenthesized_expression`.
        // Both group parameters, so recurse into either.
        NodeKind::Sequence | NodeKind::List | NodeKind::ParenthesizedExpression => {
            for child in node.children() {
                collect_parameter_nodes(child, parameters);
            }
        }
        _ => {}
    }
}

fn single_symbol_assignment_target<'tree>(node: M2Node<'tree>) -> Option<&'tree str> {
    (node.kind == NodeKind::Symbol).then(|| node.text())
}

fn declaration_symbol_kind(kind: SymbolKind, value: Option<M2Node<'_>>) -> SymbolKind {
    let declares_class = value
        .filter(|value| value.kind == NodeKind::NewStatement)
        .and_then(|value| value.child_by_field_name("type"))
        .is_some_and(|type_node| type_node.kind == NodeKind::Symbol && type_node.text() == "Type");
    if declares_class {
        SymbolKind::CLASS
    } else {
        kind
    }
}

fn declared_type_parent<'tree>(
    value: M2Node<'tree>,
    type_name: Option<&ObjectName>,
    knowledge: &(impl TypeKnowledge + ?Sized),
) -> Option<ObjectName> {
    if let Some(parent) = ring::RingSemantics::value_parent(type_name, knowledge) {
        return Some(parent);
    }
    if value.kind != NodeKind::NewStatement
        || !type_name.is_some_and(|type_name| type_name_denotes_type(type_name, knowledge))
    {
        return None;
    }
    clause_of(value, NodeKind::OfClause)
        .and_then(clause_value)
        .and_then(symbol_node_text)
        .map(ObjectName::new)
}

pub fn symbol_node_text<'tree>(node: M2Node<'tree>) -> Option<&'tree str> {
    node.kind.is_symbol_like().then(|| node.text())
}

fn method_declaration_typical_value(node: M2Node) -> Option<Option<ObjectName>> {
    if !node.is_space_application() {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if symbol_node_text(left) != Some("method") {
        return None;
    }

    Some(find_option_value(node, "TypicalValue"))
}

/// Check if a binary expression is a call to the `method` function, catching
/// cases where the tree structure doesn't perfectly match a space_application.
fn is_method_call(node: M2Node) -> bool {
    if node.kind != NodeKind::BinaryExpression {
        return false;
    }
    node.child_by_field_name("left")
        .and_then(|left| symbol_node_text(left))
        == Some("method")
}

fn find_option_value(node: M2Node, option_name: &str) -> Option<ObjectName> {
    if node.is_option_assignment() {
        let left = node.child_by_field_name("left")?;
        let right = node.child_by_field_name("right")?;
        if symbol_node_text(left) == Some(option_name) {
            return symbol_node_text(right).map(ObjectName::new);
        }
    }

    for child in node.named_children() {
        if let Some(value) = find_option_value(child, option_name) {
            return Some(value);
        }
    }
    None
}

fn literal_option_assignment(node: M2Node) -> Option<LiteralOption> {
    if !node.is_option_assignment() {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    let key = symbol_node_text(left)?;
    let value = literal_option_value(right)?;
    Some(LiteralOption {
        option: ObjectName::new(key),
        value: ObjectName::new(value),
    })
}

fn enclosing_definition_range(
    node: M2Node<'_>,
    source: &(impl SourceNavigation + ?Sized),
) -> TextRange {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind == NodeKind::Cell {
            return source.range_for_node(parent);
        }
        current = parent;
    }
    source.range_for_node(node)
}

fn literal_option_value(node: M2Node<'_>) -> Option<&str> {
    if node.kind.is_symbol_like() || node.kind.is_literal() {
        Some(node.text())
    } else {
        None
    }
}

/// The operator token of a prefix/postfix expression, e.g. `-` in `-X` / `X-`.
fn operator_text(node: M2Node<'_>) -> Option<&str> {
    let operator = node.child_by_field_name("operator")?;
    Some(operator.text())
}

/// Whether `type_name` (an inferred static class or a referenced name) denotes a
/// TYPE, i.e. is `Type` itself or one of its descendants (`SelfInitializingType`,
/// …). Without the registry only the exact `Type` is recognized.
fn type_name_denotes_type(
    type_name: &ObjectName,
    knowledge: &(impl TypeKnowledge + ?Sized),
) -> bool {
    type_name.name() == "Type" || knowledge.is_subtype(type_name, &ObjectName::new("Type"))
}

pub fn method_installation_signature(node: M2Node) -> Option<(String, Vec<ObjectName>)> {
    if !node.is_space_application() {
        return None;
    }

    let callable = node.child_by_field_name("left")?;
    let arguments = node.child_by_field_name("right")?;
    let callable = symbol_node_text(callable)?;
    let domain = method_installation_domain(arguments)?;
    Some((callable.to_string(), domain))
}

fn method_installation_parameter_types_for_function(
    function_node: M2Node,
) -> Option<Vec<ObjectName>> {
    let mut current = function_node;
    while let Some(parent) = current.parent() {
        if parent.kind == NodeKind::LambdaExpression {
            return None;
        }

        if parent.is_assignment() {
            let left = parent.child_by_field_name("left")?;
            let right = parent.child_by_field_name("right")?;
            let operator = parent.child_by_field_name("operator")?;
            if operator.text() != ":=" {
                return None;
            }
            if !right.contains(function_node) {
                return None;
            }
            return method_installation_signature(left).map(|(_, domain)| domain);
        }

        current = parent;
    }

    None
}

fn method_installation_assignment_for_callable_node<'tree>(
    node: M2Node<'tree>,
) -> Option<M2Node<'tree>> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        current = parent;

        if !current.is_space_application() {
            continue;
        }

        let callable = current.child_by_field_name("left")?;
        if !callable.contains(node) {
            continue;
        }

        if is_colon_equal_assignment_left(current) {
            return current.parent();
        }
    }

    None
}

/// The first direct clause of `node` of the given kind (`then`/`else`/`do`/…).
fn clause_of(node: M2Node, kind: NodeKind) -> Option<M2Node> {
    node.named_children().find(|child| child.kind == kind)
}

/// The value expression a clause wraps (`then E` → `E`): its single named child.
fn clause_value(clause: M2Node) -> Option<M2Node> {
    clause.named_children().next()
}

/// Whether a node kind is a `try` clause (so the remaining named child is the
/// guarded body).
fn is_try_clause(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::ThenClause
            | NodeKind::ElseClause
            | NodeKind::ExceptClause
            | NodeKind::DoClause
            | NodeKind::WhenClause
    )
}

fn is_loop_clause(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::FromClause
            | NodeKind::ToClause
            | NodeKind::InClause
            | NodeKind::WhenClause
            | NodeKind::ListClause
            | NodeKind::DoClause
    )
}

fn child_scope_policy(parent: M2Node<'_>, child: M2Node<'_>) -> Option<ChildScopePolicy> {
    match parent.kind {
        NodeKind::IfStatement => {
            let is_condition = parent
                .child_by_field_name("condition")
                .is_some_and(|condition| condition.id() == child.id());
            (is_condition || matches!(child.kind, NodeKind::ThenClause | NodeKind::ElseClause))
                .then_some(ChildScopePolicy::CONDITIONAL)
        }
        NodeKind::TryStatement => {
            let is_body = parent
                .named_child(0)
                .is_some_and(|body| body.id() == child.id());
            (is_body || is_try_clause(child.kind)).then_some(ChildScopePolicy::CONDITIONAL)
        }
        NodeKind::ForStatement => {
            is_loop_clause(child.kind).then_some(ChildScopePolicy::LOOP_CLAUSE)
        }
        NodeKind::WhileStatement => {
            let is_condition = parent
                .named_child(0)
                .is_some_and(|condition| condition.id() == child.id());
            (is_condition || is_loop_clause(child.kind)).then_some(ChildScopePolicy::LOOP_CLAUSE)
        }
        _ => None,
    }
}

/// The value a node denotes, peeling parenthesized grouping: `(a)` → `a`,
/// `((a))` → `a`. A parenthesized expression whose final child is `muted`
/// (`(a;)`) denotes null, so it has no value node. A non-parenthesized node is
/// its own value. `()` and `(a, b)` are `Sequence` nodes, left untouched.
fn parenthesized_value(node: M2Node) -> Option<M2Node> {
    let mut current = node;
    while current.kind == NodeKind::ParenthesizedExpression {
        current = current.final_value_child()?;
    }
    Some(current)
}

pub fn method_installation_domain(node: M2Node) -> Option<Vec<ObjectName>> {
    let node = parenthesized_value(node)?;
    if matches!(node.kind, NodeKind::Sequence | NodeKind::List) {
        // Each element is one dispatch position, so the arity is the count of
        // them — that must be preserved exactly. A non-symbol element is still a
        // real position: `f(ZZ, a.b) := …` installs at arity 2, because `a.b`
        // evaluates to a type at install time. We just cannot resolve its type
        // name statically, so we keep the position under its source text (which
        // will not match any known type → an unresolved parameter type) rather
        // than dropping it and under-counting the arity. Comments ride along as
        // named children and are not dispatch positions.
        let domain = node
            .collection_elements()
            .map(|child| ObjectName::new(symbol_node_text(child).unwrap_or_else(|| child.text())))
            .collect::<Vec<_>>();
        return (!domain.is_empty()).then_some(domain);
    }

    symbol_node_text(node).map(|name| vec![ObjectName::new(name)])
}

fn is_colon_equal_assignment_left(node: M2Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !parent.is_assignment() {
        return false;
    }
    if parent
        .child_by_field_name("left")
        .is_none_or(|left| left.id() != node.id())
    {
        return false;
    }

    parent
        .child_by_field_name("operator")
        .is_some_and(|operator| operator.text() == ":=")
}

fn is_range_smaller(a: TextRange, b: TextRange) -> bool {
    // Very simple check: is a contained in b?
    let starts_inside = a.start.line > b.start.line
        || (a.start.line == b.start.line && a.start.character >= b.start.character);
    let ends_inside =
        a.end.line < b.end.line || (a.end.line == b.end.line && a.end.character <= b.end.character);
    starts_inside && ends_inside && a != b
}
