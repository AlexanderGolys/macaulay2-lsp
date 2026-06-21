use tower_lsp::lsp_types::{
    DocumentFormattingOptions, FoldingRange, FoldingRangeProviderCapability, OneOf, TextEdit,
};
use tree_sitter::Parser;

use crate::node_metadata::{M2Node, NodeKind};
use crate::util::full_document_range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOptions {
    indent: String,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            indent: "    ".to_string(),
        }
    }
}

impl FormatOptions {
    pub fn new(tab_size: u32, insert_spaces: bool) -> Self {
        let indent = if insert_spaces {
            " ".repeat(tab_size.max(1) as usize)
        } else {
            "\t".to_string()
        };

        Self { indent }
    }
}

pub(crate) fn document_formatting_provider_capability(
) -> Option<OneOf<bool, DocumentFormattingOptions>> {
    Some(OneOf::Left(true))
}

pub(crate) fn folding_range_provider_capability() -> Option<FoldingRangeProviderCapability> {
    Some(FoldingRangeProviderCapability::Simple(true))
}

pub(crate) fn document_formatting_text_edits(
    text: &str,
    tab_size: u32,
    insert_spaces: bool,
) -> Vec<TextEdit> {
    let formatted =
        format_document_text_with_options(text, &FormatOptions::new(tab_size, insert_spaces));
    if formatted == text {
        return Vec::new();
    }

    vec![TextEdit {
        range: full_document_range(text),
        new_text: formatted,
    }]
}

