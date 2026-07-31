//! Black-box coverage for the complete stdio language-server pipeline.

use std::env;

use serde_json::json;

use crate::support::{document_position, position, response_array, LspProcess, TestWorkspace};

const SOURCE: &str = include_str!("../fixtures/capability_spectrum.m2");

#[tokio::test]
async fn workspace_symbols_exclude_function_body_bindings_from_unopened_files() {
    let source = "outer := x -> (localBinding := x; globalBinding = x; localBinding)\n";
    let workspace = TestWorkspace::new(source);
    let mut server = LspProcess::spawn().await;
    server.initialize(&workspace.root_uri()).await;

    let symbols = server
        .request("workspace/symbol", json!({"query": ""}))
        .await;
    let symbols = response_array(&symbols);
    assert!(symbols.iter().any(|symbol| {
        symbol["name"] == "outer"
            && symbol["location"]["uri"] == workspace.uri
            && symbol["containerName"].is_null()
    }));
    assert!(symbols
        .iter()
        .all(|symbol| { symbol["name"] != "localBinding" && symbol["name"] != "globalBinding" }));

    server
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": workspace.uri,
                    "languageId": "macaulay2",
                    "version": 1,
                    "text": source
                }
            }),
        )
        .await;
    server
        .wait_for_notification("textDocument/publishDiagnostics")
        .await;

    let document_symbols = server
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": workspace.uri}}),
        )
        .await;
    let children = &response_array(&document_symbols)[0]["children"];
    assert!(response_array(children)
        .iter()
        .any(|symbol| symbol["name"] == "localBinding"));
    assert!(response_array(children)
        .iter()
        .any(|symbol| symbol["name"] == "globalBinding"));

    server.shutdown().await;
}

