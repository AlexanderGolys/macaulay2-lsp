use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use typesystem::BuiltinData;

mod analysis;
mod capabilities;
mod document;
mod package_index;
mod record_lsp;
mod typesystem;
mod util;

use capabilities::code_actions::available_code_actions;
use capabilities::diagnostics::publish_diagnostics;
use capabilities::document_symbols::document_symbol_response;
use capabilities::formatting::{
    document_formatting_provider_capability, document_formatting_text_edits,
    folding_range_provider_capability, folding_ranges,
};
use capabilities::hover::hover_response;
use capabilities::navigation::{
    completion_response, goto_definition_response, references_response, workspace_symbols_response,
};
use capabilities::semantic_tokens::{semantic_tokens_response, LEGEND_TYPES};
use capabilities::type_hierarchy::{TypeHierarchyCapabilityService, TYPE_HIERARCHY_METHOD};
use document::DocumentSnapshot;
#[cfg(test)]
use package_index::extractor_script_candidates;
use package_index::{collect_imported_packages, PackageIndexer, SourceResolver};
#[cfg(test)]
use package_index::package_source_string;
#[cfg(test)]
use record_lsp::record_package;
use record_lsp::{record_source_file, record_source_line, record_symbol_kind};

#[derive(Debug)]
struct Backend {
    client: Client,
    builtins: BuiltinData,
    source_resolver: SourceResolver,
    package_indexer: PackageIndexer,
    package_indexes: DashMap<String, BuiltinData>,
    documents: DashMap<Url, DocumentSnapshot>,
    semantic_tokens_augment_syntax: AtomicBool,
    type_hierarchy_dynamic_registration: AtomicBool,
}

