//! End-to-end analysis behavior observed through standard LSP capabilities.

use serde_json::{json, Value};

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

async fn inlay_hints(session: &mut DocumentSession) -> Vec<Value> {
    session
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
        .await
        .as_array()
        .cloned()
        .expect("expected inlay hints")
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
async fn unassigned_symbols_are_enum_members_until_their_binding() {
    let source = "a = b\nZZ[a, b, c]\nb = c\n";
    let mut session = DocumentSession::open(source).await;
    let tokens = semantic_tokens(&mut session).await;
    let enum_member = session.semantic_token_type("enumMember");
    let variable = session.semantic_token_type("variable");

    for occurrence in [0, 1] {
        assert_eq!(token_at(&tokens, source, "a", occurrence).0, variable);
    }
    for occurrence in [0, 1] {
        assert_eq!(token_at(&tokens, source, "b", occurrence).0, enum_member);
    }
    assert_eq!(token_at(&tokens, source, "b", 2).0, variable);
    for occurrence in [0, 1] {
        assert_eq!(token_at(&tokens, source, "c", occurrence).0, enum_member);
    }

    session.shutdown().await;
}

#[tokio::test]
async fn control_flow_conditions_require_booleans_without_function_coloring() {
    let source = concat!(
        "while i do 2;\n",
        "while true do 2;\n",
        "condition := true\n",
        "while condition do 2;\n",
        "while 1 do 2;\n",
        "if j then 2 else 3\n",
        "if false then 2 else 3\n",
        "if condition then 2 else 3\n",
        "if 1 then 2 else 3\n",
        "callable = value -> value\n",
        "if callable then 2 else 3\n",
    );
    let mut session =
        DocumentSession::open_with_related(source, "i = value -> value\nj = value -> value\n")
            .await;
    let tokens = semantic_tokens(&mut session).await;

    assert_eq!(
        token_at(&tokens, source, "i", 1).0,
        session.semantic_token_type("enumMember")
    );
    assert_eq!(
        token_at(&tokens, source, "j", 0).0,
        session.semantic_token_type("enumMember")
    );
    assert_eq!(
        token_at(&tokens, source, "callable", 1).0,
        session.semantic_token_type("variable")
    );
    assert_eq!(diagnostic_lines(&session, "E17"), vec![0, 4]);
    assert_eq!(diagnostic_lines(&session, "E18"), vec![5, 8, 10]);

    session.shutdown().await;
}

#[tokio::test]
async fn only_original_core_compiled_functions_are_builtin() {
    let source = "f = scan\nf\nscan\nscan = f\nscan\n";
    let mut session = DocumentSession::open(source).await;
    let tokens = semantic_tokens(&mut session).await;
    let function = session.semantic_token_type("function");
    let builtin = session.semantic_token_modifier("builtin");

    for occurrence in [0, 1] {
        let (token_type, modifiers) = token_at(&tokens, source, "scan", occurrence);
        assert_eq!(token_type, function);
        assert_eq!(modifiers, builtin);
    }
    for occurrence in [2, 3] {
        let (token_type, modifiers) = token_at(&tokens, source, "scan", occurrence);
        assert_eq!(token_type, function);
        assert_eq!(modifiers & builtin, 0);
    }
    for occurrence in 0..=2 {
        let (token_type, modifiers) = token_at(&tokens, source, "f", occurrence);
        assert_eq!(token_type, function);
        assert_eq!(modifiers & builtin, 0);
    }

    session.shutdown().await;
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
async fn source_type_descendants_are_classified_through_the_typechecker() {
    let source = "TT = new Type\nTTT = new TT\nTTT\n";
    let mut session = DocumentSession::open(source).await;
    let tokens = semantic_tokens(&mut session).await;
    let class = session.semantic_token_type("class");

    for line in 0..=2 {
        assert_eq!(
            tokens
                .iter()
                .find_map(|(token_line, character, token_type, _)| {
                    (*token_line == line && *character == 0).then_some(*token_type)
                }),
            Some(class),
            "line {} should classify its binding as a class",
            line + 1
        );
    }

    session.shutdown().await;
}

#[tokio::test]
async fn output_references_follow_prior_cell_types_without_semantic_tokens() {
    let source = concat!(
        "1\n",
        "a = oo\n",
        "\"hello\"\n",
        "b = ooo\n",
        "c = o3\n",
        "d = oooo\n",
    );
    let mut session = DocumentSession::open(source).await;

    assert!(
        !session.diagnostic_codes().contains(&"E14"),
        "valid output references should not produce missing-cell warnings: {:?}",
        session.diagnostics()
    );

    for (line, expected) in [(1, "ZZ"), (3, "ZZ"), (4, "String"), (5, "String")] {
        assert_eq!(hover_type_at(&mut session, line, 0).await, expected);
    }

    let tokens = semantic_tokens(&mut session).await;
    for line in [1, 3, 4, 5] {
        assert!(
            tokens
                .iter()
                .all(|(token_line, character, _, _)| *token_line != line || *character != 4),
            "output reference on line {} should not have a semantic token",
            line + 1
        );
    }

    session.replace("1\n\"hidden\";\na = oo\n").await;
    assert_eq!(hover_type_at(&mut session, 2, 0).await, "ZZ");

    session.replace("o100 := \"hello\"\nx = o100\n").await;
    assert_eq!(hover_type_at(&mut session, 1, 0).await, "String");
    let tokens = semantic_tokens(&mut session).await;
    assert!(
        tokens
            .iter()
            .any(|(line, character, _, _)| *line == 1 && *character == 4),
        "a resolved user binding named like an output reference must retain its semantic token"
    );
    assert!(!session.diagnostic_codes().contains(&"E14"));

    session
        .replace("x = oo\ny = o0\nz = o9\nw = oooo\nsymbol oo\n")
        .await;
    for line in 0..=3 {
        assert_eq!(hover_type_at(&mut session, line, 0).await, "Symbol");
    }
    assert_eq!(diagnostic_lines(&session, "E14"), vec![0, 1, 2]);
    for diagnostic in session
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic["code"] == "E14")
    {
        assert_eq!(diagnostic["severity"], 2);
        assert!(diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("unassigned `Symbol`")));
    }

    session.replace("1\nw = oooo\nsymbol o9\n").await;
    assert_eq!(hover_type_at(&mut session, 1, 0).await, "Symbol");
    assert_eq!(diagnostic_lines(&session, "E14"), vec![1]);

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
        ("f = first {ideal}\nf ZZ := y -> y\n", "E08", false),
        ("f = method()\nf ZZ = x -> x\n", "E11", true),
        ("f = x -> x\nf ZZ = y -> y\n", "E11", true),
        ("f = 1\nf ZZ = y -> y\n", "E11", false),
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
async fn control_transfers_require_their_runtime_body() {
    let mut session = DocumentSession::open("f := x -> return x\n").await;

    for (source, expected) in [
        ("f := x -> return x\n", false),
        ("return x\n", true),
        ("if c then break\n", true),
        ("for i from (break i) to 3 list i\n", true),
        ("for i to 3 list if i > 1 then break i\n", false),
        ("for i to 3 do if i > 1 then break i\n", false),
        ("while c do break c\n", false),
        ("for i to 3 list continue i\n", false),
        ("for i to 3 do continue\n", false),
        ("for i to 3 do continue i\n", true),
        ("while c list continue c\n", false),
        ("while c do continue c\n", true),
        ("apply(0..3, i -> continue i)\n", true),
        ("apply({1}, {2}, (i, j) -> continue(i + j))\n", true),
        ("scan(0..3, i -> continue)\n", true),
        ("scan(0..3, i -> continue i)\n", true),
        ("apply(0..3, i -> break i)\n", false),
        ("scan(0..3, i -> break i)\n", false),
        ("f := i -> break i\n", true),
        ("for i to 3 do (f := x -> break; f i)\n", true),
        ("for i to 3 list (for j to 3 do continue i)\n", true),
        ("apply(i -> break i, {1})\n", true),
        ("f := apply -> apply({1}, i -> break i)\n", true),
    ] {
        replace_and_assert_diagnostic(&mut session, source, "E15", expected).await;
        assert!(
            !session.diagnostic_codes().contains(&"E00"),
            "control-transfer fixture should parse:\n{source}\nall diagnostics: {:?}",
            session.diagnostics()
        );
    }

    session.replace("scan(0..3, i -> continue i)\n").await;
    let diagnostic = session
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "E15")
        .expect("value-bearing continue in scan should be rejected");
    assert_eq!(diagnostic["severity"], 1);
    assert!(diagnostic["message"]
        .as_str()
        .is_some_and(|message| message.contains("requires a `list` clause")));

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
        .replace("(x, (y, z, z), w) = (1, [2, \"3\"], 3)\n")
        .await;
    let diagnostic = session
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "E05")
        .expect("nested assignment shape mismatch should be diagnosed");
    assert_eq!(
        diagnostic["range"],
        json!({
            "start": {"line": 0, "character": 4},
            "end": {"line": 0, "character": 13}
        })
    );

    session
        .replace("[x, y] = [ignored; 2, 3]\n[x, y, z] = [, 1,]\n[x, y] = [, 1,]\n")
        .await;
    assert_eq!(diagnostic_lines(&session, "E05"), vec![2]);

    session
        .replace(
            "(x, y) = 1\n[x, y] := \"aa\"\nz = \"a\"; {x, x} = z\n[a, b] = true\n[x] = \"a\"\nf = z -> ((x, y) := z)\n(x, y) := unknownValue\nvalues = {1, 2}; [x, y] = values\nvalues = (1, 2); [x, y] = values\n[a, [b, c]] = [1, 2]\n",
        )
        .await;
    assert_eq!(diagnostic_lines(&session, "E16"), vec![0, 1, 2, 3, 9]);

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
            "ZZ | RR",
        ),
        ("joined := if condition then 1\njoined\n", "ZZ | Nothing"),
        (
            "joined := try unknownName then 1 else 2.0\njoined\n",
            "ZZ | RR",
        ),
        ("fallback := try 1\nfallback\n", "ZZ | Nothing"),
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
async fn callable_aliases_reinstall_methods_and_reassignments_remain_source_ordered() {
    let source = concat!(
        "f = method()\n",
        "f Thing := x -> x\n",
        "g = f\n",
        "f ZZ := QQ => x -> x / 2\n",
        "x = g 1\n",
        "y = f 1\n",
        "g ZZ := ZZ => x -> x * 2\n",
        "z = g 1\n",
        "w = f 1\n",
        "f ZZ := Array => x -> [x]\n",
        "x = f 1\n",
        "y = g 1\n",
        "u = x\n",
        "v = y\n",
    );
    let mut session = DocumentSession::open(source).await;

    for (line, expected) in [
        (4, "QQ"),
        (5, "QQ"),
        (7, "ZZ"),
        (8, "ZZ"),
        (10, "Array"),
        (11, "Array"),
        (12, "Array"),
        (13, "Array"),
    ] {
        assert_eq!(
            hover_type_at(&mut session, line, 0).await,
            expected,
            "unexpected binding type on line {}",
            line + 1
        );
    }

    let hints = inlay_labels_by_line(&mut session).await;
    for line in [10, 11] {
        assert!(
            hints
                .iter()
                .any(|(hint_line, label)| *hint_line == line && label == "Array"),
            "reassignment on line {} should have an Array hint: {hints:?}",
            line + 1
        );
    }

    session.replace("x = 1\na = x\nx = \"a\"\nb = x\n").await;
    let before_reassignment = session
        .request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": session.uri()},
                "position": {"line": 1, "character": 4}
            }),
        )
        .await;
    assert_eq!(before_reassignment["range"]["start"]["line"], 0);

    let after_reassignment = session
        .request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": session.uri()},
                "position": {"line": 3, "character": 4}
            }),
        )
        .await;
    assert_eq!(after_reassignment["range"]["start"]["line"], 2);

    session.shutdown().await;
}

