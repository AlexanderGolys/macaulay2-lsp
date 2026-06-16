use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, SymbolKind};

use crate::typesystem::{BuiltinData, OperatorInfo, Record, ResolvedSignature, SignatureUsage};

pub(crate) fn record_package(record: &Record) -> Option<&str> {
    record.extra.get("package")?.as_str()
}

pub(crate) fn record_source_file(record: &Record) -> Option<&str> {
    record
        .documentation
        .as_ref()
        .and_then(|documentation| documentation.source_file.as_deref())
        .or_else(|| {
            record
                .extra
                .get("package_source_file")
                .and_then(|value| value.as_str())
        })
}

pub(crate) fn record_source_line(record: &Record) -> u32 {
    record
        .documentation
        .as_ref()
        .and_then(|documentation| documentation.source_line)
        .and_then(|line| u32::try_from(line.saturating_sub(1)).ok())
        .unwrap_or(0)
}

pub(crate) fn record_symbol_kind(record: &Record) -> SymbolKind {
    if record.type_info.is_some() {
        return SymbolKind::CLASS;
    }

    if record.function_info.is_some() {
        return SymbolKind::FUNCTION;
    }

    match record.class.0.as_str() {
        "Package" => SymbolKind::NAMESPACE,
        "Type" => SymbolKind::CLASS,
        "Option" => SymbolKind::PROPERTY,
        _ => SymbolKind::VARIABLE,
    }
}

pub(crate) fn record_hover_with_package(
    record: &Record,
    package: Option<&str>,
    builtins: &BuiltinData,
) -> Hover {
    record_hover_with_package_and_usage(record, package, builtins, None)
}

pub(crate) fn record_hover_with_package_and_usage(
    record: &Record,
    package: Option<&str>,
    builtins: &BuiltinData,
    usage: Option<&SignatureUsage>,
) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: record_hover_markdown(record, package, builtins, usage),
        }),
        range: None,
    }
}

fn record_hover_markdown(
    record: &Record,
    package: Option<&str>,
    builtins: &BuiltinData,
    usage: Option<&SignatureUsage>,
) -> String {
    let title_signature = usage
        .and_then(|usage| usage.pinned.as_ref())
        .map(|signature| {
            format!(
                " `{}`",
                signature_label(signature, record.operator_info.as_ref())
            )
        })
        .unwrap_or_default();
    let mut markdown = format!(
        "(`{}`) **{}**\t{}\n\n",
        record.class.0, record.name, title_signature
    );

    if let Some(package) = package.or_else(|| record_package(record)) {
        markdown.push_str(&format!("Package: `{package}`\n"));
    }

    if let Some(option_role) = record.option_role() {
        markdown.push_str(&format!("Option Role: `{option_role}`\n"));

        match option_role {
            "key" => {
                let usages = builtins.option_usage_names(&record.name.0, 8);
                if !usages.is_empty() {
                    markdown.push_str("**Accepted By Methods:**\n");
                    for usage in usages {
                        markdown.push_str(&format!("- `{usage}`\n"));
                    }
                    markdown.push('\n');
                }
            }
            "value" => {
                let usages = builtins.option_value_usage_names(&record.name.0, 8);
                if !usages.is_empty() {
                    markdown.push_str("**Valid As Option Value:**\n");
                    for usage in usages {
                        markdown.push_str(&format!("- `{usage}`\n"));
                    }
                    markdown.push('\n');
                }
            }
            _ => {}
        }
    }

    if let Some(desc) = &record.description_short {
        markdown.push_str(&format!("{}\n\n", desc));
    }

    if let Some(typical_value) = record_typical_value(record) {
        markdown.push_str(&format!("Typical Value: `{typical_value}`\n\n"));
    }

    if let Some(signature) = usage.and_then(|usage| usage.pinned.as_ref()) {
        markdown.push_str("**Signature:**\n");
        markdown.push_str(&format!(
            "- `{}`\n",
            signature_label(signature, record.operator_info.as_ref())
        ));
        if let Some(doc_key) = &signature.doc_key {
            markdown.push_str(&format!("Documentation: `{doc_key}`\n\n"));
        }

        if let Some(usage) = usage {
            append_usage_signature_section(
                &mut markdown,
                "Excluded Signatures For This Usage",
                &usage.excluded,
                record.operator_info.as_ref(),
            );
        }
    } else if let Some(usage) = usage {
        append_usage_signature_section(
            &mut markdown,
            "Possible Signatures For This Usage",
            &usage.possible,
            record.operator_info.as_ref(),
        );
        append_usage_signature_section(
            &mut markdown,
            "Excluded Signatures For This Usage",
            &usage.excluded,
            record.operator_info.as_ref(),
        );
    }

    if record.function_info.is_some() {
        let documented_signatures = if usage.is_some() {
            Vec::new()
        } else {
            builtins.documented_signatures(record)
        };
        if !documented_signatures.is_empty() {
            markdown.push_str("**Documented Signatures:**\n");
            for method in documented_signatures.iter().take(15) {
                markdown.push_str(&format!(
                    "- `{}`\n",
                    signature_label(method, record.operator_info.as_ref())
                ));
            }
            if documented_signatures.len() > 15 {
                markdown.push_str("- ...\n");
            }
            markdown.push('\n');
        }

        let installed_methods = if usage.is_some() {
            Vec::new()
        } else {
            builtins.undocumented_installed_methods(record)
        };
        if !installed_methods.is_empty() {
            markdown.push_str("**Installed Methods:**\n");
            for method in installed_methods.iter().take(15) {
                let signature = ResolvedSignature {
                    signature: method.signature.clone(),
                    output_types: Vec::new(),
                    is_specialized: false,
                    examples: Vec::new(),
                    doc_key: None,
                };
                markdown.push_str(&format!(
                    "- `{}`\n",
                    signature_label(&signature, record.operator_info.as_ref())
                ));
            }
            if installed_methods.len() > 15 {
                markdown.push_str("- ...\n");
            }
        }
    }

    let examples = usage
        .and_then(|usage| usage.pinned.as_ref())
        .filter(|signature| !signature.examples.is_empty())
        .map(|signature| signature.examples.as_slice())
        .unwrap_or(record.examples.as_slice());
    if !examples.is_empty() {
        markdown.push_str("\n**Examples:**\n```macaulay2\n");
        for example in examples.iter().take(6) {
            markdown.push_str(&example.0);
            markdown.push('\n');
        }
        markdown.push_str("```\n");
    }

    // Builtin records carry no doc text (the typecheck index is compressed); the
    // rendered docs live in a separate map read once at startup. Append them
    // below the structured header. Package records keep their own docs and have
    // no entry here, so this is a no-op for them.
    if let Some(doc) = builtins.doc_markdown(&record.name) {
        markdown.push_str("\n---\n\n");
        markdown.push_str(doc);
        markdown.push('\n');
    }

    markdown
}

