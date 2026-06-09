use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use tower_lsp::lsp_types::{Diagnostic, Position, Range as LspRange, SymbolKind};
use tree_sitter::Tree;

use crate::capabilities::diagnostics::ORPHAN_ELSE_DIAGNOSTIC_MESSAGE;
use crate::node_metadata::{M2Node, NodeKind};
use crate::typesystem::{BuiltinData, InstanceID};
use crate::util::binary_expression_operator_kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingRole {
    Ordinary,
    Parameter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(u32);

#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub kind: SymbolKind,
    pub role: BindingRole,
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
    pub registry: SemanticRegistry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInfo {
    pub domain: Vec<String>,
    pub codomain: Option<String>,
    pub range: LspRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub symbol: SymbolId,
    pub range: LspRange,
    pub typical_value: Option<String>,
    pub methods: Vec<MethodInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStaticFacts {
    pub argument_types: Vec<Option<String>>,
    pub literal_options: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanKey {
    pub range: LspRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpressionType {
    Unknown,
    Known(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionKind {
    Literal,
    Name,
    Expr,
    Assign,
    ScopeExpr,
    ControlExpr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingInfo {
    pub symbol: SymbolId,
    pub kind: SymbolKind,
    pub role: BindingRole,
    pub range: LspRange,
    pub scope_idx: usize,
    pub type_name: Option<String>,
    pub value_range: Option<LspRange>,
    pub declaration_range: LspRange,
    pub span: SpanKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeInfo {
    pub range: LspRange,
    pub parent_idx: Option<usize>,
    pub introducer: Option<SpanKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionFact {
    pub span: SpanKey,
    pub kind: ExpressionKind,
    pub input_nodes: Vec<SpanKey>,
    pub operator: Option<String>,
    pub result_type: ExpressionType,
    pub scope_idx: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallInfo {
    pub span: SpanKey,
    pub callable_name: Option<String>,
    pub argument_types: Vec<Option<String>>,
    pub result_type: ExpressionType,
    pub candidate_methods: Vec<MethodInfo>,
}

#[derive(Debug, Default)]
pub struct SemanticRegistry {
    pub symbol_names: Vec<String>,
    pub symbol_ids: HashMap<String, SymbolId>,
    pub scopes: Vec<ScopeInfo>,
    pub bindings: Vec<BindingInfo>,
    pub bindings_by_symbol: HashMap<SymbolId, Vec<usize>>,
    pub node_scopes: HashMap<SpanKey, usize>,
    pub expressions: HashMap<SpanKey, ExpressionFact>,
    pub calls: HashMap<SpanKey, CallInfo>,
    pub functions: HashMap<SymbolId, FunctionInfo>,
}

impl SpanKey {
    fn from_node(text: &str, node: M2Node) -> Self {
        Self {
            range: to_lsp_range(text, node.range()),
        }
    }
}

impl Hash for SpanKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.range.start.line.hash(state);
        self.range.start.character.hash(state);
        self.range.end.line.hash(state);
        self.range.end.character.hash(state);
    }
}

impl SemanticRegistry {
    fn intern_symbol(&mut self, name: &str) -> SymbolId {
        if let Some(symbol) = self.symbol_ids.get(name) {
            return *symbol;
        }
        let symbol = SymbolId(self.symbol_names.len() as u32);
        self.symbol_names.push(name.to_string());
        self.symbol_ids.insert(name.to_string(), symbol);
        symbol
    }

    fn resolve_symbol(&self, name: &str) -> Option<SymbolId> {
        self.symbol_ids.get(name).copied()
    }

    fn symbol_name(&self, symbol: SymbolId) -> &str {
        &self.symbol_names[symbol.0 as usize]
    }
}

impl Analysis {
    pub fn find_definition(&self, name: &str, pos: Position) -> Option<LspRange> {
        self.get_symbol_at(name, pos).map(|symbol| symbol.range)
    }

    pub fn get_symbol_at(&self, name: &str, pos: Position) -> Option<&SymbolInfo> {
        self.lookup_symbol_at(name, pos)
    }

    #[cfg(test)]
    pub fn registry(&self) -> &SemanticRegistry {
        &self.registry
    }

    pub fn get_binding_at(&self, name: &str, pos: Position) -> Option<&BindingInfo> {
        let scope_idx = self.find_scope_at(pos)?;
        let symbol = self.registry.resolve_symbol(name)?;
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            let binding = self
                .registry
                .bindings_by_symbol
                .get(&symbol)
                .into_iter()
                .flatten()
                .filter_map(|binding_idx| self.registry.bindings.get(*binding_idx))
                .filter(|binding| binding.scope_idx == idx && binding.range.start <= pos)
                .max_by_key(|binding| (binding.range.start.line, binding.range.start.character));
            if binding.is_some() {
                return binding;
            }
            curr = self.scopes[idx].parent_idx;
        }
        None
    }

    #[cfg(test)]
    pub fn expression_fact(&self, text: &str, node: M2Node) -> Option<&ExpressionFact> {
        self.registry
            .expressions
            .get(&SpanKey::from_node(text, node))
    }

    #[cfg(test)]
    pub fn function(&self, name: &str) -> Option<&FunctionInfo> {
        let symbol = self.registry.resolve_symbol(name)?;
        self.registry.functions.get(&symbol)
    }

    pub fn function_by_symbol(&self, symbol: SymbolId) -> Option<&FunctionInfo> {
        self.registry.functions.get(&symbol)
    }

    pub fn symbol_name(&self, symbol: SymbolId) -> &str {
        self.registry.symbol_name(symbol)
    }

    #[cfg(test)]
    pub fn binding_name(&self, binding: &BindingInfo) -> &str {
        self.symbol_name(binding.symbol)
    }

    pub fn typed_bindings_in_range(&self, range: LspRange) -> Vec<&BindingInfo> {
        self.registry
            .bindings
            .iter()
            .filter(|binding| binding.type_name.is_some())
            .filter(|binding| matches!(binding.kind, SymbolKind::VARIABLE | SymbolKind::FUNCTION))
            .filter(|binding| {
                let position = binding.range.end;
                is_pos_in_range(position, range)
            })
            .collect()
    }

    pub fn typed_expression_facts_in_range(&self, range: LspRange) -> Vec<&ExpressionFact> {
        self.registry
            .expressions
            .values()
            .filter(|fact| matches!(fact.kind, ExpressionKind::Expr))
            .filter(|fact| matches!(fact.result_type, ExpressionType::Known(_)))
            .filter(|fact| is_range_within_range(fact.span.range, range))
            .collect()
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

    /// Minimal analysis with no tree walking - used when heavy analysis is disabled
    pub fn empty() -> Self {
        Analysis {
            scopes: vec![Scope {
                range: LspRange::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                symbols: HashMap::new(),
                parent_idx: None,
            }],
            diagnostics: Vec::new(),
            registry: SemanticRegistry::default(),
        }
    }

    pub fn new_with_builtins(tree: &Tree, text: &str, builtins: Option<&BuiltinData>) -> Self {
        let mut analysis = Analysis {
            scopes: vec![Scope {
                range: LspRange::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                symbols: HashMap::new(),
                parent_idx: None,
            }],
            diagnostics: Vec::new(),
            registry: SemanticRegistry {
                scopes: vec![ScopeInfo {
                    range: LspRange::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
                    parent_idx: None,
                    introducer: None,
                }],
                ..Default::default()
            },
        };
        analysis.collect_diagnostics(tree.root_node(), text);
        analysis.build_scopes(M2Node::new(tree.root_node()), text, 0, builtins);
        analysis.collect_expression_facts(M2Node::new(tree.root_node()), text, builtins);
        analysis.collect_unused_binding_diagnostics(tree.root_node(), text);
        analysis
    }

    fn build_scopes(
        &mut self,
        node: M2Node,
        text: &str,
        current_scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) {
        let mut next_scope_idx = current_scope_idx;

        match node.kind {
            NodeKind::LambdaExpression => {
                next_scope_idx = self.push_scope(node, text, Some(current_scope_idx));

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
                        Some(right) if right.kind == NodeKind::LambdaExpression => {
                            SymbolKind::FUNCTION
                        }
                        Some(right) if method_declaration_typical_value(right, text).is_some() => {
                            SymbolKind::FUNCTION
                        }
                        _ => SymbolKind::VARIABLE,
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
                            right,
                            text,
                            DefinitionScope::Local,
                            SymbolRegistration {
                                kind: symbol_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.as_deref(),
                                node: left,
                                value_node: right,
                                scope_idx: current_scope_idx,
                            },
                        ),
                        "=" if current_scope_idx == 0 => self.collect_definitions(
                            left,
                            right,
                            text,
                            DefinitionScope::Global,
                            SymbolRegistration {
                                kind: symbol_kind,
                                role: BindingRole::Ordinary,
                                type_name: type_name.as_deref(),
                                node: left,
                                value_node: right,
                                scope_idx: current_scope_idx,
                            },
                        ),
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        // Recurse into children
        for child in node.children() {
            self.build_scopes(child, text, next_scope_idx, builtins);
        }
    }

    fn collect_parameters(
        &mut self,
        node: M2Node,
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
                SymbolRegistration {
                    kind: SymbolKind::VARIABLE,
                    role: BindingRole::Parameter,
                    type_name,
                    node: parameter_node,
                    value_node: None,
                    scope_idx,
                },
                text,
            );
        }
    }

    fn collect_definitions(
        &mut self,
        node: M2Node,
        value_node: Option<M2Node>,
        text: &str,
        definition_scope: DefinitionScope,
        registration: SymbolRegistration<'_>,
    ) {
        match node.kind {
            NodeKind::Symbol => {
                let name = &text[node.start_byte()..node.end_byte()];
                match definition_scope {
                    DefinitionScope::Local => self.add_symbol(
                        name,
                        SymbolRegistration {
                            node,
                            value_node,
                            ..registration
                        },
                        text,
                    ),
                    DefinitionScope::Global => {
                        if !self.is_defined_in_chain(name, registration.scope_idx) {
                            self.add_symbol(
                                name,
                                SymbolRegistration {
                                    node,
                                    value_node,
                                    scope_idx: 0,
                                    ..registration
                                },
                                text,
                            );
                        }
                    }
                }
            }
            NodeKind::Sequence | NodeKind::List => {
                for child in node.named_children() {
                    if child.kind == NodeKind::Symbol {
                        self.collect_definitions(
                            child,
                            value_node,
                            text,
                            definition_scope,
                            SymbolRegistration {
                                type_name: None,
                                ..registration
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn add_symbol(&mut self, name: &str, registration: SymbolRegistration<'_>, text: &str) {
        let SymbolRegistration {
            kind,
            role,
            type_name,
            node,
            value_node,
            scope_idx,
        } = registration;
        let symbol = SymbolInfo {
            kind,
            role,
            range: to_lsp_range(text, node.range()),
            type_name: type_name.map(ToString::to_string),
        };
        let symbol_id = self.registry.intern_symbol(name);
        self.scopes[scope_idx]
            .symbols
            .entry(name.to_string())
            .or_default()
            .push(symbol);
        let binding = BindingInfo {
            symbol: symbol_id,
            kind,
            role,
            range: to_lsp_range(text, node.range()),
            scope_idx,
            type_name: type_name.map(ToString::to_string),
            value_range: value_node.map(|value| to_lsp_range(text, value.range())),
            declaration_range: enclosing_definition_range(node, text),
            span: SpanKey::from_node(text, node),
        };
        let binding_idx = self.registry.bindings.len();
        self.registry.bindings.push(binding);
        self.registry
            .bindings_by_symbol
            .entry(symbol_id)
            .or_default()
            .push(binding_idx);
    }

    pub fn local_method_installation_signature_at<'a>(
        &'a self,
        node: M2Node,
        text: &str,
    ) -> Option<(&'a FunctionInfo, &'a MethodInfo)> {
        let installation = method_installation_expression_for_callable_node(node, text)?;
        let (name, domain) = method_installation_signature(installation, text)?;
        let symbol = self.registry.resolve_symbol(&name)?;
        let method = self.registry.functions.get(&symbol)?;
        let installation_range = to_lsp_range(text, installation.range());
        let signature = method
            .methods
            .iter()
            .rev()
            .find(|signature| signature.domain == domain && signature.range == installation_range)
            .or_else(|| {
                method
                    .methods
                    .iter()
                    .rev()
                    .find(|signature| signature.domain == domain)
            })?;

        Some((method, signature))
    }

    pub fn infer_call_static_facts(
        &self,
        node: M2Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) -> CallStaticFacts {
        let scope_idx = self.find_scope_at(node_position(text, node)).unwrap_or(0);
        self.infer_call_facts(node, text, scope_idx, builtins)
    }

    pub fn infer_expression_static_type_name(
        &self,
        node: M2Node,
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
        node: M2Node,
        text: &str,
    ) {
        let range = to_lsp_range(text, node.range());
        let symbol = self.registry.intern_symbol(name);
        let method = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
                range,
                typical_value: None,
                methods: Vec::new(),
            });
        method.range = range;
        method.typical_value = typical_value;
    }

    fn collect_local_method_installation(
        &mut self,
        node: M2Node,
        right: Option<M2Node>,
        text: &str,
    ) {
        let Some((name, domain)) = method_installation_signature(node, text) else {
            return;
        };
        let range = to_lsp_range(text, node.range());
        let symbol = self.registry.intern_symbol(&name);
        let method = self
            .registry
            .functions
            .entry(symbol)
            .or_insert_with(|| FunctionInfo {
                symbol,
                range,
                typical_value: None,
                methods: Vec::new(),
            });
        let codomain = right
            .and_then(|right| explicit_method_installation_codomain(right, text))
            .or_else(|| method.typical_value.clone());
        method.methods.push(MethodInfo {
            domain: domain.clone(),
            codomain: codomain.clone(),
            range,
        });
    }

    fn push_scope(&mut self, node: M2Node, text: &str, parent_idx: Option<usize>) -> usize {
        let range = to_lsp_range(text, node.range());
        let new_scope = Scope {
            range,
            symbols: HashMap::new(),
            parent_idx,
        };
        self.scopes.push(new_scope);
        let scope_idx = self.scopes.len() - 1;
        self.registry.scopes.push(ScopeInfo {
            range,
            parent_idx,
            introducer: Some(SpanKey::from_node(text, node)),
        });
        self.registry
            .node_scopes
            .insert(SpanKey::from_node(text, node), scope_idx);
        scope_idx
    }

    fn lookup_symbol_at(&self, name: &str, pos: Position) -> Option<&SymbolInfo> {
        let binding = self.get_binding_at(name, pos)?;
        self.scopes[binding.scope_idx]
            .symbols
            .get(name)?
            .iter()
            .find(|symbol| symbol.range == binding.range)
    }

    fn collect_expression_facts(
        &mut self,
        node: M2Node,
        text: &str,
        builtins: Option<&BuiltinData>,
    ) {
        let position = node_position(text, node);
        let scope_idx = self.find_scope_at(position).unwrap_or(0);
        let key = SpanKey::from_node(text, node);
        self.registry.node_scopes.insert(key.clone(), scope_idx);

        if let Some(kind) = expression_kind(node, text) {
            let result_type = self
                .infer_static_type_name(node, text, scope_idx, builtins)
                .map(ExpressionType::Known)
                .unwrap_or(ExpressionType::Unknown);
            let input_nodes = expression_inputs(node)
                .into_iter()
                .map(|child| SpanKey::from_node(text, child))
                .collect();
            let operator = expression_operator_text(node, text).map(ToString::to_string);
            self.registry.expressions.insert(
                key.clone(),
                ExpressionFact {
                    span: key.clone(),
                    kind,
                    input_nodes,
                    operator: operator.clone(),
                    result_type: result_type.clone(),
                    scope_idx,
                },
            );

            if let Some(call_info) =
                self.call_info_for_expression(node, text, scope_idx, builtins, &key, result_type)
            {
                self.registry.calls.insert(key.clone(), call_info);
            }
        }

        for child in node.children() {
            self.collect_expression_facts(child, text, builtins);
        }
    }

    fn call_info_for_expression(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
        key: &SpanKey,
        result_type: ExpressionType,
    ) -> Option<CallInfo> {
        if !matches!(
            node.kind,
            NodeKind::BinaryExpression | NodeKind::PrefixExpression
        ) {
            return None;
        }

        if is_assignment_expression(node, text) || is_option_assignment_expression(node, text) {
            return None;
        }

        if is_space_operator_expression(node) {
            let callable = node.child_by_field_name("left")?;
            let argument = node.child_by_field_name("right")?;
            let callable_name = symbol_node_text(callable, text).map(ToString::to_string);
            let facts = self.infer_call_facts(argument, text, scope_idx, builtins);
            let candidate_methods = callable_name
                .as_deref()
                .and_then(|name| self.registry.resolve_symbol(name))
                .and_then(|symbol| self.registry.functions.get(&symbol))
                .map(|callable| {
                    callable
                        .methods
                        .iter()
                        .filter(|signature| {
                            signature_matches_domain(
                                &signature.domain,
                                &facts.argument_types,
                                builtins,
                            )
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            return Some(CallInfo {
                span: key.clone(),
                callable_name,
                argument_types: facts.argument_types,
                result_type,
                candidate_methods,
            });
        }

        let operator = expression_operator_text(node, text)?;
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");
        let operand = node.child_by_field_name("operand");
        let argument_types = if let Some(operand) = operand {
            vec![self.infer_static_type_name(operand, text, scope_idx, builtins)]
        } else {
            vec![
                left.and_then(|child| {
                    self.infer_static_type_name(child, text, scope_idx, builtins)
                }),
                right.and_then(|child| {
                    self.infer_static_type_name(child, text, scope_idx, builtins)
                }),
            ]
        };

        Some(CallInfo {
            span: key.clone(),
            callable_name: Some(operator.to_string()),
            argument_types,
            result_type,
            candidate_methods: Vec::new(),
        })
    }

    fn infer_static_type_name(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        match node.kind {
            NodeKind::LambdaExpression => Some("Function".to_string()),
            NodeKind::BinaryExpression
                if method_declaration_typical_value(node, text).is_some() =>
            {
                Some("MethodFunction".to_string())
            }
            NodeKind::List => Some("List".to_string()),
            NodeKind::Array => Some("Array".to_string()),
            NodeKind::AngleBarList => Some("AngleBarList".to_string()),
            NodeKind::Sequence => {
                self.infer_sequence_static_type_name(node, text, scope_idx, builtins)
            }
            NodeKind::StringLiteral => Some("String".to_string()),
            NodeKind::IntegerLiteral => Some("ZZ".to_string()),
            NodeKind::FloatLiteral => Some("RR".to_string()),
            NodeKind::Symbol | NodeKind::ResolvedSymbol => {
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
            NodeKind::BinaryExpression => {
                if is_space_operator_expression(node) {
                    let callable = node.child_by_field_name("left")?;
                    let argument = node.child_by_field_name("right")?;
                    let call_facts = self.infer_call_facts(argument, text, scope_idx, builtins);
                    if let Some(callable) = symbol_node_text(callable, text) {
                        if let Some(return_type) = self.resolve_local_call_return_type(
                            callable,
                            &call_facts.argument_types,
                            builtins,
                        ) {
                            return Some(return_type);
                        }
                        if let Some(return_type) = builtins.and_then(|builtins| {
                            builtins.resolve_call_return_type_with_options(
                                callable,
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
            NodeKind::NewStatement => node
                .child_by_field_name("type")
                .filter(|type_node| type_node.kind == NodeKind::Symbol)
                .map(|type_node| text[type_node.start_byte()..type_node.end_byte()].to_string()),
            _ => None,
        }
    }

    fn infer_call_facts(
        &self,
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> CallStaticFacts {
        if node.kind == NodeKind::Sequence {
            let mut facts = CallStaticFacts::default();
            for child in node.named_children() {
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
        node: M2Node,
        text: &str,
        scope_idx: usize,
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        let children = node.named_children().collect::<Vec<_>>();
        match children.as_slice() {
            [child] => self.infer_static_type_name(*child, text, scope_idx, builtins),
            _ => Some("Sequence".to_string()),
        }
    }

    fn resolve_local_call_return_type(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        builtins: Option<&BuiltinData>,
    ) -> Option<String> {
        let symbol = self.registry.resolve_symbol(callable)?;
        let method = self.registry.functions.get(&symbol)?;
        let matching_codomains = method
            .methods
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

    pub(crate) fn binding_idx_at(&self, name: &str, pos: Position) -> Option<usize> {
        let scope_idx = self.find_scope_at(pos)?;
        let symbol = self.registry.resolve_symbol(name)?;
        let mut curr = Some(scope_idx);
        while let Some(idx) = curr {
            let binding = self
                .registry
                .bindings_by_symbol
                .get(&symbol)
                .into_iter()
                .flatten()
                .filter_map(|binding_idx| {
                    self.registry
                        .bindings
                        .get(*binding_idx)
                        .map(|binding| (*binding_idx, binding))
                })
                .filter(|(_, binding)| binding.scope_idx == idx && binding.range.start <= pos)
                .max_by_key(|(_, binding)| {
                    (binding.range.start.line, binding.range.start.character)
                });
            if let Some((binding_idx, _)) = binding {
                return Some(binding_idx);
            }
            curr = self.scopes[idx].parent_idx;
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum DefinitionScope {
    Local,
    Global,
}

#[derive(Debug, Clone, Copy)]
struct SymbolRegistration<'a> {
    kind: SymbolKind,
    role: BindingRole,
    type_name: Option<&'a str>,
    node: M2Node<'a>,
    value_node: Option<M2Node<'a>>,
    scope_idx: usize,
}

fn expression_kind(node: M2Node<'_>, text: &str) -> Option<ExpressionKind> {
    match node.kind {
        NodeKind::StringLiteral | NodeKind::IntegerLiteral | NodeKind::FloatLiteral => {
            Some(ExpressionKind::Literal)
        }
        NodeKind::Symbol | NodeKind::ResolvedSymbol => Some(ExpressionKind::Name),
        NodeKind::List
        | NodeKind::Array
        | NodeKind::AngleBarList
        | NodeKind::Sequence
        | NodeKind::Cell => Some(ExpressionKind::ScopeExpr),
        NodeKind::IfStatement
        | NodeKind::WhileStatement
        | NodeKind::ForStatement
        | NodeKind::NewStatement => Some(ExpressionKind::ControlExpr),
        NodeKind::LambdaExpression | NodeKind::BinaryExpression | NodeKind::PrefixExpression => {
            if is_assignment_expression(node, text) {
                Some(ExpressionKind::Assign)
            } else {
                Some(ExpressionKind::Expr)
            }
        }
        _ => None,
    }
}

fn expression_inputs(node: M2Node<'_>) -> Vec<M2Node<'_>> {
    [
        "left",
        "right",
        "operand",
        "condition",
        "body",
        "parameters",
    ]
    .into_iter()
    .filter_map(|field| node.child_by_field_name(field))
    .collect()
}

fn expression_operator_text<'a>(node: M2Node<'_>, text: &'a str) -> Option<&'a str> {
    node.child_by_field_name("operator")
        .map(|operator| &text[operator.start_byte()..operator.end_byte()])
}

fn collect_parameter_nodes<'tree>(node: M2Node<'tree>, parameters: &mut Vec<M2Node<'tree>>) {
    match node.kind {
        NodeKind::Symbol => parameters.push(node),
        NodeKind::Sequence | NodeKind::List => {
            for child in node.children() {
                collect_parameter_nodes(child, parameters);
            }
        }
        _ => {}
    }
}

fn single_symbol_assignment_target<'a>(node: M2Node, text: &'a str) -> Option<&'a str> {
    (node.kind == NodeKind::Symbol).then(|| &text[node.start_byte()..node.end_byte()])
}

pub(crate) fn binary_expression_operator<'a>(node: M2Node, text: &'a str) -> Option<&'a str> {
    if node.kind != NodeKind::BinaryExpression {
        return None;
    }

    node.child_by_field_name("operator")
        .map(|operator| &text[operator.start_byte()..operator.end_byte()])
}

pub(crate) fn is_space_operator_expression(node: M2Node<'_>) -> bool {
    node.kind == NodeKind::BinaryExpression
        && binary_expression_operator_kind(node.inner()) == Some("SPACE")
}

pub(crate) fn is_assignment_expression(node: M2Node<'_>, text: &str) -> bool {
    node.kind == NodeKind::BinaryExpression
        && matches!(
            binary_expression_operator(node, text),
            Some("=" | ":=" | "<-")
        )
}

pub(crate) fn is_option_assignment_expression(node: M2Node<'_>, text: &str) -> bool {
    node.kind == NodeKind::BinaryExpression && binary_expression_operator(node, text) == Some("=>")
}

pub(crate) fn symbol_node_text<'a>(node: M2Node, text: &'a str) -> Option<&'a str> {
    node.kind
        .is_symbol_like()
        .then(|| &text[node.start_byte()..node.end_byte()])
}

fn method_declaration_typical_value(node: M2Node, text: &str) -> Option<Option<String>> {
    if !is_space_operator_expression(node) {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if symbol_node_text(left, text) != Some("method") {
        return None;
    }

    Some(find_option_value(node, text, "TypicalValue"))
}

fn find_option_value(node: M2Node, text: &str, option_name: &str) -> Option<String> {
    if is_option_assignment_expression(node, text) {
        let left = node.child_by_field_name("left")?;
        let right = node.child_by_field_name("right")?;
        if symbol_node_text(left, text) == Some(option_name) {
            return symbol_node_text(right, text).map(ToString::to_string);
        }
    }

    for child in node.named_children() {
        if let Some(value) = find_option_value(child, text, option_name) {
            return Some(value);
        }
    }
    None
}

fn literal_option_assignment(node: M2Node, text: &str) -> Option<(String, String)> {
    if !is_option_assignment_expression(node, text) {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    let key = symbol_node_text(left, text)?;
    let value = literal_option_value(right, text)?;
    Some((key.to_string(), value.to_string()))
}

fn enclosing_definition_range(node: M2Node<'_>, text: &str) -> LspRange {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind == NodeKind::Cell {
            return to_lsp_range(text, parent.range());
        }
        current = parent;
    }
    to_lsp_range(text, node.range())
}

fn literal_option_value<'a>(node: M2Node, text: &'a str) -> Option<&'a str> {
    if node.kind.is_symbol_like() || node.kind.is_literal() {
        Some(&text[node.start_byte()..node.end_byte()])
    } else {
        None
    }
}

fn explicit_method_installation_codomain(node: M2Node, text: &str) -> Option<String> {
    if !is_option_assignment_expression(node, text) {
        return None;
    }

    let codomain = node.child_by_field_name("left")?;
    symbol_node_text(codomain, text).map(ToString::to_string)
}

pub(crate) fn method_installation_signature(
    node: M2Node,
    text: &str,
) -> Option<(String, Vec<String>)> {
    if !is_space_operator_expression(node) {
        return None;
    }

    let callable = node.child_by_field_name("left")?;
    let arguments = node.child_by_field_name("right")?;
    let callable = symbol_node_text(callable, text)?;
    let domain = method_installation_domain(arguments, text)?;
    Some((callable.to_string(), domain))
}

fn method_installation_parameter_types_for_function(
    function_node: M2Node,
    text: &str,
) -> Option<Vec<String>> {
    let mut current = function_node;
    while let Some(parent) = current.parent() {
        if parent.kind == NodeKind::LambdaExpression {
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
    node: M2Node<'tree>,
    text: &str,
) -> Option<M2Node<'tree>> {
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

pub(crate) fn method_installation_domain(node: M2Node, text: &str) -> Option<Vec<String>> {
    if matches!(node.kind, NodeKind::Sequence | NodeKind::List) {
        let domain = node
            .named_children()
            .filter_map(|child| symbol_node_text(child, text).map(ToString::to_string))
            .collect::<Vec<_>>();
        return (!domain.is_empty()).then_some(domain);
    }

    symbol_node_text(node, text).map(|name| vec![name.to_string()])
}

fn node_is_within(ancestor: M2Node, node: M2Node) -> bool {
    ancestor.start_byte() <= node.start_byte() && node.end_byte() <= ancestor.end_byte()
}

fn is_colon_equal_assignment_left(node: M2Node, text: &str) -> bool {
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

fn signature_matches(
    signature: &MethodInfo,
    argument_types: &[Option<String>],
    builtins: Option<&BuiltinData>,
) -> bool {
    signature_matches_domain(&signature.domain, argument_types, builtins)
}

fn signature_matches_domain(
    expected_domain: &[String],
    argument_types: &[Option<String>],
    builtins: Option<&BuiltinData>,
) -> bool {
    expected_domain.len() == argument_types.len()
        && expected_domain
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

pub(crate) fn to_lsp_range(text: &str, range: tree_sitter::Range) -> LspRange {
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

pub(crate) fn node_position(text: &str, node: M2Node) -> Position {
    to_lsp_range(text, node.range()).start
}

pub(crate) fn floor_char_boundary(text: &str, byte_index: usize) -> usize {
    let mut byte_index = byte_index.min(text.len());
    while byte_index > 0 && !text.is_char_boundary(byte_index) {
        byte_index -= 1;
    }
    byte_index
}

pub(crate) fn utf16_len_for_byte_span(text: &str, start_byte: usize, end_byte: usize) -> u32 {
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

fn is_range_within_range(inner: LspRange, outer: LspRange) -> bool {
    let starts_inside = inner.start.line > outer.start.line
        || (inner.start.line == outer.start.line && inner.start.character >= outer.start.character);
    let ends_inside = inner.end.line < outer.end.line
        || (inner.end.line == outer.end.line && inner.end.character <= outer.end.character);
    starts_inside && ends_inside
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
    use crate::capabilities::diagnostics::{
        member_index_for_ambiguous_float_literal, AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE,
        UNUSED_BINDING_DIAGNOSTIC_CODE,
    };
    use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};
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
            Some(SymbolKind::FUNCTION)
        );
        assert_eq!(
            analysis
                .get_symbol_at("x", Position::new(0, 10))
                .map(|symbol| symbol.role),
            Some(BindingRole::Parameter)
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
    fn infers_static_types_from_new_constructors() {
        let builtins = BuiltinData::load_from_index(
            include_str!("./data/m2-types.jsonl"),
            include_str!("./data/m2-docs.jsonl"),
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
        let builtins = BuiltinData::load_from_index(
            include_str!("./data/m2-types.jsonl"),
            include_str!("./data/m2-docs.jsonl"),
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
            "{\"name\":\"f\",\"class\":\"MethodFunction\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"Ideal\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"Ideal\"],\"output_types\":[\"Ring\"]}],\"general_signature\":{\"signature\":[\"f\"],\"output_types\":[\"Thing\"]}}}\n",
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
            "{\"name\":\"+\",\"class\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"],\"output_types\":[\"ZZ\"]}]}}\n",
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
            .function("p")
            .expect("method declaration should create local method metadata");

        assert_eq!(method.typical_value.as_deref(), Some("List"));
        assert_eq!(
            method
                .methods
                .iter()
                .map(|signature| signature.domain.clone())
                .collect::<Vec<_>>(),
            vec![
                vec!["ZZ".to_string(), "ZZ".to_string()],
                vec!["List".to_string(), "ZZ".to_string()]
            ]
        );
        assert!(method
            .methods
            .iter()
            .all(|signature| signature.codomain.as_deref() == Some("List")));
        assert_eq!(
            analysis
                .get_symbol_at("p", Position::new(1, 0))
                .map(|symbol| symbol.kind),
            Some(SymbolKind::FUNCTION)
        );
    }

    #[test]
    fn infers_static_types_from_local_method_typical_values() {
        let builtins = BuiltinData::load_from_index(
            include_str!("./data/m2-types.jsonl"),
            include_str!("./data/m2-docs.jsonl"),
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
        let builtins = BuiltinData::load_from_index(
            include_str!("./data/m2-types.jsonl"),
            include_str!("./data/m2-docs.jsonl"),
        );
        let analysis =
            analyze_with_builtins("f = method()\nf ZZ := x -> -x\ny := f 1\ny\n", &builtins);

        let method = analysis
            .function("f")
            .expect("method declaration should be tracked");
        assert_eq!(method.typical_value, None);
        assert_eq!(method.methods[0].domain, vec!["ZZ"]);
        assert_eq!(
            analysis
                .get_symbol_at("y", Position::new(3, 0))
                .and_then(|symbol| symbol.type_name.as_deref()),
            None
        );
    }

    #[test]
    fn explicit_local_method_codomains_override_typical_values() {
        let builtins = BuiltinData::load_from_index(
            include_str!("./data/m2-types.jsonl"),
            include_str!("./data/m2-docs.jsonl"),
        );
        let analysis = analyze_with_builtins(
            "f = method(TypicalValue => List)\nf ZZ := Ring => x -> x\ny := f 1\ny\n",
            &builtins,
        );

        let method = analysis
            .function("f")
            .expect("local method should be tracked");
        assert_eq!(method.typical_value.as_deref(), Some("List"));
        assert_eq!(method.methods[0].codomain.as_deref(), Some("Ring"));
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
            "{\"name\":\"f\",\"class\":\"MethodFunctionWithOptions\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"ZZ\"],\"output_types\":[\"String\"]}]}}\n",
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
            "{\"name\":\"f\",\"class\":\"MethodFunctionWithOptions\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"ZZ\"],\"output_types\":[\"String\"]}]}}\n",
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
            "{\"name\":\"QQ\",\"class\":\"Ring\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{}}\n{\"name\":\"SPACE\",\"class\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"SPACE\",\"Ring\",\"Array\"]}]}}\n",
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
    fn registry_tracks_bindings_and_local_callables() {
        let analysis =
            analyze("f = method(TypicalValue => List)\nf ZZ := Ring => x -> x\ny := f 1\ny\n");

        let binding = analysis
            .get_binding_at("y", Position::new(3, 0))
            .expect("binding should resolve through registry");
        assert_eq!(binding.scope_idx, 0);
        assert_eq!(binding.type_name.as_deref(), Some("Ring"));

        let callable = analysis
            .function("f")
            .expect("callable should be registered");
        assert_eq!(callable.typical_value.as_deref(), Some("List"));
        assert_eq!(callable.methods.len(), 1);
        assert_eq!(callable.methods[0].domain, vec!["ZZ"]);
        assert_eq!(callable.methods[0].codomain.as_deref(), Some("Ring"));
    }

    #[test]
    fn registry_tracks_expression_and_call_facts() {
        let builtins = BuiltinData::load_from_split(
            "+\n",
            "{\"name\":\"+\",\"class\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"],\"output_types\":[\"ZZ\"]}]}}\n",
        );
        let text = "x := 1\ny := 2\nz := x + y\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(&builtins));
        let assignment = tree
            .root_node()
            .descendant_for_byte_range(18, 23)
            .expect("assignment should exist");
        let binary = M2Node::new(
            assignment
                .child_by_field_name("right")
                .expect("assignment should have right-hand expression"),
        );
        let fact = analysis
            .expression_fact(text, binary)
            .expect("expression fact should be registered");
        assert_eq!(fact.kind, ExpressionKind::Expr);
        assert_eq!(fact.result_type, ExpressionType::Known("ZZ".to_string()));
        let call = analysis
            .registry()
            .calls
            .get(&SpanKey::from_node(text, binary))
            .expect("call info should be registered");
        assert_eq!(call.callable_name.as_deref(), Some("+"));
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
                .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
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
            ORPHAN_ELSE_DIAGNOSTIC_MESSAGE
        );
        assert_eq!(analysis.diagnostics[0].range.start, Position::new(1, 4));
        assert_eq!(analysis.diagnostics[0].range.end, Position::new(1, 8));
    }

    #[test]
    fn diagnoses_orphan_else_on_new_line_in_example_shape() {
        let analysis = analyze(
            "if runtimeDict#?name then runtimeDict#name\nelse if isGlobalSmbol name then getGlobalSymbol name\n",
        );
        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].message,
            ORPHAN_ELSE_DIAGNOSTIC_MESSAGE
        );
        assert_eq!(analysis.diagnostics[0].range.start, Position::new(1, 0));
    }

    #[test]
    fn does_not_warn_on_unused_top_level_exports() {
        let analysis = analyze("f := x -> x\nx = 1\n");

        assert!(
            analysis.diagnostics.iter().all(|diagnostic| {
                diagnostic.code
                    != Some(NumberOrString::String(
                        UNUSED_BINDING_DIAGNOSTIC_CODE.to_string(),
                    ))
            }),
            "top-level bindings should not be warned as unused exports"
        );
    }

    #[test]
    fn diagnoses_ambiguous_float_member_access() {
        let analysis = analyze("x.3\n");
        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].message,
            AMBIGUOUS_FLOAT_MEMBER_ACCESS_DIAGNOSTIC_MESSAGE
        );
        assert_eq!(
            analysis.diagnostics[0].severity,
            Some(DiagnosticSeverity::WARNING)
        );
        assert_eq!(analysis.diagnostics[0].range.start, Position::new(0, 0));
    }

    #[test]
    fn does_not_diagnose_ambiguous_member_access_with_whitespace() {
        let analysis = analyze("x .3\n");
        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn ambiguous_member_access_helper_requires_dot_prefixed_float() {
        assert_eq!(
            member_index_for_ambiguous_float_literal(".3"),
            Some("0".to_string())
        );
        assert_eq!(member_index_for_ambiguous_float_literal("3.0"), None);
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
