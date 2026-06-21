use std::collections::HashSet;

use tower_lsp::lsp_types::*;

use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind};
use crate::typesystem::BuiltinData;
use crate::util::*;

pub(crate) fn collect_document_symbols(
    document: &DocumentSnapshot,
    builtins: &BuiltinData,
) -> Vec<DocumentSymbol> {
    let text = document.text();
    let mut scopes = DocumentSymbolScopes::new();
    collect_document_symbols_from(document.root_node(), text, builtins, &mut scopes)
}

#[derive(Debug)]
struct DocumentSymbolScopes {
    names: Vec<HashSet<String>>,
    options: HashSet<String>,
}

impl DocumentSymbolScopes {
    fn new() -> Self {
        Self {
            names: vec![HashSet::new()],
            options: HashSet::new(),
        }
    }

    /// Record an option key on its first appearance anywhere in the document.
    /// Returns `true` only the first time, so repeated keys are listed once.
    fn introduce_option(&mut self, name: &str) -> bool {
        self.options.insert(name.to_string())
    }

    fn push(&mut self) {
        self.names.push(HashSet::new());
    }

    fn pop(&mut self) {
        self.names.pop();
    }

    fn add_current(&mut self, name: &str) {
        if let Some(scope) = self.names.last_mut() {
            scope.insert(name.to_string());
        }
    }

    fn introduce_local(&mut self, name: &str) -> bool {
        let Some(scope) = self.names.last_mut() else {
            return false;
        };
        scope.insert(name.to_string())
    }

    fn introduce_global_if_missing(&mut self, name: &str) -> bool {
        if self.names.len() > 1 {
            return false;
        }

        if self.names.iter().rev().any(|scope| scope.contains(name)) {
            return false;
        }

        self.names[0].insert(name.to_string());
        true
    }
}

fn collect_document_symbols_from(
    node: tree_sitter::Node,
    text: &str,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Vec<DocumentSymbol> {
    if is_assignment_expression(node, text) {
        return collect_assignment_document_symbols(node, text, builtins, scopes);
    }
    if is_option_assignment_expression(node, text) {
        return collect_property_document_symbols(node, text, builtins, scopes);
    }

    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        symbols.extend(collect_document_symbols_from(child, text, builtins, scopes));
    }
    symbols
}

