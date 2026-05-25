use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use dashmap::DashMap;
use serde_json::{json, Value};
use tower::Service;
use tower_lsp::jsonrpc::{Request, Response, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tree_sitter::Parser;
use typesystem::{BuiltinData, M2SemanticTokenType};

mod analysis;
mod formatting;
mod package_index;
mod record_lsp;
mod typesystem;

use analysis::{Analysis, SymbolInfo, SymbolKind};
use formatting::{format_document_text_with_options, FormatOptions};
#[cfg(test)]
use package_index::extractor_script_candidates;
use package_index::{
    collect_imported_packages, package_source_string, PackageIndexer, SourceResolver,
};
#[cfg(test)]
use record_lsp::record_package;
use record_lsp::{
    record_hover_with_package, record_source_file, record_source_line, record_symbol_kind,
};

const TYPE_HIERARCHY_METHOD: &str = "textDocument/prepareTypeHierarchy";

#[derive(Debug)]
struct TypeHierarchyCapabilityService<S> {
    inner: S,
}

impl<S> TypeHierarchyCapabilityService<S> {
    fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Service<Request> for TypeHierarchyCapabilityService<S>
where
    S: Service<Request, Response = Option<Response>> + Send + 'static,
    S::Error: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let should_advertise_type_hierarchy = req.method() == "initialize"
            && !request_type_hierarchy_dynamic_registration(req.params());
        let fut = self.inner.call(req);

        Box::pin(async move {
            let response = fut.await?;
            if should_advertise_type_hierarchy {
                Ok(response.map(advertise_type_hierarchy_capability))
            } else {
                Ok(response)
            }
        })
    }
}

fn request_type_hierarchy_dynamic_registration(params: Option<&Value>) -> bool {
    params
        .and_then(|params| params.get("capabilities"))
        .and_then(|capabilities| capabilities.get("textDocument"))
        .and_then(|text_document| text_document.get("typeHierarchy"))
        .and_then(|type_hierarchy| type_hierarchy.get("dynamicRegistration"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn advertise_type_hierarchy_capability(response: Response) -> Response {
    if !response.is_ok() {
        return response;
    }

    let (id, body) = response.into_parts();
    Response::from_parts(
        id,
        body.map(|mut result| {
            if let Some(capabilities) = result
                .get_mut("capabilities")
                .and_then(Value::as_object_mut)
            {
                capabilities
                    .entry("typeHierarchyProvider")
                    .or_insert_with(|| json!(true));
            }
            result
        }),
    )
}

#[derive(Debug)]
struct Backend {
    client: Client,
    builtins: BuiltinData,
    source_resolver: SourceResolver,
    package_indexer: PackageIndexer,
    package_indexes: DashMap<String, BuiltinData>,
    documents: DashMap<Url, String>,
    analyses: DashMap<Url, Analysis>,
    semantic_tokens_augment_syntax: AtomicBool,
    type_hierarchy_dynamic_registration: AtomicBool,
}

const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::TYPE,        // 0
    SemanticTokenType::FUNCTION,    // 1
    SemanticTokenType::VARIABLE,    // 2
    SemanticTokenType::PARAMETER,   // 3
    SemanticTokenType::PROPERTY,    // 4
    SemanticTokenType::NAMESPACE,   // 5
    SemanticTokenType::ENUM_MEMBER, // 6
    SemanticTokenType::CLASS,       // 7
    SemanticTokenType::KEYWORD,     // 8
    SemanticTokenType::STRING,      // 9
    SemanticTokenType::NUMBER,      // 10
    SemanticTokenType::OPERATOR,    // 11
    SemanticTokenType::COMMENT,     // 12
    SemanticTokenType::METHOD,      // 13
    SemanticTokenType::REGEXP,      // 14
    SemanticTokenType::MODIFIER,    // 15
];

const OPTION_MODIFIER: u32 = 1 << 0;
const COMMAND_MODIFIER: u32 = 1 << 1;
const FILE_MODIFIER: u32 = 1 << 2;
const MANIPULATOR_MODIFIER: u32 = 1 << 3;
const DECLARATION_MODIFIER: u32 = 1 << 4;
const CONSTRUCTOR_MODIFIER: u32 = 1 << 5;

fn utf16_col_to_byte(line: &str, utf16_col: u32) -> usize {
    let mut current_col = 0;

    for (byte_index, ch) in line.char_indices() {
        let next_col = current_col + ch.len_utf16() as u32;
        if next_col > utf16_col {
            return byte_index;
        }
        current_col = next_col;
    }

    line.len()
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

fn tree_sitter_point_from_lsp_position(
    text: &str,
    position: Position,
) -> Option<tree_sitter::Point> {
    let line = text.lines().nth(position.line as usize)?;
    let byte_col = utf16_col_to_byte(line, position.character);
    Some(tree_sitter::Point::new(position.line as usize, byte_col))
}

fn symbol_prefix_at(text: &str, position: Position) -> Option<String> {
    let line = text.lines().nth(position.line as usize)?;
    let cursor = utf16_col_to_byte(line, position.character);
    let start = line[..cursor]
        .char_indices()
        .rev()
        .find_map(|(index, ch)| {
            if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                None
            } else {
                Some(index + ch.len_utf8())
            }
        })
        .unwrap_or(0);
    let prefix = &line[start..cursor];
    (!prefix.is_empty()).then(|| prefix.to_string())
}

fn option_assignment_role(
    text: &str,
    node: tree_sitter::Node,
    builtins: &BuiltinData,
) -> Option<M2SemanticTokenType> {
    let parent = node.parent()?;
    if parent.kind() != "option_assignment" {
        return None;
    }

    let node_text = &text[node.start_byte()..node.end_byte()];
    if parent
        .child_by_field_name("left")
        .is_some_and(|left| left.id() == node.id())
        && builtins.is_option_name(node_text)
    {
        return Some(M2SemanticTokenType::EnumMember);
    }

    if parent
        .child_by_field_name("right")
        .is_some_and(|right| right.id() == node.id())
        && builtins.is_option_value_name(node_text)
    {
        return Some(M2SemanticTokenType::EnumMember);
    }

    None
}

fn local_symbol_hover(name: &str, symbol: &SymbolInfo) -> Hover {
    let label = match symbol.kind {
        SymbolKind::Function => "User-defined function",
        SymbolKind::Variable => "User-defined variable",
        SymbolKind::Parameter => "Function parameter",
    };
    let line = symbol.range.start.line + 1;
    let character = symbol.range.start.character + 1;
    let type_line = symbol
        .type_name
        .as_ref()
        .map(|type_name| format!("\n\nType: `{type_name}`"))
        .unwrap_or_default();
    let markdown = format!(
        "**{}**\n\n{}{}\n\nDefined at `{line}:{character}`",
        name, label, type_line
    );

    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: None,
    }
}

fn local_symbol_semantic_token_type(
    symbol: &SymbolInfo,
    _position: Position,
    builtins: &BuiltinData,
) -> M2SemanticTokenType {
    if let Some(type_name) = &symbol.type_name {
        if let Some(token) = builtins.get_semantic_token_for_static_type(type_name) {
            return token.token_type;
        }
    }

    match symbol.kind {
        SymbolKind::Function => M2SemanticTokenType::Function,
        SymbolKind::Variable => M2SemanticTokenType::Variable,
        SymbolKind::Parameter => M2SemanticTokenType::Parameter,
    }
}

fn is_keyword_node_kind(kind: &str) -> bool {
    matches!(
        kind,
        "if" | "then"
            | "else"
            | "from"
            | "to"
            | "when"
            | "do"
            | "in"
            | "of"
            | "list"
            | "for"
            | "while"
            | "break"
            | "continue"
            | "return"
            | "try"
            | "catch"
            | "throw"
            | "time"
            | "timing"
            | "elapsedTime"
            | "elapsedTiming"
            | "profile"
            | "shield"
            | "TEST"
            | "breakpoint"
            | "new"
    )
}

fn is_modifier_node_kind(kind: &str) -> bool {
    matches!(
        kind,
        "global" | "local" | "symbol" | "threadVariable" | "threadLocal"
    )
}

fn is_operator_node(node: tree_sitter::Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent
        .child_by_field_name("operator")
        .is_some_and(|operator| operator.id() == node.id())
}

fn is_first_named_child(parent: tree_sitter::Node, child: tree_sitter::Node) -> bool {
    parent
        .named_child(0)
        .is_some_and(|first| first.id() == child.id())
}

fn binary_expression_left_symbol<'a>(text: &'a str, node: tree_sitter::Node) -> Option<&'a str> {
    if node.kind() != "binary_expression" {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if left.kind() != "symbol" {
        return None;
    }

    let operator = node.child_by_field_name("operator")?;
    if operator.kind() != "space" {
        return None;
    }

    Some(&text[left.start_byte()..left.end_byte()])
}