pub(crate) fn folding_ranges(text: &str) -> Vec<FoldingRange> {
    folding_ranges_for_text(text)
        .into_iter()
        .map(|range| FoldingRange {
            start_line: range.start_line,
            start_character: None,
            end_line: range.end_line,
            end_character: None,
            kind: None,
            collapsed_text: None,
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FormatEdit {
    start_byte: usize,
    end_byte: usize,
    replacement: &'static str,
}

#[cfg(test)]
pub fn format_document_text(text: &str) -> String {
    format_document_text_with_options(text, &FormatOptions::default())
}

pub fn format_document_text_with_options(text: &str, options: &FormatOptions) -> String {
    // Basic spacing only, and provably string/comment-safe: every edit either
    // adjusts whitespace adjacent to a real operator/punctuation node
    // (`normalize_whitespace`) or rebuilds a line's leading indentation
    // (`reindent_from_tree`). Neither rewrites token text, so string and comment
    // contents are never modified. No reflow/line-breaking, no byte-scanning.
    let formatted = normalize_whitespace(text);
    let mut formatted = reindent_from_tree(&formatted, options);

    if text.ends_with('\n') {
        formatted.push('\n');
    }

    formatted
}

/// Re-indent every line of already-normalized `text` from a fresh parse: parse #2
/// of the two-parse design (parse #1 ran in `normalize_whitespace`). Each line's
/// leading whitespace is rebuilt as `options.indent.repeat(depth)` from the
/// tree-derived depth; lines inside a multiline string/raw-string are emitted
/// verbatim so their interior spacing is preserved.
fn reindent_from_tree(text: &str, options: &FormatOptions) -> String {
    let layout = TreeIndentLayout::build(text);
    text.lines()
        .enumerate()
        .map(|(row, line)| {
            if layout.is_literal_line(row) {
                return line.to_string();
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return String::new();
            }
            let mut indented = options.indent.repeat(layout.depth(row));
            indented.push_str(trimmed);
            indented
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn folding_ranges_for_text(text: &str) -> Vec<FormatFoldRange> {
    let layout = TreeIndentLayout::build(text);
    let indented_lines = text
        .lines()
        .enumerate()
        .filter_map(|(row, line)| {
            (!line.trim().is_empty()).then_some(IndentedLine {
                line: row as u32,
                depth: layout.depth(row),
            })
        })
        .collect::<Vec<_>>();

    collect_indent_fold_ranges(&indented_lines)
}

/// Per-line indentation depths derived from a tree-sitter parse of normalized
/// text, plus the rows that lie inside a multiline string and must be left
/// verbatim. `depth(row) = bracket_depth(row) + continuation(row)`.
struct TreeIndentLayout {
    depths: Vec<usize>,
    literal_rows: Vec<bool>,
}

impl TreeIndentLayout {
    fn build(text: &str) -> Self {
        let line_count = text.lines().count().max(1);
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_macaulay2::language())
            .is_err()
        {
            return Self {
                depths: vec![0; line_count],
                literal_rows: vec![false; line_count],
            };
        }
        let Some(tree) = parser.parse(text, None) else {
            return Self {
                depths: vec![0; line_count],
                literal_rows: vec![false; line_count],
            };
        };

        let root = M2Node::new(tree.root_node());
        let brackets = collect_bracket_groups(root, line_count);
        let literal_rows = collect_literal_rows(root, line_count);
        let line_leads = line_leading_blank(text, line_count);

        let depths = (0..line_count)
            .map(|row| {
                bracket_depth(row, &brackets, &line_leads)
                    + line_continuation(row, root, text, &line_leads)
            })
            .collect();

        Self {
            depths,
            literal_rows,
        }
    }

    fn depth(&self, row: usize) -> usize {
        self.depths.get(row).copied().unwrap_or(0)
    }

    fn is_literal_line(&self, row: usize) -> bool {
        self.literal_rows.get(row).copied().unwrap_or(false)
    }
}

/// A multiline bracket node, keyed by the row it opens on. Brackets that open on
/// the same row collapse to one indent level, so the `open_row` is the group id.
#[derive(Debug, Clone, Copy)]
struct BracketGroup {
    open_row: usize,
    close_row: usize,
    /// Column of the closing delimiter on `close_row`; its closer "begins the
    /// line" when this equals the line's leading-whitespace length.
    close_col: usize,
}

/// Collect every multiline bracket node (`(…)`, `{…}`, `[…]`, `<|…|>`) spanning
/// more than one row, keyed by open/close row for the depth computation. The
/// closer's width (`)` is 1, `|>` is 2) does not matter; only its start column.
fn collect_bracket_groups(root: M2Node<'_>, line_count: usize) -> Vec<BracketGroup> {
    let mut groups = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // A multiline parenthesized block `(\n …\n)` indents its body like a
        // collection, though it is not a collection value.
        if node.kind.is_collection_expression() || node.kind == NodeKind::ParenthesizedExpression {
            let open_row = node.start_position().row;
            let close_position = node.end_position();
            if open_row < close_position.row {
                groups.push(BracketGroup {
                    open_row,
                    close_row: close_position.row,
                    close_col: close_position
                        .column
                        .saturating_sub(closer_width(node.kind)),
                });
            }
        }
        if node.is_error() {
            collect_unclosed_error_brackets(node, line_count, &mut groups);
        }
        stack.extend(node.children());
    }
    groups
}

/// Recover unclosed opener tokens that tree-sitter leaves loose inside an ERROR
/// node (e.g. a `(` at EOF with no `)`). Each unmatched opener still indents its
/// body, so it opens a group from its row to the last line of the document.
fn collect_unclosed_error_brackets(
    error: M2Node<'_>,
    line_count: usize,
    groups: &mut Vec<BracketGroup>,
) {
    let mut open_rows = Vec::new();
    for child in error.children() {
        if child.is_opening_delimiter() {
            open_rows.push(child.start_position().row);
        } else if child.is_closing_delimiter() {
            open_rows.pop();
        }
    }
    let last_row = line_count.saturating_sub(1);
    for open_row in open_rows {
        if open_row < last_row {
            groups.push(BracketGroup {
                open_row,
                close_row: last_row,
                close_col: usize::MAX,
            });
        }
    }
}

/// The byte width of a bracket node's closing delimiter: `|>` is two, all others
/// (`)`, `}`, `]`) are one.
fn closer_width(kind: NodeKind) -> usize {
    if kind == NodeKind::AngleBarList {
        2
    } else {
        1
    }
}

/// Rows whose start lies strictly inside a multiline string node (the second and
/// later rows of a `"…"` or `///…///` literal). Their contents are preserved
/// verbatim, never re-indented.
fn collect_literal_rows(root: M2Node<'_>, line_count: usize) -> Vec<bool> {
    let mut literal = vec![false; line_count];
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is(NodeKind::StringLiteral) {
            let start_row = node.start_position().row;
            let end_row = node.end_position().row;
            for row in (start_row + 1)..=end_row {
                if let Some(slot) = literal.get_mut(row) {
                    *slot = true;
                }
            }
        }
        stack.extend(node.children());
    }
    literal
}

/// The leading-whitespace length (in bytes) of each line, used purely as layout
/// to test whether a closer is the first non-whitespace token on its line.
fn line_leading_blank(text: &str, line_count: usize) -> Vec<usize> {
    let mut leads = vec![0; line_count];
    for (row, line) in text.lines().enumerate() {
        if let Some(slot) = leads.get_mut(row) {
            *slot = line.len() - line.trim_start().len();
        }
    }
    leads
}

/// `bracket_depth(row)` = (distinct open-rows of multiline brackets currently
/// open across this row) minus (distinct groups whose closer begins this row).
/// The latter dedents closing-delimiter lines back to the opener level. After
/// `normalize_multiline_closing_delimiters` a multiline closer is on its own
/// line, so any multiline group closing on `row` has its closer leading.
fn bracket_depth(row: usize, brackets: &[BracketGroup], line_leads: &[usize]) -> usize {
    let leading_blank = line_leads.get(row).copied().unwrap_or(0);
    let mut active = Vec::new();
    let mut leading_closed = Vec::new();
    for group in brackets {
        if group.open_row < row && row <= group.close_row && !active.contains(&group.open_row) {
            active.push(group.open_row);
        }
        if group.close_row == row
            && group.close_col == leading_blank
            && !leading_closed.contains(&group.open_row)
        {
            leading_closed.push(group.open_row);
        }
    }
    active.len().saturating_sub(leading_closed.len())
}

/// `continuation(row)` is a flat +1 for a line that continues an expression or a
/// clause body broken onto a later line (see the rule cases inline).
fn line_continuation(row: usize, root: M2Node<'_>, text: &str, line_leads: &[usize]) -> usize {
    let Some(first) = first_leaf_on_row(root, row) else {
        return 0;
    };

    // (a) The first token is the start of the right operand of a binary
    // expression whose operator dangled on an earlier row (`a +\nb`).
    if is_right_operand_first_token(first, row, text) {
        return 1;
    }

    // (b) The first token starts a clause body whose keyword is on an earlier row
    // (`then\n  f(x)`, `else\n  c`, `do\n  body`).
    if is_clause_body_first_token(first, row) {
        return 1;
    }

    // (c) The first token is an `else`/`then` keyword whose controlling `if` is
    // not standalone (a ternary `x := if … then\n … else …`); a standalone `if`
    // aligns its `else` via bracket_depth instead.
    if is_dangling_clause_keyword(first, row, text, line_leads) {
        return 1;
    }

    0
}

/// The leftmost leaf node starting on `row`.
fn first_leaf_on_row(root: M2Node<'_>, row: usize) -> Option<M2Node<'_>> {
    let mut best: Option<M2Node<'_>> = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.start_position().row > row || node.end_position().row < row {
            continue;
        }
        let is_leaf = node.child(0).is_none();
        if is_leaf && node.start_position().row == row && node.start_byte() < node.end_byte() {
            best = match best {
                Some(current) if current.start_byte() <= node.start_byte() => Some(current),
                _ => Some(node),
            };
        }
        stack.extend(node.children());
    }
    best
}

/// Whether `node` is the first token of the right operand of a binary expression
/// whose operator dangled on a row before `row`. Only spaced operators carry a
/// continuation: a compact operator like `*` left at line end (`a*\nb`) does not
/// indent its continuation, matching the line-final-operator spacing pass.
fn is_right_operand_first_token(node: M2Node<'_>, row: usize, text: &str) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.is(NodeKind::BinaryExpression) {
            if let (Some(operator), Some(right)) = (
                parent.child_by_field_name("operator"),
                parent.child_by_field_name("right"),
            ) {
                let operator_text = &text[operator.start_byte()..operator.end_byte()];
                if right.start_byte() == node.start_byte()
                    && operator.start_position().row < row
                    && is_spaced_line_final_operator(operator_text)
                {
                    return true;
                }
            }
        }
        current = parent;
    }
    false
}

