//! In-memory partition of the builtin corpus by home package, plus the
//! `LoadedPackages` tracker — the substrate for loaded-package scoping. Every
//! request builds a [`ScopedIndex`] from the document's imports and queries it
//! instead of a single merged index; parse-time analysis stays Core-scoped via
//! the `Core` partition clone held by the backend.

use std::collections::HashMap;

use crate::builtin_index::{BuiltinIndex, PackageName};
use crate::package_index::collect_imported_packages;
use crate::typesystem::{
    semantic_token_for_static_type_from_knowledge, semantic_token_from_knowledge, BuiltinData,
    InstanceID, LspKnowledge, M2SemanticToken, MethodSignature, PartitionedTypeKnowledge, Record,
    ResolvedSignature, SemanticTokenKnowledge, SignatureUsage, TypeKnowledge,
    TypeKnowledgeProvider,
};

/// Every shipped package's `BuiltinData`, keyed by home package, plus the
/// default-loaded baseline. Built once from the single embedded corpus.
#[derive(Debug, Clone)]
pub(crate) struct PackagePartitionedIndex {
    partitions: HashMap<PackageName, BuiltinData>,
    default_loaded: Vec<String>,
}

impl PackagePartitionedIndex {
    /// Parse the combined corpus once, partition by home package, and build one
    /// `BuiltinData` per partition (each entry carries its own folded markdown).
    /// The baseline is the corpus `meta` record's `default_loaded`; a corpus
    /// without it is corrupt, so we fail fast rather than guess.
    pub fn from_corpus(corpus: &str) -> Self {
        let index = BuiltinIndex::load(corpus);
        let sub_indexes = index.partition_by_package();

        let partitions = sub_indexes
            .iter()
            .map(|(package, sub_index)| (package.clone(), BuiltinData::from_index(sub_index)))
            .collect();

        let default_loaded = index.default_loaded();
        assert!(
            !default_loaded.is_empty(),
            "corpus is missing the mandatory leading `meta` record with default_loaded"
        );

        PackagePartitionedIndex {
            partitions,
            default_loaded: default_loaded.to_vec(),
        }
    }

    pub fn partition(&self, package: &str) -> Option<&BuiltinData> {
        self.partitions.get(package)
    }

    pub fn default_loaded(&self) -> &[String] {
        &self.default_loaded
    }

    pub fn symbol_count(&self) -> usize {
        self.partitions.values().map(BuiltinData::len).sum()
    }

    /// A non-materializing view over the partitions loaded for a document, in
    /// resolution order (baseline/Core-first, then import order). Imports with no
    /// partition in the corpus are skipped. Borrows only the partitions —
    /// `loaded` is consumed for membership/order and is not retained, so the
    /// returned `ScopedIndex` is free to outlive it.
    pub fn scoped<'a>(&'a self, loaded: &LoadedPackages) -> ScopedIndex<'a> {
        let partitions = loaded
            .as_slice()
            .iter()
            .filter_map(|package| {
                self.partitions
                    .get_key_value(package.as_str())
                    .map(|(key, data)| (key.as_str(), data))
            })
            .collect();
        ScopedIndex { partitions }
    }
}

impl TypeKnowledgeProvider for PackagePartitionedIndex {
    type Knowledge<'a> = ScopedIndex<'a>;

    fn knowledge_for<'a>(&'a self, imported_packages: &[String]) -> Self::Knowledge<'a> {
        let loaded = LoadedPackages::from_parts(self.default_loaded(), imported_packages);
        self.scoped(&loaded)
    }
}

impl PartitionedTypeKnowledge for PackagePartitionedIndex {
    fn get_record_from_package(&self, package: &str, name: &InstanceID) -> Option<Record> {
        self.partition(package)?.get_record(name)
    }
}

/// The ordered set of packages in scope for a document: the default-loaded
/// baseline followed by the packages the text imports, deduplicated. A pure
/// function of the text — adding or removing an import simply re-derives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedPackages(Vec<String>);