fn is_regexp_string_argument(text: &str, node: tree_sitter::Node) -> bool {
    if node.kind() != "string_literal" {
        return false;
    }

    call_like_left_symbol_for_argument(text, node, false)
        .is_some_and(|name| matches!(name, "match" | "regex" | "select" | "replace" | "separate"))
}

fn is_namespace_string_argument(text: &str, node: tree_sitter::Node) -> bool {
    if node.kind() != "string_literal" {
        return false;
    }

    call_like_left_symbol_for_argument(text, node, true).is_some_and(|name| {
        matches!(
            name,
            "loadPackage"
                | "installPackage"
                | "uninstallPackage"
                | "needsPackage"
                | "export"
                | "endPackage"
                | "newPackage"
                | "importFrom"
                | "exportFrom"
        )
    })
}

fn call_like_left_symbol_for_argument<'a>(
    text: &'a str,
    mut node: tree_sitter::Node,
    allow_list_argument: bool,
) -> Option<&'a str> {
    let mut parent = node.parent()?;
    if parent.kind() == "sequence" && !is_first_named_child(parent, node) {
        return None;
    }

    loop {
        if let Some(name) = binary_expression_left_symbol(text, parent) {
            return Some(name);
        }

        if parent.kind() == "list" && !allow_list_argument {
            return None;
        }

        if !matches!(parent.kind(), "sequence" | "list") {
            return None;
        }

        node = parent;
        parent = node.parent()?;
    }
}

fn enclosing_node_of_kind<'a>(
    mut node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    loop {
        if node.kind() == kind {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn syntax_semantic_token_type(text: &str, node: tree_sitter::Node) -> Option<M2SemanticTokenType> {
    if is_operator_node(node) {
        return Some(M2SemanticTokenType::Operator);
    }

    match node.kind() {
        "integer_literal" | "float_literal" => Some(M2SemanticTokenType::Number),
        "string_literal" if is_regexp_string_argument(text, node) => {
            Some(M2SemanticTokenType::Regexp)
        }
        "string_literal" if is_namespace_string_argument(text, node) => {
            Some(M2SemanticTokenType::Namespace)
        }
        "string_literal" => Some(M2SemanticTokenType::String),
        "line_comment" | "block_comment" => Some(M2SemanticTokenType::Comment),
        kind if !node.is_named() && is_modifier_node_kind(kind) => {
            Some(M2SemanticTokenType::Modifier)
        }
        kind if !node.is_named() && is_keyword_node_kind(kind) => {
            Some(M2SemanticTokenType::Keyword)
        }
        _ => None,
    }
}

fn should_emit_syntax_token_when_augmenting(text: &str, node: tree_sitter::Node) -> bool {
    matches!(
        syntax_semantic_token_type(text, node),
        Some(M2SemanticTokenType::Modifier)
            | Some(M2SemanticTokenType::Regexp)
            | Some(M2SemanticTokenType::EnumMember)
            | Some(M2SemanticTokenType::Property)
            | Some(M2SemanticTokenType::Namespace)
    )
}

fn should_emit_builtin_token_when_augmenting(token: &typesystem::M2SemanticToken) -> bool {
    matches!(
        token.token_type,
        M2SemanticTokenType::Function
            | M2SemanticTokenType::Method
            | M2SemanticTokenType::Class
            | M2SemanticTokenType::Type
            | M2SemanticTokenType::Operator
            | M2SemanticTokenType::Namespace
    ) || token.is_command
        || token.is_file
        || token.is_manipulator
        || token.is_constructor
}

fn builtin_semantic_token_modifiers(token: &typesystem::M2SemanticToken) -> u32 {
    let mut modifiers = 0;
    if token.is_command {
        modifiers |= COMMAND_MODIFIER;
    }
    if token.is_file {
        modifiers |= FILE_MODIFIER;
    }
    if token.is_manipulator {
        modifiers |= MANIPULATOR_MODIFIER;
    }
    if token.is_constructor {
        modifiers |= CONSTRUCTOR_MODIFIER;
    }
    modifiers
}

fn workspace_symbol_dedupe_key(package: &str, name: &str) -> String {
    format!("{package}:{name}")
}

fn should_include_workspace_symbol(package: &str, name: &str) -> bool {
    !(package == "Core" && name.starts_with("Core$"))
}

fn collect_semantic_tokens(
    text: &str,
    analysis: Option<&Analysis>,
    builtins: &BuiltinData,
    augments_syntax_tokens: bool,
) -> Vec<SemanticToken> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_macaulay2::language())
        .unwrap();
    let tree = parser.parse(text, None).unwrap();
    let root_node = tree.root_node();

    let mut tokens = Vec::new();
    let mut cursor = root_node.walk();
    let mut prev_line = 0;
    let mut prev_start = 0;
    let mut reached_root = false;

    while !reached_root {
        let node = cursor.node();
        let kind = node.kind();

        let mut emitted_token = false;
        if kind == "symbol"
            || kind == "identifier"
            || kind == "resolved_symbol"
            || kind == "builtin_constant"
            || syntax_semantic_token_type(text, node).is_some()
        {
            let start_byte = node.start_byte();
            let end_byte = node.end_byte();
            let node_text = &text[start_byte..end_byte];
            let start_pos = node.start_position();
            let line_start_byte = start_byte.saturating_sub(start_pos.column);
            let start_char = utf16_len_for_byte_span(text, line_start_byte, start_byte);
            let position = Position::new(start_pos.row as u32, start_char);

            let mut token_type: Option<u32> = None;
            let mut modifiers: u32 = 0;
            let option_role = option_assignment_role(text, node, builtins);

            if let Some(role) = option_role {
                token_type = Some(role as u32);
                modifiers |= OPTION_MODIFIER;
            }

            if token_type.is_none() {
                if let Some(analysis) = analysis {
                    if let Some(symbol) = analysis.get_symbol_at(node_text, position) {
                        token_type = Some(local_symbol_semantic_token_type(
                            symbol, position, builtins,
                        ) as u32);
                        if let Some(type_name) = &symbol.type_name {
                            if let Some(token) =
                                builtins.get_semantic_token_for_static_type(type_name)
                            {
                                modifiers |= builtin_semantic_token_modifiers(&token);
                            }
                        }
                        if symbol.kind == SymbolKind::Parameter && position == symbol.range.start {
                            modifiers |= DECLARATION_MODIFIER;
                        }
                        if symbol.kind == SymbolKind::Function
                            && builtins.is_constructor_name(node_text)
                        {
                            modifiers |= CONSTRUCTOR_MODIFIER;
                        }
                    }
                }
            }

            if token_type.is_none() {
                if let Some(token) = builtins.get_semantic_token(node_text) {
                    if !augments_syntax_tokens || should_emit_builtin_token_when_augmenting(&token)
                    {
                        token_type = Some(token.token_type as u32);
                        modifiers |= builtin_semantic_token_modifiers(&token);
                    }
                }
            }

            if token_type.is_none() {
                if !augments_syntax_tokens || should_emit_syntax_token_when_augmenting(text, node) {
                    token_type =
                        syntax_semantic_token_type(text, node).map(|token_type| token_type as u32);
                }
            }

            if let Some(token_type) = token_type {
                let line = start_pos.row as u32;
                let length = utf16_len_for_byte_span(text, start_byte, end_byte);

                let delta_line = line - prev_line;
                let delta_start = if delta_line == 0 {
                    start_char - prev_start
                } else {
                    start_char
                };

                tokens.push(SemanticToken {
                    delta_line,
                    delta_start,
                    length,
                    token_type,
                    token_modifiers_bitset: modifiers,
                });

                prev_line = line;
                prev_start = start_char;
                emitted_token = true;
            }
        }

        if !emitted_token && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                reached_root = true;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }

    tokens
}

