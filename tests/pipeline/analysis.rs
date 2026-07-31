//! End-to-end analysis behavior observed through standard LSP capabilities.

use serde_json::json;

use crate::support::{position, response_array, DocumentSession};

fn diagnostic_lines(session: &DocumentSession, code: &str) -> Vec<u64> {
    session
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic["code"] == code)
        .filter_map(|diagnostic| diagnostic["range"]["start"]["line"].as_u64())
        .collect()
}

async fn replace_and_assert_diagnostic(
    session: &mut DocumentSession,
    source: &str,
    code: &str,
    expected: bool,
) {
    session.replace(source).await;
    assert_eq!(
        session.diagnostic_codes().contains(&code),
        expected,
        "unexpected {code} diagnostic state for:\n{source}\nall diagnostics: {:?}",
        session.diagnostics()
    );
}

async fn hover_type_at(session: &mut DocumentSession, line: u64, character: u64) -> String {
    let hover = session
        .request(
            "textDocument/hover",
            json!({
                "textDocument": {"uri": session.uri()},
                "position": {
                    "line": line,
                    "character": character
                }
            }),
        )
        .await;
    let markdown = hover["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a markdown hover at {line}:{character}, got {hover}"));
    markdown
        .lines()
        .find_map(|line| line.strip_prefix("Type: `")?.strip_suffix('`'))
        .unwrap_or_else(|| panic!("hover should contain a type line: {markdown}"))
        .to_string()
}

async fn inlay_labels(session: &mut DocumentSession) -> Vec<String> {
    let hints = session
        .request(
            "textDocument/inlayHint",
            json!({
                "textDocument": {"uri": session.uri()},
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 100, "character": 0}
                }
            }),
        )
        .await;
    hints
        .as_array()
        .unwrap_or_else(|| panic!("expected inlay hints, got {hints}"))
        .iter()
        .filter_map(|hint| hint["label"].as_str().map(str::to_string))
        .collect()
}

async fn inlay_labels_by_line(session: &mut DocumentSession) -> Vec<(u64, String)> {
    let hints = session
        .request(
            "textDocument/inlayHint",
            json!({
                "textDocument": {"uri": session.uri()},
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 100, "character": 0}
                }
            }),
        )
        .await;
    hints
        .as_array()
        .unwrap_or_else(|| panic!("expected inlay hints, got {hints}"))
        .iter()
        .filter_map(|hint| {
            Some((
                hint["position"]["line"].as_u64()?,
                hint["label"].as_str()?.to_string(),
            ))
        })
        .collect()
}

async fn semantic_tokens(session: &mut DocumentSession) -> Vec<(u64, u64, u64, u64)> {
    let response = session
        .request(
            "textDocument/semanticTokens/full",
            json!({"textDocument": {"uri": session.uri()}}),
        )
        .await;
    let encoded = response_array(&response["data"]);
    let mut line = 0;
    let mut character = 0;
    encoded
        .chunks_exact(5)
        .map(|token| {
            let delta_line = token[0]
                .as_u64()
                .expect("token line delta should be numeric");
            let delta_character = token[1]
                .as_u64()
                .expect("token character delta should be numeric");
            if delta_line == 0 {
                character += delta_character;
            } else {
                line += delta_line;
                character = delta_character;
            }
            (
                line,
                character,
                token[3].as_u64().expect("token type should be numeric"),
                token[4]
                    .as_u64()
                    .expect("token modifiers should be numeric"),
            )
        })
        .collect()
}

fn token_at(
    tokens: &[(u64, u64, u64, u64)],
    source: &str,
    needle: &str,
    occurrence: usize,
) -> (u64, u64) {
    let position = position(source, needle, occurrence);
    let line = position["line"]
        .as_u64()
        .expect("fixture line should be numeric");
    let character = position["character"]
        .as_u64()
        .expect("fixture character should be numeric");
    tokens
        .iter()
        .find_map(|(token_line, token_character, token_type, modifiers)| {
            (*token_line == line && *token_character == character)
                .then_some((*token_type, *modifiers))
        })
        .unwrap_or_else(|| panic!("missing semantic token for {needle:?} occurrence {occurrence}"))
}