impl LoadedPackages {
    /// `default_loaded ∪ collect_imported_packages(text)`, baseline-first then
    /// import-order, with duplicates dropped (a re-import of a default package
    /// does not move it).
    pub fn resolve(default_loaded: &[String], text: &str) -> Self {
        Self::from_parts(default_loaded, &collect_imported_packages(text))
    }

    /// Combine the baseline with an already-collected import list (e.g. the set a
    /// document snapshot memoized from its own tree), baseline-first then
    /// import-order, deduplicated. The hot path on each request: no parsing, just
    /// a dedup over a handful of names.
    pub fn from_parts(default_loaded: &[String], imported: &[String]) -> Self {
        let mut ordered = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for package in default_loaded.iter().chain(imported.iter()) {
            if seen.insert(package.as_str()) {
                ordered.push(package.clone());
            }
        }
        LoadedPackages(ordered)
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// An ordered, borrowing view over the loaded packages' `BuiltinData`. Lookups
/// resolve first-match across the ordered partitions; searches aggregate across
/// all of them. Holds only references — never a merged or rebuilt index.
#[derive(Debug, Clone)]
pub(crate) struct ScopedIndex<'a> {
    partitions: Vec<(&'a str, &'a BuiltinData)>,
}

impl<'a> ScopedIndex<'a> {
    fn find_map<T>(&self, query: impl Fn(&'a str, &'a BuiltinData) -> Option<T>) -> Option<T> {
        self.partitions
            .iter()
            .find_map(|(package, data)| query(package, data))
    }

    pub fn get_record_with_package(&self, name: &InstanceID) -> Option<(&'a str, Record)> {
        self.find_map(|package, data| data.get_record(name).map(|record| (package, record)))
    }

    pub fn get_record(&self, name: &InstanceID) -> Option<Record> {
        self.get_record_with_package(name).map(|(_, record)| record)
    }

    pub fn resolve_call_signature_usage(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<SignatureUsage> {
        self.find_map(|_, data| data.resolve_call_signature_usage(callable, argument_types))
    }

    /// Names across all loaded partitions starting with `prefix`, deduped by name
    /// (first occurrence wins, baseline-first), capped at `limit`. Each entry is
    /// `(package, name)` so callers can label provenance.
    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(&'a str, &'a str)> {
        self.aggregate_names(limit, |data, remaining| {
            data.names_with_prefix(prefix, remaining)
        })
    }

    pub fn matching_names(&self, query: &str, limit: usize) -> Vec<(&'a str, &'a str)> {
        self.aggregate_names(limit, |data, remaining| {
            data.matching_names(query, remaining)
        })
    }

    fn aggregate_names(
        &self,
        limit: usize,
        per_partition: impl Fn(&'a BuiltinData, usize) -> Vec<&'a str>,
    ) -> Vec<(&'a str, &'a str)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (package, data) in &self.partitions {
            if out.len() >= limit {
                break;
            }
            for name in per_partition(data, limit.saturating_sub(out.len())) {
                if seen.insert(name) {
                    out.push((*package, name));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        out
    }
}

impl TypeKnowledge for ScopedIndex<'_> {
    fn is_available(&self) -> bool {
        true
    }

    fn get_record(&self, name: &InstanceID) -> Option<Record> {
        ScopedIndex::get_record(self, name)
    }

    fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        options: &[(String, String)],
    ) -> Option<String> {
        self.find_map(|_, data| {
            data.resolve_call_return_type_with_options(callable, argument_types, options)
        })
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        self.partitions
            .iter()
            .any(|(_, data)| data.is_subtype(child, parent))
    }
}

impl SemanticTokenKnowledge for ScopedIndex<'_> {
    fn semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        semantic_token_from_knowledge(self, name)
    }

    fn semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        semantic_token_for_static_type_from_knowledge(self, type_name)
    }