/// Whether `node` is the first token of a clause body (`then`/`else`/`do`/`list`)
/// whose clause keyword is on a row before `row`.
fn is_clause_body_first_token(node: M2Node<'_>, row: usize) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind,
            NodeKind::ThenClause | NodeKind::ElseClause | NodeKind::DoClause | NodeKind::ListClause
        ) {
            if let (Some(keyword), Some(body)) = (parent.child(0), parent.named_child(0)) {
                if body.start_byte() == node.start_byte() && keyword.start_position().row < row {
                    return true;
                }
            }
        }
        current = parent;
    }
    false
}

/// Whether `node` is an `else`/`then` clause keyword beginning `row` that
/// continues a non-standalone `if` (a ternary). Two parse shapes occur:
///   * A proper clause keyword inside an `if_statement`: a continuation only when
///     that `if` is not standalone (a standalone `if` aligns its `else` via
///     bracket_depth instead).
///   * An orphaned keyword that tree-sitter, recovering a line-broken ternary,
///     demotes to a leading `symbol` with no enclosing `if` — always the
///     continuation of the ternary begun on an earlier line.
fn is_dangling_clause_keyword(
    node: M2Node<'_>,
    row: usize,
    text: &str,
    line_leads: &[usize],
) -> bool {
    if !is_clause_keyword_leaf(node, text) {
        return false;
    }
    if node.start_position().row != row {
        return false;
    }
    match enclosing_if_statement(node) {
        Some(if_statement) => !if_statement_is_standalone(if_statement, line_leads),
        None => true,
    }
}

/// Whether a leaf is an `else`/`then` clause keyword. Normally these are the
/// keyword token kinds, but when a line-broken ternary is parsed the keyword is
/// demoted to a bare `symbol`; since `else`/`then` are reserved words no real
/// identifier carries that text, so a symbol spelled so is the misparsed keyword.
fn is_clause_keyword_leaf(node: M2Node<'_>, text: &str) -> bool {
    if matches!(node.raw_kind(), "then" | "else") {
        return true;
    }
    node.is(NodeKind::Symbol)
        && matches!(&text[node.start_byte()..node.end_byte()], "else" | "then")
}

/// The nearest enclosing `if_statement` of a clause keyword, walking up through
/// its clause parent.
fn enclosing_if_statement(node: M2Node<'_>) -> Option<M2Node<'_>> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.is(NodeKind::IfStatement) {
            return Some(parent);
        }
        current = parent;
    }
    None
}

