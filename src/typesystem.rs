//! Indexed Macaulay2 builtin metadata, static type relations, and the compact
//! queries shared by LSP capabilities.
//!
//! This module does not evaluate Macaulay2. It combines generated corpus facts
//! with the type lattice to answer conservative questions about known objects,
//! method signatures, option values, hover text, and semantic-token roles.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display};

use crate::builtin_index::{register_entry_keys, IndexedEntry, OperatorForm};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable identifier for an indexed M2 object or type.
pub struct InstanceID(pub String);

impl InstanceID {
    /// Construct an identifier from an unqualified or package-qualified name.
    pub fn new(name: &str) -> Self {
        InstanceID(name.to_string())
    }
}

impl Display for InstanceID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for InstanceID {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for InstanceID {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One executable example attached to a corpus record or method signature.
pub struct CodeExample(pub String);

#[derive(Debug, Clone)]
/// The normalized metadata known for one builtin object, type, or callable.
pub struct Record {
    pub name: InstanceID,
    pub class: InstanceID,
    pub description_short: Option<String>,
    pub examples: Vec<CodeExample>,
    pub package: Option<String>,
    pub source_file: Option<String>,
    pub typical_value: Option<String>,
    pub function_info: Option<FunctionInfo>,
    pub option_info: Option<OptionInfo>,
    pub operator_info: Option<OperatorInfo>,
    pub type_info: Option<TypeInfo>,
    /// Whether a `Symbol`-class object is `protect`ed. `None` ⇒ the corpus did
    /// not record it; the classifier then falls back to the class-is-`Symbol`
    /// proxy (see [`BuiltinData::is_protected_symbol`]).
    pub protected: Option<bool>,
}

#[derive(Debug, Clone)]
/// Callable metadata, separating installed methods from richer documentation.
pub struct FunctionInfo {
    pub methods: Vec<MethodSignature>,
    pub documented_methods: Vec<DocumentedMethodSignature>,
    pub general_signature: Option<DocumentedMethodSignature>,
}

#[derive(Debug, Clone)]
/// The documented options accepted by a callable.
pub struct OptionInfo {
    pub options: Vec<MethodOption>,
}

#[derive(Debug, Clone)]
/// One option key and, when available, its textual default value.
pub struct MethodOption {
    pub name: InstanceID,
}

#[derive(Debug, Clone)]
/// An installed method domain: callable name followed by its argument types.
pub struct MethodSignature {
    pub signature: Vec<InstanceID>,
}

#[derive(Debug, Clone)]
/// A method signature enriched with codomain, examples, and documentation key.
pub struct DocumentedMethodSignature {
    pub signature: Vec<InstanceID>,
    pub output_types: Vec<InstanceID>,
    pub examples: Vec<CodeExample>,
    pub doc_key: Option<InstanceID>,
}

#[derive(Debug, Clone)]
/// Parser and runtime metadata for an operator-backed callable.
pub struct OperatorInfo {
    pub method_symbol: InstanceID,
    pub forms: Vec<String>,
    /// Per-form operator attributes from the corpus (`binary` → `["Flexible"]`,
    /// …). Flexibility is per-form: an operator can be flexible as a prefix yet
    /// not as a binary, so it is queried via [`OperatorInfo::is_flexible`].
    pub attributes: HashMap<OperatorForm, Vec<String>>,
}

/// The operator attribute marking a form as accepting runtime method
/// installation (`X op Y := …`).
const FLEXIBLE_ATTRIBUTE: &str = "Flexible";

impl OperatorInfo {
    /// Whether this operator accepts a method installed on the given form
    /// (`"binary"`/`"prefix"`/`"postfix"`) — i.e. that form is `Flexible`.
    pub fn is_flexible(&self, form: &str) -> bool {
        self.attributes
            .get(form)
            .is_some_and(|attributes| attributes.iter().any(|a| a == FLEXIBLE_ATTRIBUTE))
    }
}

#[derive(Debug, Clone)]
/// Direct hierarchy and instance facts for an indexed M2 type.
pub struct TypeInfo {
    pub subtypes: Vec<InstanceID>,
    pub parent_type: Option<InstanceID>,
}

/// Two-sided type hierarchy: `ancestors` (sorted, for upward `is_subtype`/lub/glb
/// queries) and `children` (immediate subtypes, for the downward `type_hierarchy`
/// view). Instance checks only ever walk upward; `children` is read straight, not
/// recomputed.
#[derive(Debug, Clone, Default)]
pub struct TypeLattice {
    ancestors: HashMap<InstanceID, Vec<InstanceID>>,
}

impl TypeLattice {
    /// Build the lattice from the `m2-index.jsonl` type records: each carries its
    /// full ancestor chain, sorted here for binary search.
    pub fn from_type_index(index: &crate::builtin_index::BuiltinIndex) -> Self {
        let mut ancestors: HashMap<InstanceID, Vec<InstanceID>> = HashMap::new();

        for entry in index.types() {
            let id = InstanceID::new(&entry.name);
            // The corpus `ancestors` field is the is-a chain ABOVE the immediate
            // parent (verified against M2: `ancestors Array` = {Array, VisibleList,
            // BasicList, Thing}, while the field carries only {BasicList, Thing}).
            // Fold the immediate `parent` in so `is_subtype(child, parent)` holds
            // for the direct edge — otherwise it only ever succeeds reflexively.
            let mut chain: Vec<InstanceID> =
                entry.ancestors.iter().map(|a| InstanceID::new(a)).collect();
            if let Some(parent) = &entry.parent {
                chain.push(InstanceID::new(parent));
            }
            chain.sort();
            chain.dedup();
            ancestors.insert(id, chain);
        }

        TypeLattice { ancestors }
    }

