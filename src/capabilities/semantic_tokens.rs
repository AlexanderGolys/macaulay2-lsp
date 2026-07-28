//! Semantic-token extraction from syntax, static analysis, and builtin metadata.

use tower_lsp::lsp_types::*;

use crate::analysis::BindingView;
use crate::document::DocumentSnapshot;
use crate::documentation::DocumentationSnippet;
use crate::meta::{BindingRole, Metadata};
use crate::node_metadata::{M2Node, NodeKind, NodeKindMetadata};
use crate::source::{DocumentSpan, SourceNavigation};
use crate::typesystem::{
    M2SemanticToken, M2SemanticTokenProvenance, M2SemanticTokenType, SemanticTokenKnowledge,
};
use crate::workspace_index::WorkspaceDefinitionKnowledge;

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
    SemanticTokenType::DECORATOR,      // 17
];

pub(crate) const LEGEND_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::new("option"),   // 0
    SemanticTokenModifier::new("command"),  // 1
    SemanticTokenModifier::new("file"),     // 2
    SemanticTokenModifier::DECLARATION,     // 3
    SemanticTokenModifier::DEFAULT_LIBRARY, // 4
    SemanticTokenModifier::new("builtin"),  // 5
    SemanticTokenModifier::new("macro"),    // 6
];

pub(crate) const OPTION_MODIFIER: u32 = 1 << 0;
pub(crate) const COMMAND_MODIFIER: u32 = 1 << 1;
pub(crate) const FILE_MODIFIER: u32 = 1 << 2;
pub(crate) const DECLARATION_MODIFIER: u32 = 1 << 3;
pub(crate) const DEFAULT_LIBRARY_MODIFIER: u32 = 1 << 4;
pub(crate) const BUILTIN_MODIFIER: u32 = 1 << 5;
pub(crate) const MACRO_MODIFIER: u32 = 1 << 6;

