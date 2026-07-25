//! Planning sketch for the retained, node-owned semantic model.
//!
//! This file is intentionally not connected to `main.rs` and is not expected to
//! compile yet.  It exists so the complete design can be read and navigated in
//! one place while we decide the architecture.
//!
//! Important distinctions:
//!
//! - the semantic tree strongly owns its children;
//! - all cross-tree relationships are weak pointers;
//! - scopes are owned by the nodes that introduce them;
//! - shared Symbols contain weak pointers directly to their occurrence nodes;
//! - source positions are read from the occurrence's Tree-sitter Node;
//! - Definition and Reassignment are derived from ordered Assignment nodes;
//! - destruction expires the weak occurrence and marks its Symbol/Scope dirty.

use std::{
    cmp::Ordering,
    collections::HashMap,
    sync::{Arc, Mutex, RwLock, Weak},
};

use tree_sitter::{InputEdit, Node, Point, Range, Tree};

// ---------------------------------------------------------------------------
// Direct node pointers
// ---------------------------------------------------------------------------

pub type NodeRef = Arc<dyn M2Node>;
pub type WeakNodeRef = Weak<dyn M2Node>;

pub type ScopeOwnerRef = Arc<dyn ScopeOwner>;
pub type WeakScopeOwnerRef = Weak<dyn ScopeOwner>;

pub type SymbolOccurrenceRef = Arc<dyn SymbolOccurrence>;
pub type WeakSymbolOccurrenceRef = Weak<dyn SymbolOccurrence>;

/// The common interface of every retained semantic node.
///
/// There is deliberately no NodeId and no stored source position.  Other
/// objects point directly to an M2Node, then ask its current Tree-sitter Node
/// for its range or position.
pub trait M2Node: Send + Sync {
    fn syntax(&self) -> SyntaxNode;

    fn parent(&self) -> Option<WeakNodeRef>;

    /// Strong child ownership makes removing a node destroy its entire semantic
    /// subtree, unless some incorrect external strong pointer keeps it alive.
    fn children(&self) -> Vec<NodeRef>;

    fn owning_scope(&self) -> WeakScopeRef;

    fn start_position(&self) -> Point {
        self.syntax().node().start_position()
    }

    fn range(&self) -> Range {
        self.syntax().node().range()
    }

    /// Visit children in M2 evaluation order.
    ///
    /// Most nodes use source/child order.  Nodes such as assignment override
    /// this: the RHS is visited before the LHS Assignment occurrence.
    fn visit_evaluation_order(&self, visit: &mut dyn FnMut(NodeRef));
}

// ---------------------------------------------------------------------------
// Tree-sitter attachment
// ---------------------------------------------------------------------------

/// An owned snapshot that keeps the Tree alive while its Node is being used.
///
/// The final implementation must hide Tree-sitter's `Node<'tree>` lifetime
/// here.  A retained Node is not a live view into a later edited Tree:
///
/// - `Tree::edit` updates Nodes retrieved from the Tree afterward;
/// - an already-retained Node must receive `Node::edit`;
/// - a reparsed/changed node must be rebound to a Node from the new Tree.
///
/// The exact safe storage mechanism is still undecided.  The public semantic
/// model only relies on `SyntaxAttachment::current`.
pub struct SyntaxNode {
    tree: Arc<Tree>,
    node: Node<'static>,
}

impl SyntaxNode {
    pub fn node(&self) -> Node<'_> {
        // Planning placeholder: the final wrapper ties the returned Node's
        // lifetime to `self.tree`.
        todo!("return the retained Tree-sitter Node while keeping tree alive")
    }
}

pub struct SyntaxAttachment {
    current: RwLock<SyntaxNode>,
}

impl SyntaxAttachment {
    pub fn current(&self) -> SyntaxNode {
        todo!("clone the current owned Tree/Node pair")
    }

