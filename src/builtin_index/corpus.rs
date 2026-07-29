//! JSONL deserialization boundary for the generated builtin corpus.

use std::collections::HashMap;

use serde::Deserialize;

/// Deserialize every non-empty physical line in a generated JSONL corpus.
///
/// Markdown newlines are escaped inside each JSON object, so one physical line
/// always contains one complete record.
pub(super) fn deserialize_records(corpus: &str) -> impl Iterator<Item = RawRecord> + '_ {
    corpus.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let record: RawRecord = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("malformed corpus line: {error}\n{line}"));
        if record.kind != "meta" && record.name.is_empty() {
            panic!(
                "corpus record of kind '{}' has no name: {line}",
                record.kind
            );
        }
        Some(record)
    })
}

/// Wire representation of one generated corpus record.
#[derive(Debug, Deserialize)]
pub(super) struct RawRecord {
    pub(super) kind: String,
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) normalized_name: String,
    #[serde(default)]
    pub(super) aliases: Vec<String>,
    #[serde(default)]
    pub(super) extra_keys: Vec<String>,
    #[serde(default)]
    pub(super) package: Option<String>,
    #[serde(default)]
    pub(super) class: Option<String>,
    #[serde(default)]
    pub(super) parent: Option<String>,
    #[serde(default)]
    pub(super) typical_value: Option<String>,
    #[serde(default)]
    pub(super) options: Vec<RawOptionSpec>,
    #[serde(default)]
    pub(super) methods: Vec<RawMethod>,
    #[serde(default)]
    pub(super) operator: Option<RawOperator>,
    #[serde(default)]
    pub(super) default_loaded: Vec<String>,
    #[serde(default)]
    pub(super) protected: Option<bool>,
    #[serde(default)]
    pub(super) markdown: Option<String>,
}

/// Wire representation of one callable method signature.
#[derive(Debug, Deserialize)]
pub(super) struct RawMethod {
    #[serde(default)]
    pub(super) domain: Vec<String>,
    #[serde(default, rename = "typicalValue")]
    pub(super) typical_value: Option<String>,
}

/// Wire representation of one callable option specification.
#[derive(Debug, Deserialize)]
pub(super) struct RawOptionSpec {
    pub(super) key: String,
    #[serde(default, rename = "possibleValues")]
    pub(super) possible_values: Vec<String>,
}

/// Wire representation of an operator's forms and form attributes.
#[derive(Debug, Deserialize)]
pub(super) struct RawOperator {
    #[serde(default)]
    pub(super) forms: Vec<String>,
    #[serde(default)]
    pub(super) attributes: HashMap<RawOperatorForm, Vec<String>>,
}

/// Lowercase operator form used by the generated corpus.
#[derive(Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub(super) enum RawOperatorForm {
    Binary,
    Prefix,
    Postfix,
    Assignment,
}