pub(crate) fn collect_semantic_tokens(
    document: &DocumentSnapshot,
    builtins: &(impl SemanticTokenKnowledge + ?Sized),
    workspace_index: &(impl WorkspaceDefinitionKnowledge + ?Sized),
    uri: &Url,
    augments_syntax_tokens: bool,
) -> Vec<SemanticToken> {
    let text = document.text();
    let analysis = document.analysis();
    let root_node = document.root_node();
    let classifier = SemanticTokenClassifier {
        document,
        builtins,
        workspace_index,
        uri,
    };
    let mut emitter = SemanticTokenEmitter::new(document);

    let mut cursor = root_node.walk();
    let mut reached_root = false;

    while !reached_root {
        let node = M2Node::new(cursor.node(), text);
        let syntax_token_type = syntax_semantic_token_type(node);

        let mut emitted_token = emitter.emit_documentation_container_tokens(
            node,
            syntax_token_type,
            augments_syntax_tokens,
        );
        if !emitted_token && (node.kind.is_symbol_like() || syntax_token_type.is_some()) {
            let source = document.span_for_node(node);
            let position = source.range().start;
            let binding = analysis
                .get_symbol_at(node.text(), position)
                .map(|symbol| (symbol, position == symbol.range.start));
            let emit_syntax = !augments_syntax_tokens
                || should_emit_syntax_token_when_augmenting(syntax_token_type);

            if let Some((token_type, modifiers)) = classifier.classify(node, binding, emit_syntax) {
                emitted_token = emitter.push(SemanticSpan {
                    source,
                    token_type,
                    modifiers,
                });
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

    emitter.finish()
}

struct SemanticTokenEmitter<'a> {
    document: &'a DocumentSnapshot,
    tokens: Vec<SemanticToken>,
    previous: Position,
}

impl<'a> SemanticTokenEmitter<'a> {
    fn new(document: &'a DocumentSnapshot) -> Self {
        Self {
            document,
            tokens: Vec::new(),
            previous: Position::new(0, 0),
        }
    }

    fn emit_documentation_container_tokens(
        &mut self,
        node: M2Node<'_>,
        syntax_token_type: Option<M2SemanticTokenType>,
        augments_syntax_tokens: bool,
    ) -> bool {
        let Some(base_type) = syntax_token_type.filter(|token_type| {
            matches!(
                token_type,
                M2SemanticTokenType::Comment | M2SemanticTokenType::String
            )
        }) else {
            return false;
        };

        let mut spans = self
            .document
            .documentation_snippets()
            .iter()
            .filter(|snippet| {
                let (start, end) = snippet.byte_span();
                node.start_byte() <= start && end <= node.end_byte()
            })
            .map(|snippet| documentation_snippet_semantic_span(self.document, snippet))
            .collect::<Vec<_>>();
        if spans.is_empty() {
            return false;
        }
        spans.sort_by_key(|span| {
            let bytes = span.source.bytes();
            (bytes.start, bytes.end)
        });

        let emit_base =
            !augments_syntax_tokens || should_emit_syntax_token_when_augmenting(Some(base_type));
        let mut cursor = node.start_byte();
        let mut emitted = false;

        for span in spans {
            let span_bytes = span.source.bytes();
            if span_bytes.start < cursor {
                continue;
            }
            if emit_base && cursor < span_bytes.start {
                emitted |= self.push(SemanticSpan {
                    source: self.document.span_for_bytes(cursor..span_bytes.start),
                    token_type: base_type,
                    modifiers: 0,
                });
            }
            cursor = span_bytes.end;
            emitted |= self.push(span);
        }

        if emitted && emit_base && cursor < node.end_byte() {
            emitted |= self.push(SemanticSpan {
                source: self.document.span_for_bytes(cursor..node.end_byte()),
                token_type: base_type,
                modifiers: 0,
            });
        }

        emitted
    }

    fn push(&mut self, span: SemanticSpan) -> bool {
        let mut emitted = false;
        for range in self.document.visible_ranges(&span.source) {
            let delta_line = range.start.line - self.previous.line;
            let delta_start = if delta_line == 0 {
                range.start.character - self.previous.character
            } else {
                range.start.character
            };
            self.tokens.push(SemanticToken {
                delta_line,
                delta_start,
                length: range.end.character - range.start.character,
                token_type: span.token_type as u32,
                token_modifiers_bitset: span.modifiers,
            });
            self.previous = range.start;
            emitted = true;
        }
        emitted
    }

    fn finish(self) -> Vec<SemanticToken> {
        self.tokens
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticSpan {
    source: DocumentSpan,
    token_type: M2SemanticTokenType,
    modifiers: u32,
}

fn documentation_snippet_semantic_span(
    document: &DocumentSnapshot,
    snippet: &DocumentationSnippet,
) -> SemanticSpan {
    let (start_byte, end_byte) = snippet.byte_span();
    SemanticSpan {
        source: document.span_for_bytes(start_byte..end_byte),
        token_type: M2SemanticTokenType::Property,
        modifiers: 0,
    }
}

struct SemanticTokenClassifier<'a, B: ?Sized, W: ?Sized> {
    document: &'a DocumentSnapshot,
    builtins: &'a B,
    workspace_index: &'a W,
    uri: &'a Url,
}

impl<B, W> SemanticTokenClassifier<'_, B, W>
where
    B: SemanticTokenKnowledge + ?Sized,
    W: WorkspaceDefinitionKnowledge + ?Sized,
{
    fn classify(
        &self,
        node: M2Node<'_>,
        binding: Option<(BindingView<'_>, bool)>,
        emit_syntax: bool,
    ) -> Option<(M2SemanticTokenType, u32)> {
        classify_semantic_node(self, node, binding, emit_syntax)
    }
}

fn classify_semantic_node<B, W>(
    context: &SemanticTokenClassifier<'_, B, W>,
    node: M2Node<'_>,
    binding: Option<(BindingView<'_>, bool)>,
    emit_syntax: bool,
) -> Option<(M2SemanticTokenType, u32)>
where
    B: SemanticTokenKnowledge + ?Sized,
    W: WorkspaceDefinitionKnowledge + ?Sized,
{
    let document = context.document;
    let analysis = document.analysis();
    let builtins = context.builtins;
    let workspace_index = context.workspace_index;
    let node_text = node.text();
    let syntax_token_type = syntax_semantic_token_type(node);
    let indexed_token = node
        .kind
        .is_symbol_like()
        .then(|| builtins.semantic_token(node_text))
        .flatten();
    let mut token_type = None;
    let mut modifiers = 0;
    let mut resolved_from_index = false;

    if document.is_macro_name(node) {
        token_type = Some(M2SemanticTokenType::Method);
        modifiers |= MACRO_MODIFIER;
    }

    if token_type.is_none() && is_quoted_global_key_access(node) {
        token_type = Some(M2SemanticTokenType::Property);
    }

    if token_type.is_none() && analysis.is_method_installation_codomain(node, document) {
        token_type = Some(M2SemanticTokenType::Type);
        resolved_from_index = indexed_token.is_some();
    }

    if token_type.is_none() {
        if let Some(role) = option_assignment_role(node, builtins) {
            token_type = Some(role);
            modifiers |= OPTION_MODIFIER;
            resolved_from_index = indexed_token.is_some();
        }
    }

    if token_type.is_none() {
        if let Some(type_param_role) = method_installation_type_parameter(node, builtins) {
            token_type = Some(type_param_role);
            resolved_from_index = true;
        }
    }

    if token_type.is_none() {
        if let Some((symbol, is_declaration)) = binding {
            token_type = Some(local_symbol_semantic_token_type(&symbol, builtins));
            if let Some(type_name) = symbol.meta().type_name {
                if let Some(token) =
                    static_type_semantic_token_for_local_symbol(&symbol, type_name, builtins)
                {
                    modifiers |= builtin_semantic_token_modifiers(&token);
                }
            }
            if is_declaration {
                modifiers |= DECLARATION_MODIFIER;
            }
        }
    }

    if token_type.is_none() {
        if let Some(token) = indexed_token {
            token_type = Some(token.token_type);
            modifiers |= builtin_semantic_token_modifiers(&token);
            resolved_from_index = true;
        }
    }

    if token_type.is_none() {
        token_type = workspace_index.semantic_token_type(node_text, context.uri);
    }

    if token_type.is_none() && emit_syntax {
        token_type = syntax_token_type;
    }

    if resolved_from_index {
        modifiers |= indexed_semantic_token_modifiers(&indexed_token?);
    }
    token_type.map(|token_type| (token_type, modifiers))
}

pub(crate) fn option_assignment_role(
    node: M2Node<'_>,
    builtins: &(impl SemanticTokenKnowledge + ?Sized),
) -> Option<M2SemanticTokenType> {
    let parent = node.parent()?;
    if !parent.is_option_assignment() {
        return None;
    }

    let node_text = node.text();
    // The KEY of a `k => v` pair. A protected symbol key is a nominal enum
    // member (`Strategy`, `Hilbert`); every other key — an unprotected symbol or
    // a string used as a dictionary key — is a field/property.
    if parent
        .child_by_field_name("left")
        .is_some_and(|left| left.id() == node.id())
    {
        if node.kind == NodeKind::Symbol && builtins.is_protected_symbol(node_text) {
            return Some(M2SemanticTokenType::EnumMember);
        }
        return Some(M2SemanticTokenType::Property);
    }

    if parent
        .child_by_field_name("right")
        .is_some_and(|right| right.id() == node.id())
    {
        let option_key = parent
            .child_by_field_name("left")
            .filter(|left| left.kind.is_symbol_like())
            .map(|left| left.text())?;
        if builtins.is_option_value_for_key(option_key, node_text) {
            return Some(M2SemanticTokenType::EnumMember);
        }
    }

    None
}

pub(crate) fn local_symbol_semantic_token_type(
    symbol: &(impl Metadata + ?Sized),
    builtins: &(impl SemanticTokenKnowledge + ?Sized),
) -> M2SemanticTokenType {
    let meta = symbol.meta();
    if meta.binding_role == Some(BindingRole::Parameter) {
        return M2SemanticTokenType::Parameter;
    }

    if let Some(type_name) = meta.type_name {
        if let Some(token) =
            static_type_semantic_token_for_local_symbol(symbol, type_name, builtins)
        {
            return token.token_type;
        }
    }

    match meta.symbol_kind {
        Some(SymbolKind::FUNCTION) => M2SemanticTokenType::Function,
        Some(SymbolKind::VARIABLE) => M2SemanticTokenType::Variable,
        Some(SymbolKind::METHOD) => M2SemanticTokenType::Method,
        Some(SymbolKind::CLASS) => M2SemanticTokenType::Class,
        Some(SymbolKind::NAMESPACE) => M2SemanticTokenType::Namespace,
        Some(SymbolKind::PROPERTY) => M2SemanticTokenType::Property,
        Some(SymbolKind::CONSTANT) => M2SemanticTokenType::EnumMember,
        _ => M2SemanticTokenType::Variable,
    }
}

pub(crate) fn builtin_semantic_token_modifiers(token: &M2SemanticToken) -> u32 {
    let mut modifiers = 0;
    if token.is_command || token.is_manipulator {
        modifiers |= COMMAND_MODIFIER;
    }
    if token.is_file {
        modifiers |= FILE_MODIFIER;
    }
    modifiers
}

fn indexed_semantic_token_modifiers(token: &M2SemanticToken) -> u32 {
    let mut modifiers = builtin_semantic_token_modifiers(token);
    match token.provenance {
        M2SemanticTokenProvenance::None => {}
        M2SemanticTokenProvenance::DefaultLibrary => modifiers |= DEFAULT_LIBRARY_MODIFIER,
        M2SemanticTokenProvenance::Builtin => modifiers |= BUILTIN_MODIFIER,
    }
    modifiers
}

fn static_type_semantic_token_for_local_symbol(
    symbol: &(impl Metadata + ?Sized),
    type_name: &str,
    builtins: &(impl SemanticTokenKnowledge + ?Sized),
) -> Option<M2SemanticToken> {
    let mut token = builtins.semantic_token_for_static_type(type_name)?;
    match symbol.meta().symbol_kind {
        // A ring value (`R = QQ[x]`) is itself the runtime type of its elements,
        // but it is not an M2 class declaration. Keep actual classes, including
        // locally declared and function-produced classes, as `class`.
        Some(SymbolKind::VARIABLE)
            if token.token_type == M2SemanticTokenType::Class
                && builtins.is_subtype(type_name, "Ring") =>
        {
            token.token_type = M2SemanticTokenType::Type;
            Some(token)
        }
        Some(SymbolKind::VARIABLE)
            if matches!(
                token.token_type,
                M2SemanticTokenType::String | M2SemanticTokenType::Number
            ) =>
        {
            None
        }
        _ => Some(token),
    }
}

fn syntax_semantic_token_type(node: M2Node<'_>) -> Option<M2SemanticTokenType> {
    if node.is_operator() {
        return Some(M2SemanticTokenType::Operator);
    }

    match node.kind {
        NodeKind::IntegerLiteral | NodeKind::FloatLiteral => Some(M2SemanticTokenType::Number),
        NodeKind::StringLiteral if is_regexp_string_argument(node) => {
            Some(M2SemanticTokenType::Regexp)
        }
        NodeKind::StringLiteral if is_namespace_string_argument(node) => {
            Some(M2SemanticTokenType::Namespace)
        }
        NodeKind::StringLiteral if is_hash_key_string(node) => Some(M2SemanticTokenType::Property),
        NodeKind::StringLiteral => Some(M2SemanticTokenType::String),
        kind if kind.is_comment() => Some(M2SemanticTokenType::Comment),
        _ if node.is_modifier_token() => Some(M2SemanticTokenType::Modifier),
        _ if node.is_keyword_token() => Some(M2SemanticTokenType::Keyword),
        _ => None,
    }
}

fn should_emit_syntax_token_when_augmenting(token_type: Option<M2SemanticTokenType>) -> bool {
    matches!(
        token_type,
        Some(M2SemanticTokenType::Modifier)
            | Some(M2SemanticTokenType::Regexp)
            | Some(M2SemanticTokenType::EnumMember)
            | Some(M2SemanticTokenType::Property)
            | Some(M2SemanticTokenType::Namespace)
    )
}

fn is_regexp_string_argument(node: M2Node<'_>) -> bool {
    if node.kind != NodeKind::StringLiteral {
        return false;
    }

    call_like_left_symbol_for_argument(node, false)
        .is_some_and(|name| matches!(name, "match" | "regex" | "select" | "replace" | "separate"))
}

fn is_namespace_string_argument(node: M2Node<'_>) -> bool {
    if node.kind != NodeKind::StringLiteral {
        return false;
    }

    // Only the first positional argument of these calls names a package (a real
    // namespace): `loadPackage "Pkg"`, `importFrom("Pkg", {syms})`, and so on.
    // Passing `false` for `allow_list_argument` means a string buried in a list
    // argument never resolves to its callee, so the symbol names in
    // `export {"foo"}` or the imported names in `importFrom("Pkg", {"foo"})` stay
    // plain strings. `export`/`exportMutable` are absent entirely: their arguments
    // name symbols defined in this package, not modules.
    call_like_left_symbol_for_argument(node, false).is_some_and(|name| {
        matches!(
            name,
            "loadPackage"
                | "installPackage"
                | "uninstallPackage"
                | "needsPackage"
                | "endPackage"
                | "newPackage"
                | "importFrom"
                | "exportFrom"
        )
    })
}

/// A string literal used as a hash-table key: the left operand of `=>`
/// (`"Quote" => "symbol"`), or the right operand of the `#` / `#?` lookup
/// operators (`h#"key"`, `h#?"key"`). The value on the right of `=>` keeps its
/// own classification, and symbol keys to `#` stay value references (they are
/// evaluated, not quoted).
fn is_hash_key_string(node: M2Node<'_>) -> bool {
    if node.kind != NodeKind::StringLiteral {
        return false;
    }
    node.parent().is_some_and(|parent| {
        let is_assignment_key = parent.is_option_assignment()
            && parent
                .child_by_field_name("left")
                .is_some_and(|left| left.id() == node.id());
        let is_lookup_key = matches!(parent.binary_operator(), Some("#" | "#?"))
            && parent
                .child_by_field_name("right")
                .is_some_and(|right| right.id() == node.id());
        is_assignment_key || is_lookup_key
    })
}

/// A symbol read as a quoted global key: the right operand of the `.` or `.?`
/// member operator (`R.name`, `R.?name`). M2 quotes the right side as a global
/// symbol used as a hash key, so it is a property rather than a value reference.
fn is_quoted_global_key_access(node: M2Node<'_>) -> bool {
    if !node.kind.is_symbol_like() {
        return false;
    }
    node.parent().is_some_and(|parent| {
        matches!(parent.binary_operator(), Some("." | ".?"))
            && parent
                .child_by_field_name("right")
                .is_some_and(|right| right.id() == node.id())
    })
}

fn call_like_left_symbol_for_argument<'tree>(
    mut node: M2Node<'tree>,
    allow_list_argument: bool,
) -> Option<&'tree str> {
    let mut parent = node.parent()?;
    if parent.kind == NodeKind::Sequence && !parent.is_first_collection_element(node) {
        return None;
    }

    loop {
        if let Some(name) = binary_expression_left_symbol(parent) {
            return Some(name);
        }

        if parent.kind == NodeKind::List && !allow_list_argument {
            return None;
        }

        // A single parenthesized argument `f("x")` is a `parenthesized_expression`,
        // a multi-argument call a `sequence`; both wrap an argument before the
        // `callee ARG` application.
        if !matches!(
            parent.kind,
            NodeKind::Sequence | NodeKind::List | NodeKind::ParenthesizedExpression
        ) {
            return None;
        }

        node = parent;
        parent = node.parent()?;
    }
}

fn binary_expression_left_symbol(node: M2Node<'_>) -> Option<&str> {
    if !node.is_space_application() {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if left.kind != NodeKind::Symbol {
        return None;
    }

    Some(left.text())
}

fn method_installation_type_parameter(
    node: M2Node<'_>,
    builtins: &(impl SemanticTokenKnowledge + ?Sized),
) -> Option<M2SemanticTokenType> {
    let node_text = node.text();
    let parent = node.parent()?;

    if is_type_parameter_in_domain(parent) && is_known_type(builtins, node_text) {
        return Some(M2SemanticTokenType::Type);
    }

    if is_type_parameter_in_lambda(node, parent) && is_known_type(builtins, node_text) {
        return Some(M2SemanticTokenType::Type);
    }

    None
}

fn is_known_type(builtins: &(impl SemanticTokenKnowledge + ?Sized), name: &str) -> bool {
    builtins.semantic_token(name).is_some_and(|token| {
        matches!(
            token.token_type,
            M2SemanticTokenType::Class | M2SemanticTokenType::Type
        )
    })
}

fn is_type_parameter_in_domain(mut ancestor: M2Node<'_>) -> bool {
    // A domain type sits inside `(T1, T2)` (sequence) or, for a single-type domain
    // `f(T) := …`, a `parenthesized_expression`; climb through either.
    while matches!(
        ancestor.kind,
        NodeKind::Sequence | NodeKind::List | NodeKind::ParenthesizedExpression
    ) {
        ancestor = match ancestor.parent() {
            Some(p) => p,
            None => return false,
        };
    }

    if ancestor.kind != NodeKind::BinaryExpression {
        return false;
    }
    let op_text = match ancestor.binary_operator() {
        Some(op) => op,
        None => return false,
    };
    if matches!(op_text, "=" | ":=" | "<-" | "=>") {
        return false;
    }

    let grandparent = match ancestor.parent() {
        Some(gp) => gp,
        None => return false,
    };
    if !grandparent.is_assignment() {
        return false;
    }
    let assignment_op = match grandparent.binary_operator() {
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

fn is_type_parameter_in_lambda(node: M2Node<'_>, lambda: M2Node<'_>) -> bool {
    if lambda.kind != NodeKind::LambdaExpression {
        return false;
    }
    if lambda
        .child_by_field_name("left")
        .is_none_or(|param| param.id() != node.id())
    {
        return false;
    }

    let mut current = lambda;
    loop {
        let parent = match current.parent() {
            Some(p) => p,
            None => return false,
        };

        if parent.kind == NodeKind::BinaryExpression {
            let op = parent.binary_operator();
            if matches!(op, Some("=" | ":=")) {
                let lhs = match parent.child_by_field_name("left") {
                    Some(l) => l,
                    None => return false,
                };
                if !is_method_installation_lhs(lhs) {
                    return false;
                }
                let rhs = match parent.child_by_field_name("right") {
                    Some(r) => r,
                    None => return false,
                };
                return rhs.contains(lambda);
            }
            current = parent;
            continue;
        }
        current = parent;
    }
}

fn is_method_installation_lhs(node: M2Node<'_>) -> bool {
    if node.kind != NodeKind::BinaryExpression {
        return false;
    }
    let op = match node.binary_operator() {
        Some(op) => op,
        None => return false,
    };
    !matches!(op, "=" | ":=" | "<-" | "=>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_index::BuiltinData;
    use crate::document::DocumentSnapshot;
    use crate::partitioned_index::PackagePartitionedIndex;
    use crate::typesystem::TypeKnowledgeProvider;
    use crate::typesystem::{M2SemanticToken, M2SemanticTokenType};
    use crate::workspace_index::WorkspaceIndex;
    use tree_sitter::Parser;

    fn document(text: &str, builtins: &BuiltinData) -> DocumentSnapshot {
        DocumentSnapshot::from_text(text.to_string(), builtins).expect("fixture should parse")
    }

    /// Collect tokens for a single isolated document — no other workspace files,
    /// so the cross-file classification step contributes nothing.
    fn collect_tokens(
        document: &DocumentSnapshot,
        builtins: &BuiltinData,
        augments_syntax_tokens: bool,
    ) -> Vec<SemanticToken> {
        let workspace_index = WorkspaceIndex::default();
        let uri = Url::parse("file:///fixture.m2").expect("valid fixture uri");
        collect_semantic_tokens(
            document,
            builtins,
            &workspace_index,
            &uri,
            augments_syntax_tokens,
        )
    }

    fn token_at(tokens: &[SemanticToken], line: u32, character: u32) -> Option<&SemanticToken> {
        let mut token_line = 0;
        let mut token_start = 0;
        for token in tokens {
            if token.delta_line == 0 {
                token_start += token.delta_start;
            } else {
                token_line += token.delta_line;
                token_start = token.delta_start;
            }
            if token_line == line
                && character >= token_start
                && character < token_start + token.length
            {
                return Some(token);
            }
        }
        None
    }

    fn token_type_at(tokens: &[SemanticToken], line: u32, character: u32) -> Option<u32> {
        token_at(tokens, line, character).map(|token| token.token_type)
    }

    #[test]
    fn multi_line_string_token_is_split_per_line() {
        // A raw string spans two lines. The LSP protocol forbids a token from
        // crossing a line boundary, so it must be emitted as one token per line.
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let text = "x := ///alpha\nbeta///\n";
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        let line_lengths = text
            .match_indices('\n')
            .map(|(byte, _)| document.position_for_byte(byte).character)
            .chain(std::iter::once(
                document.position_for_byte(text.len()).character,
            ))
            .collect::<Vec<_>>();

        // Decode the delta-encoded stream to absolute positions and assert every
        // token fits within its own source line.
        let mut line = 0u32;
        let mut start = 0u32;
        for token in &tokens {
            if token.delta_line > 0 {
                line += token.delta_line;
                start = token.delta_start;
            } else {
                start += token.delta_start;
            }
            assert!(
                start + token.length <= line_lengths[line as usize],
                "token at line {line} col {start} len {} runs past the {}-wide line",
                token.length,
                line_lengths[line as usize]
            );
        }

        let string_tokens = tokens
            .iter()
            .filter(|token| token.token_type == M2SemanticTokenType::String as u32)
            .count();
        assert!(
            string_tokens >= 2,
            "the two-line raw string must yield a token per line, got {string_tokens}"
        );
    }

    #[test]
    fn cross_file_class_reference_is_highlighted_as_a_class() {
        // An M2 class defined at the top level of another workspace file
        // highlights as CLASS where it is referenced, even though it is neither
        // a local binding nor a builtin in the referencing file.
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let workspace_index = WorkspaceIndex::default();
        let defs_uri = Url::parse("file:///defs.m2").expect("valid uri");
        workspace_index.index_file(&defs_uri, "TokenStream = new Type of List\n", &builtins);

        let main_uri = Url::parse("file:///main.m2").expect("valid uri");
        let text = "TokenStream\n";
        let document = document(text, &builtins);
        let tokens =
            collect_semantic_tokens(&document, &builtins, &workspace_index, &main_uri, false);

        let class_token = M2SemanticTokenType::Class as u32;
        assert!(
            tokens.iter().any(|token| token.token_type == class_token),
            "expected a CLASS token for the cross-file class reference, got {tokens:?}"
        );
    }

    #[test]
    fn cross_file_lookup_excludes_the_current_file() {
        // The current file's own definitions come from its live analysis, not the
        // workspace index, so a self-reference must not be sourced cross-file.
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let workspace_index = WorkspaceIndex::default();
        let main_uri = Url::parse("file:///main.m2").expect("valid uri");
        workspace_index.index_file(&main_uri, "TokenStream = new Type of List\n", &builtins);
        // Excluding main.m2 leaves no other definition, so nothing is contributed.
        assert!(workspace_index
            .semantic_token_type("TokenStream", &main_uri)
            .is_none());
    }

    #[test]
    fn semantic_tokens_classify_parameter_body_references_as_parameters() {
        let text = "f := x -> x";
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);

        let tokens = collect_tokens(&document, &builtins, false);

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
    fn typed_parameter_references_remain_parameters() {
        let text = "f ZZ := x -> x";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);

        let tokens = collect_tokens(&document, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Type as u32,
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
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

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
    fn semantic_tokens_color_backtick_mentions_as_properties() {
        let text = "x := 1\n-- use `x` and `ideal`\n";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        let local = token_at(&tokens, 1, 8).expect("local documentation reference is tokenized");
        assert_eq!(local.token_type, M2SemanticTokenType::Property as u32);
        assert_eq!(local.token_modifiers_bitset, 0);

        let builtin =
            token_at(&tokens, 1, 16).expect("builtin documentation reference is tokenized");
        assert_eq!(builtin.token_type, M2SemanticTokenType::Property as u32);
        assert_eq!(builtin.token_modifiers_bitset, 0);

        assert_eq!(
            token_at(&tokens, 1, 7).map(|token| token.token_type),
            Some(M2SemanticTokenType::Comment as u32),
            "the backtick delimiter remains comment-colored"
        );
    }

    #[test]
    fn semantic_tokens_keep_backtick_mentions_when_augmenting_syntax() {
        let text = "x := 1\n-- use `x`\n";
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);

        assert_eq!(
            token_at(&tokens, 1, 8).map(|token| token.token_type),
            Some(M2SemanticTokenType::Property as u32)
        );
        assert!(
            token_at(&tokens, 1, 7).is_none(),
            "syntax highlighting owns the surrounding comment"
        );
    }

    #[test]
    fn semantic_tokens_use_one_property_color_for_complete_comment_code() {
        let text = concat!(
            "-- inspect `instance(t, Comment)`\n",
            "Comment = new Type of HashTable\n",
            "t := 1\n",
            "instance := x -> x\n",
        );
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);
        let line = text.lines().next().unwrap();

        assert_eq!(
            token_type_at(&tokens, 0, line.find("instance").unwrap() as u32),
            Some(M2SemanticTokenType::Property as u32)
        );
        assert_eq!(
            token_type_at(&tokens, 0, line.find("(t").unwrap() as u32 + 1),
            Some(M2SemanticTokenType::Property as u32)
        );
        assert_eq!(
            token_type_at(&tokens, 0, line.find("Comment").unwrap() as u32),
            Some(M2SemanticTokenType::Property as u32)
        );
    }

    #[test]
    fn comment_code_does_not_create_document_bindings() {
        let text = "-- example `ghost := x -> x`\nghost\n";
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);
        let comment_line = text.lines().next().unwrap();

        assert_eq!(
            token_type_at(&tokens, 0, comment_line.find("ghost").unwrap() as u32),
            Some(M2SemanticTokenType::Property as u32)
        );
        assert_eq!(
            token_type_at(&tokens, 1, 0),
            None,
            "the isolated snippet assignment must not bind the real document"
        );
    }

    #[test]
    fn comment_code_uses_one_property_color_when_augmenting() {
        let text = "-- example `if true then 1 + 2 else \"x\"`\n";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);
        let line = text.lines().next().unwrap();

        for fragment in ["if", "true", "then", "1", "+", "2", "else", "\"x\""] {
            assert_eq!(
                token_type_at(&tokens, 0, line.find(fragment).unwrap() as u32),
                Some(M2SemanticTokenType::Property as u32)
            );
        }
    }

    #[test]
    fn semantic_tokens_classify_binding_qualifiers_as_modifiers() {
        let text = "global x\nlocal y\nsymbol z\nthreadLocal w\nthreadVariable q";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

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
    fn semantic_tokens_classify_debug_keywords() {
        let text = "step 1\nfinish 2";
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);

        let tokens = collect_tokens(&document, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Keyword as u32,
                M2SemanticTokenType::Number as u32,
                M2SemanticTokenType::Keyword as u32,
                M2SemanticTokenType::Number as u32,
            ]
        );
    }

    #[test]
    fn semantic_tokens_do_not_classify_booleans_as_keywords() {
        let text = "if true then false else true";
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

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
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

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
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

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
                M2SemanticTokenType::Namespace as u32, // loadPackage "Graphs"
                M2SemanticTokenType::Namespace as u32, // installPackage("Normaliz")
                M2SemanticTokenType::Namespace as u32, // uninstallPackage "Foo"
                M2SemanticTokenType::Namespace as u32, // needsPackage "Core"
                M2SemanticTokenType::String as u32,    // export "thing" — a symbol name
                M2SemanticTokenType::Namespace as u32, // endPackage "Pkg"
                M2SemanticTokenType::Namespace as u32, // newPackage("Pkg")
                M2SemanticTokenType::Namespace as u32, // importFrom("Pkg") — first arg
                M2SemanticTokenType::String as u32,    // exportFrom {"Pkg"} — list element
                M2SemanticTokenType::String as u32,    // print "ordinary"
            ]
        );
    }

    #[test]
    fn namespace_argument_selection_skips_muted_parts_but_not_null_slots() {
        let text = concat!(
            "loadPackage(ignored;\"AfterMuted\")\n",
            "loadPackage(,\"AfterNull\")\n",
            "loadPackage(\"Muted\";)\n",
        );
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

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
                M2SemanticTokenType::String as u32,
                M2SemanticTokenType::String as u32,
            ]
        );
    }

    #[test]
    fn semantic_tokens_keep_exported_symbol_names_as_strings() {
        // `export`/`exportMutable` arguments name symbols defined in this package,
        // not modules. For `importFrom`/`exportFrom` only the first argument (the
        // package) is a namespace; the imported/exported symbol names are not.
        let text = concat!(
            "export {\"installMacro\", \"expandSource\"}\n",
            "exportMutable {\"state\"}\n",
            "importFrom(\"Core\", {\"first\", \"second\"})\n",
            "exportFrom(\"Pkg\", \"only\")\n",
        );
        let builtins = BuiltinData::empty();

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

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
                M2SemanticTokenType::String as u32,    // "installMacro"
                M2SemanticTokenType::String as u32,    // "expandSource"
                M2SemanticTokenType::String as u32,    // "state"
                M2SemanticTokenType::Namespace as u32, // importFrom "Core" — the package
                M2SemanticTokenType::String as u32,    // "first" — imported symbol
                M2SemanticTokenType::String as u32,    // "second" — imported symbol
                M2SemanticTokenType::Namespace as u32, // exportFrom "Pkg" — the package
                M2SemanticTokenType::String as u32,    // "only" — exported symbol
            ]
        );
    }

    #[test]
    fn semantic_tokens_classify_string_hash_keys_as_properties() {
        // The string on the left of `=>` is a hash key (property); the value on
        // the right stays an ordinary string.
        let text = concat!(
            "h = new HashTable from {\n",
            "    \"Quote\" => \"symbol\",\n",
            "    \"GlobalQuote\" => \"global\"\n",
            "}\n",
        );
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .filter(|token_type| {
                    *token_type == M2SemanticTokenType::Property as u32
                        || *token_type == M2SemanticTokenType::String as u32
                })
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Property as u32, // "Quote"
                M2SemanticTokenType::String as u32,   // "symbol"
                M2SemanticTokenType::Property as u32, // "GlobalQuote"
                M2SemanticTokenType::String as u32,   // "global"
            ]
        );
    }

    #[test]
    fn semantic_tokens_classify_lookup_string_keys_as_properties() {
        // A string on the right of `#` / `#?` is a literal key (property). A
        // symbol key (`h#k`) is evaluated, so it stays an ordinary reference.
        let text = "a = h#\"first\"\nb = h#?\"second\"\nc = \"plain\"\n";
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        assert_eq!(
            tokens
                .iter()
                .map(|token| token.token_type)
                .filter(|token_type| {
                    *token_type == M2SemanticTokenType::Property as u32
                        || *token_type == M2SemanticTokenType::String as u32
                })
                .collect::<Vec<_>>(),
            vec![
                M2SemanticTokenType::Property as u32, // h#"first"
                M2SemanticTokenType::Property as u32, // h#?"second"
                M2SemanticTokenType::String as u32,   // "plain"
            ]
        );
    }

    #[test]
    fn semantic_tokens_classify_dot_access_keys_as_properties() {
        // `name` is also a global variable, yet the quoted global key in `R.name`
        // and `R.?name` must still win as a property over any other role.
        let text = "name = 5\nx = R.name\ny = R.?name\n";
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        let property_count = tokens
            .iter()
            .filter(|token| token.token_type == M2SemanticTokenType::Property as u32)
            .count();
        // Exactly the two dot-access keys, not the `name = 5` binding on line 1.
        assert_eq!(property_count, 2);
    }

    #[test]
    fn string_valued_locals_remain_variables() {
        let text = "s := 1\nt := toString s\nt\n";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);

        let tokens = collect_tokens(&document, &builtins, true);

        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.token_type == M2SemanticTokenType::String as u32)
                .count(),
            0,
            "ordinary locals should not be recolored as string literals from inferred type"
        );
    }

    #[test]
    fn default_library_commands_use_method_while_local_command_values_stay_callable() {
        let text = "saveClearAll := clearAll\nclearAll = new Command from { () -> () }\nprotect symbol clearAll";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);

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
            3,
            "aliased and locally rebound Command values should keep a standard callable base"
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|token| {
                    token.token_type == M2SemanticTokenType::Method as u32
                        && token.token_modifiers_bitset & COMMAND_MODIFIER == COMMAND_MODIFIER
                        && token.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER
                            == DEFAULT_LIBRARY_MODIFIER
                })
                .count(),
            1,
            "the direct default-library command should use method+defaultLibrary"
        );
    }

    #[test]
    fn semantic_tokens_merge_manipulators_into_command_modifier() {
        let text = "endl";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);
        let token = token_at(&tokens, 0, 0).expect("endl should have a semantic token");

        assert_eq!(token.token_type, M2SemanticTokenType::Operator as u32);
        assert_eq!(
            token.token_modifiers_bitset & COMMAND_MODIFIER,
            COMMAND_MODIFIER,
            "M2 Manipulator values share the command palette role"
        );
        assert_eq!(
            token.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER,
            DEFAULT_LIBRARY_MODIFIER
        );
    }

    #[test]
    fn semantic_tokens_preserve_file_modifier_alongside_default_library() {
        let text = "stdio";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);
        let token = token_at(&tokens, 0, 0).expect("stdio should have a semantic token");

        assert_eq!(token.token_type, M2SemanticTokenType::Variable as u32);
        assert_eq!(token.token_modifiers_bitset & FILE_MODIFIER, FILE_MODIFIER);
        assert_eq!(
            token.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER,
            DEFAULT_LIBRARY_MODIFIER
        );
    }

    #[test]
    fn indexed_objects_use_variable_plus_default_library_without_tainting_locals() {
        let text = "true\nlocalValue := true\nlocalValue";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, true);

        let indexed = token_at(&tokens, 0, 0).expect("indexed true should be highlighted");
        assert_eq!(indexed.token_type, M2SemanticTokenType::Variable as u32);
        assert_eq!(
            indexed.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER,
            DEFAULT_LIBRARY_MODIFIER
        );

        let declaration = token_at(&tokens, 1, 0).expect("local declaration should be highlighted");
        assert_eq!(
            declaration.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER,
            0,
            "a local binding must not inherit provenance from its indexed value"
        );
        let reference = token_at(&tokens, 2, 0).expect("local reference should be highlighted");
        assert_eq!(
            reference.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER,
            0
        );
    }

    #[test]
    fn imported_index_tokens_use_scoped_role_without_core_provenance() {
        let corpus = concat!(
            "{\"kind\":\"meta\",\"default_loaded\":[\"Core\"]}\n",
            "{\"kind\":\"type\",\"name\":\"MethodFunction\",",
            "\"package\":\"$Core$Core\",\"class\":\"$Core$Type\",",
            "\"parent\":\"$Core$Function\",\"ancestors\":[\"$Core$Function\",\"$Core$Thing\"]}\n",
            "{\"kind\":\"methodFunction\",\"name\":\"pkgFn\",",
            "\"package\":\"$Pkg$Pkg\",\"class\":\"$Core$MethodFunction\",",
            "\"methods\":[{\"domain\":[],\"typicalValue\":null}]}\n",
        );
        let provider = PackagePartitionedIndex::from_corpus(corpus);
        let text = "needsPackage \"Pkg\"\npkgFn";
        let document =
            DocumentSnapshot::from_text(text.to_string(), &provider).expect("fixture should parse");
        let scoped = provider.knowledge_for(document.imported_packages());
        let workspace_index = WorkspaceIndex::default();
        let uri = Url::parse("file:///fixture.m2").expect("valid fixture uri");

        let tokens = collect_semantic_tokens(&document, &scoped, &workspace_index, &uri, true);
        let token = token_at(&tokens, 1, 0).expect("imported pkgFn should be highlighted");
        assert_eq!(token.token_type, M2SemanticTokenType::Method as u32);
        assert_eq!(token.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER, 0);
        assert_eq!(token.token_modifiers_bitset & BUILTIN_MODIFIER, 0);

        let document = DocumentSnapshot::from_text("pkgFn".to_string(), &provider)
            .expect("unimported fixture should parse");
        let scoped = provider.knowledge_for(document.imported_packages());
        let tokens = collect_semantic_tokens(&document, &scoped, &workspace_index, &uri, true);
        assert!(
            token_at(&tokens, 0, 0).is_none(),
            "the same package object must disappear when its import is absent"
        );
    }

    #[test]
    fn parameter_references_use_parameter_semantic_token_type() {
        let symbol = crate::meta::Meta {
            symbol_kind: Some(SymbolKind::VARIABLE),
            binding_role: Some(BindingRole::Parameter),
            ..crate::meta::Meta::default()
        };
        let builtins = BuiltinData::empty();

        assert_eq!(
            local_symbol_semantic_token_type(&symbol, &builtins),
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
            provenance: M2SemanticTokenProvenance::None,
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
            provenance: M2SemanticTokenProvenance::None,
        };

        let modifiers = builtin_semantic_token_modifiers(&token);

        assert_eq!(modifiers, 0);
    }

    #[test]
    fn builtin_function_role_does_not_bake_in_provenance_modifier() {
        let token = M2SemanticToken {
            token_type: M2SemanticTokenType::Function,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
            provenance: M2SemanticTokenProvenance::None,
        };

        let modifiers = builtin_semantic_token_modifiers(&token);

        assert_eq!(modifiers, 0);
    }

    #[test]
    fn builtin_constructor_like_names_do_not_emit_constructor_modifier() {
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let token = builtins
            .get_semantic_token("toString")
            .expect("toString should have builtin metadata");

        assert_eq!(token.token_type, M2SemanticTokenType::Method);
        assert_eq!(builtin_semantic_token_modifiers(&token), 0);
    }

    #[test]
    fn option_keys_classify_by_protected_symbol() {
        // The key of a `k => v` pair is classified by whether it is a protected
        // symbol: `Strategy` (a protected class-`Symbol` builtin) is a nominal
        // enum member, while `myKey` (an unprotected user name) is a field. The
        // value `7` is not a symbol, so it is not classified here.
        let text = "f(x, Strategy => 4, myKey => 7)";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let root = M2Node::new(tree.root_node(), text);

        let mut roles = Vec::new();
        for node in root.descendants() {
            if node.kind == NodeKind::Symbol {
                roles.push((node.text(), option_assignment_role(node, &builtins)));
            }
        }

        assert!(roles.contains(&("Strategy", Some(M2SemanticTokenType::EnumMember))));
        assert!(roles.contains(&("myKey", Some(M2SemanticTokenType::Property))));
    }

    #[test]
    fn option_keys_keep_option_and_builtin_provenance_modifiers() {
        let text = "f(x, Strategy => 4, custom => 7)";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        let builtin = token_at(
            &tokens,
            0,
            text.find("Strategy").expect("fixture contains Strategy") as u32,
        )
        .expect("builtin option key is highlighted");
        assert_eq!(
            builtin.token_modifiers_bitset & OPTION_MODIFIER,
            OPTION_MODIFIER
        );
        assert_eq!(
            builtin.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER,
            DEFAULT_LIBRARY_MODIFIER
        );

        let custom = token_at(
            &tokens,
            0,
            text.find("custom").expect("fixture contains custom") as u32,
        )
        .expect("custom option key is highlighted");
        assert_eq!(
            custom.token_modifiers_bitset & OPTION_MODIFIER,
            OPTION_MODIFIER
        );
        assert_eq!(custom.token_modifiers_bitset & DEFAULT_LIBRARY_MODIFIER, 0);
    }

    #[test]
    fn local_classes_use_class_tokens_and_binding_sites_are_declarations() {
        let text = "TokenStream = new Type of List\nTokenStream\n";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        let declaration = token_at(&tokens, 0, 0).expect("class declaration is highlighted");
        assert_eq!(declaration.token_type, M2SemanticTokenType::Class as u32);
        assert_eq!(
            declaration.token_modifiers_bitset & DECLARATION_MODIFIER,
            DECLARATION_MODIFIER
        );

        let reference = token_at(&tokens, 1, 0).expect("class reference is highlighted");
        assert_eq!(reference.token_type, M2SemanticTokenType::Class as u32);
        assert_eq!(reference.token_modifiers_bitset & DECLARATION_MODIFIER, 0);
    }

    #[test]
    fn semantic_tokens_never_emit_zero_length_entries() {
        let text = "f ZZ := x -> x";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        assert!(tokens.iter().all(|token| token.length > 0));
    }

    #[test]
    fn algebraic_values_use_general_standard_roles_without_class_modifiers() {
        let text = concat!(
            "R = QQ[x,y]\n",
            "I = ideal(x^2,y)\n",
            "Q = R/I\n",
            "M = Q^2\n",
            "x\n",
        );
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        for (line, token_type) in [
            (0, M2SemanticTokenType::Type),
            (1, M2SemanticTokenType::Variable),
            (2, M2SemanticTokenType::Type),
            (3, M2SemanticTokenType::Variable),
            (4, M2SemanticTokenType::Variable),
        ] {
            let token = token_at(&tokens, line, 0)
                .unwrap_or_else(|| panic!("line {line} should carry an algebraic token"));
            assert_eq!(token.token_type, token_type as u32, "line {line}");
            assert_eq!(token.token_modifiers_bitset & !DECLARATION_MODIFIER, 0);
        }
    }

    #[test]
    fn algebraic_class_names_remain_standard_class_tokens() {
        let text = "PolynomialRing\nIdeal\nRingElement\n";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        for line in 0..=2 {
            let token = token_at(&tokens, line, 0).expect("class name should be highlighted");
            assert_eq!(token.token_type, M2SemanticTokenType::Class as u32);
            assert_eq!(
                token.token_modifiers_bitset, DEFAULT_LIBRARY_MODIFIER,
                "class names should retain only their indexed provenance"
            );
        }
    }

    #[test]
    fn procedural_macro_names_use_method_plus_macro_without_parse_errors() {
        let text = concat!(
            "x = $outer $inner 1 $ $\n",
            "y = 2\n",
            "message = \"$fake 3 $\"\n",
        );
        let builtins = BuiltinData::empty();
        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        assert!(
            document.diagnostics().is_empty(),
            "matched macro syntax should be parsed through its masked sigils: {:?}",
            document.diagnostics()
        );
        for character in [5, 12] {
            let token = token_at(&tokens, 0, character)
                .unwrap_or_else(|| panic!("macro name at {character} should be highlighted"));
            assert_eq!(token.token_type, M2SemanticTokenType::Method as u32);
            assert_eq!(
                token.token_modifiers_bitset & MACRO_MODIFIER,
                MACRO_MODIFIER
            );
        }
        assert!(
            document
                .analysis()
                .get_binding_at("y", Position::new(1, 0))
                .is_some(),
            "source after a macro invocation should remain visible to analysis"
        );
        let fake = text.lines().nth(2).unwrap().find("fake").unwrap() as u32;
        assert_eq!(
            token_at(&tokens, 2, fake).map(|token| token.token_type),
            Some(M2SemanticTokenType::String as u32),
            "macro-like text inside a string stays ordinary string syntax"
        );
    }

    #[test]
    fn semantic_token_modifier_bits_match_legend_order() {
        assert_eq!(
            LEGEND_MODIFIERS,
            &[
                SemanticTokenModifier::new("option"),
                SemanticTokenModifier::new("command"),
                SemanticTokenModifier::new("file"),
                SemanticTokenModifier::DECLARATION,
                SemanticTokenModifier::DEFAULT_LIBRARY,
                SemanticTokenModifier::new("builtin"),
                SemanticTokenModifier::new("macro"),
            ]
        );
        assert_eq!(OPTION_MODIFIER, 1 << 0);
        assert_eq!(COMMAND_MODIFIER, 1 << 1);
        assert_eq!(FILE_MODIFIER, 1 << 2);
        assert_eq!(DECLARATION_MODIFIER, 1 << 3);
        assert_eq!(DEFAULT_LIBRARY_MODIFIER, 1 << 4);
        assert_eq!(BUILTIN_MODIFIER, 1 << 5);
        assert_eq!(MACRO_MODIFIER, 1 << 6);
    }

    #[test]
    fn core_default_library_and_compiled_builtin_modifiers_are_disjoint() {
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));

        for name in ["ZZ", "true", "ideal", "stdio"] {
            let token = builtins
                .get_semantic_token(name)
                .unwrap_or_else(|| panic!("{name} should have semantic metadata"));
            let modifiers = indexed_semantic_token_modifiers(&token);
            assert_eq!(
                modifiers & DEFAULT_LIBRARY_MODIFIER,
                DEFAULT_LIBRARY_MODIFIER,
                "{name} should be a Core default-library object"
            );
            assert_eq!(
                modifiers & BUILTIN_MODIFIER,
                0,
                "{name} should not carry the compiled builtin modifier"
            );
        }

        let scan = builtins
            .get_semantic_token("scan")
            .expect("scan should have semantic metadata");
        let modifiers = indexed_semantic_token_modifiers(&scan);
        assert_eq!(modifiers & BUILTIN_MODIFIER, BUILTIN_MODIFIER);
        assert_eq!(
            modifiers & DEFAULT_LIBRARY_MODIFIER,
            0,
            "compiled builtins must not also carry defaultLibrary"
        );
    }

    #[test]
    fn method_installation_domain_emits_type_for_known_types() {
        let text = "Ring Element := x -> x";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);
        let token_types: Vec<u32> = tokens.iter().map(|t| t.token_type).collect();

        let type_param = M2SemanticTokenType::Type as u32;
        assert!(
            token_types.contains(&type_param),
            "Ring in method installation should be Type, got {:?}",
            token_types
        );
    }

    #[test]
    fn explicit_method_codomain_outranks_option_field_classification() {
        let text = "\
p = method(TypicalValue => List)
p(ZZ) := Array => x -> [x]
";
        let builtins = BuiltinData::load_from_index(include_str!("../data/m2-index.jsonl"));

        let document = document(text, &builtins);
        let tokens = collect_tokens(&document, &builtins, false);

        assert_eq!(
            token_type_at(&tokens, 1, 9),
            Some(M2SemanticTokenType::Type as u32),
            "the explicit Array codomain is a type role, not an option field"
        );
    }
}
