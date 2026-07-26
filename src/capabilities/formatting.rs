//! Tree-sitter-guided formatting and folding ranges for Macaulay2 source.

use tower_lsp::lsp_types::{
    DocumentFormattingOptions, FoldingRange, FoldingRangeKind, FoldingRangeProviderCapability,
    OneOf, TextEdit,
};
use tree_sitter::Parser;

use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::util::full_document_range;

pub(crate) trait FormattingConfiguration {
    fn indent_width(&self) -> Option<u32>;
    fn use_tabs(&self) -> Option<bool>;
    fn compact_factor_operators(&self) -> bool;
    fn break_after_semicolon(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOptions {
    indent: String,
    compact_factor_operators: bool,
    break_after_semicolon: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            indent: "    ".to_string(),
            compact_factor_operators: false,
            break_after_semicolon: true,
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

        Self {
            indent,
            ..Self::default()
        }
    }

    pub(crate) fn from_configuration(
        tab_size: u32,
        insert_spaces: bool,
        configuration: &(impl FormattingConfiguration + ?Sized),
    ) -> Self {
        let tab_size = configuration.indent_width().unwrap_or(tab_size);
        let use_tabs = configuration.use_tabs().unwrap_or(!insert_spaces);
        let mut options = Self::new(tab_size, !use_tabs);
        options.compact_factor_operators = configuration.compact_factor_operators();
        options.break_after_semicolon = configuration.break_after_semicolon();
        options
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
    configuration: &(impl FormattingConfiguration + ?Sized),
) -> Vec<TextEdit> {
    let options = FormatOptions::from_configuration(tab_size, insert_spaces, configuration);
    let formatted = format_document_text_with_options(text, &options);
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
            kind: (range.kind == FormatFoldKind::Comment).then_some(FoldingRangeKind::Comment),
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

/// Format the whole document: whitespace normalization around operators and
/// punctuation, then tree-derived re-indentation with `options`.
pub fn format_document_text_with_options(text: &str, options: &FormatOptions) -> String {
    // Basic spacing only, and provably string/comment-safe: every edit either
    // adjusts whitespace adjacent to a real operator/punctuation node
    // (`normalize_whitespace`) or rebuilds a line's leading indentation
    // (`reindent_from_tree`). Neither rewrites token text, so string and comment
    // contents are never modified. No reflow/line-breaking, no byte-scanning.
    let newline = detect_line_ending(text);
    let formatted = normalize_whitespace(text, options);
    let mut formatted = reindent_from_tree(&formatted, options, newline);

    if text.ends_with('\n') {
        formatted.push_str(newline);
    }

    formatted
}

/// The line terminator the document already uses, so re-indentation preserves it
/// instead of silently rewriting every CRLF line ending to LF (the trap behind
/// `str::lines()` + `join("\n")`). A document with any `\r\n` is treated as CRLF.
fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Re-indent every line of already-normalized `text` from a fresh parse: parse #2
/// of the two-parse design (parse #1 ran in `normalize_whitespace`). Each line's
/// leading whitespace is rebuilt as `options.indent.repeat(depth)` from the
/// tree-derived depth; lines inside a multiline string/raw-string are emitted
/// verbatim so their interior spacing is preserved.
fn reindent_from_tree(text: &str, options: &FormatOptions, newline: &str) -> String {
    let layout = TreeIndentLayout::build(text, options.compact_factor_operators);
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
            if layout.preserves_literal_trailing_space(row) {
                indented.push_str(line.trim_start());
            } else {
                indented.push_str(trimmed);
            }
            indented
        })
        .collect::<Vec<_>>()
        .join(newline)
}

/// The document's fold ranges, derived from tree-based indentation depths.
pub fn folding_ranges_for_text(text: &str) -> Vec<FormatFoldRange> {
    let layout = TreeIndentLayout::build(text, false);
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

    let mut ranges = collect_indent_fold_ranges(&indented_lines);
    ranges.extend(layout.comment_folds);
    ranges.sort_by_key(|range| (range.start_line, range.end_line, range.kind));
    ranges
}

/// Per-line indentation depths derived from a tree-sitter parse of normalized
/// text, plus the rows that lie inside a multiline string and must be left
/// verbatim and the comment ranges that can be folded.
/// `depth(row) = bracket_depth(row) + continuation(row)`.
struct TreeIndentLayout {
    depths: Vec<usize>,
    literal_rows: Vec<bool>,
    literal_start_rows: Vec<bool>,
    comment_folds: Vec<FormatFoldRange>,
}

impl TreeIndentLayout {
    fn build(text: &str, compact_factor_operators: bool) -> Self {
        let line_count = text.lines().count().max(1);
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_macaulay2::language())
            .is_err()
        {
            return Self {
                depths: vec![0; line_count],
                literal_rows: vec![false; line_count],
                literal_start_rows: vec![false; line_count],
                comment_folds: Vec::new(),
            };
        }
        let Some(tree) = parser.parse(text, None) else {
            return Self {
                depths: vec![0; line_count],
                literal_rows: vec![false; line_count],
                literal_start_rows: vec![false; line_count],
                comment_folds: Vec::new(),
            };
        };

