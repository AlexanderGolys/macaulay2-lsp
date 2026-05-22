use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tree_sitter::Parser;
use typesystem::{BuiltinData, M2SemanticTokenType};

mod analysis;
mod typesystem;

use analysis::Analysis;

// @@@tag.a

#[derive(Debug)]
struct Backend {
    client: Client,
    builtins: BuiltinData,
    documents: DashMap<Url, String>,
    analyses: DashMap<Url, Analysis>,
}

const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::TYPE,           // 0
    SemanticTokenType::FUNCTION,       // 1
    SemanticTokenType::VARIABLE,       // 2
    SemanticTokenType::PARAMETER,      // 3
    SemanticTokenType::PROPERTY,       // 4
    SemanticTokenType::NAMESPACE,      // 5
    SemanticTokenType::new("file"),    // 6
    SemanticTokenType::new("command"), // 7
];

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

impl Backend {
    fn new(client: Client) -> Self {
        let builtin_names = include_str!("./data/builtins.names");
        let builtin_details = include_str!("./data/builtins.details.jsonl");
        let builtins = BuiltinData::load_from_split(builtin_names, builtin_details);
        Backend {
            client,
            builtins,
            documents: DashMap::new(),
            analyses: DashMap::new(),
        }
    }

    async fn on_change(&self, params: TextDocumentItem) {
        let uri = params.uri.clone();
        self.documents.insert(uri.clone(), params.text.clone());

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        if let Some(tree) = parser.parse(&params.text, None) {
            let analysis = Analysis::new(&tree, &params.text);
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
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: LEGEND_TYPES.into(),
                                token_modifiers: vec![
                                    SemanticTokenModifier::new("option"),
                                    SemanticTokenModifier::new("builtin"),
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

            if self.builtins.contains_name(node_text) {
                let Some(record) = self
                    .builtins
                    .get_record(&typesystem::InstanceID(node_text.to_string()))
                else {
                    return Ok(None);
                };
                let mut markdown = format!("**{}**\n\n", record.name);
                markdown.push_str(&format!("Type: `{}`\n\n", record.data_type.0));

                if let Some(desc) = &record.description_short {
                    markdown.push_str(&format!("{}\n\n", desc));
                }

                if let Some(val) = record.extra.get("typical_value") {
                    markdown.push_str(&format!("Typical Value: `{}`\n\n", val));
                }

                if let Some(func_info) = &record.function_info {
                    markdown.push_str("**Installed Methods:**\n");
                    for method in func_info.methods.iter().take(5) {
                        markdown.push_str(&format!(
                            "- `({})` \n",
                            method
                                .signature
                                .iter()
                                .map(|s| s.0.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if func_info.methods.len() > 5 {
                        markdown.push_str("- ...\n");
                    }
                }

                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: markdown,
                    }),
                    range: None,
                }));
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

        let items = self
            .builtins
            .names_with_prefix(&prefix, 80)
            .into_iter()
            .map(|name| CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                ..Default::default()
            })
            .collect();

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

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();
        let root_node = tree.root_node();

        let mut tokens = Vec::new();
        let mut cursor = root_node.walk();
        let mut prev_line = 0;
        let mut prev_start = 0;
        let mut reached_root = false;

        while !reached_root {
            let node = cursor.node();
            let kind = node.kind();

            if kind == "symbol"
                || kind == "identifier"
                || kind == "resolved_symbol"
                || kind == "builtin_constant"
            {
                let start_byte = node.start_byte();
                let end_byte = node.end_byte();
                let node_text = &text[start_byte..end_byte];
                let start_pos = node.start_position();
                let line_start_byte = start_byte.saturating_sub(start_pos.column);
                let start_char = utf16_len_for_byte_span(&text, line_start_byte, start_byte);
                let position = Position::new(start_pos.row as u32, start_char);

                let mut token_type: Option<u32> = None;
                let mut modifiers: u32 = 0;

                // 1. Check local analysis first
                if let Some(analysis) = self.analyses.get(&uri) {
                    if let Some(symbol_kind) = analysis.get_symbol_kind_at(node_text, position) {
                        token_type = match symbol_kind {
                            analysis::SymbolKind::Variable => {
                                Some(M2SemanticTokenType::Variable as u32)
                            }
                            analysis::SymbolKind::Parameter => {
                                Some(M2SemanticTokenType::Parameter as u32)
                            }
                        };
                    }
                }

                // 2. Fallback to builtins
                if token_type.is_none() {
                    let result = self.builtins.get_token_index(node_text);
                    if let Some(t) = result {
                        token_type = Some(t as u32);
                        modifiers |= 2; // RESERVED
                    }
                }

                // 3. Check if it's a key (left side of =>)
                if let Some(parent) = node.parent() {
                    if parent.kind() == "option_assignment" {
                        if let Some(left) = parent.child_by_field_name("left") {
                            if left.id() == node.id() {
                                modifiers |= 1; // KEY
                            }
                        }
                    }
                }

                if let Some(token_type) = token_type {
                    let line = start_pos.row as u32;
                    let length = utf16_len_for_byte_span(&text, start_byte, end_byte);

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

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
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
        }

        Ok(None)
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend::new(client));
    Server::new(stdin, stdout, socket).serve(service).await;
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
}
