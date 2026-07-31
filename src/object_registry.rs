//! Canonical object identities, typed object handles, and registry lookup.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display, Result};
use std::ptr;
use std::sync::Arc;
use std::{cmp::Ordering, fmt::Formatter};

use crate::builtin_index::{BuiltinIndex, OptionFacts, Record};
#[cfg(test)]
use crate::package_index::collect_imported_packages;
use crate::package_index::PackageImport;
use tower_lsp::lsp_types::Position;

/// Nominal spelling used to resolve a Macaulay2 object in an environment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectName(pub String);

impl ObjectName {
    /// Construct a nominal lookup spelling.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Borrow the spelling.
    pub fn name(&self) -> &str {
        &self.0
    }
}

impl Display for ObjectName {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result {
        formatter.write_str(self.name())
    }
}

impl Borrow<str> for ObjectName {
    fn borrow(&self) -> &str {
        self.name()
    }
}

impl AsRef<str> for ObjectName {
    fn as_ref(&self) -> &str {
        self.name()
    }
}

/// Syntactic form in which an operator is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorForm {
    Binary,
    Prefix,
    Postfix,
    Assignment,
}

impl Display for OperatorForm {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result {
        formatter.write_str(match self {
            Self::Binary => "binary",
            Self::Prefix => "prefix",
            Self::Postfix => "postfix",
            Self::Assignment => "assignment",
        })
    }
}

/// Name and membership indexes for one package's objects.
#[derive(Debug, Clone, Default)]
pub struct PackageObjects {
    pub objects: Vec<ObjectId>,
    pub objects_by_name: HashMap<ObjectName, ObjectId>,
}

/// Immutable indexed object population from which package registries are loaded.
#[derive(Debug)]
struct ObjectCatalog {
    index: BuiltinIndex,
    option_facts: OptionFacts,
    packages: HashMap<ObjectId, PackageObjects>,
    packages_by_name: HashMap<ObjectName, ObjectId>,
    default_loaded: Vec<String>,
}

/// Ordered registry of the packages loaded for one Macaulay2 environment.
#[derive(Debug, Clone)]
pub struct ObjectRegistry {
    catalog: Arc<ObjectCatalog>,
    packages: Vec<PackageRegistration>,
}

/// One source-ordered package inclusion in an object registry.
#[derive(Debug, Clone)]
struct PackageRegistration {
    package: ObjectId,
    effective_from: Option<Position>,
}

/// Range-specific view of an object registry.
#[derive(Debug, Clone, Copy)]
pub struct ObjectRegistryView<'registry> {
    registry: &'registry ObjectRegistry,
    position: Position,
}

impl Default for ObjectRegistry {
    fn default() -> Self {
        Self::from_index(BuiltinIndex::default())
    }
}

impl ObjectRegistry {
    /// Build the shared catalog and a registry containing its default packages.
    pub fn load(corpus: &str) -> Self {
        Self::from_index(BuiltinIndex::load(corpus))
    }

    /// Number of primary objects; aliases do not increase this count.
    pub fn len(&self) -> usize {
        let mut seen = HashSet::new();
        self.packages
            .iter()
            .filter(|registration| seen.insert(&registration.package))
            .filter_map(|registration| self.catalog.packages.get(&registration.package))
            .map(|package| package.objects.len())
            .sum()
    }

    /// Build the shared catalog and load only the fresh-session package baseline.
    pub fn from_index(index: BuiltinIndex) -> Self {
        let option_facts = OptionFacts::from_records(index.records());
        let default_loaded = index.default_loaded_packages().to_vec();
        let mut package_objects: HashMap<ObjectId, PackageObjects> = HashMap::new();
        for record in index.records() {
            let package = package_objects.entry(record.package.clone()).or_default();
            package.objects.push(record.id.clone());
            for name in record.lookup_names() {
                package
                    .objects_by_name
                    .entry(name)
                    .or_insert_with(|| record.id.clone());
            }
        }
        let packages_by_name = package_objects
            .keys()
            .map(|package| {
                let package_record = index
                    .object(package)
                    .expect("every package identity must name a registered object");
                (package_record.name.clone(), package.clone())
            })
            .collect();
        let catalog = Arc::new(ObjectCatalog {
            index,
            option_facts,
            packages: package_objects,
            packages_by_name,
            default_loaded,
        });
        let mut registry = Self {
            catalog,
            packages: Vec::new(),
        };
        let mut baseline = registry.catalog.default_loaded.clone();
        if baseline.is_empty() {
            baseline.push("Core".to_string());
        }
        for package in baseline {
            registry.load_package(&ObjectName::new(package));
        }
        registry
    }

