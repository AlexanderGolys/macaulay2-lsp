use std::collections::HashMap;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range as LspRange};
use tree_sitter::{Node, Tree};

use crate::typesystem::{BuiltinData, InstanceID};

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
    pub type_name: Option<String>,
}

#[derive(Debug)]
pub struct Scope {
    pub range: LspRange,
    pub symbols: HashMap<String, Vec<SymbolInfo>>,
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

    pub fn get_symbol_at(&self, name: &str, pos: Position) -> Option<&SymbolInfo> {
        let scope_idx = self.find_scope_at(pos)?;
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            if let Some(symbols) = self.scopes[idx].symbols.get(name) {
                if let Some(symbol) = symbols
                    .iter()
                    .rev()
                    .find(|symbol| symbol.range.start <= pos)
                {
                    return Some(symbol);
                }
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

    #[cfg(test)]
    pub fn new(tree: &Tree, text: &str) -> Self {
        Self::new_with_builtins(tree, text, None)
    }

    pub fn new_with_builtins(tree: &Tree, text: &str, builtins: Option<&BuiltinData>) -> Self {
        let mut analysis = Analysis {
            scopes: vec![Scope {
                range: LspRange::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                symbols: HashMap::new(),
                parent_idx: None,
            }],
            diagnostics: Vec::new(),
        };
        analysis.collect_diagnostics(tree.root_node(), text);
        analysis.build_scopes(tree.root_node(), text, 0, builtins);
        analysis
    }

    fn collect_diagnostics(&mut self, node: Node, text: &str) {
        if node.is_error() {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(text, node.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Syntax error".to_string(),
                ..Default::default()
            });
        } else if node.is_missing() {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(text, node.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("Missing: {}", node.kind()),
                ..Default::default()
            });
        } else if node.kind() == "assignment_expression" {
            self.validate_assignment_form(node, text);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_diagnostics(child, text);
        }
    }

    fn validate_assignment_form(&mut self, node: Node, text: &str) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let Some(operator) = node.child_by_field_name("operator") else {
            return;
        };
        let op_text = &text[operator.start_byte()..operator.end_byte()];

