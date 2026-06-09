use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};
use crate::node_metadata::{M2Node, NodeKind};

#[cfg(test)]
use crate::analysis::ExpressionFact;
use crate::analysis::{Analysis, BindingInfo, FunctionInfo};
use crate::typesystem::BuiltinData;
use crate::util::{
    byte_index_from_lsp_position, floor_char_boundary, node_range,
    tree_sitter_point_from_byte_index, tree_sitter_point_from_lsp_position,
};

#[derive(Debug)]
pub(crate) struct DocumentSnapshot {
    text: String,
    tree: Tree,
    analysis: Analysis,
}

impl DocumentSnapshot {
    pub(crate) fn from_text(text: String, builtins: &BuiltinData) -> Option<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .ok()?;
        let tree = parser.parse(&text, None)?;
        let analysis = Analysis::new_with_builtins(&tree, &text, Some(builtins));
        Some(Self {
            text,
            tree,
            analysis,
        })
    }

    pub(crate) fn apply_changes(
        &mut self,
        changes: &[TextDocumentContentChangeEvent],
        builtins: &BuiltinData,
    ) -> Option<()> {
        for change in changes {
            if let Some(range) = change.range {
                self.apply_incremental_change(range, &change.text, builtins)?;
            } else {
                let replacement = change.text.clone();
                let rebuilt = Self::from_text(replacement, builtins)?;
                *self = rebuilt;
            }
        }

        Some(())
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn analysis(&self) -> &Analysis {
        &self.analysis
    }

    pub(crate) fn diagnostics(&self) -> &[tower_lsp::lsp_types::Diagnostic] {
        &self.analysis.diagnostics
    }

    pub(crate) fn binding_at_position(&self, position: Position) -> Option<&BindingInfo> {
        let node = self.symbol_node_at_position(position)?;
        let name = self.text_for(node);
        self.analysis.get_binding_at(name, position)
    }

    #[cfg(test)]
    pub(crate) fn expression_fact_at_position(
        &self,
        position: Position,
    ) -> Option<&ExpressionFact> {
        let node = self.node_at_position_minimal(position)?;
        self.analysis.expression_fact(&self.text, node)
    }

    pub(crate) fn callable_at_position(&self, position: Position) -> Option<&FunctionInfo> {
        let binding = self.binding_at_position(position)?;
        self.analysis.function_by_symbol(binding.symbol)
    }

    pub(crate) fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    pub(crate) fn range_for(&self, node: Node<'_>) -> Range {
        node_range(&self.text, node)
    }



    pub(crate) fn point_for_position(&self, position: Position) -> Option<Point> {
        tree_sitter_point_from_lsp_position(&self.text, position)
    }

    pub(crate) fn node_at_position_minimal(&self, position: Position) -> Option<Node<'_>> {
        let point = self.point_for_position(position)?;
        self.root_node().descendant_for_point_range(point, point)
    }

    pub(crate) fn symbol_node_at_position(&self, position: Position) -> Option<Node<'_>> {
        let mut node = self.node_at_position_minimal(position)?;

        loop {
            if matches!(node.kind(), "symbol" | "identifier" | "resolved_symbol") {
                return Some(node);
            }
            node = node.parent()?;
        }
    }

    pub(crate) fn text_for<'a>(&'a self, node: Node<'_>) -> &'a str {
        &self.text[node.start_byte()..node.end_byte()]
    }

    pub(crate) fn enclosing_node_of_kind<'a>(
        &self,
        mut node: Node<'a>,
        kind: &str,
    ) -> Option<Node<'a>> {
        loop {
            if node.kind() == kind {
                return Some(node);
            }
            node = node.parent()?;
        }
    }

    fn apply_incremental_change(
        &mut self,
        range: Range,
        replacement: &str,
        builtins: &BuiltinData,
    ) -> Option<()> {
        let start_byte = floor_char_boundary(
            &self.text,
            byte_index_from_lsp_position(&self.text, range.start)?,
        );
        let old_end_byte = floor_char_boundary(
            &self.text,
            byte_index_from_lsp_position(&self.text, range.end)?,
        );
        let start_position = tree_sitter_point_from_byte_index(&self.text, start_byte);
        let old_end_position = tree_sitter_point_from_byte_index(&self.text, old_end_byte);
        let new_end_byte = start_byte + replacement.len();
        let new_end_position = advance_point(start_position, replacement);

        let edit = InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        };

        let mut edited_tree = self.tree.clone();
        edited_tree.edit(&edit);
        self.text
            .replace_range(start_byte..old_end_byte, replacement);

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .ok()?;
        let tree = parser.parse(&self.text, Some(&edited_tree))?;
        let analysis = Analysis::new_with_builtins(&tree, &self.text, Some(builtins));
        self.tree = tree;
        self.analysis = analysis;
        Some(())
    }
}

