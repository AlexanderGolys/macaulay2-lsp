use std::collections::HashMap;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range as LspRange};
use tree_sitter::{Node, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Variable,
    Parameter,
}

#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub kind: SymbolKind,
    pub range: LspRange,
}

#[derive(Debug)]
pub struct Scope {
    pub range: LspRange,
    pub symbols: HashMap<String, SymbolInfo>,
    pub parent_idx: Option<usize>,
}

#[derive(Debug)]
pub struct Analysis {
    pub scopes: Vec<Scope>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    pub fn find_definition(&self, name: &str, pos: Position) -> Option<LspRange> {
        self.get_symbol_at(name, pos).map(|symbol| symbol.range)
    }

    pub fn get_symbol_kind_at(&self, name: &str, pos: Position) -> Option<SymbolKind> {
        self.get_symbol_at(name, pos).map(|symbol| symbol.kind)
    }

    pub fn get_symbol_at(&self, name: &str, pos: Position) -> Option<&SymbolInfo> {
        let scope_idx = self.find_scope_at(pos)?;
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            if let Some(symbol) = self.scopes[idx].symbols.get(name) {
                return Some(symbol);
            }
            curr = self.scopes[idx].parent_idx;
        }
        None
    }

    fn find_scope_at(&self, pos: Position) -> Option<usize> {
        let mut best_idx = None;
        let mut best_range: Option<LspRange> = None;

        for (idx, scope) in self.scopes.iter().enumerate() {
            if is_pos_in_range(pos, scope.range) {
                match best_range {
                    None => {
                        best_idx = Some(idx);
                        best_range = Some(scope.range);
                    }
                    Some(r) => {
                        // We want the smallest (most nested) scope
                        if is_range_smaller(scope.range, r) {
                            best_idx = Some(idx);
                            best_range = Some(scope.range);
                        }
                    }
                }
            }
        }
        best_idx
    }

    pub fn new(tree: &Tree, text: &str) -> Self {
        let mut analysis = Analysis {
            scopes: vec![Scope {
                range: LspRange::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                symbols: HashMap::new(),
                parent_idx: None,
            }],
            diagnostics: Vec::new(),
        };
        analysis.collect_diagnostics(tree.root_node());
        analysis.build_scopes(tree.root_node(), text, 0);
        analysis
    }

    fn collect_diagnostics(&mut self, node: Node) {
        if node.is_error() {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(node.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Syntax error".to_string(),
                ..Default::default()
            });
        } else if node.is_missing() {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(node.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("Missing: {}", node.kind()),
                ..Default::default()
            });
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_diagnostics(child);
        }
    }

    fn build_scopes(&mut self, node: Node, text: &str, current_scope_idx: usize) {
        let mut next_scope_idx = current_scope_idx;

        match node.kind() {
            "function_expression" => {
                // Create a new scope for the function
                let range = to_lsp_range(node.range());
                let new_scope = Scope {
                    range,
                    symbols: HashMap::new(),
                    parent_idx: Some(current_scope_idx),
                };
                self.scopes.push(new_scope);
                next_scope_idx = self.scopes.len() - 1;

                // Add parameters to the new scope
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    self.collect_parameters(params_node, text, next_scope_idx);
                }
            }
            "assignment_expression" => {
                let left = node.child_by_field_name("left");
                let op = node.child_by_field_name("operator");
                let right = node.child_by_field_name("right");

                if let (Some(left), Some(op)) = (left, op) {
                    let op_text = &text[op.start_byte()..op.end_byte()];
                    let is_local = op_text == ":=";
                    let symbol_kind = match right {
                        Some(right) if right.kind() == "function_expression" => {
                            SymbolKind::Function
                        }
                        _ => SymbolKind::Variable,
                    };

                    self.collect_definitions(left, text, current_scope_idx, is_local, symbol_kind);
                }
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.build_scopes(child, text, next_scope_idx);
        }
    }

    fn collect_parameters(&mut self, node: Node, text: &str, scope_idx: usize) {
        match node.kind() {
            "symbol" => {
                let name = &text[node.start_byte()..node.end_byte()];
                self.add_symbol(name, SymbolKind::Parameter, node, scope_idx);
            }
            "sequence" | "list" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.collect_parameters(child, text, scope_idx);
                }
            }
            _ => {}
        }
    }

    fn collect_definitions(
        &mut self,
        node: Node,
        text: &str,
        scope_idx: usize,
        is_local: bool,
        kind: SymbolKind,
    ) {
        match node.kind() {
            "symbol" => {
                let name = &text[node.start_byte()..node.end_byte()];
                if is_local {
                    self.add_symbol(name, kind, node, scope_idx);
                } else {
                    // Search if it exists in parent scopes, otherwise it's global (scope 0)
                    if !self.is_defined_in_chain(name, scope_idx) {
                        self.add_symbol(name, kind, node, 0);
                    }
                }
            }
            "sequence" | "list" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.collect_definitions(child, text, scope_idx, is_local, kind);
                }
            }
            _ => {}
        }
    }

    fn add_symbol(&mut self, name: &str, kind: SymbolKind, node: Node, scope_idx: usize) {
        let symbol = SymbolInfo {
            kind,
            range: to_lsp_range(node.range()),
        };
        self.scopes[scope_idx]
            .symbols
            .insert(name.to_string(), symbol);
    }

    fn is_defined_in_chain(&self, name: &str, start_scope_idx: usize) -> bool {
        let mut curr = Some(start_scope_idx);
        while let Some(idx) = curr {
            if self.scopes[idx].symbols.contains_key(name) {
                return true;
            }
            curr = self.scopes[idx].parent_idx;
        }
        false
    }
}

fn to_lsp_range(range: tree_sitter::Range) -> LspRange {
    LspRange::new(
        Position::new(
            range.start_point.row as u32,
            range.start_point.column as u32,
        ),
        Position::new(range.end_point.row as u32, range.end_point.column as u32),
    )
}

fn is_pos_in_range(pos: Position, range: LspRange) -> bool {
    if pos.line < range.start.line || pos.line > range.end.line {
        return false;
    }
    if pos.line == range.start.line && pos.character < range.start.character {
        return false;
    }
    if pos.line == range.end.line && pos.character >= range.end.character {
        return false;
    }
    true
}

fn is_range_smaller(a: LspRange, b: LspRange) -> bool {
    // Very simple check: is a contained in b?
    let starts_inside = a.start.line > b.start.line
        || (a.start.line == b.start.line && a.start.character >= b.start.character);
    let ends_inside =
        a.end.line < b.end.line || (a.end.line == b.end.line && a.end.character <= b.end.character);
    starts_inside && ends_inside && a != b
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn analyze(text: &str) -> Analysis {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        Analysis::new(&tree, text)
    }

    #[test]
    fn classifies_user_defined_functions_and_parameters() {
        let analysis = analyze("f := x -> x\nf 1");

        assert_eq!(
            analysis.get_symbol_kind_at("f", Position::new(1, 0)),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            analysis.get_symbol_kind_at("x", Position::new(0, 10)),
            Some(SymbolKind::Parameter)
        );
    }
}
