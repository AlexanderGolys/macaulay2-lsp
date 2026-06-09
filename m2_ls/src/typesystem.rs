use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeExample(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub name: InstanceID,
    pub class: InstanceID,
    #[serde(default)]
    pub description_short: Option<String>,
    #[serde(default)]
    pub description_long: Option<String>,
    #[serde(default)]
    pub examples: Vec<CodeExample>,
    #[serde(default)]
    pub extra: HashMap<String, Value>,
    #[serde(default)]
    pub documentation: Option<DocumentationInfo>,
    #[serde(default)]
    pub function_info: Option<FunctionInfo>,
    #[serde(default)]
    pub option_info: Option<OptionInfo>,
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
    #[serde(default)]
    pub documented_methods: Vec<DocumentedMethodSignature>,
    #[serde(default)]
    pub general_signature: Option<DocumentedMethodSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionInfo {
    #[serde(default)]
    pub options: Vec<MethodOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodOption {
    pub name: InstanceID,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentationInfo {
    pub status: DocumentationStatus,
    #[serde(default)]
    pub doc_key: Option<InstanceID>,
    #[serde(default)]
    pub source_file: Option<String>,
    #[serde(default)]
    pub source_line: Option<u64>,
    #[serde(default)]
    pub upstream_eval_status: Option<String>,
    #[serde(default)]
    pub upstream_raw: Option<String>,
    #[serde(default)]
    pub upstream_fields: Vec<String>,
    #[serde(default)]
    pub upstream_field_data: HashMap<String, Value>,
    #[serde(default)]
    pub upstream_description_short: Option<String>,
    #[serde(default)]
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
pub struct DocumentedMethodSignature {
    #[serde(default)]
    pub signature: Vec<InstanceID>,
    #[serde(default)]
    pub output_types: Vec<InstanceID>,
    #[serde(default)]
    pub examples: Vec<CodeExample>,
    #[serde(default)]
    pub doc_key: Option<InstanceID>,
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
    #[serde(default)]
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
    #[serde(default)]
    pub class: Option<InstanceID>,
    #[serde(default)]
    pub class_ancestors: Vec<InstanceID>,
    #[serde(default)]
    pub children: Vec<InstanceID>,
    #[serde(default)]
    pub instances: Vec<InstanceID>,
}

/// Two-sided type hierarchy: `ancestors` (sorted, for upward `is_subtype`/lub/glb
/// queries) and `children` (immediate subtypes, for the downward `type_hierarchy`
/// view). Instance checks only ever walk upward; `children` is read straight, not
/// recomputed.
#[derive(Debug, Clone, Default)]
pub struct TypeLattice {
    ancestors: HashMap<InstanceID, Vec<InstanceID>>,
    children: HashMap<InstanceID, Vec<InstanceID>>,
}

impl TypeLattice {
    pub fn from_builtins(builtins: &BuiltinData) -> Self {
        let mut ancestors: HashMap<_, Vec<_>> = HashMap::new();
        let mut children: HashMap<_, Vec<_>> = HashMap::new();

        for name in &builtins.names {
            let Some(record) = builtins.get_record(name) else {
                continue;
            };
            let Some(type_info) = record.type_info else {
                continue;
            };
            ancestors.insert(name.clone(), {
                let mut chain = Vec::with_capacity(type_info.ancestors.len() + 1);
                for ancestor in &type_info.ancestors {
                    chain.push(ancestor.clone());
                }
                chain.sort();
                chain.dedup();
                chain
            });
            for subtype in &type_info.subtypes {
                children
                    .entry(subtype.clone())
                    .or_default()
                    .push(name.clone());
            }
        }

        TypeLattice {
            ancestors,
            children,
        }
    }

    /// Build the lattice from the `m2-types.jsonl` type records: each carries its
    /// full ancestor chain (sorted here for binary search) and its immediate
    /// subtypes (the children edge).
    pub fn from_type_index(index: &crate::builtin_index::BuiltinIndex) -> Self {
        let mut ancestors: HashMap<InstanceID, Vec<InstanceID>> = HashMap::new();
        let mut children: HashMap<InstanceID, Vec<InstanceID>> = HashMap::new();

        for entry in index.types() {
            let id = InstanceID::new(&entry.name);
            let mut chain: Vec<InstanceID> =
                entry.ancestors.iter().map(|a| InstanceID::new(a)).collect();
            chain.sort();
            chain.dedup();
            ancestors.insert(id.clone(), chain);
            children.insert(
                id,
                entry.subtypes.iter().map(|s| InstanceID::new(s)).collect(),
            );
        }

        TypeLattice {
            ancestors,
            children,
        }
    }

    /// Immediate subtypes of `parent` (the downward edge, for `type_hierarchy`).
    pub fn children(&self, parent: &InstanceID) -> &[InstanceID] {
        self.children.get(parent).map_or(&[], Vec::as_slice)
    }

    pub fn is_subtype(&self, child: &InstanceID, parent: &InstanceID) -> bool {
        child == parent
            || self
                .ancestors
                .get(child)
                .is_some_and(|chain| chain.binary_search(parent).is_ok())
    }

    pub fn least_upper_bound(&self, a: &InstanceID, b: &InstanceID) -> Option<InstanceID> {
        if a == b {
            return Some(a.clone());
        }
        let ancestors_a = self.ancestors.get(a)?;
        let ancestors_b = self.ancestors.get(b)?;
        if ancestors_b.binary_search(a).is_ok() {
            return Some(b.clone());
        }
        if ancestors_a.binary_search(b).is_ok() {
            return Some(a.clone());
        }
        let common: Vec<_> = ancestors_a
            .iter()
            .filter(|ancestor| ancestors_b.binary_search(ancestor).is_ok())
            .collect();
        common
            .into_iter()
            .min_by_key(|candidate| {
                ancestors_a
                    .iter()
                    .filter(|a| {
                        self.ancestors
                            .get(a)
                            .is_some_and(|chain| chain.contains(candidate))
                    })
                    .count()
            })
            .cloned()
    }

    pub fn greatest_lower_bound(&self, a: &InstanceID, b: &InstanceID) -> Option<InstanceID> {
        if self.is_subtype(a, b) {
            return Some(a.clone());
        }
        if self.is_subtype(b, a) {
            return Some(b.clone());
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct BuiltinData {
    names: Vec<InstanceID>,
    name_to_index: HashMap<InstanceID, usize>,
    details: String,
    detail_ranges: Vec<(usize, usize)>,
    type_facts: TypeFacts,
    type_lattice: TypeLattice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSignature {
    pub signature: Vec<InstanceID>,
    pub output_types: Vec<InstanceID>,
    pub is_specialized: bool,
    pub examples: Vec<CodeExample>,
    pub doc_key: Option<InstanceID>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SignatureUsage {
    pub pinned: Option<ResolvedSignature>,
    pub possible: Vec<ResolvedSignature>,
    pub excluded: Vec<ResolvedSignature>,
}

#[derive(Debug, Clone, Default)]
pub struct TypeFacts {
    signature_codomains: HashMap<(String, Vec<String>), String>,
    /// Installed signatures grouped by method key, for the dispatch lookup —
    /// each key's domains are scanned once per call, not the whole table.
    signatures_by_key: HashMap<String, Vec<(Vec<String>, String)>>,
    option_value_usages: HashMap<String, Vec<OptionValueUsage>>,
    option_values_by_slot: HashMap<(String, String), Vec<String>>,
    option_codomains: Vec<OptionCodomainFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionValueUsage {
    pub callable: String,
    pub option: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OptionCodomainFact {
    callable: String,
    domain: Vec<String>,
    key: String,
    value: String,
    codomain: String,
}

#[derive(Debug, Deserialize)]
struct TypeFactRecord {
    callable: String,
    #[serde(default)]
    signatures: Vec<TypeFactSignature>,
    #[serde(default)]
    options: Vec<TypeFactOption>,
    #[serde(default)]
    option_codomains: Vec<TypeFactOptionCodomain>,
}

#[derive(Debug, Deserialize)]
struct TypeFactSignature {
    #[serde(default)]
    domain: Vec<String>,
    codomain: String,
}

#[derive(Debug, Deserialize)]
struct TypeFactOption {
    key: String,
    #[serde(default)]
    values: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TypeFactOptionCodomain {
    key: String,
    value: String,
    codomain: String,
    #[serde(default)]
    domain: Vec<String>,
}

/// Whether `domain` admits `arguments` componentwise: each argument is the
/// expected type or a subtype of it.
fn domain_admits(lattice: &TypeLattice, domain: &[String], arguments: &[&str]) -> bool {
    domain.len() == arguments.len()
        && domain.iter().zip(arguments).all(|(expected, actual)| {
            *actual == expected.as_str()
                || lattice.is_subtype(&InstanceID::new(actual), &InstanceID::new(expected))
        })
}

/// Whether domain `a` is at least as specific as `b` componentwise (each part of
/// `a` is the matching part of `b` or a subtype of it).
fn domain_at_least_as_specific(lattice: &TypeLattice, a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x == y || lattice.is_subtype(&InstanceID::new(x), &InstanceID::new(y))
        })
}

impl TypeFacts {
    fn load_jsonl(input: &str) -> Self {
        let mut facts = TypeFacts::default();
        for line in input.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(record) = serde_json::from_str::<TypeFactRecord>(line) else {
                continue;
            };

            for signature in record.signatures {
                facts.signature_codomains.insert(
                    (record.callable.clone(), signature.domain),
                    signature.codomain,
                );
            }

            for option in record.options {
                let slot = (record.callable.clone(), option.key.clone());
                for value in option.values {
                    facts
                        .option_values_by_slot
                        .entry(slot.clone())
                        .or_default()
                        .push(value.clone());
                    facts
                        .option_value_usages
                        .entry(value)
                        .or_default()
                        .push(OptionValueUsage {
                            callable: record.callable.clone(),
                            option: option.key.clone(),
                        });
                }
            }

            for option_codomain in record.option_codomains {
                facts.option_codomains.push(OptionCodomainFact {
                    callable: record.callable.clone(),
                    domain: option_codomain.domain,
                    key: option_codomain.key,
                    value: option_codomain.value,
                    codomain: option_codomain.codomain,
                });
            }
        }

        for usages in facts.option_value_usages.values_mut() {
            usages.sort_by(|left, right| {
                (left.callable.as_str(), left.option.as_str())
                    .cmp(&(right.callable.as_str(), right.option.as_str()))
            });
            usages.dedup();
        }
        for values in facts.option_values_by_slot.values_mut() {
            values.sort();
            values.dedup();
        }

        facts
    }

    /// Build the typecheck facts from the `m2-types.jsonl` callable records.
    /// Honours the monotone rule: a method with no codomain (`typicalValue` null)
    /// contributes no signature — the lookup stays silent rather than guess.
    pub fn from_type_index(index: &crate::builtin_index::BuiltinIndex) -> Self {
        let mut facts = TypeFacts::default();
        for callable in index.callables() {
            for signature in &callable.signatures {
                let Some(codomain) = signature.codomain.as_ref() else {
                    continue;
                };
                facts.signature_codomains.insert(
                    (callable.name.clone(), signature.domain.clone()),
                    codomain.clone(),
                );
                facts
                    .signatures_by_key
                    .entry(callable.name.clone())
                    .or_default()
                    .push((signature.domain.clone(), codomain.clone()));
            }
            for option in &callable.options {
                let slot = (callable.name.clone(), option.key.clone());
                for value in &option.possible_values {
                    facts
                        .option_values_by_slot
                        .entry(slot.clone())
                        .or_default()
                        .push(value.clone());
                    facts
                        .option_value_usages
                        .entry(value.clone())
                        .or_default()
                        .push(OptionValueUsage {
                            callable: callable.name.clone(),
                            option: option.key.clone(),
                        });
                }
            }
        }
        for usages in facts.option_value_usages.values_mut() {
            usages.sort_by(|left, right| {
                (left.callable.as_str(), left.option.as_str())
                    .cmp(&(right.callable.as_str(), right.option.as_str()))
            });
            usages.dedup();
        }
        for values in facts.option_values_by_slot.values_mut() {
            values.sort();
            values.dedup();
        }
        facts
    }

    /// Standard multiple-dispatch lookup: among the methods installed under
    /// `key`, pick the most-specific one whose domain componentwise admits the
    /// argument types (each `actual <: expected`), and return its codomain.
    /// `None` ⇒ nothing applicable (or undocumented) — the checker stays silent.
    pub fn resolve_codomain(
        &self,
        lattice: &TypeLattice,
        key: &str,
        arguments: &[&str],
    ) -> Option<&str> {
        let mut best: Option<&(Vec<String>, String)> = None;
        for candidate in self.signatures_by_key.get(key)? {
            let (domain, _) = candidate;
            if domain.len() != arguments.len() {
                continue;
            }
            if !domain_admits(lattice, domain, arguments) {
                continue;
            }
            best = match best {
                None => Some(candidate),
                // Prefer the more-specific domain; first installed wins on a tie.
                Some(current) if domain_at_least_as_specific(lattice, domain, &current.0) => {
                    Some(candidate)
                }
                Some(current) => Some(current),
            };
        }
        best.map(|(_, codomain)| codomain.as_str())
    }

    fn signature_codomain(&self, signature: &[InstanceID]) -> Option<&str> {
        let callable = signature.first()?.0.as_str();
        let domain = signature
            .iter()
            .skip(1)
            .map(|part| part.0.clone())
            .collect::<Vec<_>>();
        self.signature_codomains
            .get(&(callable.to_string(), domain))
            .map(String::as_str)
    }
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

    pub fn matching_names(&self, query: &str, limit: usize) -> Vec<&str> {
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }

        let query = query.to_lowercase();
        self.names
            .iter()
            .map(|name| name.0.as_str())
            .filter(|name| name.to_lowercase().contains(&query))
            .take(limit)
            .collect()
    }

    pub fn get_record(&self, name: &InstanceID) -> Option<Record> {
        let index = *self.name_to_index.get(name)?;
        let (start, end) = *self.detail_ranges.get(index)?;
        serde_json::from_str(&self.details[start..end]).ok()
    }

    pub fn option_usage_names(&self, option_name: &str, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }

        let option_name = option_name
            .rsplit_once('$')
            .map_or(option_name, |(_, name)| name);
        let mut usages = Vec::new();
        for (index, name) in self.names.iter().enumerate() {
            let Some((start, end)) = self.detail_ranges.get(index).copied() else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Record>(&self.details[start..end]) else {
                continue;
            };
            let Some(option_info) = &record.option_info else {
                continue;
            };
            if !option_info
                .options
                .iter()
                .any(|option| option.name.0 == option_name)
            {
                continue;
            }

            let display_name = name
                .0
                .rsplit_once('$')
                .map_or(name.0.as_str(), |(_, name)| name);
            if !usages.iter().any(|usage| usage == display_name) {
                usages.push(display_name.to_string());
            }
            if usages.len() == limit {
                break;
            }
        }

        usages
    }

    pub fn option_value_usage_names(&self, value_name: &str, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }

        let value_name = value_name
            .rsplit_once('$')
            .map_or(value_name, |(_, name)| name);
        self.type_facts
            .option_value_usages
            .get(value_name)
            .into_iter()
            .flat_map(|usages| usages.iter())
            .map(|usage| format!("{}.{}", usage.callable, usage.option))
            .take(limit)
            .collect()
    }

    pub fn is_option_name(&self, name: &str) -> bool {
        self.get_record(&InstanceID::new(name))
            .is_some_and(|record| record.option_role() == Some("key"))
    }

    pub fn is_option_value_name(&self, name: &str) -> bool {
        self.get_record(&InstanceID::new(name))
            .is_some_and(|record| record.option_role() == Some("value"))
            || self.type_facts.option_value_usages.contains_key(name)
    }

    pub fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool {
        let option_key = option_key
            .rsplit_once('$')
            .map_or(option_key, |(_, name)| name);
        let value_name = value_name
            .rsplit_once('$')
            .map_or(value_name, |(_, name)| name);

        self.type_facts
            .option_values_by_slot
            .iter()
            .filter(|((_, key), _)| key == option_key)
            .any(|(_, values)| values.iter().any(|value| value == value_name))
    }

    pub fn documented_signatures(&self, record: &Record) -> Vec<ResolvedSignature> {
        let Some(function_info) = &record.function_info else {
            return Vec::new();
        };

        let specialized_by_domain: HashMap<_, _> = function_info
            .documented_methods
            .iter()
            .filter(|method| !method.output_types.is_empty())
            .filter_map(|method| signature_domain_key(&method.signature).map(|key| (key, method)))
            .collect();
        let general_outputs = function_info
            .general_signature
            .as_ref()
            .filter(|method| !method.output_types.is_empty())
            .map(|method| method.output_types.clone());

        let mut signatures = Vec::new();
        for method in &function_info.methods {
            let Some(domain_key) = signature_domain_key(&method.signature) else {
                continue;
            };
            if let Some(documented_method) = specialized_by_domain.get(&domain_key) {
                signatures.push(ResolvedSignature {
                    signature: documented_method.signature.clone(),
                    output_types: documented_method.output_types.clone(),
                    is_specialized: true,
                    examples: documented_method.examples.clone(),
                    doc_key: documented_method.doc_key.clone(),
                });
            } else if let Some(codomain) = self.type_facts.signature_codomain(&method.signature) {
                signatures.push(ResolvedSignature {
                    signature: method.signature.clone(),
                    output_types: vec![InstanceID::new(codomain)],
                    is_specialized: true,
                    examples: Vec::new(),
                    doc_key: None,
                });
            } else if let Some(output_types) = &general_outputs {
                let general_signature = function_info.general_signature.as_ref();
                signatures.push(ResolvedSignature {
                    signature: method.signature.clone(),
                    output_types: output_types.clone(),
                    is_specialized: false,
                    examples: general_signature
                        .map(|signature| signature.examples.clone())
                        .unwrap_or_default(),
                    doc_key: general_signature.and_then(|signature| signature.doc_key.clone()),
                });
            }
        }

        if signatures.is_empty() {
            if let Some(general_signature) = &function_info.general_signature {
                if !general_signature.output_types.is_empty() {
                    signatures.push(ResolvedSignature {
                        signature: general_signature.signature.clone(),
                        output_types: general_signature.output_types.clone(),
                        is_specialized: false,
                        examples: general_signature.examples.clone(),
                        doc_key: general_signature.doc_key.clone(),
                    });
                }
            }
        }

        signatures
    }

    pub fn undocumented_installed_methods(&self, record: &Record) -> Vec<MethodSignature> {
        let Some(function_info) = &record.function_info else {
            return Vec::new();
        };

        let documented_domains: HashSet<_> = self
            .documented_signatures(record)
            .iter()
            .filter_map(|method| signature_domain_key(&method.signature))
            .collect();

        function_info
            .methods
            .iter()
            .filter(|method| {
                signature_domain_key(&method.signature)
                    .is_none_or(|key| !documented_domains.contains(&key))
            })
            .cloned()
            .collect()
    }

    pub fn resolve_call_return_type(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<String> {
        self.resolve_call_return_type_with_options(callable, argument_types, &[])
    }

    pub fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        literal_options: &[(String, String)],
    ) -> Option<String> {
        let option_codomains =
            self.option_rule_return_types(callable, argument_types, literal_options);
        if let [codomain] = option_codomains.as_slice() {
            return Some(codomain.clone());
        }
        if option_codomains.len() > 1 {
            return None;
        }

        if let Some(signature) = self.resolve_call_signature(callable, argument_types) {
            if let [output_type] = signature.output_types.as_slice() {
                return Some(output_type.0.clone());
            }
        }

        let record = self.get_record(&InstanceID::new(callable))?;
        let unknown_domain_candidates = record
            .function_info
            .as_ref()
            .and_then(|info| info.general_signature.as_ref())
            .and_then(|signature| match signature.output_types.as_slice() {
                [output_type] => Some(vec![output_type.0.clone()]),
                _ => None,
            })
            .unwrap_or_default();

        let mut candidates = unknown_domain_candidates;
        candidates.sort();
        candidates.dedup();
        if let [output_type] = candidates.as_slice() {
            Some(output_type.clone())
        } else {
            None
        }
    }

    fn option_rule_return_types(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        literal_options: &[(String, String)],
    ) -> Vec<String> {
        let option_set = literal_options.iter().cloned().collect::<HashSet<_>>();
        let mut codomains = self
            .type_facts
            .option_codomains
            .iter()
            .filter(|fact| fact.callable == callable)
            .filter(|fact| option_set.contains(&(fact.key.clone(), fact.value.clone())))
            .filter(|fact| {
                fact.domain.is_empty()
                    || (fact.domain.len() == argument_types.len()
                        && domain_possibly_matches(
                            self,
                            &fact
                                .domain
                                .iter()
                                .map(|name| InstanceID::new(name))
                                .collect::<Vec<_>>(),
                            argument_types,
                        ))
            })
            .map(|fact| fact.codomain.clone())
            .collect::<Vec<_>>();
        codomains.sort();
        codomains.dedup();
        codomains
    }

    pub fn resolve_call_signature(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<ResolvedSignature> {
        let record = self.get_record(&InstanceID::new(callable))?;
        let mut specialized_candidates = Vec::new();
        let mut general_candidates = Vec::new();
        for signature in self.documented_signatures(&record) {
            if signature.signature.first().map(|name| name.0.as_str()) != Some(callable) {
                continue;
            }
            let domain = signature.signature.get(1..).unwrap_or_default();
            if domain.is_empty() {
                continue;
            }
            if domain.len() != argument_types.len() {
                continue;
            }
            if !argument_types
                .iter()
                .zip(domain)
                .all(|(argument_type, domain_type)| match argument_type {
                    Some(argument_type) => {
                        argument_type == &domain_type.0
                            || self.is_subtype(&InstanceID::new(argument_type), domain_type)
                    }
                    None => false,
                })
            {
                continue;
            }
            if let [_output_type] = signature.output_types.as_slice() {
                if signature.is_specialized {
                    specialized_candidates.push(signature);
                } else {
                    general_candidates.push(signature);
                }
            }
        }

        let mut candidates = if specialized_candidates.is_empty() {
            general_candidates
        } else {
            specialized_candidates
        };
        take_nonminimal_signatures(self, &mut candidates);
        candidates.sort_by(|left, right| {
            let left_key = (
                left.signature
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<Vec<_>>(),
                left.output_types
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<Vec<_>>(),
            );
            let right_key = (
                right
                    .signature
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<Vec<_>>(),
                right
                    .output_types
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<Vec<_>>(),
            );
            left_key.cmp(&right_key)
        });
        candidates.dedup_by(|left, right| {
            left.signature == right.signature && left.output_types == right.output_types
        });
        if let [signature] = candidates.as_slice() {
            Some(signature.clone())
        } else {
            None
        }
    }

    pub fn resolve_call_signature_usage(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<SignatureUsage> {
        let record = self.get_record(&InstanceID::new(callable))?;
        let mut possible = Vec::new();
        let mut excluded = Vec::new();

        for signature in self.all_installed_signatures(&record) {
            if signature.signature.first().map(|name| name.0.as_str()) != Some(callable) {
                continue;
            }
            let domain = signature.signature.get(1..).unwrap_or_default();
            if domain.len() != argument_types.len() {
                continue;
            }

            if domain_possibly_matches(self, domain, argument_types) {
                possible.push(signature);
            } else {
                excluded.push(signature);
            }
        }

        dedup_signatures(&mut possible);
        dedup_signatures(&mut excluded);
        excluded.extend(take_nonminimal_signatures(self, &mut possible));

        let all_arguments_known = argument_types.iter().all(Option::is_some);
        let pinned = if all_arguments_known && possible.len() == 1 {
            Some(possible.remove(0))
        } else {
            None
        };

        if pinned.is_none() && possible.is_empty() && excluded.is_empty() {
            None
        } else {
            Some(SignatureUsage {
                pinned,
                possible,
                excluded,
            })
        }
    }

    fn all_installed_signatures(&self, record: &Record) -> Vec<ResolvedSignature> {
        let Some(function_info) = &record.function_info else {
            return Vec::new();
        };

        let documented_by_domain: HashMap<_, _> = self
            .documented_signatures(record)
            .into_iter()
            .filter_map(|signature| {
                signature_domain_key(&signature.signature).map(|key| (key, signature))
            })
            .collect();

        let mut signatures = Vec::new();
        for method in &function_info.methods {
            let Some(domain_key) = signature_domain_key(&method.signature) else {
                continue;
            };
            if let Some(documented_signature) = documented_by_domain.get(&domain_key) {
                signatures.push(documented_signature.clone());
            } else {
                signatures.push(ResolvedSignature {
                    signature: method.signature.clone(),
                    output_types: Vec::new(),
                    is_specialized: false,
                    examples: Vec::new(),
                    doc_key: None,
                });
            }
        }

        signatures
    }
}

fn take_nonminimal_signatures(
    builtins: &BuiltinData,
    signatures: &mut Vec<ResolvedSignature>,
) -> Vec<ResolvedSignature> {
    let originals = signatures.clone();
    let mut dominated = Vec::new();
    signatures.retain(|candidate| {
        let is_dominated = originals
            .iter()
            .any(|other| signature_strictly_smaller(builtins, other, candidate));
        if is_dominated {
            dominated.push(candidate.clone());
        }
        !is_dominated
    });
    dominated
}

fn signature_strictly_smaller(
    builtins: &BuiltinData,
    smaller: &ResolvedSignature,
    bigger: &ResolvedSignature,
) -> bool {
    if smaller.signature == bigger.signature {
        return false;
    }

    let Some(smaller_domain) = smaller.signature.get(1..) else {
        return false;
    };
    let Some(bigger_domain) = bigger.signature.get(1..) else {
        return false;
    };
    if smaller_domain.len() != bigger_domain.len() {
        return false;
    }

    let mut strict = false;
    for (small, big) in smaller_domain.iter().zip(bigger_domain) {
        if small == big {
            continue;
        }
        if builtins.is_subtype(small, big) {
            strict = true;
            continue;
        }
        return false;
    }

    strict
}

fn domain_possibly_matches(
    builtins: &BuiltinData,
    domain: &[InstanceID],
    argument_types: &[Option<String>],
) -> bool {
    domain
        .iter()
        .zip(argument_types)
        .all(|(domain_type, argument_type)| {
            argument_type.as_ref().is_none_or(|argument_type| {
                argument_type == &domain_type.0
                    || builtins.is_subtype(&InstanceID::new(argument_type), domain_type)
            })
        })
}

fn dedup_signatures(signatures: &mut Vec<ResolvedSignature>) {
    let mut seen = HashSet::new();
    signatures.retain(|signature| {
        seen.insert((
            signature
                .signature
                .iter()
                .map(|id| id.0.clone())
                .collect::<Vec<_>>(),
            signature
                .output_types
                .iter()
                .map(|id| id.0.clone())
                .collect::<Vec<_>>(),
        ))
    });
}

fn signature_domain_key(signature: &[InstanceID]) -> Option<Vec<String>> {
    if signature.is_empty() {
        return None;
    }
    Some(
        signature
            .iter()
            .skip(1)
            .map(|item| item.0.clone())
            .collect(),
    )
}

impl Record {
    pub fn option_role(&self) -> Option<&'static str> {
        if self.has_description("option value")
            || self.has_description("value of an optional argument")
        {
            Some("value")
        } else if self.has_description("an optional argument") {
            Some("key")
        } else {
            None
        }
    }

    fn has_description(&self, needle: &str) -> bool {
        self.description_short
            .as_deref()
            .is_some_and(|description| description.contains(needle))
            || self
                .documentation
                .as_ref()
                .and_then(|documentation| documentation.upstream_description_short.as_deref())
                .is_some_and(|description| description.contains(needle))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
#[allow(dead_code)]
pub enum M2SemanticTokenType {
    Type = 0,
    Function = 1,
    Variable = 2,
    Parameter = 3,
    Property = 4,
    Namespace = 5,
    EnumMember = 6,
    Class = 7,
    Keyword = 8,
    String = 9,
    Number = 10,
    Operator = 11,
    Comment = 12,
    Method = 13,
    Regexp = 14,
    Modifier = 15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M2SemanticToken {
    pub token_type: M2SemanticTokenType,
    pub is_command: bool,
    pub is_file: bool,
    pub is_manipulator: bool,
    pub is_constructor: bool,
}

impl BuiltinData {
    pub fn load_from_split(names: &str, details: &str) -> Self {
        Self::load_from_split_with_type_facts(names, details, "")
    }

    pub fn load_from_split_with_type_facts(names: &str, details: &str, type_facts: &str) -> Self {
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

        let mut data = BuiltinData {
            names,
            name_to_index,
            details: details.to_string(),
            detail_ranges,
            type_facts: TypeFacts::load_jsonl(type_facts),
            type_lattice: TypeLattice::default(),
        };
        let lattice = TypeLattice::from_builtins(&data);
        data.type_lattice = lattice;
        data
    }

    pub fn get_semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        let record = self.get_record(&InstanceID::new(name))?;
        let data_type = &record.class;

        let function_type = InstanceID::new("Function");
        let command_type = InstanceID::new("Command");
        let file_type = InstanceID::new("File");
        let manipulator_type = InstanceID::new("Manipulator");
        let package_type = InstanceID::new("Package");
        let keyword_type = InstanceID::new("Keyword");
        let operator_type = InstanceID::new("Operator");
        let scripted_functor_type = InstanceID::new("ScriptedFunctor");
        let symbol_type = InstanceID::new("Symbol");
        let type_type = InstanceID::new("Type");
        let compiled_function_type = InstanceID::new("CompiledFunction");
        let compiled_function_closure_type = InstanceID::new("CompiledFunctionClosure");

        let is_command = self.is_subtype(data_type, &command_type);
        let is_file = self.is_subtype(data_type, &file_type);
        let is_manipulator = self.is_subtype(data_type, &manipulator_type);
        let is_scripted_functor = self.is_subtype(data_type, &scripted_functor_type);
        let is_compiled_function = self.is_subtype(data_type, &compiled_function_type)
            || self.is_subtype(data_type, &compiled_function_closure_type);
        let is_constructor =
            self.is_constructor_name(&record.name.0) && !is_manipulator && !is_command;

        // 1. M2 classes are runtime objects whose own class is a subtype of Type.
        // Other values that participate in the type hierarchy keep the generic
        // LSP type role.
        if let Some(type_info) = &record.type_info {
            if type_info.parent_type.is_some() {
                let token_type = match &record.relation_info {
                    Some(relation_info)
                        if relation_info
                            .class_ancestors
                            .iter()
                            .any(|ancestor| ancestor == &type_type) =>
                    {
                        M2SemanticTokenType::Class
                    }
                    _ => M2SemanticTokenType::Type,
                };

                return Some(M2SemanticToken {
                    token_type,
                    is_command: false,
                    is_file: false,
                    is_manipulator: false,
                    is_constructor: false,
                });
            }
        }

        // 2. Hierarchy traversal for other categories
        if self.is_subtype(data_type, &function_type)
            || is_scripted_functor
            || is_manipulator
            || is_command
        {
            let has_installed_methods = record
                .function_info
                .as_ref()
                .is_some_and(|info| !info.methods.is_empty());
            let token_type = if is_command {
                M2SemanticTokenType::Function
            } else if is_manipulator {
                M2SemanticTokenType::Operator
            } else if is_scripted_functor || is_compiled_function {
                M2SemanticTokenType::Function
            } else if has_installed_methods {
                M2SemanticTokenType::Method
            } else {
                M2SemanticTokenType::Function
            };

            Some(M2SemanticToken {
                token_type,
                is_command,
                is_file: false,
                is_manipulator,
                is_constructor,
            })
        } else if self.is_subtype(data_type, &package_type) {
            Some(M2SemanticToken {
                token_type: M2SemanticTokenType::Namespace,
                is_command: false,
                is_file: false,
                is_manipulator: false,
                is_constructor: false,
            })
        } else if (self.is_subtype(data_type, &symbol_type) || is_file)
            && !self.is_subtype(data_type, &keyword_type)
            && !self.is_subtype(data_type, &operator_type)
        {
            let token_type = if is_file {
                M2SemanticTokenType::Variable
            } else {
                M2SemanticTokenType::EnumMember
            };

            Some(M2SemanticToken {
                token_type,
                is_command: false,
                is_file,
                is_manipulator: false,
                is_constructor: false,
            })
        } else {
            None
        }
    }

    pub fn get_semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        let type_id = InstanceID::new(type_name);
        let function_type = InstanceID::new("Function");
        let command_type = InstanceID::new("Command");
        let file_type = InstanceID::new("File");
        let manipulator_type = InstanceID::new("Manipulator");
        let package_type = InstanceID::new("Package");
        let scripted_functor_type = InstanceID::new("ScriptedFunctor");
        let symbol_type = InstanceID::new("Symbol");
        let type_type = InstanceID::new("Type");
        let compiled_function_type = InstanceID::new("CompiledFunction");
        let compiled_function_closure_type = InstanceID::new("CompiledFunctionClosure");

        let is_command = self.is_subtype(&type_id, &command_type);
        let is_file = self.is_subtype(&type_id, &file_type);
        let is_manipulator = self.is_subtype(&type_id, &manipulator_type);
        let is_compiled_function = self.is_subtype(&type_id, &compiled_function_type)
            || self.is_subtype(&type_id, &compiled_function_closure_type);
        let is_type_valued = self.is_subtype(&type_id, &type_type);

        let token_type = if type_name.starts_with("MethodFunction") {
            M2SemanticTokenType::Method
        } else if self.is_subtype(&type_id, &package_type) {
            M2SemanticTokenType::Namespace
        } else if is_type_valued {
            M2SemanticTokenType::Class
        } else if self.is_subtype(&type_id, &function_type)
            || self.is_subtype(&type_id, &scripted_functor_type)
            || is_manipulator
            || is_command
        {
            if is_command {
                M2SemanticTokenType::Function
            } else if is_manipulator {
                M2SemanticTokenType::Operator
            } else {
                M2SemanticTokenType::Function
            }
        } else if is_file {
            M2SemanticTokenType::Variable
        } else if self.is_subtype(&type_id, &symbol_type) {
            M2SemanticTokenType::EnumMember
        } else {
            return None;
        };

        Some(M2SemanticToken {
            token_type,
            is_command,
            is_file,
            is_manipulator,
            is_constructor: false,
        })
    }

    pub fn is_constructor_name(&self, name: &str) -> bool {
        let unqualified_name = name.rsplit_once('$').map_or(name, |(_, name)| name);
        let Some(target_name) = unqualified_name.strip_prefix("to") else {
            return false;
        };
        if target_name.is_empty() {
            return false;
        }

        self.get_record(&InstanceID::new(target_name))
            .is_some_and(|record| self.is_type_like_record(&record))
    }

    fn is_type_like_record(&self, record: &Record) -> bool {
        let type_type = InstanceID::new("Type");
        if record.relation_info.as_ref().is_some_and(|relation_info| {
            relation_info
                .class_ancestors
                .iter()
                .any(|ancestor| ancestor == &type_type)
        }) {
            return true;
        }

        record
            .type_info
            .as_ref()
            .is_some_and(|type_info| type_info.parent_type.is_some())
    }

    /// Check if child is a subtype of parent (inclusive), using the precomputed lattice.
    pub fn is_subtype(&self, child: &InstanceID, parent: &InstanceID) -> bool {
        self.type_lattice.is_subtype(child, parent)
    }

    pub fn least_upper_bound(&self, a: &InstanceID, b: &InstanceID) -> Option<InstanceID> {
        self.type_lattice.least_upper_bound(a, b)
    }

    pub fn greatest_lower_bound(&self, a: &InstanceID, b: &InstanceID) -> Option<InstanceID> {
        self.type_lattice.greatest_lower_bound(a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_index::BuiltinIndex;

    #[test]
    fn dispatch_walks_the_lattice_to_the_installed_supertype_method() {
        // dim is installed on Ring; PolynomialRing <: Ring.
        let jsonl = concat!(
            r#"{"kind":"type","name":"Thing","aliases":[]}"#,
            "\n",
            r#"{"kind":"type","name":"Ring","parent":"Thing","ancestors":["Thing"],"subtypes":["PolynomialRing"],"aliases":[]}"#,
            "\n",
            r#"{"kind":"type","name":"PolynomialRing","parent":"Ring","ancestors":["Ring","Thing"],"subtypes":[],"aliases":[]}"#,
            "\n",
            r#"{"kind":"function","name":"dim","methods":[{"domain":["Ring"],"typicalValue":"ZZ"}],"aliases":[]}"#,
            "\n",
        );
        let index = BuiltinIndex::load(jsonl);
        let lattice = TypeLattice::from_type_index(&index);
        let facts = TypeFacts::from_type_index(&index);

        // Exact match.
        assert_eq!(facts.resolve_codomain(&lattice, "dim", &["Ring"]), Some("ZZ"));
        // Subtype dispatch: PolynomialRing has no own method, walks up to Ring's.
        assert_eq!(
            facts.resolve_codomain(&lattice, "dim", &["PolynomialRing"]),
            Some("ZZ")
        );
        // No applicable method (Thing is a supertype, not a subtype) ⇒ silent.
        assert_eq!(facts.resolve_codomain(&lattice, "dim", &["Thing"]), None);
        // Two-sided lattice exposes the downward edge for type_hierarchy.
        assert_eq!(
            lattice.children(&InstanceID::new("Ring")),
            &[InstanceID::new("PolynomialRing")]
        );
    }

    #[test]
    fn resolves_real_gb_codomain_from_the_type_index() {
        let index = BuiltinIndex::load(include_str!("./data/m2-types.jsonl"));
        let lattice = TypeLattice::from_type_index(&index);
        let facts = TypeFacts::from_type_index(&index);
        assert_eq!(
            facts.resolve_codomain(&lattice, "gb", &["Ideal"]),
            Some("GroebnerBasis")
        );
    }

    fn generated_builtins() -> BuiltinData {
        BuiltinData::load_from_split_with_type_facts(
            include_str!("./data/builtins.names"),
            include_str!("./data/builtins.details.jsonl"),
            include_str!("./data/type_facts.jsonl"),
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

        let ring_function = builtins
            .get_record(&InstanceID::new("ring"))
            .expect("ring should be present");
        assert!(
            ring_function
                .function_info
                .as_ref()
                .and_then(|info| info.general_signature.as_ref())
                .is_some_and(|signature| signature.output_types == vec![InstanceID::new("Ring")]),
            "ring should carry the general documentation codomain"
        );
        assert!(
            builtins
                .documented_signatures(&ring_function)
                .iter()
                .any(|method| {
                    method.signature == vec![InstanceID::new("ring"), InstanceID::new("Ideal")]
                        && method.output_types == vec![InstanceID::new("Ring")]
                }),
            "ring(Ideal) should be a documented signature with known Ring codomain"
        );
        assert!(
            builtins
                .documented_signatures(&ring_function)
                .iter()
                .any(|method| {
                    method.signature
                        == vec![InstanceID::new("ring"), InstanceID::new("ChainComplex")]
                        && method.output_types == vec![InstanceID::new("Ring")]
                        && !method.is_specialized
                }),
            "installed ring domains should inherit the general Ring codomain"
        );
        assert!(
            !builtins
                .undocumented_installed_methods(&ring_function)
                .iter()
                .any(|method| method.signature
                    == vec![InstanceID::new("ring"), InstanceID::new("Ideal")]),
            "documented ring(Ideal) should not be repeated as an installed-only method"
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
            Some(M2SemanticTokenType::Method),
            "M2-defined MethodFunctionSingle values keep the method role"
        );
        assert_eq!(
            builtins
                .get_semantic_token("drop")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "CompiledFunction values use the builtin function role"
        );
        assert_eq!(
            builtins
                .get_semantic_token("apply")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "CompiledFunction values with installed methods still use the builtin function role"
        );
        assert_eq!(
            builtins
                .get_semantic_token("wedgeProduct")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "MethodFunction is a child of CompiledFunctionClosure, so it uses the builtin function role"
        );
        assert_eq!(
            builtins
                .get_semantic_token("match")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Method)
        );
        assert_eq!(
            builtins
                .get_semantic_token("toString")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Method)
        );
        assert!(
            builtins
                .get_semantic_token("toString")
                .is_some_and(|token| token.is_constructor),
            "constructor-ness should be carried as a custom modifier, not a custom token type"
        );
        assert_eq!(
            builtins
                .get_semantic_token("toList")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Method)
        );
        assert!(
            builtins
                .get_semantic_token("toList")
                .is_some_and(|token| token.is_constructor),
            "constructor-ness should be carried as a custom modifier, not a custom token type"
        );
        assert!(
            !builtins.is_constructor_name("toLower"),
            "to-prefixed names only count as constructors when the suffix is an M2 type"
        );
        assert_eq!(
            builtins
                .get_semantic_token("Ring")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Class)
        );
        assert_eq!(
            builtins
                .get_semantic_token("ZZ")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Class)
        );
        assert_eq!(
            builtins
                .get_semantic_token("MutableHashTable")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Class)
        );
        assert_eq!(
            builtins
                .get_semantic_token("Function")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Class)
        );
        assert_eq!(
            builtins
                .get_semantic_token("ScriptedFunctor")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Class)
        );
        assert_eq!(
            builtins
                .get_semantic_token("Tor")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "ScriptedFunctor values should use the function token role"
        );
        assert_eq!(
            builtins
                .get_semantic_token("Core")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Namespace),
            "M2 Package values should use the namespace role"
        );
        assert_eq!(
            builtins
                .get_semantic_token("Strategy")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::EnumMember),
            "plain metadata symbols should use enumMember unless context gives a more specific role"
        );
        assert_eq!(
            builtins
                .get_semantic_token("LongPolynomial")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::EnumMember),
            "plain option-value metadata symbols should use enumMember"
        );
        assert!(
            builtins.is_option_name("SyzygyLimit"),
            "option keys should be identifiable from generated metadata"
        );
        let syzygy_limit_usages = builtins.option_usage_names("SyzygyLimit", 8);
        assert!(
            syzygy_limit_usages.contains(&"gb".to_string())
                && syzygy_limit_usages.contains(&"syz".to_string()),
            "generated method option metadata should support option hover usage lists"
        );
        assert!(
            builtins.is_option_value_name("Bayer"),
            "generic option values should be identifiable from generated metadata"
        );
        assert!(
            builtins.is_option_value_name("LongPolynomial"),
            "package-specific option values should be identifiable from generated metadata"
        );
        assert!(
            builtins.is_option_value_for_key("Strategy", "LongPolynomial"),
            "slot-specific option values should be identifiable from compact type facts"
        );
        assert!(
            !builtins.is_option_value_for_key("SyzygyLimit", "LongPolynomial"),
            "option values should not be treated as valid for unrelated option keys"
        );
        assert_eq!(
            builtins
                .get_semantic_token("endl")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Operator),
            "M2 Manipulator values should use the operator token role"
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
            "M2 Command values should use the function token role"
        );
        assert!(
            builtins
                .get_semantic_token("clearAll")
                .is_some_and(|token| token.is_command),
            "M2 Command values should retain their runtime-class modifier"
        );
        assert_eq!(
            builtins
                .get_semantic_token_for_static_type("Command")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "Command static types should use the function token role"
        );
        assert_eq!(
            builtins
                .get_semantic_token_for_static_type("ScriptedFunctor")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Function),
            "ScriptedFunctor static types should use the function token role"
        );
        assert_eq!(
            builtins
                .get_semantic_token_for_static_type("Manipulator")
                .map(|token| token.token_type),
            Some(M2SemanticTokenType::Operator),
            "Manipulator static types should use the operator token role"
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
