use tower_lsp::lsp_types::{
    DocumentFormattingOptions, FoldingRange, FoldingRangeProviderCapability, OneOf, TextEdit,
};
use tree_sitter::{Node, Parser};

use crate::capabilities::code_actions::{refactor_if_null_branch, refactor_try_statement};
use crate::node_metadata::{M2Node, NodeKind};
use crate::util::{binary_expression_operator_kind, full_document_range};

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
    let formatted = simplify_redundant_branches(text);
    let formatted = normalize_whitespace(&formatted);
    let formatted = reflow_standalone_ifs(&formatted);
    let formatted = normalize_multiline_closing_delimiters(&formatted);
    let mut formatted = formatted
        .lines()
        .scan(IndentState::default(), |state, line| {
            Some(indent_line(line, state, options))
        })
        .collect::<Vec<_>>()
        .join("\n");

    if text.ends_with('\n') {
        formatted.push('\n');
    }

    formatted
}

fn simplify_redundant_branches(text: &str) -> String {
    let mut current = text.to_string();

    loop {
        let Some(next) = simplify_redundant_branches_once(&current) else {
            return current;
        };
        if next == current {
            return current;
        }
        current = next;
    }
}

fn simplify_redundant_branches_once(text: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_macaulay2::language())
        .ok()?;
    let tree = parser.parse(text, None)?;
    let root = tree.root_node();

    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut best_rewrite: Option<(usize, usize, String)> = None;

    while let Some(node) = stack.pop() {
        let replacement = branch_simplification(node, text);
        if let Some(replacement) = replacement {
            let candidate = (node.start_byte(), node.end_byte(), replacement);
            let should_replace = best_rewrite
                .as_ref()
                .is_none_or(|(best_start, best_end, _)| {
                    let best_len = best_end - best_start;
                    let candidate_len = candidate.1 - candidate.0;
                    candidate_len <= best_len
                });
            if should_replace {
                best_rewrite = Some(candidate);
            }
        }

        stack.extend(node.children(&mut cursor));
    }

    let (start, end, replacement) = best_rewrite?;
    let mut rewritten = String::with_capacity(text.len() - (end - start) + replacement.len());
    rewritten.push_str(&text[..start]);
    rewritten.push_str(&replacement);
    rewritten.push_str(&text[end..]);
    Some(rewritten)
}

fn branch_simplification(node: Node<'_>, text: &str) -> Option<String> {
    match node.kind() {
        "if_statement" => refactor_if_null_branch(node, text),
        "try_statement" => refactor_try_statement(node, text),
        _ => None,
    }
}

/// Reflow standalone `if` statements onto multiple lines: break after `then`,
/// and for branches with an `else`, wrap each then-body in parentheses so the
/// `) else` keeps `else` off the start of a line (a newline before `else` is a
/// syntax error at global scope; the parens make one layout legal everywhere).
/// Ternary `if`-expressions used as a value (e.g. RHS of `:=`) are left alone.
/// The emitted form is flat — the downstream indent pass adds the indentation.
fn reflow_standalone_ifs(text: &str) -> String {
    let mut current = text.to_string();
    loop {
        let Some(next) = reflow_standalone_ifs_once(&current) else {
            return current;
        };
        if next == current {
            return current;
        }
        current = next;
    }
}

fn reflow_standalone_ifs_once(text: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_macaulay2::language())
        .ok()?;
    let tree = parser.parse(text, None)?;
    let root = tree.root_node();

    // Outermost-first: a chain's `else if` inner statements are reflowed as part
    // of their parent, so we rewrite the if with the smallest start byte first.
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut best: Option<(usize, usize, String)> = None;
    while let Some(node) = stack.pop() {
        if let Some(canonical) = reflowed_statement(node, text) {
            if canonical != text[node.start_byte()..node.end_byte()] {
                let candidate = (node.start_byte(), node.end_byte(), canonical);
                let take = best
                    .as_ref()
                    .is_none_or(|(start, _, _)| candidate.0 < *start);
                if take {
                    best = Some(candidate);
                }
            }
        }
        stack.extend(node.children(&mut cursor));
    }

    let (start, end, replacement) = best?;
    let mut rewritten = String::with_capacity(text.len() - (end - start) + replacement.len());
    rewritten.push_str(&text[..start]);
    rewritten.push_str(&replacement);
    rewritten.push_str(&text[end..]);
    Some(rewritten)
}

/// Reflow a standalone control statement (`if`/`for`/`while`) to its canonical
/// multi-line form, or `None` if it is not a candidate (a value sub-expression,
/// or a body that is a single atom that reads fine inline).
fn reflowed_statement(node: Node<'_>, text: &str) -> Option<String> {
    match node.kind() {
        "if_statement"
            if is_standalone_statement(node, text) && if_chain_has_block_body(node, text) =>
        {
            canonical_if(node, text)
        }
        "for_statement" | "while_statement" if is_standalone_statement(node, text) => {
            canonical_loop(node, text)
        }
        _ => None,
    }
}