/// Whether an `if_statement` begins its own line (only whitespace precedes the
/// `if` token on its row).
fn if_statement_is_standalone(node: M2Node<'_>, line_leads: &[usize]) -> bool {
    let row = node.start_position().row;
    let column = node.start_position().column;
    line_leads.get(row).copied().unwrap_or(0) >= column
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatFoldRange {
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndentedLine {
    line: u32,
    depth: usize,
}

#[derive(Debug, Clone, Copy)]
struct OpenFoldRange {
    start_line: u32,
    depth: usize,
}

fn collect_indent_fold_ranges(lines: &[IndentedLine]) -> Vec<FormatFoldRange> {
    let mut ranges = Vec::new();
    let mut open_folds: Vec<OpenFoldRange> = Vec::new();
    let mut previous: Option<IndentedLine> = None;

    for &current in lines {
        if let Some(prev) = previous {
            if current.depth > prev.depth {
                open_folds.push(OpenFoldRange {
                    start_line: prev.line,
                    depth: prev.depth,
                });
            } else if current.depth < prev.depth {
                while let Some(open_fold) = open_folds.last() {
                    if open_fold.depth >= current.depth {
                        close_fold_range(&mut ranges, *open_fold, previous);
                        open_folds.pop();
                    } else {
                        break;
                    }
                }
            }
        }
        previous = Some(current);
    }

    while let Some(open_fold) = open_folds.pop() {
        close_fold_range(&mut ranges, open_fold, previous);
    }

    ranges
}

fn close_fold_range(
    ranges: &mut Vec<FormatFoldRange>,
    range: OpenFoldRange,
    previous: Option<IndentedLine>,
) {
    let Some(previous) = previous else {
        return;
    };
    if previous.line <= range.start_line {
        return;
    }

    ranges.push(FormatFoldRange {
        start_line: range.start_line,
        end_line: previous.line,
    });
}

fn normalize_whitespace(text: &str) -> String {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_macaulay2::language())
        .unwrap();
    let Some(tree) = parser.parse(text, None) else {
        return text.to_string();
    };

    let mut edits = Vec::new();
    collect_format_edits(M2Node::new(tree.root_node()), text, &mut edits);
    apply_format_edits(text, edits)
}

fn collect_format_edits(node: M2Node<'_>, text: &str, edits: &mut Vec<FormatEdit>) {
    if node.is_missing() {
        return;
    }

    if !node.is_error() {
        if node.is_comma() {
            push_comma_whitespace_edits(text, node, edits);
        }

        if node.is_semicolon() {
            push_semicolon_whitespace_edits(text, node, edits);
        }

        if let Some(operator) = node.child_by_field_name("operator") {
            let operator_text = &text[operator.start_byte()..operator.end_byte()];
            if is_parenthesized_call(node) {
                // A call `f(...)` that is the head of a `:=` install reads as
                // installation syntax, so it is spaced (`f (Types) := …`); an
                // ordinary call is compacted (`f(x)`).
                if is_method_installation_call_head(node, text) {
                    push_call_gap_whitespace_edit(node, text, edits, " ");
                } else {
                    push_call_whitespace_edits(node, text, edits);
                }
            } else if should_space_factor_operator_with_adjacency_factor(node, operator_text) {
                push_operator_whitespace_edits(text, operator, edits);
            } else if should_compact_prefix_operator(node.kind, operator_text) {
                push_prefix_operator_whitespace_edits(text, operator, edits);
            } else if should_compact_operator(node.kind, operator_text) {
                push_compact_operator_whitespace_edits(text, operator, edits);
            } else if should_space_operator(node.kind, operator_text) {
                push_operator_whitespace_edits(text, operator, edits);
            }
        }

        if node.is(NodeKind::LambdaExpression) {
            push_lambda_operator_whitespace_edits(node, text, edits);
        }
    }

    for child in node.children() {
        collect_format_edits(child, text, edits);
    }
}

fn should_space_operator(parent_kind: NodeKind, operator: &str) -> bool {
    match parent_kind {
        // Assignments (`=`, `:=`, `<-`), options (`=>`), and arrows (`->`) are all
        // `binary_expression` in this grammar; their operators are listed below.
        NodeKind::BinaryExpression => matches!(
            operator,
            "==" | "!="
                | "==="
                | "=!="
                | "<<"
                | "<"
                | ">"
                | "<="
                | ">="
                | "or"
                | "??"
                | "xor"
                | "and"
                | "||"
                | "|"
                | "^^"
                | "&"
                | "++"
                | "+"
                | "-"
                | "⊠"
                | "⧢"
                | "\\"
                | "\\\\"
                | "%"
                | "//"
                | ":="
                | "="
                | "<-"
                | "=>"
                | "->"
        ),
        _ => false,
    }
}

fn should_compact_operator(parent_kind: NodeKind, operator: &str) -> bool {
    parent_kind == NodeKind::BinaryExpression && is_compact_operator(operator)
}

fn is_compact_operator(operator: &str) -> bool {
    matches!(
        operator,
        "·" | "**"
            | "⊠"
            | "⧢"
            | "%"
            | "/"
            | "*"
            | "@"
            | "@@"
            | "@@?"
            | "|_"
            | "^"
            | "^**"
            | "^<"
            | "^<="
            | "^>"
            | "^>="
            | "_"
            | "_<"
            | "_<="
            | "_>"
            | "_>="
            | "#"
            | "#?"
    )
}

fn should_compact_prefix_operator(parent_kind: NodeKind, operator: &str) -> bool {
    parent_kind == NodeKind::PrefixExpression
        && matches!(
            operator,
            "+" | "-"
                | "*"
                | "#"
                | "<"
                | "<="
                | ">"
                | ">="
                | "?"
                | "<<"
                | "|-"
                | "<==="
                | "<=="
                | "??"
        )
}

/// Whether `operator`, when it dangles at the end of a line, takes a trailing
/// space and so signals that the following row is an indented continuation. Used
/// by the tree-driven indenter to decide right-operand continuation indentation.
fn is_spaced_line_final_operator(operator: &str) -> bool {
    matches!(
        operator,
        "==" | "!="
            | "==="
            | "=!="
            | "<"
            | ">"
            | "<="
            | ">="
            | "or"
            | "??"
            | "xor"
            | "and"
            | "||"
            | "|"
            | "^^"
            | "&"
            | "++"
            | "<<"
            | "+"
            | "-"
            | "=>"
            | "->"
            | "="
            | ":="
            | "<-"
            | "\\"
            | "\\\\"
            | "then"
            | "else"
            | "do"
            | "list"
    )
}

fn is_parenthesized_call(node: M2Node<'_>) -> bool {
    if !node.is(NodeKind::BinaryExpression) {
        return false;
    }

    let Some(operator) = node.child_by_field_name("operator") else {
        return false;
    };
    let Some(right) = node.child_by_field_name("right") else {
        return false;
    };

    // A call's argument list is a `sequence` (`f(a, b)`, `f()`) or, for a single
    // parenthesized argument, a `parenthesized_expression` (`f(x)`).
    operator.is_implicit_application()
        && matches!(
            right.kind,
            NodeKind::Sequence | NodeKind::ParenthesizedExpression
        )
}

/// Whether a parenthesized call `f(...)` is the head of a `:=` method install
/// (`f(Types) := fn`) — i.e. it is the left operand of an enclosing `:=`. Such a
/// head is installation syntax and is spaced rather than compacted.
fn is_method_installation_call_head(node: M2Node<'_>, text: &str) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !parent.is(NodeKind::BinaryExpression) {
        return false;
    }
    let Some(operator) = parent.child_by_field_name("operator") else {
        return false;
    };
    if &text[operator.start_byte()..operator.end_byte()] != ":=" {
        return false;
    }
    parent.child_by_field_name("left").is_some_and(|left| {
        left.start_byte() == node.start_byte() && left.end_byte() == node.end_byte()
    })
}