        let root = M2Node::new(tree.root_node(), text);
        let brackets = collect_bracket_groups(root, line_count);
        let (literal_rows, literal_start_rows) = collect_literal_rows(root, line_count);
        let line_leads = line_leading_blank(text, line_count);
        let comment_folds = collect_comment_fold_ranges(root, &line_leads);

        let depths = (0..line_count)
            .map(|row| {
                bracket_depth(row, &brackets, &line_leads)
                    + line_continuation(row, root, &line_leads, compact_factor_operators)
            })
            .collect();

        Self {
            depths,
            literal_rows,
            literal_start_rows,
            comment_folds,
        }
    }

    fn depth(&self, row: usize) -> usize {
        self.depths.get(row).copied().unwrap_or(0)
    }

    fn is_literal_line(&self, row: usize) -> bool {
        self.literal_rows.get(row).copied().unwrap_or(false)
    }

    fn preserves_literal_trailing_space(&self, row: usize) -> bool {
        self.literal_start_rows.get(row).copied().unwrap_or(false)
    }
}

/// Build folds from parser-classified comments. Consecutive full-line `--`
/// comments become one range, while each multiline block comment is its own
/// range. Keeping this tree-derived avoids mistaking comment markers inside
/// strings for comments.
fn collect_comment_fold_ranges(root: M2Node<'_>, line_leads: &[usize]) -> Vec<FormatFoldRange> {
    let mut line_comment_rows = Vec::new();
    let mut ranges = Vec::new();

    for node in root.descendants() {
        let start = node.start_position();
        let end = node.end_position();
        match node.kind {
            NodeKind::LineComment
                if start.row == end.row
                    && line_leads.get(start.row).copied() == Some(start.column) =>
            {
                line_comment_rows.push(start.row as u32);
            }
            NodeKind::BlockComment if start.row < end.row => ranges.push(FormatFoldRange {
                start_line: start.row as u32,
                end_line: end.row as u32,
                kind: FormatFoldKind::Comment,
            }),
            _ => {}
        }
    }

    line_comment_rows.sort_unstable();
    line_comment_rows.dedup();
    let mut block_start = None;
    let mut previous = None;
    for row in line_comment_rows {
        if previous.is_none_or(|previous_row| row != previous_row + 1) {
            append_line_comment_fold(&mut ranges, block_start, previous);
            block_start = Some(row);
        }
        previous = Some(row);
    }
    append_line_comment_fold(&mut ranges, block_start, previous);

    ranges
}

