use tower_lsp::lsp_types::*;

use crate::analysis::{LocalMethodInfo, LocalMethodSignature, SymbolInfo, SymbolKind};
use crate::document::DocumentSnapshot;
use crate::typesystem::{BuiltinData, InstanceID};
use crate::util::*;
use crate::{record_lsp::record_hover_with_package_and_usage, typesystem};

pub(crate) fn hover_response(
    document: &DocumentSnapshot,
    position: Position,
    builtins: &BuiltinData,
    active_package_indexes: &[(String, BuiltinData)],
) -> Option<Hover> {
    let text = document.text();
    let analysis = document.analysis();
    let node = document.node_at_position_minimal(position)?;

    if !hoverable_symbol_or_operator_node(node) {
        return None;
    }

    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
    let node_text = &text[start_byte..end_byte];

    if let Some(symbol) = analysis.get_symbol_at(node_text, position) {
        let local_installation_signature = analysis
            .local_method_installation_signature_at(node, text)
            .filter(|(method, _)| method.name == node_text);
        let local_method = local_installation_signature
            .map(|(method, _)| method)
            .or_else(|| analysis.local_method(node_text));
        let pinned_signature = local_installation_signature.map(|(_, signature)| signature);
        return Some(local_symbol_hover(
            node_text,
            symbol,
            local_method,
            pinned_signature,
        ));
    }

    for (package, package_index) in active_package_indexes {
        if let Some(record) = package_index.get_record(&InstanceID(node_text.to_string())) {
            return Some(crate::record_lsp::record_hover_with_package(
                &record,
                Some(package),
                builtins,
            ));
        }
    }

    if !builtins.contains_name(node_text) {
        return None;
    }

    let record = builtins.get_record(&typesystem::InstanceID(node_text.to_string()))?;
    let signature_usage = call_signature_usage_for_hover(node, node_text, text, analysis, builtins);
    Some(record_hover_with_package_and_usage(
        &record,
        Some("Core"),
        builtins,
        signature_usage.as_ref(),
    ))
}

pub(crate) fn local_symbol_hover(
    name: &str,
    symbol: &SymbolInfo,
    method: Option<&LocalMethodInfo>,
    pinned_signature: Option<&LocalMethodSignature>,
) -> Hover {
    let title_type = symbol
        .type_name
        .as_ref()
        .map(|type_name| format!("({type_name}) "))
        .unwrap_or_default();
    let title_signature = method
        .zip(pinned_signature)
        .map(|(method, signature)| {
            format!(" `{}`", local_method_signature_label(method, signature))
        })
        .unwrap_or_default();
    let label = match symbol.kind {
        SymbolKind::Function if method.is_some() => "User-defined method function",
        SymbolKind::Function => "User-defined function",
        SymbolKind::Variable => "User-defined variable",
        SymbolKind::Parameter => "Function parameter",
    };
    let signatures = method
        .map(|method| local_method_signatures_markdown(method, pinned_signature))
        .unwrap_or_default();
    let markdown = format!(
        "**{}{}**{}\n\n{}{}",
        title_type, name, title_signature, label, signatures
    );

    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: None,
    }
}

pub(crate) fn call_signature_usage_for_hover(
    node: tree_sitter::Node,
    node_text: &str,
    text: &str,
    analysis: &crate::analysis::Analysis,
    builtins: &BuiltinData,
) -> Option<crate::typesystem::SignatureUsage> {
    let parent = node.parent()?;

    let argument_types = if is_space_operator_expression(parent) {
        let callable = parent.child_by_field_name("left")?;
        if callable.id() != node.id() {
            return None;
        }

        let argument = parent.child_by_field_name("right")?;
        analysis
            .infer_call_static_facts(argument, text, Some(builtins))
            .argument_types
    } else if parent
        .child_by_field_name("operator")
        .is_some_and(|operator| operator.id() == node.id())
    {
        let operator = parent.child_by_field_name("operator")?;
        if operator.id() != node.id() {
            return None;
        }

        let left = parent.child_by_field_name("left")?;
        let right = parent.child_by_field_name("right")?;
        vec![
            analysis.infer_expression_static_type_name(left, text, Some(builtins)),
            analysis.infer_expression_static_type_name(right, text, Some(builtins)),
        ]
    } else {
        return None;
    };

    builtins.resolve_call_signature_usage(node_text, &argument_types)
}

pub(crate) fn hoverable_symbol_or_operator_node(node: tree_sitter::Node) -> bool {
    if matches!(
        node.kind(),
        "symbol" | "identifier" | "resolved_symbol" | "operator"
    ) {
        return true;
    }

    is_operator_node(node)
}