    /// Apply a source edit to a retained Node whose syntax was not reparsed.
    pub fn edit(&self, edit: &InputEdit) {
        let _ = edit;
        todo!("edit the retained syntax handle")
    }

    /// Replace the syntax attachment when reconciliation finds the
    /// corresponding Node in the newly parsed Tree.
    pub fn rebind(&self, tree: Arc<Tree>, node: Node<'_>) {
        let _ = (tree, node);
        todo!("replace the owned Tree/Node pair")
    }
}

// ---------------------------------------------------------------------------
// Node-owned scopes
// ---------------------------------------------------------------------------

pub type ScopeRef = Arc<Scope>;
pub type WeakScopeRef = Weak<Scope>;

/// How a nested scope sees its parent.
///
/// `visible_through` is a node pointer, not a byte position.  For a lambda it
/// is the lambda-creation node, so bindings created later in the parent scope
/// are not visible inside an earlier lambda.
pub struct ParentScope {
    pub scope: WeakScopeRef,
    pub visible_through: WeakNodeRef,
}

pub struct Scope {
    /// Weak because the owner strongly owns this Scope.
    owner: WeakScopeOwnerRef,
    parent: Option<ParentScope>,
}

impl Scope {
    pub fn owner(&self) -> Option<ScopeOwnerRef> {
        self.owner.upgrade()
    }

    pub fn parent(&self) -> Option<&ParentScope> {
        self.parent.as_ref()
    }

    /// Compare two occurrence nodes in M2 evaluation order.
    ///
    /// Tree-sitter positions establish source order between independent
    /// expressions.  The owning semantic nodes resolve cases where evaluation
    /// order differs from source order, such as assignment RHS-before-LHS.
    pub fn compare_evaluation_order(
        &self,
        left: &SymbolOccurrenceRef,
        right: &SymbolOccurrenceRef,
    ) -> Ordering {
        let _ = (left, right);
        todo!("compare by walking the scope owner's retained semantic tree")
    }
}

/// Implemented by SourceFile, Lambda, scoped Array, and any later syntax node
/// that introduces a lexical scope.
pub trait ScopeOwner: M2Node {
    fn scope(&self) -> ScopeRef;
}

// ---------------------------------------------------------------------------
// Shared Symbols and their global occurrence vectors
// ---------------------------------------------------------------------------

/// Hashable name identity.  Raw String is never used as a map key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SymbolName(Arc<str>);

pub type SymbolRef = Arc<Symbol>;
pub type WeakSymbolRef = Weak<Symbol>;

/// One entry in a Symbol's global occurrence vector.
///
/// The weak node pointer is the position.  While it is alive, its source
/// position comes from `node.syntax().node()`.  Once it dies, this entry remains
/// temporarily as a tombstone until the dirty Symbol/Scope is repaired.
pub struct OccurrenceEntry {
    pub scope: WeakScopeRef,
    pub kind: OccurrenceKind,
    pub node: WeakSymbolOccurrenceRef,
}

/// These are intrinsic syntax roles.
///
/// Definition and Reassignment are intentionally absent: they depend on which
/// binding-producing occurrence is first in the current evaluation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OccurrenceKind {
    Reference,
    Assignment,
    Parameter,
    LocalBinding,
}

pub struct Symbol {
    name: SymbolName,

    /// Occurrences from every scope in this document.  Scope ownership and
    /// ordering are obtained through the referenced nodes.
    occurrences: RwLock<Vec<OccurrenceEntry>>,

    /// Destruction/creation marks a Symbol/Scope pair dirty.  Repair is delayed
    /// until the edit transaction has removed old nodes and attached new ones.
    dirty_scopes: Mutex<Vec<WeakScopeRef>>,
}

impl Symbol {
    pub fn name(&self) -> &SymbolName {
        &self.name
    }

