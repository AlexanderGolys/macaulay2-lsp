//! Static type relations, dispatch, and semantic queries over the canonical
//! builtin records owned by `builtin_index`.
//!
//! This module does not evaluate Macaulay2. It queries generated corpus facts
//! to answer conservative questions about known objects, method signatures,
//! type relations, option values, hover text, and semantic-token roles.

use std::collections::{HashMap, HashSet};

use crate::builtin_index::{BuiltinData, CodeExample, InstanceID, MethodSignature, Record};

/// The semantic facts analysis needs from an external symbol/type index.
///
/// Keeping this as a narrow trait decouples the analysis engine from the
/// concrete generated-corpus store. The normal server implements it with
/// [`BuiltinData`]; syntax-only tests use an empty implementation. A future
/// workspace/package view can implement the same contract without teaching
/// analysis about its storage or resolution order.
pub(crate) trait TypeKnowledge {
    fn is_available(&self) -> bool;

    fn get_record(&self, name: &InstanceID) -> Option<&Record>;

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

pub(crate) trait LspKnowledge: TypeKnowledge {
    fn get_record_with_package(&self, name: &InstanceID) -> Option<(String, &Record)>;

    fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String)>;

    fn matching_names(&self, query: &str, limit: usize) -> Vec<(String, String)>;

    fn resolve_call_signature_usage(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<SignatureUsage>;

    fn documented_signatures(&self, record: &Record) -> Vec<ResolvedSignature>;

    fn undocumented_installed_methods(&self, record: &Record) -> Vec<MethodSignature>;

    fn option_usage_names(&self, option_name: &str, limit: usize) -> Vec<String>;

    fn option_value_usage_names(&self, value_name: &str, limit: usize) -> Vec<String>;

    fn doc_markdown(&self, name: &InstanceID) -> Option<String>;
}

pub(crate) trait PartitionedTypeKnowledge {
    fn get_record_from_package(&self, package: &str, name: &InstanceID) -> Option<&Record>;
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

/// One callable signature after documentation and indexed type facts are merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSignature {
    pub signature: Vec<InstanceID>,
    pub output_types: Vec<InstanceID>,
    pub is_specialized: bool,
    pub examples: Vec<CodeExample>,
    pub doc_key: Option<InstanceID>,
}

/// Installed signatures partitioned by their applicability at one call site.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SignatureUsage {
    pub pinned: Option<ResolvedSignature>,
    pub possible: Vec<ResolvedSignature>,
    pub excluded: Vec<ResolvedSignature>,
}

impl BuiltinData {
    /// Number of primary records; aliases do not increase this count.
    pub fn len(&self) -> usize {
        self.index.records().len()
    }

    /// Primary names beginning with `prefix`, in corpus order and capped at `limit`.
    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<&str> {
        if prefix.is_empty() || limit == 0 {
            return Vec::new();
        }

        self.index
            .records()
            .iter()
            .map(|record| record.name.0.as_str())
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
        self.index
            .records()
            .iter()
            .map(|record| record.name.0.as_str())
            .filter(|name| name.to_lowercase().contains(&query))
            .take(limit)
            .collect()
    }

    /// Borrow the record named by `name`, resolving aliases through the canonical
    /// index.
    pub fn get_record(&self, name: &InstanceID) -> Option<&Record> {
        self.index.record(name)
    }