fn append_line_comment_fold(
    ranges: &mut Vec<FormatFoldRange>,
    start_line: Option<u32>,
    end_line: Option<u32>,
) {
    let (Some(start_line), Some(end_line)) = (start_line, end_line) else {
        return;
    };
    if start_line == end_line {
        return;
    }
    ranges.push(FormatFoldRange {
        start_line,
        end_line,
        kind: FormatFoldKind::Comment,
    });
}

/// A multiline bracket node, keyed by the row it opens on. Brackets that open on
/// the same row collapse to one indent level, so the `open_row` is the group id.
#[derive(Debug, Clone, Copy)]
struct BracketGroup {
    open_row: usize,
    close_row: usize,
    closing_delimiter_column: usize,
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
                    closing_delimiter_column: close_position
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
                closing_delimiter_column: usize::MAX,
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
fn collect_literal_rows(root: M2Node<'_>, line_count: usize) -> (Vec<bool>, Vec<bool>) {
    let mut literal = vec![false; line_count];
    let mut literal_starts = vec![false; line_count];
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is(NodeKind::StringLiteral) {
            let start_row = node.start_position().row;
            let end_row = node.end_position().row;
            if start_row < end_row {
                if let Some(slot) = literal_starts.get_mut(start_row) {
                    *slot = true;
                }
            }
            for row in (start_row + 1)..=end_row {
                if let Some(slot) = literal.get_mut(row) {
                    *slot = true;
                }
            }
        }
        stack.extend(node.children());
    }
    (literal, literal_starts)
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
            && group.closing_delimiter_column == leading_blank
            && !leading_closed.contains(&group.open_row)
        {
            leading_closed.push(group.open_row);
        }
    }
    active.len().saturating_sub(leading_closed.len())
}

