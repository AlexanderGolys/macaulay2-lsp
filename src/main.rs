//! Macaulay2's stdio language-server entry point and protocol wiring.

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::{io, task};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

mod analysis;
mod builtin_index;
mod capabilities;
mod client_capabilities;
mod diagnostic_registry;
mod document;
mod documentation;
mod meta;
mod node_metadata;
mod package_index;
mod partitioned_index;
mod record_lsp;
mod settings;
mod typesystem;
mod util;
mod workspace_index;

use capabilities::code_actions::available_code_actions;
use capabilities::diagnostics::{publish_diagnostics, visible_diagnostics};
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
    completion_response, global_reference_ranges, goto_definition_response, is_valid_m2_identifier,
    prepare_rename_range, reference_target, references_response, rename_edits,
    workspace_symbols_response, ReferenceTarget,
};
use capabilities::semantic_tokens::{collect_semantic_tokens, LEGEND_MODIFIERS, LEGEND_TYPES};
use capabilities::signature_help::signature_help_response;
use capabilities::type_hierarchy::{
    TypeHierarchyCapabilityService, TypeHierarchyContext, TYPE_HIERARCHY_METHOD,
};
use client_capabilities::{
    refresh_if_changed, ClientSupport, InlayHintRefresh, SemanticTokensAugmentSyntax,
    TypeHierarchyDynamicRegistration, WorkspaceRefresh,
};
use diagnostic_registry::DiagnosticPolicy;
use document::DocumentSnapshot;
use package_index::SourceResolver;
#[cfg(test)]
use package_index::{collect_imported_packages, package_source_string};

use crate::partitioned_index::{LoadedPackages, PackagePartitionedIndex, ScopedIndex};
use crate::settings::{ServerSettings, SettingsStore};
use crate::workspace_index::WorkspaceIndex;

#[derive(Debug)]
struct Backend {
    client: Client,
    partitioned: PackagePartitionedIndex,
    source_resolver: SourceResolver,
    documents: DashMap<Url, DocumentSnapshot>,
    workspace_index: Arc<WorkspaceIndex>,
    settings: SettingsStore<ServerSettings>,
    semantic_tokens_augment_syntax: ClientSupport<SemanticTokensAugmentSyntax>,
    type_hierarchy_dynamic_registration: ClientSupport<TypeHierarchyDynamicRegistration>,
    inlay_hint_refresh: ClientSupport<InlayHintRefresh>,
}

impl Backend {
    fn new(client: Client) -> Self {
        let partitioned =
            PackagePartitionedIndex::from_corpus(include_str!("./data/m2-index.jsonl"));
        Backend {
            client,
            partitioned,
            source_resolver: SourceResolver::from_environment(),
            documents: DashMap::new(),
            workspace_index: Arc::new(WorkspaceIndex::default()),
            settings: SettingsStore::default(),
            semantic_tokens_augment_syntax: ClientSupport::default(),
            type_hierarchy_dynamic_registration: ClientSupport::default(),
            inlay_hint_refresh: ClientSupport::default(),
        }
    }