#[tokio::test]
async fn registered_source_roles_drive_semantic_tokens_through_the_server() {
    let source = concat!(
        "p = method(TypicalValue => List)\n",
        "p(ZZ) := Array => x -> [x]\n",
        "f(Strategy => LongPolynomial)\n",
        "R.name\n",
        "h#\"key\"\n",
        "match \"pattern\"\n",
        "needsPackage \"JSON\"\n",
    );
    let mut session = DocumentSession::open(source).await;
    let tokens = semantic_tokens(&mut session).await;

    for (needle, expected_role, description) in [
        ("ZZ", "type", "method domain"),
        ("Array", "type", "method codomain"),
        ("Strategy", "enumMember", "option key"),
        ("name", "property", "quoted member key"),
        ("\"key\"", "property", "lookup key"),
        ("\"pattern\"", "regexp", "regexp argument"),
        ("\"JSON\"", "namespace", "package argument"),
    ] {
        assert_eq!(
            token_at(&tokens, source, needle, 0).0,
            session.semantic_token_type(expected_role),
            "unexpected semantic role for {description}"
        );
    }

    session.shutdown().await;
}

#[tokio::test]
async fn installation_and_syntax_diagnostics_run_through_the_server_pipeline() {
    let mut session = DocumentSession::open("ZZ > ZZ := (a, b) -> a\n").await;
    assert!(session.diagnostic_codes().contains(&"E09"));

    for (source, code, expected) in [
        ("ZZ * ZZ := (a, b) -> a\n", "E09", false),
        ("ZZ * ZZ := (a) -> a\n", "E10", true),
        ("ZZ * ZZ := a -> a\n", "E10", false),
        ("ZZ * ZZ = (a, b, c) -> c\n", "E10", false),
        ("ZZ * ZZ = (a, b) -> a\n", "E10", true),
        ("f = x -> x\nf ZZ := y -> y\n", "E08", true),
        ("f = method()\nf ZZ := y -> y\n", "E08", false),
        ("f = method()\nf ZZ = x -> x\n", "E11", true),
        ("f = x -> x\nf ZZ = y -> y\n", "E11", true),
        ("f = method()\nf ZZ := x -> x\n", "E11", false),
        ("ZZ * ZZ = (a, b, c) -> c\n", "E11", false),
        ("if x then y\n    else z", "E00", true),
        (
            "apply(-3..3, i -> try 1/i then 1 / i except err do err)",
            "E00",
            false,
        ),
        ("if x then y else z", "E00", false),
        ("if x then y", "E00", false),
        ("gb(I, strategy => 4)\n", "E06", true),
        ("hashTable {a => 1, b => 2}\n", "E06", false),
        ("x.3\n", "E02", true),
        ("x .3\n", "E02", false),
    ] {
        replace_and_assert_diagnostic(&mut session, source, code, expected).await;
    }

    session.shutdown().await;
}

#[tokio::test]
async fn assignment_and_protection_diagnostics_preserve_source_sensitive_analysis() {
    let source = concat!(
        "[x, y] = [a, b, c]\n",
        "[p, q] = {r}\n",
        "[m, n] = (s)\n",
        "[u, v] = [g, h]\n",
        "[i, [j, k]] = [1, {2, 3, 4}]\n",
        "[c, d] = ()\n",
        "[e, f] = (a, b)\n",
    );
    let mut session = DocumentSession::open(source).await;
    assert_eq!(diagnostic_lines(&session, "E05"), vec![0, 1, 4, 5]);

    session
        .replace("[x, y] = [ignored; 2, 3]\n[x, y, z] = [, 1,]\n[x, y] = [, 1,]\n")
        .await;
    assert_eq!(diagnostic_lines(&session, "E05"), vec![2]);

    session
        .replace(
            "x#i := e\n(x+1,y) = (1,2)\n(x+1,y) := (1,2)\n(f()) <- (1)\nsource(String,Number) := peek\np(ZZ, ZZ) := (i, j) -> {i, j}\n",
        )
        .await;
    assert_eq!(diagnostic_lines(&session, "E04"), vec![0]);
    assert_eq!(diagnostic_lines(&session, "E03"), vec![1, 2]);

    session
        .replace(
            "assigned = target\nprotect assigned\nprotect unassigned\nprotect later\nlater = target\n",
        )
        .await;
    assert_eq!(diagnostic_lines(&session, "E12"), vec![1]);

    session
        .replace(
            "x = y\nprotect symbol x\nprotect (if c then symbol x else symbol y)\nprotect (if c then 1 else symbol y)\nprotect (1 + 2)\n",
        )
        .await;
    assert_eq!(diagnostic_lines(&session, "E13"), vec![2, 3]);

    session.replace("f = x -> protect x\n").await;
    assert_eq!(diagnostic_lines(&session, "E12"), vec![0]);

    session
        .replace("protect ZZ\nx = y\nprotect = f\nprotect x\n")
        .await;
    assert_eq!(diagnostic_lines(&session, "E12"), vec![0]);

    session.replace("f := x -> x\nx = 1\n").await;
    assert!(!session.diagnostic_codes().contains(&"E07"));

    session
        .replace(
            "if condition then (\n  conditionalExport = true;\n  branchLocal := 1;\n);\nif conditionalExport == true then null\n",
        )
        .await;
    assert_eq!(diagnostic_lines(&session, "E07"), vec![2]);

    session.shutdown().await;
}