fn append_usage_signature_section(
    markdown: &mut String,
    title: &str,
    signatures: &[ResolvedSignature],
    operator_info: Option<&OperatorInfo>,
) {
    if signatures.is_empty() {
        return;
    }

    markdown.push_str(&format!("**{title}:**\n"));
    for signature in signatures.iter().take(15) {
        markdown.push_str(&format!(
            "- `{}`\n",
            signature_label(signature, operator_info)
        ));
    }
    if signatures.len() > 15 {
        markdown.push_str("- ...\n");
    }
    markdown.push('\n');
}

fn record_typical_value(record: &Record) -> Option<String> {
    if let Some(value) = record.extra.get("typical_value") {
        return Some(value.to_string());
    }

    record
        .function_info
        .as_ref()
        .and_then(|info| info.general_signature.as_ref())
        .and_then(|signature| {
            (!signature.output_types.is_empty()).then(|| {
                signature
                    .output_types
                    .iter()
                    .map(|output| output.0.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
        })
}

fn signature_label(signature: &ResolvedSignature, operator_info: Option<&OperatorInfo>) -> String {
    let domain_parts = signature
        .signature
        .iter()
        .skip(1)
        .map(|s| s.0.as_str())
        .collect::<Vec<_>>();
    let domain = operator_signature_domain_label(signature, operator_info, &domain_parts)
        .unwrap_or_else(|| {
            let domain = domain_parts.join(", ");
            if domain.is_empty() {
                "?".to_string()
            } else {
                domain
            }
        });
    let outputs = signature
        .output_types
        .iter()
        .map(|s| s.0.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    if outputs.is_empty() {
        domain
    } else {
        format!("{domain} -> {outputs}")
    }
}

fn operator_signature_domain_label(
    signature: &ResolvedSignature,
    operator_info: Option<&OperatorInfo>,
    domain_parts: &[&str],
) -> Option<String> {
    let operator_info = operator_info?;
    let method_key = signature.signature.first()?.0.as_str();
    let (operator, is_assignment) = operator_method_key(method_key)?;
    if operator != operator_info.method_symbol.0 {
        return None;
    }

    match domain_parts {
        [left, right]
            if is_assignment && operator_info.forms.iter().any(|form| form == "Binary") =>
        {
            Some(format!("{left} {operator} {right} = ..."))
        }
        [operand] if is_assignment && operator_info.forms.iter().any(|form| form == "Prefix") => {
            Some(format!("{operator} {operand} = ..."))
        }
        [operand] if is_assignment && operator_info.forms.iter().any(|form| form == "Postfix") => {
            Some(format!("{operand}{operator} = ..."))
        }
        [left, right]
            if !is_assignment && operator_info.forms.iter().any(|form| form == "Binary") =>
        {
            Some(format!("{left} {operator} {right}"))
        }
        [operand] if !is_assignment && operator_info.forms.iter().any(|form| form == "Prefix") => {
            Some(format!("{operator} {operand}"))
        }
        [operand] if !is_assignment && operator_info.forms.iter().any(|form| form == "Postfix") => {
            Some(format!("{operand}{operator}"))
        }
        _ => None,
    }
}

fn operator_method_key(method_key: &str) -> Option<(&str, bool)> {
    if let Some(inner) = method_key
        .strip_prefix('(')
        .and_then(|key| key.strip_suffix(')'))
    {
        let (operator, suffix) = inner.split_once(',')?;
        return (suffix == "=").then_some((operator, true));
    }

    Some((method_key, false))
}