    fn reindex_from_disk(&self, uri: &Url) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        match fs::read_to_string(&path) {
            Ok(text) => self
                .workspace_index
                .index_file(uri, &text, &self.partitioned),
            Err(_) => self.workspace_index.remove_file(uri),
        }
    }

    /// The scoped-index prologue every per-document request shares: combine the
    /// corpus baseline with the document's imports and build the borrowing
    /// `ScopedIndex` view. Five handlers route through this so the package
    /// scoping rule lives in one place.
    fn scoped_index_for<'a>(&'a self, document: &DocumentSnapshot) -> ScopedIndex<'a> {
        let loaded = LoadedPackages::from_parts(
            self.partitioned.default_loaded(),
            document.imported_packages(),
        );
        self.partitioned.scoped(&loaded)
    }

    fn with_document<R>(
        &self,
        uri: &Url,
        request: impl FnOnce(&DocumentSnapshot) -> R,
    ) -> Option<R> {
        let document = self.documents.get(uri)?;
        Some(request(document.value()))
    }

    fn with_scoped_document<R>(
        &self,
        uri: &Url,
        request: impl FnOnce(&DocumentSnapshot, &ScopedIndex<'_>) -> R,
    ) -> Option<R> {
        let document = self.documents.get(uri)?;
        let scoped = self.scoped_index_for(document.value());
        Some(request(document.value(), &scoped))
    }

    /// Collect occurrences of a global symbol across every workspace file
    /// other than `exclude`. Prefers a live open buffer (so unsaved edits are
    /// reflected) and falls back to parsing the on-disk file. Used by both
    /// `references` and `rename` so the cross-file scan lives in one place.
    fn cross_file_global_references<'a>(
        &'a self,
        name: &'a str,
        exclude: &'a Url,
    ) -> impl Iterator<Item = (Url, Range)> + 'a {
        self.workspace_index
            .workspace_file_uris()
            .into_iter()
            .filter(move |file_uri| file_uri != exclude)
            .flat_map(move |file_uri| {
                let ranges = if let Some(open) = self.documents.get(&file_uri) {
                    global_reference_ranges(open.value(), name)
                } else if let Ok(path) = file_uri.to_file_path() {
                    fs::read_to_string(path)
                        .ok()
                        .and_then(|text| DocumentSnapshot::from_text(text, &self.partitioned))
                        .map(|snapshot| global_reference_ranges(&snapshot, name))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                ranges
                    .into_iter()
                    .map(move |range| (file_uri.clone(), range))
            })
    }

    fn type_hierarchy_context(&self) -> TypeHierarchyContext<'_, PackagePartitionedIndex> {
        TypeHierarchyContext::new(&self.partitioned, &self.source_resolver)
    }

    async fn publish_document_diagnostics(&self, uri: Url, document: &DocumentSnapshot) {
        let settings = self.settings.snapshot();
        publish_diagnostics(&self.client, uri, document, settings.diagnostics()).await;
    }

    async fn apply_settings(
        &self,
        settings: ServerSettings,
        refresh_client: &(impl WorkspaceRefresh<InlayHintRefresh> + ?Sized),
    ) -> tower_lsp::jsonrpc::Result<()> {
        let previous = self.settings.replace(settings.clone());

        if previous.diagnostics() != settings.diagnostics() {
            let diagnostics = self
                .documents
                .iter()
                .map(|document| {
                    (
                        document.key().clone(),
                        visible_diagnostics(document.value(), settings.diagnostics()),
                    )
                })
                .collect::<Vec<_>>();
            for (uri, diagnostics) in diagnostics {
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .await;
            }
        }

        refresh_if_changed(
            &self.inlay_hint_refresh,
            refresh_client,
            previous.inlay_hints() != settings.inlay_hints(),
        )
        .await
    }

    async fn on_open(&self, params: TextDocumentItem) {
        let Some(document) = DocumentSnapshot::from_text(params.text, &self.partitioned) else {
            return;
        };
        let uri = params.uri;
        self.workspace_index
            .index_file(&uri, document.text(), &self.partitioned);
        self.documents.insert(uri.clone(), document);
        if let Some(document) = self.documents.get(&uri) {
            self.publish_document_diagnostics(uri, document.value())
                .await;
        }
    }

    async fn on_change(&self, uri: Url, changes: Vec<TextDocumentContentChangeEvent>) {
        if let Some(mut document) = self.documents.get_mut(&uri) {
            if document
                .apply_changes(&changes, &self.partitioned)
                .is_none()
            {
                return;
            }
            self.workspace_index
                .index_file(&uri, document.text(), &self.partitioned);
            self.publish_document_diagnostics(uri, document.value())
                .await;
        }
    }
}

fn append_debug_log(message: &str) {
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/m2-ls.log")
    else {
        return;
    };
    let _ = writeln!(file, "{message}");
}