    pub fn is_subtype(&self, child: &str, parent: &str) -> bool {
        child == parent
            || self.ancestors.get(child).is_some_and(|chain| {
                chain
                    .binary_search_by(|candidate| candidate.as_ref().cmp(parent))
                    .is_ok()
            })
    }
}

#[derive(Debug, Clone)]
/// In-memory view of the generated builtin corpus used by one LSP workspace.
pub struct BuiltinData {
    /// Primary names, one per pooled record (1:1 with `records`).
    names: Vec<InstanceID>,
    /// Name *and* every alias → record index. More entries than `names`.
    name_to_index: HashMap<InstanceID, usize>,
    /// Records held in memory; `get_record` clones from here rather than
    /// re-deserializing a packed string per lookup.
    records: Vec<Record>,
    /// Pre-rendered hover markdown keyed by name + aliases. Read once at load
    /// (folded into each `m2-index.jsonl` record for builtins and lifted into
    /// this map by `from_index`); hover reads it from here. Empty for runtime
    /// package indexes.
    docs: HashMap<InstanceID, String>,
    type_facts: TypeFacts,
    type_lattice: TypeLattice,
}

/// The semantic facts analysis needs from an external symbol/type index.
///
/// Keeping this as a narrow trait decouples the analysis engine from the
/// concrete generated-corpus store. The normal server implements it with
/// [`BuiltinData`]; syntax-only tests use an empty implementation. A future
/// workspace/package view can implement the same contract without teaching
/// analysis about its storage or resolution order.
pub(crate) trait TypeKnowledge {
    fn is_available(&self) -> bool;

    fn get_record(&self, name: &InstanceID) -> Option<Record>;

    fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        options: &[(String, String)],
    ) -> Option<String>;

    fn is_subtype(&self, child: &str, parent: &str) -> bool;
}

/// Indexed facts needed specifically for semantic-token classification.
///
/// This is separate from [`TypeKnowledge`] because syntax/type analysis should
/// not depend on editor presentation roles. Both the concrete corpus and a
/// document-scoped package view implement it, so semantic highlighting follows
/// the same import resolution order as the other language features.
pub(crate) trait SemanticTokenKnowledge: TypeKnowledge {
    fn semantic_token(&self, name: &str) -> Option<M2SemanticToken>;

    fn semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken>;

    fn is_protected_symbol(&self, name: &str) -> bool;

    fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool;
}

/// Supplies the semantic index for one document's imported-package set.
///
/// The associated type is a borrowing view: providers may return themselves,
/// or construct a lightweight scoped view without merging/copying indexes.
pub(crate) trait TypeKnowledgeProvider {
    type Knowledge<'a>: TypeKnowledge
    where
        Self: 'a;

    fn knowledge_for<'a>(&'a self, imported_packages: &[String]) -> Self::Knowledge<'a>;
}

/// Empty semantic knowledge for parser/scope-only analysis.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoTypeKnowledge;

