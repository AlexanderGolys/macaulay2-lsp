use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::{io, task};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use typesystem::BuiltinData;

mod analysis;
mod builtin_index;
mod capabilities;
mod document;
mod node_metadata;
mod package_index;
mod partitioned_index;
mod record_lsp;
mod typesystem;
mod util;
mod workspace_index;

use capabilities::code_actions::available_code_actions;
use capabilities::diagnostics::publish_diagnostics;
use capabilities::document_highlight::{
    document_highlight_provider_capability, document_highlights,
};
use capabilities::document_symbols::collect_document_symbols;
use capabilities::formatting::{
    document_formatting_provider_capability, document_formatting_text_edits,
    folding_range_provider_capability, folding_ranges,
};
use capabilities::hover::hover_response;
use capabilities::inlay_hints::{inlay_hint_provider_capability, inlay_hints_response};
use capabilities::navigation::{
    completion_response, global_reference_ranges, goto_definition_response, prepare_rename_range,
    reference_target, references_response, rename_edits, workspace_symbols_response,
    ReferenceTarget,
};
use capabilities::semantic_tokens::{collect_semantic_tokens, LEGEND_TYPES};
use capabilities::type_hierarchy::{TypeHierarchyCapabilityService, TYPE_HIERARCHY_METHOD};
use document::DocumentSnapshot;
#[cfg(test)]
use package_index::package_source_string;
use package_index::{collect_imported_packages, PackageIndexer, SourceResolver};
#[cfg(test)]
use record_lsp::record_package;
use record_lsp::{record_source_file, record_source_line, record_symbol_kind};

use crate::partitioned_index::{LoadedPackages, PackagePartitionedIndex};
use crate::typesystem::{InstanceID, Record};
use crate::workspace_index::WorkspaceIndex;

#[derive(Debug)]
struct Backend {
    client: Client,
    builtins: BuiltinData,
    // Forward-looking P3 field: not yet consulted by query routing.
    #[allow(dead_code)]
    partitioned: PackagePartitionedIndex,
    source_resolver: SourceResolver,
    package_indexer: PackageIndexer,
    package_indexes: DashMap<String, BuiltinData>,
    documents: DashMap<Url, DocumentSnapshot>,
    workspace_index: Arc<WorkspaceIndex>,
    semantic_tokens_augment_syntax: AtomicBool,
    type_hierarchy_dynamic_registration: AtomicBool,
}

impl Backend {
    fn new(client: Client) -> Self {
        let partitioned =
            PackagePartitionedIndex::from_corpus(include_str!("./data/m2-index.jsonl"));
        // `self.builtins` is the Core partition — the new partitioned path and
        // the legacy single-blob path share one source so they cannot drift.
        // Core is always present (it is the loaded-set floor); its absence is a
        // corrupt corpus, so fail fast.
        let builtins = partitioned
            .partition("Core")
            .expect("Core partition present in builtin corpus")
            .clone();
        Backend {
            client,
            builtins,
            partitioned,
            source_resolver: SourceResolver::from_environment(),
            package_indexer: PackageIndexer::from_environment(),
            package_indexes: DashMap::new(),
            documents: DashMap::new(),
            workspace_index: Arc::new(WorkspaceIndex::default()),
            semantic_tokens_augment_syntax: AtomicBool::new(false),
            type_hierarchy_dynamic_registration: AtomicBool::new(false),
        }
    }

    fn package_index(&self, package_name: &str) -> Option<BuiltinData> {
        if let Some(index) = self.package_indexes.get(package_name) {
            return Some(index.clone());
        }

        let index = self.package_indexer.load(package_name)?;
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

    /// The ordered in-scope package set for a document: the partitioned index's
    /// default-loaded baseline plus the document's imports. Pure function of the
    /// text. Not yet consulted by query routing (P3) — provided so that work has
    /// a single source for "what is loaded here".
    #[allow(dead_code)]
    fn loaded_packages(&self, text: &str) -> LoadedPackages {
        LoadedPackages::resolve(self.partitioned.default_loaded(), text)
    }

    fn reindex_from_disk(&self, uri: &Url) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        match fs::read_to_string(&path) {
            Ok(text) => self.workspace_index.index_file(uri, &text, &self.builtins),
            Err(_) => self.workspace_index.remove_file(uri),
        }
    }

    fn record_location(&self, record: &Record) -> Option<Location> {
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
    ) -> Option<(String, BuiltinData, Record)> {
        let index = self.type_hierarchy_index(package)?;
        let record = index.get_record(&InstanceID::new(name))?;
        record.type_info.as_ref()?;
        Some((package.unwrap_or("Core").to_string(), index, record))
    }

