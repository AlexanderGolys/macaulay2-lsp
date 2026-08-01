//! LSP publication of diagnostics produced by document analysis.

use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::diagnostic_registry::DiagnosticPolicy;
use crate::document::DocumentSnapshot;

pub fn visible_diagnostics(
    document: &DocumentSnapshot,
    policy: &(impl DiagnosticPolicy + ?Sized),
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    document
        .diagnostics()
        .iter()
        .filter(|diagnostic| policy.allows(diagnostic.kind))
        .map(|diagnostic| diagnostic.to_lsp())
        .collect()
}

pub async fn publish_diagnostics(
    client: &Client,
    uri: Url,
    document: &DocumentSnapshot,
    policy: &(impl DiagnosticPolicy + ?Sized),
) {
    client
        .publish_diagnostics(uri, visible_diagnostics(document, policy), None)
        .await;
}
