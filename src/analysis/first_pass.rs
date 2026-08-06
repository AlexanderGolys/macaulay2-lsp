//! Syntax-only collection performed before semantic enrichment.

use std::cmp::Ordering;

use tower_lsp::lsp_types::{Position, Range as TextRange};

use crate::meta::BindingRole;
use crate::node_metadata::{M2Node, NodeKind};
use crate::source::SourceNavigation;
use crate::util::TextRangeExt;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    range: TextRange,
    parent: Option<usize>,
    assignments_may_escape: bool,
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
            }],
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct TraversalContext {
    pub scope: usize,
    pub assignment_scope: usize,
}

#[derive(Clone, Copy)]
pub(super) enum BindingEffect {
    Declare,
    Assign,
}

#[derive(Clone, Copy)]
pub(super) struct BindingFact<'tree> {
    pub target: M2Node<'tree>,
    pub value: Option<M2Node<'tree>>,
    pub role: BindingRole,
    pub effect: BindingEffect,
    pub scope: usize,
    pub potential_export: bool,
}

pub(super) fn walk<'tree>(
    root: M2Node<'tree>,
    source: &(impl SourceNavigation + ?Sized),
    mut collect: impl FnMut(BindingFact<'tree>, &ScopeTree),
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
    collect: &mut impl FnMut(BindingFact<'tree>, &ScopeTree),
) {
    let context = match node.kind {
        NodeKind::LambdaExpression => {
            let scope = scopes.push(node, source, inherited.scope, true);
            TraversalContext {
                scope,
                assignment_scope: scope,
            }
        }
        NodeKind::ForStatement => TraversalContext {
            scope: scopes.push(node, source, inherited.scope, false),
            ..inherited
        },
        _ => inherited,
    };

    collect_bindings(node, context, scopes, collect);

    for child in node.children() {
        let child_context = control_flow_scope(node, child).map_or(context, |kind| {
            let scope = scopes.push(child, source, context.scope, kind.assignments_may_escape());
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
    scopes: &ScopeTree,
    collect: &mut impl FnMut(BindingFact<'tree>, &ScopeTree),
) {
    let parameters = match node.kind {
        NodeKind::LambdaExpression => node.child_by_field_name("parameters"),
        NodeKind::ForStatement => node.child_by_field_name("variable"),
        _ => None,
    };
    if let Some(target) = parameters {
        emit_binding_targets(
            BindingFact {
                target,
                value: None,
                role: BindingRole::Parameter,
                effect: BindingEffect::Declare,
                scope: context.scope,
                potential_export: false,
            },
            NodeKind::is_parameter_container,
            scopes,
            collect,
        );
    }

    if !node.is_assignment() {
        return;
    }
    let Some(target) = node.child_by_field_name("left") else {
        return;
    };
    let Some(operator) = node.binary_operator() else {
        return;
    };
    let value = node.child_by_field_name("right");
    let (effect, scope, potential_export) = match operator {
        ":=" => (BindingEffect::Declare, context.scope, context.scope == 0),
        "=" => (
            BindingEffect::Assign,
            context.assignment_scope,
            context.assignment_scope == 0
                || scopes.assignments_may_escape(context.assignment_scope),
        ),
        _ => return,
    };
    emit_binding_targets(
        BindingFact {
            target,
            value,
            role: BindingRole::Ordinary,
            effect,
            scope,
            potential_export,
        },
        NodeKind::is_collection_expression,
        scopes,
        collect,
    );
}

fn emit_binding_targets<'tree>(
    fact: BindingFact<'tree>,
    contains_symbols: impl Fn(&NodeKind) -> bool + Copy,
    scopes: &ScopeTree,
    collect: &mut impl FnMut(BindingFact<'tree>, &ScopeTree),
) {
    for target in nested_symbols(fact.target, contains_symbols) {
        collect(BindingFact { target, ..fact }, scopes);
    }
}

impl ScopeTree {
    fn push(
        &mut self,
        owner: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        parent: usize,
        assignments_may_escape: bool,
    ) -> usize {
        let scope = self.scopes.len();
        self.scopes.push(Scope {
            range: source.range_for_node(owner),
            parent: Some(parent),
            assignments_may_escape,
        });
        scope
    }

    pub(super) fn owned_by(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) -> usize {
        self.with_range(source.range_for_node(node))
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
            (is_body || child.kind.is_try_clause()).then_some(ControlFlowScope::Branch)
        }
        NodeKind::ForStatement => child
            .kind
            .is_loop_clause()
            .then_some(ControlFlowScope::LoopClause),
        NodeKind::WhileStatement => {
            let is_condition = parent
                .named_child(0)
                .is_some_and(|condition| condition.id() == child.id());
            (is_condition || child.kind.is_loop_clause()).then_some(ControlFlowScope::LoopClause)
        }
        _ => None,
    }
}

pub(super) fn nested_symbols<'tree>(
    node: M2Node<'tree>,
    contains_symbols: impl Fn(&NodeKind) -> bool + Copy,
) -> Vec<M2Node<'tree>> {
    if node.kind == NodeKind::Symbol {
        return vec![node];
    }
    if !contains_symbols(&node.kind) {
        return Vec::new();
    }
    node.children()
        .flat_map(|child| nested_symbols(child, contains_symbols))
        .collect()
}
