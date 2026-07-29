//! Parse-tree analysis that records lexical bindings, static type facts, and
//! diagnostics for one document snapshot.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::RwLock;
use tower_lsp::lsp_types::{Diagnostic, Position, Range, SymbolKind};

use crate::diagnostic_registry::M2Diagnostic;
use crate::meta::{BindingRole, Meta, Metadata};
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::object_registry::ObjectName;
use crate::object_registry::{ObjectId, OperatorForm, Type, TypeData, TypeId, TypeStore};
use crate::source::SourceNavigation;
use crate::typesystem::{
    type_is_subtype, InferredType, LiteralOption, PositionedTypeKnowledge, TypeKnowledge,
};
use crate::util::position_in_range;

/// Snapshot-local identity of one lexical binding declaration.
/// Reassignments keep this identity and create new [`BindingStateId`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingId(u32);

/// Snapshot-local identity of one source-ordered state of a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingStateId(u32);

/// Typed parser-node identity used only by the transient inference cache while
/// one analysis snapshot is being constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NodeFactId(usize);

/// Complete semantic analysis of one immutable document snapshot.
/// It owns source facts, characterized method installations, and diagnostics;
/// expression types are not retained after inference.
#[derive(Debug)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub registry: SemanticRegistry,
    cache_types: bool,
    type_cache: RwLock<HashMap<NodeFactId, InferredType>>,
}

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

/// Typed source-history position of one method-installing assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct InstallationIndex(usize);

/// A named M2 method object identified by its callable and dispatch domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub id: ObjectId,
    pub head: MethodHead,
    pub domain: Vec<ObjectName>,
    /// The effective codomain of this method at its installation.
    pub codomain: Option<ObjectName>,
}

impl Method {
    fn new(head: MethodHead, domain: Vec<ObjectName>, codomain: Option<ObjectName>) -> Self {
        let head_name = match &head {
            MethodHead::Function(name) => name.name(),
            MethodHead::Operator(operator) => operator.token.name(),
        };
        let domain_name = domain
            .iter()
            .map(ObjectName::name)
            .collect::<Vec<_>>()
            .join(",");
        Self {
            id: ObjectId::new(format!("{head_name}({domain_name})")),
            head,
            domain,
            codomain,
        }
    }
}

/// A characterized assignment that installs one named [`Method`].
///
/// It is produced once during analysis and consumed by every capability instead
/// of each re-deciding installation syntax. `span` covers the whole assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInstallation {
    index: InstallationIndex,
    pub method: Method,
    pub codomain_span: Option<SpanKey>,
    pub span: SpanKey,
    pub target: SpanKey,
    pub value: Option<SpanKey>,
    /// Required arity of the installed function. Assignment handlers receive
    /// the assigned value in addition to the operands in `domain`.
    expected_rhs_arity: usize,
    pub rhs_lambda_dispatch: Option<Dispatch>,
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
    pub methods: Vec<ObjectId>,
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

/// Value-semantic source location used to key facts independently of borrowed
/// syntax-tree nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanKey {
    pub range: Range,
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
    pub range: Range,
    pub scope_idx: usize,
    pub declaration_range: Range,
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
    pub value_range: Option<Range>,
    pub span: SpanKey,
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
    pub range: Range,
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

/// One partial-order view combining source type edges with external registry
/// type edges without copying either population.
struct SourceTypeOrder<'analysis, Knowledge: ?Sized> {
    source: &'analysis SourceTypeFacts,
    external: &'analysis Knowledge,
}

impl<Knowledge: TypeKnowledge + ?Sized> TypeStore for SourceTypeOrder<'_, Knowledge> {
    fn parent_type_id(&self, type_id: &TypeId) -> Option<TypeId> {
        self.source
            .data
            .get(type_id)
            .map(|data| data.parent.clone())
            .or_else(|| {
                self.external
                    .object(type_id.object())?
                    .type_info()
                    .map(|data| data.parent.clone())
            })
    }
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
    methods: HashMap<ObjectId, InstallationIndex>,
    installations: Vec<MethodInstallation>,
    source_types: SourceTypeFacts,
    ring_generators: HashMap<ObjectName, Vec<RingGenerator>>,
}