#[tokio::test]
async fn inlay_hints_track_values_destructuring_reassignments_and_parameters() {
    let source = concat!(
        "f = (count, text) -> text\n",
        "value = f(2, \"a\")\n",
        "[x, [y, z]] = [1, {\"a\", 1.5}]\n",
        "x = 2\n",
        "x = if condition then 3 else 4\n",
        "x = if condition then \"a\" else \"b\"\n",
        "broad = method()\n",
        "broad(Thing) := (item) -> item\n",
        "unknown = broad(missing)\n",
        "literalString = \"a\"\n",
        "literalArray = [1]\n",
        "literalInteger = 1\n",
        "literalReal = 1.1\n",
        "literalParenthesized = (2)\n",
        "lambda = argument -> argument\n",
    );
    let mut session = DocumentSession::open(source).await;
    session.set_expression_type_hints(false).await;
    let hints = inlay_hints(&mut session).await;

    let parameter_hints = hints
        .iter()
        .filter(|hint| hint["kind"] == 2)
        .map(|hint| {
            (
                hint["position"]["line"].as_u64().expect("hint line"),
                hint["position"]["character"]
                    .as_u64()
                    .expect("hint character"),
                hint["label"].as_str().expect("hint label"),
            )
        })
        .collect::<Vec<_>>();
    assert!(parameter_hints.contains(&(1, 10, "count:")));
    assert!(parameter_hints.contains(&(1, 13, "text:")));
    assert!(parameter_hints.contains(&(8, 16, "item:")));

    let type_hints = hints
        .iter()
        .filter(|hint| hint["kind"] == 1)
        .map(|hint| {
            (
                hint["position"]["line"].as_u64().expect("hint line"),
                hint["position"]["character"]
                    .as_u64()
                    .expect("hint character"),
                hint["label"].as_str().expect("hint label"),
            )
        })
        .collect::<Vec<_>>();
    for expected in [(2, 16, "ZZ"), (2, 22, "String"), (2, 27, "RR")] {
        assert!(
            type_hints.contains(&expected),
            "missing destructuring hint {expected:?}: {type_hints:?}"
        );
    }
    assert!(type_hints
        .iter()
        .any(|(line, _, label)| { *line == 5 && *label == "String" }));
    assert!(type_hints.iter().all(|(_, _, label)| *label != "Thing"));
    for quiet_line in [3, 4, 8, 9, 10, 11, 12, 13, 14] {
        assert!(
            type_hints.iter().all(|(line, _, _)| *line != quiet_line),
            "line {} should not have a type hint: {type_hints:?}",
            quiet_line + 1
        );
    }

    session.shutdown().await;
}

