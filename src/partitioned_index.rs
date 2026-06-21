//! In-memory partition of the builtin corpus by home package, plus the
//! `LoadedPackages` tracker. These are the substrate for loaded-package scoping
//! (P3): they are built at startup but not yet consulted by any query — the
//! inference/hover/navigation paths still read `self.builtins` directly.

// Forward-looking API consumed in P3 (scoped query routing); allow until then.
#![allow(dead_code)]

use std::collections::HashMap;

use crate::builtin_index::BuiltinIndex;
use crate::package_index::collect_imported_packages;
use crate::typesystem::BuiltinData;

/// Every shipped package's `BuiltinData`, keyed by home package, plus the
/// default-loaded baseline. Built once from the single embedded corpus.
#[derive(Debug, Clone)]
pub(crate) struct PackagePartitionedIndex {
    partitions: HashMap<String, BuiltinData>,
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

        let mut partitions = HashMap::new();
        for (package, sub_index) in &sub_indexes {
            partitions.insert(package.clone(), BuiltinData::from_index(sub_index));
        }

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
        assert!(json.contains_name("toJSON"));
        assert!(json.contains_name("fromJSON"));

        // ...but it is NOT autoloaded: absent from the default-loaded baseline...
        assert!(
            !index.default_loaded().iter().any(|p| p == "JSON"),
            "JSON is a non-default package and must stay out of the baseline"
        );

        // ...and absent from the Core partition (so self.builtins won't resolve
        // it until P3 routes imports through loaded partitions).
        let core = index.partition("Core").expect("Core partition present");
        assert!(!core.contains_name("toJSON"));
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
}