fn local_method_signatures_markdown(
    method: &LocalMethodInfo,
    pinned_signature: Option<&LocalMethodSignature>,
) -> String {
    if let Some(pinned_signature) = pinned_signature {
        let mut lines = vec![
            "\n\n**Signature:**".to_string(),
            format!(
                "- `{}`",
                local_method_signature_label(method, pinned_signature)
            ),
        ];
        let excluded = method
            .signatures
            .iter()
            .filter(|signature| {
                signature.domain != pinned_signature.domain
                    || signature.codomain != pinned_signature.codomain
            })
            .collect::<Vec<_>>();
        if !excluded.is_empty() {
            lines.push("\n**Excluded Signatures For This Usage:**".to_string());
            for signature in excluded.iter().take(15) {
                lines.push(format!(
                    "- `{}`",
                    local_method_signature_label(method, signature)
                ));
            }
            if excluded.len() > 15 {
                lines.push("- ...".to_string());
            }
        }
        return lines.join("\n");
    }

    if method.signatures.is_empty() {
        return method
            .typical_value
            .as_ref()
            .map(|codomain| format!("\n\nCodomain: `{codomain}`"))
            .unwrap_or_default();
    }

    let mut lines = vec!["\n\n**Local Method Signatures:**".to_string()];
    for signature in &method.signatures {
        let domain = signature.domain.join(", ");
        let codomain = signature
            .codomain
            .as_ref()
            .or(method.typical_value.as_ref())
            .map(|codomain| format!(" -> {codomain}"))
            .unwrap_or_default();
        lines.push(format!("- `{domain}{codomain}`"));
    }
    lines.join("\n")
}

