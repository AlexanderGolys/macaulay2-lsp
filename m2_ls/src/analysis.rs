use std::collections::{HashMap, HashSet};
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
    pub local_methods: HashMap<String, LocalMethodInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMethodSignature {
    pub domain: Vec<String>,
    pub codomain: Option<String>,
    pub range: LspRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMethodInfo {
    pub name: String,
    pub range: LspRange,
    pub typical_value: Option<String>,
    pub signatures: Vec<LocalMethodSignature>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStaticFacts {
    pub argument_types: Vec<Option<String>>,
    pub literal_options: Vec<(String, String)>,
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
            local_methods: HashMap::new(),
        };
        analysis.collect_diagnostics(tree.root_node(), text);
        analysis.build_scopes(tree.root_node(), text, 0, builtins);
        analysis
    }

    fn collect_diagnostics(&mut self, node: Node, text: &str) {
        if node.is_error() {
            self.diagnostics.push(Diagnostic {
                range: single_line_range(text, node.start_position(), node.start_byte()),
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
        } else if is_assignment_expression(node, text) {
            self.validate_assignment_form(node, text);
        } else if node.kind() == "cell" {
            self.diagnose_orphan_else(node, text);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_diagnostics(child, text);
        }
    }

    fn diagnose_orphan_else(&mut self, cell: Node, text: &str) {
        let mut cursor = cell.walk();
        for child in cell.children(&mut cursor) {
            let else_symbol = find_first_else_symbol(child, text);
            if let Some(symbol) = else_symbol {
                self.diagnostics.push(Diagnostic {
                    range: to_lsp_range(text, symbol.range()),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: "An else clause must appear on the same line as its if statement"
                        .to_string(),
                    ..Default::default()
                });
                return;
            }
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

        let is_method_installation =
            op_text == ":=" && method_installation_signature(left, text).is_some();

        if matches!(op_text, "=" | ":=")
            && !is_method_installation
            && !multiple_assignment_targets_are_symbols(left)
        {
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
            "lambda_expression" => {
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
                    let parameter_types =
                        method_installation_parameter_types_for_function(node, text);
                    self.collect_parameters(
                        params_node,
                        text,
                        next_scope_idx,
                        parameter_types.as_deref(),
                    );
                }
            }
            _ if is_assignment_expression(node, text) => {
                let left = node.child_by_field_name("left");
                let op = node.child_by_field_name("operator");
                let right = node.child_by_field_name("right");

                if let (Some(left), Some(op)) = (left, op) {
                    let op_text = &text[op.start_byte()..op.end_byte()];
                    if op_text == ":=" {
                        self.collect_local_method_installation(left, right, text);
                    }
                    let symbol_kind = match right {
                        Some(right) if right.kind() == "lambda_expression" => SymbolKind::Function,
                        Some(right) if method_declaration_typical_value(right, text).is_some() => {
                            SymbolKind::Function
                        }
                        _ => SymbolKind::Variable,
                    };
                    let type_name = right.and_then(|right| {
                        if method_declaration_typical_value(right, text).is_some() {
                            Some("MethodFunction".to_string())
                        } else {
                            self.infer_static_type_name(right, text, current_scope_idx, builtins)
                        }
                    });

                    if let (Some(right), Some(name)) =
                        (right, single_symbol_assignment_target(left, text))
                    {
                        if let Some(typical_value) = method_declaration_typical_value(right, text) {
                            self.record_local_method_declaration(name, typical_value, left, text);
                        }
                    }

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

    fn collect_parameters(
        &mut self,
        node: Node,
        text: &str,
        scope_idx: usize,
        parameter_types: Option<&[String]>,
    ) {
        let mut parameter_nodes = Vec::new();
        collect_parameter_nodes(node, &mut parameter_nodes);
        let typed_parameters = parameter_types.filter(|types| types.len() == parameter_nodes.len());
        for (idx, parameter_node) in parameter_nodes.into_iter().enumerate() {
            let name = &text[parameter_node.start_byte()..parameter_node.end_byte()];
            let type_name = typed_parameters
                .and_then(|types| types.get(idx))
                .map(String::as_str);
            self.add_symbol(
                name,
                SymbolKind::Parameter,
                type_name,
                parameter_node,
                scope_idx,
                text,
            );
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

    pub fn local_method(&self, name: &str) -> Option<&LocalMethodInfo> {
        self.local_methods.get(name)
    }

    pub fn local_method_installation_signature_at<'a>(
        &'a self,
        node: Node,
        text: &str,
    ) -> Option<(&'a LocalMethodInfo, &'a LocalMethodSignature)> {
        let installation = method_installation_expression_for_callable_node(node, text)?;
        let (name, domain) = method_installation_signature(installation, text)?;
        let method = self.local_methods.get(&name)?;
        let installation_range = to_lsp_range(text, installation.range());
        let signature = method
            .signatures
            .iter()
            .rev()
            .find(|signature| signature.domain == domain && signature.range == installation_range)
            .or_else(|| {
                method
                    .signatures
                    .iter()
                    .rev()
                    .find(|signature| signature.domain == domain)
            })?;

        Some((method, signature))
    }

    pub fn infer_call_static_facts(
        &self,
        node: Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) -> CallStaticFacts {
        let scope_idx = self.find_scope_at(node_position(text, node)).unwrap_or(0);
        self.infer_call_facts(node, text, scope_idx, builtins)
    }

    pub fn infer_expression_static_type_name(
        &self,
        node: Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        let scope_idx = self.find_scope_at(node_position(text, node)).unwrap_or(0);
        self.infer_static_type_name(node, text, scope_idx, builtins)
    }

    fn record_local_method_declaration(
        &mut self,
        name: &str,
        typical_value: Option<String>,
        node: Node,
        text: &str,
    ) {
        let range = to_lsp_range(text, node.range());
        let method = self
            .local_methods
            .entry(name.to_string())
            .or_insert_with(|| LocalMethodInfo {
                name: name.to_string(),
                range,
                typical_value: None,
                signatures: Vec::new(),
            });
        method.range = range;
        method.typical_value = typical_value;
    }

    fn collect_local_method_installation(&mut self, node: Node, right: Option<Node>, text: &str) {
        let Some((name, domain)) = method_installation_signature(node, text) else {
            return;
        };
        let range = to_lsp_range(text, node.range());
        let method = self
            .local_methods
            .entry(name.to_string())
            .or_insert_with(|| LocalMethodInfo {
                name: name.to_string(),
                range,
                typical_value: None,
                signatures: Vec::new(),
            });
        let codomain = right
            .and_then(|right| explicit_method_installation_codomain(right, text))
            .or_else(|| method.typical_value.clone());
        method.signatures.push(LocalMethodSignature {
            domain,
            codomain,
            range,
        });
    }

    fn infer_static_type_name(
        &self,
        node: Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        match node.kind() {
            "lambda_expression" => Some("Function".to_string()),
            "binary_expression" if method_declaration_typical_value(node, text).is_some() => {
                Some("MethodFunction".to_string())
            }
            "list" => Some("List".to_string()),
            "array" => Some("Array".to_string()),
            "angle_bar_list" => Some("AngleBarList".to_string()),
            "sequence" => self.infer_sequence_static_type_name(node, text, scope_idx, builtins),
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
                    .map(|record| record.class.0)
            }
            _ if is_assignment_expression(node, text) => {
                let right = node.child_by_field_name("right")?;
                self.infer_static_type_name(right, text, scope_idx, builtins)
            }
            "binary_expression" => {
                if is_space_operator_expression(node) {
                    let callable = node.child_by_field_name("left")?;
                    let argument = node.child_by_field_name("right")?;
                    let call_facts = self.infer_call_facts(argument, text, scope_idx, builtins);
                    if let Some(callable_name) = symbol_node_text(callable, text) {
                        if let Some(return_type) = self.resolve_local_call_return_type(
                            callable_name,
                            &call_facts.argument_types,
                            builtins,
                        ) {
                            return Some(return_type);
                        }
                        if let Some(return_type) = builtins.and_then(|builtins| {
                            builtins.resolve_call_return_type_with_options(
                                callable_name,
                                &call_facts.argument_types,
                                &call_facts.literal_options,
                            )
                        }) {
                            return Some(return_type);
                        }
                    }

                    let builtins = builtins?;
                    let left_type =
                        self.infer_static_type_name(callable, text, scope_idx, Some(builtins));
                    let right_type =
                        self.infer_static_type_name(argument, text, scope_idx, Some(builtins));
                    builtins.resolve_call_return_type_with_options(
                        "SPACE",
                        &[left_type, right_type],
                        &call_facts.literal_options,
                    )
                } else {
                    let builtins = builtins?;
                    let operator = binary_expression_operator(node, text)?;
                    let left = node.child_by_field_name("left")?;
                    let right = node.child_by_field_name("right")?;
                    let left_type =
                        self.infer_static_type_name(left, text, scope_idx, Some(builtins));
                    let right_type =
                        self.infer_static_type_name(right, text, scope_idx, Some(builtins));
                    builtins.resolve_call_return_type(operator, &[left_type, right_type])
                }
            }
            "new_statement" => node
                .child_by_field_name("type")
                .filter(|type_node| type_node.kind() == "symbol")
                .map(|type_node| text[type_node.start_byte()..type_node.end_byte()].to_string()),
            _ => None,
        }
    }

    fn infer_call_facts(
        &self,
        node: Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> CallStaticFacts {
        if node.kind() == "sequence" {
            let mut facts = CallStaticFacts::default();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if let Some(option) = literal_option_assignment(child, text) {
                    facts.literal_options.push(option);
                } else {
                    facts
                        .argument_types
                        .push(self.infer_static_type_name(child, text, scope_idx, builtins));
                }
            }
            return facts;
        }

        if let Some(option) = literal_option_assignment(node, text) {
            return CallStaticFacts {
                argument_types: Vec::new(),
                literal_options: vec![option],
            };
        }

        CallStaticFacts {
            argument_types: vec![self.infer_static_type_name(node, text, scope_idx, builtins)],
            literal_options: Vec::new(),
        }
    }

    fn infer_sequence_static_type_name(
        &self,
        node: Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        match children.as_slice() {
            [child] => self.infer_static_type_name(*child, text, scope_idx, builtins),
            _ => Some("Sequence".to_string()),
        }
    }

    fn resolve_local_call_return_type(
        &self,
        callable_name: &str,
        argument_types: &[Option<String>],
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        let method = self.local_methods.get(callable_name)?;
        let matching_codomains = method
            .signatures
            .iter()
            .filter(|signature| signature_matches(signature, argument_types, builtins))
            .filter_map(|signature| {
                signature
                    .codomain
                    .clone()
                    .or_else(|| method.typical_value.clone())
            })
            .collect::<HashSet<_>>();

        if matching_codomains.len() == 1 {
            return matching_codomains.into_iter().next();
        }

        method.typical_value.clone()
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

fn collect_parameter_nodes<'tree>(node: Node<'tree>, parameters: &mut Vec<Node<'tree>>) {
    match node.kind() {
        "symbol" => parameters.push(node),
        "sequence" | "list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_parameter_nodes(child, parameters);
            }
        }
        _ => {}
    }
}

fn single_symbol_assignment_target<'a>(node: Node, text: &'a str) -> Option<&'a str> {
    (node.kind() == "symbol").then(|| &text[node.start_byte()..node.end_byte()])
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

fn is_space_operator_expression(node: Node<'_>) -> bool {
    node.kind() == "binary_expression" && binary_expression_operator_kind(node) == Some("SPACE")
}

fn is_assignment_expression(node: Node<'_>, text: &str) -> bool {
    node.kind() == "binary_expression"
        && matches!(
            binary_expression_operator(node, text),
            Some("=" | ":=" | "<-")
        )
}

fn is_option_assignment_expression(node: Node<'_>, text: &str) -> bool {
    node.kind() == "binary_expression" && binary_expression_operator(node, text) == Some("=>")
}

fn symbol_node_text<'a>(node: Node, text: &'a str) -> Option<&'a str> {
    matches!(
        node.kind(),
        "symbol" | "identifier" | "resolved_symbol" | "builtin_constant"
    )
    .then(|| &text[node.start_byte()..node.end_byte()])
}

fn method_declaration_typical_value(node: Node, text: &str) -> Option<Option<String>> {
    if !is_space_operator_expression(node) {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if symbol_node_text(left, text) != Some("method") {
        return None;
    }

    Some(find_option_value(node, text, "TypicalValue"))
}

fn find_option_value(node: Node, text: &str, option_name: &str) -> Option<String> {
    if is_option_assignment_expression(node, text) {
        let left = node.child_by_field_name("left")?;
        let right = node.child_by_field_name("right")?;
        if symbol_node_text(left, text) == Some(option_name) {
            return symbol_node_text(right, text).map(ToString::to_string);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(value) = find_option_value(child, text, option_name) {
            return Some(value);
        }
    }
    None
}

fn literal_option_assignment(node: Node, text: &str) -> Option<(String, String)> {
    if !is_option_assignment_expression(node, text) {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    let key = symbol_node_text(left, text)?;
    let value = literal_option_value(right, text)?;
    Some((key.to_string(), value.to_string()))
}

fn literal_option_value<'a>(node: Node, text: &'a str) -> Option<&'a str> {
    match node.kind() {
        "symbol" | "identifier" | "resolved_symbol" | "builtin_constant" | "boolean_literal"
        | "integer_literal" | "string_literal" => Some(&text[node.start_byte()..node.end_byte()]),
        _ => None,
    }
}

fn explicit_method_installation_codomain(node: Node, text: &str) -> Option<String> {
    if !is_option_assignment_expression(node, text) {
        return None;
    }

    let codomain = node.child_by_field_name("left")?;
    symbol_node_text(codomain, text).map(ToString::to_string)
}

fn method_installation_signature(node: Node, text: &str) -> Option<(String, Vec<String>)> {
    if !is_space_operator_expression(node) {
        return None;
    }

    let callable = node.child_by_field_name("left")?;
    let arguments = node.child_by_field_name("right")?;
    let callable_name = symbol_node_text(callable, text)?;
    let domain = method_installation_domain(arguments, text)?;
    Some((callable_name.to_string(), domain))
}

fn method_installation_parameter_types_for_function(
    function_node: Node,
    text: &str,
) -> Option<Vec<String>> {
    let mut current = function_node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "lambda_expression" {
            return None;
        }

        if is_assignment_expression(parent, text) {
            let left = parent.child_by_field_name("left")?;
            let right = parent.child_by_field_name("right")?;
            let operator = parent.child_by_field_name("operator")?;
            if &text[operator.start_byte()..operator.end_byte()] != ":=" {
                return None;
            }
            if !node_is_within(right, function_node) {
                return None;
            }
            return method_installation_signature(left, text).map(|(_, domain)| domain);
        }

        current = parent;
    }

    None
}

fn method_installation_expression_for_callable_node<'tree>(
    node: Node<'tree>,
    text: &str,
) -> Option<Node<'tree>> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        current = parent;

        if !is_space_operator_expression(current) {
            continue;
        }

        let callable = current.child_by_field_name("left")?;
        if !node_is_within(callable, node) {
            continue;
        }

        if is_colon_equal_assignment_left(current, text) {
            return Some(current);
        }
    }

    None
}

