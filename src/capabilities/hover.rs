//! Hover rendering for local analysis results and indexed builtin records.

use tower_lsp::lsp_types::*;

use crate::analysis::{FunctionInfo, MethodInfo};
use crate::document::DocumentSnapshot;
use crate::meta::{BindingRole, Metadata};
use crate::node_metadata::{M2Node, NodeKindMetadata};
use crate::partitioned_index::ScopedIndex;
use crate::record_lsp::record_hover_with_package_and_usage;
use crate::typesystem::InstanceID;

/// The hover at `position`: a local symbol renders its binding info and local
/// method signatures; a builtin/package object renders its record from the
/// partition that owns it (with call-context signature specialization).
pub(crate) fn hover_response(
    document: &DocumentSnapshot,
    position: Position,
    scoped: &ScopedIndex,
) -> Option<Hover> {
    let text = document.text();
    let analysis = document.analysis();
    let node = document.node_at_position_minimal(position)?;

    if !hoverable_symbol_or_operator_node(node) {
        return None;
    }

    let node_text = node.text();

    if let Some(symbol) = analysis.get_symbol_at(node_text, position) {
        let local_installation_signature = analysis
            .local_method_installation_signature_at(node, text)
            .filter(|(method, _)| analysis.symbol_name(method.symbol) == node_text);
        let local_method = local_installation_signature
            .map(|(method, _)| method)
            .or_else(|| document.callable_at_position(position));
        let pinned_signature = local_installation_signature.map(|(_, signature)| signature);
        return Some(local_symbol_hover(
            node_text,
            &symbol,
            local_method,
            pinned_signature,
        ));
    }

    // Render from the partition that owns the record so an imported package
    // object shows its own documentation/signatures, not only a Core lookup.
    let (package, record, owning_data) =
        scoped.record_partition(&InstanceID(node_text.to_string()))?;
    let signature_usage = call_signature_usage_for_hover(node, node_text, text, analysis, scoped);
    Some(record_hover_with_package_and_usage(
        &record,
        Some(package),
        owning_data,
        signature_usage.as_ref(),
    ))
}