/// A standalone control statement begins its own line (only whitespace before
/// it) and is a statement, not a value sub-expression. The line-start test also
/// excludes the `else if` members of a chain (their line begins with `else`).
fn is_standalone_statement(node: Node<'_>, text: &str) -> bool {
    let start = node.start_byte();
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    if !text[line_start..start].trim().is_empty() {
        return false;
    }
    // A ternary `if` (or loop used as a value) is an operand of an expression
    // (assignment RHS, operator argument), reached through expression parents.
    !matches!(
        node.parent().map(|parent| parent.kind()),
        Some("binary_expression")
            | Some("assignment_expression")
            | Some("prefix_expression")
            | Some("postfix_expression")
            | Some("lambda_expression")
    )
}

/// Whether any branch body in an `if` chain is a block worth breaking onto its
/// own line — i.e. not a single atom (`if ok then a else b` reads fine inline).
fn if_chain_has_block_body(node: Node<'_>, text: &str) -> bool {
    let Some(then_body) = if_clause_expression(node, "then_clause") else {
        return false;
    };
    if !is_atom_expression(then_body) {
        return true;
    }
    match if_clause_expression(node, "else_clause") {
        None => false,
        Some(else_body) if else_body.kind() == "if_statement" => {
            if_chain_has_block_body(else_body, text)
        }
        Some(else_body) => !is_atom_expression(else_body),
    }
}

/// A single-token body: an identifier or literal, with no call/operator/group
/// structure. These read fine on the same line as `then`/`do`.
fn is_atom_expression(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "symbol" | "resolved_symbol" | "integer_literal" | "float_literal" | "string_literal"
    )
}

/// Reflow a standalone `for`/`while` loop: break after its body keyword
/// (`do`, or `list` when there is no `do`). No `else` follows the body, so no
/// wrapping parentheses are needed. Single-atom bodies are left inline.
fn canonical_loop(node: Node<'_>, text: &str) -> Option<String> {
    let body_clause = loop_body_clause(node)?;
    let body = body_clause.named_child(0)?;
    if is_atom_expression(body) {
        return None;
    }
    let keyword = body_clause.child(0)?;
    let header = text[node.start_byte()..keyword.end_byte()].trim_end();
    Some(format!("{header}{}", broken_body(body, text)))
}

/// The clause carrying a loop's body: the `do` clause, or the `list` clause when
/// the loop has no `do`.
fn loop_body_clause<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let clauses: Vec<Node<'tree>> = node.children(&mut cursor).collect();
    clauses
        .iter()
        .find(|child| child.kind() == "do_clause")
        .or_else(|| clauses.iter().find(|child| child.kind() == "list_clause"))
        .copied()
}

/// Build the flat canonical multi-line form of an `if` chain.
fn canonical_if(node: Node<'_>, text: &str) -> Option<String> {
    let condition = node.child_by_field_name("condition")?;
    let condition_text = text[condition.start_byte()..condition.end_byte()].trim();
    let then_body = if_clause_expression(node, "then_clause")?;

    let Some(else_clause_expr) = if_clause_expression(node, "else_clause") else {
        // No else: breaking after `then` is always legal, no parens needed. A
        // block body keeps its own parens; a plain body just moves to the next
        // line.
        return Some(format!(
            "if {condition_text} then{}",
            broken_body(then_body, text)
        ));
    };

    // The protective parens around the then-body exist only to keep `else` off
    // the start of a line, which is a syntax error at global scope. Inside any
    // bracketing (parens/braces/brackets) a newline before `else` is legal, so
    // there we break after `then` and let `else` begin its own line unwrapped.
    if !is_at_global_scope(node) {
        let then_part = broken_body(then_body, text);
        return if else_clause_expr.kind() == "if_statement" {
            let tail = canonical_if(else_clause_expr, text)?;
            Some(format!("if {condition_text} then{then_part}\nelse {tail}"))
        } else {
            Some(format!(
                "if {condition_text} then{then_part}\nelse{}",
                broken_body(else_clause_expr, text)
            ))
        };
    }

    let then_inner = if_body_inner(then_body, text);
    if else_clause_expr.kind() == "if_statement" {
        let tail = canonical_if(else_clause_expr, text)?;
        Some(format!(
            "if {condition_text} then (\n{then_inner}\n) else {tail}"
        ))
    } else {
        Some(format!(
            "if {condition_text} then (\n{then_inner}\n) else{}",
            broken_body(else_clause_expr, text)
        ))
    }
}

/// Whether an `if` statement sits at global (top-level) scope, where a newline
/// before `else` is a syntax error. Only true bracketing — parentheses
/// (`sequence`), braces (`list`), or brackets (`array`) — introduces a local
/// scope; clause bodies and loop bodies do not.
fn is_at_global_scope(node: Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(parent.kind(), "sequence" | "list" | "array") {
            return false;
        }
        current = parent;
    }
    true
}

/// Render a body that follows a clause keyword with no trailing `else` to
/// protect: a parenthesized block becomes ` (\n…\n)` (keeping it attached to the
/// keyword line), any other body becomes a plain `\n<body>` on the next line.
fn broken_body(expr: Node<'_>, text: &str) -> String {
    if expr.kind() == "sequence" {
        format!(" (\n{}\n)", if_body_inner(expr, text))
    } else {
        format!("\n{}", text[expr.start_byte()..expr.end_byte()].trim())
    }
}

/// The body expression of a named clause (`then_clause`/`else_clause`) of an
/// `if` statement, i.e. the clause's expression after its keyword.
fn if_clause_expression<'tree>(node: Node<'tree>, clause_kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let clause = node
        .children(&mut cursor)
        .find(|child| child.kind() == clause_kind)?;
    clause.named_child(0)
}

