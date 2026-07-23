//! Document-symbol extraction for Macaulay2 source files.
//!
//! The outline is intentionally static: it reports bindings introduced by the
//! document, nesting bindings created in function bodies beneath their
//! function. Runtime values and names supplied by indexed packages are not
//! treated as document definitions.

use std::collections::HashSet;

use tower_lsp::lsp_types::*;

use crate::document::DocumentSnapshot;
use crate::node_metadata::{M2Node, NodeKind};
use crate::typesystem::BuiltinData;
use crate::util::*;

/// The outline view of a document: assignment bindings (functions nested under
/// their body), method installations, indexed variables, and option keys
/// introduced here (not ones defined by an indexed package).
pub(crate) fn collect_document_symbols(
    document: &DocumentSnapshot,
    builtins: &BuiltinData,
) -> Vec<DocumentSymbol> {
    let mut scopes = DocumentSymbolScopes::new();
    collect_document_symbols_from(document.root_node(), builtins, &mut scopes)
}

/// Build a `DocumentSymbol` while keeping the deprecated LSP field isolated in
/// one compatibility boundary.
#[allow(deprecated)]
fn document_symbol(
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    range: Range,
    selection_range: Range,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children,
    }
}

#[derive(Debug)]
/// Per-document state used to suppress duplicate outline entries.
struct DocumentSymbolScopes {
    /// Names introduced per lexical scope, starting with the document scope.
    names: Vec<HashSet<String>>,
    /// Option keys already emitted for this document.
    options: HashSet<String>,
}

impl DocumentSymbolScopes {
    /// Start with the document's top-level scope.
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

    /// Enter a function body, where `:=` introduces local outline entries.
    fn push(&mut self) {
        self.names.push(HashSet::new());
    }

    /// Leave the current function body.
    fn pop(&mut self) {
        self.names.pop();
    }

    /// Mark an existing name, such as a parameter, as belonging to this scope.
    fn add_current(&mut self, name: &str) {
        if let Some(scope) = self.names.last_mut() {
            scope.insert(name.to_string());
        }
    }

    /// Introduce a local binding, returning whether it was previously absent.
    fn introduce_local(&mut self, name: &str) -> bool {
        let Some(scope) = self.names.last_mut() else {
            return false;
        };
        scope.insert(name.to_string())
    }

    /// Introduce a top-level binding once; assignments in nested scopes do not
    /// contribute document-level entries.
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

/// Walk a syntax subtree and collect only constructs which define outline
/// entries; all other nodes are traversed transparently.
fn collect_document_symbols_from(
    node: M2Node,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Vec<DocumentSymbol> {
    if node.is_assignment() {
        return collect_assignment_document_symbols(node, builtins, scopes);
    }
    if node.is_option_assignment() {
        return collect_property_document_symbols(node, builtins, scopes);
    }

    let mut symbols = Vec::new();
    for child in node.children() {
        symbols.extend(collect_document_symbols_from(child, builtins, scopes));
    }
    symbols
}

/// Emit document symbols for option keys (left of `=>`), but only where a key is
/// actually introduced: a key already indexed in some package is defined there,
/// not here, and a repeated key is listed once. Keys passed to a function call
/// are therefore skipped — they are package symbols or repeats.
fn collect_property_document_symbols(
    node: M2Node,
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
            let name = symbol.text();
            if builtins.contains_name(name) || !scopes.introduce_option(name) {
                return None;
            }
            Some(document_symbol(
                name.to_string(),
                Some("option".to_string()),
                SymbolKind::PROPERTY,
                node_range(node),
                node_range(symbol),
                None,
            ))
        })
        .collect()
}