/// The hover for a symbol resolved by the local analysis: its inferred type,
/// its role label, and (for method functions) its installed signatures.
fn local_symbol_hover(
    name: &str,
    symbol: &(impl Metadata + ?Sized),
    method: Option<&FunctionInfo>,
    pinned_signature: Option<&MethodInfo>,
) -> Hover {
    let meta = symbol.meta();
    let title_type = meta
        .type_name
        .map(|type_name| format!("({type_name}) "))
        .unwrap_or_default();
    let title_signature = method
        .zip(pinned_signature)
        .map(|(method, signature)| {
            format!(" `{}`", local_method_signature_label(method, signature))
        })
        .unwrap_or_default();
    let label = match meta.symbol_kind {
        Some(SymbolKind::FUNCTION) if method.is_some() => "User-defined method function",
        Some(SymbolKind::FUNCTION) => "User-defined function",
        Some(SymbolKind::VARIABLE) if meta.binding_role == Some(BindingRole::Parameter) => {
            "Function parameter"
        }
        Some(SymbolKind::VARIABLE) => "User-defined variable",
        _ => "User-defined symbol",
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

/// The signature usage at a call site, for hover specialization: the argument
/// types at the enclosing application/operator, resolved against the loaded scope.
fn call_signature_usage_for_hover(
    node: M2Node,
    node_text: &str,
    text: &str,
    analysis: &crate::analysis::Analysis,
    scoped: &ScopedIndex,
) -> Option<crate::typesystem::SignatureUsage> {
    let parent = node.parent()?;
    // Static inference stays Core-scoped (its package path is a later phase);
    // signature-usage resolution consults the full loaded scope.
    let builtins = scoped.core();

    let argument_types = if parent.is_space_application() {
        let callable = parent.child_by_field_name("left")?;
        if callable.id() != node.id() {
            return None;
        }

        let argument = parent.child_by_field_name("right")?;
        analysis
            .infer_call_static_facts(argument, text, builtins)
            .dispatch_argument_types()
    } else if parent
        .child_by_field_name("operator")
        .is_some_and(|operator| operator.id() == node.id())
    {
        let left = parent.child_by_field_name("left")?;
        let right = parent.child_by_field_name("right")?;
        vec![
            analysis.infer_expression_static_type_name(left, text, builtins),
            analysis.infer_expression_static_type_name(right, text, builtins),
        ]
    } else {
        return None;
    };

    scoped.resolve_call_signature_usage(node_text, &argument_types)
}

/// Whether a hover over this node is meaningful: a symbol-like leaf or an
/// operator token of an expression.
fn hoverable_symbol_or_operator_node(node: M2Node) -> bool {
    if node.kind.is_symbol_like() {
        return true;
    }

    node.is_operator()
}

fn local_method_signatures_markdown(
    method: &FunctionInfo,
    pinned_signature: Option<&MethodInfo>,
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
            .methods
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

    if method.methods.is_empty() {
        return method
            .typical_value
            .as_ref()
            .map(|codomain| format!("\n\nCodomain: `{codomain}`"))
            .unwrap_or_default();
    }

    let mut lines = vec!["\n\n**Local Method Signatures:**".to_string()];
    for signature in &method.methods {
        lines.push(format!(
            "- `{}`",
            local_method_signature_label(method, signature)
        ));
    }
    lines.join("\n")
}

fn local_method_signature_label(method: &FunctionInfo, signature: &MethodInfo) -> String {
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
    use crate::analysis::Analysis;
    use crate::meta::{BindingRole, Meta};
    use crate::partitioned_index::{LoadedPackages, PackagePartitionedIndex};
    use crate::typesystem::BuiltinData;
    use tower_lsp::lsp_types::{HoverContents, Position, SymbolKind};
    use tree_sitter::Parser;

    #[test]
    fn local_hover_includes_known_static_type() {
        let symbol = Meta {
            symbol_kind: Some(SymbolKind::VARIABLE),
            binding_role: Some(BindingRole::Ordinary),
            type_name: Some("Package"),
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
        let text = "p = method(TypicalValue => List)\np(ZZ, ZZ) := (i, j) -> {i, j}\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new(&tree, text);
        let symbol = analysis
            .get_symbol_at("p", Position::new(1, 0))
            .expect("method symbol should be visible");
        let method = analysis.function("p").expect("method should be registered");

        let hover = local_symbol_hover("p", &symbol, Some(method), None);
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
            .local_method_installation_signature_at(M2Node::new(node, text), text)
            .expect("method installation should pin the installed signature");

        let hover = local_symbol_hover("p", &symbol, Some(method), Some(pinned_signature));
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
    fn hover_shows_imported_package_documentation() {
        // Hovering an object from an imported package renders that package's own
        // documentation (resolved from the owning partition, not just Core).
        let text = "needsPackage \"JSON\"\ntoJSON\n";
        let document = DocumentSnapshot::from_text(text.to_string(), &BuiltinData::empty())
            .expect("fixture should parse");
        let index = PackagePartitionedIndex::from_corpus(include_str!("../data/m2-index.jsonl"));
        let loaded = LoadedPackages::resolve(index.default_loaded(), text);
        let scoped = index.scoped(&loaded);
        let hover = hover_response(&document, Position::new(1, 0), &scoped)
            .expect("hover over an imported package object");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("imported-package hover should use markdown");
        };
        assert!(
            markup.value.contains("Package: `JSON`"),
            "got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("Encode Macaulay2 things as JSON"),
            "expected the JSON package doc body, got: {}",
            markup.value
        );
    }

    #[test]
    fn hover_call_context_specializes_builtin_method_signatures() {
        let text = "F := openOut \"test.oldvalues\"\n";
        let index = PackagePartitionedIndex::from_corpus(include_str!("../data/m2-index.jsonl"));
        let loaded = LoadedPackages::resolve(index.default_loaded(), "");
        let scoped = index.scoped(&loaded);
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_knowledge(&tree, text, scoped.core());
        let node = tree
            .root_node()
            .descendant_for_point_range(
                tree_sitter::Point::new(0, 5),
                tree_sitter::Point::new(0, 5),
            )
            .expect("openOut node should be found");

        let usage = call_signature_usage_for_hover(
            M2Node::new(node, text),
            "openOut",
            text,
            &analysis,
            &scoped,
        )
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
        let index = PackagePartitionedIndex::from_corpus(include_str!("../data/m2-index.jsonl"));
        let loaded = LoadedPackages::resolve(index.default_loaded(), "");
        let scoped = index.scoped(&loaded);
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_macaulay2::language())
            .expect("macaulay2 parser should load");
        let tree = parser.parse(text, None).expect("fixture should parse");
        let analysis = Analysis::new_with_knowledge(&tree, text, scoped.core());
        let node = tree
            .root_node()
            .descendant_for_point_range(
                tree_sitter::Point::new(2, 7),
                tree_sitter::Point::new(2, 7),
            )
            .expect("+ node should be found");
        let node = M2Node::new(node, text);
        assert!(
            hoverable_symbol_or_operator_node(node),
            "operator tokens should be hoverable"
        );

        let usage = call_signature_usage_for_hover(node, "+", text, &analysis, &scoped)
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
}