    /// Add one catalogued package to this registry, preserving load order.
    pub fn load_package(&mut self, name: &ObjectName) -> bool {
        self.register_package(name, None)
    }

    /// Register one package from the end of its source inclusion onward.
    pub fn load_package_at(&mut self, name: &ObjectName, position: Position) -> bool {
        self.register_package(name, Some(position))
    }

    /// Derive one immutable document registry by recording its package
    /// inclusions once in source order.
    pub fn with_imports(&self, imports: &[PackageImport]) -> Self {
        let mut registry = self.clone();
        for import in imports {
            registry.load_package_at(&import.package, import.effective_from);
        }
        registry
    }

    /// Derive the range-aware registry for one test source.
    #[cfg(test)]
    pub fn with_source_imports(&self, text: &str) -> Self {
        self.with_imports(&collect_imported_packages(text))
    }

    fn register_package(&mut self, name: &ObjectName, effective_from: Option<Position>) -> bool {
        let Some(package) = self.catalog.packages_by_name.get(name) else {
            return false;
        };
        if effective_from.is_none()
            && self.packages.iter().any(|registration| {
                registration.package == *package && registration.effective_from.is_none()
            })
        {
            return true;
        }
        self.packages.push(PackageRegistration {
            package: package.clone(),
            effective_from,
        });
        true
    }

    /// Resolve a loaded package's visible name to its package object identity.
    #[cfg(test)]
    pub fn package_id(&self, name: &ObjectName) -> Option<&ObjectId> {
        self.package_id_at(name, Position::new(u32::MAX, u32::MAX))
    }

    /// Resolve a package loaded at `position`.
    #[cfg(test)]
    pub fn package_id_at(&self, name: &ObjectName, position: Position) -> Option<&ObjectId> {
        let package = self.catalog.packages_by_name.get(name)?;
        self.packages
            .iter()
            .rev()
            .find(|registration| {
                &registration.package == package && registration.is_effective_at(position)
            })
            .map(|registration| &registration.package)
    }

    /// Borrow the visible name of a catalogued package object.
    pub fn package_name(&self, package: &ObjectId) -> Option<&str> {
        self.object(package).map(|record| record.name.name())
    }

    /// Packages loaded by a fresh Macaulay2 session.
    #[cfg(test)]
    pub fn default_loaded(&self) -> &[String] {
        &self.catalog.default_loaded
    }

    /// Iterate over loaded records with the most recently loaded package first.
    pub(crate) fn records_by_precedence(&self) -> impl Iterator<Item = &Record> {
        self.records_by_precedence_at(Position::new(u32::MAX, u32::MAX))
    }

    /// Iterate over records visible at `position`, latest package first.
    pub(crate) fn records_by_precedence_at(
        &self,
        position: Position,
    ) -> impl Iterator<Item = &Record> {
        let mut seen = HashSet::new();
        self.packages
            .iter()
            .rev()
            .filter(move |registration| registration.is_effective_at(position))
            .filter(move |registration| seen.insert(&registration.package))
            .filter_map(|registration| self.catalog.packages.get(&registration.package))
            .flat_map(|package| package.objects.iter())
            .filter_map(|object| self.catalog.index.object(object))
    }

    /// Borrow reverse option facts computed once from the complete corpus.
    pub fn option_facts(&self) -> &OptionFacts {
        &self.catalog.option_facts
    }

    pub(crate) fn package_objects(&self, package: &ObjectId) -> Option<&PackageObjects> {
        self.catalog.packages.get(package)
    }

    pub(crate) fn catalog_package_id(&self, name: &ObjectName) -> Option<&ObjectId> {
        self.catalog.packages_by_name.get(name)
    }

    /// Borrow every catalogued record for package-addressed static metadata.
    pub(crate) fn catalog_records(&self) -> &[Record] {
        self.catalog.index.records()
    }

    /// Borrow the registry state visible at one source position.
    pub fn at(&self, position: Position) -> ObjectRegistryView<'_> {
        ObjectRegistryView {
            registry: self,
            position,
        }
    }
}

impl PackageRegistration {
    fn is_effective_at(&self, position: Position) -> bool {
        self.effective_from
            .is_none_or(|effective_from| effective_from <= position)
    }
}