#[tokio::test]
async fn parameter_inlay_hints_require_parenthesized_fixed_user_dispatch() {
    let source = concat!(
        "fixed = (left, right) -> left\n",
        "fixed(1, 2)\n",
        "fixed (3, 4)\n",
        "unary = (item) -> item\n",
        "unary 1\n",
        "unary(1)\n",
        "variadic = values -> values\n",
        "variadic(1, 2)\n",
        "dispatch = method()\n",
        "dispatch(Thing) := (value) -> value\n",
        "dispatch(ZZ) := (integer) -> integer\n",
        "dispatch(1)\n",
        "dispatch(\"a\")\n",
        "builtin = ideal\n",
        "builtin(1, 2)\n",
    );
    let mut session = DocumentSession::open(source).await;
    session.set_expression_type_hints(false).await;
    let hints = inlay_hints(&mut session).await;
    let parameter_hints = hints
        .iter()
        .filter(|hint| hint["kind"] == 2)
        .map(|hint| {
            (
                hint["position"]["line"].as_u64().expect("hint line"),
                hint["label"].as_str().expect("hint label"),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        parameter_hints,
        vec![
            (1, "left:"),
            (1, "right:"),
            (2, "left:"),
            (2, "right:"),
            (5, "item:"),
            (11, "integer:"),
            (12, "value:"),
        ]
    );

    session.shutdown().await;
}

#[tokio::test]
async fn indexed_callable_aliases_preserve_identity_for_local_installations() {
    let source = "f = ideal\ng = f\nf ZZ := x -> x\ny = g 1\n";
    let mut session = DocumentSession::open(source).await;

    assert!(!session.diagnostic_codes().contains(&"E08"));
    assert_eq!(hover_type_at(&mut session, 3, 0).await, "Thing");

    let signature_help = session
        .request(
            "textDocument/signatureHelp",
            json!({
                "textDocument": {"uri": session.uri()},
                "position": {"line": 3, "character": 6}
            }),
        )
        .await;
    assert!(
        signature_help["signatures"]
            .as_array()
            .is_some_and(|signatures| signatures
                .iter()
                .any(|signature| { signature["label"] == "g(ZZ)" })),
        "indexed alias installation should drive signature help: {signature_help}"
    );

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
                .any(|(hint_line, label)| *hint_line == line && label == expected),
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
                .any(|(hint_line, label)| *hint_line == line && label == expected),
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
                .any(|(hint_line, label)| *hint_line == line && label == expected),
            "missing {expected} hint on line {line}: {hints:?}"
        );
    }

    session
        .replace("R = QQ[x]\nf = x^2\nJ = ideal(f)\nf\nJ\n")
        .await;
    let hints = inlay_labels_by_line(&mut session).await;
    assert!(hints.contains(&(3, "RingElement".to_string())));
    assert!(hints.contains(&(4, "Ideal".to_string())));

    session.shutdown().await;
}
