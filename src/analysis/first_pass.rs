//! Syntax-only collection performed before semantic enrichment.

use std::cmp::Ordering;

use m2_syn::visit::{self, Visit};
use m2_syn::{
    AssignmentExpr, BindingPack, Collection, Component, ElseClause, Expr, ExpressionCell, ForLoop,
    IfStatement, IterationRange, LambdaExpression, LambdaParameters, LoopBody, MutedCell,
    MutedGroup, ParenthesizedExpression, Punctuated, SequenceCell, SourceFile, Spanned, Symbol,
    ThenClause, Token, TryFallback, TryStatement, WhileLoop,
};
use tower_lsp::lsp_types::{Position, Range as TextRange};

use crate::meta::BindingRole;
use crate::node_metadata::{syntax_byte_range, M2Node};
use crate::object_registry::ObjectName;
use crate::source::SourceNavigation;
use crate::util::TextRangeExt;

use super::{is_value_cell, source_cell, Dispatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingEffect {
    Declare,
    Assign,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    range: TextRange,
    parent: Option<usize>,
    assignments_may_escape: bool,
    function_dispatch: Option<Dispatch>,
}

#[derive(Debug)]
pub(super) struct ScopeTree {
    scopes: Vec<Scope>,
}

impl Default for ScopeTree {
    fn default() -> Self {
        Self {
            scopes: vec![Scope {
                range: TextRange::new(pos!(), pos_max!()),
                parent: None,
                assignments_may_escape: false,
                function_dispatch: None,
            }],
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct TraversalContext {
    pub scope: usize,
    pub assignment_scope: usize,
}

#[derive(Debug, Clone)]
pub struct BindingFact {
    pub name: ObjectName,
    pub target: TextRange,
    pub value: Option<TextRange>,
    pub definition: TextRange,
    pub role: BindingRole,
    pub effect: BindingEffect,
    pub scope: usize,
    pub potential_export: bool,
}

impl BindingFact {
    pub fn from_node(
        target: M2Node<'_>,
        value: Option<M2Node<'_>>,
        role: BindingRole,
        effect: BindingEffect,
        scope: usize,
        potential_export: bool,
        source: &(impl SourceNavigation + ?Sized),
    ) -> Self {
        Self {
            name: ObjectName::new(target.text()),
            target: source.range_for_node(target),
            value: value.map(|value| source.range_for_node(value)),
            definition: source.range_for_node(
                source_cell(target)
                    .filter(|cell| is_value_cell(*cell))
                    .unwrap_or(target),
            ),
            role,
            effect,
            scope,
            potential_export,
        }
    }
}

pub fn syntax_range(
    syntax: &(impl Spanned + ?Sized),
    source: &(impl SourceNavigation + ?Sized),
) -> Option<TextRange> {
    let (start, end) = syntax_byte_range(syntax)?;
    Some(source.range_for_bytes(start..end))
}

pub(super) fn walk_cst(
    root: M2Node<'_>,
    source: &(impl SourceNavigation + ?Sized),
    mut collect: impl FnMut(BindingFact, &ScopeTree),
) -> ScopeTree {
    let mut scopes = ScopeTree::default();
    walk_nested(
        root,
        source,
        TraversalContext {
            scope: 0,
            assignment_scope: 0,
        },
        &mut scopes,
        &mut collect,
    );
    scopes
}

fn walk_nested<'tree>(
    node: M2Node<'tree>,
    source: &(impl SourceNavigation + ?Sized),
    inherited: TraversalContext,
    scopes: &mut ScopeTree,
    collect: &mut impl FnMut(BindingFact, &ScopeTree),
) {
    let context = if node.is::<LambdaExpression>() {
        let scope = scopes.push(
            node,
            source,
            inherited.scope,
            true,
            cst_function_dispatch(node),
        );
        TraversalContext {
            scope,
            assignment_scope: scope,
        }
    } else if node.is::<ForLoop>() {
        TraversalContext {
            scope: scopes.push(node, source, inherited.scope, false, None),
            ..inherited
        }
    } else {
        inherited
    };

    collect_bindings(node, context, source, scopes, collect);

    for child in node.children() {
        let child_context = control_flow_scope(node, child).map_or(context, |kind| {
            let scope = scopes.push(
                child,
                source,
                context.scope,
                kind.assignments_may_escape(),
                None,
            );
            TraversalContext {
                scope,
                assignment_scope: kind.assignment_scope(scope, context.assignment_scope),
            }
        });
        walk_nested(child, source, child_context, scopes, collect);
    }
}

fn collect_bindings<'tree>(
    node: M2Node<'tree>,
    context: TraversalContext,
    source: &(impl SourceNavigation + ?Sized),
    scopes: &ScopeTree,
    collect: &mut impl FnMut(BindingFact, &ScopeTree),
) {
    let parameters = if node.is::<LambdaExpression>() {
        node.child_by_field_name("parameters")
    } else if node.is::<ForLoop>() {
        node.child_by_field_name("variable")
    } else {
        None
    };
    if let Some(target) = parameters {
        for target in nested_symbols(target, M2Node::is_parameter_container) {
            collect(
                BindingFact::from_node(
                    target,
                    None,
                    BindingRole::Parameter,
                    BindingEffect::Declare,
                    context.scope,
                    false,
                    source,
                ),
                scopes,
            );
        }
    }

    if !node.is_assignment() {
        return;
    }
    let Some(target) = node.child_by_field_name("left") else {
        return;
    };
    let value = node.child_by_field_name("right");
    let (effect, scope, potential_export) = if node.has_binary_operator::<Token![:=]>() {
        (BindingEffect::Declare, context.scope, context.scope == 0)
    } else if node.has_binary_operator::<Token![=]>() {
        (
            BindingEffect::Assign,
            context.assignment_scope,
            context.assignment_scope == 0
                || scopes.assignments_may_escape(context.assignment_scope),
        )
    } else {
        return;
    };
    for target in nested_symbols(target, M2Node::is_collection_expression) {
        collect(
            BindingFact::from_node(
                target,
                value,
                BindingRole::Ordinary,
                effect,
                scope,
                potential_export,
                source,
            ),
            scopes,
        );
    }
}

pub fn walk(
    syntax: &SourceFile,
    source: &(impl SourceNavigation + ?Sized),
    mut collect: impl FnMut(BindingFact, &ScopeTree),
) -> ScopeTree {
    let mut walker = TypedWalker::new(source);
    walker.visit_source_file(syntax);
    for fact in walker.bindings {
        collect(fact, &walker.scopes);
    }
    walker.scopes
}

struct TypedWalker<'source, Source: ?Sized> {
    source: &'source Source,
    scopes: ScopeTree,
    bindings: Vec<BindingFact>,
    context: TraversalContext,
    definition: TextRange,
}

impl<'source, Source: SourceNavigation + ?Sized> TypedWalker<'source, Source> {
    fn new(source: &'source Source) -> Self {
        Self {
            source,
            scopes: ScopeTree::default(),
            bindings: Vec::new(),
            context: TraversalContext {
                scope: 0,
                assignment_scope: 0,
            },
            definition: source.full_range(),
        }
    }

    fn with_definition(&mut self, syntax: &(impl Spanned + ?Sized), visit: impl FnOnce(&mut Self)) {
        let inherited = self.definition;
        self.definition = syntax_range(syntax, self.source).unwrap_or(inherited);
        visit(self);
        self.definition = inherited;
    }

    fn with_owner_scope(
        &mut self,
        syntax: &(impl Spanned + ?Sized),
        owns_assignments: bool,
        assignments_may_escape: bool,
        function_dispatch: Option<Dispatch>,
        visit: impl FnOnce(&mut Self),
    ) {
        let inherited = self.context;
        let Some(range) = syntax_range(syntax, self.source) else {
            visit(self);
            return;
        };
        let scope = self.scopes.push_range(
            range,
            inherited.scope,
            assignments_may_escape,
            function_dispatch,
        );
        self.context = TraversalContext {
            scope,
            assignment_scope: if owns_assignments {
                scope
            } else {
                inherited.assignment_scope
            },
        };
        visit(self);
        self.context = inherited;
    }

    fn with_control_scope(
        &mut self,
        range: TextRange,
        kind: ControlFlowScope,
        visit: impl FnOnce(&mut Self),
    ) {
        let inherited = self.context;
        let scope =
            self.scopes
                .push_range(range, inherited.scope, kind.assignments_may_escape(), None);
        self.context = TraversalContext {
            scope,
            assignment_scope: kind.assignment_scope(scope, inherited.assignment_scope),
        };
        visit(self);
        self.context = inherited;
    }

    fn with_syntax_scope(
        &mut self,
        syntax: &(impl Spanned + ?Sized),
        kind: ControlFlowScope,
        visit: impl FnOnce(&mut Self),
    ) {
        let Some(range) = syntax_range(syntax, self.source) else {
            visit(self);
            return;
        };
        self.with_control_scope(range, kind, visit);
    }

    fn bind_symbol(
        &mut self,
        symbol: &Symbol,
        value: Option<&Expr>,
        role: BindingRole,
        effect: BindingEffect,
    ) {
        let Some(target) = syntax_range(symbol, self.source) else {
            return;
        };
        let scope = match effect {
            BindingEffect::Declare => self.context.scope,
            BindingEffect::Assign => self.context.assignment_scope,
        };
        self.bindings.push(BindingFact {
            name: ObjectName::new(&symbol.text),
            target,
            value: value.and_then(|value| syntax_range(value, self.source)),
            definition: self.definition,
            role,
            effect,
            scope,
            potential_export: match effect {
                BindingEffect::Declare => scope == 0,
                BindingEffect::Assign => scope == 0 || self.scopes.assignments_may_escape(scope),
            },
        });
    }

    fn bind_symbols<'ast>(
        &mut self,
        symbols: impl IntoIterator<Item = &'ast Symbol>,
        value: Option<&Expr>,
        role: BindingRole,
        effect: BindingEffect,
    ) {
        for symbol in symbols {
            self.bind_symbol(symbol, value, role, effect);
        }
    }
}