impl ObjectRegistryView<'_> {
    /// Iterate records visible here with the latest package inclusion first.
    pub(crate) fn records_by_precedence(&self) -> impl Iterator<Item = &Record> {
        self.registry.records_by_precedence_at(self.position)
    }

    /// Borrow the source-visible package name for an internal package identity.
    pub(crate) fn package_name(&self, package: &ObjectId) -> Option<&str> {
        self.registry.package_name(package)
    }

    /// Borrow corpus-wide option relationships; visibility is applied by the
    /// caller through this view's name resolution.
    pub(crate) fn option_facts(&self) -> &OptionFacts {
        self.registry.option_facts()
    }

    /// Whether `name` resolves through a package inclusion later than a source
    /// definition at `source_position`.
    pub(crate) fn shadows_source(&self, name: &ObjectName, source_position: Position) -> bool {
        self.registry
            .registration_for_name_at(name, self.position)
            .and_then(|(_, effective_from)| effective_from)
            .is_some_and(|effective_from| effective_from > source_position)
    }
}

/// Canonical symbol identity of one named Macaulay2 object.
///
/// Package records retain their qualified corpus symbol (for example
/// `$Core$Thing`). That qualification is internal to the linked corpus; source
/// lookup uses [`ObjectName`] spellings and aliases.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectId(ObjectName);

impl ObjectId {
    /// Construct an identity from a canonical symbol spelling.
    pub fn new(name: impl Into<String>) -> Self {
        Self(ObjectName(name.into()))
    }

    /// Return the canonical symbol spelling.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// Borrow this identity as its canonical object name.
    pub fn object_name(&self) -> &ObjectName {
        &self.0
    }
}

impl Borrow<ObjectName> for ObjectId {
    fn borrow(&self) -> &ObjectName {
        &self.0
    }
}

/// Identity of an object known to be a Macaulay2 type.
///
/// Construction stays inside the registry so arbitrary objects cannot be used
/// as type-order elements.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeId(ObjectId);

impl TypeId {
    /// Construct a type identity after the registry has classified the object.
    pub fn from_object(object: ObjectId) -> Self {
        Self(object)
    }

    /// Return this type's identity in the complete object population.
    pub fn object(&self) -> &ObjectId {
        &self.0
    }
}

/// Direct parent edge of one registered Macaulay2 type.
#[derive(Debug, Clone)]
pub struct TypeData {
    pub parent: TypeId,
}

/// Direct parent access required to navigate the type partial order.
pub trait TypeStore {
    /// Return the mandatory parent of `type_id`.
    fn parent_type_id(&self, type_id: &TypeId) -> Option<TypeId>;
}

/// A registry-backed type value.
///
/// The handle carries the registry needed to turn its stored parent
/// [`TypeId`] into another navigable `Type`.
#[derive(Clone)]
pub struct Type<'registry> {
    id: TypeId,
    store: &'registry dyn TypeStore,
}

impl<'registry> Type<'registry> {
    /// Bind a validated type identity to its owning registry view.
    pub fn new(id: TypeId, store: &'registry dyn TypeStore) -> Self {
        Self { id, store }
    }

    /// Return the canonical identity of this type.
    pub fn id(&self) -> &TypeId {
        &self.id
    }

    /// Return this type's mandatory parent.
    pub fn parent(&self) -> Self {
        let parent = self
            .store
            .parent_type_id(&self.id)
            .expect("a registered type must have a registered parent");
        Self::new(parent, self.store)
    }

    /// Whether this type is below `other` in the type partial order.
    pub fn is_subtype_of(&self, other: &Self) -> bool {
        if !ptr::eq(self.store, other.store) {
            return false;
        }

        let mut current = self.clone();
        let mut visited = HashSet::new();
        loop {
            if current.id == other.id {
                return true;
            }
            if !visited.insert(current.id.clone()) {
                return false;
            }
            let parent = current.parent();
            if parent.id == current.id {
                return false;
            }
            current = parent;
        }
    }
}

impl Debug for Type<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result {
        formatter.debug_tuple("Type").field(&self.id).finish()
    }
}

impl PartialEq for Type<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && ptr::eq(self.store, other.store)
    }
}

impl Eq for Type<'_> {}

impl PartialOrd for Type<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self == other {
            return Some(Ordering::Equal);
        }
        if self.is_subtype_of(other) {
            return Some(Ordering::Less);
        }
        if other.is_subtype_of(self) {
            return Some(Ordering::Greater);
        }
        None
    }
}

/// Identity and record lookup shared by every semantic object source.
pub trait ObjectKnowledge {
    /// Borrow the object with `object_id`.
    fn object(&self, object_id: &ObjectId) -> Option<&Record>;

