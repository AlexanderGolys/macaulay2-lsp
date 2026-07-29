//! Package loading and explicit-package queries over the semantic object registry.

use crate::builtin_index::Record;
use crate::object_registry::{ObjectName, ObjectRegistry, TypeId};
#[cfg(test)]
use crate::package_index::collect_imported_packages;
use crate::record_lsp::PartitionedTypeKnowledge;

impl ObjectRegistry {
    pub fn symbol_count(&self) -> usize {
        self.len()
    }

    /// Derive the range-aware registry for one test source.
    #[cfg(test)]
    pub fn with_source_imports(&self, text: &str) -> ObjectRegistry {
        self.with_imports(&collect_imported_packages(text))
    }
}

impl PartitionedTypeKnowledge for ObjectRegistry {
    fn get_record_from_package(&self, package: &str, name: &ObjectName) -> Option<&Record> {
        let package = self.catalog_package_id(&ObjectName::new(package))?;
        let object = self.package_objects(package)?.objects_by_name.get(name)?;
        self.object(object)
    }

    fn get_type_by_id(&self, type_id: &TypeId) -> Option<(String, &Record)> {
        let record = self.object(type_id.object())?;
        record.type_info()?;
        Some((self.package_name(&record.package)?.to_string(), record))
    }

    fn direct_subtypes(&self, type_id: &TypeId) -> Vec<(String, &Record)> {
        self.catalog_records()
            .iter()
            .filter_map(|record| {
                record
                    .type_info()
                    .filter(|type_info| &type_info.parent == type_id)
                    .filter(|_| &record.id != type_id.object())
                    .and_then(|_| Some((self.package_name(&record.package)?.to_string(), record)))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_registry::ObjectKnowledge;
    use crate::record_lsp::LspKnowledge;

    fn corpus() -> &'static str {
        include_str!("./data/m2-index.jsonl")
    }

    #[test]
    fn from_corpus_builds_a_core_partition() {
        let registry = ObjectRegistry::load(corpus());
        assert!(registry.package_id(&ObjectName::new("Core")).is_some());
        assert!(registry.get_record(&ObjectName::new("ZZ")).is_some());
    }

    #[test]
    fn default_loaded_uses_meta_record_when_present() {
        let synthetic = concat!(
            r#"{"kind":"meta","default_loaded":["Core","Classic"]}"#,
            "\n",
            r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#,
        );
        let index = ObjectRegistry::load(synthetic);
        assert_eq!(index.default_loaded(), &["Core", "Classic"]);
    }

    #[test]
    fn curated_extras_are_not_loaded_into_the_baseline_registry() {
        let registry = ObjectRegistry::load(corpus());
        assert!(
            !registry.default_loaded().iter().any(|p| p == "JSON"),
            "JSON is a non-default package and must stay out of the baseline"
        );
        assert!(registry.package_id(&ObjectName::new("JSON")).is_none());
        assert!(registry.get_record(&ObjectName::new("toJSON")).is_none());
        assert!(registry.get_record(&ObjectName::new("fromJSON")).is_none());
    }

    #[test]
    fn loaded_packages_picks_up_an_imported_extra() {
        let registry = ObjectRegistry::load(corpus());

        let imported = registry.with_source_imports("needsPackage \"JSON\"");
        assert!(imported.get_record(&ObjectName::new("toJSON")).is_some());

        let plain = registry.with_source_imports("1 + 1");
        assert!(plain.get_record(&ObjectName::new("toJSON")).is_none());
    }

    #[test]
    fn removing_an_import_unloads_the_package() {
        let registry = ObjectRegistry::load(corpus());

        let with_import = registry.with_source_imports("needsPackage \"JSON\"\n1 + 1");
        assert!(with_import.get_record(&ObjectName::new("toJSON")).is_some());

        let after_removal = registry.with_source_imports("1 + 1");
        assert!(
            after_removal
                .get_record(&ObjectName::new("toJSON"))
                .is_none(),
            "deleting the import line drops the package from the loaded set"
        );
    }

    #[test]
    fn scoped_resolves_only_loaded_partitions() {
        let registry = ObjectRegistry::load(corpus());

        // Baseline only: Core resolves, JSON does not.
        let scoped = registry.with_source_imports("1 + 1");
        assert!(scoped.get_record(&ObjectName::new("ZZ")).is_some());
        assert!(scoped.get_record(&ObjectName::new("toJSON")).is_none());
        let zz_id = scoped
            .resolve_object(&ObjectName::new("ZZ"))
            .expect("loaded objects resolve to identities");
        assert_eq!(
            scoped.object(&zz_id).map(|record| &record.name),
            Some(&ObjectName::new("ZZ"))
        );

        // Import JSON: now toJSON resolves, tagged with its package.
        let scoped = registry.with_source_imports("needsPackage \"JSON\"");
        assert!(scoped.package_id(&ObjectName::new("JSON")).is_some());
        let (pkg, record) = scoped
            .get_record_with_package(&ObjectName::new("toJSON"))
            .expect("toJSON resolves once JSON is loaded");
        assert_eq!(pkg, "JSON");
        assert_eq!(record.name.0, "toJSON");
    }

    #[test]
    fn scoped_skips_imports_absent_from_corpus() {
        // Importing a package not in the corpus simply contributes nothing.
        let registry = ObjectRegistry::load(corpus());
        let scoped = registry.with_source_imports("needsPackage \"NoSuchPkg\"");
        assert!(scoped.get_record(&ObjectName::new("ZZ")).is_some());
    }
}
