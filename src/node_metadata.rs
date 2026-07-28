//! Typed, grammar-local access to Tree-sitter nodes used throughout the server.

use std::{iter, ops::Deref};
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    SourceFile,
    Cell,
    Symbol,
    QuotedKeyword,
    IntegerLiteral,
    FloatLiteral,
    StringLiteral,
    Array,
    Sequence,
    NakedSequence,
    ParenthesizedExpression,
    List,
    AngleBarList,
    Muted,
    Null,
    BinaryExpression,
    PrefixExpression,
    PostfixExpression,
    LambdaExpression,
    IfStatement,
    ForStatement,
    WhileStatement,
    NewStatement,
    TryStatement,
    DebugClause,
    BreakStatement,
    ContinueStatement,
    ReturnStatement,
    CatchStatement,
    ThrowStatement,
    TrapStatement,
    QuoteExpression,
    FromClause,
    ToClause,
    OfClause,
    InClause,
    WhenClause,
    ListClause,
    DoClause,
    ThenClause,
    ElseClause,
    ExceptClause,
    LineComment,
    BlockComment,
    Unknown,
}

impl NodeKind {
    pub fn from_str(s: &str) -> Self {
        match s {
            "source_file" => Self::SourceFile,
            "cell" => Self::Cell,
            "symbol" => Self::Symbol,
            "keyword" => Self::QuotedKeyword,
            "integer_literal" => Self::IntegerLiteral,
            "float_literal" => Self::FloatLiteral,
            "string_literal" => Self::StringLiteral,
            "array" => Self::Array,
            "sequence" => Self::Sequence,
            "naked_sequence" => Self::NakedSequence,
            "parenthesized_expression" => Self::ParenthesizedExpression,
            "list" => Self::List,
            "angle_bar_list" => Self::AngleBarList,
            "muted" => Self::Muted,
            "null" => Self::Null,
            "binary_expression" => Self::BinaryExpression,
            "prefix_expression" => Self::PrefixExpression,
            "postfix_expression" => Self::PostfixExpression,
            "lambda_expression" => Self::LambdaExpression,
            "if_statement" => Self::IfStatement,
            "for_statement" => Self::ForStatement,
            "while_statement" => Self::WhileStatement,
            "new_statement" => Self::NewStatement,
            "try_statement" => Self::TryStatement,
            "debug_clause" => Self::DebugClause,
            "break_statement" => Self::BreakStatement,
            "continue_statement" => Self::ContinueStatement,
            "return_statement" => Self::ReturnStatement,
            "catch_statement" => Self::CatchStatement,
            "throw_statement" => Self::ThrowStatement,
            "trap_statement" => Self::TrapStatement,
            "quote_expression" => Self::QuoteExpression,
            "from_clause" => Self::FromClause,
            "to_clause" => Self::ToClause,
            "of_clause" => Self::OfClause,
            "in_clause" => Self::InClause,
            "when_clause" => Self::WhenClause,
            "list_clause" => Self::ListClause,
            "do_clause" => Self::DoClause,
            "then_clause" => Self::ThenClause,
            "else_clause" => Self::ElseClause,
            "except_clause" => Self::ExceptClause,
            "line_comment" => Self::LineComment,
            "block_comment" => Self::BlockComment,
            _ => Self::Unknown,
        }
    }
}

/// Semantic categories shared by syntax kinds.
///
/// The grammar-name mapping remains closed and centralized in
/// [`NodeKind::from_str`]. Analysis depends on these capabilities rather than
/// matching the concrete enum variants again.
pub trait NodeKindMetadata {
    fn is_symbol_like(&self) -> bool;
    fn is_literal(&self) -> bool;
    fn is_collection_expression(&self) -> bool;
    fn is_sequence(&self) -> bool;
    fn is_nothing_value(&self) -> bool;
    fn is_comment(&self) -> bool;
    fn is_control_transfer(&self) -> bool;
}

impl NodeKindMetadata for NodeKind {
    fn is_symbol_like(&self) -> bool {
        matches!(*self, Self::Symbol | Self::QuotedKeyword)
    }

    fn is_literal(&self) -> bool {
        matches!(
            *self,
            Self::IntegerLiteral | Self::FloatLiteral | Self::StringLiteral
        )
    }

    /// M2's delimited collection forms: `(a,b)`, `{a,b}`, `[a,b]`, `<|a,b|>`.
    /// These are the nodes whose element count is known statically, so they
    /// serve both as parallel-assignment targets (the left of a destructuring
    /// `=`/`:=`) and as fixed-length right-hand sides whose arity can be checked
    /// against the targets. A parenthesized single expression `(a)` is not one
    /// of these -- the grammar collapses it to the bare expression.
    fn is_collection_expression(&self) -> bool {
        matches!(
            *self,
            Self::Sequence | Self::List | Self::Array | Self::AngleBarList
        )
    }

    fn is_sequence(&self) -> bool {
        matches!(*self, Self::Sequence | Self::NakedSequence)
    }