#[tokio::test]
async fn example_document_exercises_the_capability_spectrum_over_stdio() {
    let workspace = TestWorkspace::new(SOURCE);
    let mut server = LspProcess::spawn().await;
    let initialized = server.initialize(&workspace.root_uri()).await;

    assert_eq!(initialized["serverInfo"]["name"], env!("CARGO_PKG_NAME"));
    assert_eq!(
        initialized["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    for capability in [
        "hoverProvider",
        "referencesProvider",
        "renameProvider",
        "documentFormattingProvider",
        "foldingRangeProvider",
        "workspaceSymbolProvider",
        "completionProvider",
        "signatureHelpProvider",
        "definitionProvider",
        "documentSymbolProvider",
        "documentHighlightProvider",
        "codeActionProvider",
        "inlayHintProvider",
        "semanticTokensProvider",
        "typeHierarchyProvider",
    ] {
        assert!(
            initialized["capabilities"].get(capability).is_some(),
            "initialize should advertise {capability}"
        );
    }

    let unopened_workspace_symbols = server
        .request("workspace/symbol", json!({"query": "crossFile"}))
        .await;
    assert!(
        response_array(&unopened_workspace_symbols)
            .iter()
            .any(|symbol| {
                symbol["name"] == "crossFileResult"
                    && symbol["location"]["uri"] == workspace.related_uri
            }),
        "workspace symbols should include files that have not been opened: {unopened_workspace_symbols}"
    );

    let unopened_hover = server
        .request(
            "textDocument/hover",
            document_position(
                "file:///document-that-was-not-opened.m2",
                json!({"line": 0, "character": 0}),
            ),
        )
        .await;
    assert!(unopened_hover.is_null());

    server
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": workspace.uri,
                    "languageId": "macaulay2",
                    "version": 1,
                    "text": SOURCE
                }
            }),
        )
        .await;
    let diagnostics = server
        .wait_for_notification("textDocument/publishDiagnostics")
        .await;
    assert_eq!(diagnostics["params"]["uri"], workspace.uri);
    assert!(diagnostics["params"]["diagnostics"].is_array());
    server
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": workspace.related_uri,
                    "languageId": "macaulay2",
                    "version": 1,
                    "text": "crossFileResult = localValue\ntoJSON\n"
                }
            }),
        )
        .await;
    let related_diagnostics = server
        .wait_for_notification("textDocument/publishDiagnostics")
        .await;
    assert_eq!(related_diagnostics["params"]["uri"], workspace.related_uri);

    let hover = server
        .request(
            "textDocument/hover",
            document_position(&workspace.uri, position(SOURCE, "toJSON", 0)),
        )
        .await;
    assert!(
        hover["contents"]["value"]
            .as_str()
            .is_some_and(|markdown| markdown.contains("Package: `JSON`")),
        "imported package hover should survive the entire server pipeline: {hover}"
    );
    let unimported_hover = server
        .request(
            "textDocument/hover",
            document_position(&workspace.related_uri, json!({"line": 1, "character": 1})),
        )
        .await;
    assert!(
        unimported_hover.is_null(),
        "loading JSON in one document must not register it in another: {unimported_hover}"
    );

    let completion = server
        .request(
            "textDocument/completion",
            document_position(
                &workspace.uri,
                json!({
                    "line": position(SOURCE, "toJ", 1)["line"],
                    "character": position(SOURCE, "toJ", 1)["character"]
                        .as_u64()
                        .expect("the fixture position should be numeric")
                        + 3
                }),
            ),
        )
        .await;
    assert!(
        response_array(&completion)
            .iter()
            .any(|item| item["label"] == "toJSON"),
        "completion should include an imported package symbol: {completion}"
    );

    let signature_help = server
        .request(
            "textDocument/signatureHelp",
            document_position(
                &workspace.uri,
                json!({
                    "line": position(SOURCE, "result", 1)["line"],
                    "character": position(SOURCE, "result", 1)["character"]
                        .as_u64()
                        .expect("the fixture position should be numeric")
                        + 2
                }),
            ),
        )
        .await;
    assert!(
        response_array(&signature_help["signatures"])
            .iter()
            .any(|signature| signature["label"] == "toJSON(Symbol) -> String"),
        "signature help should use the imported package partition: {signature_help}"
    );

    let semantic_tokens = server
        .request(
            "textDocument/semanticTokens/full",
            json!({"textDocument": {"uri": workspace.uri}}),
        )
        .await;
    assert!(
        !response_array(&semantic_tokens["data"]).is_empty(),
        "semantic token response should contain encoded tokens"
    );

    let document_symbols = server
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": workspace.uri}}),
        )
        .await;
    assert!(
        response_array(&document_symbols)
            .iter()
            .any(|symbol| symbol["name"] == "double"),
        "document symbols should include analyzed bindings: {document_symbols}"
    );

    let code_actions = server
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": {"uri": workspace.uri},
                "range": {
                    "start": position(SOURCE, "if result", 0),
                    "end": position(SOURCE, "if result", 0)
                },
                "context": {
                    "diagnostics": []
                }
            }),
        )
        .await;
    assert!(
        response_array(&code_actions)
            .iter()
            .any(|action| action["title"] == "Simplify unnecessary null branch"),
        "the conditional-null refactor should be available: {code_actions}"
    );

    let inlay_hints = server
        .request(
            "textDocument/inlayHint",
            json!({
                "textDocument": {"uri": workspace.uri},
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 100, "character": 0}
                }
            }),
        )
        .await;
    assert!(
        !response_array(&inlay_hints).is_empty(),
        "analyzed bindings should produce inlay hints"
    );

    let local_value_use = position(SOURCE, "localValue", 2);
    let highlights = server
        .request(
            "textDocument/documentHighlight",
            document_position(&workspace.uri, local_value_use.clone()),
        )
        .await;
    assert!(
        response_array(&highlights).len() >= 3,
        "documentation, declaration, and use should be highlighted: {highlights}"
    );

    let references = server
        .request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": workspace.uri},
                "position": local_value_use.clone(),
                "context": {"includeDeclaration": true}
            }),
        )
        .await;
    assert!(
        response_array(&references).len() >= 3,
        "references should include documentation, declaration, and use: {references}"
    );
    assert!(
        response_array(&references)
            .iter()
            .any(|location| location["uri"] == workspace.related_uri),
        "global references should cross open workspace documents: {references}"
    );

    let prepare_rename = server
        .request(
            "textDocument/prepareRename",
            document_position(&workspace.uri, local_value_use.clone()),
        )
        .await;
    assert_eq!(
        prepare_rename["start"],
        position(SOURCE, "localValue", 2),
        "prepare rename should identify the requested symbol"
    );

    let rename = server
        .request(
            "textDocument/rename",
            json!({
                "textDocument": {"uri": workspace.uri},
                "position": local_value_use.clone(),
                "newName": "renamedValue"
            }),
        )
        .await;
    assert!(
        rename["changes"][&workspace.uri]
            .as_array()
            .is_some_and(|edits| edits.len() >= 3),
        "rename should update documentation, declaration, and use: {rename}"
    );
    assert!(
        rename["changes"][&workspace.related_uri]
            .as_array()
            .is_some_and(|edits| edits.len() == 1),
        "rename should include references in another workspace document: {rename}"
    );

    let definition = server
        .request(
            "textDocument/definition",
            document_position(&workspace.uri, local_value_use),
        )
        .await;
    assert_eq!(definition["uri"], workspace.uri);
    assert_eq!(
        definition["range"]["start"],
        position(SOURCE, "localValue", 1),
        "go-to-definition should resolve the local declaration"
    );

    let hierarchy = server
        .request(
            "textDocument/prepareTypeHierarchy",
            document_position(&workspace.uri, position(SOURCE, "ZZ", 0)),
        )
        .await;
    let zz = response_array(&hierarchy)
        .first()
        .expect("ZZ should prepare a type hierarchy item")
        .clone();
    assert_eq!(zz["name"], "ZZ");
    let supertypes = server
        .request("typeHierarchy/supertypes", json!({"item": zz.clone()}))
        .await;
    assert!(
        response_array(&supertypes)
            .iter()
            .any(|item| item["name"] == "Number"),
        "ZZ should have Number as a static supertype: {supertypes}"
    );
    let subtypes = server
        .request("typeHierarchy/subtypes", json!({"item": zz}))
        .await;
    assert!(
        subtypes.is_array(),
        "known types should return a subtype collection"
    );

    let formatting = server
        .request(
            "textDocument/formatting",
            json!({
                "textDocument": {"uri": workspace.uri},
                "options": {
                    "tabSize": 4,
                    "insertSpaces": true
                }
            }),
        )
        .await;
    assert!(
        !response_array(&formatting).is_empty(),
        "the intentionally unformatted example should produce edits"
    );

    let folding = server
        .request(
            "textDocument/foldingRange",
            json!({"textDocument": {"uri": workspace.uri}}),
        )
        .await;
    assert!(
        !response_array(&folding).is_empty(),
        "the multiline conditional should produce a folding range"
    );

    let workspace_symbols = server
        .request("workspace/symbol", json!({"query": ""}))
        .await;
    assert!(
        response_array(&workspace_symbols).iter().any(|symbol| {
            symbol["name"] == "double" && symbol["location"]["uri"] == workspace.uri
        }),
        "workspace symbols should include document symbols from the primary source file: {workspace_symbols}"
    );
    assert!(
        response_array(&workspace_symbols).iter().any(|symbol| {
            symbol["name"] == "crossFileResult"
                && symbol["location"]["uri"] == workspace.related_uri
        }),
        "workspace symbols should include document symbols from other source files: {workspace_symbols}"
    );

    server
        .notify(
            "workspace/didChangeConfiguration",
            json!({
                "settings": {
                    "m2-ls": {
                        "diagnostics": {
                            "enabled": false
                        },
                        "formatting": {
                            "indentWidth": 2,
                            "useTabs": false,
                            "compactFactorOperators": false
                        },
                        "inlayHints": {
                            "expressionTypes": false
                        }
                    }
                }
            }),
        )
        .await;
    server
        .wait_for_server_request("workspace/inlayHint/refresh")
        .await;
    let disabled_diagnostics = server
        .wait_for_notification("textDocument/publishDiagnostics")
        .await;
    assert!(
        response_array(&disabled_diagnostics["params"]["diagnostics"]).is_empty(),
        "disabling diagnostics should republish an empty collection"
    );
    let configured_formatting = server
        .request(
            "textDocument/formatting",
            json!({
                "textDocument": {"uri": workspace.uri},
                "options": {
                    "tabSize": 8,
                    "insertSpaces": false
                }
            }),
        )
        .await;
    assert!(
        response_array(&configured_formatting)
            .first()
            .and_then(|edit| edit["newText"].as_str())
            .is_some_and(|text| text.contains("  encoded")),
        "server formatting settings should override client formatting options: {configured_formatting}"
    );
    assert!(
        server
            .server_requests
            .iter()
            .any(|method| method == "workspace/inlayHint/refresh"),
        "changing negotiated inlay-hint settings should request a client refresh"
    );

    server
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": workspace.uri,
                    "version": 2
                },
                "contentChanges": [{
                    "range": {
                        "start": {"line": 2, "character": 11},
                        "end": {"line": 2, "character": 12}
                    },
                    "text": "2"
                }]
            }),
        )
        .await;
    let changed_hover = server
        .request(
            "textDocument/hover",
            document_position(&workspace.uri, position(SOURCE, "localValue", 1)),
        )
        .await;
    assert!(
        changed_hover["contents"]["value"]
            .as_str()
            .is_some_and(|markdown| markdown.contains("User-defined binding")),
        "incremental document updates should rebuild analysis: {changed_hover}"
    );

    server
        .notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": workspace.uri}}),
        )
        .await;
    let closed_hover = server
        .request(
            "textDocument/hover",
            document_position(&workspace.uri, position(SOURCE, "localValue", 1)),
        )
        .await;
    assert!(closed_hover.is_null());

    server.shutdown().await;
}

