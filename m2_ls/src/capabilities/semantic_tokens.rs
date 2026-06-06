use tower_lsp::lsp_types::*;

use crate::analysis::{SymbolInfo, SymbolKind};
use crate::document::DocumentSnapshot;
use crate::typesystem::{BuiltinData, M2SemanticTokenType};
use crate::util::*;

pub(crate) const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::TYPE,           // 0
    SemanticTokenType::FUNCTION,       // 1
    SemanticTokenType::VARIABLE,       // 2
    SemanticTokenType::PARAMETER,      // 3
    SemanticTokenType::PROPERTY,       // 4
    SemanticTokenType::NAMESPACE,      // 5
    SemanticTokenType::ENUM_MEMBER,    // 6
    SemanticTokenType::CLASS,          // 7
    SemanticTokenType::KEYWORD,        // 8
    SemanticTokenType::STRING,         // 9
    SemanticTokenType::NUMBER,         // 10
    SemanticTokenType::OPERATOR,       // 11
    SemanticTokenType::COMMENT,        // 12
    SemanticTokenType::METHOD,         // 13
    SemanticTokenType::REGEXP,         // 14
    SemanticTokenType::MODIFIER,       // 15
    SemanticTokenType::TYPE_PARAMETER, // 16
];

pub(crate) const OPTION_MODIFIER: u32 = 1 << 0;
pub(crate) const COMMAND_MODIFIER: u32 = 1 << 1;
pub(crate) const FILE_MODIFIER: u32 = 1 << 2;
pub(crate) const MANIPULATOR_MODIFIER: u32 = 1 << 3;
pub(crate) const DECLARATION_MODIFIER: u32 = 1 << 4;
pub(crate) const CONSTRUCTOR_MODIFIER: u32 = 1 << 5;

pub(crate) fn semantic_tokens_response(
    document: &DocumentSnapshot,
    builtins: &BuiltinData,
    augments_syntax_tokens: bool,
) -> SemanticTokensResult {
    SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data: collect_semantic_tokens(document, builtins, augments_syntax_tokens),
    })
}