/// Emit document symbols for option keys (left of `=>`), but only where a key is
/// actually introduced: a key already indexed in some package is defined there,
/// not here, and a repeated key is listed once. Keys passed to a function call
/// are therefore skipped — they are package symbols or repeats.
fn collect_property_document_symbols(
    node: tree_sitter::Node,
    text: &str,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Vec<DocumentSymbol> {
    let Some(left) = node.child_by_field_name("left") else {
        return Vec::new();
    };

    let mut left_symbols = Vec::new();
    collect_left_symbol_nodes(left, &mut left_symbols);

    left_symbols
        .into_iter()
        .filter_map(|symbol| {
            let name = &text[symbol.start_byte()..symbol.end_byte()];
            if builtins.contains_name(name) || !scopes.introduce_option(name) {
                return None;
            }
            Some(DocumentSymbol {
                name: name.to_string(),
                detail: Some("option".to_string()),
                kind: SymbolKind::PROPERTY,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                range: node_range(text, node),
                selection_range: node_range(text, symbol),
                children: None,
            })
        })
        .collect()
}

fn collect_assignment_document_symbols(
    node: tree_sitter::Node,
    text: &str,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Vec<DocumentSymbol> {
    let Some(left) = node.child_by_field_name("left") else {
        return Vec::new();
    };

    let children = match node.child_by_field_name("right") {
        Some(right) if M2Node::new(right).is(NodeKind::LambdaExpression) => {
            collect_function_body_document_symbols(right, text, builtins, scopes)
        }
        _ => None,
    };

    let operator = assignment_operator(node, text);
    let mut binding_targets = Vec::new();
    collect_binding_target_nodes(left, &mut binding_targets);

    if !binding_targets.is_empty() && operator != AssignmentOperator::LeftArrow {
        return binding_targets
            .into_iter()
            .filter(|symbol| {
                let name = &text[symbol.start_byte()..symbol.end_byte()];
                match operator {
                    AssignmentOperator::ColonEqual => scopes.introduce_local(name),
                    AssignmentOperator::Equal => scopes.introduce_global_if_missing(name),
                    AssignmentOperator::LeftArrow | AssignmentOperator::Other => false,
                }
            })
            .map(|symbol| {
                let name = &text[symbol.start_byte()..symbol.end_byte()];
                DocumentSymbol {
                    name: name.to_string(),
                    detail: None,
                    kind: assignment_symbol_kind(node, text),
                    tags: None,
                    #[allow(deprecated)]
                    deprecated: None,
                    range: node_range(text, node),
                    selection_range: node_range(text, symbol),
                    children: children.clone(),
                }
            })
            .collect();
    }

    let is_method_installation_left = M2Node::new(left).kind.is_method_installation_target();

    match (operator, binary_expression_operator(left, text)) {
        (AssignmentOperator::ColonEqual, _) if is_method_installation_left => {
            vec![DocumentSymbol {
                name: text[left.start_byte()..left.end_byte()].to_string(),
                detail: Some("method".to_string()),
                kind: SymbolKind::METHOD,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                range: node_range(text, node),
                selection_range: node_range(text, left),
                children,
            }]
        }
        (AssignmentOperator::Equal, Some("_")) => vec![DocumentSymbol {
            name: text[left.start_byte()..left.end_byte()].to_string(),
            detail: Some("indexed variable".to_string()),
            kind: SymbolKind::VARIABLE,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            range: node_range(text, node),
            selection_range: node_range(text, left),
            children: None,
        }],
        (AssignmentOperator::Equal, Some(_))
            if node
                .child_by_field_name("right")
                .is_some_and(|right| M2Node::new(right).is(NodeKind::LambdaExpression))
                && is_method_installation_left =>
        {
            vec![DocumentSymbol {
                name: text[left.start_byte()..left.end_byte()].to_string(),
                detail: Some("assignment method".to_string()),
                kind: SymbolKind::METHOD,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                range: node_range(text, node),
                selection_range: node_range(text, left),
                children,
            }]
        }
        _ => Vec::new(),
    }
}

fn collect_function_body_document_symbols(
    function_node: tree_sitter::Node,
    text: &str,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Option<Vec<DocumentSymbol>> {
    let body = function_node.child_by_field_name("body")?;

    scopes.push();
    if let Some(params) = function_node.child_by_field_name("parameters") {
        let mut names = Vec::new();
        collect_parameter_names(params, text, &mut names);
        for name in names {
            scopes.add_current(&name);
        }
    }

    let children = collect_document_symbols_from(body, text, builtins, scopes);
    scopes.pop();

    (!children.is_empty()).then_some(children)
}

fn collect_left_symbol_nodes<'tree>(
    node: tree_sitter::Node<'tree>,
    symbols: &mut Vec<tree_sitter::Node<'tree>>,
) {
    match M2Node::new(node).kind {
        NodeKind::Symbol => symbols.push(node),
        NodeKind::Sequence | NodeKind::List => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_left_symbol_nodes(child, symbols);
            }
        }
        _ => {}
    }
}

fn collect_binding_target_nodes<'tree>(
    node: tree_sitter::Node<'tree>,
    symbols: &mut Vec<tree_sitter::Node<'tree>>,
) {
    match M2Node::new(node).kind {
        NodeKind::Symbol => symbols.push(node),
        NodeKind::Sequence | NodeKind::List => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if M2Node::new(child).is(NodeKind::Symbol) {
                    symbols.push(child);
                }
            }
        }
        _ => {}
    }
}