impl Backend {
    fn new(client: Client) -> Self {
        let builtin_names = include_str!("./data/builtins.names");
        let builtin_details = include_str!("./data/builtins.details.jsonl");
        let type_facts = include_str!("./data/type_facts.jsonl");
        let builtins = BuiltinData::load_from_split_with_type_facts(
            builtin_names,
            builtin_details,
            type_facts,
        );
        Backend {
            client,
            builtins,
            source_resolver: SourceResolver::from_environment(),
            package_indexer: PackageIndexer::from_environment(),
            package_indexes: DashMap::new(),
            documents: DashMap::new(),
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

    async fn on_open(&self, params: TextDocumentItem) {
        let Some(document) = DocumentSnapshot::from_text(params.text, &self.builtins) else {
            return;
        };
        let uri = params.uri;
        let _ = self.active_package_indexes(document.text());
        self.documents.insert(uri.clone(), document);
        if let Some(document) = self.documents.get(&uri) {
            publish_diagnostics(&self.client, uri, document.value()).await;
        }
    }

    async fn on_change(&self, uri: Url, changes: Vec<TextDocumentContentChangeEvent>) {
        if let Some(mut document) = self.documents.get_mut(&uri) {
            if document.apply_changes(&changes, &self.builtins).is_none() {
                return;
            }
            let _ = self.active_package_indexes(document.text());
            publish_diagnostics(&self.client, uri, document.value()).await;
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
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
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_formatting_provider: document_formatting_provider_capability(),
                folding_range_provider: folding_range_provider_capability(),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
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
            && self
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

    async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_open(TextDocumentItem {
            uri: params.text_document.uri,
            language_id: "macaulay2".to_string(),
            version: params.text_document.version,
            text: params.text_document.text,
        })
        .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.on_change(params.text_document.uri, params.content_changes)
            .await;
    }

    async fn hover(&self, params: HoverParams) -> tower_lsp::jsonrpc::Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let active_package_indexes = self.active_package_indexes(document.text());
        Ok(hover_response(
            document.value(),
            position,
            &self.builtins,
            &active_package_indexes,
        ))
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let active_package_indexes = self.active_package_indexes(document.text());
        Ok(completion_response(
            document.text(),
            position,
            &self.builtins,
            &active_package_indexes,
        ))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> tower_lsp::jsonrpc::Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };

        let augments_syntax_tokens = self.semantic_tokens_augment_syntax.load(Ordering::Relaxed);
        Ok(Some(semantic_tokens_response(
            document.value(),
            &self.builtins,
            augments_syntax_tokens,
        )))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> tower_lsp::jsonrpc::Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        Ok(Some(document_symbol_response(document.value(), &self.builtins)))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        Ok(available_code_actions(document.value(), &uri, params.range.start))
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        Ok(Some(references_response(
            document.value(),
            uri,
            position,
            params.context.include_declaration,
        )))
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<TypeHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let Some(node) = document.symbol_node_at_position(position) else {
            return Ok(None);
        };
        let name = document.text_for(node);
        let range = document.range_for(node);

        for (package, package_index) in self.active_package_indexes(document.text()) {
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
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<TypeHierarchyItem>>> {
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
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<TypeHierarchyItem>>> {
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

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        Ok(Some(document_formatting_text_edits(
            document.text(),
            params.options.tab_size,
            params.options.insert_spaces,
        )))
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };

        Ok(Some(folding_ranges(document.text())))
    }

    #[allow(deprecated)]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.trim();
        if query.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let loaded_package_indexes = self
            .package_indexes
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect::<Vec<_>>();
        Ok(Some(workspace_symbols_response(
            query,
            &loaded_package_indexes,
            &self.builtins,
            |record| self.record_location(record),
        )))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let active_package_indexes = self.active_package_indexes(document.text());
        Ok(goto_definition_response(
            document.value(),
            uri,
            position,
            &self.builtins,
            &active_package_indexes,
            &self.source_resolver,
            |record| self.record_location(record),
        ))
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
    use crate::analysis::Analysis;
    use crate::record_lsp::{record_hover_with_package, record_hover_with_package_and_usage};
    use crate::typesystem::BuiltinData;
    use tree_sitter::Parser;

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

        assert!(diagnostic_lines.is_empty());
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
    fn record_hover_includes_option_value_reverse_usage() {
        let builtins = BuiltinData::load_from_split_with_type_facts(
            "LongPolynomial\n",
            "{\"name\":\"LongPolynomial\",\"data_type\":\"Symbol\",\"description_short\":\"a Strategy option value\",\"description_long\":null,\"examples\":[],\"extra\":{}}\n",
            "{\"callable\":\"gb\",\"options\":[{\"key\":\"Strategy\",\"values\":[\"LongPolynomial\"]}]}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("LongPolynomial"))
            .expect("option value should have metadata");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("Option Role: `value`"));
        assert!(markup.value.contains("`gb.Strategy`"));
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
            markup.value.contains("`RingMap -> Ideal`"),
            "record hover should display documented method codomains"
        );
        assert!(
            markup.value.contains("```macaulay2\nR = QQ[a..d];"),
            "record hover should display saved examples"
        );
    }

    #[test]
    fn record_hover_includes_global_typical_value() {
        let builtins = BuiltinData::load_from_split(
            "method\n",
            "{\"name\":\"method\",\"data_type\":\"FunctionClosure\",\"description_short\":\"make a new method function\",\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[],\"general_signature\":{\"signature\":[\"method\"],\"output_types\":[\"MethodFunction\"]}}}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("method"))
            .expect("method should deserialize");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("Typical Value: `MethodFunction`"));
    }

    #[test]
    fn record_hover_omits_documented_signatures_from_installed_methods() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("ring"))
            .expect("ring should have builtin metadata");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(
            markup.value.contains("`Ideal -> Ring`"),
            "record hover should display documented domain-to-codomain signatures"
        );
        assert!(
            markup.value.contains("`ChainComplex -> Ring`"),
            "record hover should display domains inheriting the general codomain"
        );
        assert!(
            !markup.value.contains("`(ring, Ideal)`"),
            "record hover should not repeat documented domains as installed-only methods"
        );
    }