fn push_call_gap_whitespace_edit(
    node: M2Node<'_>,
    text: &str,
    edits: &mut Vec<FormatEdit>,
    replacement: &'static str,
) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    let Some(right) = node.child_by_field_name("right") else {
        return;
    };
    let gap = &text[left.end_byte()..right.start_byte()];
    if !gap.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
        return;
    }
    edits.push(FormatEdit {
        start_byte: left.end_byte(),
        end_byte: right.start_byte(),
        replacement,
    });
}

fn should_space_factor_operator_with_adjacency_factor(
    node: M2Node<'_>,
    operator_text: &str,
) -> bool {
    if !matches!(operator_text, "*" | "/" | "%" | "**" | "//") {
        return false;
    }
    let Some(left) = node.child_by_field_name("left") else {
        return false;
    };
    let Some(right) = node.child_by_field_name("right") else {
        return false;
    };
    !is_adjacent_factor(left) || !is_adjacent_factor(right)
}

fn is_adjacent_factor(node: M2Node<'_>) -> bool {
    matches!(
        node.kind,
        NodeKind::Symbol
            | NodeKind::IntegerLiteral
            | NodeKind::FloatLiteral
            | NodeKind::StringLiteral
            | NodeKind::Sequence
            | NodeKind::ParenthesizedExpression
            | NodeKind::List
            | NodeKind::Array
            | NodeKind::AngleBarList
            | NodeKind::PrefixExpression
            | NodeKind::PostfixExpression
            | NodeKind::BinaryExpression
    )
}

fn push_call_whitespace_edits(node: M2Node<'_>, text: &str, edits: &mut Vec<FormatEdit>) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    let Some(right) = node.child_by_field_name("right") else {
        return;
    };
    let gap = &text[left.end_byte()..right.start_byte()];
    if !gap.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
        return;
    }

    edits.push(FormatEdit {
        start_byte: left.end_byte(),
        end_byte: right.start_byte(),
        replacement: "",
    });
}

fn push_lambda_operator_whitespace_edits(
    node: M2Node<'_>,
    text: &str,
    edits: &mut Vec<FormatEdit>,
) {
    let Some(operator) = node.child_by_field_name("operator") else {
        return;
    };
    push_operator_whitespace_edits(text, operator, edits);
}