    fn is_protected_symbol(&self, name: &str) -> bool {
        let name = InstanceID::new(name);
        self.find_map(|_, data| {
            data.get_record(&name)
                .map(|_| data.is_protected_symbol(name.as_ref()))
        })
        .unwrap_or(false)
    }

    fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool {
        self.partitions
            .iter()
            .any(|(_, data)| data.is_option_value_for_key(option_key, value_name))
    }
}

impl LspKnowledge for ScopedIndex<'_> {
    fn get_record_with_package(&self, name: &InstanceID) -> Option<(String, Record)> {
        ScopedIndex::get_record_with_package(self, name)
            .map(|(package, record)| (package.to_string(), record))
    }

    fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String)> {
        ScopedIndex::names_with_prefix(self, prefix, limit)
            .into_iter()
            .map(|(package, name)| (package.to_string(), name.to_string()))
            .collect()
    }

    fn matching_names(&self, query: &str, limit: usize) -> Vec<(String, String)> {
        ScopedIndex::matching_names(self, query, limit)
            .into_iter()
            .map(|(package, name)| (package.to_string(), name.to_string()))
            .collect()
    }

    fn resolve_call_signature_usage(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<SignatureUsage> {
        ScopedIndex::resolve_call_signature_usage(self, callable, argument_types)
    }

    fn documented_signatures(&self, record: &Record) -> Vec<ResolvedSignature> {
        self.find_map(|_, data| {
            data.get_record(&record.name)
                .map(|_| data.documented_signatures(record))
        })
        .unwrap_or_default()
    }

    fn undocumented_installed_methods(&self, record: &Record) -> Vec<MethodSignature> {
        self.find_map(|_, data| {
            data.get_record(&record.name)
                .map(|_| data.undocumented_installed_methods(record))
        })
        .unwrap_or_default()
    }

    fn option_usage_names(&self, option_name: &str, limit: usize) -> Vec<String> {
        let name = InstanceID::new(option_name);
        self.find_map(|_, data| {
            data.get_record(&name)
                .map(|_| data.option_usage_names(option_name, limit))
        })
        .unwrap_or_default()
    }

    fn option_value_usage_names(&self, value_name: &str, limit: usize) -> Vec<String> {
        let name = InstanceID::new(value_name);
        self.find_map(|_, data| {
            data.get_record(&name)
                .map(|_| data.option_value_usage_names(value_name, limit))
        })
        .unwrap_or_default()
    }

    fn doc_markdown(&self, name: &InstanceID) -> Option<String> {
        self.find_map(|_, data| {
            data.get_record(name)
                .and_then(|_| data.doc_markdown(name).map(str::to_string))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> &'static str {
        include_str!("./data/m2-index.jsonl")
    }

    #[test]
    fn from_corpus_builds_a_core_partition() {
        let index = PackagePartitionedIndex::from_corpus(corpus());
        let core = index.partition("Core").expect("Core partition present");
        assert!(core
            .get_record(&crate::typesystem::InstanceID::new("ZZ"))
            .is_some());
    }

    #[test]
    #[should_panic(expected = "missing the mandatory leading `meta` record")]
    fn from_corpus_panics_without_meta() {
        // A corpus with object records but no meta line is corrupt.
        PackagePartitionedIndex::from_corpus(
            r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#,
        );
    }

    #[test]
    fn default_loaded_uses_meta_record_when_present() {
        let synthetic = concat!(
            r#"{"kind":"meta","default_loaded":["Core","Classic"]}"#,
            "\n",
            r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#,
        );
        let index = PackagePartitionedIndex::from_corpus(synthetic);
        assert_eq!(index.default_loaded(), &["Core", "Classic"]);
    }

    #[test]
    fn loaded_packages_is_baseline_then_imports_deduped() {
        let loaded = LoadedPackages::resolve(
            &["Core".to_string()],
            "needsPackage \"FooPkg\"\nneedsPackage \"Core\"",
        );
        // Baseline Core first; FooPkg appended; the re-import of Core is dropped.
        assert_eq!(
            loaded.as_slice(),
            &["Core".to_string(), "FooPkg".to_string()]
        );
    }

    #[test]
    fn curated_extras_are_non_default_partitions() {
        let index = PackagePartitionedIndex::from_corpus(corpus());

        // JSON ships as its own partition with its symbols...
        let json = index.partition("JSON").expect("JSON partition present");
        assert!(json
            .get_record(&crate::typesystem::InstanceID::new("toJSON"))
            .is_some());
        assert!(json
            .get_record(&crate::typesystem::InstanceID::new("fromJSON"))
            .is_some());

        // ...but it is NOT autoloaded: absent from the default-loaded baseline...
        assert!(
            !index.default_loaded().iter().any(|p| p == "JSON"),
            "JSON is a non-default package and must stay out of the baseline"
        );

        // ...and absent from the Core partition (so self.builtins won't resolve
        // it until P3 routes imports through loaded partitions).
        let core = index.partition("Core").expect("Core partition present");
        assert!(core
            .get_record(&crate::typesystem::InstanceID::new("toJSON"))
            .is_none());
    }

    #[test]
    fn loaded_packages_picks_up_an_imported_extra() {
        let index = PackagePartitionedIndex::from_corpus(corpus());

        let imported = LoadedPackages::resolve(index.default_loaded(), "needsPackage \"JSON\"");
        assert!(
            imported.as_slice().iter().any(|p| p == "JSON"),
            "an imported non-default package joins the loaded set"
        );

        let plain = LoadedPackages::resolve(index.default_loaded(), "1 + 1");
        assert!(
            !plain.as_slice().iter().any(|p| p == "JSON"),
            "an un-imported non-default package stays out of the loaded set"
        );
    }

    #[test]
    fn removing_an_import_unloads_the_package() {
        // `LoadedPackages` is a pure function of the text, so "the import line
        // was deleted" is identical to "the text never had it" — no add/remove
        // state to get wrong. Resolving the post-edit text simply omits JSON.
        let index = PackagePartitionedIndex::from_corpus(corpus());
        let baseline = index.default_loaded();

        let with_import = LoadedPackages::resolve(baseline, "needsPackage \"JSON\"\n1 + 1");
        assert!(with_import.as_slice().iter().any(|p| p == "JSON"));

        // The same document with the `needsPackage` line removed.
        let after_removal = LoadedPackages::resolve(baseline, "1 + 1");
        assert!(
            !after_removal.as_slice().iter().any(|p| p == "JSON"),
            "deleting the import line drops the package from the loaded set"
        );
    }

    #[test]
    fn scoped_resolves_only_loaded_partitions() {
        let index = PackagePartitionedIndex::from_corpus(corpus());

        // Baseline only: Core resolves, JSON does not.
        let baseline = LoadedPackages::resolve(index.default_loaded(), "1 + 1");
        let scoped = index.scoped(&baseline);
        assert!(scoped.get_record(&InstanceID::new("ZZ")).is_some());
        assert!(scoped.get_record(&InstanceID::new("toJSON")).is_none());

        // Import JSON: now toJSON resolves, tagged with its package.
        let loaded = LoadedPackages::resolve(index.default_loaded(), "needsPackage \"JSON\"");
        let scoped = index.scoped(&loaded);
        let (pkg, record) = scoped
            .get_record_with_package(&InstanceID::new("toJSON"))
            .expect("toJSON resolves once JSON is loaded");
        assert_eq!(pkg, "JSON");
        assert_eq!(record.name.0, "toJSON");
    }

    #[test]
    fn scoped_skips_imports_absent_from_corpus() {
        // Importing a package not in the corpus simply contributes nothing.
        let index = PackagePartitionedIndex::from_corpus(corpus());
        let loaded = LoadedPackages::resolve(index.default_loaded(), "needsPackage \"NoSuchPkg\"");
        let scoped = index.scoped(&loaded);
        assert!(scoped.get_record(&InstanceID::new("ZZ")).is_some());
    }
}
