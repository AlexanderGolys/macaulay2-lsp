use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceID(pub String);

impl InstanceID {
    pub fn new(name: &str) -> Self {
        InstanceID(name.to_string())
    }
}

impl fmt::Display for InstanceID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeExample(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub name: InstanceID,
    pub data_type: InstanceID,
    pub description_short: Option<String>,
    pub description_long: Option<String>,
    pub examples: Vec<CodeExample>,

    #[serde(default)]
    pub extra: HashMap<String, Value>,

    #[serde(default)]
    pub documentation: Option<DocumentationInfo>,

    #[serde(default)]
    pub function_info: Option<FunctionInfo>,

    #[serde(default)]
    pub operator_info: Option<OperatorInfo>,

    #[serde(default)]
    pub type_info: Option<TypeInfo>,

    #[serde(default)]
    pub relation_info: Option<RelationInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionInfo {
    #[serde(default)]
    pub methods: Vec<MethodSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentationInfo {
    pub status: DocumentationStatus,
    pub doc_key: Option<InstanceID>,
    pub source_file: Option<String>,
    pub source_line: Option<u64>,
    #[serde(default)]
    pub upstream_eval_status: Option<String>,
    #[serde(default)]
    pub upstream_raw: Option<String>,
    #[serde(default)]
    pub upstream_fields: Vec<String>,
    #[serde(default)]
    pub upstream_field_data: HashMap<String, Value>,
    pub upstream_description_short: Option<String>,
    pub upstream_description_long: Option<String>,
    #[serde(default)]
    pub upstream_inputs: Option<Value>,
    #[serde(default)]
    pub upstream_outputs: Option<Value>,
    #[serde(default)]
    pub upstream_description_body: Option<Value>,
    #[serde(default)]
    pub upstream_usage: Option<Value>,
    #[serde(default)]
    pub upstream_see_also: Option<Value>,
    #[serde(default)]
    pub upstream_key: Option<String>,
    #[serde(default)]
    pub upstream_document_tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationStatus {
    Upstream,
    Missing,
    Generated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodSignature {
    #[serde(default)]
    pub signature: Vec<InstanceID>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorInfo {
    pub method_lookup: String,
    pub method_symbol: InstanceID,
    #[serde(default)]
    pub forms: Vec<String>,
    #[serde(default)]
    pub flags: HashMap<String, Vec<String>>,
    pub flexible: bool,
    #[serde(default)]
    pub attributes: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeInfo {
    #[serde(default)]
    pub subtypes: Vec<InstanceID>,
    pub parent_type: Option<InstanceID>,
    #[serde(default)]
    pub ancestors: Vec<InstanceID>,
    #[serde(default)]
    pub instances: Vec<InstanceID>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationInfo {
    pub parent: Option<InstanceID>,
    #[serde(default)]
    pub ancestors: Vec<InstanceID>,
    pub class: Option<InstanceID>,
    #[serde(default)]
    pub class_ancestors: Vec<InstanceID>,
    #[serde(default)]
    pub children: Vec<InstanceID>,
    #[serde(default)]
    pub instances: Vec<InstanceID>,
}

#[derive(Debug, Clone)]
pub struct BuiltinData {
    names: Vec<InstanceID>,
    name_to_index: HashMap<InstanceID, usize>,
    details: String,
    detail_ranges: Vec<(usize, usize)>,
}

impl BuiltinData {
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn contains_name(&self, name: &str) -> bool {
        self.name_to_index.contains_key(&InstanceID::new(name))
    }

    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<&str> {
        if prefix.is_empty() || limit == 0 {
            return Vec::new();
        }

        self.names
            .iter()
            .map(|name| name.0.as_str())
            .filter(|name| name.starts_with(prefix))
            .take(limit)
            .collect()
    }

    pub fn get_record(&self, name: &InstanceID) -> Option<Record> {
        let index = *self.name_to_index.get(name)?;
        let (start, end) = *self.detail_ranges.get(index)?;
        serde_json::from_str(&self.details[start..end]).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum M2SemanticTokenType {
    Type = 0,
    Function = 1,
    Variable = 2,
    Parameter = 3,
    Property = 4,
    Namespace = 5,
    EnumMember = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M2SemanticToken {
    pub token_type: M2SemanticTokenType,
    pub is_command: bool,
    pub is_file: bool,
    pub is_manipulator: bool,
}

impl BuiltinData {
    pub fn load_from_split(names: &str, details: &str) -> Self {
        let names: Vec<_> = names
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(InstanceID::new)
            .collect();
        let name_to_index = names
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, name)| (name, index))
            .collect();

        let mut detail_ranges = Vec::new();
        let mut start = 0;
        for line in details.split_inclusive('\n') {
            let end = start + line.trim_end_matches('\n').len();
            if end > start {
                detail_ranges.push((start, end));
            }
            start += line.len();
        }
        if start < details.len() {
            detail_ranges.push((start, details.len()));
        }

        BuiltinData {
            names,
            name_to_index,
            details: details.to_string(),
            detail_ranges,
        }
    }

    pub fn get_semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        let record = self.get_record(&InstanceID::new(name))?;
        let data_type = &record.data_type;

        let function_type = InstanceID::new("Function");
        let command_type = InstanceID::new("Command");
        let file_type = InstanceID::new("File");
        let manipulator_type = InstanceID::new("Manipulator");
        let package_type = InstanceID::new("Package");
        let keyword_type = InstanceID::new("Keyword");
        let operator_type = InstanceID::new("Operator");
        let symbol_type = InstanceID::new("Symbol");

        let is_command = self.is_subtype(data_type, &command_type);
        let is_file = self.is_subtype(data_type, &file_type);
        let is_manipulator = self.is_subtype(data_type, &manipulator_type);

        // 1. If it has a parent type, it's a derived Type (like ZZ, Ring, etc.)
        if let Some(type_info) = &record.type_info {
            if type_info.parent_type.is_some() {
                return Some(M2SemanticToken {
                    token_type: M2SemanticTokenType::Type,
                    is_command: false,
                    is_file: false,
                    is_manipulator: false,
                });
            }
        }

        // 2. Hierarchy traversal for other categories
        if self.is_subtype(data_type, &function_type) || is_manipulator || is_command {
            Some(M2SemanticToken {
                token_type: M2SemanticTokenType::Function,
                is_command,
                is_file: false,
                is_manipulator,
            })
        } else if self.is_subtype(data_type, &package_type) {
            Some(M2SemanticToken {
                token_type: M2SemanticTokenType::Namespace,
                is_command: false,
                is_file: false,
                is_manipulator: false,
            })
        } else if (self.is_subtype(data_type, &symbol_type) || is_file)
            && !self.is_subtype(data_type, &keyword_type)
            && !self.is_subtype(data_type, &operator_type)
        {
            Some(M2SemanticToken {
                token_type: M2SemanticTokenType::Variable,
                is_command: false,
                is_file,
                is_manipulator: false,
            })
        } else {
            None
        }
    }

    /// Check if child is a subtype of parent (inclusive)
    pub fn is_subtype(&self, child: &InstanceID, parent: &InstanceID) -> bool {
        let mut current = child.clone();
        let mut visited = std::collections::HashSet::new();
        visited.insert(current.clone());

        loop {
            if current == *parent {
                return true;
            }
            // We look up 'current', but we don't hold the reference for long
            let next_parent = if let Some(record) = self.get_record(&current) {
                if let Some(type_info) = &record.type_info {
                    type_info.parent_type.clone()
                } else {
                    None
                }
            } else {
                None
            };

            match next_parent {
                Some(p) => {
                    if visited.contains(&p) {
                        break;
                    } // Cycle detected
                    current = p;
                    visited.insert(current.clone());
                }
                None => break,
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated_builtins() -> BuiltinData {
        BuiltinData::load_from_split(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
        )
    }

    #[test]
    fn generated_builtin_data_loads_core_symbols() {
        let builtins = generated_builtins();
        let generated_name_count = include_str!("./data/builtins.names")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        let generated_detail_count = include_str!("./data/builtins.details.jsonl")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        assert_eq!(
            generated_name_count, generated_detail_count,
            "names and detail JSONL should stay line-aligned"
        );
        assert_eq!(builtins.len(), generated_name_count);
        assert!(
            builtins.len() > 2_700,
            "expected a substantial generated builtin database"
        );
        assert!(builtins.contains_name("ideal"));
        assert!(
            builtins.names_with_prefix("id", 8).contains(&"ideal"),
            "name index should support live prefix symbol search"
        );

        let ideal = builtins
            .get_record(&InstanceID::new("ideal"))
            .expect("ideal should be present");
        assert_eq!(ideal.description_short.as_deref(), Some("make an ideal"));
        assert!(
            ideal
                .function_info
                .as_ref()
                .is_some_and(|info| info.methods.len() >= 10),
            "ideal should include installed method signatures"
        );

        let ring = builtins
            .get_record(&InstanceID::new("Ring"))
            .expect("Ring should be present");
        assert_eq!(
            ring.type_info
                .as_ref()
                .and_then(|info| info.parent_type.as_ref())
                .map(|parent| parent.0.as_str()),
            Some("Type")
        );
        assert!(
            ring.type_info
                .as_ref()
                .is_some_and(|info| info.subtypes.contains(&InstanceID::new("EngineRing"))),
            "Ring should carry runtime parent-tree children"
        );

        let core_stash_value = builtins
            .get_record(&InstanceID::new("Core$stashValue"))
            .expect("runtime-only Core$stashValue should be present");
        assert!(
            core_stash_value.description_short.is_none(),
            "missing docs should deserialize as null placeholders"
        );
        assert_eq!(
            core_stash_value
                .documentation
                .as_ref()
                .map(|documentation| &documentation.status),
            Some(&DocumentationStatus::Missing),
            "missing docs should be explicit TODOs, not confused with non-applicable sections"
        );
        assert!(
            builtins
                .get_record(&InstanceID::new("ZZ"))
                .is_some_and(|record| record.function_info.is_none()),
            "function_info should be absent when it does not apply"
        );

        let plus = builtins
            .get_record(&InstanceID::new("+"))
            .expect("+ operator should be present");
        assert_eq!(
            plus.operator_info
                .as_ref()
                .map(|info| info.method_lookup.as_str()),
            Some("symbol"),
            "operators should record that dispatch is looked up through symbol syntax"
        );
        assert!(
            plus.function_info
                .as_ref()
                .is_some_and(|info| info.methods.len() >= 100),
            "+ should include the symbol-keyed operator method table"
        );
        assert!(
            plus.operator_info.as_ref().is_some_and(|info| info.flexible
                && info.forms.contains(&"Binary".to_string())
                && info.forms.contains(&"Prefix".to_string())),
            "+ should preserve operator form and flag metadata"
        );
    }

    #[test]
    fn generated_builtin_data_classifies_semantic_tokens() {
        let builtins = generated_builtins();

        assert_eq!(
            builtins
                .get_semantic_token("ideal")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function)
        );
        assert_eq!(
            builtins
                .get_semantic_token("Ring")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Type)
        );
        assert_eq!(
            builtins
                .get_semantic_token("ZZ")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Type)
        );
        assert_eq!(
            builtins
                .get_semantic_token("endl")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "M2 Manipulator values should still receive a useful semantic token"
        );
        assert!(
            builtins
                .get_semantic_token("endl")
                .is_some_and(|token| token.is_manipulator),
            "M2 Manipulator values should retain their runtime-class modifier"
        );
        assert_eq!(
            builtins
                .get_semantic_token("clearAll")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "M2 Command values should use a standard semantic token"
        );
        assert!(
            builtins
                .get_semantic_token("clearAll")
                .is_some_and(|token| token.is_command),
            "M2 Command values should retain their runtime-class modifier"
        );
        assert_eq!(
            builtins
                .get_semantic_token("stderr")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Variable),
            "M2 File values should use a standard semantic token"
        );
        assert!(
            builtins
                .get_semantic_token("stderr")
                .is_some_and(|token| token.is_file),
            "M2 File values should retain their runtime-class modifier"
        );
    }

    #[test]
    fn prefix_search_does_not_require_sorted_names() {
        let builtins =
            BuiltinData::load_from_split("ZZ\nabout\nRing\ncoefficient\n", "{}\n{}\n{}\n{}\n");

        assert_eq!(builtins.names_with_prefix("ab", 8), vec!["about"]);
        assert_eq!(builtins.names_with_prefix("co", 8), vec!["coefficient"]);
        assert_eq!(builtins.names_with_prefix("R", 8), vec!["Ring"]);
        assert_eq!(builtins.names_with_prefix("Z", 8), vec!["ZZ"]);
    }
}