impl SpanKey {
    fn from_node(source: &(impl SourceNavigation + ?Sized), node: M2Node) -> Self {
        Self {
            range: source.range_for_node(node),
        }
    }
}

impl Hash for SpanKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.range.start.line.hash(state);
        self.range.start.character.hash(state);
        self.range.end.line.hash(state);
        self.range.end.character.hash(state);
    }
}

impl Analysis {
    pub fn find_definition(&self, name: &str, pos: Position) -> Option<Range> {
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
                    state.scope_idx == idx && (!constrain_to_prior || state.span.range.start <= pos)
                })
                .max_by_key(|state| {
                    (
                        state.span.range.start.line,
                        state.span.range.start.character,
                    )
                });
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
        let span = SpanKey::from_node(source, node);
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
            .filter_map(|method| self.method(method.object_name()))
    }

    /// Methods installed on `function` no later than `position`, with a later
    /// installation of the same method identity shadowing an earlier one.
    fn methods_for_at<'a>(
        &'a self,
        function: &'a FunctionInfo,
        position: Position,
    ) -> Vec<&'a Method> {
        let method_ids = function.methods.iter().collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        self.registry
            .installations
            .iter()
            .rev()
            .filter(|installation| installation.span.range.start <= position)
            .map(|installation| &installation.method)
            .filter(|method| method_ids.contains(&method.id))
            .filter(|method| seen.insert(method.id.clone()))
            .collect()
    }

    fn method_installation(&self, index: InstallationIndex) -> Option<&MethodInstallation> {
        self.registry.installations.get(index.0)
    }

    /// Resolve the current state of a named local method object.
    pub fn method(&self, name: &ObjectName) -> Option<&Method> {
        let index = self.registry.methods.get(name)?;
        Some(&self.method_installation(*index)?.method)
    }

    /// Borrow every characterized method installation in source order.
    pub fn installations(&self) -> &[MethodInstallation] {
        &self.registry.installations
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

    pub fn typed_bindings_in_range(&self, range: Range) -> Vec<BindingView<'_>> {
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
        let mut best_range: Option<Range> = None;

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
                    range: Range::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                    parent_idx: None,
                    context_assignments_may_escape: false,
                }],
                ..Default::default()
            },
            cache_types: false,
            type_cache: RwLock::new(HashMap::new()),
        };
        // Analysis-first: derive bindings, scopes, and method installations
        // before running diagnostics that consume those facts.
        analysis.build_scopes(root, source, 0, 0, knowledge);
        // Scope construction needs source-ordered partial information. Once all
        // bindings and states exist, inference is stable and can be memoized
        // transiently while the remaining semantic consumers run.
        analysis.cache_types = true;
        analysis.collect_installation_diagnostics(knowledge);
        analysis.collect_install_form_diagnostics(root, source, knowledge);
        analysis.collect_diagnostics(root, source, knowledge);
        analysis.collect_unused_binding_diagnostics(root, source);
        analysis.cache_types = false;
        analysis
            .type_cache
            .get_mut()
            .expect("type cache lock should not be poisoned")
            .clear();
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
                    self.record_method_installation(node, source, &knowledge);
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
                            self.type_of(right, source, current_scope_idx, &knowledge)
                                .dispatch_id()
                        }
                    });
                    let parent_type = right
                        .and_then(|right| {
                            declared_type_parent(right, type_name.as_ref(), &knowledge)
                        })
                        .and_then(|parent| self.resolve_type_id(&parent, &knowledge));

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
                            || type_is_subtype(&knowledge, type_name, &ObjectName::new("Ring"))
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
        let span = SpanKey::from_node(source, node);
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
        index: InstallationIndex,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<MethodInstallation> {
        let operator = node.binary_operator()?;
        let left = node.child_by_field_name("left")?;
        let (head, domain) = self.installation_shape(left, knowledge)?;
        let operand_arity = domain.len();
        let span = SpanKey::from_node(source, node);
        let target = SpanKey::from_node(source, left);
        let right = node.child_by_field_name("right");
        let value = right.map(|right| SpanKey::from_node(source, right));
        let codomain_node = right
            .filter(|right| right.is_option_assignment())
            .and_then(|right| right.child_by_field_name("left"));
        let codomain = codomain_node
            .and_then(symbol_node_text)
            .map(ObjectName::new);
        let codomain_span = codomain_node.map(|node| SpanKey::from_node(source, node));
        // The RHS function shape, read once here so the arity diagnostic need not
        // re-walk the tree. Only a plain lambda RHS carries a checkable arity.
        let rhs_lambda_dispatch = node
            .child_by_field_name("right")
            .filter(|right| right.kind == NodeKind::LambdaExpression)
            .and_then(function_dispatch);

        match operator {
            // `:=` installs by shape alone — no type check on the operands.
            ":=" => Some(MethodInstallation {
                index,
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
                        index,
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
            let knowledge = knowledge_provider.at_position(installation.span.range.start);
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
                        installation.span.range,
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
                        installation.span.range,
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
                    installation.span.range,
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
            span: SpanKey::from_node(source, node),
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
            span: SpanKey::from_node(source, registration.node),
            scope_idx: registration.scope_idx,
        });
        self.registry
            .states_by_binding
            .entry(binding_id)
            .or_default()
            .push(state_id);
    }

    fn collect_ring_generator_bindings(
        &mut self,
        ring_name: &str,
        expression: M2Node,
        rebind_node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        let containers = expression
            .descendants()
            .filter(|node| node.is_space_application())
            .filter_map(|node| {
                let head = node.child_by_field_name("left")?;
                let variables = ring_constructor_variables(node)?;
                self.type_of(head, source, 0, knowledge)
                    .principal()
                    .is_some_and(|head_type| {
                        type_is_subtype(knowledge, head_type, &ObjectName::new("Ring"))
                    })
                    .then_some(variables)
            })
            .collect::<Vec<_>>();

        let mut generators = Vec::new();
        for container in containers {
            for generator in ring_generator_bindings(container) {
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
                self.binding_definition(*binding_id)
                    .is_some_and(|binding| binding.scope_idx == 0)
            });
        let registration = SymbolRegistration {
            kind: SymbolKind::VARIABLE,
            role: BindingRole::Ordinary,
            type_name: Some(type_name),
            indexed_element_type,
            parent_type: None,
            node,
            value_node: None,
            scope_idx: 0,
            potential_export: true,
        };
        if let Some(binding_id) = binding_id {
            self.add_binding_state(binding_id, registration, source);
        } else {
            self.add_symbol(name, registration, source);
        }
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
            .contains(&installation.method.id)
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
        self.infer_call_facts(node, source, scope_idx, knowledge)
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
        self.type_of(node, source, scope_idx, knowledge)
            .dispatch_id()
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
        self.type_of(node, source, scope_idx, knowledge).label()
    }

    /// Project inferred types into the nominal names understood by method
    /// dispatch. Locally-created runtime types (most importantly a ring
    /// such as `R = QQ[x]`) walk through the local parent registry first, so an
    /// element whose exact class is `R` dispatches as a `RingElement`.
    pub fn dispatch_argument_types(
        &self,
        facts: &CallStaticFacts,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Vec<Option<ObjectName>> {
        facts
            .argument_types
            .iter()
            .map(|inferred| self.dispatch_type_id(inferred, knowledge))
            .collect()
    }

    fn dispatch_type_id(
        &self,
        inferred: &InferredType,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<ObjectName> {
        let principal = inferred.principal()?;
        let Some(mut current) = self.resolve_source_type_id(principal) else {
            return Some(principal.clone());
        };
        let mut visited = HashSet::new();

        loop {
            if !visited.insert(current.clone()) {
                return None;
            }
            let Some(data) = self.registry.source_types.data.get(&current) else {
                return Some(
                    knowledge
                        .object(current.object())
                        .map(|record| record.name.clone())
                        .unwrap_or_else(|| ObjectName::new(current.object().name())),
                );
            };
            current.clone_from(&data.parent);
        }
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
    ) {
        let index = InstallationIndex(self.registry.installations.len());
        let Some(mut installation) =
            self.classify_installation(index, assignment, source, knowledge)
        else {
            return;
        };

        // Preserve M2's distinct assignment-method form: only `:=` contributes
        // a callable signature here. `=` installations are retained for
        // diagnostics/document symbols but are not ordinary call methods.
        if assignment.binary_operator() == Some(":=") {
            self.attach_method_installation(&mut installation, knowledge);
        }

        debug_assert_eq!(installation.index.0, self.registry.installations.len());
        self.registry.installations.push(installation);
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
        let method_id = installation.method.id.clone();
        if !method.methods.contains(&method_id) {
            method.methods.push(method_id.clone());
        }
        self.registry.methods.insert(method_id, installation.index);
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

    /// The inferred type of the value `node` evaluates to — see [`InferredType`].
    /// Every value-producing node has a type; control-flow and unhandled forms
    /// fall to `Unknown`. The bound is a lower bound (a `typicalValue`), never
    /// asserted exact.
    fn type_of(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        if !self.cache_types {
            return self.compute_type_of(node, source, scope_idx, knowledge);
        }

        let node_id = NodeFactId(node.id());
        if let Some(inferred) = self
            .type_cache
            .read()
            .expect("type cache lock should not be poisoned")
            .get(&node_id)
        {
            return inferred.clone();
        }

        let inferred = self.compute_type_of(node, source, scope_idx, knowledge);
        self.type_cache
            .write()
            .expect("type cache lock should not be poisoned")
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
                InferredType::of("MethodFunction")
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
            NodeKind::StringLiteral => InferredType::of("String"),
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
        if let Some(binding) =
            self.get_binding_from_scope(name, scope_idx, source.position_for_node(node))
        {
            let package_shadows = binding.scope_idx == 0
                && knowledge.shadows_source(&binding.name, binding.state.span.range.start);
            if !package_shadows {
                return binding
                    .state
                    .type_name
                    .as_ref()
                    .map_or_else(InferredType::unknown, |type_name| {
                        InferredType::from_id(type_name.clone())
                    });
            }
        }

        if let Some(record) = knowledge.get_record(&ObjectName::new(name)) {
            return InferredType::from_id(record.class.clone());
        }

        InferredType::of("Symbol")
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

        let (Some(operator), Some(left), Some(right)) = (operator, left, right) else {
            return InferredType::unknown();
        };
        let left_type = self.type_of(left, source, scope_idx, knowledge);
        let right_type = self.type_of(right, source, scope_idx, knowledge);
        self.dispatch_codomain(knowledge, operator, &[left_type, right_type], &[])
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

        if operator == "_" {
            if left_name.as_ref() == "Symbol" {
                return Some(InferredType::of("IndexedVariable"));
            }
            if left_name.as_ref() == "IndexedVariableTable" {
                if let Some(element_type) = self
                    .get_binding_from_scope(left.text(), scope_idx, source.position_for_node(left))
                    .and_then(|binding| binding.state.indexed_element_type.as_ref())
                {
                    return Some(InferredType::of(element_type.name()));
                }
                return Some(InferredType::of("RingElement"));
            }
        }

        if matches!(operator, "_" | "@@")
            && type_is_subtype(knowledge, left_name, &ObjectName::new("Function"))
        {
            return Some(InferredType::of("FunctionClosure"));
        }

        if operator == "/" && type_is_subtype(knowledge, left_name, &ObjectName::new("Ring")) {
            let right_type = self.type_of(right?, source, scope_idx, knowledge);
            let right_name = right_type.principal()?;
            if right_name.as_ref() == "ZZ" {
                return Some(InferredType::of("QuotientRing"));
            }
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
        }

        // Otherwise the lattice decides whether the head is a function (delegating
        // to its signatures) or another SPACE method (`Ring × Array →
        // PolynomialRing`).
        let head = self.type_of(callable_node, source, scope_idx, knowledge);
        let head_is_function = head
            .principal()
            .is_some_and(|head| type_is_subtype(knowledge, head, &ObjectName::new("Function")));
        if head_is_function {
            if let Some(callable) = callable_name {
                if let Some(return_type) = knowledge.resolve_call_return_type_with_options(
                    &ObjectName::new(callable),
                    &self.dispatch_argument_types(&call_facts, knowledge),
                    &call_facts.literal_options,
                ) {
                    return InferredType::from_id(return_type);
                }
            }
            // Applying a function yields at least a Thing.
            return InferredType::of("Thing");
        }

        if let Some(result) = self.ring_application_with_trailing_operator_type(
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
            SPACE_OPERATOR,
            &[head, argument_type],
            &call_facts.literal_options,
        )
    }

    /// Square-bracket ring construction binds specially in Macaulay2 source:
    /// `R[x]/I` has a CST shaped like `R SPACE ([x] / I)`, while evaluation
    /// constructs `R[x]` before applying `/ I`. Preserve the parser's grouping,
    /// but lower that one type-directed application chain through the same
    /// dispatch table used for ordinary operators.
    fn ring_application_with_trailing_operator_type(
        &self,
        head: &InferredType,
        argument: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let head_name = head.principal()?;
        if !type_is_subtype(knowledge, head_name, &ObjectName::new("Ring")) {
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
            SPACE_OPERATOR,
            &[head.clone(), variables_type],
            &[],
        );
        let ring_name = ring_type.principal()?;
        if !type_is_subtype(knowledge, ring_name, &ObjectName::new("Ring")) {
            return None;
        }

        let trailing_type = self.type_of(trailing_operand, source, scope_idx, knowledge);
        let result = knowledge.resolve_call_return_type_with_options(
            &ObjectName::new(operator),
            &[
                self.dispatch_type_id(&ring_type, knowledge),
                self.dispatch_type_id(&trailing_type, knowledge),
            ],
            &[],
        )?;
        Some(InferredType::from_id(result))
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
        let binding = self
            .get_binding_from_scope(name, scope_idx, position)
            .filter(|binding| binding.state.kind == SymbolKind::FUNCTION)?;
        if binding.scope_idx == 0
            && knowledge.shadows_source(&binding.name, binding.state.span.range.start)
        {
            return None;
        }
        self.registry.functions.get(name)
    }

    /// A prefix/postfix operator's type: `typicalValue(op, operand)`.
    fn unary_operator_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let (Some(operator), Some(operand)) =
            (operator_text(node), node.child_by_field_name("operand"))
        else {
            return InferredType::unknown();
        };
        let operand_type = self.type_of(operand, source, scope_idx, knowledge);
        self.dispatch_codomain(knowledge, operator, &[operand_type], &[])
    }

    /// Dispatch `callable` on `args` through the M2 type table. A matched but
    /// undocumented codomain is `Thing` (≡ a null `typicalValue` under the
    /// lower-bound reading) — approximated by "the callable/operator resolves to
    /// a known object, so it dispatches"; an unidentifiable head stays `Unknown`.
    fn dispatch_codomain(
        &self,
        knowledge: &(impl TypeKnowledge + ?Sized),
        callable: &str,
        args: &[InferredType],
        options: &[LiteralOption],
    ) -> InferredType {
        if let Some(function) = self.registry.functions.get(callable) {
            if let Some(return_type) = self.resolve_local_call_return_type(
                function,
                args,
                Position::new(u32::MAX, u32::MAX),
                knowledge,
            ) {
                return InferredType::from_id(return_type);
            }
        }
        if let Some(return_type) = knowledge.resolve_call_return_type_with_options(
            &ObjectName::new(callable),
            &args
                .iter()
                .map(|argument| self.dispatch_type_id(argument, knowledge))
                .collect::<Vec<_>>(),
            options,
        ) {
            return InferredType::from_id(return_type);
        }
        if knowledge.get_record(&ObjectName::new(callable)).is_some() {
            return InferredType::of("Thing");
        }
        InferredType::unknown()
    }

    fn infer_call_facts(
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
            .filter(|signature| self.signature_matches(signature, argument_types, knowledge))
            .filter_map(|signature| signature.codomain.as_ref())
            .cloned()
            .collect::<HashSet<_>>();

        if matching_codomains.len() == 1 {
            return matching_codomains.into_iter().next();
        }

        function.typical_value.clone()
    }

    fn signature_matches(
        &self,
        signature: &Method,
        argument_types: &[InferredType],
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        self.signature_matches_domain(&signature.domain, argument_types, knowledge)
    }

    fn signature_matches_domain(
        &self,
        expected_domain: &[ObjectName],
        argument_types: &[InferredType],
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        expected_domain.len() == argument_types.len()
            && expected_domain
                .iter()
                .zip(argument_types)
                .all(|(expected, actual)| {
                    actual
                        .principal()
                        .is_some_and(|actual| self.is_subtype(actual, expected, knowledge))
                })
    }

    fn is_subtype(
        &self,
        actual: &ObjectName,
        expected: &ObjectName,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        let order = SourceTypeOrder {
            source: &self.registry.source_types,
            external: knowledge,
        };
        self.resolve_ordered_type(actual, knowledge, &order)
            .zip(self.resolve_ordered_type(expected, knowledge, &order))
            .is_some_and(|(actual, expected)| actual <= expected)
    }

    fn resolve_source_type_id(&self, name: &ObjectName) -> Option<TypeId> {
        self.registry.source_types.by_name.get(name).cloned()
    }

    fn resolve_type_id(
        &self,
        name: &ObjectName,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<TypeId> {
        self.resolve_source_type_id(name).or_else(|| {
            knowledge
                .resolve_type(name)
                .map(|type_data| type_data.id().clone())
        })
    }

    fn resolve_ordered_type<'a, Knowledge: TypeKnowledge + ?Sized>(
        &'a self,
        name: &ObjectName,
        knowledge: &'a Knowledge,
        order: &'a SourceTypeOrder<'a, Knowledge>,
    ) -> Option<Type<'a>> {
        let id = self.resolve_type_id(name, knowledge)?;
        Some(Type::new(id, order))
    }

    pub fn binding_id_at(&self, name: &str, pos: Position) -> Option<BindingId> {
        let scope_idx = self.find_scope_at(pos)?;
        self.binding_id_from_scope(name, scope_idx, pos)
    }
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

/// A ring-generator name and source node extracted from constructor syntax
/// before it is registered as a binding.
#[derive(Debug, Clone)]
struct RingGeneratorBinding<'a> {
    name: String,
    kind: RingGeneratorKind,
    node: M2Node<'a>,
}

/// Runtime binding shape produced for a ring generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RingGeneratorKind {
    Direct,
    IndexedTable,
}

