//! Parse-tree analysis that records lexical bindings, static type facts, and
//! diagnostics for one document snapshot.

mod diagnostics;
mod ring;
mod typechecker;

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Deref;
use tower_lsp::lsp_types::{Position, Range as TextRange, SymbolKind};

use crate::diagnostic_registry::{DiagnosticKind, M2Diagnostic};
use crate::meta::{BindingRole, Meta, Metadata};
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::object_registry::ObjectName;
use crate::object_registry::{ObjectId, OperatorForm, TypeData, TypeId};
use crate::semantic_token::{syntax_semantic_token_type, SourceSemanticRole, SourceSemanticToken};
use crate::source::SourceNavigation;
use crate::typesystem::{
    InferredType, LiteralOption, PositionedTypeKnowledge, TypeKnowledge, TypeRole,
};
use crate::util::position_in_range;
use typechecker::TypeChecker;

/// Identity of one scoped symbol binding within an immutable analysis snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingId(u32);

/// An M2 operator — including `SPACE`, the juxtaposition operator (`X Y` is
/// `X SPACE Y`). Just another operator, not a special "adjacency" concept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Operator {
    pub token: ObjectName,
    pub form: OperatorForm,
}

impl Operator {
    fn from_expression(node: M2Node<'_>) -> Option<Self> {
        let form = match node.kind {
            NodeKind::BinaryExpression => OperatorForm::Binary,
            NodeKind::PrefixExpression => OperatorForm::Prefix,
            NodeKind::PostfixExpression => OperatorForm::Postfix,
            _ => return None,
        };
        let token = node.child_by_field_name("operator")?;
        Some(Self {
            token: ObjectName::new(if token.is_implicit_application() {
                token.syntax_label()
            } else {
                token.text()
            }),
            form,
        })
    }

    fn is_assignment(&self) -> bool {
        self.form == OperatorForm::Binary && matches!(self.token.name(), "=" | ":=" | "<-")
    }

    fn is_option_assignment(&self) -> bool {
        self.form == OperatorForm::Binary && self.token.name() == "=>"
    }
}

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

/// Identity of one statically known source-created runtime object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceObjectId(usize);

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
    pub span: TextRange,
    expected_rhs_arity: usize,
    pub rhs_lambda_dispatch: Option<Dispatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceRangeKey(TextRange);

impl Ord for SourceRangeKey {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.0.start, self.0.end).cmp(&(other.0.start, other.0.end))
    }
}

impl PartialOrd for SourceRangeKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl MethodInstallation {
    /// The argument count the right-hand-side function must take.
    pub fn expected_rhs_arity(&self) -> usize {
        self.expected_rhs_arity
    }
}

/// Syntax-derived callable behavior used to decide whether method
/// installations take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalFunctionKind {
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
    pub typical_value: Option<ObjectName>,
    pub installations: Vec<MethodInstallationId>,
    pub dispatch: Option<Dispatch>,
    kind: LocalFunctionKind,
}

/// Static facts owned by one source-created runtime object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceObject {
    pub class: ObjectName,
    pub function: FunctionInfo,
}

/// Static facts computed for one call after separating positional arguments
/// from literal option assignments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStaticFacts {
    pub argument_types: Vec<InferredType>,
    pub literal_options: Vec<LiteralOption>,
}

/// Source-independent identity and editor-facing anchor of one lexical binding.
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
    pub states: Vec<BindingStateInfo>,
}

/// One source-ordered value and inferred-type state of a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingStateInfo {
    pub presentation_kind: SymbolKind,
    pub type_name: Option<ObjectName>,
    pub object_id: Option<SourceObjectId>,
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
    type Target = BindingInfo;

    fn deref(&self) -> &Self::Target {
        self.binding
    }
}