/// The text of a then-body to place inside `then ( … )`, with one existing layer
/// of wrapping parentheses stripped so re-formatting does not accumulate them.
fn if_body_inner(expr: Node<'_>, text: &str) -> String {
    let body = &text[expr.start_byte()..expr.end_byte()];
    if expr.kind() == "sequence" && body.starts_with('(') && body.ends_with(')') {
        body[1..body.len() - 1].trim().to_string()
    } else {
        body.trim().to_string()
    }
}

pub fn folding_ranges_for_text(text: &str) -> Vec<FormatFoldRange> {
    let mut state = IndentState::default();
    let indented_lines = text
        .lines()
        .enumerate()
        .filter_map(|(line_number, line)| {
            let indent = line_indent(line, &mut state);
            (!indent.is_blank).then_some(IndentedLine {
                line: line_number as u32,
                depth: indent.depth,
            })
        })
        .collect::<Vec<_>>();

    collect_indent_fold_ranges(&indented_lines)
}

#[derive(Debug, Default)]
struct IndentState {
    delimiters: Vec<TrackedDelimiter>,
    next_group_id: u32,
    in_block_comment: bool,
    continuation_indent: bool,
    literal: LiteralState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DelimiterGroupId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackedDelimiter {
    kind: TrackedDelimiterKind,
    group_id: Option<DelimiterGroupId>,
    special_closer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackedDelimiterKind {
    Paren,
    Brace,
    Bracket,
    AngleBarList,
}

fn indent_line(line: &str, state: &mut IndentState, options: &FormatOptions) -> String {
    if state.literal != LiteralState::None {
        update_literal_state(line, state);
        return line.to_string();
    }

    let line = trim_line_end_preserving_operator_space(line.trim_start());
    if line.is_empty() {
        state.continuation_indent = false;
        return String::new();
    }

    let leading_group_closes = leading_group_closing_count(line, state);
    let mut line_depth = active_group_count(state).saturating_sub(leading_group_closes);
    let is_continuation = state.continuation_indent || starts_with_continuation_keyword(line);
    if is_continuation && leading_group_closes == 0 {
        line_depth += 1;
    }
    let mut indented = options.indent.repeat(line_depth);
    indented.push_str(line);
    update_indent_state(line, state);
    let trimmed = code_before_line_comment(line).trim_end_matches([' ', '\t']);
    if state.literal == LiteralState::None {
        state.continuation_indent =
            line_final_operator(trimmed).is_some_and(is_spaced_line_final_operator);
        trim_line_end_preserving_operator_space(&indented).to_string()
    } else {
        indented
    }
}

fn active_group_count(state: &IndentState) -> usize {
    let mut groups = Vec::new();
    for delimiter in &state.delimiters {
        if let Some(group_id) = delimiter.group_id {
            if !groups.contains(&group_id) {
                groups.push(group_id);
            }
        }
    }
    groups.len()
}

fn leading_group_closing_count(line: &str, state: &IndentState) -> usize {
    let bytes = line.as_bytes();
    let mut delimiters = state.delimiters.clone();
    let mut groups = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        let kind = if bytes[index..].starts_with(b"|>") {
            index += 2;
            TrackedDelimiterKind::AngleBarList
        } else {
            let Some(kind) = closing_delimiter_kind(bytes[index]) else {
                break;
            };
            index += 1;
            kind
        };

        if let Some(group_id) = pop_matching_tracked_delimiter(&mut delimiters, kind)
            .and_then(|delimiter| delimiter.group_id)
        {
            if !groups.contains(&group_id) {
                groups.push(group_id);
            }
        }
    }

    groups.len()
}

fn update_indent_state(line: &str, state: &mut IndentState) {
    let bytes = line.as_bytes();
    let trailing_openers = trailing_open_delimiter_starts(line);
    let mut index = 0;
    while index < bytes.len() {
        if state.in_block_comment {
            if bytes[index..].starts_with(b"*-") {
                state.in_block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        if bytes[index..].starts_with(b"--") {
            break;
        }
        if bytes[index..].starts_with(b"-*") {
            state.in_block_comment = true;
            index += 2;
            continue;
        }
        if bytes[index..].starts_with(b"///") {
            let (next_index, closed) = skip_raw_string(bytes, index);
            if !closed {
                state.literal = LiteralState::RawString;
            }
            index = next_index;
            continue;
        }

        match bytes[index] {
            b'"' => {
                let (next_index, closed) = skip_string(bytes, index);
                if !closed {
                    state.literal = LiteralState::String;
                }
                index = next_index;
            }
            b'(' | b'{' | b'[' => {
                state.delimiters.push(TrackedDelimiter {
                    kind: opening_delimiter_kind(bytes[index])
                        .expect("opening delimiter byte should map to kind"),
                    group_id: None,
                    special_closer: trailing_openers.contains(&index),
                });
                index += 1;
            }
            b'<' if bytes[index..].starts_with(b"<|") => {
                state.delimiters.push(TrackedDelimiter {
                    kind: TrackedDelimiterKind::AngleBarList,
                    group_id: None,
                    special_closer: trailing_openers.contains(&index),
                });
                index += 2;
            }
            b')' | b'}' | b']' => {
                let kind = closing_delimiter_kind(bytes[index])
                    .expect("closing delimiter byte should map to kind");
                pop_matching_tracked_delimiter(&mut state.delimiters, kind);
                index += 1;
            }
            b'|' if bytes[index..].starts_with(b"|>") => {
                pop_matching_tracked_delimiter(
                    &mut state.delimiters,
                    TrackedDelimiterKind::AngleBarList,
                );
                index += 2;
            }
            _ => index += 1,
        }
    }

    // Any delimiter still open without a group was opened on this line: a line
    // that both closes and reopens a scope (e.g. `) else (`) leaves the stack
    // length unchanged, so keying off `len > line_start_stack_len` would miss
    // the freshly opened delimiter. Grouping every ungrouped opener under one id
    // keeps multiple trailing openers on a line at a single indent level.
    if state
        .delimiters
        .iter()
        .any(|delimiter| delimiter.group_id.is_none())
    {
        let group_id = DelimiterGroupId(state.next_group_id);
        state.next_group_id += 1;
        for delimiter in &mut state.delimiters {
            if delimiter.group_id.is_none() {
                delimiter.group_id = Some(group_id);
            }
        }
    }
}

fn opening_delimiter_kind(byte: u8) -> Option<TrackedDelimiterKind> {
    match byte {
        b'(' => Some(TrackedDelimiterKind::Paren),
        b'{' => Some(TrackedDelimiterKind::Brace),
        b'[' => Some(TrackedDelimiterKind::Bracket),
        _ => None,
    }
}

fn closing_delimiter_kind(byte: u8) -> Option<TrackedDelimiterKind> {
    match byte {
        b')' => Some(TrackedDelimiterKind::Paren),
        b'}' => Some(TrackedDelimiterKind::Brace),
        b']' => Some(TrackedDelimiterKind::Bracket),
        _ => None,
    }
}

fn pop_matching_tracked_delimiter(
    delimiters: &mut Vec<TrackedDelimiter>,
    expected_kind: TrackedDelimiterKind,
) -> Option<TrackedDelimiter> {
    while let Some(delimiter) = delimiters.pop() {
        if delimiter.kind == expected_kind {
            return Some(delimiter);
        }
    }
    None
}

fn trailing_open_delimiter_starts(line: &str) -> Vec<usize> {
    let code = code_before_line_comment(line).trim_end_matches([' ', '\t']);
    let bytes = code.as_bytes();
    let mut starts = Vec::new();
    let mut index = bytes.len();

    while index > 0 {
        if index >= 2 && bytes[index - 2..index] == *b"<|" {
            starts.push(index - 2);
            index -= 2;
            continue;
        }

        match bytes[index - 1] {
            b'(' | b'{' | b'[' => {
                starts.push(index - 1);
                index -= 1;
            }
            _ => break,
        }
    }

    starts
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum LiteralState {
    #[default]
    None,
    String,
    RawString,
}

fn update_literal_state(line: &str, state: &mut IndentState) {
    let bytes = line.as_bytes();
    match state.literal {
        LiteralState::None => {}
        LiteralState::String => {
            let (_, closed) = skip_string_contents(bytes, 0);
            if closed {
                state.literal = LiteralState::None;
            }
        }
        LiteralState::RawString => {
            if bytes.windows(3).any(|window| window == b"///") {
                state.literal = LiteralState::None;
            }
        }
    }
}

fn skip_string(bytes: &[u8], start: usize) -> (usize, bool) {
    skip_string_contents(bytes, start + 1)
}

fn skip_string_contents(bytes: &[u8], start: usize) -> (usize, bool) {
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'"' => return (index + 1, true),
            _ => index += 1,
        }
    }
    (bytes.len(), false)
}

fn skip_raw_string(bytes: &[u8], start: usize) -> (usize, bool) {
    let mut index = start + 3;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"///") {
            return (index + 3, true);
        }
        index += 1;
    }
    (bytes.len(), false)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineIndent {
    depth: usize,
    is_blank: bool,
}

#[derive(Debug, Clone, Copy)]
struct OpenFoldRange {
    start_line: u32,
    depth: usize,
}

fn line_indent(line: &str, state: &mut IndentState) -> LineIndent {
    if state.literal != LiteralState::None {
        update_literal_state(line, state);
        return LineIndent {
            depth: active_group_count(state)
                .saturating_sub(leading_group_closing_count(line, state)),
            is_blank: line.trim().is_empty(),
        };
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        state.continuation_indent = false;
        return LineIndent {
            depth: active_group_count(state),
            is_blank: true,
        };
    }
    let leading_group_closes = leading_group_closing_count(trimmed, state);
    let mut line_depth = active_group_count(state).saturating_sub(leading_group_closes);
    let is_continuation = state.continuation_indent || starts_with_continuation_keyword(trimmed);
    if is_continuation && leading_group_closes == 0 {
        line_depth += 1;
    }
    update_indent_state(trimmed, state);
    let code = code_before_line_comment(trimmed).trim_end_matches([' ', '\t']);
    if state.literal == LiteralState::None {
        state.continuation_indent =
            line_final_operator(code).is_some_and(is_spaced_line_final_operator);
    }
    LineIndent {
        depth: line_depth,
        is_blank: false,
    }
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

#[derive(Debug, Clone, Copy)]
struct Delimiter {
    kind: DelimiterKind,
    line: usize,
    group_id: Option<DelimiterGroupId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelimiterKind {
    Paren,
    Brace,
    Bracket,
    AngleBarList,
}

fn normalize_multiline_closing_delimiters(text: &str) -> String {
    let mut state = ScanState::default();
    let mut stack = Vec::new();
    let mut edits = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut line = 0;
    let mut line_start = 0;

    while index < bytes.len() {
        if state.in_block_comment {
            if bytes[index..].starts_with(b"*-") {
                state.in_block_comment = false;
                index += 2;
            } else {
                (index, line, line_start) = advance_byte(bytes, index, line, line_start);
            }
            continue;
        }

        if bytes[index..].starts_with(b"--") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"-*") {
            state.in_block_comment = true;
            index += 2;
            continue;
        }
        if bytes[index..].starts_with(b"///") {
            (index, line, line_start) = skip_raw_string_with_lines(bytes, index, line, line_start);
            continue;
        }

        match bytes[index] {
            b'\n' => {
                line += 1;
                index += 1;
                line_start = index;
            }
            b'"' => {
                (index, line, line_start) = skip_string_with_lines(bytes, index, line, line_start);
            }
            b'(' => {
                stack.push(Delimiter {
                    kind: DelimiterKind::Paren,
                    line,
                    group_id: trailing_delimiter_group(text, line_start, index),
                });
                index += 1;
            }
            b'{' => {
                stack.push(Delimiter {
                    kind: DelimiterKind::Brace,
                    line,
                    group_id: trailing_delimiter_group(text, line_start, index),
                });
                index += 1;
            }
            b'[' => {
                stack.push(Delimiter {
                    kind: DelimiterKind::Bracket,
                    line,
                    group_id: trailing_delimiter_group(text, line_start, index),
                });
                index += 1;
            }
            b'<' if bytes[index..].starts_with(b"<|") => {
                stack.push(Delimiter {
                    kind: DelimiterKind::AngleBarList,
                    line,
                    group_id: trailing_delimiter_group(text, line_start, index),
                });
                index += 2;
            }
            b')' => {
                push_multiline_closer_edit(
                    text,
                    &mut stack,
                    DelimiterKind::Paren,
                    line,
                    line_start,
                    index,
                    &mut edits,
                );
                index += 1;
            }
            b'}' => {
                push_multiline_closer_edit(
                    text,
                    &mut stack,
                    DelimiterKind::Brace,
                    line,
                    line_start,
                    index,
                    &mut edits,
                );
                index += 1;
            }
            b']' => {
                push_multiline_closer_edit(
                    text,
                    &mut stack,
                    DelimiterKind::Bracket,
                    line,
                    line_start,
                    index,
                    &mut edits,
                );
                index += 1;
            }
            b'|' if bytes[index..].starts_with(b"|>") => {
                push_multiline_closer_edit(
                    text,
                    &mut stack,
                    DelimiterKind::AngleBarList,
                    line,
                    line_start,
                    index,
                    &mut edits,
                );
                index += 2;
            }
            _ => index += 1,
        }
    }

    apply_format_edits(text, edits)
}

#[derive(Debug, Default)]
struct ScanState {
    in_block_comment: bool,
}

fn push_multiline_closer_edit(
    text: &str,
    stack: &mut Vec<Delimiter>,
    expected_kind: DelimiterKind,
    line: usize,
    line_start: usize,
    closer_start: usize,
    edits: &mut Vec<FormatEdit>,
) {
    let Some(opener) = pop_matching_delimiter(stack, expected_kind) else {
        return;
    };
    if opener.group_id.is_none() {
        return;
    }
    if opener.line == line {
        return;
    }

    let prefix = &text[line_start..closer_start];
    if prefix.trim().is_empty() {
        return;
    }

    let edit_start = line_start + prefix.trim_end_matches([' ', '\t']).len();
    edits.push(FormatEdit {
        start_byte: edit_start,
        end_byte: closer_start,
        replacement: "\n",
    });
}

fn trailing_delimiter_group(
    text: &str,
    line_start: usize,
    opener_start: usize,
) -> Option<DelimiterGroupId> {
    let line = &text[line_start..];
    let line_end = line
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(text.len());
    let line_text = &text[line_start..line_end];
    let trailing = trailing_open_delimiter_starts(line_text);
    if trailing.contains(&(opener_start - line_start)) {
        return Some(DelimiterGroupId(line_start as u32));
    }

    let code = code_before_line_comment(line_text).trim_end_matches([' ', '\t']);
    let suffix = code
        .get((opener_start - line_start + 1)..)?
        .trim_end_matches([' ', '\t']);
    if suffix.ends_with(',') || suffix.ends_with(';') {
        return Some(DelimiterGroupId(line_start as u32));
    }
    if line_final_operator(suffix).is_some() {
        return Some(DelimiterGroupId(line_start as u32));
    }

    None
}

fn pop_matching_delimiter(
    stack: &mut Vec<Delimiter>,
    expected_kind: DelimiterKind,
) -> Option<Delimiter> {
    while let Some(delimiter) = stack.pop() {
        if delimiter.kind == expected_kind {
            return Some(delimiter);
        }
    }
    None
}

fn advance_byte(
    bytes: &[u8],
    index: usize,
    mut line: usize,
    mut line_start: usize,
) -> (usize, usize, usize) {
    let next_index = index + 1;
    if bytes[index] == b'\n' {
        line += 1;
        line_start = next_index;
    }
    (next_index, line, line_start)
}

fn skip_string_with_lines(
    bytes: &[u8],
    start: usize,
    mut line: usize,
    mut line_start: usize,
) -> (usize, usize, usize) {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'"' => return (index + 1, line, line_start),
            b'\n' => {
                line += 1;
                index += 1;
                line_start = index;
            }
            _ => index += 1,
        }
    }
    (bytes.len(), line, line_start)
}