/// Install the crash hook: a panic always logs its backtrace to
/// `/tmp/m2-ls.log` (crash diagnostics are worth the write). Routine chatter
/// is opt-in via the `M2_LS_LOG` environment variable.
fn install_panic_logging() {
    panic::set_hook(Box::new(|panic_info| {
        let backtrace = Backtrace::force_capture();
        append_debug_log(&format!("panic: {panic_info}\n{backtrace}"));
    }));
    if std::env::var_os("M2_LS_LOG").is_some() {
        append_debug_log("m2-ls starting");
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.workspace_index.set_roots(workspace_roots(&params));
        // Index every `.m2` file under the project roots off the request path.
        let index = Arc::clone(&self.workspace_index);
        let knowledge = self.partitioned.clone();
        task::spawn_blocking(move || index.scan(&knowledge));
        self.semantic_tokens_augment_syntax
            .negotiate(&params.capabilities);
        self.type_hierarchy_dynamic_registration
            .negotiate(&params.capabilities);
        self.inlay_hint_refresh.negotiate(&params.capabilities);

        if let Some(options) = params.initialization_options.as_ref() {
            match ServerSettings::from_value(options) {
                Ok(settings) => {
                    self.settings.replace(settings);
                }
                Err(error) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("Ignoring invalid Macaulay2 LSP settings: {error}"),
                        )
                        .await;
                }
            }
        }

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
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    work_done_progress_options: Default::default(),
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
                                token_modifiers: LEGEND_MODIFIERS.into(),
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                        },
                    ),
                ),
                ..Default::default()
            },
            server_info: Some(server_info()),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if self.type_hierarchy_dynamic_registration.is_supported()
            && self
                .client
                .register_capability(vec![Registration {
                    id: "m2-ls-type-hierarchy".to_string(),
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
                id: "m2-ls-watch-m2-files".to_string(),
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
                    self.partitioned.symbol_count()
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

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let settings = match ServerSettings::from_value(&params.settings) {
            Ok(settings) => settings,
            Err(error) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("Ignoring invalid Macaulay2 LSP settings: {error}"),
                    )
                    .await;
                return;
            }
        };
        if let Err(error) = self.apply_settings(settings, &self.client).await {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("Failed to refresh Macaulay2 inlay hints: {error}"),
                )
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);
        // Re-index from disk so the workspace index reflects the saved file
        // rather than the last in-editor edit.
        self.reindex_from_disk(&uri);
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
        Ok(self
            .with_scoped_document(uri, |document, knowledge| {
                hover_response(document, position, knowledge)
            })
            .flatten())
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        Ok(self
            .with_scoped_document(uri, |document, knowledge| {
                completion_response(document.text(), position, document.analysis(), knowledge)
            })
            .flatten())
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        Ok(self
            .with_scoped_document(uri, |document, knowledge| {
                signature_help_response(document, position, knowledge)
            })
            .flatten())
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let augments_syntax_tokens = self.semantic_tokens_augment_syntax.is_supported();
        Ok(self.with_scoped_document(&uri, |document, knowledge| {
            let tokens = collect_semantic_tokens(
                document,
                knowledge,
                &self.workspace_index,
                &uri,
                augments_syntax_tokens,
            );
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: tokens,
            })
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        Ok(self.with_document(&uri, |document| {
            DocumentSymbolResponse::Nested(collect_document_symbols(document))
        }))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let settings = self.settings.snapshot();
        Ok(self
            .with_document(uri, |document| {
                let diagnostics = if params.context.diagnostics.is_empty() {
                    visible_diagnostics(document, settings.diagnostics())
                } else {
                    params
                        .context
                        .diagnostics
                        .into_iter()
                        .filter(|diagnostic| {
                            settings.diagnostics().allows_lsp_diagnostic(diagnostic)
                        })
                        .collect()
                };
                available_code_actions(document, uri, params.range.start, &diagnostics)
            })
            .flatten())
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;
        let expression_types = self.settings.snapshot().expression_type_hints();
        Ok(self.with_document(uri, |document| {
            inlay_hints_response(document, params.range, expression_types)
        }))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        Ok(self
            .with_scoped_document(uri, |document, knowledge| {
                document_highlights(document, position, knowledge)
            })
            .flatten())
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
        locations.extend(
            self.cross_file_global_references(&name, uri)
                .map(|(uri, range)| Location { uri, range }),
        );

        Ok(Some(locations))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let position = params.position;
        Ok(self
            .with_document(uri, |document| {
                prepare_rename_range(document, position).map(PrepareRenameResponse::Range)
            })
            .flatten())
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name.trim();
        if !is_valid_m2_identifier(new_name) {
            // Renaming to an invalid identifier would silently produce
            // unparsable code; refuse instead.
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
        for (file_uri, range) in self.cross_file_global_references(&name, uri) {
            changes.entry(file_uri).or_default().push(TextEdit {
                range,
                new_text: new_name.to_string(),
            });
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
        Ok(self
            .with_scoped_document(&uri, |document, knowledge| {
                self.type_hierarchy_context()
                    .prepare(document, knowledge, &uri, position)
            })
            .flatten())
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        Ok(self.type_hierarchy_context().supertypes(&params.item))
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        Ok(self.type_hierarchy_context().subtypes(&params.item))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let settings = self.settings.snapshot();
        Ok(self.with_document(&uri, |document| {
            document_formatting_text_edits(
                document.text(),
                params.options.tab_size,
                params.options.insert_spaces,
                settings.formatting(),
            )
        }))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        Ok(self.with_document(&uri, |document| folding_ranges(document.text())))
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
            self.source_resolver.record_location(record)
        })))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        Ok(self
            .with_scoped_document(uri, |document, knowledge| {
                goto_definition_response(
                    document,
                    uri,
                    position,
                    knowledge,
                    &self.source_resolver,
                    &self.workspace_index,
                    |record| self.source_resolver.record_location(record),
                )
            })
            .flatten())
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