fn push_operator_whitespace_edits(text: &str, operator: M2Node<'_>, edits: &mut Vec<FormatEdit>) {
    let Some(start_byte) = same_line_horizontal_whitespace_start(text, operator.start_byte())
    else {
        return;
    };
    let Some(end_byte) = same_line_horizontal_whitespace_end(text, operator.end_byte()) else {
        return;
    };

    edits.push(FormatEdit {
        start_byte,
        end_byte: operator.start_byte(),
        replacement: " ",
    });
    edits.push(FormatEdit {
        start_byte: operator.end_byte(),
        end_byte,
        replacement: " ",
    });
}

fn push_compact_operator_whitespace_edits(
    text: &str,
    operator: M2Node<'_>,
    edits: &mut Vec<FormatEdit>,
) {
    let Some(start_byte) = same_line_horizontal_whitespace_start(text, operator.start_byte())
    else {
        return;
    };
    let Some(end_byte) = same_line_horizontal_whitespace_end(text, operator.end_byte()) else {
        return;
    };

    edits.push(FormatEdit {
        start_byte,
        end_byte: operator.start_byte(),
        replacement: "",
    });
    edits.push(FormatEdit {
        start_byte: operator.end_byte(),
        end_byte,
        replacement: "",
    });
}

fn push_prefix_operator_whitespace_edits(
    text: &str,
    operator: M2Node<'_>,
    edits: &mut Vec<FormatEdit>,
) {
    let Some(end_byte) = same_line_horizontal_whitespace_end(text, operator.end_byte()) else {
        return;
    };

    edits.push(FormatEdit {
        start_byte: operator.end_byte(),
        end_byte,
        replacement: "",
    });
}

fn push_comma_whitespace_edits(text: &str, comma: M2Node<'_>, edits: &mut Vec<FormatEdit>) {
    if let Some(start_byte) = same_line_horizontal_whitespace_start(text, comma.start_byte()) {
        edits.push(FormatEdit {
            start_byte,
            end_byte: comma.start_byte(),
            replacement: "",
        });
    }

    let Some(end_byte) = same_line_horizontal_whitespace_end(text, comma.end_byte()) else {
        return;
    };
    let replacement = match text.as_bytes().get(end_byte) {
        Some(b')' | b']' | b'}' | b';') | None => "",
        _ => " ",
    };

    edits.push(FormatEdit {
        start_byte: comma.end_byte(),
        end_byte,
        replacement,
    });
}

fn push_semicolon_whitespace_edits(text: &str, semicolon: M2Node<'_>, edits: &mut Vec<FormatEdit>) {
    if let Some(start_byte) = same_line_horizontal_whitespace_start(text, semicolon.start_byte()) {
        edits.push(FormatEdit {
            start_byte,
            end_byte: semicolon.start_byte(),
            replacement: "",
        });
    }

    let Some(end_byte) = same_line_horizontal_whitespace_end(text, semicolon.end_byte()) else {
        return;
    };
    let Some(next_byte) = text.as_bytes().get(end_byte) else {
        return;
    };
    if !matches!(next_byte, b'\n' | b'\r') {
        edits.push(FormatEdit {
            start_byte: semicolon.end_byte(),
            end_byte,
            replacement: "\n",
        });
    }
}

fn same_line_horizontal_whitespace_start(text: &str, byte_index: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut start_byte = byte_index;
    while start_byte > 0 && matches!(bytes[start_byte - 1], b' ' | b'\t') {
        start_byte -= 1;
    }

    (start_byte > 0 && !matches!(bytes[start_byte - 1], b'\n' | b'\r')).then_some(start_byte)
}

fn same_line_horizontal_whitespace_end(text: &str, byte_index: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut end_byte = byte_index;
    while end_byte < bytes.len() && matches!(bytes[end_byte], b' ' | b'\t') {
        end_byte += 1;
    }

    (end_byte < bytes.len() && !matches!(bytes[end_byte], b'\n' | b'\r')).then_some(end_byte)
}