#[tokio::test]
async fn inferred_binding_types_flow_to_hover_across_language_constructs() {
    let mut session = DocumentSession::open("value := 1\nvalue\n").await;
    assert_eq!(hover_type_at(&mut session, 1, 0).await, "ZZ");

    for (source, probes) in [
        (
            "clearAll = new Command from { () -> () }\nclearAll\n",
            vec![(1, 0, "Command")],
        ),
        (
            "I := new Ideal from {}\nR := ring I\nS := ring unknownName\nR\nS\n",
            vec![(3, 0, "Ring"), (4, 0, "Ring")],
        ),
        (
            "x := 1\ny := 2\nz := x + y\nz\n",
            vec![(3, 0, "ZZ")],
        ),
        (
            "listed := for i to 3 list i\ndone := for j to 3 do j\nlooped := while condition do 1\nlisted\ndone\nlooped\n",
            vec![(3, 0, "List"), (4, 0, "Nothing"), (5, 0, "Nothing")],
        ),
        (
            "p = method(Binary => true, TypicalValue => List)\np(ZZ,ZZ) := p(List,ZZ) := (i,j) -> {i,j}\nx := p(1, 2)\nx\n",
            vec![(3, 0, "List")],
        ),
        (
            "f = method()\nf ZZ := x -> -x\ny := f 1\ny\n",
            vec![(3, 0, "Thing")],
        ),
        (
            "f = method(TypicalValue => List)\nf ZZ := Ring => x -> x\ny := f 1\ny\n",
            vec![(3, 0, "Ring")],
        ),
        ("y := unindexedName\ny\n", vec![(1, 0, "Symbol")]),
        (
            "l := {1,2}\na := [1,2]\nb := <|1,2|>\ne := ()\nf := (1)\ng := (1,2)\nl\na\nb\ne\nf\ng\n",
            vec![
                (6, 0, "List"),
                (7, 0, "Array"),
                (8, 0, "AngleBarList"),
                (9, 0, "Sequence"),
                (10, 0, "ZZ"),
                (11, 0, "Sequence"),
            ],
        ),
    ] {
        session.replace(source).await;
        for (line, character, expected) in probes {
            assert_eq!(
                hover_type_at(&mut session, line, character).await,
                expected,
                "unexpected inferred type at {line}:{character} for:\n{source}"
            );
        }
    }

    session.replace("I = ideal(12,18)\nI\n").await;
    let ideal_signature = session
        .request(
            "textDocument/signatureHelp",
            json!({
                "textDocument": {"uri": session.uri()},
                "position": {"line": 0, "character": 11}
            }),
        )
        .await;
    assert_eq!(
        hover_type_at(&mut session, 1, 0).await,
        "Ideal",
        "ideal call signature: {ideal_signature}"
    );

    for (source, expected) in [
        (
            "joined := if condition then 1 else 2.0\njoined\n",
            ": ZZ | RR",
        ),
        ("joined := if condition then 1\njoined\n", ": ZZ | Nothing"),
        (
            "joined := try unknownName then 1 else 2.0\njoined\n",
            ": ZZ | RR",
        ),
        ("fallback := try 1\nfallback\n", ": ZZ | Nothing"),
    ] {
        session.replace(source).await;
        let labels = inlay_labels(&mut session).await;
        assert!(
            labels.iter().any(|label| label == expected),
            "expected {expected:?} in inlay labels for:\n{source}\ngot {labels:?}"
        );
    }

    session.shutdown().await;
}

#[tokio::test]
async fn core_method_function_single_call_reaches_hover() {
    let mut session = DocumentSession::open("I = ideal(12,18)\nI\n").await;
    assert_eq!(hover_type_at(&mut session, 1, 0).await, "Ideal");
    session.shutdown().await;
}