#[derive(Debug, Clone, PartialEq, Eq)]
/// One callable signature after documentation and indexed type facts are merged.
pub struct ResolvedSignature {
    pub signature: Vec<InstanceID>,
    pub output_types: Vec<InstanceID>,
    pub is_specialized: bool,
    pub examples: Vec<CodeExample>,
    pub doc_key: Option<InstanceID>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
/// Installed signatures partitioned by their applicability at one call site.
pub struct SignatureUsage {
    pub pinned: Option<ResolvedSignature>,
    pub possible: Vec<ResolvedSignature>,
    pub excluded: Vec<ResolvedSignature>,
}

#[derive(Debug, Clone, Default)]
/// Compact option-value facts derived from the type index.
pub struct TypeFacts {
    signature_codomains: HashMap<SignatureKey, InstanceID>,
    option_value_usages: HashMap<InstanceID, Vec<OptionValueUsage>>,
    option_values_by_slot: HashMap<OptionSlot, Vec<InstanceID>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SignatureKey {
    callable: InstanceID,
    domain: Vec<InstanceID>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OptionSlot {
    callable: InstanceID,
    option: InstanceID,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A callable/option slot that admits a particular indexed option value.
pub struct OptionValueUsage {
    pub callable: InstanceID,
    pub option: InstanceID,
}

impl TypeFacts {
    /// Build the typecheck facts from the `m2-index.jsonl` callable records.
    pub fn from_type_index(index: &crate::builtin_index::BuiltinIndex) -> Self {
        let mut facts = TypeFacts::default();
        for callable in index.callables() {
            let callable_id = InstanceID::new(&callable.name);
            for signature in &callable.signatures {
                let Some(codomain) = signature.codomain.as_ref() else {
                    continue;
                };
                facts.signature_codomains.insert(
                    SignatureKey {
                        callable: callable_id.clone(),
                        domain: signature
                            .domain
                            .iter()
                            .map(|name| InstanceID::new(name))
                            .collect(),
                    },
                    InstanceID::new(codomain),
                );
            }
            for option in &callable.options {
                let option_id = InstanceID::new(&option.key);
                let slot = OptionSlot {
                    callable: callable_id.clone(),
                    option: option_id.clone(),
                };
                for value in &option.possible_values {
                    let value_id = InstanceID::new(value);
                    facts
                        .option_values_by_slot
                        .entry(slot.clone())
                        .or_default()
                        .push(value_id.clone());
                    facts
                        .option_value_usages
                        .entry(value_id)
                        .or_default()
                        .push(OptionValueUsage {
                            callable: callable_id.clone(),
                            option: option_id.clone(),
                        });
                }
            }
        }
        for usages in facts.option_value_usages.values_mut() {
            usages.sort_by(|left, right| {
                (left.callable.0.as_str(), left.option.0.as_str())
                    .cmp(&(right.callable.0.as_str(), right.option.0.as_str()))
            });
            usages.dedup();
        }
        for values in facts.option_values_by_slot.values_mut() {
            values.sort();
            values.dedup();
        }
        facts
    }

    fn signature_codomain(&self, signature: &[InstanceID]) -> Option<&str> {
        self.signature_codomains
            .get(&SignatureKey {
                callable: signature.first()?.clone(),
                domain: signature[1..].to_vec(),
            })
            .map(|codomain| codomain.0.as_str())
    }
}

impl BuiltinData {
    /// Number of primary records; aliases do not increase this count.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Primary names beginning with `prefix`, in corpus order and capped at `limit`.
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

    /// Primary names containing `query` case-insensitively, capped at `limit`.
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

    /// Clone the record named by `name`, resolving aliases through the same map
    /// used for all builtin lookups.
    pub fn get_record(&self, name: &InstanceID) -> Option<Record> {
        let index = *self.name_to_index.get(name)?;
        self.records.get(index).cloned()
    }

    /// The pre-rendered hover markdown for `name` (or one of its aliases), if the
    /// docs asset carried an entry. Typecheck records hold no doc text. Aliases
    /// resolve to the record's primary name, under which the docs are keyed.
    pub fn doc_markdown(&self, name: &InstanceID) -> Option<&str> {
        let index = *self.name_to_index.get(name)?;
        let primary = self.names.get(index)?;
        self.docs.get(primary).map(String::as_str)
    }

    /// Return callables which document `option_name`, with package qualifiers
    /// stripped for matching and display.
    pub fn option_usage_names(&self, option_name: &str, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }

        let option_name = option_name
            .rsplit_once('$')
            .map_or(option_name, |(_, name)| name);
        let mut usages = Vec::new();
        for (index, name) in self.names.iter().enumerate() {
            let Some(record) = self.records.get(index) else {
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

    /// Return `callable.option` slots that admit `value_name` in indexed facts.
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

    // Superseded for semantic-token key classification by `is_protected_symbol`
    // (the user's model is protected-based, not option-role-based); kept for
    // option-key completion/diagnostics, which still need documented-key-ness.
    /// Whether `name` resolves to a protected object whose class is *exactly*
    /// `Symbol` (not merely an instance of it) — M2's nominal enum members. The
    /// `protected` flag is authoritative when the corpus records it; when it does
    /// not (`None`), default to `true`, since every builtin class-`Symbol` object
    /// is in fact protected, keeping the absent-data case at the prior behaviour.
    pub fn is_protected_symbol(&self, name: &str) -> bool {
        let symbol_type = InstanceID::new("Symbol");
        self.get_record(&InstanceID::new(name))
            .is_some_and(|record| record.class == symbol_type && record.protected.unwrap_or(true))
    }

    /// Whether the indexed facts admit `value_name` for any spelling of
    /// `option_key`, ignoring package qualification.
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
            .filter(|(slot, _)| slot.option.0 == option_key)
            .any(|(_, values)| values.iter().any(|value| value.0 == value_name))
    }

    /// Merge installed methods with documentation and static codomain facts.
    /// Specialized domains win over a general documented signature.
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

    /// Installed method domains not represented by a resolved documented signature.
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

    /// Resolve a call to exactly one known return type, or stay silent when the
    /// available metadata is ambiguous or incomplete. Literal option facts are
    /// accepted for the option-sensitive dispatch path.
    pub fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        _literal_options: &[(String, String)],
    ) -> Option<String> {
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

    /// Resolve a call only when its known argument types select one signature.
    /// A specialized candidate is preferred to a general one.
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
                            || self.is_subtype(argument_type, domain_type)
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

    /// Partition installed signatures into possible and excluded candidates;
    /// pin one only when every argument type is known and unambiguous.
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

impl TypeKnowledge for BuiltinData {
    fn is_available(&self) -> bool {
        true
    }

    fn get_record(&self, name: &InstanceID) -> Option<Record> {
        BuiltinData::get_record(self, name)
    }

    fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        options: &[(String, String)],
    ) -> Option<String> {
        BuiltinData::resolve_call_return_type_with_options(self, callable, argument_types, options)
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        BuiltinData::is_subtype(self, child, parent)
    }
}

impl<T: TypeKnowledge + ?Sized> TypeKnowledge for &T {
    fn is_available(&self) -> bool {
        T::is_available(self)
    }

    fn get_record(&self, name: &InstanceID) -> Option<Record> {
        T::get_record(self, name)
    }

    fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        options: &[(String, String)],
    ) -> Option<String> {
        T::resolve_call_return_type_with_options(self, callable, argument_types, options)
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        T::is_subtype(self, child, parent)
    }
}

impl SemanticTokenKnowledge for BuiltinData {
    fn semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        BuiltinData::get_semantic_token(self, name)
    }

    fn semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        BuiltinData::get_semantic_token_for_static_type(self, type_name)
    }

    fn is_protected_symbol(&self, name: &str) -> bool {
        BuiltinData::is_protected_symbol(self, name)
    }

    fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool {
        BuiltinData::is_option_value_for_key(self, option_key, value_name)
    }
}

impl<T: SemanticTokenKnowledge + ?Sized> SemanticTokenKnowledge for &T {
    fn semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        T::semantic_token(self, name)
    }