    fn type_hierarchy_related_record(
        &self,
        package: &str,
        index: &BuiltinData,
        name: &InstanceID,
    ) -> Option<(String, Record)> {
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
        record: &Record,
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
        self.workspace_index
            .index_file(&uri, document.text(), &self.builtins);
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
            self.workspace_index
                .index_file(&uri, document.text(), &self.builtins);
            publish_diagnostics(&self.client, uri, document.value()).await;
        }
    }
}

fn append_debug_log(message: &str) {
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/m2_ls.log")
    else {
        return;
    };
    let _ = writeln!(file, "{message}");
}

fn install_panic_logging() {
    panic::set_hook(Box::new(|panic_info| {
        let backtrace = Backtrace::force_capture();
        append_debug_log(&format!("panic: {panic_info}\n{backtrace}"));
    }));
    append_debug_log("m2_ls starting");
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.workspace_index.set_roots(workspace_roots(&params));
        // Index every `.m2` file under the project roots off the request path.
        let index = Arc::clone(&self.workspace_index);
        let builtins = self.builtins.clone();
        task::spawn_blocking(move || index.scan(&builtins));
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
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                document_formatting_provider: document_formatting_provider_capability(),
                folding_range_provider: folding_range_provider_capability(),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                document_highlight_provider: document_highlight_provider_capability(),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                inlay_hint_provider: inlay_hint_provider_capability(),
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

        // Watch the workspace for `.m2` changes made outside the editor so the
        // cross-file definition index stays fresh.
        let _ = self
            .client
            .register_capability(vec![Registration {
                id: "m2_ls-watch-m2-files".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: Some(serde_json::json!({
                    "watchers": [{ "globPattern": "**/*.m2" }]
                })),
            }])
            .await;

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

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);
        // Re-index from disk so the workspace index reflects the saved file
        // rather than the last in-editor edit.
        self.reindex_from_disk(&uri);
        if self.documents.is_empty() {
            self.package_indexes.clear();
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            // Open documents are indexed from their live buffer; ignore disk
            // events for them to avoid clobbering unsaved edits.
            if self.documents.contains_key(&change.uri) {
                continue;
            }
            if change.typ == FileChangeType::DELETED {
                self.workspace_index.remove_file(&change.uri);
            } else {
                self.reindex_from_disk(&change.uri);
            }
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let loaded = LoadedPackages::from_parts(
            self.partitioned.default_loaded(),
            document.imported_packages(),
        );
        let scoped = self.partitioned.scoped(&loaded);
        Ok(hover_response(document.value(), position, &scoped))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let loaded = LoadedPackages::from_parts(
            self.partitioned.default_loaded(),
            document.imported_packages(),
        );
        let scoped = self.partitioned.scoped(&loaded);
        Ok(completion_response(document.text(), position, &scoped))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let augments_syntax_tokens = self.semantic_tokens_augment_syntax.load(Ordering::Relaxed);
        let tokens = collect_semantic_tokens(&document, &self.builtins, augments_syntax_tokens);
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
        let document = match self.documents.get(&uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let symbols = collect_document_symbols(&document, &self.builtins);
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let diagnostics = if params.context.diagnostics.is_empty() {
            document.diagnostics()
        } else {
            &params.context.diagnostics
        };
        Ok(available_code_actions(
            document.value(),
            uri,
            params.range.start,
            diagnostics,
        ))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        Ok(Some(inlay_hints_response(document.value(), params.range)))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        Ok(document_highlights(document.value(), position))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        // Resolve the target and the current file's references, then drop the
        // document guard before scanning other files (avoids holding a DashMap
        // ref across the cross-file pass).
        let (name, mut locations) = {
            let document = match self.documents.get(uri) {
                Some(document) => document,
                None => return Ok(None),
            };
            match reference_target(document.value(), position, &self.workspace_index) {
                None | Some(ReferenceTarget::Local) => {
                    // A local binding's references never leave the document.
                    return Ok(Some(references_response(
                        document.value(),
                        uri,
                        position,
                        include_declaration,
                    )));
                }
                Some(ReferenceTarget::Global(name)) => {
                    let locations = global_reference_ranges(document.value(), &name)
                        .into_iter()
                        .map(|range| Location {
                            uri: uri.clone(),
                            range,
                        })
                        .collect::<Vec<_>>();
                    (name, locations)
                }
            }
        };

        // Global symbol: collect uses from every other workspace file, preferring
        // a live buffer over the on-disk copy.
        for file_uri in self.workspace_index.workspace_file_uris() {
            if &file_uri == uri {
                continue;
            }
            let ranges = if let Some(open) = self.documents.get(&file_uri) {
                global_reference_ranges(open.value(), &name)
            } else if let Ok(path) = file_uri.to_file_path() {
                fs::read_to_string(path)
                    .ok()
                    .and_then(|text| DocumentSnapshot::from_text(text, &self.builtins))
                    .map(|snapshot| global_reference_ranges(&snapshot, &name))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            locations.extend(ranges.into_iter().map(|range| Location {
                uri: file_uri.clone(),
                range,
            }));
        }

        Ok(Some(locations))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let position = params.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        Ok(prepare_rename_range(document.value(), position).map(PrepareRenameResponse::Range))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name.trim();
        if new_name.is_empty() {
            return Ok(None);
        }

        // Resolve the target and the current file's edits first, then drop the
        // document guard before scanning other files (mirrors `references`).
        let (name, mut changes) = {
            let document = match self.documents.get(uri) {
                Some(document) => document,
                None => return Ok(None),
            };
            match reference_target(document.value(), position, &self.workspace_index) {
                None => return Ok(None),
                Some(ReferenceTarget::Local) => {
                    // A local binding renames only within its own document.
                    return Ok(rename_edits(document.value(), uri, position, new_name));
                }
                Some(ReferenceTarget::Global(name)) => {
                    let edits = global_reference_ranges(document.value(), &name)
                        .into_iter()
                        .map(|range| TextEdit {
                            range,
                            new_text: new_name.to_string(),
                        })
                        .collect::<Vec<_>>();
                    (name, HashMap::from([(uri.clone(), edits)]))
                }
            }
        };

        // Global symbol: rename its uses in every other workspace file too, so a
        // top-level rename never leaves stale references behind in the files that
        // import it.
        for file_uri in self.workspace_index.workspace_file_uris() {
            if &file_uri == uri {
                continue;
            }
            let ranges = if let Some(open) = self.documents.get(&file_uri) {
                global_reference_ranges(open.value(), &name)
            } else if let Ok(path) = file_uri.to_file_path() {
                fs::read_to_string(path)
                    .ok()
                    .and_then(|text| DocumentSnapshot::from_text(text, &self.builtins))
                    .map(|snapshot| global_reference_ranges(&snapshot, &name))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if ranges.is_empty() {
                continue;
            }
            let edits = ranges
                .into_iter()
                .map(|range| TextEdit {
                    range,
                    new_text: new_name.to_string(),
                })
                .collect::<Vec<_>>();
            changes.insert(file_uri, edits);
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }))
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
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
            if let Some(record) = package_index.get_record(&InstanceID::new(name)) {
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

        let Some(record) = self.builtins.get_record(&InstanceID::new(name)) else {
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

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
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
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.trim();
        if query.is_empty() {
            return Ok(Some(Vec::new()));
        }

        // No open-document context here, so scope to the default-loaded baseline.
        let loaded = LoadedPackages::resolve(self.partitioned.default_loaded(), "");
        let scoped = self.partitioned.scoped(&loaded);
        Ok(Some(workspace_symbols_response(query, &scoped, |record| {
            self.record_location(record)
        })))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let loaded = LoadedPackages::from_parts(
            self.partitioned.default_loaded(),
            document.imported_packages(),
        );
        let scoped = self.partitioned.scoped(&loaded);
        Ok(goto_definition_response(
            document.value(),
            uri,
            position,
            &scoped,
            &self.source_resolver,
            &self.workspace_index,
            |record| self.record_location(record),
        ))
    }
}

/// The project roots to index, preferring `workspaceFolders` and falling back
/// to the (deprecated) `rootUri` single-folder field that older clients send.
fn workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    if let Some(folders) = &params.workspace_folders {
        let roots: Vec<PathBuf> = folders
            .iter()
            .filter_map(|folder| folder.uri.to_file_path().ok())
            .collect();
        if !roots.is_empty() {
            return roots;
        }
    }
    #[allow(deprecated)]
    params
        .root_uri
        .as_ref()
        .and_then(|uri| uri.to_file_path().ok())
        .into_iter()
        .collect()
}

#[tokio::main]
async fn main() {
    install_panic_logging();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket)
        .serve(TypeHierarchyCapabilityService::new(service))
        .await;
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::*;
    use crate::analysis::Analysis;
    use crate::record_lsp::{record_hover_with_package, record_hover_with_package_and_usage};
    use crate::typesystem::BuiltinData;
    use tree_sitter::Parser;

    #[test]
    fn source_resolver_finds_package_and_doc_files_from_m2_path_roots() {
        let root = env::temp_dir().join(format!("m2-lsp-source-resolver-{}", std::process::id()));
        let packages = root.join("Macaulay2").join("packages");
        let docs = packages.join("Macaulay2Doc");
        let core = root.join("Macaulay2").join("m2");
        fs::create_dir_all(&docs).expect("test docs dir should be created");
        fs::create_dir_all(&core).expect("test core dir should be created");
        fs::write(packages.join("Graphs.m2"), "").expect("package fixture should write");
        fs::write(docs.join("operators.m2"), "").expect("doc fixture should write");
        fs::write(core.join("option.m2"), "").expect("core fixture should write");

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

        let _ = fs::remove_dir_all(root);
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
        let root = env::temp_dir().join(format!("m2-lsp-package-index-{}", std::process::id()));
        fs::create_dir_all(&root).expect("test package cache dir should be created");
        fs::write(root.join("Graphs.names"), "graph\n")
            .expect("package names fixture should write");
        fs::write(
            root.join("Graphs.details.jsonl"),
            "{\"name\":\"graph\",\"class\":\"MethodFunction\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{\"package\":\"Graphs\"}}\n",
        )
        .expect("package details fixture should write");

        let indexer = PackageIndexer {
            cache_dir: root.clone(),
        };
        let index = indexer
            .load("Graphs")
            .expect("cached package index should load");
        let record = index
            .get_record(&typesystem::InstanceID::new("graph"))
            .expect("package record should be available");

        assert_eq!(record.name.0, "graph");
        assert_eq!(record_package(&record), Some("Graphs"));

        let _ = fs::remove_dir_all(root);
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
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
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
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
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
    fn record_hover_includes_option_value_reverse_usage() {
        let builtins = BuiltinData::load_from_split_with_type_facts(
            "LongPolynomial\n",
            "{\"name\":\"LongPolynomial\",\"class\":\"Symbol\",\"description_short\":\"a Strategy option value\",\"description_long\":null,\"examples\":[],\"extra\":{}}\n",
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
            "{\"name\":\"kernel\",\"class\":\"MethodFunction\",\"description_short\":\"kernel of a map\",\"description_long\":null,\"examples\":[\"R = QQ[a..d];\",\"ker F\"],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"kernel\",\"RingMap\"]}],\"documented_methods\":[{\"signature\":[\"kernel\",\"RingMap\"],\"output_types\":[\"Ideal\"],\"examples\":[\"R = QQ[a..d];\"],\"doc_key\":\"kernel(RingMap)\"}]}}\n",
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
            "{\"name\":\"method\",\"class\":\"FunctionClosure\",\"description_short\":\"make a new method function\",\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[],\"general_signature\":{\"signature\":[\"method\"],\"output_types\":[\"MethodFunction\"]}}}\n",
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
    fn record_hover_renders_operator_documented_signatures_in_operator_form() {
        let builtins = BuiltinData::load_from_split(
            "=>\n",
            "{\"name\":\"=>\",\"class\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"operator_info\":{\"attributes\":{\"Binary\":[]},\"flags\":{\"Binary\":[]},\"flexible\":false,\"forms\":[\"Binary\"],\"method_lookup\":\"symbol\",\"method_symbol\":\"=>\"},\"function_info\":{\"methods\":[{\"signature\":[\"=>\",\"Thing\",\"Thing\"]}],\"documented_methods\":[{\"signature\":[\"=>\",\"Thing\",\"Thing\"],\"output_types\":[\"Option\"]}]}}\n",
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
            "{\"name\":\"+\",\"class\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"operator_info\":{\"attributes\":{\"Binary\":[\"Flexible\"]},\"flags\":{\"Binary\":[\"Flexible\"]},\"flexible\":true,\"forms\":[\"Binary\"],\"method_lookup\":\"symbol\",\"method_symbol\":\"+\"},\"function_info\":{\"methods\":[{\"signature\":[\"(+,=)\",\"Thing\",\"Thing\"]}]}}\n",
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
    fn record_hover_keeps_excluded_signatures_when_usage_is_pinned() {
        let builtins = BuiltinData::load_from_split(
            "f\n",
            "{\"name\":\"f\",\"class\":\"MethodFunction\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"f\",\"String\"]},{\"signature\":[\"f\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"f\",\"String\"],\"output_types\":[\"File\"]},{\"signature\":[\"f\",\"ZZ\"],\"output_types\":[\"Thing\"]}]}}\n",
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
}