    /// The pre-rendered hover markdown for `name` (or one of its aliases), if the
    /// docs asset carried an entry. Typecheck records hold no doc text. Aliases
    /// resolve to the record's primary name, under which the docs are keyed.
    pub fn doc_markdown(&self, name: &InstanceID) -> Option<&str> {
        self.get_record(name)?.markdown.as_deref()
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
        for record in self.index.records() {
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

            let display_name = record
                .name
                .0
                .rsplit_once('$')
                .map_or(record.name.0.as_str(), |(_, name)| name);
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
        self.option_facts
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

        self.option_facts
            .option_values_by_slot
            .iter()
            .filter(|(slot, _)| slot.option.0 == option_key)
            .any(|(_, values)| values.iter().any(|value| value.0 == value_name))
    }

    /// Resolve installed method codomains, falling back to the callable's
    /// function-level typical value when a method has no more precise result.
    pub fn documented_signatures(&self, record: &Record) -> Vec<ResolvedSignature> {
        let Some(function_info) = &record.function_info else {
            return Vec::new();
        };

        let general_output = record.typical_value.as_deref().map(InstanceID::new);

        let mut signatures = Vec::new();
        for method in &function_info.methods {
            if signature_domain_key(&method.signature).is_none() {
                continue;
            }
            if let Some(codomain) = &method.codomain {
                signatures.push(ResolvedSignature {
                    signature: method.signature.clone(),
                    output_types: vec![codomain.clone()],
                    is_specialized: true,
                    examples: Vec::new(),
                    doc_key: None,
                });
            } else if let Some(output_type) = &general_output {
                signatures.push(ResolvedSignature {
                    signature: method.signature.clone(),
                    output_types: vec![output_type.clone()],
                    is_specialized: false,
                    examples: Vec::new(),
                    doc_key: None,
                });
            }
        }

        if signatures.is_empty() && general_output.is_some() {
            signatures.push(ResolvedSignature {
                signature: vec![record.name.clone()],
                output_types: general_output.into_iter().collect(),
                is_specialized: false,
                examples: Vec::new(),
                doc_key: None,
            });
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

        self.get_record(&InstanceID::new(callable))?
            .typical_value
            .clone()
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
        for signature in self.documented_signatures(record) {
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

        for signature in self.all_installed_signatures(record) {
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

    fn get_record(&self, name: &InstanceID) -> Option<&Record> {
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

    fn get_record(&self, name: &InstanceID) -> Option<&Record> {
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

impl LspKnowledge for BuiltinData {
    fn get_record_with_package(&self, name: &InstanceID) -> Option<(String, &Record)> {
        let record = BuiltinData::get_record(self, name)?;
        let package = record.package.clone().unwrap_or_else(|| "Core".to_string());
        Some((package, record))
    }

    fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String)> {
        BuiltinData::names_with_prefix(self, prefix, limit)
            .into_iter()
            .map(|name| {
                let package = BuiltinData::get_record(self, &InstanceID::new(name))
                    .and_then(|record| record.package.clone())
                    .unwrap_or_else(|| "Core".to_string());
                (package, name.to_string())
            })
            .collect()
    }

    fn matching_names(&self, query: &str, limit: usize) -> Vec<(String, String)> {
        BuiltinData::matching_names(self, query, limit)
            .into_iter()
            .map(|name| {
                let package = BuiltinData::get_record(self, &InstanceID::new(name))
                    .and_then(|record| record.package.clone())
                    .unwrap_or_else(|| "Core".to_string());
                (package, name.to_string())
            })
            .collect()
    }

    fn resolve_call_signature_usage(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<SignatureUsage> {
        BuiltinData::resolve_call_signature_usage(self, callable, argument_types)
    }

    fn documented_signatures(&self, record: &Record) -> Vec<ResolvedSignature> {
        BuiltinData::documented_signatures(self, record)
    }

    fn undocumented_installed_methods(&self, record: &Record) -> Vec<MethodSignature> {
        BuiltinData::undocumented_installed_methods(self, record)
    }

    fn option_usage_names(&self, option_name: &str, limit: usize) -> Vec<String> {
        BuiltinData::option_usage_names(self, option_name, limit)
    }

    fn option_value_usage_names(&self, value_name: &str, limit: usize) -> Vec<String> {
        BuiltinData::option_value_usage_names(self, value_name, limit)
    }

    fn doc_markdown(&self, name: &InstanceID) -> Option<String> {
        BuiltinData::doc_markdown(self, name).map(str::to_string)
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

    fn get_record(&self, _name: &InstanceID) -> Option<&Record> {
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
    knowledge: &(impl TypeKnowledge + ?Sized),
    signatures: &mut Vec<ResolvedSignature>,
) -> Vec<ResolvedSignature> {
    let originals = signatures.clone();
    let mut dominated = Vec::new();
    signatures.retain(|candidate| {
        let is_dominated = originals
            .iter()
            .any(|other| signature_strictly_smaller(knowledge, other, candidate));
        if is_dominated {
            dominated.push(candidate.clone());
        }
        !is_dominated
    });
    dominated
}

fn signature_strictly_smaller(
    knowledge: &(impl TypeKnowledge + ?Sized),
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
        if knowledge.is_subtype(small.as_ref(), big.as_ref()) {
            strict = true;
            continue;
        }
        return false;
    }

    strict
}

fn domain_possibly_matches(
    knowledge: &(impl TypeKnowledge + ?Sized),
    domain: &[InstanceID],
    argument_types: &[Option<String>],
) -> bool {
    domain
        .iter()
        .zip(argument_types)
        .all(|(domain_type, argument_type)| {
            argument_type.as_ref().is_none_or(|argument_type| {
                argument_type == &domain_type.0
                    || knowledge.is_subtype(argument_type, domain_type.as_ref())
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

/// The LSP-standard token types emitted for M2 syntax and indexed metadata.
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
pub enum M2SemanticTokenProvenance {
    None,
    DefaultLibrary,
    Builtin,
}

/// A semantic-token role plus M2-specific modifier facts for one identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M2SemanticToken {
    pub token_type: M2SemanticTokenType,
    pub is_command: bool,
    pub is_file: bool,
    pub is_manipulator: bool,
    pub is_constructor: bool,
    pub provenance: M2SemanticTokenProvenance,
}

impl BuiltinData {
    /// Build a `BuiltinData` over the whole combined corpus (`m2-index.jsonl`).
    /// Production routes through `PackagePartitionedIndex::from_corpus` and uses
    /// only the Core partition; this whole-corpus convenience is for tests.
    #[cfg(test)]
    pub fn load_from_index(corpus: &str) -> Self {
        Self::from_index(crate::builtin_index::BuiltinIndex::load(corpus))
    }

    /// An empty index — no records, no facts, empty lattice. For tests and
    /// snapshots that need a `BuiltinData` placeholder with no builtin knowledge.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self::from_index(crate::builtin_index::BuiltinIndex::default())
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

    /// Check if child is a subtype of parent (inclusive), using the normalized
    /// ancestor chain stored on the canonical type record.
    pub fn is_subtype(&self, child: impl AsRef<str>, parent: impl AsRef<str>) -> bool {
        let child = child.as_ref();
        let parent = parent.as_ref();
        child == parent
            || self
                .get_record(&InstanceID::new(child))
                .and_then(|record| record.type_info.as_ref())
                .is_some_and(|type_info| {
                    type_info
                        .ancestors
                        .binary_search_by(|candidate| candidate.as_ref().cmp(parent))
                        .is_ok()
                })
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
    let provenance = if is_compiled_function {
        M2SemanticTokenProvenance::Builtin
    } else if record.package.as_deref() == Some("Core") {
        M2SemanticTokenProvenance::DefaultLibrary
    } else {
        M2SemanticTokenProvenance::None
    };

    // An indexed type whose own class is `Type` is an M2 class (for example
    // `Array`). Other type-valued objects, such as `ZZ` whose class is `Ring`,
    // keep the standard `type` role.
    if record_is_type_like(record) {
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
            provenance,
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
        let token_type = if is_manipulator {
            M2SemanticTokenType::Operator
        } else if provenance == M2SemanticTokenProvenance::DefaultLibrary {
            M2SemanticTokenType::Method
        } else if is_command || is_scripted_functor || is_compiled_function {
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
            provenance,
        })
    } else if knowledge.is_subtype(data_type.as_ref(), "Package") {
        Some(M2SemanticToken {
            token_type: M2SemanticTokenType::Namespace,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
            provenance,
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
            provenance,
        })
    } else {
        // Every remaining indexed object is still a known global value. Its
        // runtime class has no standard LSP role, so retain `variable`.
        Some(M2SemanticToken {
            token_type: M2SemanticTokenType::Variable,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
            provenance,
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
        provenance: M2SemanticTokenProvenance::None,
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
        .is_some_and(record_is_type_like)
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
                .map(|record| record.class.0.clone()),
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
            .and_then(|record| record.operator_info.clone())
            .expect("> operator should carry operator info");
        assert!(greater.is_flexible("prefix"), "> is flexible as a prefix");
        assert!(
            !greater.is_flexible("binary"),
            "> is NOT flexible as a binary"
        );

        // `-` is flexible in both forms.
        let minus = builtins
            .get_record(&InstanceID::new("-"))
            .and_then(|record| record.operator_info.clone())
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