    fn semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        T::semantic_token_for_static_type(self, type_name)
    }

    fn is_protected_symbol(&self, name: &str) -> bool {
        T::is_protected_symbol(self, name)
    }

    fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool {
        T::is_option_value_for_key(self, option_key, value_name)
    }
}

impl TypeKnowledgeProvider for BuiltinData {
    type Knowledge<'a> = &'a BuiltinData;

    fn knowledge_for<'a>(&'a self, _imported_packages: &[String]) -> Self::Knowledge<'a> {
        self
    }
}

#[cfg(test)]
impl TypeKnowledge for NoTypeKnowledge {
    fn is_available(&self) -> bool {
        false
    }

    fn get_record(&self, _name: &InstanceID) -> Option<Record> {
        None
    }

    fn resolve_call_return_type_with_options(
        &self,
        _callable: &str,
        _argument_types: &[Option<String>],
        _options: &[(String, String)],
    ) -> Option<String> {
        None
    }

    fn is_subtype(&self, _child: &str, _parent: &str) -> bool {
        false
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
                argument_type == &domain_type.0 || builtins.is_subtype(argument_type, domain_type)
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
    /// A record with only its identity known — used when a `details` line fails
    /// to parse, or as the base for records synthesized from the typecheck index.
    fn unknown(name: InstanceID) -> Self {
        Record {
            name,
            class: InstanceID::new("Thing"),
            description_short: None,
            examples: Vec::new(),
            package: None,
            source_file: None,
            typical_value: None,
            function_info: None,
            option_info: None,
            operator_info: None,
            type_info: None,
            protected: None,
        }
    }

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
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The LSP-standard token types emitted for M2 syntax and indexed metadata.
#[repr(u32)]
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
/// A semantic-token role plus M2-specific modifier facts for one identifier.
pub struct M2SemanticToken {
    pub token_type: M2SemanticTokenType,
    pub is_command: bool,
    pub is_file: bool,
    pub is_manipulator: bool,
    pub is_constructor: bool,
}

/// Conversion contract from one typed index entry into the common feature
/// record. Shared identity/class/package handling lives here; each entry kind
/// contributes only the facts it actually owns.
trait RecordEntry: IndexedEntry {
    fn default_class(&self) -> &'static str;

    fn add_facts(&self, _record: &mut Record) {}

    fn to_record(&self) -> Record {
        let mut record = Record::unknown(InstanceID::new(self.name()));
        record.class = InstanceID::new(self.class().unwrap_or(self.default_class()));
        record.package = self.package().map(ToString::to_string);
        self.add_facts(&mut record);
        record
    }
}

impl RecordEntry for crate::builtin_index::TypeEntry {
    fn default_class(&self) -> &'static str {
        "Type"
    }

    fn add_facts(&self, record: &mut Record) {
        record.type_info = Some(TypeInfo {
            subtypes: self
                .subtypes
                .iter()
                .map(|subtype| InstanceID::new(subtype))
                .collect(),
            parent_type: self.parent.as_deref().map(InstanceID::new),
        });
    }
}

impl RecordEntry for crate::builtin_index::CallableEntry {
    fn default_class(&self) -> &'static str {
        if self.is_operator {
            "Keyword"
        } else {
            "Function"
        }
    }

