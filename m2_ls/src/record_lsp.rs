use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, SymbolKind};

use crate::typesystem::{BuiltinData, Record};

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

    match record.data_type.0.as_str() {
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
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: record_hover_markdown(record, package, builtins),
        }),
        range: None,
    }
}

fn record_hover_markdown(record: &Record, package: Option<&str>, builtins: &BuiltinData) -> String {
    let mut markdown = format!("**{}**\n\n", record.name);
    markdown.push_str(&format!("Type: `{}`\n\n", record.data_type.0));

    if let Some(package) = package.or_else(|| record_package(record)) {
        markdown.push_str(&format!("Package: `{package}`\n\n"));
    }

    if let Some(option_role) = record.option_role() {
        markdown.push_str(&format!("Option Role: `{option_role}`\n\n"));

        let usages = builtins.option_usage_names(&record.name.0, 8);
        if !usages.is_empty() {
            markdown.push_str("**Used By Methods:**\n");
            for usage in usages {
                markdown.push_str(&format!("- `{usage}`\n"));
            }
            markdown.push('\n');
        }
    }

    if let Some(desc) = &record.description_short {
        markdown.push_str(&format!("{}\n\n", desc));
    }

    if let Some(val) = record.extra.get("typical_value") {
        markdown.push_str(&format!("Typical Value: `{}`\n\n", val));
    }

    if let Some(func_info) = &record.function_info {
        if !func_info.documented_methods.is_empty() {
            markdown.push_str("**Documented Signatures:**\n");
            for method in func_info.documented_methods.iter().take(15) {
                let signature = method
                    .signature
                    .iter()
                    .map(|s| s.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let outputs = method
                    .output_types
                    .iter()
                    .map(|s| s.0.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ");
                if outputs.is_empty() {
                    markdown.push_str(&format!("- `({signature})`\n"));
                } else {
                    markdown.push_str(&format!("- `({signature}) -> {outputs}`\n"));
                }
            }
            if func_info.documented_methods.len() > 15 {
                markdown.push_str("- ...\n");
            }
            markdown.push('\n');
        }

        markdown.push_str("**Installed Methods:**\n");
        for method in func_info.methods.iter().take(15) {
            markdown.push_str(&format!(
                "- `({})` \n",
                method
                    .signature
                    .iter()
                    .map(|s| s.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if func_info.methods.len() > 15 {
            markdown.push_str("- ...\n");
        }
    }

    if !record.examples.is_empty() {
        markdown.push_str("\n**Examples:**\n\n```macaulay2\n");
        for example in record.examples.iter().take(6) {
            markdown.push_str(&example.0);
            markdown.push('\n');
        }
        markdown.push_str("```\n");
    }

    markdown
}
