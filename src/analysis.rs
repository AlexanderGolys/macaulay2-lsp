//! Parse-tree analysis that records lexical bindings, static type facts, and
//! diagnostics for one document snapshot.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tower_lsp::lsp_types::{Diagnostic, Position, Range, SymbolKind};
use tree_sitter::Tree;

use crate::diagnostic_registry::M2Diagnostic;
use crate::meta::{BindingRole, Meta, Metadata};
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::typesystem::{BuiltinData, InstanceID};
use crate::util::{node_position, position_in_range, to_lsp_range};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolName(Arc<str>);

impl SymbolName {
    fn new(name: &str) -> Self {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingStateId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NodeFactId(usize);

#[derive(Debug)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub registry: SemanticRegistry,
    pub installations: Vec<MethodInstallation>,
    cache_types: bool,
    type_cache: RwLock<HashMap<NodeFactId, InferredType>>,
    #[cfg(test)]
    type_computations: AtomicUsize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInfo {
    pub domain: Vec<String>,
    pub codomain: Option<String>,
    pub range: Range,
}

/// A lazily-resolved reference to a type in the unified universe (builtins ∪
/// locally-defined). Holds the written or inferred name; resolution against the
/// layered registry happens at query time, so the reference survives per-edit
/// rebuilds with no dangling pointer into a dropped registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeRef(String);

impl TypeRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn name(&self) -> &str {
        &self.0
    }
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

/// The head of an M2 method key `(head, ...domain)`, mirroring how M2 stores
/// installed methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodHead {
    Function(String),
    Operator(Operator),
    OperatorAssign(Operator),
}

/// A characterized method installation — the single source of truth for "this
/// assignment installs a method", produced once during analysis and consumed by
/// every capability instead of each re-deciding it from raw syntax.
///
/// `domain` is the tuple of dispatch types (e.g. `[ZZ, String]`). `range` is the
/// span of the whole assignment so a consumer can match a node to its fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInstallation {
    pub head: MethodHead,
    pub domain: Vec<TypeRef>,
    pub range: Range,
    pub target: SpanKey,
    pub value: Option<SpanKey>,
    /// The argument shape of the right-hand-side function, when it is a lambda.
    /// Lets the arity diagnostic check it against [`expected_rhs_arity`] without
    /// re-walking the tree. `None` when the RHS is not a plain lambda.
    pub rhs_dispatch: Option<Dispatch>,
}

impl MethodInstallation {
    /// The argument count the right-hand-side function must take: one per domain
    /// type, plus one for the assigned value `z` in an assignment-form install.
    pub fn expected_rhs_arity(&self) -> usize {
        self.domain.len() + usize::from(matches!(self.head, MethodHead::OperatorAssign(_)))
    }
}

/// The classes whose instances are method functions — the only functions a method
/// install actually attaches a method to. Installing on any other function class
/// (`FunctionClosure`, `CompiledFunction`, …) compiles but has no effect, so we
/// warn and record nothing. `method(Options => …)` yields `MethodFunctionWithOptions`;
/// we currently tag every `method(…)` as `MethodFunction`, which is still in this
/// set, so the distinction does not change the verdict.
const METHOD_FUNCTION_CLASSES: [&str; 4] = [
    "MethodFunction",
    "MethodFunctionBinary",
    "MethodFunctionSingle",
    "MethodFunctionWithOptions",
];

/// Strip a corpus `$Package$Name` qualifier down to the bare class name, so a
/// builtin class (`$Core$CompiledFunction`) and a locally-inferred class
/// (`FunctionClosure`) compare on the same footing.
fn bare_class_name(name: &str) -> &str {
    name.rsplit_once('$').map_or(name, |(_, bare)| bare)
}

/// Whether a bare class name is one of the method-function classes.
fn is_method_function_class(class: &str) -> bool {
    METHOD_FUNCTION_CLASSES.contains(&bare_class_name(class))
}