fn local_method_signature_label(
    method: &LocalMethodInfo,
    signature: &LocalMethodSignature,
) -> String {
    let domain = signature.domain.join(", ");
    let codomain = signature
        .codomain
        .as_ref()
        .or(method.typical_value.as_ref())
        .map(|codomain| format!(" -> {codomain}"))
        .unwrap_or_default();
    format!("{domain}{codomain}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{
        Analysis, LocalMethodInfo, LocalMethodSignature, SymbolInfo, SymbolKind,
    };
    use crate::typesystem::BuiltinData;
    use tower_lsp::lsp_types::{HoverContents, Position, Range};
    use tree_sitter::Parser;

    #[test]
    fn local_hover_includes_known_static_type() {
        let symbol = SymbolInfo {
            kind: SymbolKind::Variable,
            range: Range::new(Position::new(2, 4), Position::new(2, 7)),
            type_name: Some("Package".to_string()),
        };

        let hover = local_symbol_hover("Doc", &symbol, None, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("local hover should use markdown");
        };

        assert!(
            markup.value.starts_with("**(Package) Doc**"),
            "local hover should display known static type facts before the title name"
        );
        assert!(!markup.value.contains("\n\nType: `Package`"));
        assert!(!markup.value.contains("Defined at"));
    }

    #[test]
    fn local_hover_includes_method_signatures() {
        let symbol = SymbolInfo {
            kind: SymbolKind::Function,
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            type_name: Some("MethodFunction".to_string()),
        };
        let method = LocalMethodInfo {
            name: "p".to_string(),
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            typical_value: Some("List".to_string()),
            signatures: vec![LocalMethodSignature {
                domain: vec!["ZZ".to_string(), "ZZ".to_string()],
                codomain: Some("List".to_string()),
                range: Range::new(Position::new(1, 0), Position::new(1, 8)),
            }],
        };

        let hover = local_symbol_hover("p", &symbol, Some(&method), None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("local hover should use markdown");
        };

        assert!(markup.value.contains("User-defined method function"));
        assert!(markup.value.contains("`ZZ, ZZ -> List`"));
    }

    #[test]
    fn local_method_installation_hover_pins_installed_signature() {
        let text = "p = method(TypicalValue => List)\np(ZZ, ZZ) := (i, j) -> {i, j}\np(CC, CC) := Array => (i, j) -> [i, j]\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new(&tree, text);
        let position = Position::new(1, 0);
        let node = tree
            .root_node()
            .descendant_for_point_range(
                tree_sitter::Point::new(1, 0),
                tree_sitter::Point::new(1, 0),
            )
            .expect("method name node should be found");
        let symbol = analysis
            .get_symbol_at("p", position)
            .expect("method symbol should be visible");
        let (method, pinned_signature) = analysis
            .local_method_installation_signature_at(node, text)
            .expect("method installation should pin the installed signature");

        let hover = local_symbol_hover("p", symbol, Some(method), Some(pinned_signature));
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("local hover should use markdown");
        };

        assert!(markup
            .value
            .contains("**(MethodFunction) p** `ZZ, ZZ -> List`"));
        assert!(markup.value.contains("**Signature:**"));
        assert!(markup
            .value
            .contains("**Excluded Signatures For This Usage:**"));
        assert!(markup.value.contains("`CC, CC -> Array`"));
        assert!(!markup.value.contains("**Local Method Signatures:**"));
    }

    #[test]
    fn hover_call_context_specializes_builtin_method_signatures() {
        let text = "F := openOut \"test.oldvalues\"\n";
        let builtins = BuiltinData::load_from_split(
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
        );
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(&builtins));
        let node = tree
            .root_node()
            .descendant_for_point_range(
                tree_sitter::Point::new(0, 5),
                tree_sitter::Point::new(0, 5),
            )
            .expect("openOut node should be found");

        let usage = call_signature_usage_for_hover(node, "openOut", text, &analysis, &builtins)
            .expect("openOut hover should resolve usage signatures");
        let signature = usage
            .pinned
            .expect("openOut hover should pin the String installation");

        assert_eq!(
            signature
                .signature
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>(),
            vec!["openOut", "String"]
        );
        assert_eq!(
            signature
                .output_types
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>(),
            vec!["File"]
        );
    }

    #[test]
    fn hover_call_context_specializes_operator_method_signatures() {
        let text = "x := 1\ny := 2\nz := x + y\n";
        let builtins = BuiltinData::load_from_split(
            "+\n",
            "{\"name\":\"+\",\"data_type\":\"Keyword\",\"description_short\":null,\"description_long\":null,\"examples\":[],\"extra\":{},\"function_info\":{\"methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"]}],\"documented_methods\":[{\"signature\":[\"+\",\"ZZ\",\"ZZ\"],\"output_types\":[\"ZZ\"],\"doc_key\":\"+(ZZ,ZZ)\"}]}}\n",
        );
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(&builtins));
        let node = tree
            .root_node()
            .descendant_for_point_range(
                tree_sitter::Point::new(2, 7),
                tree_sitter::Point::new(2, 7),
            )
            .expect("+ node should be found");
        assert!(
            hoverable_symbol_or_operator_node(node),
            "operator tokens should be hoverable"
        );

        let usage = call_signature_usage_for_hover(node, "+", text, &analysis, &builtins)
            .expect("+ hover should resolve usage signatures");
        let signature = usage
            .pinned
            .expect("+ hover should pin the ZZ, ZZ installation");

        assert_eq!(
            signature
                .signature
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>(),
            vec!["+", "ZZ", "ZZ"]
        );
        assert_eq!(
            signature
                .output_types
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>(),
            vec!["ZZ"]
        );
    }

    #[test]
    fn hover_operator_usage_partitions_possible_and_excluded_signatures() {
        let text = "opts = {Slope => 1, Intercept => 1}\ng = opts >> o -> x -> x\n";
        let builtins = BuiltinData::load_from_split(
            include_str!("../data/builtins.names"),
            include_str!("../data/builtins.details.jsonl"),
        );
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_builtins(&tree, text, Some(&builtins));
        let node = tree
            .root_node()
            .descendant_for_point_range(
                tree_sitter::Point::new(1, 10),
                tree_sitter::Point::new(1, 10),
            )
            .expect(">> node should be found");

        let usage = call_signature_usage_for_hover(node, ">>", text, &analysis, &builtins)
            .expect(">> hover should resolve usage signatures");

        let pinned = usage
            .pinned
            .as_ref()
            .expect("List and Function should pin the smaller >> installation");
        assert_eq!(signature_label_for_test(pinned), ">>, List, Function");
        assert_eq!(
            usage
                .possible
                .iter()
                .map(signature_label_for_test)
                .collect::<Vec<_>>(),
            Vec::<String>::new()
        );
        assert_eq!(
            usage
                .excluded
                .iter()
                .map(signature_label_for_test)
                .collect::<Vec<_>>(),
            vec![
                ">>, OptionTable, Function",
                ">>, Boolean, Function",
                ">>, CC, ZZ",
                ">>, RR, ZZ",
                ">>, ZZ, ZZ",
                ">>, RRi, ZZ",
                ">>, Thing, Thing"
            ]
        );
    }

    fn signature_label_for_test(signature: &crate::typesystem::ResolvedSignature) -> String {
        signature
            .signature
            .iter()
            .map(|id| id.0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