    #[test]
    fn record_hover_shows_operator_method_signatures() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("+"))
            .expect("+ should have operator metadata");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(
            markup.value.contains("**Installed Methods:**")
                && markup.value.contains("`Matrix + Matrix`"),
            "operator hover should show method signatures"
        );
    }

    #[test]
    fn record_hover_renders_operator_documented_signatures_in_operator_form() {
        let builtins = BuiltinData::load_from_split(
            "=>\n",
            "{\"name\":\"=>\",\"data_type\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"operator_info\":{\"attributes\":{\"Binary\":[]},\"flags\":{\"Binary\":[]},\"flexible\":false,\"forms\":[\"Binary\"],\"method_lookup\":\"symbol\",\"method_symbol\":\"=>\"},\"function_info\":{\"methods\":[{\"signature\":[\"=>\",\"Thing\",\"Thing\"]}],\"documented_methods\":[{\"signature\":[\"=>\",\"Thing\",\"Thing\"],\"output_types\":[\"Option\"]}]}}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("=>"))
            .expect("=> should have operator metadata");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("`Thing => Thing -> Option`"));
        assert!(!markup.value.contains("`Thing, Thing -> Option`"));
    }

    #[test]
    fn record_hover_renders_operator_assignment_signatures_in_operator_form() {
        let builtins = BuiltinData::load_from_split(
            "+\n",
            "{\"name\":\"+\",\"data_type\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"operator_info\":{\"attributes\":{\"Binary\":[\"Flexible\"]},\"flags\":{\"Binary\":[\"Flexible\"]},\"flexible\":true,\"forms\":[\"Binary\"],\"method_lookup\":\"symbol\",\"method_symbol\":\"+\"},\"function_info\":{\"methods\":[{\"signature\":[\"(+,=)\",\"Thing\",\"Thing\"]}]}}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("+"))
            .expect("+ should have operator metadata");

        let hover = record_hover_with_package(&record, Some("Core"), &builtins);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("`Thing + Thing = ...`"));
        assert!(!markup.value.contains("`(+,=), Thing, Thing`"));
    }

    #[test]
    fn record_hover_can_focus_on_specialized_call_signature() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("openOut"))
            .expect("openOut should have builtin metadata");
        let usage = builtins
            .resolve_call_signature_usage("openOut", &[Some("String".to_string())])
            .expect("openOut String should resolve to a documented installation");

        let hover =
            record_hover_with_package_and_usage(&record, Some("Core"), &builtins, Some(&usage));
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("**Signature:**"));
        assert!(markup.value.contains("`String -> File`"));
        assert!(markup
            .value
            .contains("(`CompiledFunction`) **openOut**\t `String -> File`"));
        assert!(markup.value.contains("Documentation: `openOut(String)`"));
        assert!(!markup.value.contains("**Documented Signatures:**"));
    }

    #[test]
    fn record_hover_keeps_excluded_signatures_when_usage_is_pinned() {
        let builtins = BuiltinData::load_from_split(
            "f\n",
            "{\"name\":\"f\",\"data_type\":\"MethodFunction\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"String\"]},{\"signature\":[\"f\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"String\"],\"output_types\":[\"File\"]},{\"signature\":[\"f\",\"ZZ\"],\"output_types\":[\"Thing\"]}]}}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("f"))
            .expect("f should have builtin metadata");
        let usage = builtins
            .resolve_call_signature_usage("f", &[Some("String".to_string())])
            .expect("f String should resolve to a documented installation");

        let hover =
            record_hover_with_package_and_usage(&record, Some("Core"), &builtins, Some(&usage));
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("**Signature:**"));
        assert!(markup.value.contains("`String -> File`"));
        assert!(markup
            .value
            .contains("**Excluded Signatures For This Usage:**"));
        assert!(markup.value.contains("`ZZ -> Thing`"));
    }

    #[test]
    fn record_hover_can_show_possible_and_excluded_usage_signatures() {
        let builtins = BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new(">>"))
            .expect(">> should have builtin metadata");
        let usage = builtins
            .resolve_call_signature_usage(">>", &[None, Some("Function".to_string())])
            .expect(">> usage should partition signatures");

        let hover =
            record_hover_with_package_and_usage(&record, Some("Core"), &builtins, Some(&usage));
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup
            .value
            .contains("**Possible Signatures For This Usage:**"));
        assert!(markup.value.contains("`OptionTable >> Function`"));
        assert!(markup.value.contains("`List >> Function`"));
        assert!(markup.value.contains("`Boolean >> Function`"));
        assert!(markup
            .value
            .contains("**Excluded Signatures For This Usage:**"));
        assert!(markup.value.contains("`Thing >> Thing`"));
        assert!(markup.value.contains("`ZZ >> ZZ`"));
        assert!(!markup.value.contains("**Installed Methods:**"));
        assert!(!markup.value.contains("`(>>,=), Type, Type`"));
    }
}
