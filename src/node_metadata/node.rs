//! Typed access to Tree-sitter nodes and traversals.

use std::{iter, marker::PhantomData};

use tree_sitter::{Node, Point};

use super::{NodeKind, NodeKindMetadata};

trait NodeClass {
    fn accepts(kind: NodeKind) -> bool;
}

#[derive(Debug, Clone, Copy)]
pub struct AnyNodeClass;

impl NodeClass for AnyNodeClass {
    fn accepts(_kind: NodeKind) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyntaxNode<'tree, Class = AnyNodeClass> {
    node: Node<'tree>,
    source: &'tree str,
    pub kind: NodeKind,
    class: PhantomData<Class>,
}

pub type M2Node<'tree> = SyntaxNode<'tree>;

macro_rules! node_classes {
    ($($node:ident => $class:ident accepts $accepts:expr;)*) => {
        $(
            #[derive(Debug, Clone, Copy)]
            pub struct $class;

            impl NodeClass for $class {
                fn accepts(kind: NodeKind) -> bool {
                    ($accepts)(kind)
                }
            }

            pub type $node<'tree> = SyntaxNode<'tree, $class>;

            impl<'tree> From<$node<'tree>> for M2Node<'tree> {
                fn from(node: $node<'tree>) -> Self {
                    node.erase()
                }
            }

            impl<'tree> TryFrom<M2Node<'tree>> for $node<'tree> {
                type Error = M2Node<'tree>;

                fn try_from(node: M2Node<'tree>) -> Result<Self, Self::Error> {
                    node.cast().ok_or(node)
                }
            }
        )*
    };
}

node_classes! {
    BinaryExpressionNode => BinaryExpressionClass accepts
        |kind| kind == NodeKind::BinaryExpression;
    LambdaExpressionNode => LambdaExpressionClass accepts
        |kind| kind == NodeKind::LambdaExpression;
}

impl<'tree> SyntaxNode<'tree> {
    pub fn new(node: Node<'tree>, source: &'tree str) -> Self {
        Self {
            kind: NodeKind::from_str(node.kind()),
            node,
            source,
            class: PhantomData,
        }
    }
}

impl<'tree, Class> SyntaxNode<'tree, Class> {
    fn cast<Target: NodeClass>(self) -> Option<SyntaxNode<'tree, Target>> {
        Target::accepts(self.kind).then(|| self.reclassify())
    }

    fn erase(self) -> M2Node<'tree> {
        self.reclassify()
    }

    fn reclassify<Target>(self) -> SyntaxNode<'tree, Target> {
        SyntaxNode {
            node: self.node,
            source: self.source,
            kind: self.kind,
            class: PhantomData,
        }
    }

    pub fn text(&self) -> &'tree str {
        &self.source[self.node.start_byte()..self.node.end_byte()]
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

    pub fn is(self, kind: NodeKind) -> bool {
        self.kind == kind
    }

    pub fn is_comma(&self) -> bool {
        self.raw_kind() == ","
    }

    pub fn is_semicolon(&self) -> bool {
        self.raw_kind() == ";"
    }

    /// The implicit-application operator: the `SPACE` token tree-sitter inserts
    /// between a function and its juxtaposed argument (`sin x`, `f(x)`).
    pub fn is_implicit_application(&self) -> bool {
        self.raw_kind() == "SPACE"
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
            && matches!(
                self.raw_kind(),
                "if" | "then"
                    | "else"
                    | "from"
                    | "to"
                    | "when"
                    | "do"
                    | "in"
                    | "of"
                    | "list"
                    | "for"
                    | "while"
                    | "break"
                    | "continue"
                    | "return"
                    | "try"
                    | "catch"
                    | "throw"
                    | "time"
                    | "timing"
                    | "elapsedTime"
                    | "elapsedTiming"
                    | "profile"
                    | "shield"
                    | "TEST"
                    | "breakpoint"
                    | "finish"
                    | "new"
                    | "step"
            )
    }

    /// An anonymous binding-modifier keyword token (`global`, `local`, `symbol`,
    /// `threadVariable`, `threadLocal`).
    pub fn is_modifier_token(&self) -> bool {
        !self.node.is_named()
            && matches!(
                self.raw_kind(),
                "global" | "local" | "symbol" | "threadVariable" | "threadLocal"
            )
    }

    /// The `then`/`else` keyword tokens, which open the clauses an `if` indents.
    pub fn is_then_or_else_keyword(&self) -> bool {
        !self.node.is_named() && matches!(self.raw_kind(), "then" | "else")
    }
}

