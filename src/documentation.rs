//! Inline symbol references embedded in comments and documentation strings.
//!
//! Macaulay2 package sources commonly mention code objects with Markdown-style
//! single backticks (`name`) both in comments and in raw `doc ///...///`
//! strings. Tree-sitter deliberately treats those regions as opaque, so this
//! small secondary index records only the delimited spans; semantic resolution
//! remains owned by the ordinary analysis and package indexes.

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::Tree;

use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::util::{position_in_range, range_from_byte_span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DocumentationReference {
    start_byte: usize,
    end_byte: usize,
    range: Range,
}

impl DocumentationReference {
    pub(crate) fn name<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start_byte..self.end_byte]
    }

    pub(crate) fn range(&self) -> Range {
        self.range
    }

    pub(crate) fn byte_span(&self) -> (usize, usize) {
        (self.start_byte, self.end_byte)
    }

    pub(crate) fn contains(&self, position: Position) -> bool {
        position_in_range(position, self.range)
    }
}

pub(crate) fn collect_documentation_references(
    text: &str,
    tree: &Tree,
) -> Vec<DocumentationReference> {
    let root = M2Node::new(tree.root_node(), text);
    let mut references = Vec::new();

    for node in root.descendants() {
        if is_documentation_container(node) {
            collect_backtick_references(node, text, &mut references);
        }
    }

    references
}

fn is_documentation_container(node: M2Node<'_>) -> bool {
    node.kind.is_comment()
        || (node.kind == NodeKind::StringLiteral && node.text().starts_with("///"))
}

fn collect_backtick_references(
    node: M2Node<'_>,
    text: &str,
    references: &mut Vec<DocumentationReference>,
) {
    let container = node.text();
    let bytes = container.as_bytes();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let Some(relative_open) = container[cursor..].find('`') else {
            break;
        };
        let open = cursor + relative_open;

        // Double/triple backtick runs are prose/code delimiters rather than the
        // single-backtick reference convention.
        if bytes.get(open.wrapping_sub(1)) == Some(&b'`') || bytes.get(open + 1) == Some(&b'`') {
            cursor = open + 1;
            continue;
        }

        let content_start = open + 1;
        let Some(relative_close) = container[content_start..].find('`') else {
            break;
        };
        let close = content_start + relative_close;
        cursor = close + 1;

        if bytes.get(close + 1) == Some(&b'`') {
            continue;
        }

        let candidate = &container[content_start..close];
        if !is_reference_candidate(candidate) {
            continue;
        }

        let start_byte = node.start_byte() + content_start;
        let end_byte = node.start_byte() + close;
        references.push(DocumentationReference {
            start_byte,
            end_byte,
            range: range_from_byte_span(text, start_byte, end_byte),
        });
    }
}

/// Keep the documentation layer deliberately syntax-agnostic: it identifies a
/// compact code word, while the real analysis/index decides whether that word
/// resolves to an M2 object. This avoids maintaining a second identifier parser.
fn is_reference_candidate(candidate: &str) -> bool {
    !candidate.is_empty()
        && !candidate.contains(char::is_whitespace)
        && !candidate.contains(['`', '\n', '\r'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn references(text: &str) -> Vec<(String, Range)> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("Macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        collect_documentation_references(text, &tree)
            .into_iter()
            .map(|reference| (reference.name(text).to_string(), reference.range()))
            .collect()
    }

    #[test]
    fn indexes_single_backticks_in_comments_and_raw_doc_strings() {
        let text = concat!(
            "x = 1 -- See `x`, not ``example code``.\n",
            "doc ///Use `x` and `ideal`.///\n",
            "s = \"ordinary `x` string\"\n",
        );

        assert_eq!(
            references(text),
            vec![
                (
                    "x".to_string(),
                    Range::new(Position::new(0, 14), Position::new(0, 15)),
                ),
                (
                    "x".to_string(),
                    Range::new(Position::new(1, 12), Position::new(1, 13)),
                ),
                (
                    "ideal".to_string(),
                    Range::new(Position::new(1, 20), Position::new(1, 25)),
                ),
            ]
        );
    }

    #[test]
    fn reference_ranges_use_utf16_columns() {
        let text = "-- 😀 see `ideal`\n";
        assert_eq!(
            references(text),
            vec![(
                "ideal".to_string(),
                Range::new(Position::new(0, 11), Position::new(0, 16)),
            )]
        );
    }
}
