//! Parse-tree analysis that records lexical bindings, static type facts, and
//! diagnostics for one document snapshot.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Arc, RwLock};
use tower_lsp::lsp_types::{Diagnostic, Position, Range, SymbolKind};

use crate::builtin_index::InstanceID;
use crate::diagnostic_registry::M2Diagnostic;
use crate::meta::{BindingRole, Meta, Metadata};
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
#[cfg(test)]
use crate::source::DocumentSource;
use crate::source::SourceNavigation;
#[cfg(test)]
use crate::typesystem::NoTypeKnowledge;
use crate::typesystem::TypeKnowledge;
use crate::util::position_in_range;

/// Snapshot-local identity of an interned source symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(u32);

/// Strongly typed, shared storage for a symbol spelling interned by a
/// [`SemanticRegistry`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolName(Arc<str>);

impl SymbolName {
    pub fn new(name: &str) -> Self {
        Self(Arc::from(name))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SymbolName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Snapshot-local identity of one lexical binding declaration.
/// Reassignments keep this identity and create new [`BindingStateId`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingId(u32);

/// Snapshot-local identity of one source-ordered state of a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingStateId(u32);

/// Typed parser-node identity used only to cache inferred types within one
/// immutable syntax tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NodeFactId(usize);

/// Complete semantic analysis of one immutable document snapshot.
/// It owns the normalized registry, characterized method installations,
/// diagnostics, and the shared per-node type cache consumed by LSP features.
#[derive(Debug)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub registry: SemanticRegistry,
    pub installations: Vec<MethodInstallation>,
    cache_types: bool,
    type_cache: RwLock<HashMap<NodeFactId, InferredType>>,
}

/// Where an operator sits relative to its operand(s). Distinguishes the arity-1
/// forms (`prefix X` vs `X postfix`) that share a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fixity {
    Prefix,
    Binary,
    Postfix,
}

/// An M2 operator — including `SPACE`, the juxtaposition operator (`X Y` is
/// `X SPACE Y`). Just another operator, not a special "adjacency" concept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operator {
    pub token: String,
    pub fixity: Fixity,
}

/// Juxtaposition's operator token, e.g. the `SPACE` in `(SPACE, Ring, Array)`.
pub const SPACE_OPERATOR: &str = "SPACE";

/// The callable or operator receiving an installed method.
/// Installation syntax affects the installed function's arity, not the
/// identity of this head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodHead {
    Function(String),
    Operator(Operator),
}

/// Stable identity of a method installation within one immutable analysis
/// snapshot. Source positions belong to [`MethodInstallation`]; semantic
/// records refer to installations through this typed identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodInstallationId(u32);

/// A characterized method installation — the single source of truth for "this
/// assignment installs a method", produced once during analysis and consumed by
/// every capability instead of each re-deciding it from raw syntax.
/// `domain` is the tuple of dispatch types (e.g. `[ZZ, String]`). `span` is the
/// source span of the whole assignment within this analysis snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInstallation {
    pub id: MethodInstallationId,
    pub head: MethodHead,
    pub domain: Vec<InstanceID>,
    /// The effective codomain of this installation: an explicit declaration, or
    /// the method function's typical value at the installation site.
    pub codomain: Option<InstanceID>,
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

/// Strip a corpus `$Package$Name` qualifier down to the bare class name, so a
/// builtin class (`$Core$CompiledFunction`) and a locally-inferred class
/// (`FunctionClosure`) compare on the same footing.
fn bare_class_name(name: &str) -> &str {
    name.rsplit_once('$').map_or(name, |(_, bare)| bare)
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

/// The corpus attribute key for an operator form, matching `operator.attributes`
/// in the index (`binary`/`prefix`/`postfix`).
fn fixity_form(fixity: Fixity) -> &'static str {
    match fixity {
        Fixity::Binary => "binary",
        Fixity::Prefix => "prefix",
        Fixity::Postfix => "postfix",
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
/// element. The same source-level rule fundocs applies via M2 `parse` to record
/// builtin dispatch.
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

/// Semantic information about one locally defined callable.
///
/// Installed methods are referenced by identity in [`Analysis::installations`]
/// so their source facts have one owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub symbol: SymbolId,
    pub typical_value: Option<InstanceID>,
    pub methods: Vec<MethodInstallationId>,
    pub dispatch: Option<Dispatch>,
    kind: LocalFunctionKind,
}

/// Syntax-derived callable behavior used to decide whether method
/// installations take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalFunctionKind {
    Unknown,
    Plain,
    Method,
}

/// Static facts computed for one call after separating positional arguments
/// from literal option assignments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStaticFacts {
    pub argument_types: Vec<InferredType>,
    pub literal_options: Vec<(String, String)>,
}

/// Value-semantic source location used to key facts independently of borrowed
/// syntax-tree nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanKey {
    pub range: Range,
}

/// The inferred type of a value: an *upward-closed* subset of the type order
/// (types ordered by specialization, with `Thing` at the bottom — if a class is
/// possible, so is every subtype of it).
///
/// By co-Yoneda such a set is the union of the principal up-sets `↑t` ("`t` or a
/// subtype") of its **minimal generators**, so it is fully described by a finite
/// set of types, read as *"the value's class is a subtype of one of these"*. One
/// generator is the common case (a single `typicalValue`, itself a lower bound);
/// several arise where control flow joins branches of differing type. `{Thing}`
/// generates the whole order, i.e. "unknown".
///
/// Everything M2 records is a *bound*: `typicalValue` never asserts exactness, so
/// inference produces only bounds. Exactness is an out-of-band annotation (human,
/// or the indexer macro) deferred to the 2.0.0 type-data work; it is not modelled
/// here.
///
/// Planned extension (deferred): a function value carries only its class today
/// (`FunctionClosure`); folding its `domain → codomain` signature into the type
/// here is what lets composition, currying, and application preserve information
/// instead of collapsing to `FunctionClosure`/`Thing`. See Open Questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredType {
    minimal_generators: Vec<InstanceID>,
}

impl InferredType {
    /// The whole type order — no information. Generated by `Thing`.
    fn unknown() -> Self {
        Self::from_id(InstanceID::new("Thing"))
    }

    /// The principal up-set `↑t`: the value's class is `t` or a subtype.
    fn of(name: &str) -> Self {
        Self::from_id(InstanceID::new(name))
    }

    fn from_id(id: InstanceID) -> Self {
        Self {
            minimal_generators: vec![id],
        }
    }