        if matches!(op_text, "=" | ":=") && !multiple_assignment_targets_are_symbols(left) {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(text, left.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("{op_text} multiple assignment targets must be symbols"),
                ..Default::default()
            });
        }

        if op_text == ":="
            && left.kind() == "binary_expression"
            && binary_expression_operator(left, text) == Some("#")
        {
            self.diagnostics.push(Diagnostic {
                range: to_lsp_range(text, left.range()),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "`:=` cannot assign to parts; use `=` for part assignment".to_string(),
                ..Default::default()
            });
        }
    }

    fn build_scopes(
        &mut self,
        node: Node,
        text: &str,
        current_scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) {
        let mut next_scope_idx = current_scope_idx;

        match node.kind() {
            "function_expression" => {
                // Create a new scope for the function
                let range = to_lsp_range(text, node.range());
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
                    let symbol_kind = match right {
                        Some(right) if right.kind() == "function_expression" => {
                            SymbolKind::Function
                        }
                        _ => SymbolKind::Variable,
                    };
                    let type_name = right.and_then(|right| {
                        self.infer_static_type_name(right, text, current_scope_idx, builtins)
                    });

                    match op_text {
                        ":=" => self.collect_definitions(
                            left,
                            text,
                            current_scope_idx,
                            DefinitionScope::Local,
                            symbol_kind,
                            type_name.as_deref(),
                        ),
                        "=" if current_scope_idx == 0 => self.collect_definitions(
                            left,
                            text,
                            current_scope_idx,
                            DefinitionScope::Global,
                            symbol_kind,
                            type_name.as_deref(),
                        ),
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.build_scopes(child, text, next_scope_idx, builtins);
        }
    }

    fn collect_parameters(&mut self, node: Node, text: &str, scope_idx: usize) {
        match node.kind() {
            "symbol" => {
                let name = &text[node.start_byte()..node.end_byte()];
                self.add_symbol(name, SymbolKind::Parameter, None, node, scope_idx, text);
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
        definition_scope: DefinitionScope,
        kind: SymbolKind,
        type_name: Option<&str>,
    ) {
        match node.kind() {
            "symbol" => {
                let name = &text[node.start_byte()..node.end_byte()];
                match definition_scope {
                    DefinitionScope::Local => {
                        self.add_symbol(name, kind, type_name, node, scope_idx, text)
                    }
                    DefinitionScope::Global => {
                        if !self.is_defined_in_chain(name, scope_idx) {
                            self.add_symbol(name, kind, type_name, node, 0, text);
                        }
                    }
                }
            }
            "sequence" | "list" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "symbol" {
                        self.collect_definitions(
                            child,
                            text,
                            scope_idx,
                            definition_scope,
                            kind,
                            None,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn add_symbol(
        &mut self,
        name: &str,
        kind: SymbolKind,
        type_name: Option<&str>,
        node: Node,
        scope_idx: usize,
        text: &str,
    ) {
        let symbol = SymbolInfo {
            kind,
            range: to_lsp_range(text, node.range()),
            type_name: type_name.map(ToString::to_string),
        };
        self.scopes[scope_idx]
            .symbols
            .entry(name.to_string())
            .or_default()
            .push(symbol);
    }

    fn infer_static_type_name(
        &self,
        node: Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        match node.kind() {
            "function_expression" => Some("Function".to_string()),
            "string_literal" => Some("String".to_string()),
            "integer_literal" => Some("ZZ".to_string()),
            "float_literal" => Some("RR".to_string()),
            "symbol" | "identifier" | "resolved_symbol" | "builtin_constant" => {
                let name = &text[node.start_byte()..node.end_byte()];
                if let Some(symbol) =
                    self.lookup_symbol_from_scope(name, scope_idx, node_position(text, node))
                {
                    if let Some(type_name) = &symbol.type_name {
                        return Some(type_name.clone());
                    }
                }

                builtins
                    .and_then(|builtins| builtins.get_record(&InstanceID::new(name)))
                    .map(|record| record.data_type.0)
            }
            "assignment_expression" => {
                let right = node.child_by_field_name("right")?;
                self.infer_static_type_name(right, text, scope_idx, builtins)
            }
            "binary_expression" => {
                if binary_expression_operator(node, text) == Some("from") {
                    node.child_by_field_name("left")
                        .and_then(|left| new_expression_type_name(left, text))
                        .map(ToString::to_string)
                } else {
                    None
                }
            }
            "new_statement" => node
                .child_by_field_name("type")
                .filter(|type_node| type_node.kind() == "symbol")
                .map(|type_node| text[type_node.start_byte()..type_node.end_byte()].to_string()),
            _ => None,
        }
    }

    fn lookup_symbol_from_scope(
        &self,
        name: &str,
        scope_idx: usize,
        pos: Position,
    ) -> Option<&SymbolInfo> {
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            if let Some(symbols) = self.scopes[idx].symbols.get(name) {
                if let Some(symbol) = symbols
                    .iter()
                    .rev()
                    .find(|symbol| symbol.range.start <= pos)
                {
                    return Some(symbol);
                }
            }
            curr = self.scopes[idx].parent_idx;
        }
        None
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

#[derive(Debug, Clone, Copy)]
enum DefinitionScope {
    Local,
    Global,
}

fn multiple_assignment_targets_are_symbols(node: Node) -> bool {
    if !matches!(node.kind(), "sequence" | "list") {
        return true;
    }

    let mut cursor = node.walk();
    let all_targets_are_symbols = node
        .named_children(&mut cursor)
        .all(|child| child.kind() == "symbol");
    all_targets_are_symbols
}

fn binary_expression_operator<'a>(node: Node, text: &'a str) -> Option<&'a str> {
    if node.kind() != "binary_expression" {
        return None;
    }

    node.child_by_field_name("operator")
        .map(|operator| &text[operator.start_byte()..operator.end_byte()])
}

fn binary_expression_operator_kind(node: Node<'_>) -> Option<&str> {
    if node.kind() != "binary_expression" {
        return None;
    }

    node.child_by_field_name("operator")
        .map(|operator| operator.kind())
}

fn new_expression_type_name<'a>(node: Node, text: &'a str) -> Option<&'a str> {
    if node.kind() != "binary_expression" || binary_expression_operator_kind(node) != Some("space")
    {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if &text[left.start_byte()..left.end_byte()] != "new" {
        return None;
    }

    let right = node.child_by_field_name("right")?;
    if right.kind() != "symbol" {
        return None;
    }

    Some(&text[right.start_byte()..right.end_byte()])
}

fn to_lsp_range(text: &str, range: tree_sitter::Range) -> LspRange {
    let start_line_byte = range.start_byte.saturating_sub(range.start_point.column);
    let end_line_byte = range.end_byte.saturating_sub(range.end_point.column);

    LspRange::new(
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

fn node_position(text: &str, node: Node) -> Position {
    to_lsp_range(text, node.range()).start
}

fn floor_char_boundary(text: &str, byte_index: usize) -> usize {
    let mut byte_index = byte_index.min(text.len());
    while byte_index > 0 && !text.is_char_boundary(byte_index) {
        byte_index -= 1;
    }
    byte_index
}

fn utf16_len_for_byte_span(text: &str, start_byte: usize, end_byte: usize) -> u32 {
    let start_byte = floor_char_boundary(text, start_byte);
    let end_byte = floor_char_boundary(text, end_byte.max(start_byte));
    text[start_byte..end_byte].encode_utf16().count() as u32
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

    fn analyze_with_builtins(text: &str, builtins: &BuiltinData) -> Analysis {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        Analysis::new_with_builtins(&tree, text, Some(builtins))
    }

    #[test]
    fn classifies_user_defined_functions_and_parameters() {
        let analysis = analyze("f := x -> x\nf 1");

        assert_eq!(
            analysis
                .get_symbol_at("f", Position::new(1, 0))
                .map(|symbol| symbol.kind),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            analysis
                .get_symbol_at("x", Position::new(0, 10))
                .map(|symbol| symbol.kind),
            Some(SymbolKind::Parameter)
        );
    }

    #[test]
    fn resolves_latest_binding_before_query_position() {
        let analysis = analyze("x := 1\ny := x\nx := 2\nx\n");

        let middle_use = analysis
            .get_symbol_at("x", Position::new(1, 5))
            .expect("middle x should resolve to the first binding");
        assert_eq!(middle_use.range.start, Position::new(0, 0));

        let later_use = analysis
            .get_symbol_at("x", Position::new(3, 0))
            .expect("later x should resolve to the second binding");
        assert_eq!(later_use.range.start, Position::new(2, 0));
    }

    #[test]
    fn analysis_ranges_use_lsp_utf16_columns() {
        let analysis = analyze("\"😀\"; x := 1\nx\n");

        let symbol = analysis
            .get_symbol_at("x", Position::new(1, 0))
            .expect("x should resolve despite non-ascii text before its definition");

        assert_eq!(
            symbol.range,
            LspRange::new(Position::new(0, 6), Position::new(0, 7))
        );
    }

    #[test]
    fn infers_static_types_from_builtin_bindings_and_aliases() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let analysis = analyze_with_builtins(
            "Doc := Macaulay2Doc\nDocAlias := Doc\nDocAlias\n",
            &builtins,
        );

        assert_eq!(
            analysis
                .get_symbol_at("Doc", Position::new(1, 12))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Package")
        );
        assert_eq!(
            analysis
                .get_symbol_at("DocAlias", Position::new(2, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Package")
        );
    }

    #[test]
    fn infers_static_types_from_new_constructors() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let analysis = analyze_with_builtins(
            "clearAll = new Command from { () -> () }\nclearAll\n",
            &builtins,
        );
        assert_eq!(
            analysis
                .get_symbol_at("clearAll", Position::new(1, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Command")
        );
    }

    #[test]
    fn diagnoses_structurally_invalid_assignment_forms() {
        let analysis = analyze(
            "x#i := e\n(x+1,y) = (1,2)\n(x+1,y) := (1,2)\n(f()) <- (1)\nsource(String,Number) := peek\n",
        );

        assert_eq!(
            analysis
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "`:=` cannot assign to parts; use `=` for part assignment",
                "= multiple assignment targets must be symbols",
                ":= multiple assignment targets must be symbols",
            ]
        );
    }
}