impl Metadata for BindingView<'_> {
    fn meta(&self) -> Meta<'_> {
        Meta {
            symbol_kind: Some(self.state.presentation_kind),
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
    pub assignments_may_escape: bool,
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
    pub bindings_by_name: HashMap<ObjectName, Vec<BindingId>>,
    pub objects: Vec<SourceObject>,
    operator_functions: HashMap<Operator, FunctionInfo>,
    installations: Vec<MethodInstallation>,
    pending_source_semantic_roles: BTreeMap<SourceRangeKey, SourceSemanticRole>,
    source_semantic_tokens: Vec<SourceSemanticToken>,
    source_types: SourceTypeFacts,
    ring_generators: HashMap<ObjectName, Vec<ring::RingGenerator>>,
}

/// Binding-registration policy selected from the assignment operator.
#[derive(Debug, Clone, Copy)]
enum DefinitionScope {
    Local,
    Global,
}

#[derive(Clone, Copy)]
enum OutputReference {
    Relative(usize),
    Absolute(usize),
}

impl OutputReference {
    fn parse(name: &str) -> Option<Self> {
        let bytes = name.as_bytes();
        if (2..=4).contains(&bytes.len()) && bytes.iter().all(|byte| *byte == b'o') {
            return Some(Self::Relative(bytes.len() - 1));
        }
        let number = name.strip_prefix('o')?;
        (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| number.parse().ok().map(Self::Absolute))
            .flatten()
    }
}

/// Complete input for creating a binding or adding a new state to one.
///
/// Keeping this packet typed ensures all registration paths pass through the
/// same bookkeeping code.
#[derive(Debug, Clone)]
struct SymbolRegistration {
    presentation_kind: SymbolKind,
    role: BindingRole,
    type_name: Option<ObjectName>,
    object_id: Option<SourceObjectId>,
    indexed_element_type: Option<ObjectName>,
    parent_type: Option<TypeId>,
    scope_idx: usize,
    potential_export: bool,
}

#[derive(Debug, Clone, Copy)]
enum ControlFlowScope {
    Branch,
    LoopClause,
}

impl ControlFlowScope {
    fn assignment_scope(self, nested_scope: usize, inherited_scope: usize) -> usize {
        match self {
            Self::Branch => nested_scope,
            Self::LoopClause => inherited_scope,
        }
    }

    fn assignments_may_escape(self) -> bool {
        matches!(self, Self::Branch)
    }
}

/// Complete semantic analysis of one immutable document snapshot.
/// It owns source facts, characterized method installations, and diagnostics;
/// expression types are not retained after inference.
#[derive(Debug)]
pub struct Analysis {
    pub diagnostics: Vec<M2Diagnostic>,
    pub registry: SemanticRegistry,
}

impl Analysis {
    pub fn find_definition(&self, name: &str, pos: Position) -> Option<TextRange> {
        let scope_idx = self.find_scope_at(pos)?;
        let binding_id = self.binding_id_from_scope(name, scope_idx, pos)?;
        self.binding(binding_id)?
            .states
            .iter()
            .filter(|state| state.value_range.map_or(state.span.end, |value| value.end) <= pos)
            .max_by_key(|state| state.value_range.map_or(state.span.end, |value| value.end))
            .map(|state| state.span)
    }

    pub fn documentation_symbol_at(&self, name: &str, pos: Position) -> Option<BindingView<'_>> {
        self.get_binding_at(name, pos).or_else(|| {
            let mut fallback = None;
            for binding_id in self.registry.bindings_by_name.get(name)? {
                let binding = self.binding_anchor(*binding_id)?;
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

    fn binding_anchor(&self, binding_id: BindingId) -> Option<BindingView<'_>> {
        let binding = self.binding(binding_id)?;
        let state = binding.states.first()?;
        Some(BindingView { binding, state })
    }

    fn binding_state_from_scope(
        &self,
        binding_id: BindingId,
        scope_idx: usize,
        pos: Position,
    ) -> Option<BindingView<'_>> {
        let binding = self.binding(binding_id)?;
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            let constrain_to_prior = idx == scope_idx;
            let state = binding
                .states
                .iter()
                .filter(|state| {
                    state.scope_idx == idx && (!constrain_to_prior || state.span.start <= pos)
                })
                .max_by_key(|state| (state.span.start.line, state.span.start.character));
            if let Some(state) = state {
                return Some(BindingView { binding, state });
            }
            curr = self.registry.scopes[idx].parent_idx;
        }
        self.binding_anchor(binding_id)
    }