    /// The single generator, when the set is principal — the boundary form the
    /// dispatch queries and inlay display consume in the basic (single-type)
    /// inference. `None` once a branch join has produced several generators.
    fn principal(&self) -> Option<&InstanceID> {
        match self.minimal_generators.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// The object identifier to feed nominal dispatch. `None` for a
    /// non-principal (joined) set, which basic dispatch cannot yet represent.
    fn dispatch_id(&self) -> Option<InstanceID> {
        self.principal().cloned()
    }

    /// The hover/inlay label for this type. Every value has a class — the floor
    /// `Thing` (≡ "unknown") and `Symbol` (an unbound name) are valid, displayable
    /// types — so a single generator always renders; a joined set renders as
    /// `A | B`. `None` only if the set is empty, which constructors never produce.
    pub fn label(&self) -> Option<String> {
        if self.minimal_generators.is_empty() {
            return None;
        }
        Some(
            self.minimal_generators
                .iter()
                .map(|generator| generator.0.as_str())
                .collect::<Vec<_>>()
                .join(" | "),
        )
    }

    /// Union of two value types (a branch join), kept minimal: a generator
    /// subsumed by a more-general one already present (`↑a ⊆ ↑b` when `a` is-a
    /// `b`) is dropped. Needs the lattice to compare subtypes; without it the
    /// union is only deduplicated.
    fn join(self, other: Self, knowledge: &(impl TypeKnowledge + ?Sized)) -> Self {
        let mut minimal_generators = self.minimal_generators;
        for generator in other.minimal_generators {
            if !minimal_generators.contains(&generator) {
                minimal_generators.push(generator);
            }
        }
        let candidates = minimal_generators.clone();
        minimal_generators.retain(|generator| {
            !candidates.iter().any(|other| {
                other != generator && knowledge.is_subtype(generator.as_ref(), other.as_ref())
            })
        });
        Self { minimal_generators }
    }
}

/// Normalized semantic category assigned to an expression fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionKind {
    Literal,
    Name,
    Expr,
    Assign,
    ScopeExpr,
    ControlExpr,
}

/// Source-independent identity and declaration properties of one lexical
/// binding.
///
/// Its value and inferred type at a particular point live in
/// [`BindingStateInfo`] records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingInfo {
    pub binding_id: BindingId,
    pub symbol: SymbolId,
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
    pub type_name: Option<InstanceID>,
    /// For an `IndexedVariableTable` binding, the local ring type produced by
    /// subscripting the table after a ring constructor or `use`-style rebind.
    pub indexed_element_type: Option<InstanceID>,
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
            type_name: self.state.type_name.as_ref().map(InstanceID::name),
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

/// Stored semantic result for one value-producing source expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionFact {
    pub kind: ExpressionKind,
    pub input_nodes: Vec<SpanKey>,
    pub operator: Option<String>,
    pub result_type: InferredType,
    pub scope_idx: usize,
}

/// Stored callable identity and positional argument types for a call
/// expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallInfo {
    pub callable_name: Option<String>,
    pub argument_types: Vec<InferredType>,
}

/// Canonical per-snapshot store of symbols, bindings, scopes, expressions, and
/// the indexes that relate them.
#[derive(Debug, Default)]
pub struct SemanticRegistry {
    pub symbol_names: Vec<SymbolName>,
    pub symbol_ids: HashMap<SymbolName, SymbolId>,
    pub scopes: Vec<ScopeInfo>,
    pub bindings: Vec<BindingInfo>,
    pub binding_states: Vec<BindingStateInfo>,
    pub bindings_by_symbol: HashMap<SymbolId, Vec<BindingId>>,
    pub states_by_binding: HashMap<BindingId, Vec<BindingStateId>>,
    pub node_scopes: HashMap<SpanKey, usize>,
    pub expressions: HashMap<SpanKey, ExpressionFact>,
    pub calls: HashMap<SpanKey, CallInfo>,
    pub functions: HashMap<SymbolId, FunctionInfo>,
    pub type_parents: HashMap<SymbolId, InstanceID>,
    ring_generators: HashMap<SymbolId, Vec<RingGenerator>>,
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

impl SemanticRegistry {
    fn intern_symbol(&mut self, name: &str) -> SymbolId {
        if let Some(symbol) = self.symbol_ids.get(name) {
            return *symbol;
        }
        let name = SymbolName::new(name);
        let symbol = SymbolId(self.symbol_names.len() as u32);
        self.symbol_names.push(name.clone());
        self.symbol_ids.insert(name, symbol);
        symbol
    }

    fn resolve_symbol(&self, name: &str) -> Option<SymbolId> {
        self.symbol_ids.get(name).copied()
    }