    /// Resolve a canonical name or alias to its object identity.
    fn resolve_object(&self, name: &ObjectName) -> Option<ObjectId>;

    /// Resolve a nominal type reference to a registry-backed type value.
    fn resolve_type(&self, name: &ObjectName) -> Option<Type<'_>>;

    /// Resolve a canonical name or alias to its object record.
    fn get_record(&self, name: &ObjectName) -> Option<&Record> {
        let object = self.resolve_object(name)?;
        self.object(&object)
    }
}

impl ObjectRegistry {
    /// Borrow the record named by `name`, resolving aliases through the canonical
    /// names of loaded packages.
    pub fn get_record(&self, name: &ObjectName) -> Option<&Record> {
        let object = self.object_id(name)?;
        self.object(&object)
    }

    /// Borrow one canonical object by its opaque identity.
    pub fn object(&self, object_id: &ObjectId) -> Option<&Record> {
        self.catalog.index.object(object_id)
    }

    /// Resolve a canonical name or alias in loaded-package order.
    pub fn object_id(&self, name: &ObjectName) -> Option<ObjectId> {
        self.object_id_at(name, Position::new(u32::MAX, u32::MAX))
    }

    /// Resolve a name or alias using registrations effective at `position`.
    pub fn object_id_at(&self, name: &ObjectName, position: Position) -> Option<ObjectId> {
        self.registration_for_name_at(name, position)
            .map(|(object, _)| object)
    }

    fn registration_for_name_at(
        &self,
        name: &ObjectName,
        position: Position,
    ) -> Option<(ObjectId, Option<Position>)> {
        self.packages.iter().rev().find_map(|registration| {
            if !registration.is_effective_at(position) {
                return None;
            }
            let object = self
                .catalog
                .packages
                .get(&registration.package)?
                .objects_by_name
                .get(name)
                .cloned()?;
            Some((object, registration.effective_from))
        })
    }

    /// Resolve a nominal type reference to a typed registry handle.
    pub fn resolve_type(&self, name: &ObjectName) -> Option<Type<'_>> {
        let object = self.object_id(name)?;
        let record = self.object(&object)?;
        record.type_info()?;
        Some(Type::new(TypeId::from_object(object), self))
    }
}

impl ObjectKnowledge for ObjectRegistry {
    fn object(&self, object_id: &ObjectId) -> Option<&Record> {
        ObjectRegistry::object(self, object_id)
    }

    fn resolve_object(&self, name: &ObjectName) -> Option<ObjectId> {
        ObjectRegistry::object_id(self, name)
    }

    fn resolve_type(&self, name: &ObjectName) -> Option<Type<'_>> {
        ObjectRegistry::resolve_type(self, name)
    }
}

impl ObjectKnowledge for ObjectRegistryView<'_> {
    fn object(&self, object_id: &ObjectId) -> Option<&Record> {
        self.registry.object(object_id)
    }

    fn resolve_object(&self, name: &ObjectName) -> Option<ObjectId> {
        self.registry.object_id_at(name, self.position)
    }

    fn resolve_type(&self, name: &ObjectName) -> Option<Type<'_>> {
        let object = self.resolve_object(name)?;
        self.object(&object)?.type_info()?;
        Some(Type::new(TypeId::from_object(object), self))
    }
}

impl TypeStore for ObjectRegistry {
    fn parent_type_id(&self, type_id: &TypeId) -> Option<TypeId> {
        self.object(type_id.object())?
            .type_info()
            .map(|data| data.parent.clone())
    }
}

impl TypeStore for ObjectRegistryView<'_> {
    fn parent_type_id(&self, type_id: &TypeId) -> Option<TypeId> {
        self.object(type_id.object())?
            .type_info()
            .map(|data| data.parent.clone())
    }
}

impl<T: ObjectKnowledge + ?Sized> ObjectKnowledge for &T {
    fn object(&self, object_id: &ObjectId) -> Option<&Record> {
        T::object(self, object_id)
    }

    fn resolve_object(&self, name: &ObjectName) -> Option<ObjectId> {
        T::resolve_object(self, name)
    }

    fn resolve_type(&self, name: &ObjectName) -> Option<Type<'_>> {
        T::resolve_type(self, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let registry = ObjectRegistry::load(corpus());
        let scoped = registry.with_source_imports("needsPackage \"NoSuchPkg\"");
        assert!(scoped.get_record(&ObjectName::new("ZZ")).is_some());
    }
}