/// Map a resolved function's class to its installation head kind: a
/// method-function class accepts method installs, any other function class
/// does not.
fn head_function_kind_of_class(class: &str) -> HeadFunctionKind {
    if is_method_function_class(class) {
        HeadFunctionKind::MethodFunction
    } else {
        HeadFunctionKind::NonMethodFunction
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
            Dispatch::Fixed(parameters.named_children().count())
        }
        // A single bare parameter binds the whole argument sequence — variadic.
        kind if kind.is_symbol_like() => Dispatch::Variadic,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub symbol: SymbolId,
    pub range: Range,
    pub typical_value: Option<String>,
    pub methods: Vec<MethodInfo>,
    pub dispatch: Option<Dispatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStaticFacts {
    pub argument_types: Vec<InferredType>,
    pub literal_options: Vec<(String, String)>,
}

impl CallStaticFacts {
    /// The argument types in the `Option<String>` form the type-registry dispatch
    /// queries consume.
    pub fn dispatch_argument_types(&self) -> Vec<Option<String>> {
        dispatch_names(&self.argument_types)
    }
}

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
    /// Minimal generators (the most-general types of the up-set); never empty.
    generators: Vec<InstanceID>,
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
            generators: vec![id],
        }
    }

    /// The single generator, when the set is principal — the boundary form the
    /// dispatch queries and inlay display consume in the basic (single-type)
    /// inference. `None` once a branch join has produced several generators.
    fn principal(&self) -> Option<&InstanceID> {
        match self.generators.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// The type name to feed the `Option<String>`-based type-registry dispatch
    /// queries (that API is owned by the WIP type registry and left untouched).
    /// `None` for a non-principal (joined) set, which the basic dispatch cannot
    /// yet represent.
    fn dispatch_name(&self) -> Option<String> {
        self.principal().map(|only| only.0.clone())
    }

    /// The hover/inlay label for this type. Every value has a class — the floor
    /// `Thing` (≡ "unknown") and `Symbol` (an unbound name) are valid, displayable
    /// types — so a single generator always renders; a joined set renders as
    /// `A | B`. `None` only if the set is empty, which constructors never produce.
    pub(crate) fn label(&self) -> Option<String> {
        if self.generators.is_empty() {
            return None;
        }
        Some(
            self.generators
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
    fn join(self, other: Self, builtins: Option<&BuiltinData>) -> Self {
        let mut generators = self.generators;
        for generator in other.generators {
            if !generators.contains(&generator) {
                generators.push(generator);
            }
        }
        if let Some(builtins) = builtins {
            let candidates = generators.clone();
            generators.retain(|generator| {
                !candidates
                    .iter()
                    .any(|other| other != generator && builtins.is_subtype(generator, other))
            });
        }
        Self { generators }
    }
}

/// Project inferred argument types into the `Option<String>` form the type
/// registry's dispatch queries consume.
fn dispatch_names(types: &[InferredType]) -> Vec<Option<String>> {
    types.iter().map(InferredType::dispatch_name).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionKind {
    Literal,
    Name,
    Expr,
    Assign,
    ScopeExpr,
    ControlExpr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingInfo {
    pub binding_id: BindingId,
    pub symbol: SymbolId,
    pub role: BindingRole,
    pub declaration_kind: SymbolKind,
    pub range: Range,
    pub scope_idx: usize,
    pub declaration_range: Range,
    pub definition_state: BindingStateId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingStateInfo {
    pub state_id: BindingStateId,
    pub binding_id: BindingId,
    pub kind: SymbolKind,
    pub type_name: Option<String>,
    pub value_range: Option<Range>,
    pub span: SpanKey,
    pub scope_idx: usize,
}

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
            symbol_kind: Some(self.state.kind),
            binding_role: Some(self.role),
            type_name: self.state.type_name.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeInfo {
    pub range: Range,
    pub parent_idx: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionFact {
    pub kind: ExpressionKind,
    pub input_nodes: Vec<SpanKey>,
    pub operator: Option<String>,
    pub result_type: InferredType,
    pub scope_idx: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallInfo {
    pub callable_name: Option<String>,
    pub argument_types: Vec<InferredType>,
    pub candidate_methods: Vec<MethodInfo>,
}

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
}

impl SpanKey {
    fn from_node(text: &str, node: M2Node) -> Self {
        Self {
            range: to_lsp_range(text, node.range()),
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

    #[cfg(test)]
    pub fn registry(&self) -> &SemanticRegistry {
        &self.registry
    }

    #[cfg(test)]
    fn type_computation_count(&self) -> usize {
        self.type_computations.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn cached_type_count(&self) -> usize {
        self.type_cache
            .read()
            .expect("type cache lock should not be poisoned")
            .len()
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
    pub fn expression_fact(&self, text: &str, node: M2Node) -> Option<&ExpressionFact> {
        self.registry
            .expressions
            .get(&SpanKey::from_node(text, node))
    }

    pub(crate) fn function(&self, name: &str) -> Option<&FunctionInfo> {
        let symbol = self.registry.resolve_symbol(name)?;
        self.registry.functions.get(&symbol)
    }

    pub fn function_by_symbol(&self, symbol: SymbolId) -> Option<&FunctionInfo> {
        self.registry.functions.get(&symbol)
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
    pub fn new(tree: &Tree, text: &str) -> Self {
        Self::new_with_builtins(tree, text, None)
    }

    pub fn new_with_builtins(tree: &Tree, text: &str, builtins: Option<&BuiltinData>) -> Self {
        let mut analysis = Analysis {
            diagnostics: Vec::new(),
            registry: SemanticRegistry {
                scopes: vec![ScopeInfo {
                    range: Range::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                    parent_idx: None,
                }],
                ..Default::default()
            },
            installations: Vec::new(),
            cache_types: false,
            type_cache: RwLock::new(HashMap::new()),
            #[cfg(test)]
            type_computations: AtomicUsize::new(0),
        };
        // Analysis-first: derive the semantic metadata (scopes, expression facts,
        // method installations) BEFORE running diagnostics, which are almost
        // entirely semantic and consume that metadata rather than re-deriving it.
        let root = M2Node::new(tree.root_node(), text);
        analysis.build_scopes(root, text, 0, 0, builtins);
        // Scope construction needs source-ordered partial information. Once all
        // bindings and states exist, inference is stable and each node's final
        // type can be memoized for all semantic consumers in this snapshot.
        analysis.cache_types = true;
        analysis.collect_expression_facts(root, text, builtins);
        analysis.collect_installations(root, text, builtins);
        analysis.collect_installation_diagnostics(builtins);
        analysis.collect_install_form_diagnostics(root, text, builtins);
        analysis.collect_diagnostics(root, text, builtins);
        analysis.collect_unused_binding_diagnostics(root, text);
        analysis
    }

    fn build_scopes(
        &mut self,
        node: M2Node,
        text: &str,
        current_scope_idx: usize,
        assignment_scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) {
        let mut next_scope_idx = current_scope_idx;
        let mut next_assignment_scope_idx = assignment_scope_idx;

        match node.kind {
            NodeKind::LambdaExpression => {
                next_scope_idx = self.push_scope(node, text, Some(current_scope_idx));
                next_assignment_scope_idx = next_scope_idx;

                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let parameter_types = method_installation_parameter_types_for_function(node);
                    self.collect_parameters(
                        params_node,
                        text,
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
                    if op_text == ":=" {
                        self.collect_local_method_installation(left, right, text, builtins);
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
                            Some("MethodFunction".to_string())
                        } else {
                            self.type_of(right, text, current_scope_idx, builtins)
                                .dispatch_name()
                        }
                    });

                    if let (Some(right), Some(name)) =
                        (right, single_symbol_assignment_target(left))
                    {
                        if let Some(typical_value) = method_declaration_typical_value(right) {
                            self.record_local_method_declaration(name, typical_value, left, text);
                        } else if right.kind == NodeKind::LambdaExpression {
                            if let Some(dispatch) = function_dispatch(right) {
                                self.record_local_function_dispatch(name, dispatch, left, text);
                            }
                        }
                    }

                    match op_text {
                        ":=" => self.collect_definitions(
                            left,
                            right,
                            text,
                            DefinitionScope::Local,
                            SymbolRegistration {
                                kind: symbol_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.as_deref(),
                                node: left,
                                value_node: right,
                                scope_idx: current_scope_idx,
                            },
                        ),
                        // `=` writes the nearest enclosing binding of the name, or
                        // creates a global when none exists anywhere up the chain.
                        // The write becomes a new state of that binding rather than
                        // a second lexical definition.
                        "=" => self.collect_definitions(
                            left,
                            right,
                            text,
                            DefinitionScope::Assign,
                            SymbolRegistration {
                                kind: symbol_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.as_deref(),
                                node: left,
                                value_node: right,
                                scope_idx: assignment_scope_idx,
                            },
                        ),
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        // Recurse into children
        for child in node.children() {
            let (child_scope_idx, child_assignment_scope_idx) =
                match child_scope_assignment_is_local(node, child) {
                    Some(assignments_are_local) => {
                        let scope_idx = self.push_scope(child, text, Some(next_scope_idx));
                        let assignment_scope_idx = if assignments_are_local {
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
                text,
                child_scope_idx,
                child_assignment_scope_idx,
                builtins,
            );
        }
    }

    /// Walk the tree once, characterizing every method installation into
    /// `self.installations`. This is the only place the install-vs-call decision
    /// is made; capabilities read the result.
    fn collect_installations(&mut self, node: M2Node, text: &str, builtins: Option<&BuiltinData>) {
        if node.is_assignment() {
            if let Some(installation) = self.classify_installation(node, text, builtins) {
                self.installations.push(installation);
            }
        }

        for child in node.children() {
            self.collect_installations(child, text, builtins);
        }
    }

    /// The installation characterized for the assignment spanning `node`, if any.
    pub(crate) fn installation_for(&self, node: M2Node, text: &str) -> Option<&MethodInstallation> {
        let range = to_lsp_range(text, node.range());
        self.installations
            .iter()
            .find(|installation| installation.range == range)
    }

    /// The function being installed by a method-installation assignment
    /// `lhs := [Codomain =>] fn`, or `None` for an ordinary assignment/call. An
    /// explicit `Codomain => fn` return-type declaration is peeled to its `fn`, so
    /// the installation's value is the function, never the `Codomain =>` "Option".
    fn installed_function<'tree>(
        &self,
        node: M2Node<'tree>,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) -> Option<M2Node<'tree>> {
        if !node.is_assignment() {
            return None;
        }
        self.classify_installation(node, text, builtins)?;
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
    pub(crate) fn classify_installation(
        &self,
        node: M2Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) -> Option<MethodInstallation> {
        let operator = node.binary_operator()?;
        let left = node.child_by_field_name("left")?;
        let (head, domain) = self.installation_shape(left, builtins)?;
        let range = to_lsp_range(text, node.range());
        let target = SpanKey::from_node(text, left);
        let value = node
            .child_by_field_name("right")
            .map(|right| SpanKey::from_node(text, right));
        // The RHS function shape, read once here so the arity diagnostic need not
        // re-walk the tree. Only a plain lambda RHS carries a checkable arity.
        let rhs_dispatch = node
            .child_by_field_name("right")
            .filter(|right| right.kind == NodeKind::LambdaExpression)
            .and_then(function_dispatch);

        match operator {
            // `:=` installs by shape alone — no type check on the operands.
            ":=" => Some(MethodInstallation {
                head,
                domain,
                range,
                target,
                value,
                rhs_dispatch,
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
                        head: MethodHead::OperatorAssign(op),
                        domain,
                        range,
                        target,
                        value,
                        rhs_dispatch,
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
        builtins: Option<&BuiltinData>,
    ) -> Option<(MethodHead, Vec<TypeRef>)> {
        // A parenthesized expression is identified with its inner value, so
        // `(T op S) := f` installs exactly like `T op S := f`. The value is the
        // final expression; a trailing `;` makes the value null, which is not an
        // installation target.
        if node.kind == NodeKind::ParenthesizedExpression {
            if node.has_trailing_semicolon() {
                return None;
            }
            let inner = node.named_children().last()?;
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
                            vec![TypeRef::new(left_name), TypeRef::new(right_name)],
                        ))
                    } else {
                        Some((
                            MethodHead::Function(left_name.to_string()),
                            method_installation_domain(right)?
                                .into_iter()
                                .map(TypeRef::new)
                                .collect(),
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
                            TypeRef::new(symbol_node_text(left)?),
                            TypeRef::new(symbol_node_text(right)?),
                        ],
                    ))
                }
            }
            NodeKind::PrefixExpression => Some((
                MethodHead::Operator(Operator {
                    token: operator_text(node)?.to_string(),
                    fixity: Fixity::Prefix,
                }),
                vec![TypeRef::new(symbol_node_text(
                    node.child_by_field_name("operand")?,
                )?)],
            )),
            NodeKind::PostfixExpression => Some((
                MethodHead::Operator(Operator {
                    token: operator_text(node)?.to_string(),
                    fixity: Fixity::Postfix,
                }),
                vec![TypeRef::new(symbol_node_text(
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
    fn operand_is_type(&self, name: &str, builtins: Option<&BuiltinData>) -> bool {
        self.local_binding_is_type(name, builtins)
            || builtins
                .and_then(|builtins| builtins.get_record(&InstanceID::new(name)))
                .is_some_and(|record| record.type_info.is_some())
    }

    /// Whether any local binding named `name` is a type — its inferred static
    /// class is `Type` or a `Type` descendant.
    fn local_binding_is_type(&self, name: &str, builtins: Option<&BuiltinData>) -> bool {
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
                    .as_deref()
                    .is_some_and(|type_name| type_name_denotes_type(type_name, builtins))
            })
    }

    /// Emit a diagnostic for every characterized installation that M2 would
    /// reject or silently ignore. Runs after [`collect_installations`] so it only
    /// consumes the stored facts; the type universe (builtins ∪ local) is queried
    /// here because validity depends on the head's class and the operator corpus.
    fn collect_installation_diagnostics(&mut self, builtins: Option<&BuiltinData>) {
        // Validity hinges on the type universe: adjacency `A B := …` is a SPACE
        // operator install when `A` is a type but a function-head install
        // otherwise, and the two have different domains (hence different arities).
        // Without builtins we cannot tell them apart, so we stay silent (monotone).
        let Some(builtins) = builtins else {
            return;
        };
        let mut diagnostics = Vec::new();
        for installation in &self.installations {
            self.installation_diagnostics(installation, Some(builtins), &mut diagnostics);
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
        text: &str,
        builtins: Option<&BuiltinData>,
    ) {
        let mut diagnostics = Vec::new();
        self.scan_install_form(node, text, builtins, &mut diagnostics);
        self.diagnostics.extend(diagnostics);
    }

    fn scan_install_form(
        &self,
        node: M2Node,
        text: &str,
        builtins: Option<&BuiltinData>,
        out: &mut Vec<Diagnostic>,
    ) {
        if let Some(name) = self.illegal_equals_install_head(node, builtins) {
            out.push(M2Diagnostic::InstallNeedsColonEquals.at(
                to_lsp_range(text, node.range()),
                format!(
                    "Installing a method on `{name}` must use `:=`, not `=`: M2 rejects this \
                     (\"no method for storing values of function {name}\"). Use `:=`."
                ),
            ));
        }
        for child in node.children() {
            self.scan_install_form(child, text, builtins, out);
        }
    }

    /// The function name when `node` is `f Domain = fn` — an `=` assignment whose
    /// left side is a function-head install shape, whose right side is a lambda
    /// (install intent, not a value store), and whose head resolves to a function.
    /// `None` otherwise.
    fn illegal_equals_install_head(
        &self,
        node: M2Node,
        builtins: Option<&BuiltinData>,
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
        builtins: Option<&BuiltinData>,
        out: &mut Vec<Diagnostic>,
    ) {
        match &installation.head {
            MethodHead::Function(name) => {
                if self.head_function_kind(name, builtins) == HeadFunctionKind::NonMethodFunction {
                    out.push(M2Diagnostic::InstallNoEffect.at(
                        installation.range,
                        format!(
                            "Installing a method on `{name}` has no effect: `{name}` is not a \
                             method function. Define it with `{name} = method()` to make method \
                             installations take effect."
                        ),
                    ));
                }
            }
            MethodHead::Operator(operator) | MethodHead::OperatorAssign(operator) => {
                let form = fixity_form(operator.fixity);
                if self.operator_form_is_flexible(&operator.token, form, builtins) == Some(false) {
                    out.push(M2Diagnostic::OperatorNotFlexible.at(
                        installation.range,
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
        if let Some(Dispatch::Fixed(actual)) = installation.rhs_dispatch {
            let expected = installation.expected_rhs_arity();
            if actual != expected {
                out.push(M2Diagnostic::InstallArity.at(
                    installation.range,
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
        builtins: Option<&BuiltinData>,
    ) -> Option<bool> {
        let record = builtins?.get_record(&InstanceID::new(token))?;
        let operator_info = record.operator_info.as_ref()?;
        Some(operator_info.is_flexible(form))
    }

    /// Classify an installation's function head by method-function-ness, querying
    /// the layered type universe (local bindings first, then builtins).
    fn head_function_kind(&self, name: &str, builtins: Option<&BuiltinData>) -> HeadFunctionKind {
        // A local binding of `name` as a function shadows any builtin — its
        // inferred class (`"FunctionClosure"` for a lambda, `"MethodFunction"`
        // for `method()`) decides method-function-ness.
        if let Some(class) = self.local_function_class(name) {
            return head_function_kind_of_class(class);
        }
        // Otherwise a builtin record heads a function iff it carries
        // `function_info`; its `class` then decides method-function-ness.
        if let Some(record) =
            builtins.and_then(|builtins| builtins.get_record(&InstanceID::new(name)))
        {
            if record.function_info.is_some() {
                return head_function_kind_of_class(&record.class.0);
            }
        }
        // Not resolvable as a function — stay silent (monotone).
        HeadFunctionKind::Unknown
    }

    /// The inferred class of a locally-bound function named `name` (the most
    /// recent `FUNCTION` binding's `type_name`), or `None` when `name` is not
    /// bound as a local function.
    fn local_function_class(&self, name: &str) -> Option<&str> {
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
            .find(|binding| binding.kind == SymbolKind::FUNCTION)
            .and_then(|binding| binding.type_name.as_deref())
    }

    fn collect_parameters(
        &mut self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        parameter_types: Option<&[String]>,
    ) {
        let mut parameter_nodes = Vec::new();
        collect_parameter_nodes(node, &mut parameter_nodes);
        let typed_parameters = parameter_types.filter(|types| types.len() == parameter_nodes.len());
        for (idx, parameter_node) in parameter_nodes.into_iter().enumerate() {
            let name = parameter_node.text();
            let type_name = typed_parameters
                .and_then(|types| types.get(idx))
                .map(String::as_str);
            self.add_symbol(
                name,
                SymbolRegistration {
                    kind: SymbolKind::VARIABLE,
                    role: BindingRole::Parameter,
                    type_name,
                    node: parameter_node,
                    value_node: None,
                    scope_idx,
                },
                text,
            );
        }
    }

    fn collect_definitions(
        &mut self,
        node: M2Node,
        value_node: Option<M2Node>,
        text: &str,
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
                        text,
                    ),
                    DefinitionScope::Assign => {
                        let position = node_position(text, node);
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
                                text,
                            );
                        } else {
                            self.add_symbol(
                                name,
                                SymbolRegistration {
                                    node,
                                    value_node,
                                    ..registration
                                },
                                text,
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
                for child in node.named_children() {
                    self.collect_definitions(
                        child,
                        value_node,
                        text,
                        definition_scope,
                        SymbolRegistration {
                            type_name: None,
                            ..registration
                        },
                    );
                }
            }
            _ => {}
        }
    }

    fn add_symbol(&mut self, name: &str, registration: SymbolRegistration<'_>, text: &str) {
        let SymbolRegistration {
            kind,
            role,
            type_name,
            node,
            value_node,
            scope_idx,
        } = registration;
        let symbol_id = self.registry.intern_symbol(name);
        let binding_id = BindingId(self.registry.bindings.len() as u32);
        let state_id = BindingStateId(self.registry.binding_states.len() as u32);
        let range = to_lsp_range(text, node.range());
        let binding = BindingInfo {
            binding_id,
            symbol: symbol_id,
            role,
            declaration_kind: declaration_symbol_kind(kind, value_node),
            range,
            scope_idx,
            declaration_range: enclosing_definition_range(node, text),
            definition_state: state_id,
        };
        let state = BindingStateInfo {
            state_id,
            binding_id,
            kind,
            type_name: type_name.map(ToString::to_string),
            value_range: value_node.map(|value| to_lsp_range(text, value.range())),
            span: SpanKey::from_node(text, node),
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
        text: &str,
    ) {
        if self.binding(binding_id).is_none() {
            return;
        }
        let state_id = BindingStateId(self.registry.binding_states.len() as u32);
        self.registry.binding_states.push(BindingStateInfo {
            state_id,
            binding_id,
            kind: registration.kind,
            type_name: registration.type_name.map(ToString::to_string),
            value_range: registration
                .value_node
                .map(|value| to_lsp_range(text, value.range())),
            span: SpanKey::from_node(text, registration.node),
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
        text: &str,
    ) -> Option<(&'a FunctionInfo, &'a MethodInfo)> {
        let installation = method_installation_expression_for_callable_node(node)?;
        let (name, domain) = method_installation_signature(installation)?;
        let symbol = self.registry.resolve_symbol(&name)?;
        let method = self.registry.functions.get(&symbol)?;
        let installation_range = to_lsp_range(text, installation.range());
        let signature = method
            .methods
            .iter()
            .rev()
            .find(|signature| signature.domain == domain && signature.range == installation_range)
            .or_else(|| {
                method
                    .methods
                    .iter()
                    .rev()
                    .find(|signature| signature.domain == domain)
            })?;

        Some((method, signature))
    }

    pub fn infer_call_static_facts(
        &self,
        node: M2Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) -> CallStaticFacts {
        let scope_idx = self.find_scope_at(node_position(text, node)).unwrap_or(0);
        self.infer_call_facts(node, text, scope_idx, builtins)
    }

    pub fn infer_expression_static_type_name(
        &self,
        node: M2Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        let scope_idx = self.find_scope_at(node_position(text, node)).unwrap_or(0);
        self.type_of(node, text, scope_idx, builtins)
            .dispatch_name()
    }

    /// Record the [`Dispatch`] shape of a lambda-defined local function on its
    /// function record, creating the record if this is its first mention.
    fn record_local_function_dispatch(
        &mut self,
        name: &str,
        dispatch: Dispatch,
        node: M2Node,
        text: &str,
    ) {
        let range = to_lsp_range(text, node.range());
        let symbol = self.registry.intern_symbol(name);
        let function = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
                range,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
            });
        function.range = range;
        function.dispatch = Some(dispatch);
    }

    fn record_local_method_declaration(
        &mut self,
        name: &str,
        typical_value: Option<String>,
        node: M2Node,
        text: &str,
    ) {
        let range = to_lsp_range(text, node.range());
        let symbol = self.registry.intern_symbol(name);
        let method = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
                range,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
            });
        method.range = range;
        method.typical_value = typical_value;
    }

    fn collect_local_method_installation(
        &mut self,
        node: M2Node,
        right: Option<M2Node>,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) {
        let Some((name, domain)) = method_installation_signature(node) else {
            return;
        };
        // An install on a non-method-function compiles but has no effect, so it
        // creates no method record (the no-effect warning is emitted separately).
        if self.head_function_kind(&name, builtins) == HeadFunctionKind::NonMethodFunction {
            return;
        }
        let range = to_lsp_range(text, node.range());
        let symbol = self.registry.intern_symbol(&name);
        let method = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
                range,
                typical_value: None,
                methods: Vec::new(),
                dispatch: None,
            });
        let codomain = right
            .and_then(explicit_method_installation_codomain)
            .or_else(|| method.typical_value.clone());
        method.methods.push(MethodInfo {
            domain: domain.clone(),
            codomain: codomain.clone(),
            range,
        });
    }

    fn push_scope(&mut self, node: M2Node, text: &str, parent_idx: Option<usize>) -> usize {
        let range = to_lsp_range(text, node.range());
        let scope_idx = self.registry.scopes.len();
        self.registry.scopes.push(ScopeInfo { range, parent_idx });
        self.registry
            .node_scopes
            .insert(SpanKey::from_node(text, node), scope_idx);
        scope_idx
    }

    fn collect_expression_facts(
        &mut self,
        node: M2Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) {
        let position = node_position(text, node);
        let scope_idx = self.find_scope_at(position).unwrap_or(0);
        let key = SpanKey::from_node(text, node);
        self.registry.node_scopes.insert(key.clone(), scope_idx);

        // A method installation `lhs := [Codomain =>] fn` is not a value
        // assignment: the LHS is a method key and `Codomain =>` is a return-type
        // declaration, not an `Option`. Type the whole node as the installed
        // function and descend only into the function body, so the install syntax
        // (the LHS and the `Codomain =>` wrapper) gets no misleading value hints.
        if let Some(function) = self.installed_function(node, text, builtins) {
            if let Some(kind) = expression_kind(node) {
                let result_type = self.type_of(function, text, scope_idx, builtins);
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
            self.collect_expression_facts(function, text, builtins);
            return;
        }

        if let Some(kind) = expression_kind(node) {
            let result_type = self.type_of(node, text, scope_idx, builtins);
            let input_nodes = expression_inputs(node)
                .into_iter()
                .map(|child| SpanKey::from_node(text, child))
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

            if let Some(call_info) = self.call_info_for_expression(node, text, scope_idx, builtins)
            {
                self.registry.calls.insert(key.clone(), call_info);
            }
        }

        for child in node.children() {
            self.collect_expression_facts(child, text, builtins);
        }
    }

    fn call_info_for_expression(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
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
            let facts = self.infer_call_facts(argument, text, scope_idx, builtins);
            let candidate_methods = callable_name
                .as_deref()
                .and_then(|name| self.registry.resolve_symbol(name))
                .and_then(|symbol| self.registry.functions.get(&symbol))
                .map(|callable| {
                    callable
                        .methods
                        .iter()
                        .filter(|signature| {
                            signature_matches_domain(
                                &signature.domain,
                                &facts.argument_types,
                                builtins,
                            )
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            return Some(CallInfo {
                callable_name,
                argument_types: facts.argument_types,
                candidate_methods,
            });
        }

        let operator = expression_operator_text(node)?;
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");
        let operand = node.child_by_field_name("operand");
        let argument_types = if let Some(operand) = operand {
            vec![self.type_of(operand, text, scope_idx, builtins)]
        } else {
            vec![
                left.map_or_else(InferredType::unknown, |child| {
                    self.type_of(child, text, scope_idx, builtins)
                }),
                right.map_or_else(InferredType::unknown, |child| {
                    self.type_of(child, text, scope_idx, builtins)
                }),
            ]
        };

        Some(CallInfo {
            callable_name: Some(operator.to_string()),
            argument_types,
            candidate_methods: Vec::new(),
        })
    }

    /// The inferred type of the value `node` evaluates to — see [`InferredType`].
    /// Every value-producing node has a type; control-flow and unhandled forms
    /// fall to `Unknown`. The bound is a lower bound (a `typicalValue`), never
    /// asserted exact.
    fn type_of(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        if !self.cache_types {
            return self.compute_type_of(node, text, scope_idx, builtins);
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

        #[cfg(test)]
        self.type_computations.fetch_add(1, Ordering::Relaxed);
        let inferred = self.compute_type_of(node, text, scope_idx, builtins);
        self.type_cache
            .write()
            .expect("type cache lock should not be poisoned")
            .insert(node_id, inferred.clone());
        inferred
    }

    fn compute_type_of(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
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
            NodeKind::Sequence => self.sequence_type(node, text, scope_idx, builtins),
            // A parenthesized expression is its inner value: `(1)` is `ZZ`, `(a+b)`
            // is the type of `a+b`. A trailing-`;` paren evaluates to the `null`
            // value (class `Nothing`); we leave it unknown for now rather than
            // pin `Nothing`.
            NodeKind::ParenthesizedExpression => match parenthesized_value(node) {
                Some(inner) => self.type_of(inner, text, scope_idx, builtins),
                None => InferredType::unknown(),
            },
            NodeKind::StringLiteral => InferredType::of("String"),
            NodeKind::IntegerLiteral => InferredType::of("ZZ"),
            NodeKind::FloatLiteral => InferredType::of("RR"),
            // A quote expression (`symbol +`, `local x`, `global y`,
            // `threadLocal z`) evaluates to the Symbol it names.
            NodeKind::QuoteExpression => InferredType::of("Symbol"),
            NodeKind::Symbol => self.symbol_type(node, text, scope_idx, builtins),
            // An assignment evaluates to its right-hand side: `a = b` / `a := b`
            // (and destructuring `{x,y} := …`) take the type of the RHS.
            _ if node.is_assignment() => match node.child_by_field_name("right") {
                Some(right) => self.type_of(right, text, scope_idx, builtins),
                None => InferredType::unknown(),
            },
            // `x => y` builds an `Option` object, whatever the operand types.
            _ if node.is_option_assignment() => InferredType::of("Option"),
            NodeKind::BinaryExpression => {
                self.binary_expression_type(node, text, scope_idx, builtins)
            }
            NodeKind::PrefixExpression | NodeKind::PostfixExpression => {
                self.unary_operator_type(node, text, scope_idx, builtins)
            }
            NodeKind::NewStatement => node
                .child_by_field_name("type")
                .filter(|type_node| type_node.kind == NodeKind::Symbol)
                .map(|type_node| InferredType::of(type_node.text()))
                .unwrap_or_else(InferredType::unknown),
            // `if c then A [else B]` is whichever branch runs; with no `else`,
            // a false condition yields `null` (`Nothing`). The static type is the
            // join of the reachable branch types.
            NodeKind::IfStatement => self.if_statement_type(node, text, scope_idx, builtins),
            // `try E [then A] [else B | except e do B]` is the success value
            // (`then A`, else `E`) joined with the failure value (`else`/`do B`,
            // else `null` since an unhandled error makes `try` evaluate to null).
            NodeKind::TryStatement => self.try_statement_type(node, text, scope_idx, builtins),
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
            NodeKind::ReturnStatement | NodeKind::BreakStatement | NodeKind::ContinueStatement => {
                self.control_transfer_type(node, text, scope_idx, builtins)
            }
            // A debug clause (`time E`, `break v`, …) passes through to the value
            // of its inner statement/expression.
            NodeKind::DebugClause => node
                .named_children()
                .next()
                .map(|inner| self.type_of(inner, text, scope_idx, builtins))
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
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        let then_type = clause_of(node, NodeKind::ThenClause)
            .and_then(clause_value)
            .map(|value| self.type_of(value, text, scope_idx, builtins))
            .unwrap_or_else(InferredType::unknown);
        let else_type = match clause_of(node, NodeKind::ElseClause).and_then(clause_value) {
            Some(value) => self.type_of(value, text, scope_idx, builtins),
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
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        let body = node
            .named_children()
            .find(|child| !is_try_clause(child.kind));
        let success_value = clause_of(node, NodeKind::ThenClause)
            .and_then(clause_value)
            .or(body);
        let success = success_value
            .map(|value| self.type_of(value, text, scope_idx, builtins))
            .unwrap_or_else(InferredType::unknown);
        let failure_value = clause_of(node, NodeKind::ElseClause)
            .or_else(|| clause_of(node, NodeKind::DoClause))
            .and_then(clause_value);
        let failure = match failure_value {
            Some(value) => self.type_of(value, text, scope_idx, builtins),
            None => InferredType::of("Nothing"),
        };
        success.join(failure, builtins)
    }

    /// The type of a control transfer (`return e` / `break e` / `continue e`):
    /// its operand's type, or `Nothing` when the transfer is bare.
    fn control_transfer_type(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        match node.named_children().next() {
            Some(operand) => self.type_of(operand, text, scope_idx, builtins),
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
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        let name = node.text();
        if let Some(binding) =
            self.get_binding_from_scope(name, scope_idx, node_position(text, node))
        {
            if let Some(type_name) = &binding.state.type_name {
                return InferredType::of(type_name);
            }
        }

        if let Some(record) =
            builtins.and_then(|builtins| builtins.get_record(&InstanceID::new(name)))
        {
            return InferredType::from_id(record.class);
        }

        InferredType::of("Symbol")
    }

    /// A sequence's type: its single inner value for a one-element sequence
    /// (`(a)` collapses to `a`), else the `Sequence` class.
    fn sequence_type(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        let children = node.named_children().collect::<Vec<_>>();
        match children.as_slice() {
            [child] => self.type_of(*child, text, scope_idx, builtins),
            _ => InferredType::of("Sequence"),
        }
    }

    /// A binary expression's type. Juxtaposition `a SPACE b` is application
    /// (handled by [`Self::application_type`]); the function-dependent operators
    /// `_` (currying) and `@@` (composition) are computed here when their
    /// function-position operand is a `Function`; everything else dispatches
    /// through the M2 type table.
    fn binary_expression_type(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        if node.is_space_application() {
            return self.application_type(node, text, scope_idx, builtins);
        }

        let operator = node.binary_operator();
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");

        if let Some(operator) = operator {
            if let Some(result) =
                self.function_dependent_operator_type(operator, left, text, scope_idx, builtins)
            {
                return result;
            }
        }

        let (Some(operator), Some(left), Some(right), Some(builtins)) =
            (operator, left, right, builtins)
        else {
            return InferredType::unknown();
        };
        let left_type = self.type_of(left, text, scope_idx, Some(builtins));
        let right_type = self.type_of(right, text, scope_idx, Some(builtins));
        self.dispatch_codomain(builtins, operator, &[left_type, right_type], &[])
    }

    /// The function-dependent operators, whose result depends on the specific
    /// function value (M2 has no dependent types, so the M2 table cannot express
    /// them): currying `f _ x` (`f_x(y) := f(x, y)`) and composition `f @@ g`
    /// both yield a `FunctionClosure` when the function-position operand is a
    /// `Function`. `None` falls through to ordinary dispatch (so `M_i`, `L_i`,
    /// … keep their table behavior).
    fn function_dependent_operator_type(
        &self,
        operator: &str,
        left: Option<M2Node>,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> Option<InferredType> {
        if !matches!(operator, "_" | "@@") {
            return None;
        }
        let builtins = builtins?;
        let head = self.type_of(left?, text, scope_idx, Some(builtins));
        let head = head.principal()?;
        builtins
            .is_subtype(head, &InstanceID::new("Function"))
            .then(|| InferredType::of("FunctionClosure"))
    }

    /// Application `f SPACE x`. A `Function` head delegates to the head's own
    /// signatures (LSP-internal dependent info resolved against the corpus),
    /// stepping beyond the M2 table whose `(Function, Thing)` row only yields
    /// `Thing`. A non-`Function` head dispatches `SPACE` through the table
    /// (`Ring × Array → PolynomialRing`).
    fn application_type(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        let (Some(callable_node), Some(argument_node)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) else {
            return InferredType::unknown();
        };
        let call_facts = self.infer_call_facts(argument_node, text, scope_idx, builtins);
        let callable_name = symbol_node_text(callable_node);

        // A locally-defined function is known to be a function from the registry
        // alone, so its application resolves without the builtin lattice: its
        // signatures give the codomain, and an undocumented one yields `Thing`
        // (applying a function gives at least a Thing).
        if let Some(callable) = callable_name {
            if self.is_local_function(callable) {
                return self
                    .resolve_local_call_return_type(callable, &call_facts.argument_types, builtins)
                    .map_or_else(
                        || InferredType::of("Thing"),
                        |return_type| InferredType::of(&return_type),
                    );
            }
        }

        // Otherwise the lattice decides whether the head is a function (delegating
        // to its signatures) or another SPACE method (`Ring × Array →
        // PolynomialRing`).
        let Some(builtins) = builtins else {
            return InferredType::unknown();
        };
        let head = self.type_of(callable_node, text, scope_idx, Some(builtins));
        let head_is_function = head
            .principal()
            .is_some_and(|head| builtins.is_subtype(head, &InstanceID::new("Function")));
        if head_is_function {
            if let Some(callable) = callable_name {
                if let Some(return_type) = builtins.resolve_call_return_type_with_options(
                    callable,
                    &call_facts.dispatch_argument_types(),
                    &call_facts.literal_options,
                ) {
                    return InferredType::of(&return_type);
                }
            }
            // Applying a function yields at least a Thing.
            return InferredType::of("Thing");
        }

        let argument_type = self.type_of(argument_node, text, scope_idx, Some(builtins));
        self.dispatch_codomain(
            builtins,
            SPACE_OPERATOR,
            &[head, argument_type],
            &call_facts.literal_options,
        )
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
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> InferredType {
        let (Some(builtins), Some(operator), Some(operand)) = (
            builtins,
            operator_text(node),
            node.child_by_field_name("operand"),
        ) else {
            return InferredType::unknown();
        };
        let operand_type = self.type_of(operand, text, scope_idx, Some(builtins));
        self.dispatch_codomain(builtins, operator, &[operand_type], &[])
    }

    /// Dispatch `callable` on `args` through the M2 type table. A matched but
    /// undocumented codomain is `Thing` (≡ a null `typicalValue` under the
    /// lower-bound reading) — approximated by "the callable/operator is a known
    /// index entry, so it dispatches"; an unidentifiable head stays `Unknown`.
    fn dispatch_codomain(
        &self,
        builtins: &BuiltinData,
        callable: &str,
        args: &[InferredType],
        options: &[(String, String)],
    ) -> InferredType {
        if let Some(return_type) =
            builtins.resolve_call_return_type_with_options(callable, &dispatch_names(args), options)
        {
            return InferredType::of(&return_type);
        }
        if builtins.get_record(&InstanceID::new(callable)).is_some() {
            return InferredType::of("Thing");
        }
        InferredType::unknown()
    }

    fn infer_call_facts(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> CallStaticFacts {
        // A single parenthesized argument `f(x)` / `f(opt => v)` denotes its inner
        // value; peel it so the argument is classified like a bare argument.
        let node = parenthesized_value(node).unwrap_or(node);
        if node.kind == NodeKind::Sequence {
            let mut facts = CallStaticFacts::default();
            for child in node.named_children() {
                if let Some(option) = literal_option_assignment(child) {
                    facts.literal_options.push(option);
                } else {
                    facts
                        .argument_types
                        .push(self.type_of(child, text, scope_idx, builtins));
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
            argument_types: vec![self.type_of(node, text, scope_idx, builtins)],
            literal_options: Vec::new(),
        }
    }

    fn resolve_local_call_return_type(
        &self,
        callable: &str,
        argument_types: &[InferredType],
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        let symbol = self.registry.resolve_symbol(callable)?;
        let method = self.registry.functions.get(&symbol)?;
        let matching_codomains = method
            .methods
            .iter()
            .filter(|signature| signature_matches(signature, argument_types, builtins))
            .filter_map(|signature| {
                signature
                    .codomain
                    .clone()
                    .or_else(|| method.typical_value.clone())
            })
            .collect::<HashSet<_>>();

        if matching_codomains.len() == 1 {
            return matching_codomains.into_iter().next();
        }

        method.typical_value.clone()
    }

    pub(crate) fn binding_id_at(&self, name: &str, pos: Position) -> Option<BindingId> {
        let scope_idx = self.find_scope_at(pos)?;
        let symbol = self.registry.resolve_symbol(name)?;
        self.binding_id_from_scope(symbol, scope_idx, pos)
    }
}

#[derive(Debug, Clone, Copy)]
enum DefinitionScope {
    Local,
    Assign,
}

#[derive(Debug, Clone, Copy)]
struct SymbolRegistration<'a> {
    kind: SymbolKind,
    role: BindingRole,
    type_name: Option<&'a str>,
    node: M2Node<'a>,
    value_node: Option<M2Node<'a>>,
    scope_idx: usize,
}

fn expression_kind(node: M2Node<'_>) -> Option<ExpressionKind> {
    match node.kind {
        NodeKind::StringLiteral | NodeKind::IntegerLiteral | NodeKind::FloatLiteral => {
            Some(ExpressionKind::Literal)
        }
        NodeKind::Symbol => Some(ExpressionKind::Name),
        NodeKind::List
        | NodeKind::Array
        | NodeKind::AngleBarList
        | NodeKind::Sequence
        | NodeKind::Cell => Some(ExpressionKind::ScopeExpr),
        // A parenthesized expression is its inner value, so it takes the inner
        // value's kind (`(a+b)` is an `Expr`, `(x)` a `Name`); a null `(a;)` skips.
        NodeKind::ParenthesizedExpression => parenthesized_value(node).and_then(expression_kind),
        NodeKind::IfStatement
        | NodeKind::WhileStatement
        | NodeKind::ForStatement
        | NodeKind::NewStatement
        | NodeKind::TryStatement
        | NodeKind::ReturnStatement
        | NodeKind::BreakStatement
        | NodeKind::ContinueStatement
        | NodeKind::DebugClause => Some(ExpressionKind::ControlExpr),
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

pub(crate) fn symbol_node_text<'tree>(node: M2Node<'tree>) -> Option<&'tree str> {
    node.kind.is_symbol_like().then(|| node.text())
}

fn method_declaration_typical_value(node: M2Node) -> Option<Option<String>> {
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

fn find_option_value(node: M2Node, option_name: &str) -> Option<String> {
    if node.is_option_assignment() {
        let left = node.child_by_field_name("left")?;
        let right = node.child_by_field_name("right")?;
        if symbol_node_text(left) == Some(option_name) {
            return symbol_node_text(right).map(ToString::to_string);
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

fn enclosing_definition_range(node: M2Node<'_>, text: &str) -> Range {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind == NodeKind::Cell {
            return to_lsp_range(text, parent.range());
        }
        current = parent;
    }
    to_lsp_range(text, node.range())
}

fn literal_option_value(node: M2Node<'_>) -> Option<&str> {
    if node.kind.is_symbol_like() || node.kind.is_literal() {
        Some(node.text())
    } else {
        None
    }
}

fn explicit_method_installation_codomain(node: M2Node) -> Option<String> {
    if !node.is_option_assignment() {
        return None;
    }

    let codomain = node.child_by_field_name("left")?;
    symbol_node_text(codomain).map(ToString::to_string)
}

/// The operator token of a prefix/postfix expression, e.g. `-` in `-X` / `X-`.
fn operator_text(node: M2Node<'_>) -> Option<&str> {
    let operator = node.child_by_field_name("operator")?;
    Some(operator.text())
}

/// Whether `type_name` (an inferred static class or a referenced name) denotes a
/// TYPE, i.e. is `Type` itself or one of its descendants (`SelfInitializingType`,
/// …). Without the registry only the exact `Type` is recognized.
fn type_name_denotes_type(type_name: &str, builtins: Option<&BuiltinData>) -> bool {
    type_name == "Type"
        || builtins.is_some_and(|builtins| {
            builtins.is_subtype(&InstanceID::new(type_name), &InstanceID::new("Type"))
        })
}

pub(crate) fn method_installation_signature(node: M2Node) -> Option<(String, Vec<String>)> {
    if !node.is_space_application() {
        return None;
    }

    let callable = node.child_by_field_name("left")?;
    let arguments = node.child_by_field_name("right")?;
    let callable = symbol_node_text(callable)?;
    let domain = method_installation_domain(arguments)?;
    Some((callable.to_string(), domain))
}

fn method_installation_parameter_types_for_function(function_node: M2Node) -> Option<Vec<String>> {
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

fn method_installation_expression_for_callable_node<'tree>(
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
            return Some(current);
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

fn child_scope_assignment_is_local(parent: M2Node<'_>, child: M2Node<'_>) -> Option<bool> {
    match parent.kind {
        NodeKind::IfStatement => {
            let is_condition = parent
                .child_by_field_name("condition")
                .is_some_and(|condition| condition.id() == child.id());
            (is_condition || matches!(child.kind, NodeKind::ThenClause | NodeKind::ElseClause))
                .then_some(true)
        }
        NodeKind::TryStatement => {
            let is_body = parent
                .named_child(0)
                .is_some_and(|body| body.id() == child.id());
            (is_body || is_try_clause(child.kind)).then_some(true)
        }
        NodeKind::ForStatement => is_loop_clause(child.kind).then_some(false),
        NodeKind::WhileStatement => {
            let is_condition = parent
                .named_child(0)
                .is_some_and(|condition| condition.id() == child.id());
            (is_condition || is_loop_clause(child.kind)).then_some(false)
        }
        _ => None,
    }
}

/// The value a node denotes, peeling parenthesized grouping: `(a)` → `a`,
/// `((a))` → `a`. A trailing-`;` parenthesized expression (`(a;)`) denotes null,
/// so it has no value node — returns `None`. A non-parenthesized node is its own
/// value. `()` and `(a, b)` are `Sequence` nodes (real values), left untouched.
fn parenthesized_value(node: M2Node) -> Option<M2Node> {
    let mut current = node;
    while current.kind == NodeKind::ParenthesizedExpression {
        if current.has_trailing_semicolon() {
            return None;
        }
        let inner = current.named_children().last()?;
        current = inner;
    }
    Some(current)
}

pub(crate) fn method_installation_domain(node: M2Node) -> Option<Vec<String>> {
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
            .named_children()
            .filter(|child| child.kind != NodeKind::Comment)
            .map(|child| {
                symbol_node_text(child)
                    .unwrap_or_else(|| child.text())
                    .to_string()
            })
            .collect::<Vec<_>>();
        return (!domain.is_empty()).then_some(domain);
    }

    symbol_node_text(node).map(|name| vec![name.to_string()])
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

fn signature_matches(
    signature: &MethodInfo,
    argument_types: &[InferredType],
    builtins: Option<&BuiltinData>,
) -> bool {
    signature_matches_domain(&signature.domain, argument_types, builtins)
}

fn signature_matches_domain(
    expected_domain: &[String],
    argument_types: &[InferredType],
    builtins: Option<&BuiltinData>,
) -> bool {
    expected_domain.len() == argument_types.len()
        && expected_domain
            .iter()
            .zip(argument_types)
            .all(|(expected, actual)| {
                actual.principal().is_some_and(|actual| {
                    actual.0 == *expected
                        || builtins.is_some_and(|builtins| {
                            builtins.is_subtype(actual, &InstanceID::new(expected))
                        })
                })
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::diagnostics::{
        member_index_for_ambiguous_float_literal, AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE,
    };
    use crate::diagnostic_registry::diagnostic_has_kind;
    use tower_lsp::lsp_types::DiagnosticSeverity;
    use tree_sitter::Parser;

    fn analyze(text: &str) -> Analysis {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        Analysis::new(&tree, text)
    }

    fn analyze_with_builtins(text: &str, builtins: &BuiltinData) -> Analysis {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        Analysis::new_with_builtins(&tree, text, Some(builtins))
    }

    fn core_builtins() -> BuiltinData {
        BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"))
    }

    /// The inferred-type label of the first expression node of `kind` in `text`.
    fn type_label_of_kind(text: &str, builtins: &BuiltinData, kind: NodeKind) -> Option<String> {
        fn find<'tree>(node: M2Node<'tree>, kind: NodeKind) -> Option<M2Node<'tree>> {
            if node.kind == kind {
                return Some(node);
            }
            node.children().find_map(|child| find(child, kind))
        }
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(builtins));
        let node = find(M2Node::new(tree.root_node(), text), kind)?;
        analysis
            .expression_fact(text, node)
            .and_then(|fact| fact.result_type.label())
    }

    fn binary_op(token: &str) -> Operator {
        Operator {
            token: token.to_string(),
            fixity: Fixity::Binary,
        }
    }

    fn domain_names(installation: &MethodInstallation) -> Vec<&str> {
        installation.domain.iter().map(TypeRef::name).collect()
    }

    #[test]
    fn colon_equal_function_method_is_always_an_installation() {
        // `:=` installs by shape, with no type check on the domain.
        let analysis = analyze("f ZZ := x -> x\n");
        assert_eq!(analysis.installations.len(), 1);
        assert_eq!(
            analysis.installations[0].head,
            MethodHead::Function("f".to_string())
        );
        assert_eq!(domain_names(&analysis.installations[0]), vec!["ZZ"]);
    }

    #[test]
    fn colon_equal_binary_operator_is_always_an_installation() {
        // `:=` does not require the operands to be types.
        let analysis = analyze("R * S := (a, b) -> a\n");
        assert_eq!(analysis.installations.len(), 1);
        assert_eq!(
            analysis.installations[0].head,
            MethodHead::Operator(binary_op("*"))
        );
        assert_eq!(domain_names(&analysis.installations[0]), vec!["R", "S"]);
    }

    #[test]
    fn colon_equal_adjacency_on_types_is_a_space_operator_installation() {
        // `X Y := f` with both operands types is the SPACE operator on the pair.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins(
            "X = new Type of HashTable\nY = new Type of HashTable\nX Y := (a, b) -> a\n",
            &builtins,
        );
        assert_eq!(analysis.installations.len(), 1);
        assert_eq!(
            analysis.installations[0].head,
            MethodHead::Operator(binary_op(SPACE_OPERATOR))
        );
        assert_eq!(domain_names(&analysis.installations[0]), vec!["X", "Y"]);
    }

    #[test]
    fn equal_binary_operator_on_builtin_types_is_an_assignment_installation() {
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("ZZ + ZZ = (a, b, c) -> c\n", &builtins);
        assert_eq!(analysis.installations.len(), 1);
        assert_eq!(
            analysis.installations[0].head,
            MethodHead::OperatorAssign(binary_op("+"))
        );
        assert_eq!(domain_names(&analysis.installations[0]), vec!["ZZ", "ZZ"]);
        // RHS must take domain.len() + 1 args (the assigned value `z`).
        assert_eq!(analysis.installations[0].expected_rhs_arity(), 3);
    }

    #[test]
    fn equal_binary_operator_on_non_types_is_a_call_not_an_installation() {
        // `a` and `b` are not types, so `a + b = f` assigns to the lvalue
        // `a + b` — a call, not an installation.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("a + b = f\n", &builtins);
        assert!(analysis.installations.is_empty());
    }

    #[test]
    fn equal_binary_operator_on_local_types_is_an_assignment_installation() {
        // The killer case for the layered (local-first) type universe: X and Y
        // are user-defined types (`new Type …`), absent from builtins, yet the
        // local registry recognizes them so `X + Y = f` installs `((+, =), X, Y)`.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins(
            "X = new Type of HashTable\nY = new Type of HashTable\nX + Y = (a, b, c) -> c\n",
            &builtins,
        );
        assert_eq!(analysis.installations.len(), 1);
        assert_eq!(
            analysis.installations[0].head,
            MethodHead::OperatorAssign(binary_op("+"))
        );
        assert_eq!(domain_names(&analysis.installations[0]), vec!["X", "Y"]);
    }

    #[test]
    fn parenthesized_operator_left_is_still_an_installation() {
        // `(T op S) := f` is identified with `T op S := f`: a paren is its inner
        // value, so the binary-operator install is recognized through it.
        let analysis = analyze("(R * S) := (a, b) -> a\n");
        assert_eq!(analysis.installations.len(), 1);
        assert_eq!(
            analysis.installations[0].head,
            MethodHead::Operator(binary_op("*"))
        );
        assert_eq!(domain_names(&analysis.installations[0]), vec!["R", "S"]);
    }

    #[test]
    fn chained_colon_equal_is_not_an_installation() {
        // `:=` is right-associative, so `x := y := z` parses as `x := (y := z)`:
        // the left of the outer `:=` is the symbol `x`, never a binary expression.
        let analysis = analyze("x := y := z\n");
        assert!(analysis.installations.is_empty());
    }

    fn dispatch_of(src: &str) -> Option<Dispatch> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(src, None).expect("fixture should parse");
        fn first_lambda(node: M2Node) -> Option<M2Node> {
            if node.kind == NodeKind::LambdaExpression {
                return Some(node);
            }
            node.named_children().find_map(first_lambda)
        }
        function_dispatch(first_lambda(M2Node::new(tree.root_node(), src))?)
    }

    #[test]
    fn dispatch_reads_arity_from_the_lambda_parameter_node() {
        // Bare symbol binds the whole sequence — any arity.
        assert_eq!(dispatch_of("f := x -> x\n"), Some(Dispatch::Variadic));
        // Parenthesized forms are fixed-arity, including length 0 and 1.
        assert_eq!(dispatch_of("f := () -> 1\n"), Some(Dispatch::Fixed(0)));
        assert_eq!(dispatch_of("f := (x) -> x\n"), Some(Dispatch::Fixed(1)));
        assert_eq!(dispatch_of("f := (x, y) -> x\n"), Some(Dispatch::Fixed(2)));
    }

    #[test]
    fn lambda_function_records_its_dispatch_shape() {
        let analysis = analyze("f := (x, y) -> x + y\ng := z -> z\n");
        assert_eq!(
            analysis.function("f").and_then(|info| info.dispatch),
            Some(Dispatch::Fixed(2))
        );
        assert_eq!(
            analysis.function("g").and_then(|info| info.dispatch),
            Some(Dispatch::Variadic)
        );
    }

    #[test]
    fn method_function_has_no_lambda_dispatch() {
        // Method functions get their arity from installed method domains, not a
        // lambda parameter list, so `dispatch` stays None.
        let analysis = analyze("p = method()\n");
        assert_eq!(analysis.function("p").and_then(|info| info.dispatch), None);
    }

    #[test]
    fn dispatch_reads_arity_from_any_collection_parameter_form() {
        // M2 does not remember the collection type of a parameter list: `{x,y}`,
        // `[x,y]`, `<|x,y|>` and `(x,y)` all define the same 2-ary function.
        assert_eq!(dispatch_of("f := {x, y} -> x\n"), Some(Dispatch::Fixed(2)));
        assert_eq!(dispatch_of("f := [x, y] -> x\n"), Some(Dispatch::Fixed(2)));
        assert_eq!(
            dispatch_of("f := <|x, y|> -> x\n"),
            Some(Dispatch::Fixed(2))
        );
    }

    fn has_diagnostic(analysis: &Analysis, kind: M2Diagnostic) -> bool {
        analysis
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic_has_kind(diagnostic, kind))
    }

    #[test]
    fn install_on_non_flexible_binary_operator_is_an_error() {
        // `>` is flexible as a prefix but not as a binary operator, so a binary
        // method install on it is rejected by M2.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("ZZ > ZZ := (a, b) -> a\n", &builtins);
        assert!(has_diagnostic(&analysis, M2Diagnostic::OperatorNotFlexible));
    }

    #[test]
    fn install_on_flexible_binary_operator_has_no_flexibility_error() {
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("ZZ * ZZ := (a, b) -> a\n", &builtins);
        assert!(!has_diagnostic(
            &analysis,
            M2Diagnostic::OperatorNotFlexible
        ));
    }

    #[test]
    fn fixed_arity_rhs_disagreeing_with_domain_is_an_error() {
        // Binary domain expects 2 arguments; the fixed-arity RHS supplies 1.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("ZZ * ZZ := (a) -> a\n", &builtins);
        assert!(has_diagnostic(&analysis, M2Diagnostic::InstallArity));
    }

    #[test]
    fn variadic_rhs_never_triggers_an_arity_error() {
        // A bare-symbol parameter absorbs any arity, so it is always valid.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("ZZ * ZZ := a -> a\n", &builtins);
        assert!(!has_diagnostic(&analysis, M2Diagnostic::InstallArity));
    }

    #[test]
    fn assignment_form_install_arity_counts_the_assigned_value() {
        // `X op Y = z` installs `((op,=), X, Y)`; the RHS needs domain + 1 = 3 args.
        let builtins = core_builtins();
        let correct = analyze_with_builtins("ZZ * ZZ = (a, b, c) -> c\n", &builtins);
        assert!(!has_diagnostic(&correct, M2Diagnostic::InstallArity));
        let wrong = analyze_with_builtins("ZZ * ZZ = (a, b) -> a\n", &builtins);
        assert!(has_diagnostic(&wrong, M2Diagnostic::InstallArity));
    }

    #[test]
    fn install_on_lambda_bound_function_has_no_effect() {
        // `f` is a `FunctionClosure` (a lambda), not a method function, so
        // installing a method on it compiles but has no effect.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("f = x -> x\nf ZZ := y -> y\n", &builtins);
        assert!(has_diagnostic(&analysis, M2Diagnostic::InstallNoEffect));
    }

    #[test]
    fn install_on_method_function_has_no_no_effect_warning() {
        // `f = method()` is a method function, so installs take effect.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("f = method()\nf ZZ := y -> y\n", &builtins);
        assert!(!has_diagnostic(&analysis, M2Diagnostic::InstallNoEffect));
    }

    #[test]
    fn no_effect_install_records_no_method() {
        // The no-effect install must not create a method record on the lambda.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("f = x -> x\nf ZZ := y -> y\n", &builtins);
        let function = analysis.function("f").expect("f is a recorded function");
        assert!(function.methods.is_empty());
    }

    #[test]
    fn operator_installs_on_thing_are_recognized() {
        // The named binary operators `..` and `_` install methods on a type pair,
        // exactly like an explicit `*`/`+` operator install.
        let builtins = core_builtins();
        for (source, token) in [
            ("Thing .. Thing := (a, b) -> a\n", ".."),
            ("Thing _ Thing := (a, b) -> a\n", "_"),
        ] {
            let analysis = analyze_with_builtins(source, &builtins);
            assert_eq!(analysis.installations.len(), 1, "for `{source}`");
            assert_eq!(
                analysis.installations[0].head,
                MethodHead::Operator(binary_op(token)),
                "for `{source}`"
            );
            assert_eq!(
                domain_names(&analysis.installations[0]),
                vec!["Thing", "Thing"]
            );
        }
    }

    #[test]
    fn install_with_codomain_arrow_records_the_function() {
        // `f(CC,CC) := Array => fn` installs (f, CC, CC) -> Array; the `Array =>`
        // codomain wrapper must not be mistaken for an Option value.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins(
            "f = method()\nf(CC, CC) := Array => (i, j) -> i\n",
            &builtins,
        );
        let f = analysis.function("f").expect("f is a recorded function");
        let method = f
            .methods
            .iter()
            .find(|method| method.domain == vec!["CC".to_string(), "CC".to_string()])
            .expect("(f, CC, CC) recorded");
        assert_eq!(method.codomain.as_deref(), Some("Array"));
    }

    #[test]
    fn chained_colon_equals_installs_every_alias() {
        // `p(ZZ,ZZ) := p(List,ZZ) := fn` installs the same body for both
        // domains; right-associativity makes the inner install the RHS of the
        // outer, and both must be recorded as methods of `p`.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins(
            "p = method()\np(ZZ, ZZ) := p(List, ZZ) := (i, j) -> {i, j}\n",
            &builtins,
        );
        let p = analysis.function("p").expect("p is a recorded function");
        let domains: Vec<&Vec<String>> = p.methods.iter().map(|method| &method.domain).collect();
        assert!(
            domains.contains(&&vec!["ZZ".to_string(), "ZZ".to_string()]),
            "missing (p, ZZ, ZZ); got {domains:?}"
        );
        assert!(
            domains.contains(&&vec!["List".to_string(), "ZZ".to_string()]),
            "missing (p, List, ZZ); got {domains:?}"
        );
    }

    #[test]
    fn equals_install_on_method_function_head_is_flagged() {
        // `f Domain = fn` must be `:=`; M2 errors ("no method for storing
        // values of function f"). Verified against M2 1.26.05.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("f = method()\nf ZZ = x -> x\n", &builtins);
        assert!(has_diagnostic(
            &analysis,
            M2Diagnostic::InstallNeedsColonEquals
        ));
    }

    #[test]
    fn equals_install_on_lambda_head_is_also_flagged() {
        // M2 rejects `f Domain = fn` for a non-method function head too
        // ("no method for storing values"). Verified against M2 1.26.05.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("f = x -> x\nf ZZ = y -> y\n", &builtins);
        assert!(has_diagnostic(
            &analysis,
            M2Diagnostic::InstallNeedsColonEquals
        ));
    }

    #[test]
    fn colon_equals_install_is_not_flagged_as_wrong_form() {
        // The correct `:=` install form must not trip the wrong-form diagnostic.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("f = method()\nf ZZ := x -> x\n", &builtins);
        assert!(!has_diagnostic(
            &analysis,
            M2Diagnostic::InstallNeedsColonEquals
        ));
    }

    #[test]
    fn operator_assignment_install_is_not_flagged_as_wrong_form() {
        // `X op Y = fn` IS the legal assignment-install form for operators.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins("ZZ * ZZ = (a, b, c) -> c\n", &builtins);
        assert!(!has_diagnostic(
            &analysis,
            M2Diagnostic::InstallNeedsColonEquals
        ));
    }

    #[test]
    fn in_scope_symbols_lists_local_bindings_by_prefix() {
        let analysis = analyze("alpha = 1\nalef = 2\nbeta = 3\n");
        let mut names: Vec<String> = analysis
            .in_scope_symbols("al", Position::new(3, 0))
            .into_iter()
            .map(|(name, _kind)| name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["alef".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn in_scope_symbols_classifies_functions() {
        let analysis = analyze("g = x -> x\n");
        let symbols = analysis.in_scope_symbols("g", Position::new(1, 0));
        assert_eq!(symbols, vec![("g".to_string(), SymbolKind::FUNCTION)]);
    }

    #[test]
    fn if_with_both_branches_joins_branch_types() {
        // `if c then 1 else 2.0` is `ZZ` or `RR` — the join of both branches.
        let builtins = core_builtins();
        let label = type_label_of_kind("if x then 1 else 2.0\n", &builtins, NodeKind::IfStatement)
            .expect("if statement should have an inferred type");
        assert!(label.contains("ZZ") && label.contains("RR"), "got {label}");
    }

    #[test]
    fn if_without_else_admits_nothing() {
        // With no `else`, a false condition makes the whole `if` evaluate to null.
        let builtins = core_builtins();
        let label = type_label_of_kind("if x then 1\n", &builtins, NodeKind::IfStatement)
            .expect("if statement should have an inferred type");
        assert!(
            label.contains("ZZ") && label.contains("Nothing"),
            "got {label}"
        );
    }

    #[test]
    fn for_list_loop_is_a_list() {
        let builtins = core_builtins();
        let label = type_label_of_kind("for i to 3 list i\n", &builtins, NodeKind::ForStatement);
        assert_eq!(label.as_deref(), Some("List"));
    }

    #[test]
    fn for_do_loop_is_nothing() {
        let builtins = core_builtins();
        let label = type_label_of_kind("for i to 3 do i\n", &builtins, NodeKind::ForStatement);
        assert_eq!(label.as_deref(), Some("Nothing"));
    }

    #[test]
    fn while_loop_is_nothing() {
        let builtins = core_builtins();
        let label = type_label_of_kind("while x do 1\n", &builtins, NodeKind::WhileStatement);
        assert_eq!(label.as_deref(), Some("Nothing"));
    }

    #[test]
    fn try_joins_success_and_failure_paths() {
        // `try E then 1 else 2.0` is `ZZ` (success) or `RR` (failure).
        let builtins = core_builtins();
        let label =
            type_label_of_kind("try x then 1 else 2.0\n", &builtins, NodeKind::TryStatement)
                .expect("try statement should have an inferred type");
        assert!(label.contains("ZZ") && label.contains("RR"), "got {label}");
    }

    #[test]
    fn bare_try_admits_nothing_on_failure() {
        // `try 1` is `ZZ` on success or null on an unhandled error.
        let builtins = core_builtins();
        let label = type_label_of_kind("try 1\n", &builtins, NodeKind::TryStatement)
            .expect("try statement should have an inferred type");
        assert!(
            label.contains("ZZ") && label.contains("Nothing"),
            "got {label}"
        );
    }

    #[test]
    fn break_with_value_takes_the_operand_type() {
        let builtins = core_builtins();
        let label = type_label_of_kind("break 7\n", &builtins, NodeKind::BreakStatement);
        assert_eq!(label.as_deref(), Some("ZZ"));
    }

    #[test]
    fn equal_assignment_in_a_function_stays_function_local() {
        let analysis = analyze(concat!(
            "outer := 0\n",
            "f := () -> (\n",
            "  outer = 1;\n",
            "  fresh = 2;\n",
            "  (outer, fresh)\n",
            ")\n",
            "outer\n",
            "fresh\n",
        ));
        let definition_line = |name: &str, position: Position| {
            analysis
                .get_symbol_at(name, position)
                .map(|symbol| symbol.range.start.line)
        };

        assert_eq!(definition_line("outer", Position::new(4, 3)), Some(2));
        assert_eq!(definition_line("fresh", Position::new(4, 10)), Some(3));
        assert_eq!(definition_line("outer", Position::new(6, 0)), Some(0));
        assert_eq!(definition_line("fresh", Position::new(7, 0)), None);
    }

    #[test]
    fn collection_constructors_do_not_create_scopes() {
        let analysis = analyze(
            "[collectionLocal := 1; collectionEqual = 2;]\n\
             collectionLocal\n\
             collectionEqual\n",
        );

        assert!(analysis
            .get_symbol_at("collectionLocal", Position::new(1, 0))
            .is_some());
        assert!(analysis
            .get_symbol_at("collectionEqual", Position::new(2, 0))
            .is_some());
        assert_eq!(analysis.registry().scopes.len(), 1);
    }

    #[test]
    fn if_and_try_regions_do_not_export_bindings() {
        let analysis = analyze(concat!(
            "if (conditionEqual = 1; conditionEqual) then ",
            "(thenLocal := 2; thenEqual = 3;) else ",
            "(elseLocal := 4; elseEqual = 5;)\n",
            "try (bodyLocal := 6; bodyEqual = 7;) then ",
            "(tryThenLocal := 8; tryThenEqual = 9;) else ",
            "(tryElseLocal := 10; tryElseEqual = 11;)\n",
            "conditionEqual\n",
            "thenLocal\n",
            "thenEqual\n",
            "elseLocal\n",
            "elseEqual\n",
            "bodyLocal\n",
            "bodyEqual\n",
            "tryThenLocal\n",
            "tryThenEqual\n",
            "tryElseLocal\n",
            "tryElseEqual\n",
        ));

        for (line, name) in [
            (2, "conditionEqual"),
            (3, "thenLocal"),
            (4, "thenEqual"),
            (5, "elseLocal"),
            (6, "elseEqual"),
            (7, "bodyLocal"),
            (8, "bodyEqual"),
            (9, "tryThenLocal"),
            (10, "tryThenEqual"),
            (11, "tryElseLocal"),
            (12, "tryElseEqual"),
        ] {
            assert!(
                analysis
                    .get_symbol_at(name, Position::new(line, 0))
                    .is_none(),
                "{name} escaped its control-flow region"
            );
        }
    }

    #[test]
    fn loop_clauses_keep_local_assignments_and_export_context_assignments() {
        let analysis = analyze(concat!(
            "for i to 2 list (\n",
            "  a := 1;\n",
            "  b = 2;\n",
            ") do (\n",
            "  k = a;\n",
            "  l := b;\n",
            "  m := 1;\n",
            "  n = 2;\n",
            ")\n",
            "a\n",
            "b\n",
            "k\n",
            "l\n",
            "m\n",
            "n\n",
        ));

        for (line, name, is_bound) in [
            (9, "a", false),
            (10, "b", true),
            (11, "k", true),
            (12, "l", false),
            (13, "m", false),
            (14, "n", true),
        ] {
            assert_eq!(
                analysis
                    .get_symbol_at(name, Position::new(line, 0))
                    .is_some(),
                is_bound,
                "unexpected post-loop binding for {name}"
            );
        }
        assert!(analysis.get_symbol_at("a", Position::new(4, 6)).is_none());
        assert!(analysis.get_symbol_at("b", Position::new(5, 7)).is_some());
    }

    #[test]
    fn a_loop_inside_a_function_exports_only_to_the_function() {
        let analysis = analyze(concat!(
            "f := () -> (\n",
            "  for i to 0 do (nestedEqual = 1;);\n",
            "  nestedEqual\n",
            ")\n",
            "nestedEqual\n",
        ));

        assert!(analysis
            .get_symbol_at("nestedEqual", Position::new(2, 2))
            .is_some());
        assert!(analysis
            .get_symbol_at("nestedEqual", Position::new(4, 0))
            .is_none());
    }

    #[test]
    fn while_condition_and_body_have_separate_local_scopes() {
        let analysis = analyze(concat!(
            "while (conditionLocal := true; conditionLocal) do (\n",
            "  bodyLocal := 1;\n",
            "  bodyEqual = 2;\n",
            ")\n",
            "conditionLocal\n",
            "bodyLocal\n",
            "bodyEqual\n",
        ));

        assert!(analysis
            .get_symbol_at("conditionLocal", Position::new(4, 0))
            .is_none());
        assert!(analysis
            .get_symbol_at("bodyLocal", Position::new(5, 0))
            .is_none());
        assert!(analysis
            .get_symbol_at("bodyEqual", Position::new(6, 0))
            .is_some());
    }

    #[test]
    fn scope_resolution_keeps_assignments_inside_their_function() {
        let analysis = analyze(concat!(
            "x = 1\n",
            "f := () -> (\n",
            "  y := 3;\n",
            "  g := () -> (x := 5; y = 6; x);\n",
            "  (x, y, g())\n",
            ")\n",
            "w := () -> (q = 8; q)\n",
            "y\n",
            "q\n",
        ));
        let def_line = |name: &str, pos: Position| {
            analysis
                .get_symbol_at(name, pos)
                .map(|symbol| symbol.range.start.line)
        };

        // g's trailing `x` resolves to its own local `x := 5` (line 3), shadowing global.
        assert_eq!(def_line("x", Position::new(3, 29)), Some(3));
        // `y = 6` is known inside g, but cannot modify f's binding statically.
        assert_eq!(def_line("y", Position::new(3, 22)), Some(3));
        // The tuple's `x` skips past (no f-local x) to the global `x` (line 0).
        assert_eq!(def_line("x", Position::new(4, 3)), Some(0));
        // f retains its own `y`; g's assignment does not escape g.
        assert_eq!(def_line("y", Position::new(4, 6)), Some(2));
        // `q = 8` is visible later in w.
        assert_eq!(def_line("q", Position::new(6, 19)), Some(6));
        // Neither function contributes bindings to the file scope.
        assert_eq!(def_line("y", Position::new(7, 0)), None);
        assert_eq!(def_line("q", Position::new(8, 0)), None);
    }

    #[test]
    fn classifies_user_defined_functions_and_parameters() {
        let analysis = analyze("f := x -> x\nf 1");

        assert_eq!(
            analysis
                .get_symbol_at("f", Position::new(1, 0))
                .map(|symbol| symbol.state.kind),
            Some(SymbolKind::FUNCTION)
        );
        assert_eq!(
            analysis
                .get_symbol_at("x", Position::new(0, 10))
                .map(|symbol| symbol.role),
            Some(BindingRole::Parameter)
        );
    }

    #[test]
    fn resolves_latest_binding_before_query_position() {
        let analysis = analyze("x := 1\ny := x\nx := 2\nx\n");

        let middle_use = analysis
            .get_symbol_at("x", Position::new(1, 5))
            .expect("middle x should resolve to the first binding");
        assert_eq!(middle_use.range.start, Position::new(0, 0));

        let later_use = analysis
            .get_symbol_at("x", Position::new(3, 0))
            .expect("later x should resolve to the second binding");
        assert_eq!(later_use.range.start, Position::new(2, 0));
    }

    #[test]
    fn reassignment_changes_later_type_without_changing_the_definition() {
        let analysis = analyze("x := 1\nbefore := x\nx = y -> y\nafter := x\n");

        let before = analysis
            .get_binding_at("x", Position::new(1, 10))
            .expect("the earlier reference should resolve");
        assert_eq!(before.range.start, Position::new(0, 0));
        assert_eq!(before.state.type_name.as_deref(), Some("ZZ"));

        let after = analysis
            .get_binding_at("x", Position::new(3, 9))
            .expect("the later reference should resolve");
        assert_eq!(after.range.start, Position::new(0, 0));
        assert_eq!(after.state.type_name.as_deref(), Some("FunctionClosure"));
        assert_eq!(before.binding_id, after.binding_id);
        assert_ne!(before.state.state_id, after.state.state_id);
    }

    #[test]
    fn binding_states_respect_source_order_and_lexical_shadowing() {
        let analysis = analyze(concat!(
            "x := 1\n",
            "f := () -> (\n",
            "  before := x;\n",
            "  x := \"local\";\n",
            "  afterShadow := x;\n",
            "  x = y -> y;\n",
            "  afterWrite := x;\n",
            ")\n",
            "outside := x\n",
        ));

        let before = analysis
            .get_binding_at("x", Position::new(2, 12))
            .expect("the use before the local definition should see the outer binding");
        assert_eq!(before.range.start, Position::new(0, 0));
        assert_eq!(before.state.type_name.as_deref(), Some("ZZ"));

        let shadowed = analysis
            .get_binding_at("x", Position::new(4, 17))
            .expect("the use after the local definition should see the local binding");
        assert_eq!(shadowed.range.start, Position::new(3, 2));
        assert_eq!(shadowed.state.type_name.as_deref(), Some("String"));

        let rewritten = analysis
            .get_binding_at("x", Position::new(6, 16))
            .expect("the use after the local write should see its new state");
        assert_eq!(rewritten.range.start, Position::new(3, 2));
        assert_eq!(
            rewritten.state.type_name.as_deref(),
            Some("FunctionClosure")
        );
        assert_eq!(shadowed.binding_id, rewritten.binding_id);
        assert_ne!(shadowed.state.state_id, rewritten.state.state_id);

        let outside = analysis
            .get_binding_at("x", Position::new(8, 11))
            .expect("the nested shadow must not change the outer binding");
        assert_eq!(outside.range.start, Position::new(0, 0));
        assert_eq!(outside.state.type_name.as_deref(), Some("ZZ"));
    }

    #[test]
    fn analysis_ranges_use_lsp_utf16_columns() {
        let analysis = analyze("\"😀\"; x := 1\nx\n");

        let symbol = analysis
            .get_symbol_at("x", Position::new(1, 0))
            .expect("x should resolve despite non-ascii text before its definition");

        assert_eq!(
            symbol.range,
            Range::new(Position::new(0, 6), Position::new(0, 7))
        );
    }

    #[test]
    fn infers_static_types_from_new_constructors() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        let analysis = analyze_with_builtins(
            "clearAll = new Command from { () -> () }\nclearAll\n",
            &builtins,
        );
        assert_eq!(
            analysis
                .get_symbol_at("clearAll", Position::new(1, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Command")
        );
    }

    #[test]
    fn infers_static_types_from_documented_call_signatures() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        let analysis = analyze_with_builtins(
            "I := new Ideal from {}\nR := ring I\nS := ring x\nR\nS\n",
            &builtins,
        );

        assert_eq!(
            analysis
                .get_symbol_at("R", Position::new(3, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Ring")
        );
        assert_eq!(
            analysis
                .get_symbol_at("S", Position::new(4, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn specialized_documented_signatures_override_general_signatures() {
        let builtins = BuiltinData::load_from_index(
            "{\"kind\":\"methodFunction\",\"name\":\"f\",\"methods\":[{\"domain\":[\"Ideal\"],\"typicalValue\":\"Ring\"}]}\n",
        );
        let analysis = analyze_with_builtins("I := new Ideal from {}\nR := f I\nR\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("R", Position::new(2, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn infers_static_types_from_documented_operator_signatures() {
        let builtins = BuiltinData::load_from_index(
            "{\"kind\":\"operator\",\"name\":\"+\",\"operator\":{\"forms\":[\"binary\"]},\"methods\":[{\"domain\":[\"ZZ\",\"ZZ\"],\"typicalValue\":\"ZZ\"}]}\n",
        );
        let analysis = analyze_with_builtins("x := 1\ny := 2\nz := x + y\nz\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("z", Position::new(3, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("ZZ")
        );
    }

    #[test]
    fn records_local_method_declarations_and_installed_signatures() {
        let analysis = analyze(
            "p = method(Binary => true, TypicalValue => List)\np(ZZ,ZZ) := p(List,ZZ) := (i,j) -> {i,j}\n",
        );
        let method = analysis
            .function("p")
            .expect("method declaration should create local method metadata");

        assert_eq!(method.typical_value.as_deref(), Some("List"));
        assert_eq!(
            method
                .methods
                .iter()
                .map(|signature| signature.domain.clone())
                .collect::<Vec<_>>(),
            vec![
                vec!["ZZ".to_string(), "ZZ".to_string()],
                vec!["List".to_string(), "ZZ".to_string()]
            ]
        );
        assert!(method
            .methods
            .iter()
            .all(|signature| signature.codomain.as_deref() == Some("List")));
        assert_eq!(
            analysis
                .get_symbol_at("p", Position::new(1, 0))
                .map(|symbol| symbol.state.kind),
            Some(SymbolKind::FUNCTION)
        );
    }

    #[test]
    fn method_installation_syntax_produces_no_option_or_lhs_value_type() {
        // `f ZZ := String => x -> x` installs (f, ZZ) -> String. The `String =>`
        // is return-type syntax (not an Option) and the `f ZZ` LHS is a method key
        // (not a call to f's typicalValue); the installation evaluates to the
        // installed function, so neither piece of syntax yields a value type.
        let analysis = analyze("f ZZ := String => x -> x\n");

        assert!(
            analysis
                .registry()
                .expressions
                .values()
                .all(|fact| fact.result_type != InferredType::of("Option")),
            "installation `=>` is syntax and must not produce an Option type"
        );
        assert!(
            analysis.registry().expressions.values().any(|fact| {
                matches!(fact.kind, ExpressionKind::Assign)
                    && fact.result_type == InferredType::of("FunctionClosure")
            }),
            "the installation should type as the installed function"
        );
    }

    #[test]
    fn infers_static_types_from_local_method_typical_values() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        let analysis = analyze_with_builtins(
            "p = method(Binary => true, TypicalValue => List)\np(ZZ,ZZ) := p(List,ZZ) := (i,j) -> {i,j}\nx := p(1, 2)\nx\n",
            &builtins,
        );

        assert_eq!(
            analysis
                .get_symbol_at("x", Position::new(3, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("List")
        );
    }

    #[test]
    fn method_installation_preserves_arity_for_non_symbol_domain_positions() {
        // `a.b` evaluates to a type at install time, so `f(ZZ, a.b) := …`
        // installs at arity 2. The member-access position is kept even though its
        // type name is not statically resolvable — dropping it would corrupt the
        // recorded arity (and with it every downstream check).
        let analysis = analyze("f = method()\nf(ZZ, a.b) := (x, y) -> x\n");
        let method = analysis.function("f").expect("method should be tracked");

        assert_eq!(method.methods[0].domain.len(), 2);
        assert_eq!(method.methods[0].domain[0], "ZZ");
    }

    #[test]
    fn method_installation_domains_type_function_parameters() {
        let analysis = analyze("f ZZ := d -> (\n  a := d\n)\n");

        assert_eq!(
            analysis
                .get_symbol_at("d", Position::new(1, 7))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("ZZ")
        );
    }

    #[test]
    fn method_installation_domains_do_not_type_nested_function_parameters() {
        let analysis = analyze("f(ZZ) := x -> (\n  h := y -> y\n  h x\n)\n");

        assert_eq!(
            analysis
                .get_symbol_at("x", Position::new(2, 4))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("ZZ")
        );
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(1, 12))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            None
        );
    }

    #[test]
    fn local_methods_without_codomains_yield_thing() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        let analysis =
            analyze_with_builtins("f = method()\nf ZZ := x -> -x\ny := f 1\ny\n", &builtins);

        let method = analysis
            .function("f")
            .expect("method declaration should be tracked");
        assert_eq!(method.typical_value, None);
        assert_eq!(method.methods[0].domain, vec!["ZZ"]);
        // A method whose codomain is unrecorded returns `Thing` (≡ a null
        // `typicalValue` under the lower-bound reading) — applying a function
        // yields at least a `Thing`, not "unknown".
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(3, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Thing")
        );
    }

    #[test]
    fn unresolved_symbols_infer_as_their_own_symbol() {
        // An unbound name evaluates to its own `Symbol` in M2 — a known fact, not
        // "unknown". With no binding and no index entry, `foo` is a `Symbol`, so
        // the binding it is assigned to is typed `Symbol`.
        let analysis = analyze("y := foo\ny\n");
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(1, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Symbol")
        );
    }

    #[test]
    fn explicit_local_method_codomains_override_typical_values() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        let analysis = analyze_with_builtins(
            "f = method(TypicalValue => List)\nf ZZ := Ring => x -> x\ny := f 1\ny\n",
            &builtins,
        );

        let method = analysis
            .function("f")
            .expect("local method should be tracked");
        assert_eq!(method.typical_value.as_deref(), Some("List"));
        assert_eq!(method.methods[0].codomain.as_deref(), Some("Ring"));
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(3, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn call_options_do_not_count_as_positional_arguments() {
        let builtins = BuiltinData::load_from_index(
            "{\"kind\":\"methodFunction\",\"name\":\"f\",\"methods\":[{\"domain\":[\"ZZ\"],\"typicalValue\":\"String\"}]}\n",
        );
        let analysis = analyze_with_builtins("y := f(1, Mode => AsList)\ny\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(1, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("String")
        );
    }

    #[test]
    fn try_then_except_expression_does_not_produce_syntax_diagnostics() {
        let analysis = analyze("apply(-3..3, i -> try 1/i then 1 / i except err do err)");

        assert!(
            analysis.diagnostics.is_empty(),
            "current grammar should accept try/then/except expressions without syntax diagnostics"
        );
    }

    #[test]
    fn infers_static_types_from_space_adjacency_facts() {
        let corpus = concat!(
            "{\"kind\":\"type\",\"name\":\"QQ\",\"class\":\"Ring\"}\n",
            "{\"kind\":\"operator\",\"name\":\"SPACE\",\"operator\":{\"forms\":[\"binary\"]},\"methods\":[{\"domain\":[\"Ring\",\"Array\"],\"typicalValue\":\"Ring\"}]}\n",
        );
        let builtins = BuiltinData::load_from_index(corpus);
        let analysis = analyze_with_builtins("R := QQ\nS := R[x,y]\nS\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("S", Position::new(2, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn infers_static_types_from_container_literals() {
        let analysis = analyze(
            "l := {1,2}\na := [1,2]\nb := <|1,2|>\ne := ()\nf := (1)\ng := (1,2)\nl\na\nb\ne\nf\ng\n",
        );

        assert_eq!(
            analysis
                .get_symbol_at("l", Position::new(6, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("List")
        );
        assert_eq!(
            analysis
                .get_symbol_at("a", Position::new(7, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Array")
        );
        assert_eq!(
            analysis
                .get_symbol_at("b", Position::new(8, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("AngleBarList")
        );
        assert_eq!(
            analysis
                .get_symbol_at("e", Position::new(9, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Sequence")
        );
        assert_eq!(
            analysis
                .get_symbol_at("f", Position::new(10, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("ZZ")
        );
        assert_eq!(
            analysis
                .get_symbol_at("g", Position::new(11, 0))
                .and_then(|symbol| symbol.state.type_name.as_deref()),
            Some("Sequence")
        );
    }

    #[test]
    fn registry_tracks_bindings_and_local_callables() {
        let analysis =
            analyze("f = method(TypicalValue => List)\nf ZZ := Ring => x -> x\ny := f 1\ny\n");

        let binding = analysis
            .get_binding_at("y", Position::new(3, 0))
            .expect("binding should resolve through registry");
        assert_eq!(binding.scope_idx, 0);
        assert_eq!(binding.state.type_name.as_deref(), Some("Ring"));

        let callable = analysis
            .function("f")
            .expect("callable should be registered");
        assert_eq!(callable.typical_value.as_deref(), Some("List"));
        assert_eq!(callable.methods.len(), 1);
        assert_eq!(callable.methods[0].domain, vec!["ZZ"]);
        assert_eq!(callable.methods[0].codomain.as_deref(), Some("Ring"));
    }

    #[test]
    fn registry_tracks_expression_and_call_facts() {
        let builtins = BuiltinData::load_from_index(
            "{\"kind\":\"operator\",\"name\":\"+\",\"operator\":{\"forms\":[\"binary\"]},\"methods\":[{\"domain\":[\"ZZ\",\"ZZ\"],\"typicalValue\":\"ZZ\"}]}\n",
        );
        let text = "x := 1\ny := 2\nz := x + y\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(&builtins));
        let assignment = tree
            .root_node()
            .descendant_for_byte_range(18, 23)
            .expect("assignment should exist");
        let binary = M2Node::new(
            assignment
                .child_by_field_name("right")
                .expect("assignment should have right-hand expression"),
            text,
        );
        let fact = analysis
            .expression_fact(text, binary)
            .expect("expression fact should be registered");
        assert_eq!(fact.kind, ExpressionKind::Expr);
        assert_eq!(fact.result_type, InferredType::of("ZZ"));
        let call = analysis
            .registry()
            .calls
            .get(&SpanKey::from_node(text, binary))
            .expect("call info should be registered");
        assert_eq!(call.callable_name.as_deref(), Some("+"));

        let computations = analysis.type_computation_count();
        assert_eq!(computations, analysis.cached_type_count());
        assert_eq!(
            analysis.infer_expression_static_type_name(binary, text, Some(&builtins)),
            Some("ZZ".to_string())
        );
        assert_eq!(
            analysis.type_computation_count(),
            computations,
            "a semantic consumer should reuse the node's final inferred type"
        );
    }

    #[test]
    fn registers_destructured_symbols_from_all_collection_targets() {
        let analysis = analyze(
            "[x, y] = {1, 2}\n<|a, b, c|> = [3, 4, 5]\n(p, q) := (6, 7)\nx\ny\na\nb\nc\np\nq\n",
        );

        for (name, line) in [("x", 4), ("y", 5), ("a", 6), ("b", 7), ("c", 8)] {
            assert!(
                analysis
                    .get_symbol_at(name, Position::new(line, 0))
                    .is_some(),
                "{name} from a bracket/angle-bar destructuring target should be registered"
            );
        }
        assert!(analysis.get_symbol_at("p", Position::new(9, 0)).is_some());
        assert!(analysis.get_symbol_at("q", Position::new(10, 0)).is_some());
    }

    #[test]
    fn registers_nested_destructured_symbols() {
        let analysis = analyze("[x, [y, z]] := {1, {2, 3}}\nx\ny\nz\n");

        for (name, line) in [("x", 1), ("y", 2), ("z", 3)] {
            assert!(
                analysis
                    .get_symbol_at(name, Position::new(line, 0))
                    .is_some(),
                "{name} from a nested destructuring target should be registered"
            );
        }
    }

    #[test]
    fn parallel_assignment_expression_has_right_hand_side_type() {
        let text = "{a, b} = [1, 2]\n";
        let analysis = analyze(text);
        let tree = {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_macaulay2::language())
                .expect("macaulay2 parser should load");
            parser.parse(text, None).expect("parse")
        };
        let assignment = M2Node::new(
            tree.root_node()
                .descendant_for_byte_range(0, 15)
                .expect("assignment node"),
            text,
        );
        // The assignment evaluates to its right-hand side, so its expression
        // type is Array even though the target is written with `{}`.
        assert_eq!(
            analysis.infer_expression_static_type_name(assignment, text, None),
            Some("Array".to_string())
        );
    }

    #[test]
    fn diagnoses_parallel_assignment_arity_mismatch() {
        // Flagged: `[a,b,c]` (3) and `{r}` (1) mismatch the 2 targets; the empty
        // sequence `()` (0) mismatches; the nested `{2,3,4}` (3) mismatches its 2
        // targets `[y,z]`. Not flagged: `(s)` is a length-1 sequence i.e. a
        // parenthesized expression (runtime-checked), `[g,h]` matches, `(a,b)`
        // is a real 2-tuple matching its 2 targets.
        let analysis = analyze(
            "[x, y] = [a, b, c]\n[p, q] = {r}\n[m, n] = (s)\n[u, v] = [g, h]\n[i, [j, k]] = [1, {2, 3, 4}]\n[c, d] = ()\n[e, f] = (a, b)\n",
        );

        let arity_errors = analysis
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .filter(|diagnostic| diagnostic.message.contains("parallel assignment binds"))
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();

        assert_eq!(arity_errors, vec![0, 1, 4, 5]);
    }

    #[test]
    fn diagnoses_structurally_invalid_assignment_forms() {
        let analysis = analyze(
            "x#i := e\n(x+1,y) = (1,2)\n(x+1,y) := (1,2)\n(f()) <- (1)\nsource(String,Number) := peek\np(ZZ, ZZ) := (i, j) -> {i, j}\n",
        );

        assert_eq!(
            analysis
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "`:=` cannot assign to parts; use `=` for part assignment",
                "= multiple assignment targets must be symbols",
                ":= multiple assignment targets must be symbols",
            ]
        );
    }

    #[test]
    fn global_scope_orphan_else_is_a_syntax_error() {
        // A line-broken `if … then …` at global scope completes at the newline,
        // so the trailing `else` is orphaned — M2 rejects it. The parser now
        // reports this directly as a syntax error (no bespoke diagnostic).
        let analysis = analyze("if x then y\n    else z");
        assert!(has_diagnostic(&analysis, M2Diagnostic::SyntaxError));
    }

    #[test]
    fn does_not_warn_on_unused_top_level_exports() {
        let analysis = analyze("f := x -> x\nx = 1\n");

        assert!(
            !has_diagnostic(&analysis, M2Diagnostic::UnusedBinding),
            "top-level bindings should not be warned as unused exports"
        );
    }

    #[test]
    fn protect_hints_only_for_names_bound_before_the_call() {
        // Removing the source-order binding lookup would incorrectly flag
        // `unassigned` and the forward use of `later`, or miss `assigned`.
        let analysis = analyze(
            "assigned = target\n\
             protect assigned\n\
             protect unassigned\n\
             protect later\n\
             later = target\n",
        );

        let hints = analysis
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.source.as_deref() == Some("protect-assigned-symbol"))
            .collect::<Vec<_>>();

        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(hints[0].range.start.line, 1);
        assert!(hints[0].message.contains("protect symbol assigned"));
    }

    #[test]
    fn protect_distinguishes_explicit_symbols_and_computed_values() {
        // Dropping the syntax/type split would warn for the explicit quote,
        // miss unknown/Symbol-valued computations, or diagnose known integers.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins(
            "x = y\n\
             protect symbol x\n\
             protect (if c then symbol x else symbol y)\n\
             protect (if c then 1 else symbol y)\n\
             protect (1 + 2)\n",
            &builtins,
        );

        let warnings = analysis
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.source.as_deref() == Some("protect-computed-symbol"))
            .collect::<Vec<_>>();

        assert_eq!(
            warnings
                .iter()
                .map(|diagnostic| diagnostic.range.start.line)
                .collect::<Vec<_>>(),
            vec![2, 3],
        );
        assert!(warnings
            .iter()
            .all(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::WARNING)));
    }

    #[test]
    fn protect_hints_for_a_bound_parameter() {
        // A parameter is a real visible binding even though there is no
        // assignment expression at the call site.
        let analysis = analyze("f = x -> protect x\n");

        assert!(analysis.diagnostics.iter().any(|diagnostic| {
            diagnostic.source.as_deref() == Some("protect-assigned-symbol")
                && diagnostic.severity == Some(DiagnosticSeverity::HINT)
        }));
    }

    #[test]
    fn protect_uses_builtin_bindings_but_ignores_a_shadowed_callable() {
        // Builtin names are already assigned at document start, while a local
        // binding named `protect` replaces the builtin callable at later uses.
        let builtins = core_builtins();
        let analysis = analyze_with_builtins(
            "protect ZZ\n\
             x = y\n\
             protect = f\n\
             protect x\n",
            &builtins,
        );

        assert_eq!(
            analysis
                .diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.source.as_deref() == Some("protect-assigned-symbol")
                })
                .map(|diagnostic| diagnostic.range.start.line)
                .collect::<Vec<_>>(),
            vec![0],
        );
    }

    #[test]
    fn diagnoses_ambiguous_float_member_access() {
        let analysis = analyze("x.3\n");
        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].message,
            AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE
        );
        assert_eq!(
            analysis.diagnostics[0].severity,
            Some(DiagnosticSeverity::WARNING)
        );
        assert_eq!(analysis.diagnostics[0].range.start, Position::new(0, 0));
    }

    #[test]
    fn does_not_diagnose_ambiguous_member_access_with_whitespace() {
        let analysis = analyze("x .3\n");
        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn ambiguous_member_access_helper_requires_dot_prefixed_float() {
        assert_eq!(
            member_index_for_ambiguous_float_literal(".3"),
            Some("3".to_string())
        );
        assert_eq!(
            member_index_for_ambiguous_float_literal(".33"),
            Some("33".to_string())
        );
        assert_eq!(member_index_for_ambiguous_float_literal("3.0"), None);
    }

    #[test]
    fn option_key_convention_fires_in_call_arguments() {
        // A lowercase option key in a call's argument sequence gets the Hint.
        let analysis = analyze("gb(I, strategy => 4)\n");
        assert!(has_diagnostic(&analysis, M2Diagnostic::OptionKeyConvention));
    }

    #[test]
    fn option_key_convention_silent_in_collection_literals() {
        // Lowercase keys are legitimate hashtable entries — the hint stays out.
        let analysis = analyze("hashTable {a => 1, b => 2}\n");
        assert!(!has_diagnostic(
            &analysis,
            M2Diagnostic::OptionKeyConvention
        ));
    }

    #[test]
    fn no_diagnostic_for_else_on_same_line() {
        let analysis = analyze("if x then y else z");
        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn no_diagnostic_for_if_without_else() {
        let analysis = analyze("if x then y");
        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn user_type_method_installation_is_syntax_not_option() {
        // Mirrors the live `rootOf` case: a `rootOf = method()` binding, then
        // `rootOf TokenStream := TokenTree => ts -> ts`. The `f Type` install form
        // makes `TokenTree =>` return-type syntax, not an Option.
        let analysis = analyze("rootOf = method()\nrootOf TokenStream := TokenTree => ts -> ts\n");
        assert!(
            analysis
                .registry()
                .expressions
                .values()
                .all(|fact| fact.result_type != InferredType::of("Option")),
            "method installation `=>` must not produce an Option type"
        );
    }
}