/// Convert an assignment into symbols for its newly introduced bindings and,
/// when its right side is a function, attach the function body's local symbols.
fn collect_assignment_document_symbols(
    node: M2Node,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Vec<DocumentSymbol> {
    let Some(left) = node.child_by_field_name("left") else {
        return Vec::new();
    };

    let children = match node.child_by_field_name("right") {
        Some(right) if right.is(NodeKind::LambdaExpression) => {
            collect_function_body_document_symbols(right, builtins, scopes)
        }
        _ => None,
    };

    let operator = assignment_operator(node);
    let mut binding_targets = Vec::new();
    collect_binding_target_nodes(left, &mut binding_targets);

    if !binding_targets.is_empty() && operator != AssignmentOperator::LeftArrow {
        return binding_targets
            .into_iter()
            .filter(|symbol| {
                let name = symbol.text();
                match operator {
                    AssignmentOperator::ColonEqual => scopes.introduce_local(name),
                    AssignmentOperator::Equal => scopes.introduce_global_if_missing(name),
                    AssignmentOperator::LeftArrow | AssignmentOperator::Other => false,
                }
            })
            .map(|symbol| {
                document_symbol(
                    symbol.text().to_string(),
                    None,
                    assignment_symbol_kind(node),
                    node_range(node),
                    node_range(symbol),
                    children.clone(),
                )
            })
            .collect();
    }

    let is_method_installation_left = left.kind.is_method_installation_target();

    match (operator, left.binary_operator()) {
        (AssignmentOperator::ColonEqual, _) if is_method_installation_left => {
            vec![document_symbol(
                left.text().to_string(),
                Some("method".to_string()),
                SymbolKind::METHOD,
                node_range(node),
                node_range(left),
                children,
            )]
        }
        (AssignmentOperator::Equal, Some("_")) => vec![document_symbol(
            left.text().to_string(),
            Some("indexed variable".to_string()),
            SymbolKind::VARIABLE,
            node_range(node),
            node_range(left),
            None,
        )],
        (AssignmentOperator::Equal, Some(_))
            if node
                .child_by_field_name("right")
                .is_some_and(|right| right.is(NodeKind::LambdaExpression))
                && is_method_installation_left =>
        {
            vec![document_symbol(
                left.text().to_string(),
                Some("assignment method".to_string()),
                SymbolKind::METHOD,
                node_range(node),
                node_range(left),
                children,
            )]
        }
        _ => Vec::new(),
    }
}

/// Collect nested outline entries for one function body in an isolated lexical
/// scope. Parameters are pre-recorded so assignments to them are not emitted.
fn collect_function_body_document_symbols(
    function_node: M2Node,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Option<Vec<DocumentSymbol>> {
    let body = function_node.child_by_field_name("body")?;

    scopes.push();
    if let Some(params) = function_node.child_by_field_name("parameters") {
        let mut names = Vec::new();
        collect_parameter_names(params, &mut names);
        for name in names {
            scopes.add_current(&name);
        }
    }

    let children = collect_document_symbols_from(body, builtins, scopes);
    scopes.pop();

    (!children.is_empty()).then_some(children)
}

fn collect_left_symbol_nodes<'tree>(node: M2Node<'tree>, symbols: &mut Vec<M2Node<'tree>>) {
    match node.kind {
        NodeKind::Symbol => symbols.push(node),
        kind if kind.is_collection_expression() => {
            for child in node.children() {
                collect_left_symbol_nodes(child, symbols);
            }
        }
        _ => {}
    }
}

/// Every symbol bound by a destructuring target, recursing through nested
/// collections (`[x, [y, z]] := …` binds all three).
fn collect_binding_target_nodes<'tree>(node: M2Node<'tree>, symbols: &mut Vec<M2Node<'tree>>) {
    match node.kind {
        NodeKind::Symbol => symbols.push(node),
        kind if kind.is_collection_expression() => {
            for child in node.named_children() {
                collect_binding_target_nodes(child, symbols);
            }
        }
        _ => {}
    }
}

/// Gather parameter names from nested parameter collections.
fn collect_parameter_names(node: M2Node, names: &mut Vec<String>) {
    match node.kind {
        NodeKind::Symbol => names.push(node.text().to_string()),
        // `(x,y)` is a `sequence`; a single `(x)` is a `parenthesized_expression`.
        kind if kind.is_collection_expression() || kind == NodeKind::ParenthesizedExpression => {
            for child in node.children() {
                collect_parameter_names(child, names);
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

/// Classify the parsed assignment operator for the outline's binding rules.
fn assignment_operator(node: M2Node) -> AssignmentOperator {
    match node.binary_operator() {
        Some("=") => AssignmentOperator::Equal,
        Some(":=") => AssignmentOperator::ColonEqual,
        Some("<-") => AssignmentOperator::LeftArrow,
        _ => AssignmentOperator::Other,
    }
}

/// Select the LSP outline kind from the assigned expression when it conveys a
/// more specific static declaration than a regular variable.
fn assignment_symbol_kind(node: M2Node) -> SymbolKind {
    match node.child_by_field_name("right") {
        Some(right)
            if right.is(NodeKind::NewStatement)
                && new_statement_type_name(right) == Some("Type") =>
        {
            SymbolKind::CLASS
        }
        Some(right) if right.is(NodeKind::LambdaExpression) => SymbolKind::FUNCTION,
        _ => SymbolKind::VARIABLE,
    }
}

/// Return the declared type name from a `new` expression when it is a symbol.
fn new_statement_type_name<'tree>(node: M2Node<'tree>) -> Option<&'tree str> {
    let type_node = node.child_by_field_name("type")?;
    if !type_node.is(NodeKind::Symbol) {
        return None;
    }

    Some(type_node.text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentSnapshot;
    use crate::typesystem::BuiltinData;
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
        fn has_function_ancestor(node: M2Node) -> bool {
            let mut node = node;
            while let Some(parent) = node.parent() {
                if parent.kind == NodeKind::LambdaExpression {
                    return true;
                }
                node = parent;
            }
            false
        }

        fn collect_static_top_level_bindings(node: M2Node, names: &mut Vec<String>) {
            if node.is_assignment() && !has_function_ancestor(node) {
                if let (Some(left), Some(operator)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("operator"),
                ) {
                    let operator_text = operator.text();
                    if left.kind == NodeKind::Symbol && operator_text.contains(['=', ':']) {
                        let name = left.text().to_string();
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
            }

            for child in node.children() {
                collect_static_top_level_bindings(child, names);
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
        collect_static_top_level_bindings(M2Node::new(tree.root_node(), text), &mut expected);

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
    fn document_symbols_cover_bracket_and_nested_destructuring_targets() {
        // `[x, y] := …` and nested `[p, [q, r]] := …` bind exactly like the
        // sequence form — the outline must list every bound name.
        let text = "[x, y] := {1, 2}\n[p, [q, r]] := [1, [2, 3]]\n";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let symbols = collect_document_symbols(&document, &builtins);
        let names: Vec<_> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();

        assert_eq!(names, vec!["x", "y", "p", "q", "r"]);
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
