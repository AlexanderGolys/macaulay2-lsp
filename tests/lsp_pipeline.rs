//! Black-box coverage for the complete stdio language-server pipeline.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::{env, fs};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use tower_lsp::lsp_types::Url;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const SOURCE: &str = include_str!("fixtures/capability_spectrum.m2");

static TEMP_DIRECTORY_ID: AtomicUsize = AtomicUsize::new(0);

/// A child `m2-ls` process with framed JSON-RPC input and output.
struct LspProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    pending: VecDeque<Value>,
    server_requests: Vec<String>,
}

impl LspProcess {
    async fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_m2-ls"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("the m2-ls test binary should start");
        let stdin = child
            .stdin
            .take()
            .expect("the language server should have stdin");
        let stdout = child
            .stdout
            .take()
            .expect("the language server should have stdout");

        Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            pending: VecDeque::new(),
            server_requests: Vec::new(),
        }
    }

    async fn initialize(&mut self, root_uri: &str) -> Value {
        self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {
                    "workspace": {
                        "inlayHint": {
                            "refreshSupport": true
                        }
                    },
                    "textDocument": {
                        "semanticTokens": {
                            "requests": {
                                "full": true
                            },
                            "tokenTypes": [],
                            "tokenModifiers": [],
                            "formats": ["relative"],
                            "augmentsSyntaxTokens": true
                        },
                        "typeHierarchy": {
                            "dynamicRegistration": false
                        }
                    }
                },
                "initializationOptions": {
                    "inlayHints": {
                        "expressionTypes": true
                    }
                }
            }),
        )
        .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method
        });
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message).await;

        loop {
            let message = if let Some(index) = self.pending.iter().position(|message| {
                message.get("method").is_none()
                    && message.get("id").and_then(Value::as_u64) == Some(id)
            }) {
                self.pending
                    .remove(index)
                    .expect("the indexed pending response should exist")
            } else {
                self.read_message().await
            };
            if message.get("method").is_some() {
                self.handle_server_message(message).await;
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                self.pending.push_back(message);
                continue;
            }
            assert!(
                message.get("error").is_none(),
                "{method} returned an error: {message}"
            );
            return message.get("result").cloned().unwrap_or(Value::Null);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let mut message = json!({
            "jsonrpc": "2.0",
            "method": method
        });
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message).await;
    }

    async fn wait_for_notification(&mut self, method: &str) -> Value {
        if let Some(index) = self
            .pending
            .iter()
            .position(|message| message.get("method") == Some(&Value::String(method.to_string())))
        {
            return self
                .pending
                .remove(index)
                .expect("the indexed pending notification should exist");
        }

        loop {
            let message = self.read_message().await;
            if message.get("method").and_then(Value::as_str) == Some(method)
                && message.get("id").is_none()
            {
                return message;
            }
            if message.get("method").is_some() {
                self.handle_server_message(message).await;
            } else {
                self.pending.push_back(message);
            }
        }
    }

    async fn wait_for_server_request(&mut self, method: &str) {
        loop {
            let message = self.read_message().await;
            if message.get("method").and_then(Value::as_str) == Some(method)
                && message.get("id").is_some()
            {
                self.handle_server_message(message).await;
                return;
            }
            if message.get("method").is_some() {
                self.handle_server_message(message).await;
            } else {
                self.pending.push_back(message);
            }
        }
    }

    async fn shutdown(mut self) {
        let result = self.request("shutdown", Value::Null).await;
        assert!(result.is_null());
        self.notify("exit", Value::Null).await;
        drop(self.stdin);
        let status = timeout(RESPONSE_TIMEOUT, self.child.wait())
            .await
            .expect("the language server should exit after shutdown")
            .expect("the language-server process should be waitable");
        assert!(status.success(), "language server exited with {status}");
    }

    async fn handle_server_message(&mut self, message: Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            self.pending.push_back(message);
            return;
        };
        let Some(id) = message.get("id").cloned() else {
            self.pending.push_back(message);
            return;
        };
        self.server_requests.push(method.to_string());
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": null
        }))
        .await;
    }

    async fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).expect("JSON-RPC messages should serialize");
        self.stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .expect("the JSON-RPC header should write");
        self.stdin
            .write_all(&body)
            .await
            .expect("the JSON-RPC body should write");
        self.stdin
            .flush()
            .await
            .expect("the JSON-RPC message should flush");
    }

    async fn read_message(&mut self) -> Value {
        timeout(RESPONSE_TIMEOUT, async {
            let mut content_length = None;
            loop {
                let mut header = String::new();
                let read = self
                    .stdout
                    .read_line(&mut header)
                    .await
                    .expect("the language-server header should be readable");
                assert_ne!(read, 0, "language server closed stdout before responding");
                if header == "\r\n" {
                    break;
                }
                if let Some(length) = header.strip_prefix("Content-Length:") {
                    content_length = Some(
                        length
                            .trim()
                            .parse::<usize>()
                            .expect("Content-Length should be an integer"),
                    );
                }
            }

            let mut body = vec![0; content_length.expect("response should have Content-Length")];
            self.stdout
                .read_exact(&mut body)
                .await
                .expect("the language-server response body should be readable");
            serde_json::from_slice(&body).expect("the language server should emit JSON")
        })
        .await
        .expect("timed out waiting for the language server")
    }
}