fn node_range(text: &str, node: tree_sitter::Node) -> Range {
    let range = node.range();
    let start_line_byte = range.start_byte.saturating_sub(range.start_point.column);
    let end_line_byte = range.end_byte.saturating_sub(range.end_point.column);

    Range::new(
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

fn full_document_range(text: &str) -> Range {
    let mut lines = text.lines();
    let Some(mut last_line) = lines.next() else {
        return Range::new(Position::new(0, 0), Position::new(0, 0));
    };

    let mut line_count = 1;
    for line in lines {
        last_line = line;
        line_count += 1;
    }

    if text.ends_with('\n') {
        Range::new(Position::new(0, 0), Position::new(line_count, 0))
    } else {
        Range::new(
            Position::new(0, 0),
            Position::new(line_count - 1, last_line.encode_utf16().count() as u32),
        )
    }
}

fn assignment_symbol_kind(node: tree_sitter::Node, text: &str) -> tower_lsp::lsp_types::SymbolKind {
    match node.child_by_field_name("right") {
        Some(right)
            if right.kind() == "new_statement"
                && new_statement_type_name(right, text) == Some("Type") =>
        {
            tower_lsp::lsp_types::SymbolKind::CLASS
        }
        Some(right) if right.kind() == "function_expression" => {
            tower_lsp::lsp_types::SymbolKind::FUNCTION
        }
        _ => tower_lsp::lsp_types::SymbolKind::VARIABLE,
    }
}

fn new_statement_type_name<'a>(node: tree_sitter::Node, text: &'a str) -> Option<&'a str> {
    let type_node = node.child_by_field_name("type")?;
    if type_node.kind() != "symbol" {
        return None;
    }

    Some(&text[type_node.start_byte()..type_node.end_byte()])
}

fn collect_left_symbol_nodes<'tree>(
    node: tree_sitter::Node<'tree>,
    symbols: &mut Vec<tree_sitter::Node<'tree>>,
) {
    match node.kind() {
        "symbol" => symbols.push(node),
        "sequence" | "list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_left_symbol_nodes(child, symbols);
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

fn collect_binding_target_nodes<'tree>(
    node: tree_sitter::Node<'tree>,
    symbols: &mut Vec<tree_sitter::Node<'tree>>,
) {
    match node.kind() {
        "symbol" => symbols.push(node),
        "sequence" | "list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "symbol" {
                    symbols.push(child);
                }
            }
        }
        _ => {}
    }
}

fn binary_expression_operator<'a>(node: tree_sitter::Node, text: &'a str) -> Option<&'a str> {
    if node.kind() != "binary_expression" {
        return None;
    }

    node.child_by_field_name("operator")
        .map(|operator| &text[operator.start_byte()..operator.end_byte()])
}

#[derive(Debug)]
struct DocumentSymbolScopes {
    names: Vec<HashSet<String>>,
}