impl<'ast, Source: SourceNavigation + ?Sized> Visit<'ast> for TypedWalker<'_, Source> {
    fn visit_expression_cell(&mut self, node: &'ast ExpressionCell) {
        self.with_definition(node, |walker| visit::visit_expression_cell(walker, node));
    }

    fn visit_sequence_cell(&mut self, node: &'ast SequenceCell) {
        self.with_definition(node, |walker| visit::visit_sequence_cell(walker, node));
    }

    fn visit_muted_cell(&mut self, node: &'ast MutedCell) {
        self.with_definition(node, |walker| visit::visit_muted_cell(walker, node));
    }

    fn visit_assignment_expr(&mut self, node: &'ast AssignmentExpr) {
        match node {
            AssignmentExpr::Assignment(assignment) => self.bind_symbol(
                &assignment.left,
                Some(&assignment.right),
                BindingRole::Ordinary,
                BindingEffect::Assign,
            ),
            AssignmentExpr::LocalAssignment(assignment) => self.bind_symbol(
                &assignment.left,
                Some(&assignment.right),
                BindingRole::Ordinary,
                BindingEffect::Declare,
            ),
            AssignmentExpr::StructuredBinding(assignment) => self.bind_symbols(
                symbols_in_binding_pack(&assignment.left),
                Some(&assignment.right),
                BindingRole::Ordinary,
                BindingEffect::Assign,
            ),
            AssignmentExpr::LocalStructuredBinding(assignment) => self.bind_symbols(
                symbols_in_binding_pack(&assignment.left),
                Some(&assignment.right),
                BindingRole::Ordinary,
                BindingEffect::Declare,
            ),
            AssignmentExpr::BinaryAssignment(_)
            | AssignmentExpr::BinaryInstallation(_)
            | AssignmentExpr::PrefixAssignment(_)
            | AssignmentExpr::PrefixInstallation(_)
            | AssignmentExpr::PostfixAssignment(_)
            | AssignmentExpr::PostfixInstallation(_)
            | AssignmentExpr::EvaluatedAssignment(_) => {}
        }
        visit::visit_assignment_expr(self, node);
    }

    fn visit_lambda_expression(&mut self, node: &'ast LambdaExpression) {
        self.with_owner_scope(
            node,
            true,
            true,
            Some(typed_function_dispatch(&node.parameters)),
            |walker| {
                walker.bind_symbols(
                    symbols_in_lambda_parameters(&node.parameters),
                    None,
                    BindingRole::Parameter,
                    BindingEffect::Declare,
                );
                visit::visit_lambda_expression(walker, node);
            },
        );
    }

    fn visit_if_statement(&mut self, node: &'ast IfStatement) {
        self.with_syntax_scope(&node.condition, ControlFlowScope::Branch, |walker| {
            walker.visit_expr(&node.condition)
        });
        self.with_syntax_scope(&node.then_clause, ControlFlowScope::Branch, |walker| {
            walker.visit_expr(&node.then_clause.value)
        });
        if let Some(clause) = node.else_clause.as_deref() {
            self.with_syntax_scope(clause, ControlFlowScope::Branch, |walker| {
                walker.visit_expr(&clause.value)
            });
        }
    }

    fn visit_for_loop(&mut self, node: &'ast ForLoop) {
        self.with_owner_scope(node, false, false, None, |walker| {
            walker.bind_symbol(
                &node.variable,
                None,
                BindingRole::Parameter,
                BindingEffect::Declare,
            );
            if let Some(range) = node.range.as_deref() {
                walker.with_syntax_scope(range, ControlFlowScope::LoopClause, |walker| {
                    visit::visit_iteration_range(walker, range)
                });
            }
            if let Some(filter) = node.filter.as_deref() {
                let range = syntax_range(filter, walker.source)
                    .zip(
                        node.when_keyword
                            .as_ref()
                            .and_then(|token| syntax_range(token, walker.source)),
                    )
                    .map(|(value, keyword)| TextRange::new(keyword.start, value.end));
                if let Some(range) = range {
                    walker.with_control_scope(range, ControlFlowScope::LoopClause, |walker| {
                        walker.visit_expr(filter)
                    });
                } else {
                    walker.visit_expr(filter);
                }
            }
            walker.with_syntax_scope(&node.body, ControlFlowScope::LoopClause, |walker| {
                visit::visit_loop_body(walker, &node.body)
            });
        });
    }

    fn visit_while_loop(&mut self, node: &'ast WhileLoop) {
        self.with_syntax_scope(&node.condition, ControlFlowScope::LoopClause, |walker| {
            walker.visit_expr(&node.condition)
        });
        self.with_syntax_scope(&node.body, ControlFlowScope::LoopClause, |walker| {
            visit::visit_loop_body(walker, &node.body)
        });
    }

    fn visit_try_statement(&mut self, node: &'ast TryStatement) {
        self.with_syntax_scope(&node.value, ControlFlowScope::Branch, |walker| {
            walker.visit_expr(&node.value)
        });
        if let Some(clause) = node.then_clause.as_deref() {
            self.with_syntax_scope(clause, ControlFlowScope::Branch, |walker| {
                walker.visit_expr(&clause.value)
            });
        }
        if let Some(fallback) = node.fallback.as_deref() {
            match fallback {
                TryFallback::ExceptClause(clause) => {
                    self.with_syntax_scope(clause, ControlFlowScope::Branch, |walker| {
                        walker.visit_expr(&clause.value)
                    });
                }
                TryFallback::ElseClause(clause) => {
                    self.with_syntax_scope(clause, ControlFlowScope::Branch, |walker| {
                        walker.visit_expr(&clause.value)
                    });
                }
            }
        }
    }
}