    pub fn register(self: &SymbolRef, node: &SymbolOccurrenceRef) {
        let entry = OccurrenceEntry {
            scope: node.owning_scope(),
            kind: node.occurrence_kind(),
            node: Arc::downgrade(node),
        };

        self.occurrences.write().unwrap().push(entry);

        if let Some(scope) = node.owning_scope().upgrade() {
            self.mark_dirty(&scope);
        }
    }

    pub fn mark_dirty(&self, scope: &ScopeRef) {
        let mut dirty = self.dirty_scopes.lock().unwrap();

        if dirty
            .iter()
            .filter_map(Weak::upgrade)
            .any(|existing| Arc::ptr_eq(&existing, scope))
        {
            return;
        }

        dirty.push(Arc::downgrade(scope));
    }

    /// Upgrade and order all surviving occurrences belonging to `scope`.
    ///
    /// This returns direct node pointers.  It never returns NodeIds or stored
    /// ranges.
    pub fn live_occurrences_in(
        &self,
        scope: &ScopeRef,
    ) -> Vec<SymbolOccurrenceRef> {
        let mut live = self
            .occurrences
            .read()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry
                    .scope
                    .upgrade()
                    .is_some_and(|owner| Arc::ptr_eq(&owner, scope))
            })
            .filter_map(|entry| entry.node.upgrade())
            .collect::<Vec<_>>();

        live.sort_by(|left, right| scope.compare_evaluation_order(left, right));
        live
    }

    /// Recompute definition/reassignment/reference facts for one Symbol in one
    /// Scope, then notify only nodes whose cached fact changed.
    pub fn repair_scope(&self, scope: &ScopeRef) {
        let occurrences = self.live_occurrences_in(scope);

        let mut definition: Option<WeakSymbolOccurrenceRef> = None;
        let mut current_assignment: Option<WeakSymbolOccurrenceRef> = None;
        let mut binding_reference_count = 0usize;

        for occurrence in occurrences {
            match occurrence.occurrence_kind() {
                OccurrenceKind::Parameter | OccurrenceKind::LocalBinding => {
                    let occurrence = Arc::downgrade(&occurrence);
                    definition.get_or_insert_with(|| occurrence.clone());
                    current_assignment = Some(occurrence.clone());

                    occurrence.update_derived(DerivedOccurrence::Definition);
                }

                OccurrenceKind::Assignment => {
                    let occurrence = Arc::downgrade(&occurrence);

                    let derived = if definition.is_none() {
                        definition = Some(occurrence.clone());
                        DerivedOccurrence::Definition
                    } else {
                        DerivedOccurrence::Reassignment {
                            definition: definition.as_ref().unwrap().clone(),
                        }
                    };

                    current_assignment = Some(occurrence.clone());
                    occurrence.update_derived(derived);
                }

                OccurrenceKind::Reference => {
                    let derived = match &current_assignment {
                        Some(assignment) => {
                            binding_reference_count += 1;

                            DerivedOccurrence::Reference {
                                definition: definition.as_ref().unwrap().clone(),
                                current_assignment: assignment.clone(),
                            }
                        }
                        None => DerivedOccurrence::Symbol,
                    };

                    occurrence.update_derived(derived);
                }
            }
        }

        if definition.is_some() && binding_reference_count == 0 {
            self.install_unused_binding_diagnostic(scope, definition.unwrap());
        } else {
            self.remove_unused_binding_diagnostic(scope);
        }

        self.prune_dead_occurrences();
    }

    fn prune_dead_occurrences(&self) {
        self.occurrences
            .write()
            .unwrap()
            .retain(|entry| entry.node.strong_count() != 0);
    }

    fn install_unused_binding_diagnostic(
        &self,
        scope: &ScopeRef,
        definition: WeakSymbolOccurrenceRef,
    ) {
        let _ = (scope, definition);
        todo!("store the diagnostic on the relevant surviving node")
    }

    fn remove_unused_binding_diagnostic(&self, scope: &ScopeRef) {
        let _ = scope;
        todo!("remove the cached diagnostic from the relevant node")
    }
}