fn method_installation_domain(node: Node, text: &str) -> Option<Vec<String>> {
    if matches!(node.kind(), "sequence" | "list") {
        let mut cursor = node.walk();
        let domain = node
            .named_children(&mut cursor)
            .filter_map(|child| symbol_node_text(child, text).map(ToString::to_string))
            .collect::<Vec<_>>();
        return (!domain.is_empty()).then_some(domain);
    }

    symbol_node_text(node, text).map(|name| vec![name.to_string()])
}

fn node_is_within(ancestor: Node, node: Node) -> bool {
    ancestor.start_byte() <= node.start_byte() && node.end_byte() <= ancestor.end_byte()
}

fn is_colon_equal_assignment_left(node: Node, text: &str) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !is_assignment_expression(parent, text) {
        return false;
    }
    if !parent
        .child_by_field_name("left")
        .is_some_and(|left| left.id() == node.id())
    {
        return false;
    }

    parent
        .child_by_field_name("operator")
        .is_some_and(|operator| &text[operator.start_byte()..operator.end_byte()] == ":=")
}

fn find_first_else_symbol<'tree>(node: Node<'tree>, text: &str) -> Option<Node<'tree>> {
    if node.kind() == "symbol" && &text[node.start_byte()..node.end_byte()] == "else" {
        return Some(node);
    }
    if let Some(left) = node.child_by_field_name("left") {
        if let Some(result) = find_first_else_symbol(left, text) {
            return Some(result);
        }
    }
    if let Some(operand) = node.child_by_field_name("operand") {
        if let Some(result) = find_first_else_symbol(operand, text) {
            return Some(result);
        }
    }
    None
}