fn skip_raw_string_with_lines(
    bytes: &[u8],
    start: usize,
    mut line: usize,
    mut line_start: usize,
) -> (usize, usize, usize) {
    let mut index = start + 3;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"///") {
            return (index + 3, line, line_start);
        }
        if bytes[index] == b'\n' {
            line += 1;
            index += 1;
            line_start = index;
        } else {
            index += 1;
        }
    }
    (bytes.len(), line, line_start)
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
    collect_format_edits(tree.root_node(), text, &mut edits);
    collect_line_final_operator_edits(text, &mut edits);
    apply_format_edits(text, edits)
}

fn collect_format_edits(node: tree_sitter::Node, text: &str, edits: &mut Vec<FormatEdit>) {
    if node.is_missing() {
        return;
    }

    if !node.is_error() {
        if node.kind() == "," {
            push_comma_whitespace_edits(text, node, edits);
        }

        if node.kind() == ";" {
            push_semicolon_whitespace_edits(text, node, edits);
        }

        if let Some(operator) = node.child_by_field_name("operator") {
            let operator_text = &text[operator.start_byte()..operator.end_byte()];
            if is_parenthesized_method_installation(node, text) {
                push_call_gap_whitespace_edit(node, text, edits, " ");
            } else if is_parenthesized_call(node) {
                push_call_whitespace_edits(node, text, edits);
            } else if should_space_factor_operator_with_adjacency_factor(node, operator_text) {
                push_operator_whitespace_edits(text, operator, edits);
            } else if should_compact_prefix_operator(node.kind(), operator_text) {
                push_prefix_operator_whitespace_edits(text, operator, edits);
            } else if should_compact_operator(node.kind(), operator_text) {
                push_compact_operator_whitespace_edits(text, operator, edits);
            } else if should_space_operator(node.kind(), operator_text) {
                push_operator_whitespace_edits(text, operator, edits);
            }
        }

        if node.kind() == "lambda_expression" {
            push_lambda_operator_whitespace_edits(node, text, edits);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_format_edits(child, text, edits);
    }
}

fn should_space_operator(parent_kind: &str, operator: &str) -> bool {
    match parent_kind {
        "assignment_expression"
        | "function_expression"
        | "option_assignment"
        | "option_attachment" => true,
        "binary_expression" => matches!(
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

fn should_compact_operator(parent_kind: &str, operator: &str) -> bool {
    parent_kind == "binary_expression" && is_compact_operator(operator)
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

fn should_compact_prefix_operator(parent_kind: &str, operator: &str) -> bool {
    parent_kind == "prefix_expression"
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

fn is_parenthesized_call(node: tree_sitter::Node) -> bool {
    if node.kind() != "binary_expression" {
        return false;
    }

    let Some(operator) = node.child_by_field_name("operator") else {
        return false;
    };
    let Some(right) = node.child_by_field_name("right") else {
        return false;
    };

    operator.kind() == "SPACE" && right.kind() == "sequence"
}

fn is_parenthesized_method_installation(node: tree_sitter::Node, text: &str) -> bool {
    if !M2Node::new(node).is(NodeKind::BinaryExpression) {
        return false;
    }
    let Some(operator) = node.child_by_field_name("operator") else {
        return false;
    };
    let Some(left) = node.child_by_field_name("left") else {
        return false;
    };
    if &text[operator.start_byte()..operator.end_byte()] != ":=" {
        return false;
    }
    M2Node::new(left).is(NodeKind::BinaryExpression)
        && binary_expression_operator_kind(left) == Some("SPACE")
}

fn push_call_gap_whitespace_edit(
    node: tree_sitter::Node,
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
    node: tree_sitter::Node,
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

fn is_adjacent_factor(node: tree_sitter::Node) -> bool {
    matches!(
        node.kind(),
        "symbol"
            | "integer_literal"
            | "float_literal"
            | "string_literal"
            | "sequence"
            | "list"
            | "array"
            | "angle_bar_list"
            | "prefix_expression"
            | "postfix_expression"
            | "member_prefix_expression"
            | "binary_expression"
    )
}

fn push_call_whitespace_edits(node: tree_sitter::Node, text: &str, edits: &mut Vec<FormatEdit>) {
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
    node: tree_sitter::Node,
    text: &str,
    edits: &mut Vec<FormatEdit>,
) {
    let Some(operator) = node.child_by_field_name("operator") else {
        return;
    };
    push_operator_whitespace_edits(text, operator, edits);
}

fn push_operator_whitespace_edits(
    text: &str,
    operator: tree_sitter::Node,
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
    operator: tree_sitter::Node,
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
    operator: tree_sitter::Node,
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

fn push_comma_whitespace_edits(text: &str, comma: tree_sitter::Node, edits: &mut Vec<FormatEdit>) {
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

fn push_semicolon_whitespace_edits(
    text: &str,
    semicolon: tree_sitter::Node,
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
        edits.push(FormatEdit {
            start_byte: semicolon.end_byte(),
            end_byte,
            replacement: "\n",
        });
    }
}

fn collect_line_final_operator_edits(text: &str, edits: &mut Vec<FormatEdit>) {
    let mut line_start = 0;
    let mut state = IndentState::default();
    for line_with_ending in text.split_inclusive('\n') {
        let line_end = line_start + line_with_ending.trim_end_matches(['\r', '\n']).len();
        let line = &text[line_start..line_end];
        if state.literal == LiteralState::None {
            push_line_final_operator_edit(text, line_start, line_end, edits);
            update_indent_state(line, &mut state);
        } else {
            update_literal_state(line, &mut state);
        }
        line_start += line_with_ending.len();
    }

    if !text.ends_with('\n') && line_start < text.len() && state.literal == LiteralState::None {
        push_line_final_operator_edit(text, line_start, text.len(), edits);
    }
}

fn push_line_final_operator_edit(
    text: &str,
    line_start: usize,
    line_end: usize,
    edits: &mut Vec<FormatEdit>,
) {
    let line = &text[line_start..line_end];
    let trimmed_end = line.trim_end_matches([' ', '\t']).len();
    if trimmed_end == 0 {
        return;
    }

    let code_end = code_before_line_comment(&line[..trimmed_end]).len();
    if code_end == 0 {
        return;
    }
    let trimmed_line = &line[..code_end];
    let Some(operator) = line_final_operator(trimmed_line) else {
        return;
    };
    let operator_start = line_start + code_end - operator.len();

    let replacement = if is_compact_operator(operator) {
        ""
    } else if is_spaced_line_final_operator(operator) {
        " "
    } else {
        return;
    };

    let mut before_start = operator_start;
    while before_start > line_start && matches!(text.as_bytes()[before_start - 1], b' ' | b'\t') {
        before_start -= 1;
    }
    edits.push(FormatEdit {
        start_byte: before_start,
        end_byte: operator_start,
        replacement,
    });

    let after_replacement = if is_compact_operator(operator) {
        ""
    } else {
        " "
    };
    edits.push(FormatEdit {
        start_byte: line_start + code_end,
        end_byte: line_start + code_end,
        replacement: after_replacement,
    });
}

fn code_before_line_comment(line: &str) -> &str {
    line.find("--")
        .map_or(line, |comment_start| &line[..comment_start])
}

/// A line opening with a continuation keyword (`else`/`then`) continues the
/// `if`/`try` expression begun on an earlier line, so it indents one level under
/// that statement even when the previous line did not end on a line-final
/// operator. The keyword must match as a whole word — `elseBranch` is an
/// identifier, not `else`.
fn starts_with_continuation_keyword(line: &str) -> bool {
    const CONTINUATION_KEYWORDS: &[&str] = &["else", "then"];
    CONTINUATION_KEYWORDS.iter().any(|keyword| {
        line.strip_prefix(keyword).is_some_and(|rest| {
            rest.bytes()
                .next()
                .is_none_or(|byte| !byte.is_ascii_alphanumeric() && byte != b'_')
        })
    })
}

fn line_final_operator(line: &str) -> Option<&'static str> {
    const OPERATORS: &[&str] = &[
        "<==>", "<==", "===>", "<===", "===", "=!=", "<=", ">=", "==", "!=", ":=", "<-", "=>",
        "->", "++", "||", "^^", "??", "\\\\", "\\", "+", "-", "=", "<", ">", "|", "&", "or", "xor",
        "and", "^**", "@@?", "@@", "|_", "^<=", "^>=", "_<=", "_>=", "**", "//", "·", "⊠", "⧢",
        "%", "/", "*", "@", "^<", "^>", "_<", "_>", "^", "_", "#?", "#", "then", "else", "do",
        "list",
    ];

    OPERATORS.iter().copied().find(|operator| {
        if !line.ends_with(operator) {
            return false;
        }
        if operator.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            let operator_start = line.len() - operator.len();
            return operator_start == 0
                || !line.as_bytes()[operator_start - 1].is_ascii_alphanumeric();
        }
        true
    })
}

fn trim_line_end_preserving_operator_space(line: &str) -> &str {
    let trimmed = line.trim_end_matches([' ', '\t']);
    if line.len() > trimmed.len()
        && line_final_operator(trimmed).is_some_and(is_spaced_line_final_operator)
    {
        &line[..trimmed.len() + 1]
    } else {
        trimmed
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
    fn normalizes_line_final_operator_whitespace() {
        assert_eq!(format_document_text("a+\nb\n"), "a + \n    b\n");
        assert_eq!(format_document_text("a   +\nb\n"), "a + \n    b\n");
        assert_eq!(format_document_text("a+   \nb\n"), "a + \n    b\n");
        assert_eq!(format_document_text("a*\nb\n"), "a*\nb\n");
        assert_eq!(format_document_text("a  *   \nb\n"), "a*\nb\n");
        assert_eq!(format_document_text("sand   \n"), "sand\n");
    }

    #[test]
    fn indents_operator_continuations() {
        let options = FormatOptions::new(3, true);

        assert_eq!(
            format_document_text_with_options("a + \nb +\nc\n", &options),
            "a + \n   b + \n   c\n"
        );
        assert_eq!(
            format_document_text_with_options("f := (a +\nb +\nc)\n", &options),
            "f := (a + \n      b + \n      c\n)\n"
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
        assert_eq!(format_document_text("f(x,\ny)\n"), "f(x,\n    y\n)\n");
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
    fn formatting_removes_redundant_if_null_branch() {
        assert_eq!(
            format_document_text("if ready then value else null\n"),
            "if ready then value\n"
        );
    }

    #[test]
    fn formatting_removes_redundant_try_else_null_branch() {
        assert_eq!(
            format_document_text("try value then result else null\n"),
            "try value then result\n"
        );
    }

    #[test]
    fn line_end_open_brackets_create_single_extra_indent_scope() {
        assert_eq!(
            format_document_text(
                "scan(docKeys, key -> (\nbaseName := recordNameFromDocKey key;\nif db#?key then result#baseName = append(result#baseName ?? {}, hashTable {\"key\" => key, \"raw\" => db#key});\n)\n);\n"
            ),
            "scan(docKeys, key -> (\n    baseName := recordNameFromDocKey key;\n    if db#?key then\n        result#baseName = append(result#baseName ?? {}, hashTable {\"key\" => key, \"raw\" => db#key});\n)\n);\n"
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
    fn puts_multiline_closing_brackets_on_their_own_line() {
        let options = FormatOptions::new(3, true);

        assert_eq!(
            format_document_text_with_options("f = x -> (\ns;\nb)\n", &options),
            "f = x -> (\n   s;\n   b\n)\n"
        );
        assert_eq!(format_document_text("f(x)\n"), "f(x)\n");
        assert_eq!(
            format_document_text("\"b)\" -- comment c)\n"),
            "\"b)\" -- comment c)\n"
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
    fn reflows_standalone_if_with_block_then_body() {
        assert_eq!(
            format_document_text("if a then f(x) else c\n"),
            "if a then (\n    f(x)\n) else\n    c\n"
        );
    }

    #[test]
    fn reflows_standalone_if_else_chain() {
        assert_eq!(
            format_document_text("if a then f(x) else if d then g(y) else h(z)\n"),
            "if a then (\n    f(x)\n) else if d then (\n    g(y)\n) else\n    h(z)\n"
        );
    }

    #[test]
    fn reflows_if_in_local_scope_without_protective_parens() {
        // Inside a function body (parens) a newline before `else` is legal, so the
        // then-body is not wrapped in protective parens.
        assert_eq!(
            format_document_text("f = () -> (\nif a then f(x) else c\n)\n"),
            "f = () -> (\n    if a then\n        f(x)\n        else\n        c\n)\n"
        );
        assert!(
            !format_document_text("f = () -> (\nif a then f(x) else c\n)\n").contains("then ("),
            "local-scope if must not introduce protective parens"
        );
    }

    #[test]
    fn keeps_protective_parens_for_nested_global_then_block() {
        // The outer `if` is global (parens required); the inner `if` lives inside
        // the outer then-block parens, so it is local (no protective parens).
        let formatted = format_document_text("if a then (\nif x then p(1) else q(2)\n) else w\n");
        assert!(formatted.starts_with("if a then (\n"));
        assert!(formatted.contains("    if x then\n        p(1)\n        else\n        q(2)\n"));
    }

    #[test]
    fn local_scope_if_reflow_is_idempotent() {
        for source in [
            "f = () -> (\nif a then f(x) else c\n)\n",
            "f = () -> (\nif a then f(x) else if d then g(y) else h(z)\n)\n",
            "if a then (\nif x then p(1) else q(2)\n) else w\n",
        ] {
            let once = format_document_text(source);
            assert_eq!(format_document_text(&once), once, "not idempotent: {source:?}");
        }
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
    fn reflows_loop_body_after_do_and_list() {
        assert_eq!(
            format_document_text("for i in L do compute(i)\n"),
            "for i in L do\n    compute(i)\n"
        );
        assert_eq!(
            format_document_text("for i in L list f(i)\n"),
            "for i in L list\n    f(i)\n"
        );
        assert_eq!(
            format_document_text("for i in L do x\n"),
            "for i in L do x\n"
        );
    }

    #[test]
    fn reflow_is_idempotent() {
        for source in [
            "if a then f(x) else if d then g(y) else h(z)\n",
            "for i in L do compute(i)\n",
            "if cond then (a;b) else (c;d)\n",
        ] {
            let once = format_document_text(source);
            assert_eq!(
                format_document_text(&once),
                once,
                "not idempotent: {source:?}"
            );
        }
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
        let chain = "extracted := if kind === \"function\" then\n\
             extractFunc(name, db)\n\
             else if kind === \"operator\" then extractOperator(name, db)\n\
             else extractObject(name, db);\n";
        let formatted = format_document_text(chain);

        // Every continuation line of the broken `if` expression sits one level in,
        // not just the first broken body.
        assert!(formatted.contains("\n    extractFunc(name, db)\n"));
        assert!(formatted
            .contains("\n    else if kind === \"operator\" then extractOperator(name, db)\n"));
        assert!(formatted.contains("\n    else extractObject(name, db);\n"));
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