impl<'tree> SyntaxNode<'tree> {
    /// The operator token's source text of a binary expression (`:=`, `=>`, ...),
    /// or `None` if this is not a binary expression. Comparing this *parsed* text
    /// against operator spellings is reading code, not node-type names, so it is
    /// safe and rename-stable.
    pub fn binary_operator(&self) -> Option<&'tree str> {
        self.cast::<BinaryExpressionClass>()?.binary_operator()
    }

    /// The parsed operator and following operand of an infix expression.
    ///
    /// Binary expressions name that operand `right`; lambda expressions name it
    /// `body`. Keeping that grammar distinction here lets consumers handle both
    /// uniformly.
    pub fn infix_operator_and_right_operand(&self) -> Option<(M2Node<'tree>, M2Node<'tree>)> {
        if let Some(binary) = self.cast::<BinaryExpressionClass>() {
            return binary.infix_operator_and_right_operand();
        }
        self.cast::<LambdaExpressionClass>()?
            .infix_operator_and_right_operand()
    }

    /// An assignment expression (`=`, `:=`, `<-`).
    pub fn is_assignment(&self) -> bool {
        self.cast::<BinaryExpressionClass>()
            .is_some_and(|binary| binary.is_assignment())
    }

    /// An option assignment (`key => value`).
    pub fn is_option_assignment(&self) -> bool {
        self.cast::<BinaryExpressionClass>()
            .is_some_and(|binary| binary.is_option_assignment())
    }

    pub fn property_key(&self) -> Option<Self> {
        self.cast::<BinaryExpressionClass>()?.property_key()
    }

    /// An implicit application `f x` / `f(x)`: a binary expression whose operator
    /// is the inserted `SPACE` token.
    pub fn is_space_application(&self) -> bool {
        self.cast::<BinaryExpressionClass>()
            .is_some_and(|binary| binary.is_space_application())
    }
}

impl<'tree> BinaryExpressionNode<'tree> {
    pub fn left(&self) -> Option<M2Node<'tree>> {
        self.child_by_field_name("left")
    }

    pub fn operator(&self) -> Option<M2Node<'tree>> {
        self.child_by_field_name("operator")
    }

    pub fn right(&self) -> Option<M2Node<'tree>> {
        self.child_by_field_name("right")
    }

    pub fn binary_operator(&self) -> Option<&'tree str> {
        self.operator().map(|operator| operator.text())
    }

    pub fn infix_operator_and_right_operand(&self) -> Option<(M2Node<'tree>, M2Node<'tree>)> {
        Some((self.operator()?, self.right()?))
    }

    pub fn is_assignment(&self) -> bool {
        matches!(self.binary_operator(), Some("=" | ":=" | "<-"))
    }

    pub fn is_option_assignment(&self) -> bool {
        self.binary_operator() == Some("=>")
    }

    pub fn property_key(&self) -> Option<M2Node<'tree>> {
        let right = self.right()?;
        match self.binary_operator()? {
            "#" | "#?" if right.kind == NodeKind::StringLiteral => Some(right),
            "." | ".?" if right.kind.is_symbol_like() => Some(right),
            _ => None,
        }
    }

    pub fn is_space_application(&self) -> bool {
        self.operator()
            .is_some_and(|operator| operator.is_implicit_application())
    }
}

impl<'tree> LambdaExpressionNode<'tree> {
    pub fn parameters(&self) -> Option<M2Node<'tree>> {
        self.child_by_field_name("parameters")
    }

    pub fn operator(&self) -> Option<M2Node<'tree>> {
        self.child_by_field_name("operator")
    }

    pub fn body(&self) -> Option<M2Node<'tree>> {
        self.child_by_field_name("body")
    }

    pub fn infix_operator_and_right_operand(&self) -> Option<(M2Node<'tree>, M2Node<'tree>)> {
        Some((self.operator()?, self.body()?))
    }
}

impl<'tree, Class> SyntaxNode<'tree, Class> {
    /// Whether this node is the `operator` field of its own parent.
    pub fn is_operator(&self) -> bool {
        self.parent()
            .and_then(|parent| parent.child_by_field_name("operator"))
            .is_some_and(|operator| operator.id() == self.node.id())
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
        if self.kind != NodeKind::StringLiteral {
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
        let source = self.source;
        self.node
            .child_by_field_name(name)
            .map(|node| M2Node::new(node, source))
    }

    pub fn parent(&self) -> Option<M2Node<'tree>> {
        let source = self.source;
        self.node.parent().map(|node| M2Node::new(node, source))
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

    /// The semantic element slots of a comma-delimited collection.
    ///
    /// Grammar 4 exposes zero-width `null` nodes for empty comma slots, so they
    /// remain in this iterator and count toward arity. Expressions terminated
    /// by `;` are wrapped in `muted` and do not contribute a value; comments are
    /// syntax extras rather than elements, so both are skipped here.
    pub fn collection_elements(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        self.named_children()
            .filter(|child| child.kind != NodeKind::Muted && !child.kind.is_comment())
    }

    /// The final value directly produced by a grouping/cell node.
    ///
    /// A trailing `muted` child means the final expression was silenced and the
    /// container produces no value node. Earlier muted expressions and comments
    /// do not obscure a later ordinary value.
    pub fn final_value_child(&self) -> Option<M2Node<'tree>> {
        self.named_children()
            .filter(|child| !child.kind.is_comment())
            .last()
            .filter(|child| child.kind != NodeKind::Muted)
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
                sibling.kind.is_nothing_value()
                    && (sibling.start_byte() == self.end_byte()
                        || sibling.end_byte() == self.start_byte())
            })
        })
    }

    /// Pre-order depth-first traversal of this subtree: the node itself, then
    /// every descendant in source order. Anonymous tokens (punctuation,
    /// delimiters, the inserted `SPACE` application operator) are included, so
    /// callers filter via `NodeKind` predicates as usual — matching the other
    /// walk methods above. The iterator borrows `&self` (the cursor holds a
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

    pub fn start_byte(&self) -> usize {
        self.node.start_byte()
    }

    pub fn end_byte(&self) -> usize {
        self.node.end_byte()
    }

    pub fn id(&self) -> usize {
        self.node.id()
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
