use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::document::DocumentSnapshot;

pub(crate) async fn publish_diagnostics(
    client: &Client,
    uri: Url,
    document: &DocumentSnapshot,
) {
    let diagnostics = document.analysis().diagnostics.clone();
    client.publish_diagnostics(uri, diagnostics, None).await;
}
