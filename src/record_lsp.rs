//! Conversion of indexed builtin records into LSP-facing hover and symbol data.

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, SymbolKind};

use crate::builtin_index::{OperatorInfo, Record};
use crate::typesystem::{LspKnowledge, ResolvedSignature, SignatureUsage};

pub(crate) fn record_package(record: &Record) -> Option<&str> {
    record.package.as_deref()
}

pub(crate) fn record_source_file(record: &Record) -> Option<&str> {
    record.source_file.as_deref()
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

pub(crate) fn record_hover_with_package_and_usage(
    record: &Record,
    package: Option<&str>,
    knowledge: &(impl LspKnowledge + ?Sized),
    usage: Option<&SignatureUsage>,
) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: record_hover_markdown(record, package, knowledge, usage),
        }),
        range: None,
    }
}

fn record_hover_markdown(
    record: &Record,
    package: Option<&str>,
    knowledge: &(impl LspKnowledge + ?Sized),
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
    let mut markdown = format!("**{}**{}\n", record.name, title_signature);

    let mut metadata = vec![format!("Type: `{}`", record.class.0)];
    if let Some(package) = package.or_else(|| record_package(record)) {
        metadata.push(format!("Package: `{package}`"));
    }
    markdown.push_str(&metadata.join(" · "));
    markdown.push_str("\n\n");

    let option_key_usages = knowledge.option_usage_names(&record.name.0, 8);
    let option_value_usages = knowledge.option_value_usage_names(&record.name.0, 8);
    if !option_key_usages.is_empty() || !option_value_usages.is_empty() {
        let role = match (option_key_usages.is_empty(), option_value_usages.is_empty()) {
            (false, false) => "key, value",
            (false, true) => "key",
            (true, false) => "value",
            (true, true) => unreachable!(),
        };
        markdown.push_str(&format!("Option Role: `{role}`\n"));
        if !option_key_usages.is_empty() {
            markdown.push_str("**Accepted By Methods:**\n");
            for usage in option_key_usages {
                markdown.push_str(&format!("- `{usage}`\n"));
            }
            markdown.push('\n');
        }
        if !option_value_usages.is_empty() {
            markdown.push_str("**Valid As Option Value:**\n");
            for usage in option_value_usages {
                markdown.push_str(&format!("- `{usage}`\n"));
            }
            markdown.push('\n');
        }
    }

    if let Some(typical_value) = record_typical_value(record) {
        markdown.push_str(&format!("Typical Value: `{typical_value}`\n\n"));
    }

    if let Some(usage) = usage.filter(|usage| usage.pinned.is_some()) {
        append_usage_signature_section(
            &mut markdown,
            "Other signatures for this call",
            &usage.excluded,
            record.operator_info.as_ref(),
        );
    } else if let Some(usage) = usage {
        append_usage_signature_section(
            &mut markdown,
            "Possible signatures for this call",
            &usage.possible,
            record.operator_info.as_ref(),
        );
        append_usage_signature_section(
            &mut markdown,
            "Other signatures for this call",
            &usage.excluded,
            record.operator_info.as_ref(),
        );
    } else {
        append_record_signatures(&mut markdown, record, knowledge);
    }

    let examples = usage
        .and_then(|usage| usage.pinned.as_ref())
        .filter(|signature| !signature.examples.is_empty())
        .map(|signature| signature.examples.as_slice())
        .unwrap_or(record.examples.as_slice());

    let mut documentation = HoverDocumentation::default();
    if let Some(doc) = knowledge.doc_markdown(&record.name) {
        documentation = compact_documentation_markdown(&doc);
        if !documentation.markdown.is_empty() {
            markdown.push_str("---\n\n");
            markdown.push_str(&documentation.markdown);
            markdown.push('\n');
        }
    }

    if !documentation.has_examples && !examples.is_empty() {
        markdown.push_str("\n## Examples\n\n");
        for (index, example) in examples.iter().take(6).enumerate() {
            markdown.push_str(&render_example_card(index + 1, &example.0, None));
            markdown.push('\n');
        }
    }

    markdown.trim_end().to_string()
}