/// `continuation(row)` is a flat +1 for a line that continues an expression or a
/// clause body broken onto a later line (see the rule cases inline).
fn line_continuation(
    row: usize,
    root: M2Node<'_>,
    line_leads: &[usize],
    compact_factor_operators: bool,
) -> usize {
    let Some(first) = first_leaf_on_row(root, row) else {
        return 0;
    };

    // (a) The first token is the start of the right operand of a binary
    // expression whose operator dangled on an earlier row (`a +\nb`).
    if is_right_operand_first_token(first, row, compact_factor_operators) {
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
    if is_dangling_clause_keyword(first, row, line_leads) {
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
fn is_right_operand_first_token(
    node: M2Node<'_>,
    row: usize,
    compact_factor_operators: bool,
) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.is(NodeKind::BinaryExpression) {
            if let (Some(operator), Some(right)) = (
                parent.child_by_field_name("operator"),
                parent.child_by_field_name("right"),
            ) {
                if right.start_byte() == node.start_byte()
                    && operator.start_position().row < row
                    && is_spaced_line_final_operator(operator.text(), compact_factor_operators)
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
fn is_dangling_clause_keyword(node: M2Node<'_>, row: usize, line_leads: &[usize]) -> bool {
    if !is_clause_keyword_leaf(node) {
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
fn is_clause_keyword_leaf(node: M2Node<'_>) -> bool {
    if node.is_then_or_else_keyword() {
        return true;
    }
    node.is(NodeKind::Symbol) && matches!(node.text(), "else" | "then")
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

/// A foldable block of consecutive lines (0-based, both bounds inclusive as
/// produced; the LSP conversion maps them onto `FoldingRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatFoldRange {
    pub start_line: u32,
    pub end_line: u32,
    pub kind: FormatFoldKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FormatFoldKind {
    Region,
    Comment,
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
        kind: FormatFoldKind::Region,
    });
}

fn normalize_whitespace(text: &str, options: &FormatOptions) -> String {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_macaulay2::language())
        .is_err()
    {
        return text.to_string();
    }
    let Some(tree) = parser.parse(text, None) else {
        return text.to_string();
    };

    let mut edits = Vec::new();
    collect_format_edits(
        M2Node::new(tree.root_node(), text),
        text,
        options,
        &mut edits,
    );
    apply_format_edits(text, edits)
}

fn collect_format_edits(
    node: M2Node<'_>,
    text: &str,
    options: &FormatOptions,
    edits: &mut Vec<FormatEdit>,
) {
    if node.is_missing() {
        return;
    }

    if !node.is_error() {
        if node.is_comma() {
            push_comma_whitespace_edits(text, node, edits);
        }

        if node.is_semicolon() {
            push_semicolon_whitespace_edits(text, node, options.break_after_semicolon, edits);
        }

        if let Some(operator) = node.child_by_field_name("operator") {
            if is_parenthesized_call(node) {
                // A call `f(...)` that is the head of a `:=` install reads as
                // installation syntax, so it is spaced (`f (Types) := …`); an
                // ordinary call is compacted (`f(x)`).
                if is_method_installation_call_head(node) {
                    push_call_gap_whitespace_edit(node, text, edits, " ");
                } else {
                    push_call_gap_whitespace_edit(node, text, edits, "");
                }
            } else {
                match operator_spacing(node.kind, operator.text()) {
                    OperatorSpacing::Spaced => {
                        push_operator_whitespace_edits(text, operator, edits)
                    }
                    OperatorSpacing::Compact => {
                        push_compact_operator_whitespace_edits(text, operator, edits)
                    }
                    OperatorSpacing::Factor => {
                        if options.compact_factor_operators && binary_operator_all_factors(node) {
                            push_compact_operator_whitespace_edits(text, operator, edits);
                        } else {
                            push_operator_whitespace_edits(text, operator, edits);
                        }
                    }
                    OperatorSpacing::Prefix => {
                        push_prefix_operator_whitespace_edits(text, operator, edits)
                    }
                    OperatorSpacing::None => {}
                }
            }
        }
    }

    for child in node.children() {
        collect_format_edits(child, text, options, edits);
    }
}

/// Per-operator spacing rule. Single source of truth consumed by the spacing
/// walk (`collect_format_edits`) and the indent continuation check
/// (`is_right_operand_first_token`). Folding the previously separate
/// `should_space_*`/`is_compact_*`/`is_spaced_line_final_*` tables into one
/// place removes the drift risk where the spaced and line-final tables forgot
/// to track each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatorSpacing {
    /// Always surround with a space on each side (`a + b`, `x := y`).
    Spaced,
    /// Always collapse adjacent whitespace (`x^y`, `a#b`).
    Compact,
    /// Factor operator (`*`, `/`, `%`, `**`): compact when both operands are
    /// adjacent factors (`a*b*c`), spaced when a non-factor or continuation
    /// needs air (`a * f(b)`).
    Factor,
    /// Unary prefix form (`-x`, `#x`): collapse only the trailing space.
    Prefix,
    /// Not a spacing-relevant operator; leave its surrounding whitespace as-is.
    None,
}

/// Spacing rule for the operator `node` carries, keyed by the parent's kind so
/// the same spelling can route differently in binary vs prefix context
/// (`-` in `BinaryExpression` is `Spaced`, in `PrefixExpression` is `Prefix`).
/// `LambdaExpression` shares the binary table: its `->` operator reads as a
/// spaced binary operator.
fn operator_spacing(parent_kind: NodeKind, operator: &str) -> OperatorSpacing {
    match parent_kind {
        NodeKind::BinaryExpression | NodeKind::LambdaExpression => {
            binary_operator_spacing(operator)
        }
        NodeKind::PrefixExpression => prefix_operator_spacing(operator),
        _ => OperatorSpacing::None,
    }
}

fn binary_operator_spacing(operator: &str) -> OperatorSpacing {
    // Factor operators participate in the adjacency rule before the static
    // spaced/compact tables.
    if matches!(operator, "*" | "/" | "%" | "**") {
        return OperatorSpacing::Factor;
    }
    if matches!(
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
            | "\\"
            | "\\\\"
            | ":="
            | "="
            | "<-"
            | "=>"
            | "->"
            | "//"
    ) {
        return OperatorSpacing::Spaced;
    }
    if matches!(
        operator,
        "·" | "⊠"
            | "⧢"
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
    ) {
        return OperatorSpacing::Compact;
    }
    OperatorSpacing::None
}

fn prefix_operator_spacing(operator: &str) -> OperatorSpacing {
    if matches!(
        operator,
        "+" | "-" | "*" | "#" | "<" | "<=" | ">" | ">=" | "?" | "<<" | "|-" | "<===" | "<==" | "??"
    ) {
        return OperatorSpacing::Prefix;
    }
    OperatorSpacing::None
}

/// Whether a `Factor`-classified binary operator collapses to compact form:
/// `true` iff both operands are adjacent factors (atoms or sub-expressions),
/// in which case spacing (`a * b`) would visually break a tight product such as
/// `2*x*y`. Otherwise a space is required to separate the operands.
fn binary_operator_all_factors(node: M2Node<'_>) -> bool {
    let Some(left) = node.child_by_field_name("left") else {
        return false;
    };
    let Some(right) = node.child_by_field_name("right") else {
        return false;
    };
    is_adjacent_factor(left) && is_adjacent_factor(right)
}

/// Whether `operator`, when it dangles at the end of a line, takes a trailing
/// space and so signals that the following row is an indented continuation.
/// Used by the tree-driven indenter to decide right-operand indentation. The
/// line-final set is exactly the binary operators classified `Spaced` above
/// (Factor operators are excluded: a compact `*` left at line-end `a*\nb` does
/// not indent). Derived from `binary_operator_spacing` so a new spaced operator
/// cannot be added to one table and forgotten in the other.
fn is_spaced_line_final_operator(operator: &str, compact_factor_operators: bool) -> bool {
    match binary_operator_spacing(operator) {
        OperatorSpacing::Spaced => true,
        OperatorSpacing::Factor => !compact_factor_operators,
        _ => false,
    }
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
fn is_method_installation_call_head(node: M2Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !parent.is(NodeKind::BinaryExpression) {
        return false;
    }
    let Some(operator) = parent.child_by_field_name("operator") else {
        return false;
    };
    if operator.text() != ":=" {
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
    let replacement = if comma.comma_borders_empty_slot() {
        ""
    } else {
        match text.as_bytes().get(end_byte) {
            Some(b')' | b']' | b'}' | b';') | None => "",
            _ => " ",
        }
    };

    edits.push(FormatEdit {
        start_byte: comma.end_byte(),
        end_byte,
        replacement,
    });
}

fn push_semicolon_whitespace_edits(
    text: &str,
    semicolon: M2Node<'_>,
    break_after_semicolon: bool,
    edits: &mut Vec<FormatEdit>,
) {
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
        let replacement = if break_after_semicolon {
            "\n"
        } else if matches!(next_byte, b')' | b']' | b'}') {
            ""
        } else {
            " "
        };
        edits.push(FormatEdit {
            start_byte: semicolon.end_byte(),
            end_byte,
            replacement,
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
    fn folds_consecutive_full_line_comments_as_one_range() {
        assert_eq!(
            folding_ranges_for_text(
                "-- first line\n  -- second line\n-- third line\nx = 1\n-- lone line\n"
            ),
            vec![FormatFoldRange {
                start_line: 0,
                end_line: 2,
                kind: FormatFoldKind::Comment,
            }]
        );
    }

    #[test]
    fn keeps_separate_line_comment_blocks_separate() {
        assert_eq!(
            folding_ranges_for_text("-- one\n-- two\n\n-- three\n-- four\n"),
            vec![
                FormatFoldRange {
                    start_line: 0,
                    end_line: 1,
                    kind: FormatFoldKind::Comment,
                },
                FormatFoldRange {
                    start_line: 3,
                    end_line: 4,
                    kind: FormatFoldKind::Comment,
                },
            ]
        );
    }

    #[test]
    fn folds_each_multiline_block_comment() {
        assert_eq!(
            folding_ranges_for_text("-* first\nmiddle\nlast *-\nx = 1\n-* another\ncomment *-\n"),
            vec![
                FormatFoldRange {
                    start_line: 0,
                    end_line: 2,
                    kind: FormatFoldKind::Comment,
                },
                FormatFoldRange {
                    start_line: 4,
                    end_line: 5,
                    kind: FormatFoldKind::Comment,
                },
            ]
        );
    }

    #[test]
    fn marks_comment_folds_for_lsp_clients() {
        let ranges = folding_ranges("-- first\n-- second\n");

        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].kind, Some(FoldingRangeKind::Comment));
    }

    #[test]
    fn does_not_treat_inline_or_string_markers_as_comment_blocks() {
        assert!(
            folding_ranges_for_text("x = \"-- not a comment\"\ny = 1 -- inline\n-- lone\n")
                .is_empty()
        );
    }

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
        assert_eq!(format_document_text("8*delta/2\n"), "8 * delta / 2\n");
        assert_eq!(format_document_text("x**y // z\n"), "x ** y // z\n");
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
        assert_eq!(
            format_document_text("values := (,a,,)\n"),
            "values := (,a,,)\n",
            "explicit null slots remain visually empty"
        );
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
    fn can_restore_compact_factor_operators() {
        let options = FormatOptions {
            compact_factor_operators: true,
            ..FormatOptions::default()
        };

        assert_eq!(
            format_document_text_with_options("8 * delta / 2\n", &options),
            "8*delta/2\n"
        );
    }

    #[test]
    fn can_keep_semicolon_separated_statements_inline() {
        let options = FormatOptions {
            break_after_semicolon: false,
            ..FormatOptions::default()
        };

        assert_eq!(
            format_document_text_with_options("i=0;j=0;\n", &options),
            "i = 0; j = 0;\n"
        );
    }

    #[test]
    fn server_settings_override_lsp_format_options() {
        let settings = crate::settings::ServerSettings::from_value(&serde_json::json!({
            "formatting": {
                "indentWidth": 2,
                "useTabs": false,
                "compactFactorOperators": true,
                "breakAfterSemicolon": false
            }
        }))
        .unwrap();
        let options = FormatOptions::from_configuration(8, false, settings.formatting());

        assert_eq!(
            format_document_text_with_options("f := (\na * b;c\n)\n", &options),
            "f := (\n  a*b; c\n)\n"
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
        assert_eq!(
            format_document_text("  x=///first line  \n  keep spaces  \n///\n"),
            "x = ///first line  \n  keep spaces  \n///\n",
            "formatting the prefix must not trim content before a literal newline"
        );
    }

    #[test]
    fn preserves_crlf_line_endings() {
        // A CRLF document must keep CRLF: the formatter must not silently rewrite
        // every line ending to LF (which would churn the whole file on save).
        assert_eq!(format_document_text("x=1\r\ny=2\r\n"), "x = 1\r\ny = 2\r\n");
        // Verbatim multiline-string content keeps its CRLF too.
        assert_eq!(
            format_document_text("x=///\r\n  keep  \r\n///\r\n"),
            "x = ///\r\n  keep  \r\n///\r\n"
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
        let formatted =
            format_document_text(include_str!("../../tests/fixtures/formatting_example.m2"));

        assert!(formatted.contains("k := ceiling((-3 + sqrt(9.0 + 8 * delta)) / 2);"));
        assert!(formatted.contains("K = ZZ / 101;"));
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
        let formatted =
            format_document_text(include_str!("../../tests/fixtures/comment_dividers.m2"));

        assert!(formatted.contains("\nprimitive = (L) -> ("));
        assert!(formatted.contains("\ntoZZ = (L) -> ("));
        assert!(!formatted.contains("\n     toZZ = (L) -> ("));
    }
}
