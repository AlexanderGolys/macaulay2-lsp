//! Versioned document snapshots that combine source text, parse tree, and
//! analysis for LSP requests.

use crate::node_metadata::{M2Node, NodeKind};
use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use tree_sitter::{InputEdit, Parser, Point, Tree};

#[cfg(test)]
use crate::analysis::ExpressionFact;
use crate::analysis::{Analysis, BindingInfo, FunctionInfo, SymbolInfo};
use crate::package_index::collect_imported_packages_in_tree;
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
    /// Packages this document imports (`needsPackage`/`loadPackage`/`debug`/
    /// `importFrom`), collected once per version from `tree`. The baseline is
    /// owned by the partitioned index, so the snapshot stores only its own
    /// contribution to the loaded set.
    imported_packages: Vec<String>,
}

/// The common first step of every reference / highlight / rename request: the
/// tree-sitter symbol node under the cursor together with its scope-aware
/// `SymbolInfo`. Resolved once per request and threaded through the downstream
/// reference collection so the target lookup is not repeated.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TargetSymbol<'a> {
    pub node: M2Node<'a>,
    pub symbol: &'a SymbolInfo,
}

impl DocumentSnapshot {
    pub(crate) fn from_text(text: String, builtins: &BuiltinData) -> Option<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .ok()?;
        let tree = parser.parse(&text, None)?;
        let analysis = Analysis::new_with_builtins(&tree, &text, Some(builtins));
        let imported_packages = collect_imported_packages_in_tree(&text, &tree);
        Some(Self {
            text,
            tree,
            analysis,
            imported_packages,
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

    /// The packages this document imports, memoized from its tree. Combined with
    /// the partitioned index's baseline (`LoadedPackages::from_parts`) to build
    /// the scoped query view.
    pub(crate) fn imported_packages(&self) -> &[String] {
        &self.imported_packages
    }

    pub(crate) fn analysis(&self) -> &Analysis {
        &self.analysis
    }

    pub(crate) fn diagnostics(&self) -> &[tower_lsp::lsp_types::Diagnostic] {
        &self.analysis.diagnostics
    }

    pub(crate) fn binding_at_position(&self, position: Position) -> Option<&BindingInfo> {
        let node = self.symbol_node_at_position(position)?;
        self.analysis.get_binding_at(node.text(), position)
    }

    /// Resolve the user symbol under `position`: its tree-sitter node plus the
    /// scope-aware `SymbolInfo` for the same site. Returns `None` when the
    /// cursor is not on a renameable / referenceable symbol (builtins, keywords,
    /// punctuation, or whitespace). The single entry point shared by reference,
    /// highlight, and rename requests.
    pub(crate) fn target_symbol_at(&self, position: Position) -> Option<TargetSymbol<'_>> {
        let node = self.symbol_node_at_position(position)?;
        let symbol = self.analysis.get_symbol_at(node.text(), position)?;
        Some(TargetSymbol { node, symbol })
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

    pub(crate) fn root_node(&self) -> M2Node<'_> {
        M2Node::new(self.tree.root_node(), &self.text)
    }

    pub(crate) fn range_for(&self, node: M2Node<'_>) -> Range {
        node_range(node)
    }

    pub(crate) fn point_for_position(&self, position: Position) -> Option<Point> {
        tree_sitter_point_from_lsp_position(&self.text, position)
    }

    pub(crate) fn node_at_position_minimal(&self, position: Position) -> Option<M2Node<'_>> {
        let point = self.point_for_position(position)?;
        self.root_node()
            .descendant_for_point_range(point, point)
            .map(|node| M2Node::new(node, &self.text))
    }

