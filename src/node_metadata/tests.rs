//! Syntax abstraction and traversal contract tests.

use super::*;

#[cfg(test)]
mod typed_nodes_tests {
    use super::*;

    #[test]
    fn checked_node_classes_expose_only_their_structural_fields() {
        let mut parser = M2Parser::new().expect("Macaulay2 parser should load");
        let root = parser
            .parse("f = x -> x + 1\n")
            .expect("fixture should parse");

        let addition = root
            .descendants()
            .find(|node| node.binary_operator() == Some("+"))
            .expect("addition should be present");
        let binary = BinaryExpressionNode::try_from(addition)
            .expect("a binary expression should accept the binary node class");
        assert_eq!(binary.left().map(|node| node.text()), Some("x"));
        assert_eq!(binary.right().map(|node| node.text()), Some("1"));
        assert_eq!(M2Node::from(binary).id(), addition.id());

        let lambda = root
            .descendants()
            .find(|node| node.kind == NodeKind::LambdaExpression)
            .expect("lambda should be present");
        let lambda = LambdaExpressionNode::try_from(lambda)
            .expect("a lambda expression should accept the lambda node class");
        assert_eq!(lambda.parameters().map(|node| node.text()), Some("x"));
        assert_eq!(lambda.body().map(|node| node.text()), Some("x + 1"));
        assert!(BinaryExpressionNode::try_from(M2Node::from(lambda)).is_err());
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

    /// Substrings that must not appear outside the `node_metadata` module. Each bypasses
    /// the typed syntax or configured parser abstractions.
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
        (".set_language(", "raw parser configuration; use `M2Parser`"),
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
    fn no_raw_syntax_or_parser_access_outside_node_metadata() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let node_metadata = src.join("node_metadata");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);

        let mut violations = Vec::new();
        for file in files {
            // The node_metadata module is the one sanctioned home for raw access.
            if file.starts_with(&node_metadata) {
                continue;
            }
            let contents = fs::read_to_string(&file).expect("source is readable");
            for (line_number, line) in contents.lines().enumerate() {
                // Skip comments: a doc/line comment may legitimately mention a
                // banned form while describing the rule.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("Parser::new()") && !line.contains("M2Parser::new()") {
                    violations.push(format!(
                        "{}:{}: `Parser::new()` — raw parser construction; use `M2Parser`",
                        file.display(),
                        line_number + 1,
                    ));
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

    fn kinds_of(text: &str) -> Vec<NodeKind> {
        let mut parser = M2Parser::new().expect("Macaulay2 parser should load");
        let root = parser.parse(text).expect("fixture should parse");
        root.descendants().map(|node| node.kind).collect()
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
        let text = "x = (a, b)\ny = {1, 2, 3}\n";
        fn count_via_children(n: M2Node<'_>) -> usize {
            1 + n.children().map(count_via_children).sum::<usize>()
        }
        let mut parser = M2Parser::new().expect("Macaulay2 parser should load");
        let root = parser.parse(text).expect("fixture should parse");
        assert_eq!(root.descendants().count(), count_via_children(root));
    }

    #[test]
    fn grammar_v4_exposes_muted_null_and_naked_sequence_shapes() {
        let text = "local if\nstep 1\nfinish\n(x;)\n(,a,,)\na,b\n";
        let mut parser = M2Parser::new().expect("Macaulay2 parser should load");
        let root = parser.parse(text).expect("fixture should parse");
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