/// Document-level interning only.  Scope and binding facts do not live here.
pub struct Symbols {
    by_name: RwLock<HashMap<SymbolName, SymbolRef>>,
}

impl Symbols {
    pub fn intern(&self, name: SymbolName) -> SymbolRef {
        if let Some(symbol) = self.by_name.read().unwrap().get(&name) {
            return symbol.clone();
        }

        let mut symbols = self.by_name.write().unwrap();
        symbols
            .entry(name.clone())
            .or_insert_with(|| {
                Arc::new(Symbol {
                    name,
                    occurrences: RwLock::new(Vec::new()),
                    dirty_scopes: Mutex::new(Vec::new()),
                })
            })
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Symbol occurrence nodes
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum DerivedOccurrence {
    /// A Reference with no preceding binding-producing occurrence.
    Symbol,

    /// The first live binding-producing occurrence in this Scope.
    Definition,

    /// Any later Assignment.  Navigation still points to the first definition.
    Reassignment {
        definition: WeakSymbolOccurrenceRef,
    },

    /// A Reference after a definition.  Type/value propagation may depend on
    /// the latest preceding assignment while navigation uses the definition.
    Reference {
        definition: WeakSymbolOccurrenceRef,
        current_assignment: WeakSymbolOccurrenceRef,
    },
}

pub trait SymbolOccurrence: M2Node {
    fn symbol(&self) -> SymbolRef;

    fn occurrence_kind(&self) -> OccurrenceKind;

    /// Returns true when the cached result changed.  Only then should later
    /// type propagation or semantic-token consumers be notified.
    fn update_derived(&self, derived: DerivedOccurrence) -> bool;
}

/// The concrete retained node for one symbol token.
///
/// It registers a weak pointer to itself after construction.  Its Drop does
/// not need to locate/remove itself from the occurrence vector: it marks the
/// Symbol/Scope dirty, and the expired weak pointer is pruned during repair.
pub struct SymbolNode {
    pub syntax: SyntaxAttachment,
    pub parent: WeakNodeRef,
    pub scope: WeakScopeRef,
    pub symbol: SymbolRef,
    pub occurrence_kind: OccurrenceKind,
    pub derived: RwLock<Option<DerivedOccurrence>>,
}

impl SymbolNode {
    /// Must be called after the new Arc<SymbolNode> has been inserted into the
    /// strongly owned semantic tree.
    pub fn register(node: &Arc<Self>) {
        let occurrence: SymbolOccurrenceRef = node.clone();
        node.symbol.register(&occurrence);
    }
}

impl Drop for SymbolNode {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.upgrade() {
            self.symbol.mark_dirty(&scope);
        }
    }
}

impl M2Node for SymbolNode {
    fn syntax(&self) -> SyntaxNode {
        self.syntax.current()
    }

    fn parent(&self) -> Option<WeakNodeRef> {
        Some(self.parent.clone())
    }

    fn children(&self) -> Vec<NodeRef> {
        Vec::new()
    }

    fn owning_scope(&self) -> WeakScopeRef {
        self.scope.clone()
    }

    fn visit_evaluation_order(&self, visit: &mut dyn FnMut(NodeRef)) {
        let _ = visit;
    }
}

impl SymbolOccurrence for SymbolNode {
    fn symbol(&self) -> SymbolRef {
        self.symbol.clone()
    }

    fn occurrence_kind(&self) -> OccurrenceKind {
        self.occurrence_kind
    }

    fn update_derived(&self, derived: DerivedOccurrence) -> bool {
        let _ = derived;
        todo!("compare and replace the cached derived occurrence")
    }
}

// ---------------------------------------------------------------------------
// Scope-owning nodes and destruction
// ---------------------------------------------------------------------------

pub struct ScopeNode {
    pub syntax: SyntaxAttachment,
    pub parent: Option<WeakNodeRef>,