fn collect_parameter_names(node: tree_sitter::Node, text: &str, names: &mut Vec<String>) {
    match M2Node::new(node).kind {
        NodeKind::Symbol => names.push(text[node.start_byte()..node.end_byte()].to_string()),
        NodeKind::Sequence | NodeKind::List => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_parameter_names(child, text, names);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssignmentOperator {
    Equal,
    ColonEqual,
    LeftArrow,
    Other,
}

fn assignment_operator(node: tree_sitter::Node, text: &str) -> AssignmentOperator {
    node.child_by_field_name("operator")
        .map(|operator| &text[operator.start_byte()..operator.end_byte()])
        .map(|operator| match operator {
            "=" => AssignmentOperator::Equal,
            ":=" => AssignmentOperator::ColonEqual,
            "<-" => AssignmentOperator::LeftArrow,
            _ => AssignmentOperator::Other,
        })
        .unwrap_or(AssignmentOperator::Other)
}

fn assignment_symbol_kind(node: tree_sitter::Node, text: &str) -> SymbolKind {
    match node.child_by_field_name("right") {
        Some(right)
            if M2Node::new(right).is(NodeKind::NewStatement)
                && new_statement_type_name(right, text) == Some("Type") =>
        {
            SymbolKind::CLASS
        }
        Some(right) if M2Node::new(right).is(NodeKind::LambdaExpression) => SymbolKind::FUNCTION,
        _ => SymbolKind::VARIABLE,
    }
}

fn new_statement_type_name<'a>(node: tree_sitter::Node, text: &'a str) -> Option<&'a str> {
    let type_node = node.child_by_field_name("type")?;
    if !M2Node::new(type_node).is(NodeKind::Symbol) {
        return None;
    }

    Some(&text[type_node.start_byte()..type_node.end_byte()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentSnapshot;
    use crate::typesystem::BuiltinData;
    use crate::util::is_assignment_expression;
    use tower_lsp::lsp_types::{Position, Range};
    use tree_sitter::Parser;

    fn document(text: &str, builtins: &BuiltinData) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), builtins).expect("fixture should parse")
    }

    #[test]
    fn document_symbol_ranges_use_lsp_utf16_columns() {
        let text = "\"😀\"; f := 1";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        assert_eq!(symbols[0].name, "f");
        assert_eq!(
            symbols[0].selection_range,
            Range::new(Position::new(0, 6), Position::new(0, 7))
        );
    }

    #[test]
    fn document_symbols_exclude_package_indexed_option_keys() {
        // Option keys passed to a call that are indexed in a package (here the
        // newPackage keys) are defined in that package, not here.
        let text = "newPackage(\"P\", Version => \"0.1\", DebuggingMode => false)\n";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        assert!(
            symbols
                .iter()
                .all(|symbol| symbol.name != "Version" && symbol.name != "DebuggingMode"),
            "package option keys must not be document symbols: {:?}",
            symbols
                .iter()
                .map(|symbol| &symbol.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn document_symbols_list_custom_option_key_once() {
        // A key not indexed in any package is listed at its first occurrence and
        // not repeated when it is reused.
        let text = "f = method(Options => {MyOpt => 1})\ng(MyOpt => 2)\n";
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        let my_opt: Vec<_> = symbols
            .iter()
            .filter(|symbol| symbol.name == "MyOpt")
            .collect();
        assert_eq!(my_opt.len(), 1, "custom option key listed exactly once");
        assert_eq!(my_opt[0].kind, SymbolKind::PROPERTY);
    }

    #[test]
    fn document_symbols_include_top_level_and_nested_assignments() {
        let text = "f := x -> (y := x + 1; y)\nR = QQ[a]\n";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "f");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert_eq!(
            symbols[0]
                .children
                .as_ref()
                .expect("function should expose local assignment children")[0]
                .name,
            "y"
        );
        assert_eq!(
            symbols[0]
                .children
                .as_ref()
                .expect("function should expose local assignment children")[0]
                .kind,
            SymbolKind::VARIABLE
        );
        assert_eq!(symbols[1].name, "R");
        assert_eq!(symbols[1].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn document_symbols_include_only_new_bindings_in_m2_scopes() {
        let text =
            "x := 1\nx := 2\ny = 1\ny = 2\nf := x -> (x = 2; K = x; z := 3; z := 4)\nK = 3\n";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        assert_eq!(
            symbols
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            vec!["x", "y", "f", "K"]
        );

        let children = symbols[2]
            .children
            .as_ref()
            .expect("function should expose local binding children");

        assert_eq!(
            children
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            vec!["z"]
        );
    }

    #[test]
    fn document_symbols_emit_repeated_top_level_binding_once() {
        // A top-level name assigned more than once is a single static symbol,
        // anchored at its first binding.
        let text = "x = 1\nargs = {}\nargs = append(args, 1)\n";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);
        let args_symbols = symbols
            .iter()
            .filter(|symbol| symbol.name == "args")
            .collect::<Vec<_>>();

        assert_eq!(
            args_symbols.len(),
            1,
            "a repeated top-level binding should be a single static document symbol"
        );
        assert_eq!(
            args_symbols[0].selection_range.start,
            Position::new(1, 0),
            "args should point at its first static binding"
        );
    }

    #[test]
    fn document_symbols_cover_static_top_level_extractor_bindings() {
        fn has_function_ancestor(mut node: tree_sitter::Node) -> bool {
            while let Some(parent) = node.parent() {
                if parent.kind() == "lambda_expression" {
                    return true;
                }
                node = parent;
            }
            false
        }

        fn collect_static_top_level_bindings(
            node: tree_sitter::Node,
            text: &str,
            names: &mut Vec<String>,
        ) {
            if is_assignment_expression(node, text) && !has_function_ancestor(node) {
                if let (Some(left), Some(right)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) {
                    let operator_text = &text[left.end_byte()..right.start_byte()];
                    if left.kind() == "symbol" && operator_text.contains(['=', ':']) {
                        let name = text[left.start_byte()..left.end_byte()].to_string();
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_static_top_level_bindings(child, text, names);
            }
        }

        let text = include_str!("../../example_m2_code/example1.m2");
        let builtins = BuiltinData::empty();
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        let tree = parser.parse(text, None).expect("fixture should parse");
        let mut expected = Vec::new();
        collect_static_top_level_bindings(tree.root_node(), text, &mut expected);

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);
        let actual = symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();

        for name in expected {
            assert!(
                actual.contains(&name.as_str()),
                "missing static top-level document symbol `{name}`"
            );
        }
    }

    #[test]
    fn document_symbols_distinguish_m2_assignment_forms() {
        let text = "\
Thing Thing := (a, b) -> a
Thing .. Thing := (a, b) -> a
toString Tally := f
(x,y) := (1,2)
z = 3
x#i = e
x_i = e
x <- e
(f()) <- e
String * String = (x, y, e) -> e
- String := peek
String ^~ := peek
";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (symbol.name.as_str(), symbol.detail.as_deref(), symbol.kind))
                .collect::<Vec<_>>(),
            vec![
                ("Thing Thing", Some("method"), SymbolKind::METHOD),
                ("Thing .. Thing", Some("method"), SymbolKind::METHOD),
                ("toString Tally", Some("method"), SymbolKind::METHOD),
                ("x", None, SymbolKind::VARIABLE),
                ("y", None, SymbolKind::VARIABLE),
                ("z", None, SymbolKind::VARIABLE),
                ("x_i", Some("indexed variable"), SymbolKind::VARIABLE),
                (
                    "String * String",
                    Some("assignment method"),
                    SymbolKind::METHOD
                ),
                ("- String", Some("method"), SymbolKind::METHOD),
                ("String ^~", Some("method"), SymbolKind::METHOD),
            ]
        );
    }

    #[test]
    fn document_symbols_cover_inheritance_type_and_method_examples() {
        let text = "\
X = new Type of BasicList
Y = new Type of X
Z = new Type of X
- X := t -> apply(t,i -> -i)
Y + X := (a,b) -> \"Y + X\"
X + Z := (a,b) -> \"X + Z\"
";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (symbol.name.as_str(), symbol.detail.as_deref(), symbol.kind))
                .collect::<Vec<_>>(),
            vec![
                ("X", None, SymbolKind::CLASS),
                ("Y", None, SymbolKind::CLASS),
                ("Z", None, SymbolKind::CLASS),
                ("- X", Some("method"), SymbolKind::METHOD),
                ("Y + X", Some("method"), SymbolKind::METHOD),
                ("X + Z", Some("method"), SymbolKind::METHOD),
            ]
        );
    }

    #[test]
    fn document_symbols_include_cst_option_properties() {
        let text = "f := x -> g(x, Strategy => LongPolynomial)";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);
        let children = symbols[0]
            .children
            .as_ref()
            .expect("function body option assignment should appear as child symbols");

        assert_eq!(children[0].name, "Strategy");
        assert_eq!(children[0].kind, SymbolKind::PROPERTY);
        assert_eq!(children[0].detail.as_deref(), Some("option"));
    }

    #[test]
    fn document_symbols_keep_to_type_functions_as_functions() {
        let text = "toString := x -> x";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);

        assert_eq!(symbols[0].name, "toString");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
    }
}