    pub(crate) fn symbol_node_at_position(&self, position: Position) -> Option<M2Node<'_>> {
        let point = self.point_for_position(position)?;
        let root = self.root_node();
        // When the cursor sits on the boundary between the anonymous SPACE
        // application operator (which the grammar emits zero-width) and an
        // adjacent symbol — e.g. the trailing `M` in an application `(f x) M`,
        // where SPACE sits exactly at `M`'s start column — a zero-width lookup
        // lands on the operator. Widening the lookup to the character under the
        // cursor lands on the symbol; the exact-point lookup is tried first so
        // ordinary mid-token positions are unaffected.
        let next = Point::new(point.row, point.column + 1);
        let starts = [
            root.descendant_for_point_range(point, point),
            root.descendant_for_point_range(point, next),
        ];
        for start in starts.into_iter().flatten() {
            let mut node = M2Node::new(start, &self.text);
            loop {
                if node.kind.is_symbol_like() {
                    return Some(node);
                }
                match node.parent() {
                    Some(parent) => node = parent,
                    None => break,
                }
            }
        }
        None
    }

    pub(crate) fn enclosing_node_of_kind<'a>(
        &self,
        mut node: M2Node<'a>,
        kind: NodeKind,
    ) -> Option<M2Node<'a>> {
        loop {
            if node.kind == kind {
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
        self.imported_packages = collect_imported_packages_in_tree(&self.text, &tree);
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
        BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"))
    }

    #[test]
    fn resolves_symbol_at_application_operand_boundary() {
        // The trailing `M` is the right operand of an application `(...) M`,
        // where the zero-width SPACE operator sits exactly at `M`'s start column.
        // A naive zero-width lookup lands on the operator; the symbol must still
        // resolve (regression: rename of such an operand returned nothing).
        let builtins = builtins();
        let text = "h Module := M -> (\n    (m(class, ring)) M;\n)\n";
        let doc = DocumentSnapshot::from_text(text.to_string(), &builtins).expect("parse");
        let trailing_m = text.lines().nth(1).unwrap().find(") M").unwrap() + 2;
        let node = doc
            .symbol_node_at_position(Position::new(1, trailing_m as u32))
            .expect("trailing application operand should resolve to its symbol");
        assert_eq!(node.text(), "M");
    }

    #[test]
    fn memoizes_imported_packages_and_rederives_on_edit() {
        let builtins = builtins();
        let mut document =
            DocumentSnapshot::from_text("needsPackage \"JSON\"\n".to_string(), &builtins)
                .expect("fixture should parse");
        assert_eq!(document.imported_packages(), &["JSON".to_string()]);

        // Append a second import incrementally; the memoized set re-derives.
        let end = Position::new(1, 0);
        document
            .apply_changes(
                &[TextDocumentContentChangeEvent {
                    range: Some(Range::new(end, end)),
                    range_length: None,
                    text: "needsPackage \"Text\"\n".to_string(),
                }],
                &builtins,
            )
            .expect("edit should parse");
        assert_eq!(
            document.imported_packages(),
            &["JSON".to_string(), "Text".to_string()]
        );
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
    fn removing_prior_definitions_retypes_the_shifted_assignment() {
        let builtins = builtins();
        let mut document = DocumentSnapshot::from_text(
            "x = y\ny = (x:=2;z=4)\nz = x = y\n".to_string(),
            &builtins,
        )
        .expect("fixture should parse");

        assert!(
            !document.analysis().registry().bindings.is_empty(),
            "the fixture should register definitions and assignment states"
        );
        let first_x = document
            .analysis()
            .get_binding_at("x", Position::new(0, 0))
            .expect("the first assignment should create global x");
        assert_eq!(first_x.type_name.as_deref(), Some("Symbol"));
        for (name, character) in [("z", 0), ("x", 4), ("y", 8)] {
            let binding = document
                .analysis()
                .get_binding_at(name, Position::new(2, character))
                .expect("the chained assignment should resolve the binding");
            assert_eq!(
                binding.type_name.as_deref(),
                Some("ZZ"),
                "{name} should have the source-ordered numeric type"
            );
        }
        for character in [0, 4, 8] {
            let node = document
                .symbol_node_at_position(Position::new(2, character))
                .expect("the surviving assignment should initially be on line 2");
            assert_eq!(document.range_for(node).start, Position::new(2, character));
        }

        document
            .apply_changes(
                &[TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 0), Position::new(2, 0))),
                    range_length: None,
                    text: String::new(),
                }],
                &builtins,
            )
            .expect("line removal should parse");

        assert_eq!(document.text(), "z = x = y\n");
        for (name, character) in [("z", 0), ("x", 4), ("y", 8)] {
            let node = document
                .symbol_node_at_position(Position::new(0, character))
                .expect("the surviving assignment should shift up by two lines");
            assert_eq!(node.text(), name);
            assert_eq!(document.range_for(node).start, Position::new(0, character));
        }

        for (name, character) in [("z", 0), ("x", 4)] {
            let binding = document
                .analysis()
                .get_binding_at(name, Position::new(0, character))
                .expect("the remaining assignment should create the binding");
            assert_eq!(
                binding.type_name.as_deref(),
                Some("Symbol"),
                "{name} must be retyped from the unresolved y"
            );
        }
        assert!(
            document
                .analysis()
                .get_binding_at("y", Position::new(0, 8))
                .is_none(),
            "the removed y definition must not survive the edit"
        );
        assert_eq!(
            document.analysis().registry().bindings.len(),
            2,
            "only the new x and z Symbol bindings should remain"
        );
        let y_fact = document
            .expression_fact_at_position(Position::new(0, 8))
            .expect("the unresolved y should retain an expression fact");
        assert_eq!(y_fact.result_type.label().as_deref(), Some("Symbol"));
        assert_eq!(
            document.analysis().scopes.len(),
            1,
            "only the root scope should remain"
        );
        assert_eq!(
            document.analysis().registry().scopes.len(),
            1,
            "the semantic registry must not retain removed local scopes"
        );
        assert!(
            document
                .analysis()
                .registry()
                .expressions
                .values()
                .all(|fact| fact.span.range.start.line == 0),
            "retained expression facts must shift from line 2 to line 0"
        );
    }

    #[test]
    fn removing_a_lambda_drops_its_local_scope() {
        let builtins = builtins();
        let mut document =
            DocumentSnapshot::from_text("f := () -> (x := 2; z = 4)\nf\n".to_string(), &builtins)
                .expect("fixture should parse");

        assert_eq!(document.analysis().scopes.len(), 2);
        assert_eq!(document.analysis().registry().scopes.len(), 2);

        document
            .apply_changes(
                &[TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 0), Position::new(1, 0))),
                    range_length: None,
                    text: String::new(),
                }],
                &builtins,
            )
            .expect("lambda removal should parse");

        assert_eq!(document.text(), "f\n");
        assert!(document
            .analysis()
            .get_binding_at("f", Position::new(0, 0))
            .is_none());
        assert!(document.analysis().registry().bindings.is_empty());
        assert_eq!(document.analysis().scopes.len(), 1);
        assert_eq!(document.analysis().registry().scopes.len(), 1);
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
        assert!(fact.result_type.label().is_some());
    }
}