fn advance_point(start: Point, inserted_text: &str) -> Point {
    let mut row = start.row;
    let mut column = start.column;

    for byte in inserted_text.bytes() {
        if byte == b'\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
        }
    }

    Point::new(row, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtins() -> BuiltinData {
        BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        )
    }

    #[test]
    fn applies_single_character_append_incrementally() {
        let builtins = builtins();
        let mut document =
            DocumentSnapshot::from_text("x".to_string(), &builtins).expect("fixture should parse");
        document
            .apply_changes(
                &[TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 1), Position::new(0, 1))),
                    range_length: None,
                    text: "+".to_string(),
                }],
                &builtins,
            )
            .expect("append should parse");

        assert_eq!(document.text(), "x+");
    }

    #[test]
    fn applies_multiple_incremental_changes_in_order() {
        let builtins = builtins();
        let mut document = DocumentSnapshot::from_text("abc".to_string(), &builtins)
            .expect("fixture should parse");
        document
            .apply_changes(
                &[
                    TextDocumentContentChangeEvent {
                        range: Some(Range::new(Position::new(0, 1), Position::new(0, 2))),
                        range_length: None,
                        text: "B".to_string(),
                    },
                    TextDocumentContentChangeEvent {
                        range: Some(Range::new(Position::new(0, 2), Position::new(0, 3))),
                        range_length: None,
                        text: "C".to_string(),
                    },
                ],
                &builtins,
            )
            .expect("changes should parse");

        assert_eq!(document.text(), "aBC");
    }

    #[test]
    fn replaces_document_when_change_has_no_range() {
        let builtins = builtins();
        let mut document = DocumentSnapshot::from_text("x := 1".to_string(), &builtins)
            .expect("fixture should parse");
        document
            .apply_changes(
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "y := 2".to_string(),
                }],
                &builtins,
            )
            .expect("replacement should parse");

        assert_eq!(document.text(), "y := 2");
    }

    #[test]
    fn exposes_registry_backed_queries_by_position() {
        let builtins = builtins();
        let document = DocumentSnapshot::from_text(
            "f = method(TypicalValue => List)\nf ZZ := Ring => x -> x\ny := f 1\ny\n".to_string(),
            &builtins,
        )
        .expect("fixture should parse");

        let binding = document
            .binding_at_position(Position::new(3, 0))
            .expect("binding should resolve");
        assert_eq!(document.analysis().binding_name(binding), "y");
        assert_eq!(binding.type_name.as_deref(), Some("Ring"));

        let callable = document
            .callable_at_position(Position::new(1, 0))
            .expect("callable should resolve");
        assert_eq!(document.analysis().symbol_name(callable.symbol), "f");
        assert_eq!(callable.methods.len(), 1);

        let fact = document
            .expression_fact_at_position(Position::new(2, 5))
            .expect("expression fact should resolve");
        assert!(matches!(
            fact.result_type,
            crate::analysis::ExpressionType::Known(_)
        ));
    }
}