    fn symbol_name(&self, symbol: SymbolId) -> &str {
        self.symbol_names[symbol.0 as usize].as_str()
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
            let symbol = self.registry.resolve_symbol(name)?;
            let mut fallback = None;
            for binding_id in self.registry.bindings_by_symbol.get(&symbol)? {
                let binding = self.binding_definition(*binding_id)?;
                if binding.scope_idx == 0 {
                    return Some(binding);
                }
                fallback.get_or_insert(binding);
            }
            fallback
        })
    }

    #[cfg(test)]
    pub fn registry(&self) -> &SemanticRegistry {
        &self.registry
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
        let symbol = self.registry.resolve_symbol(name)?;
        let binding_id = self.binding_id_from_scope(symbol, scope_idx, pos)?;
        self.binding_state_from_scope(binding_id, scope_idx, pos)
    }

    fn binding_id_from_scope(
        &self,
        symbol: SymbolId,
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
                .bindings_by_symbol
                .get(&symbol)
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

    #[cfg(test)]
    pub fn expression_fact(
        &self,
        source: &(impl SourceNavigation + ?Sized),
        node: M2Node,
    ) -> Option<&ExpressionFact> {
        self.registry
            .expressions
            .get(&SpanKey::from_node(source, node))
    }

    pub fn function(&self, name: &str) -> Option<&FunctionInfo> {
        let symbol = self.registry.resolve_symbol(name)?;
        self.registry.functions.get(&symbol)
    }

    pub fn method_installation_codomain<'a>(
        &'a self,
        installation: &'a MethodInstallation,
    ) -> Option<&'a str> {
        installation.codomain.as_ref().map(InstanceID::name)
    }

    pub fn is_method_installation_codomain(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) -> bool {
        let span = SpanKey::from_node(source, node);
        self.installations
            .iter()
            .any(|installation| installation.codomain_span.as_ref() == Some(&span))
    }

    pub fn function_by_symbol(&self, symbol: SymbolId) -> Option<&FunctionInfo> {
        self.registry.functions.get(&symbol)
    }

    pub fn methods_for<'a>(
        &'a self,
        function: &'a FunctionInfo,
    ) -> impl Iterator<Item = &'a MethodInstallation> + 'a {
        function
            .methods
            .iter()
            .filter_map(|id| self.method_installation(*id))
    }

    fn method_installation(&self, id: MethodInstallationId) -> Option<&MethodInstallation> {
        self.installations.get(id.0 as usize)
    }

    pub fn symbol_name(&self, symbol: SymbolId) -> &str {
        self.registry.symbol_name(symbol)
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

    #[cfg(test)]
    pub fn binding_name(&self, binding: BindingView<'_>) -> &str {
        self.symbol_name(binding.symbol)
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

    pub fn typed_expression_facts_in_range(
        &self,
        range: Range,
    ) -> Vec<(&SpanKey, &ExpressionFact)> {
        self.registry
            .expressions
            .iter()
            .filter(|(span, _)| is_range_within_range(span.range, range))
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
                let name = self.registry.symbol_name(binding.symbol);
                if name.starts_with(prefix) && seen.insert(binding.symbol) {
                    out.push((name.to_string(), binding.state.kind));
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

    #[cfg(test)]
    pub fn new(root: M2Node<'_>) -> Self {
        let source = DocumentSource::new(root.text().to_string());
        Self::new_with_knowledge(root, &source, &NoTypeKnowledge)
    }

    pub fn new_with_knowledge(
        root: M2Node<'_>,
        source: &(impl SourceNavigation + ?Sized),
        builtins: &(impl TypeKnowledge + ?Sized),
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
            installations: Vec::new(),
            cache_types: false,
            type_cache: RwLock::new(HashMap::new()),
        };
        // Analysis-first: derive the semantic metadata (scopes, expression facts,
        // method installations) BEFORE running diagnostics, which are almost
        // entirely semantic and consume that metadata rather than re-deriving it.
        analysis.build_scopes(root, source, 0, 0, builtins);
        // Scope construction needs source-ordered partial information. Once all
        // bindings and states exist, inference is stable and each node's final
        // type can be memoized for all semantic consumers in this snapshot.
        analysis.cache_types = true;
        analysis.collect_expression_facts(root, source, builtins);
        analysis.collect_installation_diagnostics(builtins);
        analysis.collect_install_form_diagnostics(root, source, builtins);
        analysis.collect_diagnostics(root, source, builtins);
        analysis.collect_unused_binding_diagnostics(root, source);
        analysis
    }

    fn build_scopes(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        current_scope_idx: usize,
        assignment_scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) {
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
                    self.record_method_installation(node, source, builtins);
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
                            Some(InstanceID::new("MethodFunction"))
                        } else {
                            self.type_of(right, source, current_scope_idx, builtins)
                                .dispatch_id()
                        }
                    });
                    let parent_type = right.and_then(|right| {
                        declared_type_parent(right, type_name.as_ref(), builtins)
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
                            || builtins.is_subtype(type_name.name(), "Ring")
                        {
                            self.collect_ring_generator_bindings(
                                ring_name, right, left, source, builtins,
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
                builtins,
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
        self.installations
            .iter()
            .find(|installation| installation.span == span)
    }

    /// The function being installed by a method-installation assignment
    /// `lhs := [Codomain =>] fn`, or `None` for an ordinary assignment/call. An
    /// explicit `Codomain => fn` return-type declaration is peeled to its `fn`, so
    /// the installation's value is the function, never the `Codomain =>` "Option".
    fn installed_function<'tree>(
        &self,
        node: M2Node<'tree>,
        source: &(impl SourceNavigation + ?Sized),
    ) -> Option<M2Node<'tree>> {
        if !node.is_assignment() {
            return None;
        }
        self.installation_for(node, source)?;
        let right = node.child_by_field_name("right")?;
        Some(if right.is_option_assignment() {
            right.child_by_field_name("right").unwrap_or(right)
        } else {
            right
        })
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<MethodInstallation> {
        let operator = node.binary_operator()?;
        let left = node.child_by_field_name("left")?;
        let (head, domain) = self.installation_shape(left, builtins)?;
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
            .map(InstanceID::new);
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
                id,
                head,
                domain,
                codomain,
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
                    if op.fixity == Fixity::Binary
                        && domain
                            .iter()
                            .all(|operand| self.operand_is_type(operand.name(), builtins)) =>
                {
                    Some(MethodInstallation {
                        id,
                        head: MethodHead::Operator(op),
                        domain,
                        codomain,
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<(MethodHead, Vec<InstanceID>)> {
        // A parenthesized expression is identified with its final value, so
        // `(T op S) := f` installs exactly like `T op S := f`. A final `muted`
        // child means the group evaluates to null and is not an installation
        // target.
        if node.kind == NodeKind::ParenthesizedExpression {
            let inner = node.final_value_child()?;
            return self.installation_shape(inner, builtins);
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
                    if self.operand_is_type(left_name, builtins) {
                        let right_name = symbol_node_text(right)?;
                        Some((
                            MethodHead::Operator(Operator {
                                token: SPACE_OPERATOR.to_string(),
                                fixity: Fixity::Binary,
                            }),
                            vec![InstanceID::new(left_name), InstanceID::new(right_name)],
                        ))
                    } else {
                        Some((
                            MethodHead::Function(left_name.to_string()),
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
                            token: operator.to_string(),
                            fixity: Fixity::Binary,
                        }),
                        vec![
                            InstanceID::new(symbol_node_text(left)?),
                            InstanceID::new(symbol_node_text(right)?),
                        ],
                    ))
                }
            }
            NodeKind::PrefixExpression => Some((
                MethodHead::Operator(Operator {
                    token: operator_text(node)?.to_string(),
                    fixity: Fixity::Prefix,
                }),
                vec![InstanceID::new(symbol_node_text(
                    node.child_by_field_name("operand")?,
                )?)],
            )),
            NodeKind::PostfixExpression => Some((
                MethodHead::Operator(Operator {
                    token: operator_text(node)?.to_string(),
                    fixity: Fixity::Postfix,
                }),
                vec![InstanceID::new(symbol_node_text(
                    node.child_by_field_name("operand")?,
                )?)],
            )),
            _ => None,
        }
    }

    /// Whether `name` denotes a TYPE — the hinge of the installation rules. The
    /// type universe is layered (see the loaded-package / scoped-index design):
    /// a local binding whose inferred class is `Type` (e.g. `X = new Type of …`)
    /// shadows builtins, then the builtin type records are consulted. The two
    /// stores keep their own lifecycles; only this query unifies them.
    fn operand_is_type(&self, name: &str, builtins: &(impl TypeKnowledge + ?Sized)) -> bool {
        self.local_binding_is_type(name, builtins)
            || builtins
                .get_record(&InstanceID::new(name))
                .is_some_and(|record| record.type_info().is_some())
    }

    /// Whether any local binding named `name` is a type — its inferred static
    /// class is `Type` or a `Type` descendant.
    fn local_binding_is_type(&self, name: &str, builtins: &(impl TypeKnowledge + ?Sized)) -> bool {
        let Some(symbol) = self.registry.resolve_symbol(name) else {
            return false;
        };
        self.registry
            .bindings_by_symbol
            .get(&symbol)
            .into_iter()
            .flatten()
            .filter_map(|binding_id| self.binding_definition(*binding_id))
            .any(|binding| {
                binding
                    .state
                    .type_name
                    .as_ref()
                    .is_some_and(|type_name| type_name_denotes_type(type_name, builtins))
            })
    }

    /// Emit a diagnostic for every stored installation that M2 would reject or
    /// silently ignore. Installation shapes were characterized during the
    /// source-ordered scope pass; this phase only consumes those facts.
    fn collect_installation_diagnostics(&mut self, builtins: &(impl TypeKnowledge + ?Sized)) {
        // Validity hinges on the type universe: adjacency `A B := …` is a SPACE
        // operator install when `A` is a type but a function-head install
        // otherwise, and the two have different domains (hence different arities).
        // Without external facts we cannot tell them apart, so stay monotone.
        if !builtins.is_available() {
            return;
        }
        let mut diagnostics = Vec::new();
        for installation in &self.installations {
            self.installation_diagnostics(installation, builtins, &mut diagnostics);
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) {
        let mut diagnostics = Vec::new();
        self.scan_install_form(node, source, builtins, &mut diagnostics);
        self.diagnostics.extend(diagnostics);
    }

    fn scan_install_form(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        builtins: &(impl TypeKnowledge + ?Sized),
        out: &mut Vec<Diagnostic>,
    ) {
        if let Some(name) = self.illegal_equals_install_head(node, builtins) {
            out.push(M2Diagnostic::InstallNeedsColonEquals.at(
                source.range_for_node(node),
                format!(
                    "Installing a method on `{name}` must use `:=`, not `=`: M2 rejects this \
                     (\"no method for storing values of function {name}\"). Use `:=`."
                ),
            ));
        }
        for child in node.children() {
            self.scan_install_form(child, source, builtins, out);
        }
    }

    /// The function name when `node` is `f Domain = fn` — an `=` assignment whose
    /// left side is a function-head install shape, whose right side is a lambda
    /// (install intent, not a value store), and whose head resolves to a function.
    /// `None` otherwise.
    fn illegal_equals_install_head(
        &self,
        node: M2Node,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<String> {
        if !node.is_assignment() || node.binary_operator() != Some("=") {
            return None;
        }
        let right = node.child_by_field_name("right")?;
        if right.kind != NodeKind::LambdaExpression {
            return None;
        }
        let left = node.child_by_field_name("left")?;
        let (MethodHead::Function(name), _) = self.installation_shape(left, builtins)? else {
            return None;
        };
        // M2 rejects `f Domain = fn` for ANY function head, method function or
        // not ("no method for storing values of function f"); verified against
        // v1.26.05. Stay silent only when `name` does not resolve to a function.
        (self.head_function_kind(&name, builtins) != HeadFunctionKind::Unknown).then_some(name)
    }

    /// The diagnostics for a single installation: a no-effect warning on a
    /// non-method-function head, a hard error on a non-flexible operator form, and
    /// a hard error when a fixed-arity RHS disagrees with the installed domain.
    fn installation_diagnostics(
        &self,
        installation: &MethodInstallation,
        builtins: &(impl TypeKnowledge + ?Sized),
        out: &mut Vec<Diagnostic>,
    ) {
        match &installation.head {
            MethodHead::Function(name) => {
                if self.head_function_kind(name, builtins) == HeadFunctionKind::NonMethodFunction {
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
                let form = fixity_form(operator.fixity);
                if self.operator_form_is_flexible(&operator.token, form, builtins) == Some(false) {
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
        form: &str,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<bool> {
        let record = builtins.get_record(&InstanceID::new(token))?;
        let operator_info = record.operator_info()?;
        Some(operator_info.is_flexible(form))
    }

    /// Classify an installation's function head by method-function-ness, querying
    /// the layered type universe (local bindings first, then builtins).
    fn head_function_kind(
        &self,
        name: &str,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> HeadFunctionKind {
        // A local function binding shadows any builtin. Its callable kind is
        // recorded from the defining syntax, without reverse-engineering the
        // behavior from a runtime class-name catalog.
        if let Some(kind) = self.local_function_kind(name) {
            return match kind {
                LocalFunctionKind::Method => HeadFunctionKind::MethodFunction,
                LocalFunctionKind::Plain => HeadFunctionKind::NonMethodFunction,
                LocalFunctionKind::Unknown => HeadFunctionKind::Unknown,
            };
        }
        // The generated index records whether a builtin is a method function
        // directly from its corpus kind.
        if let Some(record) = builtins.get_record(&InstanceID::new(name)) {
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
        let symbol = self.registry.resolve_symbol(name)?;
        self.registry
            .bindings_by_symbol
            .get(&symbol)?
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
                .get(&symbol)
                .map_or(LocalFunctionKind::Unknown, |function| function.kind),
        )
    }

    fn collect_parameters(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        parameter_types: Option<&[InstanceID]>,
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
                            .registry
                            .resolve_symbol(name)
                            .and_then(|symbol| {
                                self.binding_id_from_scope(symbol, registration.scope_idx, position)
                            })
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
        let symbol_id = self.registry.intern_symbol(name);
        if let Some(parent_type) = parent_type {
            self.registry.type_parents.insert(symbol_id, parent_type);
        }
        let binding_id = BindingId(self.registry.bindings.len() as u32);
        let state_id = BindingStateId(self.registry.binding_states.len() as u32);
        let range = source.range_for_node(node);
        let binding = BindingInfo {
            binding_id,
            symbol: symbol_id,
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
            .bindings_by_symbol
            .entry(symbol_id)
            .or_default()
            .push(binding_id);
    }

    fn add_binding_state(
        &mut self,
        binding_id: BindingId,
        registration: SymbolRegistration<'_>,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let Some(symbol) = self.binding(binding_id).map(|binding| binding.symbol) else {
            return;
        };
        match registration.parent_type {
            Some(parent_type) => {
                self.registry.type_parents.insert(symbol, parent_type);
            }
            None => {
                self.registry.type_parents.remove(&symbol);
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) {
        let containers = expression
            .descendants()
            .filter(|node| node.is_space_application())
            .filter_map(|node| {
                let head = node.child_by_field_name("left")?;
                let variables = ring_constructor_variables(node)?;
                self.type_of(head, source, 0, builtins)
                    .principal()
                    .is_some_and(|head_type| builtins.is_subtype(head_type.as_ref(), "Ring"))
                    .then_some(variables)
            })
            .collect::<Vec<_>>();

        let mut generators = Vec::new();
        for container in containers {
            for generator in ring_generator_bindings(container) {
                let symbol = self.registry.intern_symbol(&generator.name);
                generators.push(RingGenerator {
                    symbol,
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
                .and_then(|source| self.registry.resolve_symbol(source))
                .and_then(|source| self.registry.ring_generators.get(&source).cloned())
                .unwrap_or_default();
            for generator in &generators {
                let name = self.registry.symbol_name(generator.symbol).to_string();
                self.register_ring_generator(ring_name, &name, generator.kind, rebind_node, source);
            }
        }

        let ring = self.registry.intern_symbol(ring_name);
        self.registry.ring_generators.insert(ring, generators);
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
                    InstanceID::new(ring_name),
                    None,
                    source,
                );
            }
            RingGeneratorKind::IndexedTable => {
                self.register_dynamic_global(
                    generator_name,
                    node,
                    InstanceID::new("IndexedVariableTable"),
                    Some(InstanceID::new(ring_name)),
                    source,
                );
            }
        }
    }

    fn register_dynamic_global(
        &mut self,
        name: &str,
        node: M2Node,
        type_name: InstanceID,
        indexed_element_type: Option<InstanceID>,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let position = source.position_for_node(node);
        let binding_id = self
            .registry
            .resolve_symbol(name)
            .and_then(|symbol| self.binding_id_from_scope(symbol, 0, position))
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
        let MethodHead::Function(name) = &installation.head else {
            return None;
        };
        let method = self.function(name)?;
        method
            .methods
            .contains(&installation.id)
            .then_some((method, installation))
    }

    pub fn infer_call_static_facts(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> CallStaticFacts {
        let scope_idx = self
            .find_scope_at(source.position_for_node(node))
            .unwrap_or(0);
        self.infer_call_facts(node, source, scope_idx, builtins)
    }

    pub fn infer_expression_static_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InstanceID> {
        let scope_idx = self
            .find_scope_at(source.position_for_node(node))
            .unwrap_or(0);
        self.type_of(node, source, scope_idx, builtins)
            .dispatch_id()
    }

    /// Project inferred types into the nominal names understood by the builtin
    /// dispatch table. Locally-created runtime types (most importantly a ring
    /// such as `R = QQ[x]`) walk through the local parent registry first, so an
    /// element whose exact class is `R` dispatches as a `RingElement`.
    pub fn dispatch_argument_types(&self, facts: &CallStaticFacts) -> Vec<Option<InstanceID>> {
        facts
            .argument_types
            .iter()
            .map(|inferred| self.dispatch_type_id(inferred))
            .collect()
    }

    fn dispatch_type_id(&self, inferred: &InferredType) -> Option<InstanceID> {
        let mut current = inferred.principal()?.clone();
        let mut visited = HashSet::new();

        while let Some(symbol) = self.registry.resolve_symbol(current.name()) {
            if !visited.insert(symbol) {
                return None;
            }
            let Some(parent) = self.registry.type_parents.get(&symbol) else {
                break;
            };
            current.clone_from(parent);
        }

        Some(current)
    }

    /// Record the [`Dispatch`] shape of a lambda-defined local function on its
    /// function record, creating the record if this is its first mention.
    fn record_local_function_dispatch(&mut self, name: &str, dispatch: Dispatch) {
        let symbol = self.registry.intern_symbol(name);
        let function = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
                kind: LocalFunctionKind::Unknown,
            });
        function.dispatch = Some(dispatch);
        function.kind = LocalFunctionKind::Plain;
    }

    fn record_local_method_declaration(&mut self, name: &str, typical_value: Option<InstanceID>) {
        let symbol = self.registry.intern_symbol(name);
        let method = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) {
        let id = MethodInstallationId(self.installations.len() as u32);
        let Some(mut installation) = self.classify_installation(id, assignment, source, builtins)
        else {
            return;
        };

        // Preserve M2's distinct assignment-method form: only `:=` contributes
        // a callable signature here. `=` installations are retained for
        // diagnostics/document symbols but are not ordinary call methods.
        if assignment.binary_operator() == Some(":=") {
            self.attach_method_installation(&mut installation, builtins);
        }

        debug_assert_eq!(installation.id.0 as usize, self.installations.len());
        self.installations.push(installation);
    }

    fn attach_method_installation(
        &mut self,
        installation: &mut MethodInstallation,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) {
        let name = match &installation.head {
            MethodHead::Function(name) => {
                // An install on a non-method-function compiles but has no effect,
                // so it creates no method record.
                if self.head_function_kind(name, builtins) == HeadFunctionKind::NonMethodFunction {
                    return;
                }
                name.as_str()
            }
            MethodHead::Operator(operator) => operator.token.as_str(),
        };
        let symbol = self.registry.intern_symbol(name);
        let method = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
                kind: LocalFunctionKind::Unknown,
            });
        if installation.codomain.is_none() {
            installation.codomain.clone_from(&method.typical_value);
        }
        method.methods.push(installation.id);
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
        self.registry
            .node_scopes
            .insert(SpanKey::from_node(source, node), scope_idx);
        scope_idx
    }

    fn collect_expression_facts(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        builtins: &(impl TypeKnowledge + ?Sized),
    ) {
        let position = source.position_for_node(node);
        let scope_idx = self.find_scope_at(position).unwrap_or(0);
        let key = SpanKey::from_node(source, node);
        self.registry.node_scopes.insert(key.clone(), scope_idx);

        // A method installation `lhs := [Codomain =>] fn` is not a value
        // assignment: the LHS is a method key and `Codomain =>` is a return-type
        // declaration, not an `Option`. Type the whole node as the installed
        // function and descend only into the function body, so the install syntax
        // (the LHS and the `Codomain =>` wrapper) gets no misleading value hints.
        if let Some(function) = self.installed_function(node, source) {
            if let Some(kind) = expression_kind(node) {
                let result_type = self.type_of(function, source, scope_idx, builtins);
                self.registry.expressions.insert(
                    key.clone(),
                    ExpressionFact {
                        kind,
                        input_nodes: Vec::new(),
                        operator: None,
                        result_type,
                        scope_idx,
                    },
                );
            }
            self.collect_expression_facts(function, source, builtins);
            return;
        }

        if let Some(kind) = expression_kind(node) {
            let result_type = self.type_of(node, source, scope_idx, builtins);
            let input_nodes = expression_inputs(node)
                .into_iter()
                .map(|child| SpanKey::from_node(source, child))
                .collect();
            let operator = expression_operator_text(node).map(ToString::to_string);
            self.registry.expressions.insert(
                key.clone(),
                ExpressionFact {
                    kind,
                    input_nodes,
                    operator,
                    result_type,
                    scope_idx,
                },
            );

            if let Some(call_info) =
                self.call_info_for_expression(node, source, scope_idx, builtins)
            {
                self.registry.calls.insert(key.clone(), call_info);
            }
        }

        for child in node.children() {
            self.collect_expression_facts(child, source, builtins);
        }
    }

    fn call_info_for_expression(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<CallInfo> {
        if !matches!(
            node.kind,
            NodeKind::BinaryExpression | NodeKind::PrefixExpression
        ) {
            return None;
        }

        if node.is_assignment() || node.is_option_assignment() {
            return None;
        }

        if node.is_space_application() {
            let callable = node.child_by_field_name("left")?;
            let argument = node.child_by_field_name("right")?;
            let callable_name = symbol_node_text(callable).map(ToString::to_string);
            let facts = self.infer_call_facts_for_callable(
                argument,
                source,
                scope_idx,
                callable_name.as_deref(),
                builtins,
            );
            return Some(CallInfo {
                callable_name,
                argument_types: facts.argument_types,
            });
        }

        let operator = expression_operator_text(node)?;
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");
        let operand = node.child_by_field_name("operand");
        let argument_types = if let Some(operand) = operand {
            vec![self.type_of(operand, source, scope_idx, builtins)]
        } else {
            vec![
                left.map_or_else(InferredType::unknown, |child| {
                    self.type_of(child, source, scope_idx, builtins)
                }),
                right.map_or_else(InferredType::unknown, |child| {
                    self.type_of(child, source, scope_idx, builtins)
                }),
            ]
        };

        Some(CallInfo {
            callable_name: Some(operator.to_string()),
            argument_types,
        })
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        if !self.cache_types {
            return self.compute_type_of(node, source, scope_idx, builtins);
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

        let inferred = self.compute_type_of(node, source, scope_idx, builtins);
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
        builtins: &(impl TypeKnowledge + ?Sized),
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
                Some(inner) => self.type_of(inner, source, scope_idx, builtins),
                None => InferredType::of("Nothing"),
            },
            NodeKind::StringLiteral => InferredType::of("String"),
            NodeKind::IntegerLiteral => InferredType::of("ZZ"),
            NodeKind::FloatLiteral => InferredType::of("RR"),
            // A quote expression (`symbol +`, `local x`, `global y`,
            // `threadLocal z`) evaluates to the Symbol it names.
            NodeKind::QuoteExpression => InferredType::of("Symbol"),
            NodeKind::Symbol => self.symbol_type(node, source, scope_idx, builtins),
            // An assignment evaluates to its right-hand side: `a = b` / `a := b`
            // (and destructuring `{x,y} := …`) take the type of the RHS.
            _ if node.is_assignment() => match node.child_by_field_name("right") {
                Some(right) => self.type_of(right, source, scope_idx, builtins),
                None => InferredType::unknown(),
            },
            // `x => y` builds an `Option` object, whatever the operand types.
            _ if node.is_option_assignment() => InferredType::of("Option"),
            NodeKind::BinaryExpression => {
                self.binary_expression_type(node, source, scope_idx, builtins)
            }
            NodeKind::PrefixExpression | NodeKind::PostfixExpression => {
                self.unary_operator_type(node, source, scope_idx, builtins)
            }
            NodeKind::NewStatement => node
                .child_by_field_name("type")
                .filter(|type_node| type_node.kind == NodeKind::Symbol)
                .map(|type_node| InferredType::of(type_node.text()))
                .unwrap_or_else(InferredType::unknown),
            // `if c then A [else B]` is whichever branch runs; with no `else`,
            // a false condition yields `null` (`Nothing`). The static type is the
            // join of the reachable branch types.
            NodeKind::IfStatement => self.if_statement_type(node, source, scope_idx, builtins),
            // `try E [then A] [else B | except e do B]` is the success value
            // (`then A`, else `E`) joined with the failure value (`else`/`do B`,
            // else `null` since an unhandled error makes `try` evaluate to null).
            NodeKind::TryStatement => self.try_statement_type(node, source, scope_idx, builtins),
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
                self.control_transfer_type(node, source, scope_idx, builtins)
            }
            // A debug clause (`time E`, `break v`, …) passes through to the value
            // of its inner statement/expression.
            NodeKind::DebugClause => node
                .named_children()
                .next()
                .map(|inner| self.type_of(inner, source, scope_idx, builtins))
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let then_type = clause_of(node, NodeKind::ThenClause)
            .and_then(clause_value)
            .map(|value| self.type_of(value, source, scope_idx, builtins))
            .unwrap_or_else(InferredType::unknown);
        let else_type = match clause_of(node, NodeKind::ElseClause).and_then(clause_value) {
            Some(value) => self.type_of(value, source, scope_idx, builtins),
            None => InferredType::of("Nothing"),
        };
        then_type.join(else_type, builtins)
    }

    /// The type of a `try` statement: the success value (`then` clause if present,
    /// else the guarded body) joined with the failure value (`else`/`do` clause if
    /// present, else `Nothing` since an unhandled error makes `try` yield `null`).
    fn try_statement_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let body = node
            .named_children()
            .find(|child| !is_try_clause(child.kind));
        let success_value = clause_of(node, NodeKind::ThenClause)
            .and_then(clause_value)
            .or(body);
        let success = success_value
            .map(|value| self.type_of(value, source, scope_idx, builtins))
            .unwrap_or_else(InferredType::unknown);
        let failure_value = clause_of(node, NodeKind::ElseClause)
            .or_else(|| clause_of(node, NodeKind::DoClause))
            .and_then(clause_value);
        let failure = match failure_value {
            Some(value) => self.type_of(value, source, scope_idx, builtins),
            None => InferredType::of("Nothing"),
        };
        success.join(failure, builtins)
    }

    /// The type of a control transfer (`return e` / `break e` / `continue e`):
    /// its operand's type, or `Nothing` when the transfer is bare.
    fn control_transfer_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        match node.named_children().next() {
            Some(operand) => self.type_of(operand, source, scope_idx, builtins),
            None => InferredType::of("Nothing"),
        }
    }

    /// A symbol's type, in precedence order: an in-scope user binding (which
    /// overrides an indexed name — protection, once tracked, will forbid this for
    /// protected names), then the index's recorded class, then `Symbol` (an
    /// unbound name evaluates to its own `Symbol` in M2).
    fn symbol_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let name = node.text();
        if let Some(binding) =
            self.get_binding_from_scope(name, scope_idx, source.position_for_node(node))
        {
            if let Some(type_name) = &binding.state.type_name {
                return InferredType::from_id(type_name.clone());
            }
        }

        if let Some(record) = builtins.get_record(&InstanceID::new(name)) {
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        if node.is_space_application() {
            return self.application_type(node, source, scope_idx, builtins);
        }

        let operator = node.binary_operator();
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");

        if let Some(operator) = operator {
            if let Some(result) =
                self.special_operator_type(operator, left, right, source, scope_idx, builtins)
            {
                return result;
            }
        }

        let (Some(operator), Some(left), Some(right)) = (operator, left, right) else {
            return InferredType::unknown();
        };
        let left_type = self.type_of(left, source, scope_idx, builtins);
        let right_type = self.type_of(right, source, scope_idx, builtins);
        self.dispatch_codomain(builtins, operator, &[left_type, right_type], &[])
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let left = left?;
        let left_type = self.type_of(left, source, scope_idx, builtins);
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

        if matches!(operator, "_" | "@@") && builtins.is_subtype(left_name.as_ref(), "Function") {
            return Some(InferredType::of("FunctionClosure"));
        }

        if operator == "/" && builtins.is_subtype(left_name.as_ref(), "Ring") {
            let right_type = self.type_of(right?, source, scope_idx, builtins);
            let right_name = right_type.principal()?;
            if right_name.as_ref() == "ZZ" {
                return Some(InferredType::of("QuotientRing"));
            }
        }

        None
    }

    /// Application `f SPACE x`. A `Function` head delegates to the head's own
    /// signatures (LSP-internal dependent info resolved against the corpus),
    /// stepping beyond the M2 table whose `(Function, Thing)` row only yields
    /// `Thing`. A non-`Function` head dispatches `SPACE` through the table
    /// (`Ring × Array → PolynomialRing`).
    fn application_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
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
            builtins,
        );

        // A locally-defined function is known to be a function from the registry
        // alone, so its application resolves without the builtin lattice: its
        // signatures give the codomain, and an undocumented one yields `Thing`
        // (applying a function gives at least a Thing).
        if let Some(callable) = callable_name {
            if self.is_local_function(callable) {
                return self
                    .resolve_local_call_return_type(callable, &call_facts.argument_types, builtins)
                    .map_or_else(|| InferredType::of("Thing"), InferredType::from_id);
            }
        }

        // Otherwise the lattice decides whether the head is a function (delegating
        // to its signatures) or another SPACE method (`Ring × Array →
        // PolynomialRing`).
        let head = self.type_of(callable_node, source, scope_idx, builtins);
        let head_is_function = head
            .principal()
            .is_some_and(|head| builtins.is_subtype(head.as_ref(), "Function"));
        if head_is_function {
            if let Some(callable) = callable_name {
                if let Some(return_type) = builtins.resolve_call_return_type_with_options(
                    callable,
                    &self.dispatch_argument_types(&call_facts),
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
            builtins,
        ) {
            return result;
        }

        let argument_type = self.type_of(argument_node, source, scope_idx, builtins);
        self.dispatch_codomain(
            builtins,
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
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InferredType> {
        let head_name = head.principal()?;
        if !builtins.is_subtype(head_name.as_ref(), "Ring") {
            return None;
        }

        let operator = argument.binary_operator()?;
        let variables = argument.child_by_field_name("left")?;
        if !variables.kind.is_collection_expression() {
            return None;
        }
        let trailing_operand = argument.child_by_field_name("right")?;

        let variables_type = self.type_of(variables, source, scope_idx, builtins);
        let ring_type = self.dispatch_codomain(
            builtins,
            SPACE_OPERATOR,
            &[head.clone(), variables_type],
            &[],
        );
        let ring_name = ring_type.principal()?;
        if !builtins.is_subtype(ring_name.as_ref(), "Ring") {
            return None;
        }

        let trailing_type = self.type_of(trailing_operand, source, scope_idx, builtins);
        let result = builtins.resolve_call_return_type_with_options(
            operator,
            &[
                self.dispatch_type_id(&ring_type),
                self.dispatch_type_id(&trailing_type),
            ],
            &[],
        )?;
        Some(InferredType::from_id(result))
    }

    /// Whether `name` resolves to a function tracked in the local registry — a
    /// lambda binding or a local method declaration. Such a head is known to be a
    /// function without consulting the builtin lattice.
    fn is_local_function(&self, name: &str) -> bool {
        self.registry
            .resolve_symbol(name)
            .is_some_and(|symbol| self.registry.functions.contains_key(&symbol))
    }

    /// A prefix/postfix operator's type: `typicalValue(op, operand)`.
    fn unary_operator_type(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> InferredType {
        let (Some(operator), Some(operand)) =
            (operator_text(node), node.child_by_field_name("operand"))
        else {
            return InferredType::unknown();
        };
        let operand_type = self.type_of(operand, source, scope_idx, builtins);
        self.dispatch_codomain(builtins, operator, &[operand_type], &[])
    }

    /// Dispatch `callable` on `args` through the M2 type table. A matched but
    /// undocumented codomain is `Thing` (≡ a null `typicalValue` under the
    /// lower-bound reading) — approximated by "the callable/operator is a known
    /// index entry, so it dispatches"; an unidentifiable head stays `Unknown`.
    fn dispatch_codomain(
        &self,
        builtins: &(impl TypeKnowledge + ?Sized),
        callable: &str,
        args: &[InferredType],
        options: &[(String, String)],
    ) -> InferredType {
        if let Some(return_type) = self.resolve_local_call_return_type(callable, args, builtins) {
            return InferredType::from_id(return_type);
        }
        if let Some(return_type) = builtins.resolve_call_return_type_with_options(
            callable,
            &args
                .iter()
                .map(|argument| self.dispatch_type_id(argument))
                .collect::<Vec<_>>(),
            options,
        ) {
            return InferredType::from_id(return_type);
        }
        if builtins.get_record(&InstanceID::new(callable)).is_some() {
            return InferredType::of("Thing");
        }
        InferredType::unknown()
    }

    fn infer_call_facts(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> CallStaticFacts {
        self.infer_call_facts_for_callable(node, source, scope_idx, None, builtins)
    }

    fn infer_call_facts_for_callable(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        scope_idx: usize,
        callable: Option<&str>,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> CallStaticFacts {
        // A single parenthesized argument `f(x)` / `f(opt => v)` denotes its inner
        // value; peel it so the argument is classified like a bare argument.
        let node = parenthesized_value(node).unwrap_or(node);
        let receives_sequence =
            callable.is_some_and(|name| self.callable_receives_sequence(name, builtins));
        if node.kind == NodeKind::Sequence && !receives_sequence {
            let mut facts = CallStaticFacts::default();
            for child in node.collection_elements() {
                if let Some(option) = literal_option_assignment(child) {
                    facts.literal_options.push(option);
                } else {
                    facts
                        .argument_types
                        .push(self.type_of(child, source, scope_idx, builtins));
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
            argument_types: vec![self.type_of(node, source, scope_idx, builtins)],
            literal_options: Vec::new(),
        }
    }

    fn callable_receives_sequence(
        &self,
        name: &str,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        let local_dispatch = self
            .registry
            .resolve_symbol(name)
            .and_then(|symbol| self.registry.functions.get(&symbol))
            .and_then(|function| function.dispatch);
        if local_dispatch == Some(Dispatch::Variadic) {
            return true;
        }

        builtins
            .get_record(&InstanceID::new(name))
            .is_some_and(|record| bare_class_name(record.class.as_ref()) == "MethodFunctionSingle")
    }

    fn resolve_local_call_return_type(
        &self,
        callable: &str,
        argument_types: &[InferredType],
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> Option<InstanceID> {
        let symbol = self.registry.resolve_symbol(callable)?;
        let method = self.registry.functions.get(&symbol)?;
        let matching_codomains = self
            .methods_for(method)
            .filter(|signature| self.signature_matches(signature, argument_types, builtins))
            .filter_map(|signature| signature.codomain.as_ref())
            .cloned()
            .collect::<HashSet<_>>();

        if matching_codomains.len() == 1 {
            return matching_codomains.into_iter().next();
        }

        method.typical_value.clone()
    }

    fn signature_matches(
        &self,
        signature: &MethodInstallation,
        argument_types: &[InferredType],
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        self.signature_matches_domain(&signature.domain, argument_types, builtins)
    }

    fn signature_matches_domain(
        &self,
        expected_domain: &[InstanceID],
        argument_types: &[InferredType],
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        expected_domain.len() == argument_types.len()
            && expected_domain
                .iter()
                .zip(argument_types)
                .all(|(expected, actual)| {
                    actual
                        .principal()
                        .is_some_and(|actual| self.is_subtype(actual, expected, builtins))
                })
    }

    fn is_subtype(
        &self,
        actual: &InstanceID,
        expected: &InstanceID,
        builtins: &(impl TypeKnowledge + ?Sized),
    ) -> bool {
        if actual == expected || builtins.is_subtype(actual.name(), expected.name()) {
            return true;
        }

        let mut current = actual.0.as_str();
        let mut visited = HashSet::new();
        while let Some(symbol) = self.registry.resolve_symbol(current) {
            if !visited.insert(symbol) {
                return false;
            }
            let Some(parent) = self.registry.type_parents.get(&symbol) else {
                return false;
            };
            if parent == expected || builtins.is_subtype(parent.name(), expected.name()) {
                return true;
            }
            current = parent.name();
        }
        false
    }

    pub fn binding_id_at(&self, name: &str, pos: Position) -> Option<BindingId> {
        let scope_idx = self.find_scope_at(pos)?;
        let symbol = self.registry.resolve_symbol(name)?;
        self.binding_id_from_scope(symbol, scope_idx, pos)
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
    type_name: Option<InstanceID>,
    indexed_element_type: Option<InstanceID>,
    parent_type: Option<InstanceID>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RingGenerator {
    symbol: SymbolId,
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

fn expression_kind(node: M2Node<'_>) -> Option<ExpressionKind> {
    match node.kind {
        kind if kind.is_literal() => Some(ExpressionKind::Literal),
        NodeKind::Symbol => Some(ExpressionKind::Name),
        kind if kind.is_collection_expression()
            || kind == NodeKind::NakedSequence
            || kind == NodeKind::Cell =>
        {
            Some(ExpressionKind::ScopeExpr)
        }
        // A parenthesized expression takes its final value's kind (`(a+b)` is an
        // `Expr`, `(x)` a `Name`). A group ending in `muted` still has the
        // explicit `Nothing` value, represented as a scope expression.
        NodeKind::ParenthesizedExpression => match parenthesized_value(node) {
            Some(value) => expression_kind(value),
            None => Some(ExpressionKind::ScopeExpr),
        },
        NodeKind::IfStatement
        | NodeKind::WhileStatement
        | NodeKind::ForStatement
        | NodeKind::NewStatement
        | NodeKind::TryStatement
        | NodeKind::DebugClause => Some(ExpressionKind::ControlExpr),
        kind if kind.is_control_transfer() => Some(ExpressionKind::ControlExpr),
        NodeKind::LambdaExpression
        | NodeKind::BinaryExpression
        | NodeKind::PrefixExpression
        | NodeKind::PostfixExpression => {
            if node.is_assignment() {
                Some(ExpressionKind::Assign)
            } else {
                Some(ExpressionKind::Expr)
            }
        }
        _ => None,
    }
}

fn expression_inputs(node: M2Node<'_>) -> Vec<M2Node<'_>> {
    [
        "left",
        "right",
        "operand",
        "condition",
        "body",
        "parameters",
    ]
    .into_iter()
    .filter_map(|field| node.child_by_field_name(field))
    .collect()
}

fn expression_operator_text(node: M2Node<'_>) -> Option<&str> {
    node.child_by_field_name("operator")
        .map(|operator| operator.text())
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
    type_name: Option<&InstanceID>,
    builtins: &(impl TypeKnowledge + ?Sized),
) -> Option<InstanceID> {
    if type_name.is_some_and(|type_name| {
        type_name.name() == "Ring" || builtins.is_subtype(type_name.name(), "Ring")
    }) {
        // A ring value is itself a runtime type. Its elements have that ring as
        // their class, while the ring's instance hierarchy starts at
        // `RingElement` (`parent R === RingElement`).
        return Some(InstanceID::new("RingElement"));
    }
    if value.kind != NodeKind::NewStatement
        || !type_name.is_some_and(|type_name| type_name_denotes_type(type_name, builtins))
    {
        return None;
    }
    clause_of(value, NodeKind::OfClause)
        .and_then(clause_value)
        .and_then(symbol_node_text)
        .map(InstanceID::new)
}

pub fn symbol_node_text<'tree>(node: M2Node<'tree>) -> Option<&'tree str> {
    node.kind.is_symbol_like().then(|| node.text())
}

fn method_declaration_typical_value(node: M2Node) -> Option<Option<InstanceID>> {
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

fn find_option_value(node: M2Node, option_name: &str) -> Option<InstanceID> {
    if node.is_option_assignment() {
        let left = node.child_by_field_name("left")?;
        let right = node.child_by_field_name("right")?;
        if symbol_node_text(left) == Some(option_name) {
            return symbol_node_text(right).map(InstanceID::new);
        }
    }

    for child in node.named_children() {
        if let Some(value) = find_option_value(child, option_name) {
            return Some(value);
        }
    }
    None
}

fn literal_option_assignment(node: M2Node) -> Option<(String, String)> {
    if !node.is_option_assignment() {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    let key = symbol_node_text(left)?;
    let value = literal_option_value(right)?;
    Some((key.to_string(), value.to_string()))
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
    type_name: &InstanceID,
    builtins: &(impl TypeKnowledge + ?Sized),
) -> bool {
    type_name.name() == "Type" || builtins.is_subtype(type_name.name(), "Type")
}

pub fn method_installation_signature(node: M2Node) -> Option<(String, Vec<InstanceID>)> {
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
) -> Option<Vec<InstanceID>> {
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

pub fn method_installation_domain(node: M2Node) -> Option<Vec<InstanceID>> {
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
            .map(|child| InstanceID::new(symbol_node_text(child).unwrap_or_else(|| child.text())))
            .collect::<Vec<_>>();
        return (!domain.is_empty()).then_some(domain);
    }

    symbol_node_text(node).map(|name| vec![InstanceID::new(name)])
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

fn is_range_within_range(inner: Range, outer: Range) -> bool {
    let starts_inside = inner.start.line > outer.start.line
        || (inner.start.line == outer.start.line && inner.start.character >= outer.start.character);
    let ends_inside = inner.end.line < outer.end.line
        || (inner.end.line == outer.end.line && inner.end.character <= outer.end.character);
    starts_inside && ends_inside
}

fn is_range_smaller(a: Range, b: Range) -> bool {
    // Very simple check: is a contained in b?
    let starts_inside = a.start.line > b.start.line
        || (a.start.line == b.start.line && a.start.character >= b.start.character);
    let ends_inside =
        a.end.line < b.end.line || (a.end.line == b.end.line && a.end.character <= b.end.character);
    starts_inside && ends_inside && a != b
}
