//! Closed grammar-kind mapping and semantic syntax categories.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    SourceFile,
    Cell,
    Symbol,
    QuotedKeyword,
    IntegerLiteral,
    FloatLiteral,
    StringLiteral,
    RawStringLiteral,
    Array,
    Sequence,
    NakedSequence,
    ParenthesizedExpression,
    List,
    AngleBarList,
    Muted,
    EmptyComponent,
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
            "raw_string_literal" => Self::RawStringLiteral,
            "array" => Self::Array,
            "sequence" => Self::Sequence,
            "naked_sequence" => Self::NakedSequence,
            "parenthesized_expression" => Self::ParenthesizedExpression,
            "list" => Self::List,
            "angle_bar_list" => Self::AngleBarList,
            "muted" => Self::Muted,
            "empty_component" => Self::EmptyComponent,
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
    fn is_string_literal(&self) -> bool;
    fn is_collection_expression(&self) -> bool;
    fn is_sequence(&self) -> bool;
    fn is_nothing_value(&self) -> bool;
    fn is_comment(&self) -> bool;
    fn is_control_transfer(&self) -> bool;
    fn is_value_expression(&self) -> bool;
}

impl NodeKindMetadata for NodeKind {
    fn is_symbol_like(&self) -> bool {
        matches!(*self, Self::Symbol | Self::QuotedKeyword)
    }

    fn is_literal(&self) -> bool {
        matches!(
            *self,
            Self::IntegerLiteral
                | Self::FloatLiteral
                | Self::StringLiteral
                | Self::RawStringLiteral
        )
    }

    fn is_string_literal(&self) -> bool {
        matches!(*self, Self::StringLiteral | Self::RawStringLiteral)
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
        matches!(*self, Self::Muted | Self::EmptyComponent)
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

    fn is_value_expression(&self) -> bool {
        self.is_literal()
            || self.is_collection_expression()
            || self.is_control_transfer()
            || matches!(
                *self,
                Self::Symbol
                    | Self::NakedSequence
                    | Self::Cell
                    | Self::ParenthesizedExpression
                    | Self::IfStatement
                    | Self::WhileStatement
                    | Self::ForStatement
                    | Self::NewStatement
                    | Self::TryStatement
                    | Self::DebugClause
                    | Self::LambdaExpression
                    | Self::BinaryExpression
                    | Self::PrefixExpression
                    | Self::PostfixExpression
            )
    }
}