fn signature_matches(
    signature: &LocalMethodSignature,
    argument_types: &[Option<String>],
    builtins: Option<&BuiltinData>,
) -> bool {
    signature.domain.len() == argument_types.len()
        && signature
            .domain
            .iter()
            .zip(argument_types)
            .all(|(expected, actual)| {
                actual.as_ref().is_some_and(|actual| {
                    actual == expected
                        || builtins.is_some_and(|builtins| {
                            builtins
                                .is_subtype(&InstanceID::new(actual), &InstanceID::new(expected))
                        })
                })
            })
}

fn single_line_range(text: &str, start: tree_sitter::Point, start_byte: usize) -> LspRange {
    let start_line_byte = start_byte.saturating_sub(start.column);
    let line_end_byte = text[start_byte..]
        .find('\n')
        .map(|i| start_byte + i)
        .unwrap_or(text.len());

    LspRange::new(
        Position::new(
            start.row as u32,
            utf16_len_for_byte_span(text, start_line_byte, start_byte),
        ),
        Position::new(
            start.row as u32,
            utf16_len_for_byte_span(text, start_line_byte, line_end_byte),
        ),
    )
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
    fn infers_static_types_from_documented_call_signatures() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let analysis = analyze_with_builtins(
            "I := new Ideal from {}\nR := ring I\nS := ring x\nR\nS\n",
            &builtins,
        );

        assert_eq!(
            analysis
                .get_symbol_at("R", Position::new(3, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Ring")
        );
        assert_eq!(
            analysis
                .get_symbol_at("S", Position::new(4, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn specialized_documented_signatures_override_general_signatures() {
        let builtins = BuiltinData::load_from_split(
            "f\n",
            "{\"name\":\"f\",\"data_type\":\"MethodFunction\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"Ideal\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"Ideal\"],\"output_types\":[\"Ring\"]}],\"general_signature\":{\"signature\":[\"f\"],\"output_types\":[\"Thing\"]}}}\n",
        );
        let analysis = analyze_with_builtins("I := new Ideal from {}\nR := f I\nR\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("R", Position::new(2, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn infers_static_types_from_documented_operator_signatures() {
        let builtins = BuiltinData::load_from_split(
            "+\n",
            "{\"name\":\"+\",\"data_type\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"],\"output_types\":[\"ZZ\"]}]}}\n",
        );
        let analysis = analyze_with_builtins("x := 1\ny := 2\nz := x + y\nz\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("z", Position::new(3, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("ZZ")
        );
    }

    #[test]
    fn records_local_method_declarations_and_installed_signatures() {
        let analysis = analyze(
            "p = method(Binary => true, TypicalValue => List)\np(ZZ,ZZ) := p(List,ZZ) := (i,j) -> {i,j}\n",
        );
        let method = analysis
            .local_method("p")
            .expect("method declaration should create local method metadata");

        assert_eq!(method.typical_value.as_deref(), Some("List"));
        assert_eq!(
            method
                .signatures
                .iter()
                .map(|signature| signature.domain.clone())
                .collect::<Vec<_>>(),
            vec![
                vec!["ZZ".to_string(), "ZZ".to_string()],
                vec!["List".to_string(), "ZZ".to_string()]
            ]
        );
        assert!(method
            .signatures
            .iter()
            .all(|signature| signature.codomain.as_deref() == Some("List")));
        assert_eq!(
            analysis
                .get_symbol_at("p", Position::new(1, 0))
                .map(|symbol| symbol.kind),
            Some(SymbolKind::Function)
        );
    }

    #[test]
    fn infers_static_types_from_local_method_typical_values() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let analysis = analyze_with_builtins(
            "p = method(Binary => true, TypicalValue => List)\np(ZZ,ZZ) := p(List,ZZ) := (i,j) -> {i,j}\nx := p(1, 2)\nx\n",
            &builtins,
        );

        assert_eq!(
            analysis
                .get_symbol_at("x", Position::new(3, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("List")
        );
    }

    #[test]
    fn method_installation_domains_type_function_parameters() {
        let analysis = analyze("f ZZ := d -> (\n  a := d\n)\n");

        assert_eq!(
            analysis
                .get_symbol_at("d", Position::new(1, 7))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("ZZ")
        );
    }

    #[test]
    fn method_installation_domains_do_not_type_nested_function_parameters() {
        let analysis = analyze("f(ZZ) := x -> (\n  h := y -> y\n  h x\n)\n");

        assert_eq!(
            analysis
                .get_symbol_at("x", Position::new(2, 4))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("ZZ")
        );
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(1, 12))
                .and_then(|symbol| symbol.type_name.as_deref()),
            None
        );
    }

    #[test]
    fn local_methods_without_codomains_remain_unknown() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let analysis =
            analyze_with_builtins("f = method()\nf ZZ := x -> -x\ny := f 1\ny\n", &builtins);

        let method = analysis
            .local_method("f")
            .expect("method declaration should be tracked");
        assert_eq!(method.typical_value, None);
        assert_eq!(method.signatures[0].domain, vec!["ZZ"]);
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(3, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            None
        );
    }

    #[test]
    fn explicit_local_method_codomains_override_typical_values() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let analysis = analyze_with_builtins(
            "f = method(TypicalValue => List)\nf ZZ := Ring => x -> x\ny := f 1\ny\n",
            &builtins,
        );

        let method = analysis
            .local_method("f")
            .expect("local method should be tracked");
        assert_eq!(method.typical_value.as_deref(), Some("List"));
        assert_eq!(method.signatures[0].codomain.as_deref(), Some("Ring"));
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(3, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn infers_static_types_from_option_sensitive_facts() {
        let builtins = BuiltinData::load_from_split_with_type_facts(
            "f\n",
            "{\"name\":\"f\",\"data_type\":\"MethodFunctionWithOptions\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"ZZ\"],\"output_types\":[\"String\"]}]}}\n",
            "{\"callable\":\"f\",\"option_codomains\":[{\"domain\":[\"ZZ\"],\"key\":\"Mode\",\"value\":\"AsList\",\"codomain\":\"List\"}]}\n",
        );
        let analysis = analyze_with_builtins("y := f(1, Mode => AsList)\ny\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(1, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("List")
        );
    }

    #[test]
    fn call_options_do_not_count_as_positional_arguments() {
        let builtins = BuiltinData::load_from_split(
            "f\n",
            "{\"name\":\"f\",\"data_type\":\"MethodFunctionWithOptions\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"ZZ\"],\"output_types\":[\"String\"]}]}}\n",
        );
        let analysis = analyze_with_builtins("y := f(1, Mode => AsList)\ny\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(1, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("String")
        );
    }

    #[test]
    fn try_then_except_expression_does_not_produce_syntax_diagnostics() {
        let analysis = analyze("apply(-3..3, i -> try 1/i then 1 / i except err do err)");

        assert!(
            analysis.diagnostics.is_empty(),
            "current grammar should accept try/then/except expressions without syntax diagnostics"
        );
    }

    #[test]
    fn infers_static_types_from_space_adjacency_facts() {
        let builtins = BuiltinData::load_from_split_with_type_facts(
            "QQ\nSPACE\n",
            "{\"name\":\"QQ\",\"data_type\":\"Ring\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{}}\n{\"name\":\"SPACE\",\"data_type\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"SPACE\",\"Ring\",\"Array\"]}]}}\n",
            "{\"callable\":\"SPACE\",\"signatures\":[{\"domain\":[\"Ring\",\"Array\"],\"codomain\":\"Ring\"}]}\n",
        );
        let analysis = analyze_with_builtins("R := QQ\nS := R[x,y]\nS\n", &builtins);

        assert_eq!(
            analysis
                .get_symbol_at("S", Position::new(2, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Ring")
        );
    }

    #[test]
    fn infers_static_types_from_container_literals() {
        let analysis = analyze(
            "l := {1,2}\na := [1,2]\nb := <|1,2|>\ne := ()\nf := (1)\ng := (1,2)\nl\na\nb\ne\nf\ng\n",
        );

        assert_eq!(
            analysis
                .get_symbol_at("l", Position::new(6, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("List")
        );
        assert_eq!(
            analysis
                .get_symbol_at("a", Position::new(7, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Array")
        );
        assert_eq!(
            analysis
                .get_symbol_at("b", Position::new(8, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("AngleBarList")
        );
        assert_eq!(
            analysis
                .get_symbol_at("e", Position::new(9, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Sequence")
        );
        assert_eq!(
            analysis
                .get_symbol_at("f", Position::new(10, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("ZZ")
        );
        assert_eq!(
            analysis
                .get_symbol_at("g", Position::new(11, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            Some("Sequence")
        );
    }

    #[test]
    fn diagnoses_structurally_invalid_assignment_forms() {
        let analysis = analyze(
            "x#i := e\n(x+1,y) = (1,2)\n(x+1,y) := (1,2)\n(f()) <- (1)\nsource(String,Number) := peek\np(ZZ, ZZ) := (i, j) -> {i, j}\n",
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

    #[test]
    fn diagnoses_orphan_else_on_new_line_in_global_scope() {
        let analysis = analyze("if x then y\n    else z");
        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].message,
            "An else clause must appear on the same line as its if statement"
        );
        assert_eq!(analysis.diagnostics[0].range.start, Position::new(1, 4));
        assert_eq!(analysis.diagnostics[0].range.end, Position::new(1, 8));
    }

    #[test]
    fn no_diagnostic_for_else_on_same_line() {
        let analysis = analyze("if x then y else z");
        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn no_diagnostic_for_if_without_else() {
        let analysis = analyze("if x then y");
        assert!(analysis.diagnostics.is_empty());
    }
}