#[tokio::test]
async fn runtime_syntax_example_reaches_analysis_without_error_diagnostics() {
    let source = include_str!("../fixtures/weird_valid_syntax.m2");
    let workspace = TestWorkspace::new(source);
    let mut server = LspProcess::spawn().await;
    server.initialize(&workspace.root_uri()).await;
    server
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": workspace.uri,
                    "languageId": "macaulay2",
                    "version": 1,
                    "text": source
                }
            }),
        )
        .await;

    let diagnostics = server
        .wait_for_notification("textDocument/publishDiagnostics")
        .await;
    let errors = response_array(&diagnostics["params"]["diagnostics"])
        .iter()
        .filter(|diagnostic| diagnostic["severity"] == 1)
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "runtime-valid syntax should complete the pipeline without errors: {errors:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn package_objects_become_visible_only_after_their_source_inclusion() {
    let source = "toJSON\nneedsPackage \"JSON\"\ntoJSON\n";
    let workspace = TestWorkspace::new(source);
    let mut server = LspProcess::spawn().await;
    server.initialize(&workspace.root_uri()).await;
    server
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": workspace.uri,
                    "languageId": "macaulay2",
                    "version": 1,
                    "text": source
                }
            }),
        )
        .await;
    server
        .wait_for_notification("textDocument/publishDiagnostics")
        .await;

    let before = server
        .request(
            "textDocument/hover",
            document_position(&workspace.uri, json!({"line": 0, "character": 1})),
        )
        .await;
    assert!(
        before.is_null(),
        "JSON must not be registered before needsPackage: {before}"
    );

    let after = server
        .request(
            "textDocument/hover",
            document_position(&workspace.uri, json!({"line": 2, "character": 1})),
        )
        .await;
    assert!(
        after["contents"]["value"]
            .as_str()
            .is_some_and(|markdown| markdown.contains("Package: `JSON`")),
        "JSON must be registered after needsPackage: {after}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn lambda_continuations_and_untyped_local_copies_keep_editor_roles() {
    let source = "\
expandMacro (Macro, String) := String => (m, block) ->
resultSource (transformOf m)(tokenStream parseMacroTree block)

matchingMacroClose = (src, bodyStart, outerName) -> (
    nestedNames := {};
    k := bodyStart;
)
";
    let workspace = TestWorkspace::new(source);
    let mut server = LspProcess::spawn().await;
    let initialized = server.initialize(&workspace.root_uri()).await;
    server
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": workspace.uri,
                    "languageId": "macaulay2",
                    "version": 1,
                    "text": source
                }
            }),
        )
        .await;
    server
        .wait_for_notification("textDocument/publishDiagnostics")
        .await;

    let formatting = server
        .request(
            "textDocument/formatting",
            json!({
                "textDocument": {"uri": workspace.uri},
                "options": {
                    "tabSize": 4,
                    "insertSpaces": true
                }
            }),
        )
        .await;
    let formatted = response_array(&formatting)
        .first()
        .and_then(|edit| edit["newText"].as_str())
        .expect("the unindented lambda body should produce a whole-document edit");
    assert!(
        formatted.contains(
            "(m, block) ->\n    resultSource (transformOf m)(tokenStream parseMacroTree block)"
        ),
        "the lambda body should be indented as an operator continuation: {formatted}"
    );

    let semantic_tokens = server
        .request(
            "textDocument/semanticTokens/full",
            json!({"textDocument": {"uri": workspace.uri}}),
        )
        .await;
    let variable_type = response_array(
        &initialized["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"],
    )
    .iter()
    .position(|token_type| token_type == "variable")
    .expect("the negotiated semantic-token legend should contain variable")
        as u64;
    let target = position(source, "k :=", 0);
    let target_line = target["line"]
        .as_u64()
        .expect("the target line should be numeric");
    let target_character = target["character"]
        .as_u64()
        .expect("the target character should be numeric");
    let mut line = 0;
    let mut character = 0;
    let token_type = response_array(&semantic_tokens["data"])
        .chunks_exact(5)
        .find_map(|token| {
            let delta_line = token[0].as_u64()?;
            let delta_character = token[1].as_u64()?;
            let length = token[2].as_u64()?;
            if delta_line == 0 {
                character += delta_character;
            } else {
                line += delta_line;
                character = delta_character;
            }
            (line == target_line
                && target_character >= character
                && target_character < character + length)
                .then(|| token[3].as_u64())
                .flatten()
        })
        .expect("the local k binding should have a semantic token");
    assert_eq!(
        token_type, variable_type,
        "a local copy of an untyped parameter should remain a variable"
    );

    server.shutdown().await;
}