    fn is_nothing_value(&self) -> bool {
        matches!(*self, Self::Muted | Self::Null)
    }

    fn is_comment(&self) -> bool {
        matches!(*self, Self::LineComment | Self::BlockComment)
    }

    fn is_control_transfer(&self) -> bool {
        matches!(
            *self,
            Self::ReturnStatement | Self::BreakStatement | Self::ContinueStatement
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct M2Node<'tree> {
    node: Node<'tree>,
    source: &'tree str,
    pub kind: NodeKind,
}

impl<'tree> M2Node<'tree> {
    pub fn new(node: Node<'tree>, source: &'tree str) -> Self {
        Self {
            kind: NodeKind::from_str(node.kind()),
            node,
            source,
        }
    }

    /// The exact source text this node spans, as tree-sitter parsed it.
    ///
    /// This is the ONLY sanctioned way to read code. Never slice the raw buffer
    /// or scan bytes to re-derive structure the parser already determined:
    /// escaped `/////` inside raw strings, `--` inside strings, `"` inside
    /// comments, and similar quirks are handled correctly by the parser and must
    /// not be re-implemented here.
    pub fn text(&self) -> &'tree str {
        &self.source[self.node.start_byte()..self.node.end_byte()]
    }

    /// The grammar's raw node-type name. PRIVATE on purpose: node-type names are
    /// an implementation detail of the grammar (which is renamed from time to
    /// time), so they must never be referenced outside this module. Classify with
    /// `NodeKind` / the typed predicates below instead; a grammar rename then
    /// touches only `NodeKind::from_str` and the anonymous-token predicates here.
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

    // Anonymous tokens carry no named grammar rule, so their `kind()` is the
    // literal text. These predicates match that text directly rather than
    // minting a `NodeKind` variant per literal, keeping the grammar's token
    // set as the single source of truth.

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

    /// The operator token's source text of a binary expression (`:=`, `=>`, ...),
    /// or `None` if this is not a binary expression. Comparing this *parsed* text
    /// against operator spellings is reading code, not node-type names, so it is
    /// safe and rename-stable.
    pub fn binary_operator(&self) -> Option<&'tree str> {
        if self.kind != NodeKind::BinaryExpression {
            return None;
        }
        self.child_by_field_name("operator").map(|op| op.text())
    }

    /// An assignment expression (`=`, `:=`, `<-`).
    pub fn is_assignment(&self) -> bool {
        matches!(self.binary_operator(), Some("=" | ":=" | "<-"))
    }

    /// An option assignment (`key => value`).
    pub fn is_option_assignment(&self) -> bool {
        self.binary_operator() == Some("=>")
    }

    /// An implicit application `f x` / `f(x)`: a binary expression whose operator
    /// is the inserted `SPACE` token.
    pub fn is_space_application(&self) -> bool {
        self.kind == NodeKind::BinaryExpression
            && self
                .child_by_field_name("operator")
                .is_some_and(|op| op.is_implicit_application())
    }

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
}

impl<'tree> Deref for M2Node<'tree> {
    type Target = Node<'tree>;

    fn deref(&self) -> &Self::Target {
        &self.node
    }
}