/// A unique on-disk workspace removed when its test finishes.
struct TestWorkspace {
    root: std::path::PathBuf,
    source_path: std::path::PathBuf,
    uri: String,
    related_source_path: std::path::PathBuf,
    related_uri: String,
}

impl TestWorkspace {
    fn new(source: &str) -> Self {
        let unique = TEMP_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            env::temp_dir().join(format!("m2-ls-integration-{}-{unique}", std::process::id()));
        fs::create_dir(&root).expect("the integration workspace should be created");
        let source_path = root.join("capability_spectrum.m2");
        fs::write(&source_path, source).expect("the integration fixture should be written");
        let uri = Url::from_file_path(&source_path)
            .expect("the fixture path should become a URI")
            .to_string();
        let related_source_path = root.join("related.m2");
        fs::write(&related_source_path, "crossFileResult = localValue\n")
            .expect("the related integration fixture should be written");
        let related_uri = Url::from_file_path(&related_source_path)
            .expect("the related fixture path should become a URI")
            .to_string();

        Self {
            root,
            source_path,
            uri,
            related_source_path,
            related_uri,
        }
    }

    fn root_uri(&self) -> String {
        Url::from_directory_path(&self.root)
            .expect("the workspace path should become a URI")
            .to_string()
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.source_path);
        let _ = fs::remove_file(&self.related_source_path);
        let _ = fs::remove_dir(&self.root);
    }
}

fn position(source: &str, needle: &str, occurrence: usize) -> Value {
    let byte_index = source
        .match_indices(needle)
        .nth(occurrence)
        .map(|(index, _)| index)
        .unwrap_or_else(|| panic!("fixture should contain occurrence {occurrence} of {needle:?}"));
    let prefix = &source[..byte_index];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let character = source[line_start..byte_index].encode_utf16().count();
    json!({
        "line": line,
        "character": character
    })
}

fn document_position(uri: &str, position: Value) -> Value {
    json!({
        "textDocument": {
            "uri": uri
        },
        "position": position
    })
}

fn response_array(result: &Value) -> &[Value] {
    result
        .as_array()
        .unwrap_or_else(|| panic!("expected an array response, got {result}"))
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
                    "text": "crossFileResult = localValue\n"
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
        .request("workspace/symbol", json!({"query": "ideal"}))
        .await;
    assert!(
        workspace_symbols.is_array(),
        "workspace symbols should return an LSP collection"
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
            .is_some_and(|markdown| markdown.contains("User-defined variable")),
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
    let source = include_str!("fixtures/weird_valid_syntax.m2");
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