/// Compact reference to a registered generator retained for later ring
/// rebinding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RingGenerator {
    name: ObjectName,
    kind: RingGeneratorKind,
}

fn ring_constructor_variables(application: M2Node<'_>) -> Option<M2Node<'_>> {
    let argument = application.child_by_field_name("right")?;
    if argument.kind.is_collection_expression() {
        return Some(argument);
    }
    argument
        .child_by_field_name("left")
        .filter(|left| left.kind.is_collection_expression())
}

fn ring_generator_bindings(container: M2Node<'_>) -> Vec<RingGeneratorBinding<'_>> {
    let elements = container.collection_elements().collect::<Vec<_>>();
    let variable_base = elements
        .iter()
        .find_map(|element| option_value_node(*element, "VariableBaseName"))
        .and_then(generator_base_name);
    let mut bindings = Vec::new();

    for element in elements {
        if element.is_option_assignment() {
            if let Some(variables) = option_value_node(element, "Variables") {
                if variables.kind == NodeKind::IntegerLiteral {
                    let name = variable_base.clone().unwrap_or_else(|| "p".to_string());
                    push_ring_generator(
                        &mut bindings,
                        RingGeneratorBinding {
                            name,
                            kind: RingGeneratorKind::IndexedTable,
                            node: variables,
                        },
                    );
                } else {
                    collect_ring_generator_spec(variables, &mut bindings);
                }
            }
            continue;
        }
        collect_ring_generator_spec(element, &mut bindings);
    }

    bindings
}

fn option_value_node<'tree>(node: M2Node<'tree>, key: &str) -> Option<M2Node<'tree>> {
    if !node.is_option_assignment() {
        return None;
    }
    node.child_by_field_name("left")
        .filter(|left| left.kind == NodeKind::Symbol && left.text() == key)?;
    node.child_by_field_name("right")
}

