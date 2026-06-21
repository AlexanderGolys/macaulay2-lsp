//! In-memory partition of the builtin corpus by home package, plus the
//! `LoadedPackages` tracker. These are the substrate for loaded-package scoping
//! (P3): they are built at startup but not yet consulted by any query — the
//! inference/hover/navigation paths still read `self.builtins` directly.

// Forward-looking API consumed in P3 (scoped query routing); allow until then.
#![allow(dead_code)]

use std::collections::HashMap;

use crate::builtin_index::BuiltinIndex;
use crate::package_index::collect_imported_packages;
use crate::typesystem::{load_docs_markdown_by_package, BuiltinData};

/// Every shipped package's `BuiltinData`, keyed by home package, plus the
/// default-loaded baseline. Built once from the single embedded corpus.
#[derive(Debug, Clone)]
pub(crate) struct PackagePartitionedIndex {
    partitions: HashMap<String, BuiltinData>,
    default_loaded: Vec<String>,
}

impl PackagePartitionedIndex {
    /// Parse the corpus once, partition by home package, and build one
    /// `BuiltinData` per partition. The baseline is the corpus `meta` record's
    /// `default_loaded`; when the corpus carries none (today's Core-only file),
    /// it falls back to the sorted set of packages actually present — correct
    /// while only Core ships, and self-correcting once fundocs emits `meta`.
    pub fn from_corpus(types_jsonl: &str, docs_jsonl: &str) -> Self {
        let index = BuiltinIndex::load(types_jsonl);
        let sub_indexes = index.partition_by_package();
        let mut docs_by_package = load_docs_markdown_by_package(docs_jsonl);

        let mut partitions = HashMap::new();
        for (package, sub_index) in &sub_indexes {
            let docs = docs_by_package.remove(package).unwrap_or_default();
            partitions.insert(package.clone(), BuiltinData::from_index(sub_index, docs));
        }

        let default_loaded = if index.default_loaded().is_empty() {
            let mut present: Vec<String> = partitions.keys().cloned().collect();
            present.sort();
            present
        } else {
            index.default_loaded().to_vec()
        };

        PackagePartitionedIndex {
            partitions,
            default_loaded,
        }
    }

    pub fn partition(&self, package: &str) -> Option<&BuiltinData> {
        self.partitions.get(package)
    }

    pub fn default_loaded(&self) -> &[String] {
        &self.default_loaded
    }

    pub fn packages(&self) -> impl Iterator<Item = &str> {
        self.partitions.keys().map(String::as_str)
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
        let mut ordered = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for package in default_loaded
            .iter()
            .cloned()
            .chain(collect_imported_packages(text))
        {
            if seen.insert(package.clone()) {
                ordered.push(package);
            }
        }
        LoadedPackages(ordered)
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> &'static str {
        include_str!("./data/m2-types.jsonc")
    }
    fn docs() -> &'static str {
        include_str!("./data/m2-docs.jsonl")
    }

    #[test]
    fn from_corpus_builds_a_core_partition() {
        let index = PackagePartitionedIndex::from_corpus(corpus(), docs());
        let core = index.partition("Core").expect("Core partition present");
        assert!(core
            .get_record(&crate::typesystem::InstanceID::new("ZZ"))
            .is_some());
    }

    #[test]
    fn default_loaded_falls_back_to_present_packages_without_meta() {
        // Today's corpus has no meta record, so the baseline is the packages
        // present — Core only.
        let index = PackagePartitionedIndex::from_corpus(corpus(), docs());
        assert_eq!(index.default_loaded(), &["Core"]);
    }

    #[test]
    fn default_loaded_uses_meta_record_when_present() {
        let synthetic = r#"[
            {"kind":"meta","default_loaded":["Core","Classic"]},
            {"kind":"type","name":"ZZ","package":"$Core$Core"}
        ]"#;
        let index = PackagePartitionedIndex::from_corpus(synthetic, "");
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
}