fn server_info() -> ServerInfo {
    ServerInfo {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    }
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
    use std::future::{poll_fn, Future};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{env, fs};

    use super::*;
    use crate::analysis::Analysis;
    use crate::capabilities::formatting::FormattingConfiguration;
    use crate::diagnostic_registry::M2Diagnostic;
    use crate::record_lsp::record_hover_with_package_and_usage;
    use crate::typesystem::BuiltinData;
    use tower::Service;
    use tower_lsp::jsonrpc::{Request as JsonRpcRequest, Response as JsonRpcResponse};
    use tree_sitter::Parser;

    #[derive(Default)]
    struct CountingInlayHintRefresh {
        calls: AtomicUsize,
    }

    impl WorkspaceRefresh<InlayHintRefresh> for CountingInlayHintRefresh {
        fn refresh(&self) -> impl Future<Output = tower_lsp::jsonrpc::Result<()>> + Send {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(()))
        }
    }

    async fn call_lsp(
        service: &mut LspService<Backend>,
        request: JsonRpcRequest,
    ) -> Option<JsonRpcResponse> {
        poll_fn(|context| service.poll_ready(context))
            .await
            .expect("LSP service should become ready");
        service
            .call(request)
            .await
            .expect("LSP service call should succeed")
    }

    #[test]
    fn document_request_helpers_share_missing_and_scoped_behavior() {
        let (service, _socket) = LspService::new(Backend::new);
        let uri = Url::parse("file:///tmp/m2-ls-request-helper-test.m2").unwrap();

        assert!(service
            .inner()
            .with_document(&uri, |document| document.text().to_string())
            .is_none());
        assert!(service
            .inner()
            .with_scoped_document(&uri, |_, _| ())
            .is_none());

        let document = DocumentSnapshot::from_text(
            "needsPackage \"JSON\"\n".to_string(),
            &service.inner().partitioned,
        )
        .unwrap();
        service.inner().documents.insert(uri.clone(), document);

        assert_eq!(
            service
                .inner()
                .with_document(&uri, |document| document.text().to_string()),
            Some("needsPackage \"JSON\"\n".to_string())
        );
        assert_eq!(
            service
                .inner()
                .with_scoped_document(&uri, |_, knowledge| knowledge
                    .get_record(&typesystem::InstanceID::new("toJSON"))
                    .is_some()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn lsp_service_applies_initial_and_live_settings() {
        let (mut service, _socket) = LspService::new(Backend::new);
        let initialize = JsonRpcRequest::build("initialize")
            .params(serde_json::json!({
                "capabilities": {},
                "initializationOptions": {
                    "formatting": {
                        "compactFactorOperators": true
                    },
                    "inlayHints": {
                        "expressionTypes": true
                    }
                }
            }))
            .id(1)
            .finish();

        let response = call_lsp(&mut service, initialize)
            .await
            .expect("initialize should return a response");
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            env!("CARGO_PKG_NAME")
        );
        assert_eq!(
            response["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        let initial = service.inner().settings.snapshot();
        assert!(initial.formatting().compact_factor_operators());
        assert!(initial.expression_type_hints());

        let uri = Url::parse("file:///tmp/m2-ls-settings-test.m2").unwrap();
        let document =
            DocumentSnapshot::from_text("x = (\n".to_string(), &service.inner().partitioned)
                .expect("fixture should parse");
        assert!(!document.diagnostics().is_empty());
        service.inner().documents.insert(uri.clone(), document);

        let change = JsonRpcRequest::build("workspace/didChangeConfiguration")
            .params(serde_json::json!({
                "settings": {
                    "m2-ls": {
                        "diagnostics": {
                            "enabled": false
                        },
                        "formatting": {
                            "indentWidth": 2,
                            "useTabs": false,
                            "maxLineWidth": 12,
                            "compactFactorOperators": false
                        }
                    }
                }
            }))
            .finish();
        assert!(call_lsp(&mut service, change).await.is_none());

        let current = service.inner().settings.snapshot();
        assert!(!current.diagnostics().allows(M2Diagnostic::SyntaxError));
        assert!(!current.formatting().compact_factor_operators());
        let document = service
            .inner()
            .documents
            .iter()
            .next()
            .expect("test document should remain open");
        assert!(visible_diagnostics(document.value(), current.diagnostics()).is_empty());
        drop(document);

        let document = DocumentSnapshot::from_text(
            "f := (\nlongName*a*b\n)\n".to_string(),
            &service.inner().partitioned,
        )
        .expect("formatting fixture should parse");
        service.inner().documents.insert(uri.clone(), document);
        let formatting = JsonRpcRequest::build("textDocument/formatting")
            .params(serde_json::json!({
                "textDocument": {
                    "uri": uri
                },
                "options": {
                    "tabSize": 8,
                    "insertSpaces": false
                }
            }))
            .id(2)
            .finish();
        let response = call_lsp(&mut service, formatting)
            .await
            .expect("formatting should return a response");
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(
            response["result"][0]["newText"],
            "f := (\n  longName *\n    a * b\n)\n"
        );
    }

    #[tokio::test]
    async fn live_inlay_hint_changes_use_negotiated_workspace_refresh() {
        let (mut service, _socket) = LspService::new(Backend::new);
        let initialize = JsonRpcRequest::build("initialize")
            .params(serde_json::json!({
                "capabilities": {
                    "workspace": {
                        "inlayHint": {
                            "refreshSupport": true
                        }
                    }
                }
            }))
            .id(1)
            .finish();
        call_lsp(&mut service, initialize)
            .await
            .expect("initialize should return a response");

        let refresh = CountingInlayHintRefresh::default();
        let enabled = ServerSettings::from_value(&serde_json::json!({
            "inlayHints": {
                "expressionTypes": true
            }
        }))
        .unwrap();
        service
            .inner()
            .apply_settings(enabled.clone(), &refresh)
            .await
            .unwrap();
        assert_eq!(refresh.calls.load(Ordering::Relaxed), 1);

        service
            .inner()
            .apply_settings(enabled, &refresh)
            .await
            .unwrap();
        assert_eq!(refresh.calls.load(Ordering::Relaxed), 1);

        let formatting_only = ServerSettings::from_value(&serde_json::json!({
            "formatting": {
                "indentWidth": 2
            },
            "inlayHints": {
                "expressionTypes": true
            }
        }))
        .unwrap();
        service
            .inner()
            .apply_settings(formatting_only, &refresh)
            .await
            .unwrap();
        assert_eq!(refresh.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reports_crate_identity_to_lsp_clients() {
        let info = server_info();

        assert_eq!(info.name, env!("CARGO_PKG_NAME"));
        assert_eq!(info.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

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
        let root = crate::node_metadata::M2Node::new(tree.root_node(), text);
        let mut packages = Vec::new();
        for node in root.descendants() {
            if node.kind == crate::node_metadata::NodeKind::StringLiteral {
                if let Some(package_name) = package_source_string(node) {
                    packages.push(package_name);
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

        let hover = record_hover_with_package_and_usage(&record, Some("Core"), &builtins, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(
            markup.value.contains("Package: `Core`"),
            "record hover should display the package supplied by the LSP context"
        );
    }

    #[test]
    fn option_value_usage_lookup_resolves_from_possible_values() {
        let corpus = concat!(
            "{\"kind\":\"symbol\",\"name\":\"LongPolynomial\",\"class\":\"Symbol\"}\n",
            "{\"kind\":\"methodFunction\",\"name\":\"gb\",\"options\":[{\"key\":\"Strategy\",\"possibleValues\":[\"LongPolynomial\"]}]}\n",
        );
        let builtins = BuiltinData::load_from_index(corpus);

        assert_eq!(
            builtins.option_value_usage_names("LongPolynomial", 8),
            vec!["gb.Strategy"],
        );
    }

    #[test]
    fn record_hover_includes_documented_signatures_and_examples() {
        let corpus = concat!(
            "{\"kind\":\"methodFunction\",\"name\":\"kernel\",",
            "\"methods\":[{\"domain\":[\"RingMap\"],\"typicalValue\":\"Ideal\"}],",
            "\"markdown\":\"**Examples:**\\n```macaulay2\\nR = QQ[a..d];\\nker F\\n```\"}\n",
        );
        let builtins = BuiltinData::load_from_index(corpus);
        let record = builtins
            .get_record(&typesystem::InstanceID::new("kernel"))
            .expect("kernel should deserialize");

        let hover = record_hover_with_package_and_usage(&record, Some("Core"), &builtins, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(
            markup.value.contains("`(RingMap) -> Ideal`"),
            "record hover should display documented method codomains"
        );
        assert!(
            markup.value.contains("> ```macaulay2\n> R = QQ[a..d];"),
            "record hover should display saved examples in a bordered card"
        );
    }

    #[test]
    fn record_hover_includes_global_typical_value() {
        let builtins = BuiltinData::load_from_index(
            "{\"kind\":\"methodFunction\",\"name\":\"method\",\"typical_value\":\"MethodFunction\"}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("method"))
            .expect("method should deserialize");

        let hover = record_hover_with_package_and_usage(&record, Some("Core"), &builtins, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("Typical Value: `MethodFunction`"));
    }

    #[test]
    fn record_hover_renders_operator_documented_signatures_in_operator_form() {
        let builtins = BuiltinData::load_from_index(
            "{\"kind\":\"operator\",\"name\":\"=>\",\"operator\":{\"forms\":[\"binary\"]},\"methods\":[{\"domain\":[\"Thing\",\"Thing\"],\"typicalValue\":\"Option\"}]}\n",
        );
        let record = builtins
            .get_record(&typesystem::InstanceID::new("=>"))
            .expect("=> should have operator metadata");

        let hover = record_hover_with_package_and_usage(&record, Some("Core"), &builtins, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("`Thing => Thing -> Option`"));
        assert!(!markup.value.contains("`Thing, Thing -> Option`"));
    }

    #[test]
    fn record_hover_keeps_excluded_signatures_when_usage_is_pinned() {
        // f dispatches on String→File and ZZ→Ring; pinning to String excludes ZZ→Ring
        let builtins = BuiltinData::load_from_index(concat!(
            "{\"kind\":\"methodFunction\",\"name\":\"f\",\"methods\":[",
            "{\"domain\":[\"String\"],\"typicalValue\":\"File\"},",
            "{\"domain\":[\"ZZ\"],\"typicalValue\":\"Ring\"}",
            "]}\n",
        ));
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

        assert!(markup.value.starts_with("**f** `(String) -> File`"));
        assert!(!markup.value.contains("**Signature:**"));
        assert!(markup.value.contains("**Other signatures for this call:**"));
        assert!(markup.value.contains("`(ZZ) -> Ring`"));
    }
}