fn generator_base_name(node: M2Node<'_>) -> Option<String> {
    match node.kind {
        NodeKind::Symbol => Some(node.text().to_string()),
        NodeKind::StringLiteral => node.string_literal_inner_text().map(ToString::to_string),
        _ => None,
    }
}

fn collect_ring_generator_spec<'tree>(
    node: M2Node<'tree>,
    bindings: &mut Vec<RingGeneratorBinding<'tree>>,
) {
    if node.kind == NodeKind::Symbol {
        push_ring_generator(
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
            push_ring_generator(
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
                (indexed_variable_base(left), indexed_variable_base(right))
            {
                if left_base.text() == right_base.text() {
                    push_ring_generator(
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

            if let Some(names) = simple_symbol_range(node, left, right) {
                for name in names {
                    push_ring_generator(
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
            collect_ring_generator_spec(element, bindings);
        }
    }
}

fn indexed_variable_base(node: M2Node<'_>) -> Option<M2Node<'_>> {
    (node.binary_operator() == Some("_"))
        .then(|| node.child_by_field_name("left"))
        .flatten()
        .filter(|base| base.kind == NodeKind::Symbol)
}

fn simple_symbol_range(
    range: M2Node<'_>,
    left: M2Node<'_>,
    right: M2Node<'_>,
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

fn push_ring_generator<'tree>(
    bindings: &mut Vec<RingGeneratorBinding<'tree>>,
    binding: RingGeneratorBinding<'tree>,
) {
    if !bindings
        .iter()
        .any(|existing| existing.name == binding.name)
    {
        bindings.push(binding);
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
    if type_name.is_some_and(|type_name| {
        type_name.name() == "Ring"
            || type_is_subtype(knowledge, type_name, &ObjectName::new("Ring"))
    }) {
        // A ring value is itself a runtime type. Its elements have that ring as
        // their class, while the ring's instance hierarchy starts at
        // `RingElement` (`parent R === RingElement`).
        return Some(ObjectName::new("RingElement"));
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
) -> Range {
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
    type_name.name() == "Type" || type_is_subtype(knowledge, type_name, &ObjectName::new("Type"))
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

fn is_range_smaller(a: Range, b: Range) -> bool {
    // Very simple check: is a contained in b?
    let starts_inside = a.start.line > b.start.line
        || (a.start.line == b.start.line && a.start.character >= b.start.character);
    let ends_inside =
        a.end.line < b.end.line || (a.end.line == b.end.line && a.end.character <= b.end.character);
    starts_inside && ends_inside && a != b
}
