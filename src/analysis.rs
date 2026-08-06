//! Parse-tree analysis that records lexical bindings, static type facts, and
//! diagnostics for one document snapshot.

mod diagnostics;
mod lexical;
mod ring;
mod typechecker;

pub use diagnostics::{
    ambiguous_float_member_access_rewrite, coalescence_rewrite, redundant_control_parentheses_inner,
};

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::ops::Deref;
use tower_lsp::lsp_types::{Position, Range as TextRange, SymbolKind};

use crate::builtin_index::CallableKind;
use crate::diagnostic_registry::{DiagnosticKind, M2Diagnostic};
use crate::meta::{BindingRole, Meta, Metadata};
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::object_registry::ObjectName;
use crate::object_registry::{ObjectId, OperatorForm, TypeData, TypeId};
use crate::semantic_token::{syntax_semantic_token_type, SourceSemanticRole, SourceSemanticToken};
use crate::source::SourceNavigation;
use crate::typesystem::{
    InferredType, LiteralOption, PositionedTypeKnowledge, SubtypeEvidence, TypeKnowledge, TypeRole,
};
use lexical::{control_flow_scope, is_try_clause, nested_symbols, ScopeTree};
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

/// Identity of one statically known callable runtime object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallableObjectId(usize);

/// Source-characterized method signature.
///
/// Domain and codomain names remain nominal here because local bindings and
/// package inclusions make their resolution source-position dependent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub head: MethodHead,
    pub domain: Vec<ObjectName>,
    pub codomain: Option<ObjectName>,
    pub parameter_names: Option<Vec<ObjectName>>,
}

impl Method {
    fn new(
        head: MethodHead,
        domain: Vec<ObjectName>,
        codomain: Option<ObjectName>,
        parameter_names: Option<Vec<ObjectName>>,
    ) -> Self {
        Self {
            head,
            domain,
            codomain,
            parameter_names,
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
    let parameters = (lambda.kind == NodeKind::LambdaExpression)
        .then(|| lambda.child_by_field_name("parameters"))
        .flatten()?;
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

/// A lambda-body codomain that can safely drive an annotation quick fix.
pub struct MethodCodomainDeduction {
    pub codomain: ObjectName,
    pub annotated_codomain: Option<ObjectName>,
    pub diagnostic_range: TextRange,
    pub edit: MethodCodomainEdit,
}

/// The source edit needed to make a method codomain annotation precise.
pub enum MethodCodomainEdit {
    Add(TextRange),
    Replace,
}

/// The outline-relevant semantic shape of one characterized assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentFactKind {
    MethodInstallation(MethodInstallationId),
    IndexedVariable,
    ScopedCallable,
}

/// Source-facing assignment facts retained by analysis for capability projections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentFact {
    pub label: String,
    pub span: TextRange,
    pub target_span: TextRange,
    pub value_span: Option<TextRange>,
    pub scope_idx: usize,
    pub kind: AssignmentFactKind,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallableHeadKind {
    PlainFunction,
    MethodFunction,
    Unknown,
}

/// Semantic information about one statically tracked callable.
///
/// Installed methods are referenced by identity in the snapshot's semantic
/// registry
/// so their source facts have one owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub typical_value: Option<ObjectName>,
    pub installations: Vec<MethodInstallationId>,
    pub dispatch: Option<Dispatch>,
    pub parameter_names: Option<Vec<ObjectName>>,
    kind: LocalFunctionKind,
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
    pub potential_export: bool,
    pub range: TextRange,
    pub scope_idx: usize,
    pub states: Vec<BindingStateInfo>,
}

/// One source-ordered value and inferred-type state of a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingStateInfo {
    pub presentation_kind: SymbolKind,
    pub type_name: Option<ObjectName>,
    pub object_id: Option<CallableObjectId>,
    pub indexed_element_type: Option<ObjectName>,
    pub source_type: Option<TypeId>,
    pub value_range: Option<TextRange>,
    pub definition_range: TextRange,
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

/// Typed source-declared type edges and their source-symbol identities.
#[derive(Debug, Default)]
struct SourceTypeFacts {
    data: HashMap<TypeId, TypeData>,
}

/// Canonical per-snapshot store of symbols, bindings, scopes, and their indexes.
#[derive(Debug, Default)]
struct SemanticRegistry {
    scopes: ScopeTree,
    bindings: Vec<BindingInfo>,
    bindings_by_name: HashMap<ObjectName, Vec<BindingId>>,
    callable_objects: Vec<FunctionInfo>,
    indexed_callable_objects: HashMap<ObjectId, CallableObjectId>,
    operator_functions: HashMap<Operator, FunctionInfo>,
    installations: Vec<MethodInstallation>,
    assignment_facts: Vec<AssignmentFact>,
    pending_source_semantic_roles: Vec<(TextRange, SourceSemanticRole)>,
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
pub enum OutputReference {
    Relative(NonZeroUsize),
    Absolute(NonZeroUsize),
    MissingAbsolute,
}

impl OutputReference {
    pub fn parse(name: &str) -> Option<Self> {
        let bytes = name.as_bytes();
        if (2..=4).contains(&bytes.len()) && bytes.iter().all(|byte| *byte == b'o') {
            return NonZeroUsize::new(bytes.len() - 1).map(Self::Relative);
        }
        let number = name.strip_prefix('o')?;
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(
            number
                .parse()
                .ok()
                .and_then(NonZeroUsize::new)
                .map_or(Self::MissingAbsolute, Self::Absolute),
        )
    }

