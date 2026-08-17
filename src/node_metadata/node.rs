//! Typed access to Tree-sitter nodes and traversals.

use std::{collections::HashSet, iter};

use m2_syn::treesitter::TreeSitterNode;
use m2_syn::visit::{self, Visit};
use m2_syn::{
    AdjacentExpression, AngleBarList, Array, BlockComment, BreakStatement, Collection,
    ContinueStatement, ElseClause, EmptyComponent, ExceptClause, Expr, FloatLiteral, ForLoop,
    IfStatement, IntegerLiteral, IterationRange, LineComment, List, LoopBody, MutedCell,
    MutedGroup, NakedSequence, NewStatement, ParenthesizedExpression, QuoteExpression,
    RawStringLiteral, Reconstruct, ReturnStatement, Sequence, SourceFile, SourceId, Spanned,
    StringLiteral, Symbol, ThenClause, Token, TryStatement, WhileLoop,
};
use tree_sitter::{Node, Point};

/// Snapshot-local identity of one Tree-sitter syntax node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxNodeId(usize);

#[derive(Debug, Clone, Copy)]
pub struct M2Node<'tree> {
    node: Node<'tree>,
    source: &'tree str,
}

pub fn visit_source_nodes<'tree>(
    root: M2Node<'tree>,
    syntax: Option<&SourceFile>,
    mut visit: impl FnMut(M2Node<'tree>),
) {
    if let Some(syntax) = syntax {
        SyntaxNodeWalker::new(root, visit).visit_source_file(syntax);
    } else {
        root.descendants().for_each(&mut visit);
    }
}

pub fn visit_expression_nodes<'tree>(
    root: M2Node<'tree>,
    syntax: Option<&Expr>,
    mut visit: impl FnMut(M2Node<'tree>),
) {
    if let Some(syntax) = syntax {
        SyntaxNodeWalker::new(root, visit).visit_expr(syntax);
    } else {
        root.descendants().for_each(&mut visit);
    }
}

struct SyntaxNodeWalker<'tree, F> {
    root: M2Node<'tree>,
    visit: F,
    seen: HashSet<SyntaxNodeId>,
}