fn symbols_in_lambda_parameters(parameters: &LambdaParameters) -> Vec<&Symbol> {
    match parameters {
        LambdaParameters::Symbol(symbol) => vec![symbol],
        LambdaParameters::ParenthesizedExpression(collection) => {
            symbols_in_parenthesized(collection)
        }
        LambdaParameters::Sequence(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
        LambdaParameters::List(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
        LambdaParameters::Array(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
        LambdaParameters::AngleBarList(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
    }
}

fn typed_function_dispatch(parameters: &LambdaParameters) -> Dispatch {
    match parameters {
        LambdaParameters::Symbol(_) => Dispatch::Variadic,
        LambdaParameters::ParenthesizedExpression(_) => Dispatch::Fixed(1),
        LambdaParameters::Sequence(collection) => Dispatch::Fixed(collection.elements.len()),
        LambdaParameters::List(collection) => Dispatch::Fixed(collection.elements.len()),
        LambdaParameters::Array(collection) => Dispatch::Fixed(collection.elements.len()),
        LambdaParameters::AngleBarList(collection) => Dispatch::Fixed(collection.elements.len()),
    }
}

fn cst_function_dispatch(lambda: M2Node) -> Option<Dispatch> {
    let parameters = lambda.child_by_field_name("parameters")?;
    if parameters.is::<ParenthesizedExpression>() {
        Some(Dispatch::Fixed(1))
    } else if parameters.is_collection_expression() {
        Some(Dispatch::Fixed(parameters.collection_elements().count()))
    } else {
        Some(Dispatch::Variadic)
    }
}

fn symbols_in_binding_pack(binding: &BindingPack) -> Vec<&Symbol> {
    match binding {
        BindingPack::ParenthesizedExpression(collection) => symbols_in_parenthesized(collection),
        BindingPack::Sequence(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
        BindingPack::List(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
        BindingPack::Array(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
        BindingPack::AngleBarList(collection) => {
            symbols_in_components(&collection.muted, &collection.elements)
        }
    }
}

fn symbols_in_parenthesized(collection: &m2_syn::ParenthesizedExpression) -> Vec<&Symbol> {
    let mut symbols = symbols_in_muted(&collection.muted);
    if let Some(value) = collection.value.as_deref() {
        collect_binding_symbols(value, &mut symbols);
    }
    symbols
}

fn symbols_in_components<'ast>(
    muted: &'ast [MutedGroup],
    elements: &'ast Punctuated<Component>,
) -> Vec<&'ast Symbol> {
    let mut symbols = symbols_in_muted(muted);
    collect_component_symbols(elements, &mut symbols);
    symbols
}

fn symbols_in_muted(muted: &[MutedGroup]) -> Vec<&Symbol> {
    let mut symbols = Vec::new();
    for group in muted {
        collect_component_symbols(&group.elements, &mut symbols);
    }
    symbols
}

fn collect_component_symbols<'ast>(
    components: &'ast Punctuated<Component>,
    symbols: &mut Vec<&'ast Symbol>,
) {
    for component in components {
        if let Component::Expr(expression) = component {
            collect_binding_symbols(expression, symbols);
        }
    }
}

fn collect_binding_symbols<'ast>(expression: &'ast Expr, symbols: &mut Vec<&'ast Symbol>) {
    match expression {
        Expr::Symbol(symbol) => symbols.push(symbol),
        Expr::Collection(collection) => match collection {
            Collection::ParenthesizedExpression(collection) => {
                symbols.extend(symbols_in_parenthesized(collection));
            }
            Collection::Sequence(collection) => symbols.extend(symbols_in_components(
                &collection.muted,
                &collection.elements,
            )),
            Collection::List(collection) => symbols.extend(symbols_in_components(
                &collection.muted,
                &collection.elements,
            )),
            Collection::Array(collection) => symbols.extend(symbols_in_components(
                &collection.muted,
                &collection.elements,
            )),
            Collection::AngleBarList(collection) => symbols.extend(symbols_in_components(
                &collection.muted,
                &collection.elements,
            )),
        },
        _ => {}
    }
}

impl ScopeTree {
    fn push(
        &mut self,
        owner: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        parent: usize,
        assignments_may_escape: bool,
        function_dispatch: Option<Dispatch>,
    ) -> usize {
        self.push_range(
            source.range_for_node(owner),
            parent,
            assignments_may_escape,
            function_dispatch,
        )
    }

    fn push_range(
        &mut self,
        range: TextRange,
        parent: usize,
        assignments_may_escape: bool,
        function_dispatch: Option<Dispatch>,
    ) -> usize {
        let scope = self.scopes.len();
        self.scopes.push(Scope {
            range,
            parent: Some(parent),
            assignments_may_escape,
            function_dispatch,
        });
        scope
    }

    pub(super) fn owned_by(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) -> usize {
        let range = source.range_for_node(node);
        self.with_range(range)
            .or_else(|| {
                self.scopes
                    .iter()
                    .enumerate()
                    .filter(|(_, scope)| {
                        scope.range.start == range.start && scope.range.end <= range.end
                    })
                    .max_by_key(|(_, scope)| scope.range.end)
                    .map(|(scope, _)| scope)
            })
            .expect("scope owner should be registered before semantic collection")
    }

    pub(super) fn with_range(&self, range: TextRange) -> Option<usize> {
        self.scopes.iter().position(|scope| scope.range == range)
    }

    pub(super) fn at(&self, position: Position) -> Option<usize> {
        self.scopes
            .iter()
            .enumerate()
            .filter(|(_, scope)| scope.range.contains_position(position))
            .min_by(|(_, left), (_, right)| {
                if left.range.is_inside(right.range) {
                    Ordering::Less
                } else if right.range.is_inside(left.range) {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            })
            .map(|(scope, _)| scope)
    }

    pub(super) fn len(&self) -> usize {
        self.scopes.len()
    }

    pub(super) fn parent(&self, scope: usize) -> Option<usize> {
        self.scopes.get(scope)?.parent
    }

    pub(super) fn assignments_may_escape(&self, scope: usize) -> bool {
        self.scopes[scope].assignments_may_escape
    }

    pub(super) fn function_dispatch(
        &self,
        owner: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) -> Option<Dispatch> {
        self.scopes[self.owned_by(owner, source)].function_dispatch
    }
}

#[derive(Clone, Copy)]
pub(super) enum ControlFlowScope {
    Branch,
    LoopClause,
}

impl ControlFlowScope {
    pub(super) fn assignment_scope(self, nested: usize, inherited: usize) -> usize {
        match self {
            Self::Branch => nested,
            Self::LoopClause => inherited,
        }
    }

    fn assignments_may_escape(self) -> bool {
        matches!(self, Self::Branch)
    }
}

pub(super) fn control_flow_scope(
    parent: M2Node<'_>,
    child: M2Node<'_>,
) -> Option<ControlFlowScope> {
    let is_field = |field| {
        parent
            .child_by_field_name(field)
            .is_some_and(|value| value.id() == child.id())
    };
    if parent.is::<IfStatement>() {
        (is_field("condition") || child.is::<ThenClause>() || child.is::<ElseClause>())
            .then_some(ControlFlowScope::Branch)
    } else if parent.is::<TryStatement>() {
        (is_field("value") || child.is::<ThenClause>() || child.is::<TryFallback>())
            .then_some(ControlFlowScope::Branch)
    } else if parent.is::<ForLoop>() {
        (child.is::<IterationRange>() || is_field("filter") || child.is::<LoopBody>())
            .then_some(ControlFlowScope::LoopClause)
    } else if parent.is::<WhileLoop>() {
        (is_field("condition") || child.is::<LoopBody>()).then_some(ControlFlowScope::LoopClause)
    } else {
        None
    }
}

pub(super) fn nested_symbols<'tree>(
    node: M2Node<'tree>,
    contains_symbols: impl Fn(&M2Node<'tree>) -> bool + Copy,
) -> Vec<M2Node<'tree>> {
    if node.is::<Symbol>() {
        return vec![node];
    }
    if !contains_symbols(&node) {
        return Vec::new();
    }
    node.children()
        .flat_map(|child| nested_symbols(child, contains_symbols))
        .collect()
}
