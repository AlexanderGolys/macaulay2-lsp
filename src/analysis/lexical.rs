use std::cmp::Ordering;

use tower_lsp::lsp_types::{Position, Range as TextRange};

use crate::node_metadata::{M2Node, NodeKind};
use crate::source::SourceNavigation;
use crate::util::position_in_range;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    range: TextRange,
    parent: Option<usize>,
    assignments_may_escape: bool,
}

#[derive(Debug, Default)]
pub(super) struct ScopeTree {
    scopes: Vec<Scope>,
}

impl ScopeTree {
    pub(super) fn collect(root: M2Node, source: &(impl SourceNavigation + ?Sized)) -> Self {
        let mut scopes = Self {
            scopes: vec![Scope {
                range: TextRange::new(pos!(), pos_max!()),
                parent: None,
                assignments_may_escape: false,
            }],
        };
        scopes.collect_nested(root, source, 0);
        scopes
    }

    fn collect_nested(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        current_scope: usize,
    ) {
        let nested_scope = match node.kind {
            NodeKind::LambdaExpression => self.push(node, source, current_scope, true),
            NodeKind::ForStatement => self.push(node, source, current_scope, false),
            _ => current_scope,
        };

        for child in node.children() {
            let child_scope = control_flow_scope(node, child).map_or(nested_scope, |scope| {
                self.push(child, source, nested_scope, scope.assignments_may_escape())
            });
            self.collect_nested(child, source, child_scope);
        }
    }

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
            .filter(|(_, scope)| position_in_range(position, scope.range))
            .min_by(|(_, left), (_, right)| {
                if range_is_inside(left.range, right.range) {
                    Ordering::Less
                } else if range_is_inside(right.range, left.range) {
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

pub(super) fn is_try_clause(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::ThenClause
            | NodeKind::ElseClause
            | NodeKind::ExceptClause
            | NodeKind::DoClause
            | NodeKind::WhenClause
    )
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

fn range_is_inside(inner: TextRange, outer: TextRange) -> bool {
    let starts_inside = inner.start.line > outer.start.line
        || (inner.start.line == outer.start.line && inner.start.character >= outer.start.character);
    let ends_inside = inner.end.line < outer.end.line
        || (inner.end.line == outer.end.line && inner.end.character <= outer.end.character);
    starts_inside && ends_inside && inner != outer
}