impl<'tree, F> SyntaxNodeWalker<'tree, F>
where
    F: FnMut(M2Node<'tree>),
{
    fn new(root: M2Node<'tree>, visit: F) -> Self {
        Self {
            root,
            visit,
            seen: HashSet::new(),
        }
    }

    fn record<Syntax>(&mut self, syntax: &Syntax)
    where
        Syntax: Reconstruct<TreeSitterNode<'tree, 'tree>> + Spanned,
    {
        if let Some(node) = self.root.descendant_for_syntax(syntax) {
            if self.seen.insert(node.id()) {
                (self.visit)(node);
            }
        }
    }
}

impl<'ast, 'tree, F> Visit<'ast> for SyntaxNodeWalker<'tree, F>
where
    F: FnMut(M2Node<'tree>),
{
    fn visit_expr(&mut self, node: &'ast Expr) {
        self.record(node);
        visit::visit_expr(self, node);
    }

    fn visit_collection(&mut self, node: &'ast Collection) {
        self.record(node);
        visit::visit_collection(self, node);
    }

    fn visit_symbol(&mut self, node: &'ast Symbol) {
        self.record(node);
        visit::visit_symbol(self, node);
    }

    fn visit_naked_sequence(&mut self, node: &'ast NakedSequence) {
        self.record(node);
        visit::visit_naked_sequence(self, node);
    }

    fn visit_muted_cell(&mut self, node: &'ast MutedCell) {
        self.record(node);
        visit::visit_muted_cell(self, node);
    }

    fn visit_muted_group(&mut self, node: &'ast MutedGroup) {
        self.record(node);
        visit::visit_muted_group(self, node);
    }

    fn visit_empty_component(&mut self, node: &'ast EmptyComponent) {
        self.record(node);
        visit::visit_empty_component(self, node);
    }
}

impl<'tree> M2Node<'tree> {
    pub(super) fn new(node: Node<'tree>, source: &'tree str) -> Self {
        Self { node, source }
    }

    pub fn text(&self) -> &'tree str {
        &self.source[self.node.start_byte()..self.node.end_byte()]
    }

    pub fn is<T>(&self) -> bool
    where
        T: Reconstruct<TreeSitterNode<'tree, 'tree>>,
    {
        T::matches(&TreeSitterNode::new(
            self.node,
            self.source.as_bytes(),
            SourceId(0),
        ))
    }

    pub fn is_symbol_like(&self) -> bool {
        self.is::<Symbol>()
            || self.parent().is_some_and(|parent| {
                parent.is::<QuoteExpression>()
                    && parent
                        .child_by_field_name("token")
                        .is_some_and(|token| token.id() == self.id())
            })
    }

    pub fn is_literal(&self) -> bool {
        self.is::<IntegerLiteral>()
            || self.is::<FloatLiteral>()
            || self.is::<StringLiteral>()
            || self.is::<RawStringLiteral>()
    }

    pub fn is_string_literal(&self) -> bool {
        self.is::<StringLiteral>() || self.is::<RawStringLiteral>()
    }

    pub fn is_collection_expression(&self) -> bool {
        self.is::<Sequence>()
            || self.is::<List>()
            || self.is::<Array>()
            || self.is::<AngleBarList>()
    }

    pub fn is_delimited_expression(&self) -> bool {
        self.is_collection_expression() || self.is::<ParenthesizedExpression>()
    }

    pub fn is_parameter_container(&self) -> bool {
        self.is::<Sequence>() || self.is::<List>() || self.is::<ParenthesizedExpression>()
    }

    pub fn is_sequence(&self) -> bool {
        self.is::<Sequence>() || self.is::<NakedSequence>()
    }

    pub fn is_nothing_value(&self) -> bool {
        self.is::<MutedCell>() || self.is::<EmptyComponent>()
    }

    pub fn is_comment(&self) -> bool {
        self.is::<LineComment>() || self.is::<BlockComment>()
    }

    pub fn is_control_transfer(&self) -> bool {
        self.is::<ReturnStatement>()
            || self.is::<BreakStatement>()
            || self.is::<ContinueStatement>()
    }

    pub fn is_keyword_statement(&self) -> bool {
        self.is::<IfStatement>()
            || self.is::<ForLoop>()
            || self.is::<WhileLoop>()
            || self.is::<NewStatement>()
            || self.is::<TryStatement>()
    }

    pub fn is_keyword_clause(&self) -> bool {
        self.is::<IterationRange>()
            || self.is::<LoopBody>()
            || self.is::<ThenClause>()
            || self.is::<ElseClause>()
            || self.is::<ExceptClause>()
    }

    pub fn closing_delimiter_width(&self) -> usize {
        if self.is::<AngleBarList>() {
            2
        } else {
            1
        }
    }

    fn raw_kind(&self) -> &'tree str {
        self.node.kind()
    }

    /// A human-facing label for the node's grammar kind, for diagnostic messages
    /// only (e.g. "Missing: )"). This is a display value, never branched on, so it
    /// stays correct across grammar renames.
    pub fn syntax_label(&self) -> &'tree str {
        self.raw_kind()
    }

    pub fn is_comma(&self) -> bool {
        self.is::<Token![,]>()
    }

    pub fn is_semicolon(&self) -> bool {
        self.is::<Token![;]>()
    }

    /// The implicit-application operator: the `SPACE` token tree-sitter inserts
    /// between a function and its juxtaposed argument (`sin x`, `f(x)`).
    pub fn is_implicit_application(&self) -> bool {
        self.is::<Token![SPACE]>()
    }

    pub fn has_binary_operator<T: m2_syn::Token>(&self) -> bool {
        self.binary_operator()
            .is_some_and(super::matches_token::<T>)
    }

    /// An opening collection delimiter: `(`, `{`, `[`, or `<|`.
    pub fn is_opening_delimiter(&self) -> bool {
        matches!(self.raw_kind(), "(" | "{" | "[" | "<|")
    }

    /// A closing collection delimiter: `)`, `}`, `]`, or `|>`.
    pub fn is_closing_delimiter(&self) -> bool {
        matches!(self.raw_kind(), ")" | "}" | "]" | "|>")
    }

    /// An anonymous keyword token (`if`, `then`, `for`, `return`, `time`, ...) —
    /// the bare keyword leaves, not the named clause/statement nodes that contain
    /// them. Used for keyword highlighting.
    pub fn is_keyword_token(&self) -> bool {
        !self.node.is_named()
            && (self.is::<Token![if]>()
                || self.is::<Token![then]>()
                || self.is::<Token![else]>()
                || self.is::<Token![from]>()
                || self.is::<Token![to]>()
                || self.is::<Token![when]>()
                || self.is::<Token![do]>()
                || self.is::<Token![in]>()
                || self.is::<Token![of]>()
                || self.is::<Token![list]>()
                || self.is::<Token![for]>()
                || self.is::<Token![while]>()
                || self.is::<Token![break]>()
                || self.is::<Token![continue]>()
                || self.is::<Token![return]>()
                || self.is::<Token![try]>()
                || self.is::<Token![catch]>()
                || self.is::<Token![throw]>()
                || self.is::<Token![time]>()
                || self.is::<Token![timing]>()
                || self.is::<Token![elapsedTime]>()
                || self.is::<Token![elapsedTiming]>()
                || self.is::<Token![profile]>()
                || self.is::<Token![shield]>()
                || self.is::<Token![TEST]>()
                || self.is::<Token![breakpoint]>()
                || self.is::<Token![finish]>()
                || self.is::<Token![new]>()
                || self.is::<Token![step]>())
    }

    /// An anonymous binding-modifier keyword token (`global`, `local`, `symbol`,
    /// `threadVariable`, `threadLocal`).
    pub fn is_modifier_token(&self) -> bool {
        !self.node.is_named()
            && (self.is::<Token![global]>()
                || self.is::<Token![local]>()
                || self.is::<Token![symbol]>()
                || self.is::<Token![threadVariable]>()
                || self.is::<Token![threadLocal]>())
    }

    /// The `then`/`else` keyword tokens, which open the clauses an `if` indents.
    pub fn is_then_or_else_keyword(&self) -> bool {
        !self.node.is_named() && (self.is::<Token![then]>() || self.is::<Token![else]>())
    }
}