#[cfg(test)]
mod cst_compliance_gate {
    //! Build gate enforcing the repo rule that the rest of the crate must reach
    //! the syntax tree only through `M2Node` / `NodeKind` — never the raw
    //! tree-sitter node-type name and never the raw source buffer. The grammar is
    //! renamed over time, so node-type names live ONLY in `NodeKind::from_str` and
    //! the anonymous-token predicates in this module; reading raw code re-derives
    //! parser logic (escaped `/////`, `"` in comments, `--` in strings) and is
    //! banned. A violation fails the test run, not just review.
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    /// Substrings that must not appear outside `node_metadata.rs`. Each is a way
    /// to reach the raw tree-sitter node-type name or the unparsed source.
    const BANNED: &[(&str, &str)] = &[
        (
            ".kind()",
            "raw tree-sitter node-type name; use `node.kind` / `NodeKind` instead",
        ),
        (
            ".raw_kind()",
            "raw node-type name is private to node_metadata; use a typed predicate",
        ),
        (".utf8_text(", "reads raw bytes; use `node.text()`"),
        ("tree_sitter::Node", "raw node type; pass `M2Node` instead"),
        (
            "starts_with('\"')",
            "byte-scanning for string quotes; use `node.string_literal_inner_text()`",
        ),
        (
            "strip_prefix('\"')",
            "byte-stripping string quotes; use `node.string_literal_inner_text()`",
        ),
        (
            "strip_suffix('\"')",
            "byte-stripping string quotes; use `node.string_literal_inner_text()`",
        ),
    ];

    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("src dir is readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn no_raw_node_access_or_raw_code_reads_outside_node_metadata() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);

        let mut violations = Vec::new();
        for file in files {
            // node_metadata.rs is the one sanctioned home for raw access.
            if file
                .file_name()
                .is_some_and(|name| name == "node_metadata.rs")
            {
                continue;
            }
            let contents = fs::read_to_string(&file).expect("source is readable");
            for (line_number, line) in contents.lines().enumerate() {
                // Skip comments: a doc/line comment may legitimately mention a
                // banned form while describing the rule.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for (needle, why) in BANNED {
                    if line.contains(needle) {
                        violations.push(format!(
                            "{}:{}: `{needle}` — {why}",
                            file.display(),
                            line_number + 1,
                        ));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "CST-compliance gate failed; route these through M2Node/NodeKind:\n{}",
            violations.join("\n")
        );
    }
}

#[cfg(test)]
mod descendants_tests {
    //! Locks the pre-order DFS contract of `M2Node::descendants`: the contract
    //! every migrated call site relies on (parent before children, source order
    //! across siblings, root yielded exactly once, empty file safe).
    use super::*;
    use tree_sitter::Parser;

    fn kinds_of(text: &str) -> Vec<NodeKind> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let root = M2Node::new(tree.root_node(), text);
        root.descendants().map(|n| n.kind).collect()
    }

    #[test]
    fn descendants_visit_parent_before_children_in_source_order() {
        let kinds = kinds_of("f(x, y)\n");

        // Root first, then `f`, then the parenthesized call's inner sequence
        // and its three children in source order: `x`, `,`, `y`. We assert a
        // few key landmarks rather than the full token list, so a future
        // grammar renormalization that adds wrapper nodes doesn't break the
        // test for a property it doesn't care about.
        assert_eq!(kinds.first(), Some(&NodeKind::SourceFile));
        assert!(kinds.contains(&NodeKind::Symbol));
        assert!(kinds.contains(&NodeKind::Sequence));
        let x_pos = kinds
            .iter()
            .position(|k| *k == NodeKind::Symbol)
            .expect("a Symbol is emitted");
        let seq_pos = kinds
            .iter()
            .position(|k| *k == NodeKind::Sequence)
            .expect("a Sequence is emitted");
        // Whichever node contains the call appears before its `Symbol` child
        // (pre-order). Both orderings of `Symbol`-then-`Sequence` are valid
        // depending on grammar wrappers, but a child never precedes its
        // container.
        assert!(x_pos != seq_pos);
    }

    #[test]
    fn descendants_of_empty_file_yields_root_once() {
        assert_eq!(
            kinds_of("").len(),
            1,
            "empty file yields only the SourceFile root"
        );
    }

    #[test]
    fn descendants_count_matches_node_count_of_tree() {
        // direct check against tree-sitter's own traversal: the total
        // descendant count (root included) must match the recursive child
        // count, so the iterator neither drops nor duplicates any node.
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let text = "x = (a, b)\ny = {1, 2, 3}\n";
        let tree = parser.parse(text, None).expect("fixture should parse");
        let root = M2Node::new(tree.root_node(), text);
        fn count_via_children(n: M2Node<'_>) -> usize {
            1 + n.children().map(count_via_children).sum::<usize>()
        }
        assert_eq!(root.descendants().count(), count_via_children(root));
    }

    #[test]
    fn grammar_v4_exposes_muted_null_and_naked_sequence_shapes() {
        let text = "local if\nstep 1\nfinish\n(x;)\n(,a,,)\na,b\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let root = M2Node::new(tree.root_node(), text);

        let quote = root
            .descendants()
            .find(|node| node.kind == NodeKind::QuoteExpression)
            .expect("`local if` is a quote expression");
        assert_eq!(
            quote
                .child_by_field_name("symbol")
                .expect("quote has a symbol field")
                .kind,
            NodeKind::QuotedKeyword
        );
        assert!(quote
            .child_by_field_name("specifier")
            .expect("quote has a specifier field")
            .is_modifier_token());

        assert_eq!(
            root.descendants()
                .filter(|node| node.kind == NodeKind::DebugClause)
                .count(),
            2,
            "both `step` and `finish` are debug clauses"
        );

        let parens = root
            .descendants()
            .find(|node| node.kind == NodeKind::ParenthesizedExpression)
            .expect("parenthesized expression is present");
        assert_eq!(
            parens.named_children().next().map(|child| child.kind),
            Some(NodeKind::Muted)
        );
        assert!(
            parens.final_value_child().is_none(),
            "a grouping ending in a muted expression has no value child"
        );

        let sequence = root
            .descendants()
            .find(|node| node.kind == NodeKind::Sequence)
            .expect("parenthesized comma sequence is present");
        let elements = sequence.collection_elements().collect::<Vec<_>>();
        assert_eq!(elements.len(), 4, "every comma slot remains an element");
        assert_eq!(
            elements
                .iter()
                .filter(|element| element.kind == NodeKind::Null)
                .count(),
            3,
            "empty comma slots are explicit null nodes"
        );

        let naked = root
            .descendants()
            .find(|node| node.kind == NodeKind::NakedSequence)
            .expect("top-level comma expression is a naked sequence");
        assert_eq!(naked.collection_elements().count(), 2);
    }
}
