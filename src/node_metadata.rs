use std::ops::Deref;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    SourceFile,
    Cell,
    Symbol,
    ResolvedSymbol,
    IntegerLiteral,
    FloatLiteral,
    StringLiteral,
    EscapeSequence,
    RawStringEscape,
    Array,
    Sequence,
    List,
    AngleBarList,
    BinaryExpression,
    PrefixExpression,
    PostfixExpression,
    LambdaExpression,
    IfStatement,
    ForStatement,
    WhileStatement,
    NewStatement,
    TryStatement,
    StepStatement,
    DebugClause,
    BreakStatement,
    ContinueStatement,
    ReturnStatement,
    CatchStatement,
    ThrowStatement,
    TrapStatement,
    Cobinding,
    LocalCobinding,
    GlobalCobinding,
    ThreadCobinding,
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
    SilencedExpression,
    // Anonymous tokens (named=false in the grammar) the formatter inspects by
    // kind: the implicit-application operator and the sequence separators.
    Space,
    Comma,
    Semicolon,
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
            "resolved_symbol" => Self::ResolvedSymbol,
            "integer_literal" => Self::IntegerLiteral,
            "float_literal" => Self::FloatLiteral,
            "string_literal" => Self::StringLiteral,
            "escape_sequence" => Self::EscapeSequence,
            "raw_string_escape" => Self::RawStringEscape,
            "array" => Self::Array,
            "sequence" => Self::Sequence,
            "list" => Self::List,
            "angle_bar_list" => Self::AngleBarList,
            "binary_expression" => Self::BinaryExpression,
            "prefix_expression" => Self::PrefixExpression,
            "postfix_expression" => Self::PostfixExpression,
            "lambda_expression" => Self::LambdaExpression,
            "if_statement" => Self::IfStatement,
            "for_statement" => Self::ForStatement,
            "while_statement" => Self::WhileStatement,
            "new_statement" => Self::NewStatement,
            "try_statement" => Self::TryStatement,
            "step_statement" => Self::StepStatement,
            "debug_clause" => Self::DebugClause,
            "break_statement" => Self::BreakStatement,
            "continue_statement" => Self::ContinueStatement,
            "return_statement" => Self::ReturnStatement,
            "catch_statement" => Self::CatchStatement,
            "throw_statement" => Self::ThrowStatement,
            "trap_statement" => Self::TrapStatement,
            "cobinding" => Self::Cobinding,
            "local_cobinding" => Self::LocalCobinding,
            "global_cobinding" => Self::GlobalCobinding,
            "thread_cobinding" => Self::ThreadCobinding,
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
            "silenced_expression" => Self::SilencedExpression,
            "SPACE" => Self::Space,
            "," => Self::Comma,
            ";" => Self::Semicolon,
            "line_comment" => Self::LineComment,
            "block_comment" => Self::BlockComment,
            _ => Self::Unknown,
        }
    }

    pub fn is_symbol_like(self) -> bool {
        matches!(self, Self::Symbol | Self::ResolvedSymbol)
    }

    pub fn is_literal(self) -> bool {
        matches!(
            self,
            Self::IntegerLiteral | Self::FloatLiteral | Self::StringLiteral
        )
    }

    pub fn is_method_installation_target(self) -> bool {
        matches!(
            self,
            Self::BinaryExpression | Self::PrefixExpression | Self::PostfixExpression
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct M2Node<'tree> {
    node: tree_sitter::Node<'tree>,
    pub kind: NodeKind,
}

impl<'tree> M2Node<'tree> {
    pub fn new(node: tree_sitter::Node<'tree>) -> Self {
        Self {
            kind: NodeKind::from_str(node.kind()),
            node,
        }
    }

    pub fn raw_kind(&self) -> &'tree str {
        self.node.kind()
    }

    pub fn is(self, kind: NodeKind) -> bool {
        self.kind == kind
    }

    pub fn child_by_field_name(&self, name: &str) -> Option<M2Node<'tree>> {
        self.node.child_by_field_name(name).map(M2Node::new)
    }

    pub fn parent(&self) -> Option<M2Node<'tree>> {
        self.node.parent().map(M2Node::new)
    }

    pub fn children(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        let mut cursor = self.node.walk();
        self.node
            .children(&mut cursor)
            .map(M2Node::new)
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn child(&self, index: u32) -> Option<M2Node<'tree>> {
        self.node.child(index).map(M2Node::new)
    }

    pub fn named_child(&self, index: u32) -> Option<M2Node<'tree>> {
        self.node.named_child(index).map(M2Node::new)
    }

    pub fn named_children(&self) -> impl Iterator<Item = M2Node<'tree>> + '_ {
        let mut cursor = self.node.walk();
        self.node
            .named_children(&mut cursor)
            .map(M2Node::new)
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn inner(&self) -> tree_sitter::Node<'tree> {
        self.node
    }

    pub fn start_byte(&self) -> usize {
        self.node.start_byte()
    }

    pub fn end_byte(&self) -> usize {
        self.node.end_byte()
    }
}

impl<'tree> From<tree_sitter::Node<'tree>> for M2Node<'tree> {
    fn from(node: tree_sitter::Node<'tree>) -> Self {
        Self::new(node)
    }
}

impl<'tree> Deref for M2Node<'tree> {
    type Target = tree_sitter::Node<'tree>;

    fn deref(&self) -> &Self::Target {
        &self.node
    }
}