impl DocumentSymbolScopes {
    fn new() -> Self {
        Self {
            names: vec![HashSet::new()],
        }
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

fn collect_parameter_names(node: tree_sitter::Node, text: &str, names: &mut Vec<String>) {
    match node.kind() {
        "symbol" => names.push(text[node.start_byte()..node.end_byte()].to_string()),
        "sequence" | "list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_parameter_names(child, text, names);
            }
        }
        _ => {}
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

fn collect_document_symbols_from(
    node: tree_sitter::Node,
    text: &str,
    builtins: &BuiltinData,
    scopes: &mut DocumentSymbolScopes,
) -> Vec<DocumentSymbol> {
    match node.kind() {
        "assignment_expression" => {
            return collect_assignment_document_symbols(node, text, builtins, scopes)
        }
        "option_assignment" => return collect_property_document_symbols(node, text),
        _ => {}
    }

    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        symbols.extend(collect_document_symbols_from(child, text, builtins, scopes));
    }
    symbols
}

fn collect_property_document_symbols(node: tree_sitter::Node, text: &str) -> Vec<DocumentSymbol> {
    let Some(left) = node.child_by_field_name("left") else {
        return Vec::new();
    };

    let mut left_symbols = Vec::new();
    collect_left_symbol_nodes(left, &mut left_symbols);

    left_symbols
        .into_iter()
        .map(|symbol| DocumentSymbol {
            name: text[symbol.start_byte()..symbol.end_byte()].to_string(),
            detail: Some("option".to_string()),
            kind: tower_lsp::lsp_types::SymbolKind::PROPERTY,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            range: node_range(text, node),
            selection_range: node_range(text, symbol),
            children: None,
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
        Some(right) if right.kind() == "function_expression" => {
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

    let is_method_installation_left = matches!(
        left.kind(),
        "binary_expression" | "prefix_expression" | "postfix_expression"
    );

    match (operator, binary_expression_operator(left, text)) {
        (AssignmentOperator::ColonEqual, _) if is_method_installation_left => {
            vec![DocumentSymbol {
                name: text[left.start_byte()..left.end_byte()].to_string(),
                detail: Some("method".to_string()),
                kind: tower_lsp::lsp_types::SymbolKind::METHOD,
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
            kind: tower_lsp::lsp_types::SymbolKind::VARIABLE,
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
                .is_some_and(|right| right.kind() == "function_expression")
                && is_method_installation_left =>
        {
            vec![DocumentSymbol {
                name: text[left.start_byte()..left.end_byte()].to_string(),
                detail: Some("assignment method".to_string()),
                kind: tower_lsp::lsp_types::SymbolKind::METHOD,
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

fn collect_document_symbols(text: &str, builtins: &BuiltinData) -> Vec<DocumentSymbol> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_macaulay2::language())
        .unwrap();
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };

    let mut scopes = DocumentSymbolScopes::new();
    collect_document_symbols_from(tree.root_node(), text, builtins, &mut scopes)
}

fn symbol_node_at_position<'tree>(
    root_node: tree_sitter::Node<'tree>,
    text: &str,
    position: Position,
) -> Option<tree_sitter::Node<'tree>> {
    let point = tree_sitter_point_from_lsp_position(text, position)?;
    let mut node = root_node.descendant_for_point_range(point, point)?;

    loop {
        if matches!(node.kind(), "symbol" | "identifier" | "resolved_symbol") {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn collect_reference_ranges(
    text: &str,
    analysis: &Analysis,
    position: Position,
    include_declaration: bool,
) -> Vec<Range> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_macaulay2::language())
        .unwrap();
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let root_node = tree.root_node();
    let Some(target_node) = symbol_node_at_position(root_node, text, position) else {
        return Vec::new();
    };
    let target_name = &text[target_node.start_byte()..target_node.end_byte()];
    let Some(target_symbol) = analysis.get_symbol_at(target_name, position) else {
        return Vec::new();
    };
    let target_range = target_symbol.range;

    let mut references = Vec::new();
    let mut cursor = root_node.walk();
    let mut reached_root = false;
    while !reached_root {
        let node = cursor.node();
        if matches!(node.kind(), "symbol" | "identifier" | "resolved_symbol") {
            let node_text = &text[node.start_byte()..node.end_byte()];
            if node_text == target_name {
                let position = node_range(text, node).start;
                if let Some(symbol) = analysis.get_symbol_at(node_text, position) {
                    let range = node_range(text, node);
                    if symbol.range == target_range
                        && (include_declaration || range != target_range)
                    {
                        references.push(range);
                    }
                }
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                reached_root = true;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }

    references
}

impl Backend {
    fn new(client: Client) -> Self {
        let builtin_names = include_str!("./data/builtins.names");
        let builtin_details = include_str!("./data/builtins.details.jsonl");
        let builtins = BuiltinData::load_from_split(builtin_names, builtin_details);
        Backend {
            client,
            builtins,
            source_resolver: SourceResolver::from_environment(),
            package_indexer: PackageIndexer::from_environment(),
            package_indexes: DashMap::new(),
            documents: DashMap::new(),
            analyses: DashMap::new(),
            semantic_tokens_augment_syntax: AtomicBool::new(false),
            type_hierarchy_dynamic_registration: AtomicBool::new(false),
        }
    }

    fn package_index(&self, package_name: &str) -> Option<BuiltinData> {
        if let Some(index) = self.package_indexes.get(package_name) {
            return Some(index.clone());
        }

        let index = self.package_indexer.load_or_generate(package_name)?;
        self.package_indexes
            .insert(package_name.to_string(), index.clone());
        Some(index)
    }

    fn active_package_indexes(&self, text: &str) -> Vec<(String, BuiltinData)> {
        collect_imported_packages(text)
            .into_iter()
            .filter_map(|package| {
                let index = self.package_index(&package)?;
                Some((package, index))
            })
            .collect()
    }

    fn record_location(&self, record: &typesystem::Record) -> Option<Location> {
        let source_file = record_source_file(record)?;
        let path = self.source_resolver.resolve_source_file(source_file)?;
        let uri = Url::from_file_path(path).ok()?;
        let position = Position::new(record_source_line(record), 0);
        Some(Location {
            uri,
            range: Range::new(position, position),
        })
    }

    fn type_hierarchy_index(&self, package: Option<&str>) -> Option<BuiltinData> {
        match package {
            Some(package) if package != "Core" => self.package_index(package),
            _ => Some(self.builtins.clone()),
        }
    }

    fn type_hierarchy_package(item: &TypeHierarchyItem) -> Option<&str> {
        item.data
            .as_ref()
            .and_then(|data| data.get("package"))
            .and_then(|package| package.as_str())
    }

    fn type_hierarchy_record(
        &self,
        package: Option<&str>,
        name: &str,
    ) -> Option<(String, BuiltinData, typesystem::Record)> {
        let index = self.type_hierarchy_index(package)?;
        let record = index.get_record(&typesystem::InstanceID::new(name))?;
        record.type_info.as_ref()?;
        Some((package.unwrap_or("Core").to_string(), index, record))
    }

    fn type_hierarchy_related_record(
        &self,
        package: &str,
        index: &BuiltinData,
        name: &typesystem::InstanceID,
    ) -> Option<(String, typesystem::Record)> {
        if let Some(record) = index.get_record(name) {
            return Some((package.to_string(), record));
        }

        self.builtins
            .get_record(name)
            .map(|record| ("Core".to_string(), record))
    }

    fn type_hierarchy_item(
        &self,
        package: &str,
        record: &typesystem::Record,
        occurrence_uri: Option<Url>,
        occurrence_range: Option<Range>,
    ) -> TypeHierarchyItem {
        let location = self.record_location(record);
        let uri = occurrence_uri
            .or_else(|| location.as_ref().map(|location| location.uri.clone()))
            .unwrap_or_else(|| Url::parse("macaulay2:/builtins").expect("valid builtin URI"));
        let range = occurrence_range
            .or_else(|| location.as_ref().map(|location| location.range))
            .unwrap_or_else(|| Range::new(Position::new(0, 0), Position::new(0, 0)));
        let detail = record
            .type_info
            .as_ref()
            .and_then(|type_info| type_info.parent_type.as_ref())
            .filter(|parent| parent != &&record.name)
            .map(|parent| format!("Parent: {parent}"));

        TypeHierarchyItem {
            name: record.name.0.clone(),
            kind: record_symbol_kind(record),
            tags: None,
            detail,
            uri,
            range,
            selection_range: range,
            data: Some(serde_json::json!({
                "name": record.name.0.clone(),
                "package": package,
            })),
        }
    }

    async fn on_change(&self, params: TextDocumentItem) {
        let uri = params.uri.clone();
        self.documents.insert(uri.clone(), params.text.clone());
        let _ = self.active_package_indexes(&params.text);

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        if let Some(tree) = parser.parse(&params.text, None) {
            let analysis = Analysis::new_with_builtins(&tree, &params.text, Some(&self.builtins));
            let diagnostics = analysis.diagnostics.clone();
            self.analyses.insert(uri.clone(), analysis);

            self.client
                .publish_diagnostics(uri, diagnostics, None)
                .await;
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let text_document_capabilities = params.capabilities.text_document;
        let augments_syntax_tokens = text_document_capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.semantic_tokens.as_ref())
            .and_then(|semantic_tokens| semantic_tokens.augments_syntax_tokens)
            .unwrap_or(false);
        self.semantic_tokens_augment_syntax
            .store(augments_syntax_tokens, Ordering::Relaxed);
        let type_hierarchy_dynamic_registration = text_document_capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.type_hierarchy)
            .and_then(|type_hierarchy| type_hierarchy.dynamic_registration)
            .unwrap_or(false);
        self.type_hierarchy_dynamic_registration
            .store(type_hierarchy_dynamic_registration, Ordering::Relaxed);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: LEGEND_TYPES.into(),
                                token_modifiers: vec![
                                    SemanticTokenModifier::new("option"),
                                    SemanticTokenModifier::new("command"),
                                    SemanticTokenModifier::new("file"),
                                    SemanticTokenModifier::new("manipulator"),
                                    SemanticTokenModifier::DECLARATION,
                                    SemanticTokenModifier::new("constructor"),
                                ],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                        },
                    ),
                ),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if self
            .type_hierarchy_dynamic_registration
            .load(Ordering::Relaxed)
        {
            if self
                .client
                .register_capability(vec![Registration {
                    id: "m2_ls-type-hierarchy".to_string(),
                    method: TYPE_HIERARCHY_METHOD.to_string(),
                    register_options: Some(serde_json::json!({
                        "documentSelector": [
                            { "language": "macaulay2" }
                        ]
                    })),
                }])
                .await
                .is_ok()
            {
                self.client
                    .log_message(MessageType::INFO, "Macaulay2 type hierarchy registered")
                    .await;
            }
        }

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Macaulay2 LSP initialized with {} builtin symbols",
                    self.builtins.len()
                ),
            )
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_change(TextDocumentItem {
            uri: params.text_document.uri,
            language_id: "macaulay2".to_string(),
            version: params.text_document.version,
            text: params.text_document.text,
        })
        .await;
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        self.on_change(TextDocumentItem {
            uri: params.text_document.uri,
            language_id: "macaulay2".to_string(),
            version: params.text_document.version,
            text: std::mem::take(&mut params.content_changes[0].text),
        })
        .await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();
        let root_node = tree.root_node();

        let Some(point) = tree_sitter_point_from_lsp_position(&text, position) else {
            return Ok(None);
        };
        let node = match root_node.descendant_for_point_range(point, point) {
            Some(n) => n,
            None => return Ok(None),
        };

        let kind = node.kind();
        if kind == "symbol" || kind == "identifier" || kind == "operator" {
            let start_byte = node.start_byte();
            let end_byte = node.end_byte();
            let node_text = &text[start_byte..end_byte];

            if let Some(analysis) = self.analyses.get(uri) {
                if let Some(symbol) = analysis.get_symbol_at(node_text, position) {
                    return Ok(Some(local_symbol_hover(node_text, symbol)));
                }
            }

            for (package, package_index) in self.active_package_indexes(&text) {
                if let Some(record) =
                    package_index.get_record(&typesystem::InstanceID(node_text.to_string()))
                {
                    return Ok(Some(record_hover_with_package(
                        &record,
                        Some(&package),
                        &self.builtins,
                    )));
                }
            }

            if self.builtins.contains_name(node_text) {
                let Some(record) = self
                    .builtins
                    .get_record(&typesystem::InstanceID(node_text.to_string()))
                else {
                    return Ok(None);
                };
                return Ok(Some(record_hover_with_package(
                    &record,
                    Some("Core"),
                    &self.builtins,
                )));
            }
        }

        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let Some(prefix) = symbol_prefix_at(&text, position) else {
            return Ok(None);
        };

        let mut seen = HashSet::new();
        let mut items = Vec::new();

        for (package, package_index) in self.active_package_indexes(&text) {
            for name in package_index.names_with_prefix(&prefix, 40) {
                if seen.insert(name.to_string()) {
                    items.push(CompletionItem {
                        label: name.to_string(),
                        kind: Some(CompletionItemKind::FUNCTION),
                        detail: Some(format!("Package: {package}")),
                        ..Default::default()
                    });
                }
            }
        }

        items.extend(
            self.builtins
                .names_with_prefix(&prefix, 80usize.saturating_sub(items.len()))
                .into_iter()
                .filter(|name| seen.insert((*name).to_string()))
                .map(|name| CompletionItem {
                    label: name.to_string(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    ..Default::default()
                }),
        );

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let text = match self.documents.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let analysis = self.analyses.get(&uri);
        let augments_syntax_tokens = self.semantic_tokens_augment_syntax.load(Ordering::Relaxed);
        let tokens = collect_semantic_tokens(
            &text,
            analysis.as_deref(),
            &self.builtins,
            augments_syntax_tokens,
        );

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let text = match self.documents.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let symbols = collect_document_symbols(&text, &self.builtins);
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let Some(analysis) = self.analyses.get(uri) else {
            return Ok(None);
        };

        let references = collect_reference_ranges(
            &text,
            &analysis,
            position,
            params.context.include_declaration,
        )
        .into_iter()
        .map(|range| Location {
            uri: uri.clone(),
            range,
        })
        .collect();

        Ok(Some(references))
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let text = match self.documents.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        let Some(tree) = parser.parse(&text, None) else {
            return Ok(None);
        };
        let Some(node) = symbol_node_at_position(tree.root_node(), &text, position) else {
            return Ok(None);
        };
        let name = &text[node.start_byte()..node.end_byte()];
        let range = node_range(&text, node);

        for (package, package_index) in self.active_package_indexes(&text) {
            if let Some(record) = package_index.get_record(&typesystem::InstanceID::new(name)) {
                if record.type_info.is_some() {
                    return Ok(Some(vec![self.type_hierarchy_item(
                        &package,
                        &record,
                        Some(uri.clone()),
                        Some(range),
                    )]));
                }
            }
        }

        let Some(record) = self.builtins.get_record(&typesystem::InstanceID::new(name)) else {
            return Ok(None);
        };
        if record.type_info.is_none() {
            return Ok(None);
        }

        Ok(Some(vec![self.type_hierarchy_item(
            "Core",
            &record,
            Some(uri.clone()),
            Some(range),
        )]))
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let package = Self::type_hierarchy_package(&params.item);
        let Some((package, index, record)) = self.type_hierarchy_record(package, &params.item.name)
        else {
            return Ok(None);
        };

        let Some(parent_name) = record
            .type_info
            .as_ref()
            .and_then(|type_info| type_info.parent_type.as_ref())
            .filter(|parent| parent != &&record.name)
        else {
            return Ok(Some(Vec::new()));
        };

        let Some((parent_package, parent_record)) =
            self.type_hierarchy_related_record(&package, &index, parent_name)
        else {
            return Ok(Some(Vec::new()));
        };

        Ok(Some(vec![self.type_hierarchy_item(
            &parent_package,
            &parent_record,
            None,
            None,
        )]))
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let package = Self::type_hierarchy_package(&params.item);
        let Some((package, index, record)) = self.type_hierarchy_record(package, &params.item.name)
        else {
            return Ok(None);
        };

        let mut items = Vec::new();
        if let Some(type_info) = &record.type_info {
            for subtype in &type_info.subtypes {
                if subtype == &record.name {
                    continue;
                }
                if let Some((subtype_package, subtype_record)) =
                    self.type_hierarchy_related_record(&package, &index, subtype)
                {
                    items.push(self.type_hierarchy_item(
                        &subtype_package,
                        &subtype_record,
                        None,
                        None,
                    ));
                }
            }
        }

        Ok(Some(items))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let text = match self.documents.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let formatted = format_document_text_with_options(
            &text,
            &FormatOptions::new(params.options.tab_size, params.options.insert_spaces),
        );
        if formatted == text {
            return Ok(Some(Vec::new()));
        }

        Ok(Some(vec![TextEdit {
            range: full_document_range(&text),
            new_text: formatted,
        }]))
    }

    #[allow(deprecated)]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.trim();
        if query.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let mut symbols = Vec::new();
        let mut seen = HashSet::new();

        for package_entry in self.package_indexes.iter() {
            let package = package_entry.key().clone();
            for name in package_entry.value().matching_names(query, 80) {
                let Some(record) = package_entry
                    .value()
                    .get_record(&typesystem::InstanceID(name.to_string()))
                else {
                    continue;
                };
                let Some(location) = self.record_location(&record) else {
                    continue;
                };
                if seen.insert(workspace_symbol_dedupe_key(&package, name)) {
                    symbols.push(SymbolInformation {
                        name: name.to_string(),
                        kind: record_symbol_kind(&record),
                        tags: None,
                        deprecated: None,
                        location,
                        container_name: Some(package.clone()),
                    });
                }
            }
        }

        for name in self
            .builtins
            .matching_names(query, 120usize.saturating_sub(symbols.len()))
        {
            if !should_include_workspace_symbol("Core", name) {
                continue;
            }
            let Some(record) = self
                .builtins
                .get_record(&typesystem::InstanceID(name.to_string()))
            else {
                continue;
            };
            let Some(location) = self.record_location(&record) else {
                continue;
            };
            if seen.insert(workspace_symbol_dedupe_key("Core", name)) {
                symbols.push(SymbolInformation {
                    name: name.to_string(),
                    kind: record_symbol_kind(&record),
                    tags: None,
                    deprecated: None,
                    location,
                    container_name: Some("Core".to_string()),
                });
            }
        }

        Ok(Some(symbols))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();
        let root_node = tree.root_node();

        let Some(point) = tree_sitter_point_from_lsp_position(&text, position) else {
            return Ok(None);
        };
        let node = match root_node.descendant_for_point_range(point, point) {
            Some(n) => n,
            None => return Ok(None),
        };

        if let Some(string_node) = enclosing_node_of_kind(node, "string_literal") {
            if let Some(package_name) = package_source_string(&text, string_node) {
                if let Some(path) = self.source_resolver.resolve_package_file(package_name) {
                    if let Ok(uri) = Url::from_file_path(path) {
                        return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                            uri,
                            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        })));
                    }
                }
            }
        }

        let kind = node.kind();
        if kind == "symbol" || kind == "identifier" {
            let start_byte = node.start_byte();
            let end_byte = node.end_byte();
            let node_text = &text[start_byte..end_byte];

            if let Some(analysis) = self.analyses.get(uri) {
                if let Some(range) = analysis.find_definition(node_text, position) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                        uri: uri.clone(),
                        range,
                    })));
                }
            }

            for (_, package_index) in self.active_package_indexes(&text) {
                if let Some(record) =
                    package_index.get_record(&typesystem::InstanceID(node_text.to_string()))
                {
                    if let Some(location) = self.record_location(&record) {
                        return Ok(Some(GotoDefinitionResponse::Scalar(location)));
                    }
                }
            }

            if let Some(record) = self
                .builtins
                .get_record(&typesystem::InstanceID(node_text.to_string()))
            {
                if let Some(location) = self.record_location(&record) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(location)));
                }
            }
        }

        Ok(None)
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket)
        .serve(TypeHierarchyCapabilityService::new(service))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_prefix_uses_lsp_utf16_columns() {
        assert_eq!(
            symbol_prefix_at("éideal", Position::new(0, 3)).as_deref(),
            Some("éid")
        );
        assert_eq!(
            symbol_prefix_at("😀 ideal", Position::new(0, 7)).as_deref(),
            Some("idea")
        );
    }

    #[test]
    fn initialize_result_advertises_static_type_hierarchy() {
        let response = Response::from_ok(
            1.into(),
            json!({
                "capabilities": {
                    "hoverProvider": true
                }
            }),
        );

        let response = advertise_type_hierarchy_capability(response);
        let result = response
            .result()
            .expect("response should remain successful");

        assert_eq!(result["capabilities"]["typeHierarchyProvider"], json!(true));
    }

    #[test]
    fn type_hierarchy_dynamic_registration_detection_defaults_to_static() {
        let dynamic = Request::build("initialize")
            .params(json!({
                "capabilities": {
                    "textDocument": {
                        "typeHierarchy": {
                            "dynamicRegistration": true
                        }
                    }
                }
            }))
            .id(1)
            .finish();
        let static_only = Request::build("initialize")
            .params(json!({
                "capabilities": {
                    "textDocument": {
                        "typeHierarchy": {
                            "dynamicRegistration": false
                        }
                    }
                }
            }))
            .id(2)
            .finish();

        assert!(request_type_hierarchy_dynamic_registration(
            dynamic.params()
        ));
        assert!(!request_type_hierarchy_dynamic_registration(
            static_only.params()
        ));
        assert!(!request_type_hierarchy_dynamic_registration(None));
    }

    #[test]
    fn tree_sitter_points_convert_utf16_to_byte_columns() {
        let point = tree_sitter_point_from_lsp_position("é ideal", Position::new(0, 3))
            .expect("position should be on the first line");
        assert_eq!(point.column, 4);

        let point = tree_sitter_point_from_lsp_position("😀 ideal", Position::new(0, 3))
            .expect("position should be on the first line");
        assert_eq!(point.column, 5);
    }

    #[test]
    fn semantic_token_spans_use_utf16_units() {
        let text = "😀 ideal";
        let start = text.find("ideal").expect("fixture should contain token");
        let end = start + "ideal".len();

        assert_eq!(utf16_len_for_byte_span(text, 0, start), 3);
        assert_eq!(utf16_len_for_byte_span(text, start, end), 5);
    }

    #[test]
    fn full_document_range_handles_utf16_columns() {
        assert_eq!(
            full_document_range("x\n😀 ideal"),
            Range::new(Position::new(0, 0), Position::new(1, 8))
        );
        assert_eq!(
            full_document_range("x\n"),
            Range::new(Position::new(0, 0), Position::new(1, 0))
        );
    }

    #[test]
    fn source_resolver_finds_package_and_doc_files_from_m2_path_roots() {
        let root =
            std::env::temp_dir().join(format!("m2-lsp-source-resolver-{}", std::process::id()));
        let packages = root.join("Macaulay2").join("packages");
        let docs = packages.join("Macaulay2Doc");
        let core = root.join("Macaulay2").join("m2");
        std::fs::create_dir_all(&docs).expect("test docs dir should be created");
        std::fs::create_dir_all(&core).expect("test core dir should be created");
        std::fs::write(packages.join("Graphs.m2"), "").expect("package fixture should write");
        std::fs::write(docs.join("operators.m2"), "").expect("doc fixture should write");
        std::fs::write(core.join("option.m2"), "").expect("core fixture should write");

        let resolver = SourceResolver::new(vec![packages.clone()]);

        assert_eq!(
            resolver.resolve_package_file("Graphs"),
            Some(packages.join("Graphs.m2"))
        );
        assert_eq!(
            resolver.resolve_source_file("Macaulay2Doc/operators.m2"),
            Some(docs.join("operators.m2"))
        );
        assert_eq!(
            resolver.resolve_source_file("m2/option.m2"),
            Some(core.join("option.m2"))
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn package_source_string_detects_import_like_calls() {
        let text =
            "needsPackage \"Graphs\"\nloadPackage(\"Normaliz\", Reload => true)\ndebug \"Core\"";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let root = tree.root_node();
        let mut packages = Vec::new();
        let mut cursor = root.walk();
        let mut reached_root = false;
        while !reached_root {
            let node = cursor.node();
            if node.kind() == "string_literal" {
                if let Some(package_name) = package_source_string(text, node) {
                    packages.push(package_name);
                }
            }

            if cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    reached_root = true;
                    break;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        assert_eq!(packages, vec!["Graphs", "Normaliz", "Core"]);
    }

    #[test]
    fn collect_imported_packages_deduplicates_import_like_calls() {
        let text = "needsPackage \"Graphs\"\nloadPackage(\"Normaliz\")\nneedsPackage \"Graphs\"";

        assert_eq!(
            collect_imported_packages(text),
            vec!["Graphs".to_string(), "Normaliz".to_string()]
        );
    }

    #[test]
    fn package_indexer_loads_cached_line_aligned_package_records() {
        let root =
            std::env::temp_dir().join(format!("m2-lsp-package-index-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test package cache dir should be created");
        std::fs::write(root.join("Graphs.names"), "graph\n")
            .expect("package names fixture should write");
        std::fs::write(
            root.join("Graphs.details.jsonl"),
            "{\"name\":\"graph\",\"data_type\":\"MethodFunction\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{\"package\":\"Graphs\"}}\n",
        )
        .expect("package details fixture should write");

        let indexer = PackageIndexer {
            cache_dir: root.clone(),
            extractor_script: None,
        };
        let index = indexer
            .load("Graphs")
            .expect("cached package index should load");
        let record = index
            .get_record(&typesystem::InstanceID::new("graph"))
            .expect("package record should be available");

        assert_eq!(record.name.0, "graph");
        assert_eq!(record_package(&record), Some("Graphs"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn package_indexer_searches_crate_script_path() {
        let crate_script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/extract_package_index.m2");

        assert!(
            extractor_script_candidates()
                .iter()
                .any(|candidate| candidate == &crate_script),
            "extractor discovery should include the crate-local script"
        );
        assert!(
            crate_script.exists(),
            "crate-local package extractor fixture should exist"
        );
    }

    #[test]
    fn collect_reference_ranges_finds_same_file_local_symbols() {
        let text = "f := x -> (y := x + x; y)\nf 1";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new(&tree, text);

        let with_declaration =
            collect_reference_ranges(text, &analysis, Position::new(0, 16), true);
        let without_declaration =
            collect_reference_ranges(text, &analysis, Position::new(0, 16), false);

        assert_eq!(
            with_declaration,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(0, 16), Position::new(0, 17)),
                Range::new(Position::new(0, 20), Position::new(0, 21)),
            ]
        );
        assert_eq!(
            without_declaration,
            vec![
                Range::new(Position::new(0, 16), Position::new(0, 17)),
                Range::new(Position::new(0, 20), Position::new(0, 21)),
            ]
        );
    }

    #[test]
    fn document_symbol_ranges_use_lsp_utf16_columns() {
        let text = "\"😀\"; f := 1";
        let builtins = BuiltinData::load_from_split("", "");

        let symbols = collect_document_symbols(text, &builtins);

        assert_eq!(symbols[0].name, "f");
        assert_eq!(
            symbols[0].selection_range,
            Range::new(Position::new(0, 6), Position::new(0, 7))
        );
    }

    #[test]
    fn reference_ranges_use_lsp_utf16_columns() {
        let text = "f := x -> (\"😀\"; x + x)";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new(&tree, text);

        let ranges = collect_reference_ranges(text, &analysis, Position::new(0, 17), true);

        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(0, 17), Position::new(0, 18)),
                Range::new(Position::new(0, 21), Position::new(0, 22)),
            ]
        );
    }

    #[test]
    fn weird_valid_m2_runtime_syntax_documents_current_parser_gaps() {
        let text = include_str!("../tests/fixtures/weird_valid_syntax.m2");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new(&tree, text);
        let diagnostic_lines = analysis
            .diagnostics
            .iter()
            .map(|diagnostic| {
                text.lines()
                    .nth(diagnostic.range.start.line as usize)
                    .expect("diagnostic should point into the fixture")
            })
            .collect::<Vec<_>>();

        assert_eq!(diagnostic_lines, vec![".2x.2", "x.2"]);
    }

    #[test]
    fn parameter_references_use_parameter_semantic_token_type() {
        let symbol = SymbolInfo {
            kind: SymbolKind::Parameter,
            range: Range::new(Position::new(0, 5), Position::new(0, 6)),
            type_name: None,
        };
        let builtins = BuiltinData::load_from_split("", "");

        assert_eq!(
            local_symbol_semantic_token_type(&symbol, Position::new(0, 5), &builtins),
            M2SemanticTokenType::Parameter
        );
        assert_eq!(
            local_symbol_semantic_token_type(&symbol, Position::new(0, 10), &builtins),
            M2SemanticTokenType::Parameter
        );
    }

    #[test]
    fn local_hover_includes_known_static_type() {
        let symbol = SymbolInfo {
            kind: SymbolKind::Variable,
            range: Range::new(Position::new(2, 4), Position::new(2, 7)),
            type_name: Some("Package".to_string()),
        };

        let hover = local_symbol_hover("Doc", &symbol);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("local hover should use markdown");
        };

        assert!(
            markup.value.contains("Type: `Package`"),
            "local hover should display known static type facts"
        );
    }

    #[test]
    fn record_hover_includes_explicit_package_context() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("clearAll"))
            .expect("clearAll should have builtin metadata");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(
            markup.value.contains("Package: `Core`"),
            "record hover should display the package supplied by the LSP context"
        );
    }

    #[test]
    fn record_hover_includes_option_role() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("SyzygyLimit"))
            .expect("SyzygyLimit should have builtin metadata");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(
            markup.value.contains("Option Role: `key`"),
            "record hover should identify option keys"
        );
        assert!(
            markup.value.contains("- `gb`") && markup.value.contains("- `syz`"),
            "record hover should list methods using known option keys"
        );
    }

    #[test]
    fn record_hover_includes_documented_signatures_and_examples() {
        let builtins = BuiltinData::load_from_split(
            "kernel\n",
            "{\"name\":\"kernel\",\"data_type\":\"MethodFunction\",\"description_short\":\"kernel of a map\",\"description_long\":null,\"examples\":[\"R = QQ[a..d];\",\"ker F\"],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"kernel\",\"RingMap\"]}],\"documented_methods\":[{\"signature\":[\"kernel\",\"RingMap\"],\"output_types\":[\"Ideal\"],\"examples\":[\"R = QQ[a..d];\"],\"doc_key\":\"kernel(RingMap)\"}]}}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("kernel"))
            .expect("kernel should deserialize");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(
            markup.value.contains("`(kernel, RingMap) -> Ideal`"),
            "record hover should display documented method codomains"
        );
        assert!(
            markup.value.contains("```macaulay2\nR = QQ[a..d];"),
            "record hover should display saved examples"
        );
    }

    #[test]
    fn workspace_symbols_omit_core_qualified_twins() {
        assert!(!should_include_workspace_symbol("Core", "Core$name"));
        assert!(should_include_workspace_symbol("Core", "name"));
        assert!(should_include_workspace_symbol("SomePackage", "Core$name"));
    }

    #[test]
    fn semantic_tokens_classify_parameter_body_references_as_parameters() {
        let text = "f := x -> x";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new(&tree, text);
        let builtins = BuiltinData::load_from_split("", "");

        let tokens = collect_semantic_tokens(text, Some(&analysis), &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Function as u32,
                M2SemanticTokenType::Operator as u32,
                M2SemanticTokenType::Parameter as u32,
                M2SemanticTokenType::Operator as u32,
                M2SemanticTokenType::Parameter as u32,
            ]
        );
        assert_eq!(
            tokens[2].token_modifiers_bitset & DECLARATION_MODIFIER,
            DECLARATION_MODIFIER
        );
        assert_eq!(tokens[4].token_modifiers_bitset & DECLARATION_MODIFIER, 0);
    }

    #[test]
    fn semantic_tokens_include_recognized_syntax_tokens() {
        let text = "-- hi\nif x then 42 + 1 else \"no\"\nlocal y";
        let builtins = BuiltinData::load_from_split("", "");

        let tokens = collect_semantic_tokens(text, None, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Comment as u32,
                M2SemanticTokenType::Keyword as u32,
                M2SemanticTokenType::Keyword as u32,
                M2SemanticTokenType::Number as u32,
                M2SemanticTokenType::Operator as u32,
                M2SemanticTokenType::Number as u32,
                M2SemanticTokenType::Keyword as u32,
                M2SemanticTokenType::String as u32,
                M2SemanticTokenType::Modifier as u32,
            ]
        );
    }

    #[test]
    fn semantic_tokens_classify_binding_qualifiers_as_modifiers() {
        let text = "global x\nlocal y\nsymbol z\nthreadLocal w\nthreadVariable q";
        let builtins = BuiltinData::load_from_split("", "");

        let tokens = collect_semantic_tokens(text, None, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Modifier as u32,
                M2SemanticTokenType::Modifier as u32,
                M2SemanticTokenType::Modifier as u32,
                M2SemanticTokenType::Modifier as u32,
                M2SemanticTokenType::Modifier as u32,
            ]
        );
    }

    #[test]
    fn semantic_tokens_do_not_classify_booleans_as_keywords() {
        let text = "if true then false else true";
        let builtins = BuiltinData::load_from_split("", "");

        let tokens = collect_semantic_tokens(text, None, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Keyword as u32,
                M2SemanticTokenType::Keyword as u32,
                M2SemanticTokenType::Keyword as u32,
            ]
        );
    }

    #[test]
    fn semantic_tokens_classify_regex_string_arguments_as_regexp() {
        let text = "match(\"a+\", s)\nreplace(\"a+\", \"b\", s)\nseparate(\"a+\", s)";
        let builtins = BuiltinData::load_from_split("", "");

        let tokens = collect_semantic_tokens(text, None, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .filter(|token_type| {
                    *token_type == M2SemanticTokenType::Regexp as u32
                        || *token_type == M2SemanticTokenType::String as u32
                })
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Regexp as u32,
                M2SemanticTokenType::Regexp as u32,
                M2SemanticTokenType::String as u32,
                M2SemanticTokenType::Regexp as u32,
            ]
        );
    }

    #[test]
    fn semantic_tokens_classify_package_argument_strings_as_namespaces() {
        let text = concat!(
            "loadPackage \"Graphs\"\n",
            "installPackage(\"Normaliz\")\n",
            "uninstallPackage \"Foo\"\n",
            "needsPackage \"Core\"\n",
            "export \"thing\"\n",
            "endPackage \"Pkg\"\n",
            "newPackage(\"Pkg\")\n",
            "importFrom(\"Pkg\")\n",
            "exportFrom {\"Pkg\"}\n",
            "print \"ordinary\""
        );
        let builtins = BuiltinData::load_from_split("", "");

        let tokens = collect_semantic_tokens(text, None, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .filter(|token_type| {
                    *token_type == M2SemanticTokenType::Namespace as u32
                        || *token_type == M2SemanticTokenType::String as u32
                })
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::String as u32,
            ]
        );
    }

    #[test]
    fn semantic_tokens_use_static_types_for_user_defined_symbols() {
        let text = "Doc := Macaulay2Doc\nDocAlias := Doc\nDocAlias#\"raw documentation database\"\nZZAlias := ZZ\nQQAlias := QQ\nZZAlias QQAlias\nn := 1\nn";
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(&builtins));

        let tokens = collect_semantic_tokens(text, Some(&analysis), &builtins, true);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .filter(|token_type| *token_type == M2SemanticTokenType::Namespace as u32)
                .count(),
            5,
            "package-typed local aliases and their references should classify as namespace"
        );
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .filter(|token_type| *token_type == M2SemanticTokenType::Class as u32)
                .count(),
            6,
            "aliases bound to class-valued objects should classify as class, including references"
        );
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .filter(|token_type| *token_type == M2SemanticTokenType::Variable as u32)
                .count(),
            2,
            "integer-valued locals should remain variables even though their static type is ZZ"
        );
    }

    #[test]
    fn semantic_tokens_classify_commands_as_functions_with_command_modifier() {
        let text = "saveClearAll := clearAll\nclearAll = new Command from { () -> () }\nprotect symbol clearAll";
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(&builtins));

        let tokens = collect_semantic_tokens(text, Some(&analysis), &builtins, true);

        assert_eq!(
            tokens
                .iter()
                .filter(|token| {
                    token.token_type == M2SemanticTokenType::Operator as u32
                        && token.token_modifiers_bitset & COMMAND_MODIFIER == COMMAND_MODIFIER
                })
                .count(),
            0,
            "Command values should no longer use operator+command"
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|token| {
                    token.token_type == M2SemanticTokenType::Function as u32
                        && token.token_modifiers_bitset & COMMAND_MODIFIER == COMMAND_MODIFIER
                })
                .count(),
            4,
            "builtin, aliased, and locally rebound Command values should use function+command"
        );
    }

    #[test]
    fn semantic_tokens_repaint_builtin_identifiers_when_client_needs_full_colorization() {
        let text = "drop Ring any";
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );

        let tokens = collect_semantic_tokens(text, None, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| (token.token_type, token.token_modifiers_bitset))
                .filter(|(token_type, _)| *token_type != M2SemanticTokenType::Operator as u32)
                .collect::<Vec<_>>(),
            vec![
                (M2SemanticTokenType::Function as u32, 0),
                (M2SemanticTokenType::Class as u32, 0),
                (M2SemanticTokenType::Function as u32, 0),
            ]
        );
    }

    #[test]
    fn semantic_tokens_augmenting_syntax_keeps_high_value_builtins_without_broad_repaint() {
        let text = "-- hi\nMacaulay2Doc#\"raw documentation database\"\nCore.Dictionary\nQQ\nZZ\nif drop then \"x\" else any\nlocal y\nmatch(\"a+\", s)";
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );

        let tokens = collect_semantic_tokens(text, None, &builtins, true);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Namespace as u32,
                M2SemanticTokenType::Class as u32,
                M2SemanticTokenType::Class as u32,
                M2SemanticTokenType::Class as u32,
                M2SemanticTokenType::Function as u32,
                M2SemanticTokenType::Function as u32,
                M2SemanticTokenType::Modifier as u32,
                M2SemanticTokenType::Method as u32,
                M2SemanticTokenType::Regexp as u32,
            ],
            "syntax-augmenting clients should keep high-value package/callable Core tokens without repainting every builtin category"
        );
    }

    #[test]
    fn document_symbols_include_top_level_and_nested_assignments() {
        let text = "f := x -> (y := x + 1; y)\nR = QQ[a]\n";
        let builtins = BuiltinData::load_from_split("", "");

        let symbols = collect_document_symbols(text, &builtins);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "f");
        assert_eq!(symbols[0].kind, tower_lsp::lsp_types::SymbolKind::FUNCTION);
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
            tower_lsp::lsp_types::SymbolKind::VARIABLE
        );
        assert_eq!(symbols[1].name, "R");
        assert_eq!(symbols[1].kind, tower_lsp::lsp_types::SymbolKind::VARIABLE);
    }

    #[test]
    fn document_symbols_include_only_new_bindings_in_m2_scopes() {
        let text =
            "x := 1\nx := 2\ny = 1\ny = 2\nf := x -> (x = 2; K = x; z := 3; z := 4)\nK = 3\n";
        let builtins = BuiltinData::load_from_split("", "");

        let symbols = collect_document_symbols(text, &builtins);

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
    fn document_symbols_include_static_bindings_from_extractor_script_once() {
        let text = include_str!("../scripts/extract_builtins.m2");
        let builtins = BuiltinData::load_from_split("", "");

        let symbols = collect_document_symbols(text, &builtins);
        let args_symbols = symbols
            .iter()
            .filter(|symbol| symbol.name == "args")
            .collect::<Vec<_>>();

        assert_eq!(
            args_symbols.len(),
            1,
            "top-level args should be a single static document symbol"
        );
        assert_eq!(
            args_symbols[0].selection_range.start,
            Position::new(11, 0),
            "args should point at the first static binding"
        );
    }

    #[test]
    fn document_symbols_cover_static_top_level_extractor_bindings() {
        fn has_function_ancestor(mut node: tree_sitter::Node) -> bool {
            while let Some(parent) = node.parent() {
                if parent.kind() == "function_expression" {
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
            if node.kind() == "assignment_expression" && !has_function_ancestor(node) {
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

        let text = include_str!("../scripts/extract_builtins.m2");
        let builtins = BuiltinData::load_from_split("", "");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        let tree = parser.parse(text, None).expect("extractor should parse");
        let mut expected = Vec::new();
        collect_static_top_level_bindings(tree.root_node(), text, &mut expected);

        let symbols = collect_document_symbols(text, &builtins);
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
        let builtins = BuiltinData::load_from_split("", "");

        let symbols = collect_document_symbols(text, &builtins);

        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (symbol.name.as_str(), symbol.detail.as_deref(), symbol.kind))
                .collect::<Vec<_>>(),
            vec![
                (
                    "Thing Thing",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
                (
                    "Thing .. Thing",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
                (
                    "toString Tally",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
                ("x", None, tower_lsp::lsp_types::SymbolKind::VARIABLE),
                ("y", None, tower_lsp::lsp_types::SymbolKind::VARIABLE),
                ("z", None, tower_lsp::lsp_types::SymbolKind::VARIABLE),
                (
                    "x_i",
                    Some("indexed variable"),
                    tower_lsp::lsp_types::SymbolKind::VARIABLE
                ),
                (
                    "String * String",
                    Some("assignment method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
                (
                    "- String",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
                (
                    "String ^~",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
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
        let builtins = BuiltinData::load_from_split("", "");

        let symbols = collect_document_symbols(text, &builtins);

        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (symbol.name.as_str(), symbol.detail.as_deref(), symbol.kind))
                .collect::<Vec<_>>(),
            vec![
                ("X", None, tower_lsp::lsp_types::SymbolKind::CLASS),
                ("Y", None, tower_lsp::lsp_types::SymbolKind::CLASS),
                ("Z", None, tower_lsp::lsp_types::SymbolKind::CLASS),
                (
                    "- X",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
                (
                    "Y + X",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
                (
                    "X + Z",
                    Some("method"),
                    tower_lsp::lsp_types::SymbolKind::METHOD
                ),
            ]
        );
    }

    #[test]
    fn document_symbols_include_cst_option_properties() {
        let text = "f := x -> g(x, Strategy => LongPolynomial)";
        let builtins = BuiltinData::load_from_split("", "");

        let symbols = collect_document_symbols(text, &builtins);
        let children = symbols[0]
            .children
            .as_ref()
            .expect("function body option assignment should appear as child symbols");

        assert_eq!(children[0].name, "Strategy");
        assert_eq!(children[0].kind, tower_lsp::lsp_types::SymbolKind::PROPERTY);
        assert_eq!(children[0].detail.as_deref(), Some("option"));
    }

    #[test]
    fn document_symbols_keep_to_type_functions_as_functions() {
        let text = "toString := x -> x";
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );

        let symbols = collect_document_symbols(text, &builtins);

        assert_eq!(symbols[0].name, "toString");
        assert_eq!(symbols[0].kind, tower_lsp::lsp_types::SymbolKind::FUNCTION);
    }

    #[test]
    fn builtin_type_tokens_do_not_use_custom_type_modifier() {
        let token = typesystem::M2SemanticToken {
            token_type: M2SemanticTokenType::Type,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
        };

        let modifiers = builtin_semantic_token_modifiers(&token);

        assert_eq!(modifiers, 0);
    }

    #[test]
    fn builtin_class_tokens_do_not_use_custom_type_modifier() {
        let token = typesystem::M2SemanticToken {
            token_type: M2SemanticTokenType::Class,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
        };

        let modifiers = builtin_semantic_token_modifiers(&token);

        assert_eq!(modifiers, 0);
    }

    #[test]
    fn builtin_function_tokens_do_not_use_provenance_modifiers() {
        let token = typesystem::M2SemanticToken {
            token_type: M2SemanticTokenType::Function,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
        };

        let modifiers = builtin_semantic_token_modifiers(&token);

        assert_eq!(modifiers, 0);
    }

    #[test]
    fn compiled_builtin_function_tokens_do_not_use_provenance_modifiers() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let token = builtins
            .get_semantic_token("drop")
            .expect("drop should have builtin metadata");

        assert_eq!(token.token_type, M2SemanticTokenType::Function);
        assert_eq!(builtin_semantic_token_modifiers(&token), 0);
    }

    #[test]
    fn builtin_constructor_tokens_use_official_type_and_custom_modifier() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let token = builtins
            .get_semantic_token("toString")
            .expect("toString should have builtin metadata");

        assert_eq!(token.token_type, M2SemanticTokenType::Method);
        assert_eq!(
            builtin_semantic_token_modifiers(&token) & CONSTRUCTOR_MODIFIER,
            CONSTRUCTOR_MODIFIER
        );
    }

    #[test]
    fn option_assignment_symbols_have_context_roles() {
        let text = "f(x, Strategy => LongPolynomial)";
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let root = tree.root_node();

        let mut roles = Vec::new();
        let mut cursor = root.walk();
        let mut reached_root = false;
        while !reached_root {
            let node = cursor.node();
            if node.kind() == "symbol" {
                roles.push((
                    &text[node.start_byte()..node.end_byte()],
                    option_assignment_role(text, node, &builtins),
                ));
            }

            if cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    reached_root = true;
                    break;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        assert!(roles.contains(&("Strategy", Some(M2SemanticTokenType::EnumMember))));
        assert!(roles.contains(&("LongPolynomial", Some(M2SemanticTokenType::EnumMember))));
    }

    #[test]
    fn option_assignment_roles_require_metadata() {
        let text = "f(x, notAnOption => notAnOptionValue)";
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let root = tree.root_node();

        let mut roles = Vec::new();
        let mut cursor = root.walk();
        let mut reached_root = false;
        while !reached_root {
            let node = cursor.node();
            if node.kind() == "symbol" {
                roles.push((
                    &text[node.start_byte()..node.end_byte()],
                    option_assignment_role(text, node, &builtins),
                ));
            }

            if cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    reached_root = true;
                    break;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        assert!(roles.contains(&("notAnOption", None)));
        assert!(roles.contains(&("notAnOptionValue", None)));
    }

    #[test]
    fn semantic_token_modifier_bits_match_legend_order() {
        assert_eq!(OPTION_MODIFIER, 1 << 0);
        assert_eq!(COMMAND_MODIFIER, 1 << 1);
        assert_eq!(FILE_MODIFIER, 1 << 2);
        assert_eq!(MANIPULATOR_MODIFIER, 1 << 3);
        assert_eq!(DECLARATION_MODIFIER, 1 << 4);
        assert_eq!(CONSTRUCTOR_MODIFIER, 1 << 5);
    }
}