    pub fn function_at(&self, name: &str, position: Position) -> Option<&FunctionInfo> {
        let binding = self.get_binding_at(name, position)?;
        self.function_for_binding(binding)
    }

    pub fn method_installation_codomain<'a>(
        &'a self,
        installation: &'a MethodInstallation,
    ) -> Option<&'a str> {
        installation.method.codomain.as_ref().map(ObjectName::name)
    }

    pub fn function_for_binding(&self, binding: BindingView<'_>) -> Option<&FunctionInfo> {
        Some(
            &self
                .registry
                .objects
                .get(binding.state.object_id?.0)?
                .function,
        )
    }

    pub fn methods_for<'a>(
        &'a self,
        function: &'a FunctionInfo,
    ) -> impl Iterator<Item = &'a Method> + 'a {
        function
            .installations
            .iter()
            .rev()
            .filter_map(|id| self.method_installation(*id))
            .fold(Vec::new(), |mut methods, installation| {
                if !methods
                    .iter()
                    .any(|method: &&Method| method.domain == installation.method.domain)
                {
                    methods.push(&installation.method);
                }
                methods
            })
            .into_iter()
            .rev()
    }

    /// Methods installed on `function` no later than `position`, with a later
    /// installation of the same method identity shadowing an earlier one.
    fn methods_for_at<'a>(
        &'a self,
        function: &'a FunctionInfo,
        position: Position,
    ) -> Vec<&'a Method> {
        let mut methods = Vec::new();
        for installation in function
            .installations
            .iter()
            .rev()
            .filter_map(|id| self.method_installation(*id))
            .filter(|installation| installation.span.start <= position)
        {
            if !methods
                .iter()
                .any(|method: &&Method| method.domain == installation.method.domain)
            {
                methods.push(&installation.method);
            }
        }
        methods.reverse();
        methods
    }

    fn method_installation(&self, id: MethodInstallationId) -> Option<&MethodInstallation> {
        self.registry.installations.get(id.0)
    }

    /// Borrow every characterized method installation in source order.
    pub fn installations(&self) -> &[MethodInstallation] {
        &self.registry.installations
    }

    pub fn scope_with_range(&self, range: TextRange) -> Option<usize> {
        self.registry
            .scopes
            .iter()
            .position(|scope| scope.range == range)
    }

    pub fn source_semantic_tokens(&self) -> &[SourceSemanticToken] {
        &self.registry.source_semantic_tokens
    }

    pub fn bindings_in_scope(&self, scope_idx: usize) -> impl Iterator<Item = BindingView<'_>> {
        self.bindings()
            .filter(move |binding| binding.scope_idx == scope_idx)
    }

    pub fn bindings(&self) -> impl Iterator<Item = BindingView<'_>> {
        self.registry
            .bindings
            .iter()
            .filter_map(|binding| self.binding_anchor(binding.binding_id))
    }

    pub fn typed_binding_states_in_range(&self, range: TextRange) -> Vec<BindingView<'_>> {
        self.registry
            .bindings
            .iter()
            .flat_map(|binding| {
                binding
                    .states
                    .iter()
                    .map(|state| BindingView { binding, state })
            })
            .filter(|binding| binding.state.type_name.is_some())
            .filter(|binding| {
                let position = binding.state.span.end;
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
                    out.push((
                        binding.name.name().to_string(),
                        binding.state.presentation_kind,
                    ));
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
                    range: TextRange::new(pos!(), pos_max!()),
                    parent_idx: None,
                    assignments_may_escape: false,
                }],
                ..Default::default()
            },
        };
        // Analysis-first: derive bindings, scopes, and method installations
        // before running diagnostics that consume those facts.
        analysis.build_scopes(root, source, 0, 0, knowledge);
        analysis.registry.pending_source_semantic_roles.clear();
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
        self.record_source_semantic_roles(node, source, &knowledge);
        self.record_source_semantic_token(node, source);
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
                    let type_name = right.and_then(|right| {
                        if method_declaration_typical_value(right).is_some()
                            || is_method_call(right)
                        {
                            Some(TypeRole::MethodFunction.object_name())
                        } else {
                            TypeChecker::new(self)
                                .type_of(right, source, current_scope_idx, &knowledge)
                                .principal()
                                .cloned()
                        }
                    });
                    let parent_type = right.and_then(|right| {
                        self.declared_type_parent(right, type_name.as_ref(), &knowledge)
                    });
                    let presentation_kind = if right.is_some_and(|right| {
                        right.kind == NodeKind::LambdaExpression
                            || method_declaration_typical_value(right).is_some()
                            || is_method_call(right)
                    }) || type_name.as_ref().is_some_and(|type_name| {
                        knowledge.has_type_role(type_name, TypeRole::Function)
                    }) {
                        SymbolKind::FUNCTION
                    } else if type_name.as_ref().is_some_and(|type_name| {
                        TypeChecker::new(self).is_subtype(
                            type_name,
                            &TypeRole::Type.object_name(),
                            &knowledge,
                        )
                    }) {
                        SymbolKind::CLASS
                    } else {
                        SymbolKind::VARIABLE
                    };
                    let target_name = single_symbol_assignment_target(left);
                    let object_id = right.zip(target_name).and_then(|(right, _)| {
                        self.source_object_for_value(
                            right,
                            type_name.as_ref(),
                            source,
                            current_scope_idx,
                        )
                    });

                    match op_text {
                        ":=" => self.collect_definitions(
                            left,
                            right,
                            source,
                            DefinitionScope::Local,
                            SymbolRegistration {
                                presentation_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.clone(),
                                object_id,
                                indexed_element_type: None,
                                parent_type: parent_type.clone(),
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
                            DefinitionScope::Global,
                            SymbolRegistration {
                                presentation_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.clone(),
                                object_id,
                                indexed_element_type: None,
                                parent_type,
                                scope_idx: assignment_scope_idx,
                                potential_export: assignment_scope_idx == 0
                                    || self.registry.scopes[assignment_scope_idx]
                                        .assignments_may_escape,
                            },
                        ),
                        _ => {}
                    }

                    if let (Some(right), Some(type_name), Some(ring_name)) = (
                        right,
                        type_name.as_ref(),
                        single_symbol_assignment_target(left),
                    ) {
                        if knowledge.has_type_role(type_name, TypeRole::Ring) {
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
                match control_flow_scope(node, child) {
                    Some(scope_kind) => {
                        let scope_idx = self.push_scope(
                            child,
                            source,
                            Some(next_scope_idx),
                            scope_kind.assignments_may_escape(),
                        );
                        let assignment_scope_idx =
                            scope_kind.assignment_scope(scope_idx, next_assignment_scope_idx);
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

    fn record_source_semantic_roles(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        if node.is_option_assignment() {
            if let Some(left) = node.child_by_field_name("left") {
                self.register_source_semantic_role(
                    source.range_for_node(left),
                    SourceSemanticRole::OptionKey,
                );
                if let (Some(key), Some(right)) = (
                    symbol_node_text(left).map(ObjectName::new),
                    node.child_by_field_name("right"),
                ) {
                    self.register_source_semantic_role(
                        source.range_for_node(right),
                        SourceSemanticRole::OptionValue(key),
                    );
                }
            }
        }

        if let Some(property) = node.property_key() {
            self.register_source_semantic_role(
                source.range_for_node(property),
                SourceSemanticRole::PropertyKey,
            );
        }

        if node.kind == NodeKind::StringLiteral && indexed_string_names_package(node, knowledge) {
            self.register_source_semantic_role(
                source.range_for_node(node),
                SourceSemanticRole::NamespaceArgument,
            );
        }
    }

    fn register_source_semantic_role(&mut self, range: TextRange, role: SourceSemanticRole) {
        self.registry
            .pending_source_semantic_roles
            .entry(SourceRangeKey(range))
            .or_insert(role);
    }

    fn record_source_semantic_token(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        if node.kind == NodeKind::Symbol && OutputReference::parse(node.text()).is_some() {
            return;
        }
        let syntax_token_type = syntax_semantic_token_type(node);
        let is_symbol = node.kind.is_symbol_like();
        if !is_symbol && syntax_token_type.is_none() {
            return;
        }

        let span = source.span_for_node(node);
        let source_role = self
            .registry
            .pending_source_semantic_roles
            .get(&SourceRangeKey(span.range()))
            .cloned();
        self.registry
            .source_semantic_tokens
            .push(SourceSemanticToken {
                span,
                syntax_token_type,
                source_role,
                is_symbol,
                is_unquoted_symbol: node.kind == NodeKind::Symbol,
            });
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
    ) -> Option<(MethodInstallation, Vec<TextRange>)> {
        let operator = node.binary_operator()?;
        let left = node.child_by_field_name("left")?;
        let (head, domain_nodes) = self.installation_shape(left, knowledge)?;
        let domain = domain_nodes
            .iter()
            .map(|node| ObjectName::new(symbol_node_text(*node).unwrap_or_else(|| node.text())))
            .collect::<Vec<_>>();
        let domain_spans = domain_nodes
            .iter()
            .map(|node| source.range_for_node(*node))
            .collect::<Vec<_>>();
        let operand_arity = domain.len();
        let span = source.range_for_node(node);
        let right = node.child_by_field_name("right");
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
            ":=" => Some((
                MethodInstallation {
                    id,
                    method: Method::new(head, domain, codomain),
                    span,
                    expected_rhs_arity: operand_arity,
                    rhs_lambda_dispatch,
                },
                domain_spans.into_iter().chain(codomain_span).collect(),
            )),
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
                    Some((
                        MethodInstallation {
                            id,
                            method: Method::new(MethodHead::Operator(op), domain, codomain),
                            span,
                            expected_rhs_arity: operand_arity + 1,
                            rhs_lambda_dispatch,
                        },
                        domain_spans.into_iter().chain(codomain_span).collect(),
                    ))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Classify the left side of an assignment into a `(MethodHead, domain)`
    /// pair (the bare, non-assignment head), or `None` if it is not an
    /// installation target at all. The `=`/`:=` rule is applied by the caller.
    fn installation_shape<'tree>(
        &self,
        node: M2Node<'tree>,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<(MethodHead, Vec<M2Node<'tree>>)> {
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
                let operator = Operator::from_expression(node)?;
                if node.is_space_application() {
                    // `A B` (juxtaposition = the SPACE operator): a method on the
                    // named function `A` when `A` is a function, or a SPACE
                    // operator method on the type pair when `A` is a type.
                    let left_name = symbol_node_text(left)?;
                    if self.operand_is_type(left_name, knowledge) {
                        symbol_node_text(right)?;
                        Some((MethodHead::Operator(operator), vec![left, right]))
                    } else {
                        Some((
                            MethodHead::Function(ObjectName::new(left_name)),
                            method_installation_domain_nodes(right)?,
                        ))
                    }
                } else {
                    // `X op Y`: an explicit binary-operator method.
                    if operator.is_assignment() || operator.is_option_assignment() {
                        return None;
                    }
                    symbol_node_text(left)?;
                    symbol_node_text(right)?;
                    Some((MethodHead::Operator(operator), vec![left, right]))
                }
            }
            NodeKind::PrefixExpression => {
                let operand = node.child_by_field_name("operand")?;
                symbol_node_text(operand)?;
                Some((
                    MethodHead::Operator(Operator::from_expression(node)?),
                    vec![operand],
                ))
            }
            NodeKind::PostfixExpression => {
                let operand = node.child_by_field_name("operand")?;
                symbol_node_text(operand)?;
                Some((
                    MethodHead::Operator(Operator::from_expression(node)?),
                    vec![operand],
                ))
            }
            _ => None,
        }
    }

    /// Whether `name` denotes a TYPE — the hinge of the installation rules. The
    /// type universe is layered:
    /// a local binding whose inferred class is `Type` (e.g. `X = new Type of …`)
    /// takes precedence over objects visible in the current environment.
    fn operand_is_type(&self, name: &str, knowledge: &(impl TypeKnowledge + ?Sized)) -> bool {
        self.registry
            .source_types
            .by_name
            .contains_key(&ObjectName::new(name))
            || knowledge
                .get_record(&ObjectName::new(name))
                .is_some_and(|record| record.type_info().is_some())
    }

    fn declared_type_parent(
        &self,
        value: M2Node<'_>,
        type_name: Option<&ObjectName>,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<TypeId> {
        if let Some(parent) = ring::RingSemantics::value_parent(type_name, knowledge) {
            return TypeChecker::new(self).resolve_type_id(&parent, knowledge);
        }
        let type_name = type_name.filter(|type_name| {
            value.kind == NodeKind::NewStatement
                && TypeChecker::new(self).is_subtype(
                    type_name,
                    &TypeRole::Type.object_name(),
                    knowledge,
                )
        })?;
        let parent = clause_of(value, NodeKind::OfClause)
            .and_then(clause_value)
            .and_then(symbol_node_text)
            .map(ObjectName::new)
            .unwrap_or_else(|| type_name.clone());
        TypeChecker::new(self).resolve_type_id(&parent, knowledge)
    }

    fn head_is_method_function(
        &self,
        name: &str,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        if let Some(binding) = self.get_binding_at(name, position) {
            if let Some(function) = self.function_for_binding(binding) {
                return function.kind == LocalFunctionKind::Method;
            }
            return binding.state.type_name.as_ref().is_some_and(|type_name| {
                knowledge.has_type_role(type_name, TypeRole::MethodFunction)
            });
        }
        knowledge
            .get_record(&ObjectName::new(name))
            .and_then(|record| record.callable())
            .is_some_and(|callable| callable.is_method_function())
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
                parameter_node,
                None,
                SymbolRegistration {
                    presentation_kind: SymbolKind::VARIABLE,
                    role: BindingRole::Parameter,
                    type_name,
                    object_id: None,
                    indexed_element_type: None,
                    parent_type: None,
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
        registration: SymbolRegistration,
    ) {
        match node.kind {
            NodeKind::Symbol => {
                let name = node.text();
                match definition_scope {
                    DefinitionScope::Local => {
                        self.add_symbol(name, node, value_node, registration, source)
                    }
                    DefinitionScope::Global => {
                        let position = source.position_for_node(node);
                        let binding_id = self
                            .binding_id_from_scope(name, registration.scope_idx, position)
                            .filter(|binding_id| {
                                self.binding_anchor(*binding_id).is_some_and(|binding| {
                                    binding.scope_idx == registration.scope_idx
                                })
                            });
                        if let Some(binding_id) = binding_id {
                            self.add_binding_state(
                                binding_id,
                                node,
                                value_node,
                                registration,
                                source,
                            );
                        } else {
                            self.add_symbol(name, node, value_node, registration, source);
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
                            object_id: None,
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
        node: M2Node<'_>,
        value_node: Option<M2Node<'_>>,
        registration: SymbolRegistration,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let SymbolRegistration {
            presentation_kind,
            role,
            type_name,
            object_id,
            indexed_element_type,
            parent_type,
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
        let range = source.range_for_node(node);
        let state = BindingStateInfo {
            presentation_kind,
            type_name,
            object_id,
            indexed_element_type,
            value_range: value_node.map(|value| source.range_for_node(value)),
            span: range,
            scope_idx,
        };
        let binding = BindingInfo {
            binding_id,
            name: name.clone(),
            role,
            declaration_kind: presentation_kind,
            potential_export,
            range,
            scope_idx,
            declaration_range: enclosing_definition_range(node, source),
            states: vec![state],
        };
        self.registry.bindings.push(binding);
        self.registry
            .bindings_by_name
            .entry(name)
            .or_default()
            .push(binding_id);
    }

    fn add_binding_state(
        &mut self,
        binding_id: BindingId,
        node: M2Node<'_>,
        value_node: Option<M2Node<'_>>,
        registration: SymbolRegistration,
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
        let state = BindingStateInfo {
            presentation_kind: registration.presentation_kind,
            type_name: registration.type_name,
            object_id: registration.object_id,
            indexed_element_type: registration.indexed_element_type,
            value_range: value_node.map(|value| source.range_for_node(value)),
            span: source.range_for_node(node),
            scope_idx: registration.scope_idx,
        };
        if let Some(binding) = self.registry.bindings.get_mut(binding_id.0 as usize) {
            binding.states.push(state);
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
        let method = self.function_at(name.name(), installation.span.start)?;
        method
            .installations
            .contains(&installation.id)
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

    fn source_object_for_value(
        &mut self,
        value: M2Node,
        type_name: Option<&ObjectName>,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
    ) -> Option<SourceObjectId> {
        if value.kind == NodeKind::ParenthesizedExpression {
            return self.source_object_for_value(
                value.final_value_child()?,
                type_name,
                source,
                scope_idx,
            );
        }
        if value.kind == NodeKind::Symbol {
            return self
                .get_binding_from_scope(value.text(), scope_idx, source.position_for_node(value))?
                .state
                .object_id;
        }

        let function = if let Some(typical_value) = method_declaration_typical_value(value) {
            Some(FunctionInfo {
                typical_value,
                installations: Vec::new(),
                dispatch: None,
                kind: LocalFunctionKind::Method,
            })
        } else if value.kind == NodeKind::LambdaExpression {
            Some(FunctionInfo {
                typical_value: None,
                installations: Vec::new(),
                dispatch: function_dispatch(value),
                kind: LocalFunctionKind::Plain,
            })
        } else {
            None
        };
        let class = type_name?.clone();
        let function = function?;
        let id = SourceObjectId(self.registry.objects.len());
        self.registry.objects.push(SourceObject { class, function });
        Some(id)
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
        let (mut installation, method_type_spans) =
            self.classify_installation(id, assignment, source, knowledge)?;

        // Preserve M2's distinct assignment-method form: only `:=` contributes
        // a callable signature here. `=` installations are retained for
        // diagnostics/document symbols but are not ordinary call methods.
        if assignment.binary_operator() == Some(":=") {
            self.attach_method_installation(&mut installation, knowledge);
        }

        for range in method_type_spans {
            self.registry
                .pending_source_semantic_roles
                .insert(SourceRangeKey(range), SourceSemanticRole::MethodType);
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
        match &installation.method.head {
            MethodHead::Function(name) => {
                if !self.head_is_method_function(name.name(), installation.span.start, knowledge) {
                    return;
                }
                let Some(object_id) = self
                    .get_binding_at(name.name(), installation.span.start)
                    .and_then(|binding| binding.state.object_id)
                else {
                    return;
                };
                let Some(function) = self
                    .registry
                    .objects
                    .get_mut(object_id.0)
                    .map(|object| &mut object.function)
                else {
                    return;
                };
                if installation.method.codomain.is_none() {
                    installation
                        .method
                        .codomain
                        .clone_from(&function.typical_value);
                }
                function.installations.push(installation.id);
            }
            MethodHead::Operator(operator) => {
                let function = self
                    .registry
                    .operator_functions
                    .entry(operator.clone())
                    .or_insert_with(|| FunctionInfo {
                        typical_value: None,
                        installations: Vec::new(),
                        dispatch: None,
                        kind: LocalFunctionKind::Plain,
                    });
                function.installations.push(installation.id);
            }
        }
    }

    fn push_scope(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        parent_idx: Option<usize>,
        assignments_may_escape: bool,
    ) -> usize {
        let range = source.range_for_node(node);
        let scope_idx = self.registry.scopes.len();
        self.registry.scopes.push(ScopeInfo {
            range,
            parent_idx,
            assignments_may_escape,
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

fn control_flow_scope(parent: M2Node<'_>, child: M2Node<'_>) -> Option<ControlFlowScope> {
    match parent.kind {
        NodeKind::IfStatement => {
            let is_condition = parent
                .child_by_field_name("condition")
                .is_some_and(|condition| condition.id() == child.id());
            (is_condition || matches!(child.kind, NodeKind::ThenClause | NodeKind::ElseClause))
                .then_some(ControlFlowScope::Branch)
        }
        NodeKind::TryStatement => {
            let is_body = parent
                .named_child(0)
                .is_some_and(|body| body.id() == child.id());
            (is_body || is_try_clause(child.kind)).then_some(ControlFlowScope::Branch)
        }
        NodeKind::ForStatement => {
            is_loop_clause(child.kind).then_some(ControlFlowScope::LoopClause)
        }
        NodeKind::WhileStatement => {
            let is_condition = parent
                .named_child(0)
                .is_some_and(|condition| condition.id() == child.id());
            (is_condition || is_loop_clause(child.kind)).then_some(ControlFlowScope::LoopClause)
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

fn method_installation_domain_nodes(node: M2Node) -> Option<Vec<M2Node>> {
    let node = parenthesized_value(node)?;
    if matches!(node.kind, NodeKind::Sequence | NodeKind::List) {
        let domain = node.collection_elements().collect::<Vec<_>>();
        return (!domain.is_empty()).then_some(domain);
    }

    symbol_node_text(node).map(|_| vec![node])
}

pub fn method_installation_domain(node: M2Node) -> Option<Vec<ObjectName>> {
    method_installation_domain_nodes(node).map(|nodes| {
        nodes
            .into_iter()
            .map(|node| ObjectName::new(symbol_node_text(node).unwrap_or_else(|| node.text())))
            .collect()
    })
}

fn call_like_left_symbol_for_argument<'tree>(
    mut node: M2Node<'tree>,
    allow_list_argument: bool,
) -> Option<&'tree str> {
    let mut parent = node.parent()?;
    if parent.kind == NodeKind::Sequence && !parent.is_first_collection_element(node) {
        return None;
    }

    loop {
        if parent.is_space_application() {
            let left = parent.child_by_field_name("left")?;
            if left.kind == NodeKind::Symbol {
                return Some(left.text());
            }
        }

        if parent.kind == NodeKind::List && !allow_list_argument {
            return None;
        }
        if !matches!(
            parent.kind,
            NodeKind::Sequence | NodeKind::List | NodeKind::ParenthesizedExpression
        ) {
            return None;
        }

        node = parent;
        parent = node.parent()?;
    }
}

fn indexed_string_names_package(
    node: M2Node<'_>,
    knowledge: &(impl TypeKnowledge + ?Sized),
) -> bool {
    let Some(callable_name) = call_like_left_symbol_for_argument(node, false) else {
        return false;
    };
    let Some(callable) = knowledge
        .get_record(&ObjectName::new(callable_name))
        .and_then(|record| record.callable())
    else {
        return false;
    };
    let (Some(string), Some(package)) = (
        knowledge.type_role_id(TypeRole::String),
        knowledge.type_role_id(TypeRole::Package),
    ) else {
        return false;
    };
    let accepts_string = callable
        .methods
        .iter()
        .any(|method| method.domain.first() == Some(string.object()));
    let accepts_package = callable
        .methods
        .iter()
        .any(|method| method.domain.first() == Some(package.object()));
    let returns_package = callable.methods.iter().any(|method| {
        method.domain.first() == Some(string.object()) && method.codomain.as_ref() == Some(&package)
    });
    accepts_string && (accepts_package || returns_package)
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