    fn add_facts(&self, record: &mut Record) {
        let methods = self
            .signatures
            .iter()
            .map(|signature| {
                let mut method = Vec::with_capacity(signature.domain.len() + 1);
                method.push(InstanceID::new(self.name()));
                method.extend(signature.domain.iter().map(|part| InstanceID::new(part)));
                MethodSignature { signature: method }
            })
            .collect();

        let general_signature =
            self.typical_value
                .as_ref()
                .map(|typical_value| DocumentedMethodSignature {
                    signature: vec![InstanceID::new(self.name())],
                    output_types: vec![InstanceID::new(typical_value)],
                    examples: Vec::new(),
                    doc_key: None,
                });

        record.function_info = Some(FunctionInfo {
            methods,
            documented_methods: Vec::new(),
            general_signature,
        });

        if !self.options.is_empty() {
            record.option_info = Some(OptionInfo {
                options: self
                    .options
                    .iter()
                    .map(|option| MethodOption {
                        name: InstanceID::new(&option.key),
                    })
                    .collect(),
            });
        }

        if self.is_operator {
            record.operator_info = Some(OperatorInfo {
                method_symbol: InstanceID::new(self.name()),
                forms: self.forms.clone(),
                attributes: self.operator_attributes.clone(),
            });
        }

        record.typical_value.clone_from(&self.typical_value);
    }
}

impl RecordEntry for crate::builtin_index::ObjectEntry {
    fn default_class(&self) -> &'static str {
        "Thing"
    }

    fn add_facts(&self, record: &mut Record) {
        record.protected = self.protected;
    }
}

#[derive(Default)]
struct BuiltinDataBuilder {
    names: Vec<InstanceID>,
    name_to_index: HashMap<InstanceID, usize>,
    records: Vec<Record>,
    docs: HashMap<InstanceID, String>,
}

impl BuiltinDataBuilder {
    fn append<T: RecordEntry>(&mut self, entries: &[T]) {
        for entry in entries {
            let id = self.records.len();
            register_entry_keys(&mut self.name_to_index, entry, id, InstanceID::new);
            let name = InstanceID::new(entry.name());
            self.names.push(name.clone());
            if let Some(markdown) = entry.markdown() {
                self.docs
                    .entry(name)
                    .or_insert_with(|| markdown.to_string());
            }
            self.records.push(entry.to_record());
        }
    }

    fn finish(self, type_facts: TypeFacts, type_lattice: TypeLattice) -> BuiltinData {
        BuiltinData {
            names: self.names,
            name_to_index: self.name_to_index,
            records: self.records,
            docs: self.docs,
            type_facts,
            type_lattice,
        }
    }
}

