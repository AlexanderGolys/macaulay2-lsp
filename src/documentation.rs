//! Inline Macaulay2 snippets embedded in comments and documentation strings.
//!
//! Macaulay2 package sources commonly mention code with Markdown-style single
//! backticks both in comments and in raw `doc ///...///` strings. Tree-sitter
//! deliberately treats those regions as opaque, so each span receives a small
//! isolated parse for reference extraction.

use tower_lsp::lsp_types::{Position, Range as TextRange};

use crate::node_metadata::{M2Node, M2Parser, NodeKind, NodeKindMetadata};
use crate::source::{ByteRange, DocumentSpan, SourceNavigation};
use crate::util::position_in_range;

/// One backtick-delimited source snippet parsed for embedded references.
#[derive(Debug)]
pub(crate) struct DocumentationSnippet {
    bytes: ByteRange,
}

impl DocumentationSnippet {
    pub(crate) fn byte_span(&self) -> (usize, usize) {
        (self.bytes.start, self.bytes.end)
    }
}

/// One symbol mention extracted from an otherwise opaque documentation region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocumentationReference {
    span: DocumentSpan,
}

impl DocumentationReference {
    pub(crate) fn name<'a>(&self, text: &'a str) -> &'a str {
        &text[self.span.bytes()]
    }

    pub(crate) fn range(&self) -> TextRange {
        self.span.range()
    }

    pub(crate) fn byte_span(&self) -> (usize, usize) {
        let bytes = self.span.bytes();
        (bytes.start, bytes.end)
    }

    pub(crate) fn contains(&self, position: Position) -> bool {
        position_in_range(position, self.span.range())
    }
}

pub(crate) fn collect_documentation(
    source: &(impl SourceNavigation + ?Sized),
    root: M2Node<'_>,
) -> (Vec<DocumentationSnippet>, Vec<DocumentationReference>) {
    let Some(mut parser) = M2Parser::new() else {
        return (Vec::new(), Vec::new());
    };
    let mut snippets = Vec::new();
    let mut references = Vec::new();

    for node in root.descendants() {
        if is_documentation_container(node) {
            collect_backtick_snippets(node, source, &mut parser, &mut snippets, &mut references);
        }
    }

    references.sort_by_key(|reference| reference.byte_span());
    references.dedup_by_key(|reference| reference.byte_span());
    (snippets, references)
}

fn is_documentation_container(node: M2Node<'_>) -> bool {
    node.kind.is_comment()
        || (node.kind == NodeKind::StringLiteral && node.text().starts_with("///"))
}

fn collect_backtick_snippets(
    node: M2Node<'_>,
    source: &(impl SourceNavigation + ?Sized),
    parser: &mut M2Parser,
    snippets: &mut Vec<DocumentationSnippet>,
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
        if !is_code_candidate(candidate) {
            continue;
        }

        let start_byte = node.start_byte() + content_start;
        let end_byte = node.start_byte() + close;
        let Some(root) = parser.parse(candidate) else {
            continue;
        };
        references.extend(
            root.descendants()
                .filter(|symbol| symbol.kind.is_symbol_like())
                .map(|symbol| {
                    let symbol_start = start_byte + symbol.start_byte();
                    let symbol_end = start_byte + symbol.end_byte();
                    DocumentationReference {
                        span: source.span_for_bytes(symbol_start..symbol_end),
                    }
                }),
        );
        snippets.push(DocumentationSnippet {
            bytes: start_byte..end_byte,
        });
    }
}

fn is_code_candidate(candidate: &str) -> bool {
    !candidate.trim().is_empty() && !candidate.contains(['`', '\n', '\r'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn references(text: &str) -> Vec<(String, TextRange)> {
        let source = crate::source::DocumentSource::new(text.to_string());
        let mut parser = M2Parser::new().expect("Macaulay2 parser should load");
        let root = parser.parse(text).expect("fixture should parse");
        collect_documentation(&source, root)
            .1
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
                ("x".to_string(), TextRange::new(pos!(0, 14), pos!(0, 15)),),
                ("x".to_string(), TextRange::new(pos!(1, 12), pos!(1, 13)),),
                (
                    "ideal".to_string(),
                    TextRange::new(pos!(1, 20), pos!(1, 25)),
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
                TextRange::new(pos!(0, 11), pos!(0, 16)),
            )]
        );
    }

    #[test]
    fn parses_symbol_references_from_complete_code_spans() {
        let text = "-- `Comment(...)` and `instance(t, Comment)`\n";

        assert_eq!(
            references(text)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["Comment", "instance", "t", "Comment"]
        );
    }
}
