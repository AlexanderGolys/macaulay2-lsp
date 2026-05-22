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

pub(crate) fn record_symbol_kind(record: &Record, builtins: &BuiltinData) -> SymbolKind {
    if builtins.is_constructor_name(&record.name.0) {
        return SymbolKind::CONSTRUCTOR;
    }

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

pub(crate) fn record_hover(record: &Record) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: record_hover_markdown(record),
        }),
        range: None,
    }
}

fn record_hover_markdown(record: &Record) -> String {
    let mut markdown = format!("**{}**\n\n", record.name);
    markdown.push_str(&format!("Type: `{}`\n\n", record.data_type.0));

    if let Some(package) = record_package(record) {
        markdown.push_str(&format!("Package: `{package}`\n\n"));
    }

    if let Some(desc) = &record.description_short {
        markdown.push_str(&format!("{}\n\n", desc));
    }

    if let Some(val) = record.extra.get("typical_value") {
        markdown.push_str(&format!("Typical Value: `{}`\n\n", val));
    }

    if let Some(func_info) = &record.function_info {
        markdown.push_str("**Installed Methods:**\n");
        for method in func_info.methods.iter().take(5) {
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
        if func_info.methods.len() > 5 {
            markdown.push_str("- ...\n");
        }
    }

    markdown
}