impl BuiltinData {
    /// Build a `BuiltinData` from an already-parsed `BuiltinIndex`. Hover
    /// markdown is folded into each entry by the corpus generator, so the docs
    /// map is built here from the entries themselves — no separate docs asset.
    pub fn from_index(index: &crate::builtin_index::BuiltinIndex) -> Self {
        let type_lattice = TypeLattice::from_type_index(index);
        let type_facts = TypeFacts::from_type_index(index);

        let mut records = BuiltinDataBuilder::default();
        records.append(index.types());
        records.append(index.callables());
        records.append(index.objects());
        records.finish(type_facts, type_lattice)
    }

    /// Build a `BuiltinData` over the whole combined corpus (`m2-index.jsonl`).
    /// Production routes through `PackagePartitionedIndex::from_corpus` and uses
    /// only the Core partition; this whole-corpus convenience is for tests.
    #[cfg(test)]
    pub fn load_from_index(corpus: &str) -> Self {
        Self::from_index(&crate::builtin_index::BuiltinIndex::load(corpus))
    }

    /// An empty index — no records, no facts, empty lattice. For tests and
    /// snapshots that need a `BuiltinData` placeholder with no builtin knowledge.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self::from_index(&crate::builtin_index::BuiltinIndex::default())
    }

    /// Classify an indexed object by its runtime class and hierarchy for LSP
    /// semantic tokens.
    pub fn get_semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        semantic_token_from_knowledge(self, name)
    }

    /// Classify a known static type when recoloring a local symbol by inference.
    pub fn get_semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        semantic_token_for_static_type_from_knowledge(self, type_name)
    }

    /// Check if child is a subtype of parent (inclusive), using the precomputed lattice.
    pub fn is_subtype(&self, child: impl AsRef<str>, parent: impl AsRef<str>) -> bool {
        self.type_lattice
            .is_subtype(child.as_ref(), parent.as_ref())
    }
}

pub(crate) fn semantic_token_from_knowledge(
    knowledge: &(impl TypeKnowledge + ?Sized),
    name: &str,
) -> Option<M2SemanticToken> {
    let record = knowledge.get_record(&InstanceID::new(name))?;
    let data_type = &record.class;

    let is_command = knowledge.is_subtype(data_type.as_ref(), "Command");
    let is_file = knowledge.is_subtype(data_type.as_ref(), "File");
    let is_manipulator = knowledge.is_subtype(data_type.as_ref(), "Manipulator");
    let is_scripted_functor = knowledge.is_subtype(data_type.as_ref(), "ScriptedFunctor");
    let is_compiled_function = knowledge.is_subtype(data_type.as_ref(), "CompiledFunction")
        || knowledge.is_subtype(data_type.as_ref(), "CompiledFunctionClosure");
    let is_constructor =
        indexed_name_is_constructor(knowledge, &record.name.0) && !is_manipulator && !is_command;

    // An indexed type whose own class is `Type` is an M2 class (for example
    // `Array`). Other type-valued objects, such as `ZZ` whose class is `Ring`,
    // keep the standard `type` role.
    if record_is_type_like(&record) {
        return Some(M2SemanticToken {
            token_type: if data_type.as_ref() == "Type" {
                M2SemanticTokenType::Class
            } else {
                M2SemanticTokenType::Type
            },
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
        });
    }

    if knowledge.is_subtype(data_type.as_ref(), "Function")
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
    } else if knowledge.is_subtype(data_type.as_ref(), "Package") {
        Some(M2SemanticToken {
            token_type: M2SemanticTokenType::Namespace,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
        })
    } else if (knowledge.is_subtype(data_type.as_ref(), "Symbol") || is_file)
        && !knowledge.is_subtype(data_type.as_ref(), "Keyword")
        && !knowledge.is_subtype(data_type.as_ref(), "Operator")
    {
        // A nominal enum member is an object whose class is exactly `Symbol`
        // and is protected. Unprotected symbols remain variables.
        let is_symbol_class = data_type.as_ref() == "Symbol";
        let token_type = if is_symbol_class && record.protected.unwrap_or(true) {
            M2SemanticTokenType::EnumMember
        } else {
            M2SemanticTokenType::Variable
        };

        Some(M2SemanticToken {
            token_type,
            is_command: false,
            is_file,
            is_manipulator: false,
            is_constructor: false,
        })
    } else {
        // Every remaining indexed object is still a known global value. Its
        // runtime class has no standard LSP role, so retain `variable` and let
        // provenance/type modifiers specialize it.
        Some(M2SemanticToken {
            token_type: M2SemanticTokenType::Variable,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
        })
    }
}

