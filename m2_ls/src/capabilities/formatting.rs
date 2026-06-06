use tower_lsp::lsp_types::{
    DocumentFormattingOptions, FoldingRange, FoldingRangeProviderCapability, OneOf, TextEdit,
};
use tree_sitter::Parser;

use crate::util::full_document_range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOptions {
    indent: String,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            indent: "     ".to_string(),
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
    let formatted = normalize_whitespace(text);
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
    depth: usize,
    in_block_comment: bool,
    continuation_indent: bool,
    literal: LiteralState,
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

    let leading_closes = leading_closing_delimiters(line);
    let mut line_depth = state.depth.saturating_sub(leading_closes);
    if state.continuation_indent && leading_closes == 0 {
        line_depth += 1;
    }
    let mut indented = options.indent.repeat(line_depth);
    indented.push_str(line);
    update_indent_depth(line, state);
    let trimmed = code_before_line_comment(line).trim_end_matches([' ', '\t']);
    if state.literal == LiteralState::None {
        state.continuation_indent =
            line_final_operator(trimmed).is_some_and(is_spaced_line_final_operator);
        trim_line_end_preserving_operator_space(&indented).to_string()
    } else {
        indented
    }
}

fn leading_closing_delimiters(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut count = 0;
    while index < bytes.len() {
        match bytes[index] {
            b')' | b'}' | b']' => {
                count += 1;
                index += 1;
            }
            b'|' if bytes[index..].starts_with(b"|>") => {
                count += 1;
                index += 2;
            }
            _ => break,
        }
    }
    count
}

fn update_indent_depth(line: &str, state: &mut IndentState) {
    let bytes = line.as_bytes();
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
                state.depth += 1;
                index += 1;
            }
            b'<' if bytes[index..].starts_with(b"<|") => {
                state.depth += 1;
                index += 2;
            }
            b')' | b'}' | b']' => {
                state.depth = state.depth.saturating_sub(1);
                index += 1;
            }
            b'|' if bytes[index..].starts_with(b"|>") => {
                state.depth = state.depth.saturating_sub(1);
                index += 2;
            }
            _ => index += 1,
        }
    }
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
            depth: state.depth.saturating_sub(leading_closing_delimiters(line)),
            is_blank: line.trim().is_empty(),
        };
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        state.continuation_indent = false;
        return LineIndent {
            depth: state.depth,
            is_blank: true,
        };
    }
    let leading_closes = leading_closing_delimiters(trimmed);
    let mut line_depth = state.depth.saturating_sub(leading_closes);
    if state.continuation_indent && leading_closes == 0 {
        line_depth += 1;
    }
    update_indent_depth(trimmed, state);
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
                });
                index += 1;
            }
            b'{' => {
                stack.push(Delimiter {
                    kind: DelimiterKind::Brace,
                    line,
                });
                index += 1;
            }
            b'[' => {
                stack.push(Delimiter {
                    kind: DelimiterKind::Bracket,
                    line,
                });
                index += 1;
            }
            b'<' if bytes[index..].starts_with(b"<|") => {
                stack.push(Delimiter {
                    kind: DelimiterKind::AngleBarList,
                    line,
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
    if node.kind() != "binary_expression" {
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
    left.kind() == "binary_expression" && binary_expression_operator_kind(left) == Some("SPACE")
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

fn binary_expression_operator_kind(node: tree_sitter::Node<'_>) -> Option<&str> {
    if node.kind() != "binary_expression" {
        return None;
    }
    node.child_by_field_name("operator")
        .map(|operator| operator.kind())
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
            update_indent_depth(line, &mut state);
        } else {
            update_literal_state(line, &mut state);
        }
        line_start += line_with_ending.len();
    }

    if !text.ends_with('\n') && line_start < text.len() {
        if state.literal == LiteralState::None {
            push_line_final_operator_edit(text, line_start, text.len(), edits);
        }
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

fn line_final_operator(line: &str) -> Option<&'static str> {
    const OPERATORS: &[&str] = &[
        "<==>", "<==", "===>", "<===", "===", "=!=", "<=", ">=", "==", "!=", ":=", "<-", "=>",
        "->", "++", "||", "^^", "??", "\\\\", "\\", "+", "-", "=", "<", ">", "|", "&", "or", "xor",
        "and", "^**", "@@?", "@@", "|_", "^<=", "^>=", "_<=", "_>=", "**", "//", "·", "⊠", "⧢",
        "%", "/", "*", "@", "^<", "^>", "_<", "_>", "^", "_", "#?", "#", "then", "else", "do",
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
        assert_eq!(format_document_text("a+\nb\n"), "a + \n     b\n");
        assert_eq!(format_document_text("a   +\nb\n"), "a + \n     b\n");
        assert_eq!(format_document_text("a+   \nb\n"), "a + \n     b\n");
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
        assert_eq!(format_document_text("f(x,\ny)\n"), "f(x,\n     y\n)\n");
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
            "f := (i = 0;\n     j = 0;\n)\n"
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
    fn formats_valid_nodes_when_other_nodes_have_parse_errors() {
        assert_eq!(
            format_document_text("x+y\nbad := (\na:=b\n"),
            "x + y\nbad := (\n     a := b\n"
        );
    }

    #[test]
    fn formats_example_file_despite_parser_gaps() {
        let formatted = format_document_text(include_str!("../../../example_m2_code/example1.m2"));

        assert!(formatted.contains("k := ceiling((-3 + sqrt(9.0 + 8*delta))/2);"));
        assert!(formatted.contains("K = ZZ/101;"));
        assert!(formatted.contains("randomPlanePoints = (delta, R) -> ("));
        assert!(formatted.contains("random(source gens Ip2, R^{-d})"));
        assert!(formatted.contains("SyzygyLimit => 60"));
    }

    #[test]
    fn keeps_top_level_symbols_unindented_after_comment_dividers() {
        let formatted = format_document_text(include_str!("../../../example_m2_code/example2.m2"));

        assert!(formatted.contains("\nprimitive = (L) -> ("));
        assert!(formatted.contains("\ntoZZ = (L) -> ("));
        assert!(!formatted.contains("\n     toZZ = (L) -> ("));
    }
}