    pub fn referenced_value<'tree>(self, node: M2Node<'tree>) -> Option<M2Node<'tree>> {
        let mut cell = node;
        while cell.kind != NodeKind::Cell {
            cell = cell.parent()?;
        }
        let root = cell
            .parent()
            .filter(|parent| parent.kind == NodeKind::SourceFile)?;
        let preceding_cells = root
            .named_children()
            .filter(|candidate| {
                candidate.kind == NodeKind::Cell && candidate.end_byte() <= cell.start_byte()
            })
            .collect::<Vec<_>>();

        match self {
            Self::Relative(distance) => preceding_cells
                .iter()
                .rev()
                .filter_map(M2Node::final_value_child)
                .nth(distance.get() - 1),
            Self::Absolute(number) => preceding_cells
                .get(number.get() - 1)
                .and_then(M2Node::final_value_child),
            Self::MissingAbsolute => None,
        }
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
    object_id: Option<CallableObjectId>,
    indexed_element_type: Option<ObjectName>,
    parent_type: Option<TypeId>,
    scope_idx: usize,
    potential_export: bool,
}

/// The parsed loop or function body receiving a control transfer.
#[derive(Clone, Copy)]
pub enum ControlTransferTarget<'tree> {
    Function(M2Node<'tree>),
    ListLoop(M2Node<'tree>),
    DoLoop(M2Node<'tree>),
    LoopCallback {
        function: M2Node<'tree>,
        callable: M2Node<'tree>,
    },
}

impl<'tree> ControlTransferTarget<'tree> {
    pub fn owner(self) -> M2Node<'tree> {
        match self {
            Self::Function(owner) | Self::ListLoop(owner) | Self::DoLoop(owner) => owner,
            Self::LoopCallback { function, .. } => function,
        }
    }

    pub fn accepts(self, transfer: M2Node<'_>) -> bool {
        match (transfer.kind, self) {
            (NodeKind::ReturnStatement, Self::Function(_))
            | (
                NodeKind::BreakStatement,
                Self::ListLoop(_) | Self::DoLoop(_) | Self::LoopCallback { .. },
            )
            | (NodeKind::ContinueStatement, Self::ListLoop(_)) => true,
            (NodeKind::ContinueStatement, Self::DoLoop(_)) => transfer.named_child(0).is_none(),
            _ => false,
        }
    }
}

/// Complete semantic analysis of one immutable document snapshot.
/// It owns source facts, characterized method installations, and diagnostics;
/// expression types are not retained after inference.
#[derive(Debug)]
pub struct Analysis {
    pub diagnostics: Vec<M2Diagnostic>,
    registry: SemanticRegistry,
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

    pub fn visible_source_binding_at(
        &self,
        name: &str,
        pos: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<BindingView<'_>> {
        let scope_idx = self.find_scope_at(pos)?;
        self.visible_source_binding_from_scope(name, scope_idx, pos, knowledge)
    }

    fn visible_source_binding_from_scope(
        &self,
        name: &str,
        scope_idx: usize,
        pos: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<BindingView<'_>> {
        self.get_binding_from_scope(name, scope_idx, pos)
            .filter(|binding| Self::source_binding_is_visible(*binding, knowledge))
    }

    pub fn source_binding_is_visible(
        binding: BindingView<'_>,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        binding.scope_idx != 0 || !knowledge.shadows_source(&binding.name, binding.state.span.start)
    }

    pub fn control_transfer_target<'tree>(
        &self,
        transfer: M2Node<'tree>,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<ControlTransferTarget<'tree>> {
        if !transfer.kind.is_control_transfer() {
            return None;
        }

        let mut direct_child = transfer;
        while let Some(parent) = direct_child.parent() {
            match transfer.kind {
                NodeKind::ReturnStatement if parent.kind == NodeKind::LambdaExpression => {
                    return parent
                        .child_by_field_name("body")
                        .is_some_and(|body| body.contains(transfer))
                        .then_some(ControlTransferTarget::Function(parent));
                }
                NodeKind::BreakStatement | NodeKind::ContinueStatement => {
                    if parent.kind == NodeKind::LambdaExpression {
                        let callable = direct_callback_callable(parent)?;
                        let name = callable.text();
                        let position = source.position_for_node(callable);
                        if self
                            .visible_source_binding_at(name, position, knowledge)
                            .is_some()
                            || knowledge
                                .get_record(&ObjectName::new(name))
                                .and_then(|record| record.callable())
                                .is_none()
                        {
                            return None;
                        }
                        return match name {
                            "apply" | "scan" => Some(ControlTransferTarget::LoopCallback {
                                function: parent,
                                callable,
                            }),
                            _ => None,
                        };
                    }
                    if matches!(
                        parent.kind,
                        NodeKind::ForStatement | NodeKind::WhileStatement
                    ) {
                        return match direct_child.kind {
                            NodeKind::ListClause => Some(ControlTransferTarget::ListLoop(parent)),
                            NodeKind::DoClause => Some(ControlTransferTarget::DoLoop(parent)),
                            _ => None,
                        };
                    }
                }
                _ => {}
            }
            direct_child = parent;
        }
        None
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
            curr = self.registry.scopes.parent(idx);
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
            curr = self.registry.scopes.parent(idx);
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
        self.registry
            .callable_objects
            .get(binding.state.object_id?.0)
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

    pub fn method_installation(&self, id: MethodInstallationId) -> Option<&MethodInstallation> {
        self.registry.installations.get(id.0)
    }

    pub fn assignment_facts(&self) -> &[AssignmentFact] {
        &self.registry.assignment_facts
    }

    pub fn scope_with_range(&self, range: TextRange) -> Option<usize> {
        self.registry.scopes.with_range(range)
    }

    pub fn scope_count(&self) -> usize {
        self.registry.scopes.len()
    }

    pub fn parent_scope(&self, scope_idx: usize) -> Option<usize> {
        self.registry.scopes.parent(scope_idx)
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

    pub fn binding_states(&self) -> impl Iterator<Item = BindingView<'_>> {
        self.registry.bindings.iter().flat_map(|binding| {
            binding
                .states
                .iter()
                .map(|state| BindingView { binding, state })
        })
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
            current = self.registry.scopes.parent(idx);
        }
        out
    }

    fn find_scope_at(&self, pos: Position) -> Option<usize> {
        self.registry.scopes.at(pos)
    }

    pub fn new_with_knowledge(
        root: M2Node<'_>,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl PositionedTypeKnowledge + ?Sized),
    ) -> Self {
        let mut analysis = Analysis {
            diagnostics: Vec::new(),
            registry: SemanticRegistry {
                scopes: ScopeTree::collect(root, source),
                ..Default::default()
            },
        };
        analysis.analyze_semantics(root, source, 0, 0, knowledge);
        analysis.registry.pending_source_semantic_roles.clear();
        analysis.collect_diagnostics(root, source, knowledge);
        analysis
    }

    fn analyze_semantics(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        current_scope_idx: usize,
        assignment_scope_idx: usize,
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        let knowledge = knowledge_provider.at_position(source.position_for_node(node));
        self.record_source_semantic_roles(node, source, &knowledge);
        self.record_source_semantic_token(node, source, &knowledge);
        let mut next_scope_idx = current_scope_idx;
        let mut next_assignment_scope_idx = assignment_scope_idx;

        match node.kind {
            NodeKind::LambdaExpression => {
                next_scope_idx = self.registry.scopes.owned_by(node, source);
                next_assignment_scope_idx = next_scope_idx;

                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let parameter_types = method_installation_parameter_types_for_function(node);
                    self.register_parameters(
                        params_node,
                        source,
                        next_scope_idx,
                        parameter_types.as_deref(),
                    );
                }
            }
            NodeKind::ForStatement => {
                next_scope_idx = self.registry.scopes.owned_by(node, source);
                if let Some(variable) = node.child_by_field_name("variable") {
                    self.register_parameters(variable, source, next_scope_idx, None);
                }
            }
            _ if node.is_assignment() => {
                let left = node.child_by_field_name("left");
                let op = node.child_by_field_name("operator");
                let right = node.child_by_field_name("right");

                if let (Some(left), Some(op)) = (left, op) {
                    let op_text = op.text();
                    let installation_id = self.record_method_installation(node, source, &knowledge);
                    self.record_assignment_fact(
                        node,
                        left,
                        right,
                        current_scope_idx,
                        installation_id,
                        source,
                    );
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
                        self.declared_type_parent(
                            right,
                            type_name.as_ref(),
                            source.position_for_node(right),
                            &knowledge,
                        )
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
                            source.position_for_node(left),
                            &knowledge,
                        )
                    }) {
                        SymbolKind::CLASS
                    } else {
                        SymbolKind::VARIABLE
                    };
                    let target_name = single_symbol_assignment_target(left);
                    let object_id = right.zip(target_name).and_then(|(right, _)| {
                        self.callable_object_for_value(right, source, current_scope_idx, &knowledge)
                    });

                    match op_text {
                        ":=" => self.register_definitions(
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
                        "=" => self.register_definitions(
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
                                    || self
                                        .registry
                                        .scopes
                                        .assignments_may_escape(assignment_scope_idx),
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

        // Recurse into children
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
            self.analyze_semantics(
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

        if node.kind.is_string_literal() && indexed_string_names_package(node, knowledge) {
            self.register_source_semantic_role(
                source.range_for_node(node),
                SourceSemanticRole::NamespaceArgument,
            );
        }
    }

    fn register_source_semantic_role(&mut self, range: TextRange, role: SourceSemanticRole) {
        if self
            .registry
            .pending_source_semantic_roles
            .iter()
            .all(|(registered, _)| *registered != range)
        {
            self.registry
                .pending_source_semantic_roles
                .push((range, role));
        }
    }

    fn set_source_semantic_role(&mut self, range: TextRange, role: SourceSemanticRole) {
        if let Some((_, registered)) = self
            .registry
            .pending_source_semantic_roles
            .iter_mut()
            .find(|(registered, _)| *registered == range)
        {
            *registered = role;
        } else {
            self.registry
                .pending_source_semantic_roles
                .push((range, role));
        }
    }

    fn record_source_semantic_token(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        if node.kind == NodeKind::Symbol && OutputReference::parse(node.text()).is_some() {
            let position = source.position_for_node(node);
            if self
                .visible_source_binding_at(node.text(), position, knowledge)
                .is_none()
            {
                return;
            }
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
            .iter()
            .find(|(range, _)| *range == span.range())
            .map(|(_, role)| role.clone());
        self.registry
            .source_semantic_tokens
            .push(SourceSemanticToken {
                span,
                syntax_token_type,
                source_role,
                is_symbol,
                is_unquoted_symbol: node.kind == NodeKind::Symbol,
                is_expression_symbol: is_expression_symbol(node),
                is_condition_value: is_condition_value(node),
            });
    }

    fn record_assignment_fact(
        &mut self,
        assignment: M2Node,
        target: M2Node,
        value: Option<M2Node>,
        scope_idx: usize,
        installation_id: Option<MethodInstallationId>,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let kind = if let Some(id) = installation_id {
            AssignmentFactKind::MethodInstallation(id)
        } else {
            if assignment.binary_operator() != Some("=") {
                return;
            }
            match target.binary_operator() {
                Some("_") => AssignmentFactKind::IndexedVariable,
                Some(_) => AssignmentFactKind::ScopedCallable,
                None => return,
            }
        };
        self.registry.assignment_facts.push(AssignmentFact {
            label: target.text().to_string(),
            span: source.range_for_node(assignment),
            target_span: source.range_for_node(target),
            value_span: value.map(|value| source.range_for_node(value)),
            scope_idx,
            kind,
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
    ) -> Option<(MethodInstallation, Vec<(TextRange, SourceSemanticRole)>)> {
        let operator = node.binary_operator()?;
        let left = node.child_by_field_name("left")?;
        let position = source.position_for_node(left);
        let (head, domain_nodes) = self.installation_shape(left, position, knowledge)?;
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
        let rhs_lambda = right.and_then(assigned_lambda);
        let codomain_node = right.and_then(method_codomain_annotation);
        let codomain = codomain_node
            .and_then(symbol_node_text)
            .map(ObjectName::new);
        let codomain_span = codomain_node.map(|node| source.range_for_node(node));
        let method_semantic_spans = domain_spans
            .iter()
            .copied()
            .map(|range| (range, SourceSemanticRole::MethodType))
            .chain(codomain_span.map(|range| (range, SourceSemanticRole::MethodAnnotation)))
            .collect::<Vec<_>>();
        // The RHS function shape, read once here so the arity diagnostic need not
        // re-walk the tree. Only a plain lambda RHS carries a checkable arity.
        let rhs_lambda_dispatch = rhs_lambda.and_then(function_dispatch);
        let parameter_names = rhs_lambda.and_then(fixed_parameter_names);

        match operator {
            // `:=` installs by shape alone — no type check on the operands.
            ":=" => Some((
                MethodInstallation {
                    id,
                    method: Method::new(head, domain, codomain, parameter_names),
                    span,
                    expected_rhs_arity: operand_arity,
                    rhs_lambda_dispatch,
                },
                method_semantic_spans.clone(),
            )),
            // `=` installs only the assignment form of a BINARY operator (incl.
            // SPACE), and only when every operand is a type; otherwise the same
            // syntax assigns to the lvalue `X op Y`, which is a call.
            "=" => match head {
                MethodHead::Operator(op)
                    if op.form == OperatorForm::Binary
                        && domain.iter().all(|operand| {
                            self.operand_is_type(operand.name(), position, knowledge)
                        }) =>
                {
                    Some((
                        MethodInstallation {
                            id,
                            method: Method::new(
                                MethodHead::Operator(op),
                                domain,
                                codomain,
                                parameter_names,
                            ),
                            span,
                            expected_rhs_arity: operand_arity + 1,
                            rhs_lambda_dispatch,
                        },
                        method_semantic_spans,
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
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<(MethodHead, Vec<M2Node<'tree>>)> {
        // A parenthesized expression is identified with its final value, so
        // `(T op S) := f` installs exactly like `T op S := f`. A final `muted`
        // child means the group evaluates to null and is not an installation
        // target.
        if node.kind == NodeKind::ParenthesizedExpression {
            let inner = node.final_value_child()?;
            return self.installation_shape(inner, position, knowledge);
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
                    if self.operand_is_type(left_name, position, knowledge) {
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
    fn operand_is_type(
        &self,
        name: &str,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        self.get_binding_at(name, position)
            .is_some_and(|binding| binding.state.source_type.is_some())
            || knowledge
                .get_record(&ObjectName::new(name))
                .is_some_and(|record| record.type_info().is_some())
    }

    fn declared_type_parent(
        &self,
        value: M2Node<'_>,
        type_name: Option<&ObjectName>,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<TypeId> {
        if let Some(parent) = ring::RingSemantics::value_parent(type_name, knowledge) {
            return TypeChecker::new(self).resolve_type_id_at(&parent, position, knowledge);
        }
        let type_name = type_name.filter(|type_name| {
            value.kind == NodeKind::NewStatement
                && TypeChecker::new(self).is_subtype(
                    type_name,
                    &TypeRole::Type.object_name(),
                    position,
                    knowledge,
                )
        })?;
        let parent = clause_of(value, NodeKind::OfClause)
            .and_then(clause_value)
            .and_then(symbol_node_text)
            .map(ObjectName::new)
            .unwrap_or_else(|| type_name.clone());
        TypeChecker::new(self).resolve_type_id_at(&parent, position, knowledge)
    }

    fn callable_head_kind(
        &self,
        name: &str,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> CallableHeadKind {
        if let Some(binding) = self.get_binding_at(name, position) {
            if let Some(function) = self.function_for_binding(binding) {
                return match function.kind {
                    LocalFunctionKind::Plain => CallableHeadKind::PlainFunction,
                    LocalFunctionKind::Method => CallableHeadKind::MethodFunction,
                };
            }
            return binding
                .state
                .type_name
                .as_ref()
                .map(|type_name| {
                    if knowledge.has_type_role(type_name, TypeRole::MethodFunction) {
                        CallableHeadKind::MethodFunction
                    } else if knowledge.has_type_role(type_name, TypeRole::Function) {
                        CallableHeadKind::PlainFunction
                    } else {
                        CallableHeadKind::Unknown
                    }
                })
                .unwrap_or(CallableHeadKind::Unknown);
        }
        knowledge
            .get_record(&ObjectName::new(name))
            .and_then(|record| record.callable())
            .map_or(CallableHeadKind::Unknown, |callable| {
                if callable.is_method_function() {
                    CallableHeadKind::MethodFunction
                } else {
                    CallableHeadKind::PlainFunction
                }
            })
    }

    fn register_parameters(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        parameter_types: Option<&[ObjectName]>,
    ) {
        let parameter_nodes = nested_symbols(node, |kind| {
            matches!(
                kind,
                NodeKind::Sequence | NodeKind::List | NodeKind::ParenthesizedExpression
            )
        });
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

    fn register_definitions(
        &mut self,
        node: M2Node,
        value_node: Option<M2Node>,
        source: &(impl SourceNavigation + ?Sized),
        definition_scope: DefinitionScope,
        registration: SymbolRegistration,
    ) {
        let structured_target = node.kind != NodeKind::Symbol;
        for definition in nested_symbols(node, NodeKindMetadata::is_collection_expression) {
            let registration = if structured_target {
                SymbolRegistration {
                    type_name: None,
                    object_id: None,
                    ..registration.clone()
                }
            } else {
                registration.clone()
            };
            self.register_definition(
                definition,
                value_node,
                source,
                definition_scope,
                registration,
            );
        }
    }

    fn register_definition(
        &mut self,
        node: M2Node,
        value_node: Option<M2Node>,
        source: &(impl SourceNavigation + ?Sized),
        definition_scope: DefinitionScope,
        registration: SymbolRegistration,
    ) {
        let name = node.text();
        match definition_scope {
            DefinitionScope::Local => self.add_symbol(name, node, value_node, registration, source),
            DefinitionScope::Global => {
                let position = source.position_for_node(node);
                let binding_id = self
                    .binding_id_from_scope(name, registration.scope_idx, position)
                    .filter(|binding_id| {
                        self.binding_anchor(*binding_id)
                            .is_some_and(|binding| binding.scope_idx == registration.scope_idx)
                    });
                if let Some(binding_id) = binding_id {
                    self.add_binding_state(binding_id, node, value_node, registration, source);
                } else {
                    self.add_symbol(name, node, value_node, registration, source);
                }
            }
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
        let binding_id = BindingId(self.registry.bindings.len() as u32);
        let source_type = parent_type.map(|parent_type| {
            let type_id = TypeId::from_source_type(
                ObjectId::new(format!("$Source${}:0", binding_id.0)),
                &parent_type,
            );
            self.registry.source_types.data.insert(
                type_id.clone(),
                TypeData {
                    parent: Some(parent_type),
                },
            );
            type_id
        });
        let range = source.range_for_node(node);
        let state = BindingStateInfo {
            presentation_kind,
            type_name,
            object_id,
            indexed_element_type,
            source_type,
            value_range: value_node.map(|value| source.range_for_node(value)),
            definition_range: enclosing_definition_range(node, source),
            span: range,
            scope_idx,
        };
        let binding = BindingInfo {
            binding_id,
            name: name.clone(),
            role,
            potential_export,
            range,
            scope_idx,
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
        let Some(state_index) = self.binding(binding_id).map(|binding| binding.states.len()) else {
            return;
        };
        let source_type = registration.parent_type.map(|parent_type| {
            let type_id = TypeId::from_source_type(
                ObjectId::new(format!("$Source${}:{state_index}", binding_id.0)),
                &parent_type,
            );
            self.registry.source_types.data.insert(
                type_id.clone(),
                TypeData {
                    parent: Some(parent_type),
                },
            );
            type_id
        });
        let state = BindingStateInfo {
            presentation_kind: registration.presentation_kind,
            type_name: registration.type_name,
            object_id: registration.object_id,
            indexed_element_type: registration.indexed_element_type,
            source_type,
            value_range: value_node.map(|value| source.range_for_node(value)),
            definition_range: enclosing_definition_range(node, source),
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

    pub fn call_parameter_names(
        &self,
        call: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<Vec<ObjectName>> {
        if !call.is_space_application() {
            return None;
        }
        let callable = call.child_by_field_name("left")?;
        let arguments = call.child_by_field_name("right")?;
        if callable.kind != NodeKind::Symbol {
            return None;
        }
        let position = source.position_for_node(callable);
        let binding = self.visible_source_binding_at(callable.text(), position, knowledge)?;
        let function = self.function_for_binding(binding)?;
        let facts = self.infer_call_static_facts(arguments, source, knowledge);
        TypeChecker::new(self).local_call_parameter_names(
            function,
            &facts.argument_types,
            position,
            knowledge,
        )
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

    pub fn method_codomain_deduction(
        &self,
        assignment: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<MethodCodomainDeduction> {
        self.installation_for(assignment, source)?;
        let left = assignment.child_by_field_name("left")?;
        let right = assignment.child_by_field_name("right")?;
        let lambda = assigned_lambda(right)?;
        let body = lambda.child_by_field_name("body")?;
        let codomain = self.infer_expression_static_type(body, source, knowledge)?;
        if codomain == TypeRole::Thing.object_name() {
            return None;
        }

        let annotation = method_codomain_annotation(right);
        let annotated_codomain = annotation.map(|annotation| ObjectName::new(annotation.text()));
        if let Some(annotation) = annotation {
            match TypeChecker::new(self).subtype_evidence(
                &codomain,
                annotated_codomain.as_ref()?,
                source.position_for_node(annotation),
                knowledge,
            ) {
                SubtypeEvidence::Proven | SubtypeEvidence::Unknown => return None,
                SubtypeEvidence::Disproven => {}
            }
        }

        let edit = annotation.map_or_else(
            || {
                MethodCodomainEdit::Add({
                    let start = source.position_for_node(lambda);
                    TextRange::new(start, start)
                })
            },
            |_| MethodCodomainEdit::Replace,
        );
        Some(MethodCodomainDeduction {
            codomain,
            annotated_codomain,
            diagnostic_range: annotation.map_or_else(
                || source.range_for_node(left),
                |node| source.range_for_node(node),
            ),
            edit,
        })
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
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Vec<Option<ObjectId>> {
        TypeChecker::new(self).dispatch_argument_ids(facts, position, knowledge)
    }

    fn callable_object_for_value(
        &mut self,
        value: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<CallableObjectId> {
        if value.kind == NodeKind::ParenthesizedExpression {
            return self.callable_object_for_value(
                value.final_value_child()?,
                source,
                scope_idx,
                knowledge,
            );
        }
        if value.kind == NodeKind::Symbol {
            if let Some(binding) = self.get_binding_from_scope(
                value.text(),
                scope_idx,
                source.position_for_node(value),
            ) {
                if binding.scope_idx != 0
                    || !knowledge.shadows_source(&binding.name, binding.state.span.start)
                {
                    return binding.state.object_id;
                }
            }

            let indexed_id = knowledge.resolve_object(&ObjectName::new(value.text()))?;
            if let Some(object_id) = self
                .registry
                .indexed_callable_objects
                .get(&indexed_id)
                .copied()
            {
                return Some(object_id);
            }
            let callable = knowledge.object(&indexed_id)?.callable()?;
            let function = FunctionInfo {
                typical_value: callable
                    .typical_value
                    .as_ref()
                    .and_then(|type_id| knowledge.type_name(type_id))
                    .cloned(),
                installations: Vec::new(),
                dispatch: None,
                parameter_names: None,
                kind: match callable.kind {
                    CallableKind::MethodFunction => LocalFunctionKind::Method,
                    CallableKind::Function | CallableKind::Operator(_) => LocalFunctionKind::Plain,
                },
            };
            let object_id = CallableObjectId(self.registry.callable_objects.len());
            self.registry.callable_objects.push(function);
            self.registry
                .indexed_callable_objects
                .insert(indexed_id, object_id);
            return Some(object_id);
        }

        let function = if let Some(typical_value) = method_declaration_typical_value(value) {
            Some(FunctionInfo {
                typical_value,
                installations: Vec::new(),
                dispatch: None,
                parameter_names: None,
                kind: LocalFunctionKind::Method,
            })
        } else if value.kind == NodeKind::LambdaExpression {
            Some(FunctionInfo {
                typical_value: None,
                installations: Vec::new(),
                dispatch: function_dispatch(value),
                parameter_names: fixed_parameter_names(value),
                kind: LocalFunctionKind::Plain,
            })
        } else {
            None
        };
        let function = function?;
        let id = CallableObjectId(self.registry.callable_objects.len());
        self.registry.callable_objects.push(function);
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

        for (range, role) in method_type_spans {
            self.set_source_semantic_role(range, role);
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
                if self.callable_head_kind(name.name(), installation.span.start, knowledge)
                    != CallableHeadKind::MethodFunction
                {
                    return;
                }
                let Some(object_id) = self
                    .get_binding_at(name.name(), installation.span.start)
                    .and_then(|binding| binding.state.object_id)
                else {
                    return;
                };
                let Some(function) = self.registry.callable_objects.get_mut(object_id.0) else {
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
                        parameter_names: None,
                        kind: LocalFunctionKind::Plain,
                    });
                function.installations.push(installation.id);
            }
        }
    }

    pub fn binding_id_at(&self, name: &str, pos: Position) -> Option<BindingId> {
        let scope_idx = self.find_scope_at(pos)?;
        self.binding_id_from_scope(name, scope_idx, pos)
    }
}

fn fixed_parameter_names(lambda: M2Node<'_>) -> Option<Vec<ObjectName>> {
    if !matches!(function_dispatch(lambda), Some(Dispatch::Fixed(_))) {
        return None;
    }
    let parameters = nested_symbols(lambda.child_by_field_name("parameters")?, |kind| {
        matches!(
            kind,
            NodeKind::Sequence | NodeKind::List | NodeKind::ParenthesizedExpression
        )
    });
    Some(
        parameters
            .into_iter()
            .map(|parameter| ObjectName::new(parameter.text()))
            .collect(),
    )
}

fn assigned_lambda(node: M2Node<'_>) -> Option<M2Node<'_>> {
    let node = parenthesized_value(node)?;
    if node.kind == NodeKind::LambdaExpression {
        return Some(node);
    }
    (node.binary_operator() == Some("=>"))
        .then(|| node.child_by_field_name("right"))
        .flatten()
        .and_then(assigned_lambda)
}

fn method_codomain_annotation(node: M2Node<'_>) -> Option<M2Node<'_>> {
    node.is_option_assignment()
        .then(|| node.child_by_field_name("left"))
        .flatten()
}

fn single_symbol_assignment_target<'tree>(node: M2Node<'tree>) -> Option<&'tree str> {
    (node.kind == NodeKind::Symbol).then(|| node.text())
}

pub fn symbol_node_text<'tree>(node: M2Node<'tree>) -> Option<&'tree str> {
    node.kind.is_symbol_like().then(|| node.text())
}

fn is_expression_symbol(node: M2Node<'_>) -> bool {
    if node.kind != NodeKind::Symbol || matches!(node.text(), "true" | "false") {
        return false;
    }
    if node
        .parent()
        .is_some_and(|parent| parent.kind == NodeKind::QuoteExpression)
    {
        return false;
    }

    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.is_assignment() {
            return parent
                .child_by_field_name("left")
                .is_none_or(|left| !left.contains(node));
        }
        current = parent;
    }

    true
}

fn is_condition_value(node: M2Node<'_>) -> bool {
    if node.kind != NodeKind::Symbol {
        return false;
    }

    if node.parent().is_some_and(|parent| {
        parent.is_space_application()
            && parent
                .child_by_field_name("left")
                .is_some_and(|left| left.id() == node.id())
    }) {
        return false;
    }

    let mut condition = node;
    while let Some(parent) = condition.parent() {
        let owner_condition = match parent.kind {
            NodeKind::IfStatement => parent.child_by_field_name("condition"),
            NodeKind::WhileStatement => parent.named_child(0),
            _ => None,
        };
        if let Some(owner_condition) = owner_condition {
            return owner_condition.id() == condition.id();
        }
        condition = parent;
    }

    false
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

fn direct_callback_callable(lambda: M2Node<'_>) -> Option<M2Node<'_>> {
    let mut argument = lambda;
    let mut parent = argument.parent()?;
    while matches!(
        parent.kind,
        NodeKind::Sequence | NodeKind::ParenthesizedExpression
    ) {
        if parent.kind == NodeKind::Sequence
            && parent
                .collection_elements()
                .last()
                .is_none_or(|last| last.id() != argument.id())
        {
            return None;
        }
        argument = parent;
        parent = argument.parent()?;
    }

    if !parent.is_space_application()
        || parent
            .child_by_field_name("right")
            .is_none_or(|right| right.id() != argument.id())
    {
        return None;
    }
    parent
        .child_by_field_name("left")
        .filter(|callable| callable.kind == NodeKind::Symbol)
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