pub(crate) fn semantic_token_for_static_type_from_knowledge(
    knowledge: &(impl TypeKnowledge + ?Sized),
    type_name: &str,
) -> Option<M2SemanticToken> {
    let is_command = knowledge.is_subtype(type_name, "Command");
    let is_file = knowledge.is_subtype(type_name, "File");
    let is_manipulator = knowledge.is_subtype(type_name, "Manipulator");
    let is_type_valued = knowledge.is_subtype(type_name, "Type");

    let token_type = if type_name.starts_with("MethodFunction") {
        M2SemanticTokenType::Method
    } else if knowledge.is_subtype(type_name, "Package") {
        M2SemanticTokenType::Namespace
    } else if is_type_valued {
        if knowledge
            .get_record(&InstanceID::new(type_name))
            .is_some_and(|record| record.class.as_ref() == "Type")
        {
            M2SemanticTokenType::Class
        } else {
            M2SemanticTokenType::Type
        }
    } else if knowledge.is_subtype(type_name, "Function")
        || knowledge.is_subtype(type_name, "ScriptedFunctor")
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
    } else if knowledge.is_subtype(type_name, "Symbol") {
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

fn indexed_name_is_constructor(knowledge: &(impl TypeKnowledge + ?Sized), name: &str) -> bool {
    let unqualified_name = name.rsplit_once('$').map_or(name, |(_, name)| name);
    let Some(target_name) = unqualified_name.strip_prefix("to") else {
        return false;
    };
    if target_name.is_empty() {
        return false;
    }

    knowledge
        .get_record(&InstanceID::new(target_name))
        .is_some_and(|record| record_is_type_like(&record))
}

fn record_is_type_like(record: &Record) -> bool {
    record
        .type_info
        .as_ref()
        .is_some_and(|type_info| type_info.parent_type.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_walks_the_lattice_to_the_installed_supertype_method() {
        // dim is installed on Ring; PolynomialRing <: Ring.
        let corpus = concat!(
            r#"{"kind":"type","name":"Thing","aliases":[],"extra_keys":[]}"#,
            "\n",
            r#"{"kind":"type","name":"Ring","parent":"Thing","ancestors":["Thing"],"subtypes":["PolynomialRing"],"aliases":[],"extra_keys":[]}"#,
            "\n",
            r#"{"kind":"type","name":"PolynomialRing","parent":"Ring","ancestors":["Ring","Thing"],"subtypes":[],"aliases":[],"extra_keys":[]}"#,
            "\n",
            r#"{"kind":"function","name":"dim","methods":[{"domain":["Ring"],"typicalValue":"ZZ"}],"aliases":[],"extra_keys":[]}"#,
        );
        let builtins = BuiltinData::load_from_index(corpus);

        // Exact match.
        assert_eq!(
            builtins
                .resolve_call_return_type_with_options("dim", &[Some("Ring".to_string())], &[],),
            Some("ZZ".to_string())
        );
        // Subtype dispatch: PolynomialRing has no own method, walks up to Ring's.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                "dim",
                &[Some("PolynomialRing".to_string())],
                &[],
            ),
            Some("ZZ".to_string())
        );
        // No applicable method (Thing is a supertype, not a subtype) ⇒ silent.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                "dim",
                &[Some("Thing".to_string())],
                &[],
            ),
            None
        );
    }

    #[test]
    fn resolves_real_gb_codomain_from_the_type_index() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        assert_eq!(
            builtins
                .resolve_call_return_type_with_options("gb", &[Some("Ideal".to_string())], &[],),
            Some("GroebnerBasis".to_string())
        );
    }

    #[test]
    fn is_subtype_holds_for_the_immediate_parent_edge() {
        // Regression: the corpus `ancestors` field omits the immediate parent, so
        // a lattice built from it alone made `is_subtype(child, parent)` succeed
        // only reflexively (`new Type` worked, `new SelfInitializingType` did not).
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));

        // Direct parent edges (verified against M2): SelfInitializingType <: Type,
        // Array <: VisibleList.
        assert!(builtins.is_subtype("SelfInitializingType", "Type"));
        assert!(builtins.is_subtype(InstanceID::new("Array"), InstanceID::new("VisibleList")));
        // Transitive edges still hold, and unrelated types stay unrelated.
        assert!(builtins.is_subtype("Array", "Thing"));
        assert!(!builtins.is_subtype("Array", "Type"));
    }

    fn generated_builtins() -> BuiltinData {
        BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"))
    }

    #[test]
    fn generated_builtin_data_loads_core_symbols() {
        let builtins = generated_builtins();
        assert!(
            builtins.len() > 1_000,
            "expected a substantial builtin database from the typecheck index"
        );
        assert!(builtins.get_record(&InstanceID::new("ideal")).is_some());
        assert!(
            builtins.names_with_prefix("id", 8).contains(&"ideal"),
            "name index should support live prefix symbol search"
        );
        // Aliases resolve to the same pooled record.
        assert!(
            builtins
                .get_record(&InstanceID::new("Core$ideal"))
                .is_some(),
            "package-qualified aliases should resolve"
        );

        // A function: installed method signatures plus hover docs from the docs asset.
        let ideal = builtins
            .get_record(&InstanceID::new("ideal"))
            .expect("ideal should be present");
        assert!(
            ideal
                .function_info
                .as_ref()
                .is_some_and(|info| info.methods.len() >= 10),
            "ideal should carry its installed method signatures"
        );
        assert!(
            builtins.doc_markdown(&InstanceID::new("ideal")).is_some(),
            "hover docs should load from the separate docs asset"
        );

        // A type: its lattice edges live in type_info; no function_info.
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
            "Ring should carry its lattice subtypes"
        );
        assert!(
            builtins
                .get_record(&InstanceID::new("ZZ"))
                .is_some_and(|record| record.type_info.is_some() && record.function_info.is_none()),
            "ZZ is a type, so it carries type_info and no function_info"
        );

        // An object (constant / option key) is surfaced with its class.
        assert_eq!(
            builtins
                .get_record(&InstanceID::new("pi"))
                .map(|record| record.class.0),
            Some("Constant".to_string()),
            "objects should be pooled as records carrying their class"
        );

        // An operator: forms drive hover labels.
        let plus = builtins
            .get_record(&InstanceID::new("+"))
            .expect("+ operator should be present");
        assert!(
            plus.operator_info.as_ref().is_some_and(|info| {
                info.forms.contains(&"Binary".to_string())
                    && info.forms.contains(&"Prefix".to_string())
            }),
            "+ should preserve its operator forms (binary + prefix)"
        );
    }

    #[test]
    fn operator_flexibility_is_per_form() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));

        // `>` is flexible as a prefix but NOT as a binary — the asymmetric case.
        let greater = builtins
            .get_record(&InstanceID::new(">"))
            .and_then(|record| record.operator_info)
            .expect("> operator should carry operator info");
        assert!(greater.is_flexible("prefix"), "> is flexible as a prefix");
        assert!(
            !greater.is_flexible("binary"),
            "> is NOT flexible as a binary"
        );

        // `-` is flexible in both forms.
        let minus = builtins
            .get_record(&InstanceID::new("-"))
            .and_then(|record| record.operator_info)
            .expect("- operator should carry operator info");
        assert!(minus.is_flexible("binary"), "- is flexible as a binary");
        assert!(minus.is_flexible("prefix"), "- is flexible as a prefix");
    }

    #[test]
    fn prefix_search_does_not_require_sorted_names() {
        // Corpus order: ZZ, about, Ring, coefficient — non-alphabetical across kinds
        let corpus = concat!(
            "{\"kind\":\"type\",\"name\":\"ZZ\"}\n",
            "{\"kind\":\"symbol\",\"name\":\"about\"}\n",
            "{\"kind\":\"type\",\"name\":\"Ring\"}\n",
            "{\"kind\":\"methodFunction\",\"name\":\"coefficient\"}\n",
        );
        let builtins = BuiltinData::load_from_index(corpus);

        assert_eq!(builtins.names_with_prefix("ab", 8), vec!["about"]);
        assert_eq!(builtins.names_with_prefix("co", 8), vec!["coefficient"]);
        assert_eq!(builtins.names_with_prefix("R", 8), vec!["Ring"]);
        assert_eq!(builtins.names_with_prefix("Z", 8), vec!["ZZ"]);
    }

    #[test]
    fn new_corpus_preserves_hover_and_subtype_facts() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));

        // hover markdown still resolves by bare name
        assert!(builtins.doc_markdown(&InstanceID::new("ideal")).is_some());

        // a known subtype edge survives the deref (ZZ is-a Ring's ancestor chain)
        assert!(builtins.is_subtype(InstanceID::new("ZZ"), InstanceID::new("Thing")));

        // a known method codomain resolves (ideal of a … → Ideal is documented)
        assert!(builtins.get_record(&InstanceID::new("ideal")).is_some());
    }
}