    /// The only strong ownership path to descendants.
    pub children: RwLock<Vec<NodeRef>>,

    /// This Scope cannot outlive its introducing node unless an incorrect
    /// external strong ScopeRef is retained.
    pub scope: ScopeRef,

    /// SourceFile, Lambda, ArrayScope, etc.  This is the normalized semantic
    /// kind created in the one sanctioned Tree-sitter constructor.
    pub kind: ScopeNodeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeNodeKind {
    SourceFile,
    Lambda,
    Array,
}

impl M2Node for ScopeNode {
    fn syntax(&self) -> SyntaxNode {
        self.syntax.current()
    }

    fn parent(&self) -> Option<WeakNodeRef> {
        self.parent.clone()
    }

    fn children(&self) -> Vec<NodeRef> {
        self.children.read().unwrap().clone()
    }

    fn owning_scope(&self) -> WeakScopeRef {
        Arc::downgrade(&self.scope)
    }

    fn visit_evaluation_order(&self, visit: &mut dyn FnMut(NodeRef)) {
        for child in self.children() {
            visit(child.clone());
            child.visit_evaluation_order(visit);
        }
    }
}

impl ScopeOwner for ScopeNode {
    fn scope(&self) -> ScopeRef {
        self.scope.clone()
    }
}

// No custom ScopeNode destructor is required:
//
// 1. removing the parent's strong pointer drops ScopeNode;
// 2. ScopeNode's strong child vector drops the entire subtree;
// 3. SymbolNode destructors mark their shared Symbols dirty;
// 4. occurrence vectors contain only weak pointers, so they keep nothing alive;
// 5. ScopeNode's ScopeData is then destroyed with its owner;
// 6. weak parent-scope and dependency pointers safely fail to upgrade.

// ---------------------------------------------------------------------------
// Assignment evaluation order
// ---------------------------------------------------------------------------

pub struct AssignmentNode {
    pub syntax: SyntaxAttachment,
    pub parent: WeakNodeRef,
    pub scope: WeakScopeRef,
    pub left: NodeRef,
    pub right: NodeRef,
}

impl M2Node for AssignmentNode {
    fn syntax(&self) -> SyntaxNode {
        self.syntax.current()
    }

    fn parent(&self) -> Option<WeakNodeRef> {
        Some(self.parent.clone())
    }

    fn children(&self) -> Vec<NodeRef> {
        vec![self.left.clone(), self.right.clone()]
    }

    fn owning_scope(&self) -> WeakScopeRef {
        self.scope.clone()
    }

    fn visit_evaluation_order(&self, visit: &mut dyn FnMut(NodeRef)) {
        // M2 evaluates the value before installing/updating the binding.
        visit(self.right.clone());
        self.right.visit_evaluation_order(visit);

        visit(self.left.clone());
        self.left.visit_evaluation_order(visit);
    }
}

// ---------------------------------------------------------------------------
// Later node-wise type propagation
// ---------------------------------------------------------------------------

pub type WeakTypedNodeRef = Weak<dyn TypedNode>;

pub trait TypedNode: M2Node {
    fn cached_type(&self) -> Option<TypeRef>;

    /// Recalculate from this node's direct inputs.  Return true only if the
    /// result changed, so the edit transaction schedules dependents once.
    fn recompute_type(&self) -> bool;

    fn type_dependents(&self) -> Vec<WeakTypedNodeRef>;
}

pub type TypeRef = Arc<dyn M2Type>;

pub trait M2Type: Send + Sync {}

// Type dependencies are weak:
//
// Assignment/child changes
//     -> update the affected Reference node
//     -> Reference recomputes its cached type
//     -> only if changed, enqueue its parent expression
//     -> continue until no cached type changes
//
// The queue and current edit transaction can be owned by SourceFileNode.  No
// document-wide Analysis value is required.