fn apply_format_edits(text: &str, mut edits: Vec<FormatEdit>) -> String {
    if edits.is_empty() {
        return text.to_string();
    }

    edits.sort_by_key(|edit| (edit.start_byte, edit.end_byte));
    let mut filtered_edits = Vec::with_capacity(edits.len());
    for edit in edits {
        if edit.start_byte == edit.end_byte && edit.replacement.is_empty() {
            continue;
        }
        if filtered_edits
            .last()
            .is_some_and(|previous: &FormatEdit| previous.end_byte > edit.start_byte)
        {
            return text.to_string();
        }
        filtered_edits.push(edit);
    }

    let mut formatted = text.to_string();
    for edit in filtered_edits.into_iter().rev() {
        formatted.replace_range(edit.start_byte..edit.end_byte, edit.replacement);
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_whitespace_without_reflowing_code() {
        assert_eq!(
            format_document_text("x := 1  \n  y = 2\t\n"),
            "x := 1\ny = 2\n"
        );
        assert_eq!(format_document_text("x := 1  "), "x := 1");
    }

    #[test]
    fn spaces_parenthesized_method_install_head() {
        // An install head reads as installation syntax: `f (Types) := …`.
        assert_eq!(
            format_document_text("f(ZZ,ZZ) := (i,j) -> i\n"),
            "f (ZZ, ZZ) := (i, j) -> i\n"
        );
    }

    #[test]
    fn ordinary_call_stays_compact() {
        // A plain call is not an install; it keeps `f(args)` with no space.
        assert_eq!(format_document_text("g(5)\n"), "g(5)\n");
        assert_eq!(format_document_text("h(1, 2)\n"), "h(1, 2)\n");
    }

    #[test]
    fn normalizes_operator_whitespace() {
        assert_eq!(format_document_text("x+y\n"), "x + y\n");
        assert_eq!(format_document_text("x+ y\n"), "x + y\n");
        assert_eq!(format_document_text("x   +y\n"), "x + y\n");
        assert_eq!(format_document_text("x\t+\t y\n"), "x + y\n");
        assert_eq!(format_document_text("x:=y\n"), "x := y\n");
        assert_eq!(format_document_text("f:=x->x+1\n"), "f := x -> x + 1\n");
        assert_eq!(
            format_document_text("f(x, Strategy=>LongPolynomial)\n"),
            "f(x, Strategy => LongPolynomial)\n"
        );
        assert_eq!(format_document_text("8 * delta / 2\n"), "8*delta/2\n");
        assert_eq!(format_document_text("x ** y // z\n"), "x**y // z\n");
        assert_eq!(format_document_text("x<<y\n"), "x << y\n");
        assert_eq!(format_document_text("x ^ y # 0\n"), "x^y#0\n");
    }

    #[test]
    fn compacts_unary_prefix_operators() {
        assert_eq!(
            format_document_text("iCoeff := - jRow#0;\n"),
            "iCoeff := -jRow#0;\n"
        );
        assert_eq!(format_document_text("x := not true\n"), "x := not true\n");
        assert_eq!(format_document_text("a-b\n"), "a - b\n");
    }

    #[test]
    fn indents_operator_continuations() {
        let options = FormatOptions::new(3, true);

        assert_eq!(
            format_document_text_with_options("a + \nb +\nc\n", &options),
            "a +\n   b +\n   c\n"
        );
        assert_eq!(
            format_document_text_with_options("f := (a +\nb +\nc)\n", &options),
            "f := (a +\n      b +\n      c)\n"
        );
        assert_eq!(
            format_document_text_with_options("-----\ntop = 1\n", &options),
            "-----\ntop = 1\n"
        );
    }

    #[test]
    fn normalizes_comma_whitespace() {
        assert_eq!(format_document_text("f(x,y)\n"), "f(x, y)\n");
        assert_eq!(format_document_text("f(x ,y)\n"), "f(x, y)\n");
        assert_eq!(format_document_text("f(x,  y)\n"), "f(x, y)\n");
        assert_eq!(format_document_text("f(x ,  y)\n"), "f(x, y)\n");
        assert_eq!(format_document_text("f(x,)\n"), "f(x,)\n");
        assert_eq!(format_document_text("f(x,\ny)\n"), "f(x,\n    y)\n");
    }

    #[test]
    fn normalizes_semicolon_whitespace() {
        assert_eq!(format_document_text("i=0;j=0;\n"), "i = 0;\nj = 0;\n");
        assert_eq!(format_document_text("i = 0 ; j = 0\n"), "i = 0;\nj = 0\n");
        assert_eq!(format_document_text("i = 0;\n j = 0\n"), "i = 0;\nj = 0\n");
        assert_eq!(
            format_document_text("\"x;y\" -- comment a;b\n"),
            "\"x;y\" -- comment a;b\n"
        );
        assert_eq!(
            format_document_text("f := (i=0;j=0;)\n"),
            "f := (i = 0;\n    j = 0;\n)\n"
        );
    }

    #[test]
    fn line_end_open_brackets_create_single_extra_indent_scope() {
        assert_eq!(
            format_document_text(
                "scan(docKeys, key -> (\nbaseName := recordNameFromDocKey key;\nif db#?key then result#baseName = append(result#baseName ?? {}, hashTable {\"key\" => key, \"raw\" => db#key});\n)\n);\n"
            ),
            "scan(docKeys, key -> (\n    baseName := recordNameFromDocKey key;\n    if db#?key then result#baseName = append(result#baseName ?? {}, hashTable {\"key\" => key, \"raw\" => db#key});\n)\n);\n"
        );
    }

    #[test]
    fn inline_open_brackets_do_not_force_multiline_closer() {
        // A ternary `if` (value position) is left inline, so its inline
        // `then(...)` brackets are not pushed onto their own lines.
        assert_eq!(
            format_document_text("x := if a then(b) else(c)\n"),
            "x := if a then(b) else(c)\n"
        );
    }

    #[test]
    fn uses_configured_indent_width() {
        let options = FormatOptions::new(2, true);

        assert_eq!(
            format_document_text_with_options("f := (i=0;j=0;)\n", &options),
            "f := (i = 0;\n  j = 0;\n)\n"
        );
        assert_eq!(
            format_document_text_with_options("f := (i=0;j=0;)\n", &FormatOptions::new(4, false)),
            "f := (i = 0;\n\tj = 0;\n)\n"
        );
    }

    #[test]
    fn normalizes_parenthesized_call_whitespace() {
        assert_eq!(format_document_text("f (x,y)\n"), "f(x, y)\n");
        assert_eq!(format_document_text("f \t (x)\n"), "f(x)\n");
        assert_eq!(
            format_document_text("random (source gens Ip2, R^{-d})\n"),
            "random(source gens Ip2, R^{-d})\n"
        );
        assert_eq!(format_document_text("f x\n"), "f x\n");
    }

    #[test]
    fn preserves_non_operator_text() {
        assert_eq!(
            format_document_text("\"x+y\" -- comment a+b\n"),
            "\"x+y\" -- comment a+b\n"
        );
        assert_eq!(format_document_text("-x+y\n"), "-x + y\n");
        assert_eq!(format_document_text("R=QQ[a..d]\n"), "R = QQ[a..d]\n");
        assert_eq!(format_document_text("x#0 + y\n"), "x#0 + y\n");
        assert_eq!(format_document_text("x\n+y\n"), "x\n+y\n");
    }

    #[test]
    fn preserves_multiline_string_contents() {
        assert_eq!(
            format_document_text("x=///\n  keep spaces  \n///\ny=1\n"),
            "x = ///\n  keep spaces  \n///\ny = 1\n"
        );
        assert_eq!(
            format_document_text("x=\"first\n  keep spaces  \nlast\"\ny=1\n"),
            "x = \"first\n  keep spaces  \nlast\"\ny = 1\n"
        );
    }

    #[test]
    fn leaves_single_word_if_bodies_inline() {
        assert_eq!(
            format_document_text("if a then b else c\n"),
            "if a then b else c\n"
        );
        assert_eq!(format_document_text("if a then b\n"), "if a then b\n");
    }

    #[test]
    fn leaves_ternary_if_untouched() {
        assert_eq!(
            format_document_text("x := if a then f(x) else g(y)\n"),
            "x := if a then f(x) else g(y)\n"
        );
    }

    #[test]
    fn formats_valid_nodes_when_other_nodes_have_parse_errors() {
        assert_eq!(
            format_document_text("x+y\nbad := (\na:=b\n"),
            "x + y\nbad := (\n    a := b\n"
        );
    }

    #[test]
    fn formats_example_file_despite_parser_gaps() {
        let formatted = format_document_text(include_str!("../../example_m2_code/example1.m2"));

        assert!(formatted.contains("k := ceiling((-3 + sqrt(9.0 + 8*delta))/2);"));
        assert!(formatted.contains("K = ZZ/101;"));
        assert!(formatted.contains("randomPlanePoints = (delta, R) -> ("));
        assert!(formatted.contains("random(source gens Ip2, R^{-d})"));
        assert!(formatted.contains("SyzygyLimit => 60"));
    }

    #[test]
    fn indents_else_branches_of_line_broken_if_expression() {
        // A line-broken `if`/`else` chain is only valid M2 inside brackets: at
        // global scope the newline after a completable `if … then …` ends the
        // expression and the following `else` is a syntax error. Within `( … )`
        // the newline is whitespace, so the whole chain is one expression. Each
        // `else if …` aligns with its `if`, and the final `else` belongs to the
        // nested `if kind === "operator"`, so it indents one level deeper.
        let chain = "extracted := (\n\
             if kind === \"function\" then\n\
             extractFunc(name, db)\n\
             else if kind === \"operator\" then extractOperator(name, db)\n\
             else extractObject(name, db)\n\
             );\n";
        let formatted = format_document_text(chain);

        assert_eq!(
            formatted,
            "extracted := (\n\
             \x20   if kind === \"function\" then\n\
             \x20       extractFunc(name, db)\n\
             \x20   else if kind === \"operator\" then extractOperator(name, db)\n\
             \x20       else extractObject(name, db)\n\
             );\n"
        );
        // The result is stable under re-formatting.
        assert_eq!(format_document_text(&formatted), formatted);
    }

    #[test]
    fn indents_body_of_else_parenthesized_block() {
        // The `else (` block reopens a scope on a line that also closes one, so
        // its body must indent just like the `then (` block body.
        assert_eq!(
            format_document_text("if cond then (\na;\nb\n) else (\nc;\nd\n)\n"),
            "if cond then (\n    a;\n    b\n) else (\n    c;\n    d\n)\n"
        );
    }

    #[test]
    fn keeps_top_level_symbols_unindented_after_comment_dividers() {
        let formatted = format_document_text(include_str!("../../example_m2_code/example2.m2"));

        assert!(formatted.contains("\nprimitive = (L) -> ("));
        assert!(formatted.contains("\ntoZZ = (L) -> ("));
        assert!(!formatted.contains("\n     toZZ = (L) -> ("));
    }
}
