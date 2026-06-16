use tower_lsp::lsp_types::*;

pub(crate) fn utf16_col_to_byte(line: &str, utf16_col: u32) -> usize {
    let mut current_col = 0;

    for (byte_index, ch) in line.char_indices() {
        let next_col = current_col + ch.len_utf16() as u32;
        if next_col > utf16_col {
            return byte_index;
        }
        current_col = next_col;
    }

    line.len()
}

pub(crate) fn floor_char_boundary(text: &str, byte_index: usize) -> usize {
    let mut byte_index = byte_index.min(text.len());
    while byte_index > 0 && !text.is_char_boundary(byte_index) {
        byte_index -= 1;
    }
    byte_index
}

pub(crate) fn utf16_len_for_byte_span(text: &str, start_byte: usize, end_byte: usize) -> u32 {
    let start_byte = floor_char_boundary(text, start_byte);
    let end_byte = floor_char_boundary(text, end_byte.max(start_byte));
    text[start_byte..end_byte].encode_utf16().count() as u32
}

pub(crate) fn byte_index_from_lsp_position(text: &str, position: Position) -> Option<usize> {
    let mut line_start = 0usize;
    let mut current_line = 0u32;

    for segment in text.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        if current_line == position.line {
            return Some(line_start + utf16_col_to_byte(line, position.character));
        }
        line_start += segment.len();
        current_line += 1;
    }

    if current_line == position.line {
        return Some(text.len());
    }

    None
}

pub(crate) fn tree_sitter_point_from_lsp_position(
    text: &str,
    position: Position,
) -> Option<tree_sitter::Point> {
    let byte_index = byte_index_from_lsp_position(text, position)?;
    Some(tree_sitter_point_from_byte_index(text, byte_index))
}

pub(crate) fn tree_sitter_point_from_byte_index(
    text: &str,
    byte_index: usize,
) -> tree_sitter::Point {
    let byte_index = floor_char_boundary(text, byte_index);
    let prefix = &text[..byte_index];
    let row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map(|index| index + 1).unwrap_or(0);
    tree_sitter::Point::new(row, byte_index.saturating_sub(line_start))
}

pub(crate) fn node_range(text: &str, node: tree_sitter::Node) -> Range {
    let range = node.range();
    let start_line_byte = range.start_byte.saturating_sub(range.start_point.column);
    let end_line_byte = range.end_byte.saturating_sub(range.end_point.column);

    Range::new(
        Position::new(
            range.start_point.row as u32,
            utf16_len_for_byte_span(text, start_line_byte, range.start_byte),
        ),
        Position::new(
            range.end_point.row as u32,
            utf16_len_for_byte_span(text, end_line_byte, range.end_byte),
        ),
    )
}

pub(crate) fn full_document_range(text: &str) -> Range {
    let mut lines = text.lines();
    let Some(mut last_line) = lines.next() else {
        return Range::new(Position::new(0, 0), Position::new(0, 0));
    };

    let mut line_count = 1;
    for line in lines {
        last_line = line;
        line_count += 1;
    }

    if text.ends_with('\n') {
        Range::new(Position::new(0, 0), Position::new(line_count, 0))
    } else {
        Range::new(
            Position::new(0, 0),
            Position::new(line_count - 1, last_line.encode_utf16().count() as u32),
        )
    }
}

pub(crate) fn binary_expression_operator_kind(node: tree_sitter::Node<'_>) -> Option<&str> {
    if !matches!(node.kind(), "binary_expression" | "comparison_expression") {
        return None;
    }

    node.child_by_field_name("operator")
        .map(|operator| operator.kind())
}

pub(crate) fn binary_expression_operator<'a>(
    node: tree_sitter::Node,
    text: &'a str,
) -> Option<&'a str> {
    if !matches!(node.kind(), "binary_expression" | "comparison_expression") {
        return None;
    }

    node.child_by_field_name("operator")
        .map(|operator| &text[operator.start_byte()..operator.end_byte()])
}

pub(crate) fn is_assignment_expression(node: tree_sitter::Node<'_>, text: &str) -> bool {
    node.kind() == "binary_expression"
        && matches!(
            binary_expression_operator(node, text),
            Some("=" | ":=" | "<-")
        )
}

pub(crate) fn is_option_assignment_expression(node: tree_sitter::Node<'_>, text: &str) -> bool {
    node.kind() == "binary_expression" && binary_expression_operator(node, text) == Some("=>")
}

pub(crate) fn is_space_operator_expression(node: tree_sitter::Node<'_>) -> bool {
    node.kind() == "binary_expression" && binary_expression_operator_kind(node) == Some("SPACE")
}

pub(crate) fn is_operator_node(node: tree_sitter::Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent
        .child_by_field_name("operator")
        .is_some_and(|operator| operator.id() == node.id())
}

pub(crate) fn node_is_within(ancestor: tree_sitter::Node, node: tree_sitter::Node) -> bool {
    ancestor.start_byte() <= node.start_byte() && node.end_byte() <= ancestor.end_byte()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_sitter_points_convert_utf16_to_byte_columns() {
        let point = tree_sitter_point_from_lsp_position("é ideal", Position::new(0, 3))
            .expect("position should be on the first line");
        assert_eq!(point.column, 4);

        let point = tree_sitter_point_from_lsp_position("😀 ideal", Position::new(0, 3))
            .expect("position should be on the first line");
        assert_eq!(point.column, 5);
    }

    #[test]
    fn semantic_token_spans_use_utf16_units() {
        let text = "😀 ideal";
        let start = text.find("ideal").expect("fixture should contain token");
        let end = start + "ideal".len();

        assert_eq!(utf16_len_for_byte_span(text, 0, start), 3);
        assert_eq!(utf16_len_for_byte_span(text, start, end), 5);
    }

    #[test]
    fn full_document_range_handles_utf16_columns() {
        assert_eq!(
            full_document_range("x\n😀 ideal"),
            Range::new(Position::new(0, 0), Position::new(1, 8))
        );
        assert_eq!(
            full_document_range("x\n"),
            Range::new(Position::new(0, 0), Position::new(1, 0))
        );
    }
}
