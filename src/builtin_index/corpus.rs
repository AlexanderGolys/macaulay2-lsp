//! JSONL deserialization boundary for the generated builtin corpus.

use std::collections::HashMap;

use serde::Deserialize;

/// Deserialize every non-empty physical line in a generated JSONL corpus.
///
/// Markdown newlines are escaped inside each JSON object, so one physical line
/// always contains one complete record.
pub fn deserialize_records(corpus: &str) -> impl Iterator<Item = RawRecord> + '_ {
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
pub struct RawRecord {
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub normalized_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub extra_keys: Vec<String>,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub typical_value: Option<String>,
    #[serde(default)]
    pub options: Vec<RawOptionSpec>,
    #[serde(default)]
    pub methods: Vec<RawMethod>,
    #[serde(default)]
    pub operator: Option<RawOperator>,
    #[serde(default)]
    pub default_loaded: Vec<String>,
    #[serde(default)]
    pub protected: Option<bool>,
    #[serde(default)]
    pub markdown: Option<String>,
}

/// Wire representation of one callable method signature.
#[derive(Debug, Deserialize)]
pub struct RawMethod {
    #[serde(default)]
    pub domain: Vec<String>,
    #[serde(default, rename = "typicalValue")]
    pub typical_value: Option<String>,
}

/// Wire representation of one callable option specification.
#[derive(Debug, Deserialize)]
pub struct RawOptionSpec {
    pub key: String,
    #[serde(default, rename = "possibleValues")]
    pub possible_values: Vec<String>,
}

/// Wire representation of an operator's forms and form attributes.
#[derive(Debug, Deserialize)]
pub struct RawOperator {
    #[serde(default)]
    pub forms: Vec<String>,
    #[serde(default)]
    pub attributes: HashMap<RawOperatorForm, Vec<String>>,
}

/// Lowercase operator form used by the generated corpus.
#[derive(Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum RawOperatorForm {
    Binary,
    Prefix,
    Postfix,
    Assignment,
}