fn append_record_signatures(
    markdown: &mut String,
    record: &Record,
    knowledge: &(impl LspKnowledge + ?Sized),
) {
    if record.function_info.is_none() {
        return;
    }

    let mut labels = knowledge
        .documented_signatures(record)
        .iter()
        .map(|signature| signature_label(signature, record.operator_info.as_ref()))
        .collect::<Vec<_>>();
    for method in knowledge.undocumented_installed_methods(record) {
        let signature = ResolvedSignature {
            signature: method.signature,
            output_types: Vec::new(),
            is_specialized: false,
            examples: Vec::new(),
            doc_key: None,
        };
        let label = signature_label(&signature, record.operator_info.as_ref());
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        return;
    }

    let class = record
        .class
        .0
        .rsplit_once('$')
        .map_or(record.class.0.as_str(), |(_, class)| class);
    let title = if class.starts_with("MethodFunction") {
        "Methods"
    } else {
        "Signatures"
    };
    markdown.push_str(&format!("**{title}:**\n"));
    for label in labels.iter().take(15) {
        markdown.push_str(&format!("- `{label}`\n"));
    }
    if labels.len() > 15 {
        markdown.push_str("- ...\n");
    }
    markdown.push('\n');
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

#[derive(Default)]
struct HoverDocumentation {
    markdown: String,
    has_examples: bool,
}

struct MarkdownFence {
    language: String,
    content: String,
}

fn compact_documentation_markdown(documentation: &str) -> HoverDocumentation {
    let mut compact = HoverDocumentation::default();
    let mut section_name = String::new();
    let mut section_lines = Vec::new();

    for line in documentation.lines() {
        if let Some(heading) = documentation_section_heading(line) {
            append_documentation_section(&mut compact, &section_name, &section_lines);
            section_name = heading.to_string();
            section_lines.clear();
        } else {
            section_lines.push(line);
        }
    }
    append_documentation_section(&mut compact, &section_name, &section_lines);
    compact.markdown = compact.markdown.trim().to_string();
    compact
}

fn documentation_section_heading(line: &str) -> Option<&str> {
    line.strip_prefix("## ")
        .or_else(|| (line.trim() == "**Examples:**").then_some("Examples"))
}

fn append_documentation_section(
    compact: &mut HoverDocumentation,
    section_name: &str,
    lines: &[&str],
) {
    if matches!(section_name, "Methods" | "Method details") {
        return;
    }
    if section_name == "Examples" {
        let fences = markdown_fences(lines);
        for (index, (input, output)) in example_pairs(&fences).into_iter().enumerate() {
            if !compact.has_examples {
                compact.markdown.push_str("## Examples\n\n");
            }
            compact
                .markdown
                .push_str(&render_example_card(index + 1, input, output));
            compact.markdown.push('\n');
            compact.has_examples = true;
        }
        return;
    }

    let content = lines
        .iter()
        .copied()
        .filter(|line| !line.contains("_undocumented_"))
        .filter(|line| {
            if section_name.is_empty() {
                !line.starts_with("# ") && !is_documentation_metadata(line)
            } else {
                true
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let content = content.trim();
    if content.is_empty() {
        return;
    }

    if !compact.markdown.is_empty() {
        compact.markdown.push_str("\n\n");
    }
    if !section_name.is_empty() {
        compact.markdown.push_str(&format!("## {section_name}\n\n"));
    }
    compact.markdown.push_str(content);
}

fn is_documentation_metadata(line: &str) -> bool {
    ["- **Package:**", "- **Class:**"]
        .iter()
        .any(|prefix| line.trim_start().starts_with(prefix))
}

fn markdown_fences(lines: &[&str]) -> Vec<MarkdownFence> {
    let mut fences = Vec::new();
    let mut language = None;
    let mut content = Vec::new();

    for line in lines {
        if let Some(opening) = line.trim().strip_prefix("```") {
            if language.is_none() {
                language = Some(opening.trim().to_string());
                content.clear();
            } else {
                fences.push(MarkdownFence {
                    language: language.take().unwrap_or_default(),
                    content: content.join("\n"),
                });
            }
        } else if language.is_some() {
            content.push(*line);
        }
    }
    if let Some(language) = language {
        fences.push(MarkdownFence {
            language,
            content: content.join("\n"),
        });
    }
    fences
}

fn example_pairs(fences: &[MarkdownFence]) -> Vec<(&str, Option<&str>)> {
    let mut examples = Vec::new();
    let mut index = 0;
    while index < fences.len() {
        let fence = &fences[index];
        if is_macaulay2_fence(&fence.language)
            && fences
                .get(index + 1)
                .is_some_and(|next| is_result_fence(&next.language))
        {
            examples.push((
                fence.content.as_str(),
                Some(fences[index + 1].content.as_str()),
            ));
            index += 2;
            continue;
        }

        let (input, output) = split_combined_example(&fence.content);
        examples.push((input, output));
        index += 1;
    }
    examples
}

fn is_macaulay2_fence(language: &str) -> bool {
    matches!(language.to_ascii_lowercase().as_str(), "macaulay2" | "m2")
}

fn is_result_fence(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "" | "text" | "output"
    )
}

fn split_combined_example(content: &str) -> (&str, Option<&str>) {
    let lines = content.lines().collect::<Vec<_>>();
    if lines.len() < 2 {
        return (content, None);
    }

    let mut input_end = 1;
    while input_end < lines.len()
        && (lines[input_end - 1].trim_end().ends_with(';')
            || delimiter_balance(&lines[..input_end]) > 0
            || lines[input_end].starts_with(char::is_whitespace))
    {
        input_end += 1;
    }
    if input_end == lines.len() {
        return (content, None);
    }

    let input_bytes = lines[..input_end]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .saturating_sub(1);
    let output_start = input_bytes + 1;
    (&content[..input_bytes], Some(&content[output_start..]))
}

fn delimiter_balance(lines: &[&str]) -> isize {
    lines
        .iter()
        .flat_map(|line| line.chars())
        .fold(0, |balance, character| match character {
            '(' | '[' | '{' => balance + 1,
            ')' | ']' | '}' => balance - 1,
            _ => balance,
        })
}

fn render_example_card(index: usize, input: &str, output: Option<&str>) -> String {
    let mut card = format!("> **Example {index}**\n>\n> **Input**\n>\n");
    append_blockquote_fence(&mut card, "macaulay2", input);
    if let Some(output) = output.filter(|output| !output.trim().is_empty()) {
        card.push_str(">\n> **Result**\n>\n");
        append_blockquote_fence(&mut card, "text", output);
    }
    card
}

fn append_blockquote_fence(markdown: &mut String, language: &str, content: &str) {
    markdown.push_str(&format!("> ```{language}\n"));
    for line in content.lines() {
        markdown.push_str(&format!("> {line}\n"));
    }
    markdown.push_str("> ```\n");
}

fn record_typical_value(record: &Record) -> Option<String> {
    record
        .typical_value
        .as_ref()
        .map(|type_id| type_id.name().to_string())
}

fn signature_label(signature: &ResolvedSignature, operator_info: Option<&OperatorInfo>) -> String {
    let domain_parts = signature
        .signature
        .iter()
        .skip(1)
        .map(|s| s.0.as_str())
        .collect::<Vec<_>>();
    let domain = operator_signature_domain_label(signature, operator_info, &domain_parts)
        .unwrap_or_else(|| format!("({})", domain_parts.join(", ")));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_index::{BuiltinData, InstanceID};
    use tower_lsp::lsp_types::HoverContents;

    #[test]
    fn compact_hover_removes_duplicate_metadata_and_boxes_examples() {
        let corpus = concat!(
            "{\"kind\":\"methodFunction\",\"name\":\"scan\",",
            "\"package\":\"$Core$Core\",\"class\":\"$Core$CompiledFunction\",",
            "\"description\":\"Apply a function to each element\",",
            "\"methods\":[",
            "{\"domain\":[\"ZZ\",\"Function\"],\"typicalValue\":\"Number\"},",
            "{\"domain\":[\"BasicList\",\"Function\"],\"typicalValue\":null}],",
            "\"markdown\":\"# scan\\nApply a function to each element\\n\\n",
            "- **Package:** [Core](Core.md)\\n- **Class:** `CompiledFunction`\\n",
            "- **Returns:** _undocumented_\\n\\n## Description\\nUseful details.\\n\\n",
            "## Methods\\n- [`scan(ZZ,Function)`](#method)\\n\\n",
            "## Examples\\n```macaulay2\\nscan(3, print)\\n0\\n1\\n2\\n```\\n\\n",
            "## Method details\\nResult type: _undocumented_\\n\\n",
            "## See also\\n- `apply`\"}\n",
        );
        let knowledge = BuiltinData::load_from_index(corpus);
        let record = knowledge
            .get_record(&InstanceID::new("scan"))
            .expect("scan should deserialize");
        let hover = record_hover_with_package_and_usage(record, Some("Core"), &knowledge, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup
            .value
            .starts_with("**scan**\nType: `CompiledFunction` · Package: `Core`"));
        assert!(markup.value.contains("**Signatures:**"));
        assert!(markup.value.contains("`(ZZ, Function) -> Number`"));
        assert!(markup.value.contains("`(BasicList, Function)`"));
        assert!(!markup.value.contains("# scan"));
        assert!(!markup.value.contains("_undocumented_"));
        assert!(!markup.value.contains("## Methods"));
        assert!(!markup.value.contains("## Method details"));
        assert!(markup.value.contains("## Description\n\nUseful details."));
        assert!(markup.value.contains("> **Example 1**"));
        assert!(markup
            .value
            .contains("> ```macaulay2\n> scan(3, print)\n> ```"));
        assert!(markup
            .value
            .contains("> **Result**\n>\n> ```text\n> 0\n> 1\n> 2\n> ```"));
        assert!(markup.value.contains("## See also\n\n- `apply`"));
    }

    #[test]
    fn adjacent_input_and_result_fences_share_one_example_card() {
        let documentation = concat!(
            "## Examples\n",
            "```macaulay2\nvalue = computation()\n```\n",
            "```text\no1 = 42\n```\n",
        );

        let compact = compact_documentation_markdown(documentation);

        assert_eq!(compact.markdown.matches("> **Example").count(), 1);
        assert!(compact
            .markdown
            .contains("> ```macaulay2\n> value = computation()\n> ```"));
        assert!(compact.markdown.contains("> ```text\n> o1 = 42\n> ```"));
    }

    #[test]
    fn option_roles_come_from_structured_option_relationships() {
        let corpus = concat!(
            "{\"kind\":\"function\",\"name\":\"compute\",\"options\":[",
            "{\"key\":\"Strategy\",\"possibleValues\":[\"Fast\"]}]}\n",
            "{\"kind\":\"symbol\",\"name\":\"Strategy\",\"class\":\"$Core$Symbol\"}\n",
            "{\"kind\":\"symbol\",\"name\":\"Fast\",\"class\":\"$Core$Symbol\"}\n",
        );
        let knowledge = BuiltinData::load_from_index(corpus);

        let key = knowledge
            .get_record(&InstanceID::new("Strategy"))
            .expect("option key should load");
        let key_hover = record_hover_with_package_and_usage(key, None, &knowledge, None);
        let HoverContents::Markup(key_markup) = key_hover.contents else {
            panic!("record hover should use markdown");
        };
        assert!(key_markup.value.contains("Option Role: `key`"));
        assert!(key_markup
            .value
            .contains("**Accepted By Methods:**\n- `compute`"));

        let value = knowledge
            .get_record(&InstanceID::new("Fast"))
            .expect("option value should load");
        let value_hover = record_hover_with_package_and_usage(value, None, &knowledge, None);
        let HoverContents::Markup(value_markup) = value_hover.contents else {
            panic!("record hover should use markdown");
        };
        assert!(value_markup.value.contains("Option Role: `value`"));
        assert!(value_markup
            .value
            .contains("**Valid As Option Value:**\n- `compute.Strategy`"));
    }

    #[test]
    fn generated_scan_hover_keeps_the_selected_domain_by_the_name() {
        let knowledge = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        let record = knowledge
            .get_record(&InstanceID::new("scan"))
            .expect("generated scan metadata should load");
        let usage = knowledge
            .resolve_call_signature_usage(
                "scan",
                &[
                    Some(InstanceID::new("BasicList")),
                    Some(InstanceID::new("Function")),
                ],
            )
            .expect("scan should resolve the BasicList, Function usage");
        let hover =
            record_hover_with_package_and_usage(record, Some("Core"), &knowledge, Some(&usage));
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.starts_with(
            "**scan** `(BasicList, Function)`\nType: `CompiledFunction` · Package: `Core`"
        ));
        assert!(!markup.value.contains("_undocumented_"));
        assert!(!markup.value.contains("## Methods"));
        assert!(markup.value.contains("## Examples"));
        assert!(markup.value.contains("> **Example 1**"));
    }

    #[test]
    fn record_hover_includes_explicit_package_context() {
        let knowledge = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        let record = knowledge
            .get_record(&InstanceID::new("clearAll"))
            .expect("clearAll should have builtin metadata");

        let hover = record_hover_with_package_and_usage(record, Some("Core"), &knowledge, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("Package: `Core`"));
    }

    #[test]
    fn option_value_usage_lookup_resolves_from_possible_values() {
        let corpus = concat!(
            "{\"kind\":\"symbol\",\"name\":\"LongPolynomial\",\"class\":\"Symbol\"}\n",
            "{\"kind\":\"methodFunction\",\"name\":\"gb\",\"options\":[{\"key\":\"Strategy\",\"possibleValues\":[\"LongPolynomial\"]}]}\n",
        );
        let knowledge = BuiltinData::load_from_index(corpus);

        assert_eq!(
            knowledge.option_value_usage_names("LongPolynomial", 8),
            vec!["gb.Strategy"],
        );
    }

    #[test]
    fn record_hover_includes_documented_signatures_and_examples() {
        let corpus = concat!(
            "{\"kind\":\"methodFunction\",\"name\":\"kernel\",",
            "\"methods\":[{\"domain\":[\"RingMap\"],\"typicalValue\":\"Ideal\"}],",
            "\"markdown\":\"**Examples:**\\n```macaulay2\\nR = QQ[a..d];\\nker F\\n```\"}\n",
        );
        let knowledge = BuiltinData::load_from_index(corpus);
        let record = knowledge
            .get_record(&InstanceID::new("kernel"))
            .expect("kernel should deserialize");

        let hover = record_hover_with_package_and_usage(record, Some("Core"), &knowledge, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("`(RingMap) -> Ideal`"));
        assert!(markup.value.contains("> ```macaulay2\n> R = QQ[a..d];"));
    }

    #[test]
    fn record_hover_includes_global_typical_value() {
        let knowledge = BuiltinData::load_from_index(
            "{\"kind\":\"methodFunction\",\"name\":\"method\",\"typical_value\":\"MethodFunction\"}\n",
        );
        let record = knowledge
            .get_record(&InstanceID::new("method"))
            .expect("method should deserialize");

        let hover = record_hover_with_package_and_usage(record, Some("Core"), &knowledge, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("Typical Value: `MethodFunction`"));
    }

    #[test]
    fn record_hover_renders_documented_operator_signatures_in_operator_form() {
        let knowledge = BuiltinData::load_from_index(
            "{\"kind\":\"operator\",\"name\":\"=>\",\"operator\":{\"forms\":[\"binary\"]},\"methods\":[{\"domain\":[\"Thing\",\"Thing\"],\"typicalValue\":\"Option\"}]}\n",
        );
        let record = knowledge
            .get_record(&InstanceID::new("=>"))
            .expect("=> should have operator metadata");

        let hover = record_hover_with_package_and_usage(record, Some("Core"), &knowledge, None);
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.contains("`Thing => Thing -> Option`"));
        assert!(!markup.value.contains("`Thing, Thing -> Option`"));
    }

    #[test]
    fn record_hover_keeps_excluded_signatures_when_usage_is_pinned() {
        let knowledge = BuiltinData::load_from_index(concat!(
            "{\"kind\":\"methodFunction\",\"name\":\"f\",\"methods\":[",
            "{\"domain\":[\"String\"],\"typicalValue\":\"File\"},",
            "{\"domain\":[\"ZZ\"],\"typicalValue\":\"Ring\"}",
            "]}\n",
        ));
        let record = knowledge
            .get_record(&InstanceID::new("f"))
            .expect("f should have builtin metadata");
        let usage = knowledge
            .resolve_call_signature_usage("f", &[Some(InstanceID::new("String"))])
            .expect("f String should resolve to a documented installation");

        let hover =
            record_hover_with_package_and_usage(record, Some("Core"), &knowledge, Some(&usage));
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("record hover should use markdown");
        };

        assert!(markup.value.starts_with("**f** `(String) -> File`"));
        assert!(!markup.value.contains("**Signature:**"));
        assert!(markup.value.contains("**Other signatures for this call:**"));
        assert!(markup.value.contains("`(ZZ) -> Ring`"));
    }
}