impl<'tree> M2Node<'tree> {
    /// The operator token's source text of a binary expression (`:=`, `=>`, ...),
    /// or `None` if this is not a binary expression. Comparing this *parsed* text
    /// against operator spellings is reading code, not node-type names, so it is
    /// safe and rename-stable.
    pub fn binary_operator(&self) -> Option<&'tree str> {
        if self.is::<AdjacentExpression>() {
            return Some(super::token_spelling::<Token![SPACE]>());
        }
        self.child_by_field_name("left")?;
        self.child_by_field_name("right")?;
        self.child_by_field_name("operator").map(|operator| {
            if operator.is_implicit_application() {
                super::token_spelling::<Token![SPACE]>()
            } else {
                operator.text()
            }
        })
    }

    /// An assignment expression (`=`, `:=`, `<-`).
    pub fn is_assignment(&self) -> bool {
        self.binary_operator().is_some_and(|operator| {
            super::matches_token::<Token![=]>(operator)
                || super::matches_token::<Token![:=]>(operator)
                || super::matches_token::<Token![<-]>(operator)
        })
    }

    /// An option assignment (`key => value`).
    pub fn is_option_assignment(&self) -> bool {
        self.has_binary_operator::<Token![=>]>()
    }

    pub fn property_key(&self) -> Option<Self> {
        let right = self.child_by_field_name("right")?;
        let operator = self.binary_operator()?;
        (((super::matches_token::<Token![#]>(operator)
            || super::matches_token::<Token![#?]>(operator))
            && right.is_string_literal())
            || ((super::matches_token::<Token![.]>(operator)
                || super::matches_token::<Token![.?]>(operator))
                && right.is_symbol_like()))
        .then_some(right)
    }

    /// An implicit application `f x` / `f(x)`: a binary expression whose operator
    /// is the inserted `SPACE` token.
    pub fn is_space_application(&self) -> bool {
        self.is::<AdjacentExpression>() || self.has_binary_operator::<Token![SPACE]>()
    }
}

impl<'tree> M2Node<'tree> {
    /// Whether this node is the `operator` field of its own parent.
    pub fn is_operator(&self) -> bool {
        self.parent()
            .and_then(|parent| parent.child_by_field_name("operator"))
            .is_some_and(|operator| operator.id() == self.id())
    }

    /// Whether `other`'s byte span lies within this node's span.
    pub fn contains(&self, other: M2Node<'_>) -> bool {
        self.start_byte() <= other.start_byte() && other.end_byte() <= self.end_byte()
    }

    /// The text *inside* a string literal's delimiters — the value without the
    /// surrounding `"`/`"` or `///`/`///` — located via the parser's own delimiter
    /// tokens (first and last child), never by scanning for quote characters.
    /// Escaped quotes, raw-string `/////`, and `"`-in-comment / `--`-in-string
    /// nesting are the parser's concern, not ours. Returns `None` for a non-string
    /// node or a literal missing a delimiter (e.g. an unterminated string).
    pub fn string_literal_inner_text(&self) -> Option<&'tree str> {
        if !self.is_string_literal() {
            return None;
        }
        let child_count = self.node.child_count();
        if child_count < 2 {
            return None;
        }
        let open = self.node.child(0)?;
        let close = self.node.child((child_count - 1) as u32)?;
        let (start, end) = (open.end_byte(), close.start_byte());
        (start <= end).then(|| &self.source[start..end])
    }

    pub fn child_by_field_name(&self, name: &str) -> Option<M2Node<'tree>> {
        let name = match (self.raw_kind(), name) {
            ("new_statement", "type") => "class",
            ("quote_expression", "symbol") => "token",
            _ => name,
        };
        let source = self.source;
        self.node
            .child_by_field_name(name)
            .map(|node| M2Node::new(node, source))
            .or_else(|| {
                (self.raw_kind() == "quote_expression" && name == "specifier")
                    .then(|| self.children().find(M2Node::is_modifier_token))
                    .flatten()
            })
            .or_else(|| {
                (self.raw_kind() == "except_clause" && name == "value")
                    .then(|| {
                        let exception = self.node.child_by_field_name("exception")?;
                        self.named_children()
                            .find(|child| child.node.id() != exception.id())
                    })
                    .flatten()
            })
            .or_else(|| {
                (self.raw_kind() == "try_statement" && name == "value")
                    .then(|| self.named_children().next())
                    .flatten()
            })
    }

    pub fn parent(&self) -> Option<M2Node<'tree>> {
        let source = self.source;
        self.node.parent().map(|node| M2Node::new(node, source))
    }

    pub fn ancestors(self) -> impl Iterator<Item = M2Node<'tree>> {
        iter::successors(self.parent(), |node| node.parent())
    }

    pub fn enclosing_node(
        self,
        predicate: impl Fn(&M2Node<'tree>) -> bool,
    ) -> Option<M2Node<'tree>> {
        iter::once(self)
            .chain(self.ancestors())
            .find(|node| predicate(node))
    }

    pub fn root(self) -> M2Node<'tree> {
        self.ancestors().last().unwrap_or(self)
    }

    pub fn children(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        let source = self.source;
        let mut cursor = self.node.walk();
        self.node
            .children(&mut cursor)
            .map(|node| M2Node::new(node, source))
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn child(&self, index: u32) -> Option<M2Node<'tree>> {
        let source = self.source;
        self.node.child(index).map(|node| M2Node::new(node, source))
    }

    pub fn named_child(&self, index: u32) -> Option<M2Node<'tree>> {
        let source = self.source;
        self.node
            .named_child(index)
            .map(|node| M2Node::new(node, source))
    }

    pub fn named_children(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        let source = self.source;
        let mut cursor = self.node.walk();
        self.node
            .named_children(&mut cursor)
            .map(|node| M2Node::new(node, source))
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Return the smallest descendant spanning the requested point range.
    pub fn descendant_for_point_range(&self, start: Point, end: Point) -> Option<M2Node<'tree>> {
        let source = self.source;
        self.node
            .descendant_for_point_range(start, end)
            .map(|node| M2Node::new(node, source))
    }

    pub fn descendant_for_syntax<Syntax>(&self, syntax: &Syntax) -> Option<M2Node<'tree>>
    where
        Syntax: Reconstruct<TreeSitterNode<'tree, 'tree>> + Spanned,
    {
        let (start, end) = super::syntax_byte_range(syntax)?;
        let source = self.source;
        let mut node = self
            .node
            .descendant_for_byte_range(start, end)
            .map(|node| M2Node::new(node, source))?;
        loop {
            if node.is::<Syntax>() && node.start_byte() <= start && node.end_byte() >= end {
                return Some(node);
            }
            node = node.parent()?;
        }
    }

    /// The semantic element slots of a comma-delimited collection.
    ///
    /// The grammar exposes zero-width `empty_component` nodes for empty comma
    /// slots, so they remain in this iterator and count toward arity. Expressions terminated
    /// by `;` are wrapped in `muted` and do not contribute a value; comments are
    /// syntax extras rather than elements, so both are skipped here.
    pub fn collection_elements(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        self.named_children()
            .filter(|child| !child.is::<MutedCell>() && !child.is_comment())
    }

    /// The final value directly produced by a grouping/cell node.
    ///
    /// A trailing `muted` child means the final expression was silenced and the
    /// container produces no value node. Earlier muted expressions and comments
    /// do not obscure a later ordinary value.
    pub fn final_value_child(&self) -> Option<M2Node<'tree>> {
        self.named_children()
            .filter(|child| !child.is_comment())
            .last()
            .filter(|child| !child.is::<MutedCell>())
    }

    pub fn is_first_collection_element(&self, child: M2Node<'_>) -> bool {
        self.collection_elements()
            .next()
            .is_some_and(|first| first.id() == child.id())
    }

    /// Whether this comma borders a zero-width empty collection slot. Named
    /// sibling APIs can step past zero-width nodes, so compare the parser-owned
    /// slot boundaries with the comma boundaries instead.
    pub fn comma_borders_empty_slot(&self) -> bool {
        if !self.is_comma() {
            return false;
        }
        self.parent().is_some_and(|parent| {
            parent.named_children().any(|sibling| {
                sibling.is_nothing_value()
                    && (sibling.start_byte() == self.end_byte()
                        || sibling.end_byte() == self.start_byte())
            })
        })
    }

    /// Pre-order depth-first traversal of this subtree: the node itself, then
    /// every descendant in source order. Anonymous tokens (punctuation,
    /// delimiters, the inserted `SPACE` application operator) are included, so
    /// callers filter via typed predicates as usual — matching the other walk
    /// methods above. The iterator borrows `&self` (the cursor holds a
    /// tree-sitter borrow), but the yielded `M2Node<'tree>` items outlive the
    /// iterator, like `children()` above.
    pub fn descendants(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        let source = self.source;
        let mut cursor = self.node.walk();
        let mut reached_root = false;
        iter::from_fn(move || {
            if reached_root {
                return None;
            }
            let node = M2Node::new(cursor.node(), source);
            if !cursor.goto_first_child() && !cursor.goto_next_sibling() {
                loop {
                    if !cursor.goto_parent() {
                        reached_root = true;
                        break;
                    }
                    if cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            Some(node)
        })
    }

    pub fn symbols(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        self.descendants().filter(M2Node::is_symbol_like)
    }

    pub fn start_byte(&self) -> usize {
        self.node.start_byte()
    }

    pub fn end_byte(&self) -> usize {
        self.node.end_byte()
    }

    pub fn id(&self) -> SyntaxNodeId {
        SyntaxNodeId(self.node.id())
    }

    pub fn start_position(&self) -> Point {
        self.node.start_position()
    }

    pub fn end_position(&self) -> Point {
        self.node.end_position()
    }

    pub fn child_count(&self) -> usize {
        self.node.child_count()
    }

    pub fn is_error(&self) -> bool {
        self.node.is_error()
    }

    pub fn is_missing(&self) -> bool {
        self.node.is_missing()
    }

    #[cfg(test)]
    pub fn has_error(&self) -> bool {
        self.node.has_error()
    }
}