#[tokio::test]
async fn scopes_bindings_and_callable_metadata_drive_editor_capabilities() {
    let source = concat!(
        "outside := 1\n",
        "f = parameter -> (\n",
        "  inside := parameter;\n",
        "  ins\n",
        ")\n",
        "out\n",
        "[a, [b, c]] = [1, {2, 3}]\n",
        "p = method(Binary => true, TypicalValue => List)\n",
        "p(ZZ,ZZ) := p(List,ZZ) := (i,j) -> {i,j}\n",
        "result := p(1,2)\n",
        "result\n",
    );
    let mut session = DocumentSession::open(source).await;

    let local_completions = session.completion_labels("ins", 1).await;
    assert!(local_completions.iter().any(|label| label == "inside"));
    let global_completions = session.completion_labels("out", 1).await;
    assert!(global_completions.iter().any(|label| label == "outside"));
    assert!(!global_completions.iter().any(|label| label == "inside"));

    let definition = session
        .request_at("textDocument/definition", "parameter", 1)
        .await;
    assert_eq!(
        definition["range"]["start"],
        position(source, "parameter", 0)
    );

    let symbols = session
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": session.uri()}}),
        )
        .await;
    let symbol_names = response_array(&symbols)
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect::<Vec<_>>();
    for expected in ["outside", "f", "a", "b", "c", "p", "result"] {
        assert!(
            symbol_names.contains(&expected),
            "document symbols should contain {expected}: {symbols}"
        );
    }

    let signatures = session
        .request_at("textDocument/signatureHelp", "1,2", 0)
        .await;
    assert!(
        response_array(&signatures["signatures"])
            .iter()
            .any(|signature| signature["label"] == "p(ZZ, ZZ) -> List"),
        "local installed method signatures should reach signature help: {signatures}"
    );
    assert_eq!(hover_type_at(&mut session, 10, 0).await, "List");

    let tokens = semantic_tokens(&mut session).await;
    let function_token = session.semantic_token_type("function");
    let parameter_token = session.semantic_token_type("parameter");
    let (function_type, function_modifiers) = token_at(&tokens, source, "f", 0);
    assert_eq!(
        function_type, function_token,
        "local callables should be function tokens"
    );
    assert_ne!(
        function_modifiers & (1 << 3),
        0,
        "the callable declaration should carry the declaration modifier"
    );
    for occurrence in [0, 1] {
        assert_eq!(
            token_at(&tokens, source, "parameter", occurrence).0,
            parameter_token,
            "function parameters should retain the standard parameter role"
        );
    }

    session.shutdown().await;
}

#[tokio::test]
async fn algebraic_runtime_types_and_generator_rebinding_reach_hover() {
    let source = concat!(
        "R = QQ[a..d]\n",
        "I = ideal(a^2,b)\n",
        "Q = R/I\n",
        "M = Q^2\n",
        "J = ideal(a,b)\n",
        "N = M/J\n",
        "a\n",
        "b\n",
        "R\n",
        "I\n",
        "Q\n",
        "M\n",
        "J\n",
        "N\n",
    );
    let mut session = DocumentSession::open(source).await;
    let hints = inlay_labels_by_line(&mut session).await;
    for (line, expected) in [
        (6, "Q"),
        (7, "Q"),
        (8, "PolynomialRing"),
        (9, "Ideal"),
        (10, "QuotientRing"),
        (11, "Module"),
        (12, "Ideal"),
        (13, "Module"),
    ] {
        assert!(
            hints
                .iter()
                .any(|(hint_line, label)| *hint_line == line && label == &format!(": {expected}")),
            "missing {expected} hint on line {line}: {hints:?}"
        );
    }

    session
        .replace(
            "before := u_0\nS = ZZ/101[t_0..t_3]\nafter := t_1\nV = QQ[Variables => 3, VariableBaseName => v]\ngenerated := v_2\nt\nS\nbefore\nafter\ngenerated\n",
        )
        .await;
    let hints = inlay_labels_by_line(&mut session).await;
    for (line, expected) in [
        (5, "IndexedVariableTable"),
        (6, "PolynomialRing"),
        (7, "IndexedVariable"),
        (8, "S"),
        (9, "V"),
    ] {
        assert!(
            hints
                .iter()
                .any(|(hint_line, label)| *hint_line == line && label == &format!(": {expected}")),
            "missing {expected} hint on line {line}: {hints:?}"
        );
    }

    session
        .replace("A = QQ[x]\nJ = ideal(x^2)\nQ = A/J\nx\nQ\nT = QQ[y]/J\ny\nT\n")
        .await;
    let hints = inlay_labels_by_line(&mut session).await;
    for (line, expected) in [(3, "Q"), (4, "QuotientRing"), (6, "T"), (7, "QuotientRing")] {
        assert!(
            hints
                .iter()
                .any(|(hint_line, label)| *hint_line == line && label == &format!(": {expected}")),
            "missing {expected} hint on line {line}: {hints:?}"
        );
    }

    session
        .replace("R = QQ[x]\nf = x^2\nJ = ideal(f)\nf\nJ\n")
        .await;
    let hints = inlay_labels_by_line(&mut session).await;
    assert!(hints.contains(&(3, ": RingElement".to_string())));
    assert!(hints.contains(&(4, ": Ideal".to_string())));

    session.shutdown().await;
}