pub(crate) fn collect_semantic_tokens(
    document: &DocumentSnapshot,
    builtins: &BuiltinData,
    augments_syntax_tokens: bool,
) -> Vec<SemanticToken> {
    let text = document.text();
    let analysis = document.analysis();
    let root_node = document.root_node();

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
                if let Some(type_param_role) =
                    method_installation_type_parameter(text, node, builtins)
                {
                    token_type = Some(type_param_role as u32);
                }
            }

            if token_type.is_none() {
                if let Some(symbol) = analysis.get_symbol_at(node_text, position) {
                    token_type =
                        Some(local_symbol_semantic_token_type(symbol, position, builtins) as u32);
                    if let Some(type_name) = &symbol.type_name {
                        if let Some(token) = builtins.get_semantic_token_for_static_type(type_name)
                        {
                            modifiers |= builtin_semantic_token_modifiers(&token);
                        }
                    }
                    if symbol.kind == SymbolKind::Parameter && position == symbol.range.start {
                        modifiers |= DECLARATION_MODIFIER;
                    }
                    if symbol.kind == SymbolKind::Function && builtins.is_constructor_name(node_text)
                    {
                        modifiers |= CONSTRUCTOR_MODIFIER;
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

            if token_type.is_none()
                && (!augments_syntax_tokens || should_emit_syntax_token_when_augmenting(text, node))
            {
                token_type =
                    syntax_semantic_token_type(text, node).map(|token_type| token_type as u32);
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

pub(crate) fn option_assignment_role(
    text: &str,
    node: tree_sitter::Node,
    builtins: &BuiltinData,
) -> Option<M2SemanticTokenType> {
    let parent = node.parent()?;
    if !is_option_assignment_expression(parent, text) {
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

pub(crate) fn local_symbol_semantic_token_type(
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

pub(crate) fn builtin_semantic_token_modifiers(token: &crate::typesystem::M2SemanticToken) -> u32 {
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

fn syntax_semantic_token_type(text: &str, node: tree_sitter::Node) -> Option<M2SemanticTokenType> {
    if is_cobinding_operator_node(node) {
        return Some(M2SemanticTokenType::Modifier);
    }

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

fn should_emit_builtin_token_when_augmenting(token: &crate::typesystem::M2SemanticToken) -> bool {
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

fn is_cobinding_operator_node(node: tree_sitter::Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        "cobinding" | "global_cobinding" | "local_cobinding" | "thread_cobinding"
    ) && parent
        .child_by_field_name("operator")
        .is_some_and(|operator| operator.id() == node.id())
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

fn binary_expression_left_symbol<'a>(text: &'a str, node: tree_sitter::Node) -> Option<&'a str> {
    if node.kind() != "binary_expression" {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if left.kind() != "symbol" {
        return None;
    }

    let operator = node.child_by_field_name("operator")?;
    if operator.kind() != "SPACE" {
        return None;
    }

    Some(&text[left.start_byte()..left.end_byte()])
}

fn is_first_named_child(parent: tree_sitter::Node, child: tree_sitter::Node) -> bool {
    parent
        .named_child(0)
        .is_some_and(|first| first.id() == child.id())
}

fn method_installation_type_parameter(
    text: &str,
    node: tree_sitter::Node,
    builtins: &BuiltinData,
) -> Option<M2SemanticTokenType> {
    let node_text = &text[node.start_byte()..node.end_byte()];
    let parent = node.parent()?;

    if is_type_parameter_in_domain(node, parent, text) && is_known_type(builtins, node_text) {
        return Some(M2SemanticTokenType::TypeParameter);
    }

    if is_type_parameter_in_lambda(node, parent, text) && is_known_type(builtins, node_text) {
        return Some(M2SemanticTokenType::TypeParameter);
    }

    if is_type_parameter_in_typical_value(node, parent, text) && is_known_type(builtins, node_text)
    {
        return Some(M2SemanticTokenType::TypeParameter);
    }

    None
}

fn is_known_type(builtins: &BuiltinData, name: &str) -> bool {
    builtins.get_semantic_token(name).is_some_and(|token| {
        matches!(
            token.token_type,
            M2SemanticTokenType::Class | M2SemanticTokenType::Type
        )
    })
}

fn is_type_parameter_in_domain(
    node: tree_sitter::Node,
    mut ancestor: tree_sitter::Node,
    text: &str,
) -> bool {
    while matches!(ancestor.kind(), "sequence" | "list") {
        ancestor = match ancestor.parent() {
            Some(p) => p,
            None => return false,
        };
    }

    if ancestor.kind() != "binary_expression" {
        return false;
    }
    let op_text = match binary_expression_operator(ancestor, text) {
        Some(op) => op,
        None => return false,
    };
    if matches!(op_text, "=" | ":=" | "<-" | "=>") {
        return false;
    }
    if !node_is_within(ancestor, node) {
        return false;
    }

    let grandparent = match ancestor.parent() {
        Some(gp) => gp,
        None => return false,
    };
    if !is_assignment_expression(grandparent, text) {
        return false;
    }
    let assignment_op = match binary_expression_operator(grandparent, text) {
        Some(op) => op,
        None => return false,
    };
    if !matches!(assignment_op, "=" | ":=") {
        return false;
    }

    grandparent
        .child_by_field_name("left")
        .is_some_and(|left| left.id() == ancestor.id())
}

fn is_type_parameter_in_lambda(
    node: tree_sitter::Node,
    lambda: tree_sitter::Node,
    text: &str,
) -> bool {
    if lambda.kind() != "lambda_expression" {
        return false;
    }
    if !lambda
        .child_by_field_name("left")
        .is_some_and(|param| param.id() == node.id())
    {
        return false;
    }

    let mut current = lambda;
    loop {
        let parent = match current.parent() {
            Some(p) => p,
            None => return false,
        };

        if parent.kind() == "binary_expression" {
            let op = binary_expression_operator(parent, text);
            if matches!(op, Some("=" | ":=")) {
                let lhs = match parent.child_by_field_name("left") {
                    Some(l) => l,
                    None => return false,
                };
                if !is_method_installation_lhs(lhs, text) {
                    return false;
                }
                let rhs = match parent.child_by_field_name("right") {
                    Some(r) => r,
                    None => return false,
                };
                return node_is_within(rhs, lambda);
            }
            current = parent;
            continue;
        }
        current = parent;
    }
}

fn is_method_installation_lhs(node: tree_sitter::Node, text: &str) -> bool {
    if node.kind() != "binary_expression" {
        return false;
    }
    let op = match binary_expression_operator(node, text) {
        Some(op) => op,
        None => return false,
    };
    !matches!(op, "=" | ":=" | "<-" | "=>")
}

fn is_type_parameter_in_typical_value(
    node: tree_sitter::Node,
    parent: tree_sitter::Node,
    text: &str,
) -> bool {
    if parent.kind() != "binary_expression" {
        return false;
    }
    if binary_expression_operator(parent, text) != Some("=>") {
        return false;
    }
    if !parent
        .child_by_field_name("left")
        .is_some_and(|left| left.id() == node.id())
    {
        return false;
    }

    let mut current = parent;
    loop {
        let grandparent = match current.parent() {
            Some(gp) => gp,
            None => return false,
        };

        if grandparent.kind() == "binary_expression" {
            let op = binary_expression_operator(grandparent, text);
            if matches!(op, Some("=" | ":=")) {
                let rhs = match grandparent.child_by_field_name("right") {
                    Some(r) => r,
                    None => return false,
                };
                if !node_is_within(rhs, parent) {
                    return false;
                }
                let lhs = match grandparent.child_by_field_name("left") {
                    Some(l) => l,
                    None => return false,
                };
                return is_method_installation_lhs(lhs, text);
            }
            return false;
        }
        current = grandparent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentSnapshot;
    use crate::typesystem::{BuiltinData, M2SemanticToken, M2SemanticTokenType};
    use tree_sitter::Parser;

    fn document(text: &str, builtins: &BuiltinData) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), builtins).expect("fixture should parse")
    }

    #[test]
    fn semantic_tokens_classify_parameter_body_references_as_parameters() {
        let text = "f := x -> x";
        let builtins = BuiltinData::load_from_split("", "");
        let document = document(text, &builtins);

        let tokens = collect_semantic_tokens(&document, &builtins, false);

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

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, false);

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

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, false);

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

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, false);

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

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, false);

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

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, false);

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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
        );
        let document = document(text, &builtins);

        let tokens = collect_semantic_tokens(&document, &builtins, true);

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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
        );
        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, true);

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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
        );

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, false);

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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
        );

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, true);

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
    fn builtin_type_tokens_do_not_use_custom_type_modifier() {
        let token = M2SemanticToken {
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
        let token = M2SemanticToken {
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
        let token = M2SemanticToken {
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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
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
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
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

    #[test]
    fn method_installation_domain_emits_type_parameter_for_known_types() {
        let text = "Ring Element := x -> x";
        let builtins = BuiltinData::load_from_split(
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
        );

        let document = document(text, &builtins);
        let tokens = collect_semantic_tokens(&document, &builtins, false);
        let token_types: Vec<u32> = tokens.iter().map(|t| t.token_type).collect();

        let type_param = M2SemanticTokenType::TypeParameter as u32;
        assert!(
            token_types.contains(&type_param),
            "Ring in method installation should be TypeParameter, got {:?}",
            token_types
        );
    }
}
