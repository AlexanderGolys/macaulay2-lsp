//! Canonical in-memory representation of `data/m2-index.jsonl`.
//!
//! Every builtin is stored once as a [`Record`]. Type, callable, operator, and
//! option data are capabilities of that record rather than separate object
//! populations. Name, alias, and package lookup are indexes over this one
//! population. JSONL and Serde details live in the private `corpus` submodule.

mod corpus;

use std::mem;
use std::{collections::HashMap, iter};

use crate::object_registry::{ObjectId, ObjectName, OperatorForm, TypeData, TypeId};
use corpus::{deserialize_records, RawOperatorForm, RawOptionSpec, RawRecord};

/// The canonical metadata for one builtin M2 object.
#[derive(Debug, Clone)]
pub struct Record {
    pub id: ObjectId,
    pub name: ObjectName,
    pub class: ObjectName,
    pub package: ObjectId,
    pub data: ObjectData,
    pub protected: bool,
    aliases: Vec<String>,
    markdown: Option<String>,
}

/// The mutually exclusive semantic shape of one indexed M2 object.
#[derive(Debug, Clone)]
pub enum ObjectData {
    Plain,
    Type(TypeData),
    Callable(CallableInfo),
}

/// Callable metadata derived from the callable's corpus record.
#[derive(Debug, Clone)]
pub struct CallableInfo {
    pub kind: CallableKind,
    pub typical_value: Option<TypeId>,
    pub methods: Vec<MethodSignature>,
    pub options: Vec<CallableOption>,
    pub receives_sequence: bool,
}

/// The callable form encoded by the corpus, with operator metadata nested under
/// the callable it specializes.
#[derive(Debug, Clone)]
pub enum CallableKind {
    Function,
    MethodFunction,
    Operator(OperatorInfo),
}

impl Record {
    pub fn callable(&self) -> Option<&CallableInfo> {
        match &self.data {
            ObjectData::Callable(callable) => Some(callable),
            ObjectData::Plain | ObjectData::Type(_) => None,
        }
    }

    pub fn type_info(&self) -> Option<&TypeData> {
        match &self.data {
            ObjectData::Type(type_info) => Some(type_info),
            ObjectData::Plain | ObjectData::Callable(_) => None,
        }
    }

    pub fn operator_info(&self) -> Option<&OperatorInfo> {
        self.callable()?.operator_info()
    }

    pub fn lookup_names(&self) -> impl Iterator<Item = ObjectName> + '_ {
        iter::once(self.name.clone()).chain(self.aliases.iter().map(ObjectName::new))
    }

    pub fn markdown(&self) -> Option<&str> {
        self.markdown.as_deref()
    }
}

impl CallableInfo {
    pub fn is_method_function(&self) -> bool {
        matches!(self.kind, CallableKind::MethodFunction)
    }

    pub fn operator_info(&self) -> Option<&OperatorInfo> {
        match &self.kind {
            CallableKind::Operator(operator) => Some(operator),
            CallableKind::Function | CallableKind::MethodFunction => None,
        }
    }
}

/// One option key and its structured value constraints.
#[derive(Debug, Clone)]
pub struct CallableOption {
    pub name: ObjectName,
    pub possible_values: Vec<ObjectName>,
}

/// Reverse option relationships indexed from the canonical callable records.
#[derive(Debug, Clone, Default)]
pub struct OptionFacts {
    pub option_value_usages: HashMap<ObjectName, Vec<OptionValueUsage>>,
    pub option_values_by_slot: HashMap<OptionSlot, Vec<ObjectName>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OptionSlot {
    pub callable: ObjectName,
    pub option: ObjectName,
}

/// A callable/option slot that admits a particular indexed option value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionValueUsage {
    pub callable: ObjectName,
    pub option: ObjectName,
}

impl OptionFacts {
    pub fn from_records(records: &[Record]) -> Self {
        let mut facts = OptionFacts::default();
        for record in records {
            let Some(callable) = record.callable() else {
                continue;
            };
            let callable_id = record.name.clone();
            for option in &callable.options {
                let option_id = option.name.clone();
                let slot = OptionSlot {
                    callable: callable_id.clone(),
                    option: option_id.clone(),
                };
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
}

/// Resolved dispatch domain and optional return type of one indexed method.
///
/// Domains use general object identities because M2 supports singleton dispatch
/// objects as well as types. Codomains are validated type identities.
#[derive(Debug, Clone)]
pub struct MethodSignature {
    pub domain: Vec<ObjectId>,
    pub codomain: Option<TypeId>,
}

/// Type references retained only until every corpus object has been indexed.
struct PendingCallableTypes {
    callable: ObjectId,
    typical_value: Option<ObjectName>,
    methods: Vec<PendingMethodTypes>,
}

struct PendingMethodTypes {
    domain: Vec<ObjectName>,
    codomain: Option<ObjectName>,
}

/// Parser and runtime metadata for an operator-backed callable.
#[derive(Debug, Clone)]
pub struct OperatorInfo {
    pub method_symbol: ObjectName,
    pub forms: Vec<String>,
    pub form_attributes: HashMap<OperatorForm, Vec<String>>,
}

/// The operator attribute marking a form as accepting runtime method
/// installation (`X op Y := …`).
const FLEXIBLE_ATTRIBUTE: &str = "Flexible";

impl OperatorInfo {
    pub fn is_flexible(&self, form: OperatorForm) -> bool {
        self.form_attributes
            .get(&form)
            .is_some_and(|attributes| attributes.iter().any(|a| a == FLEXIBLE_ATTRIBUTE))
    }
}

/// Canonical record population plus name/alias lookup and corpus-global metadata.
#[derive(Debug, Clone, Default)]
pub struct BuiltinIndex {
    records: Vec<Record>,
    record_index_by_id: HashMap<ObjectId, usize>,
    record_index_by_name: HashMap<ObjectName, ObjectId>,
    opaque_types: HashMap<ObjectName, TypeId>,
    default_loaded: Vec<String>,
}

impl BuiltinIndex {
    pub fn load(corpus: &str) -> Self {
        let mut index = BuiltinIndex::default();
        let mut pending_type_parents = Vec::new();
        let mut pending_callable_types = Vec::new();
        let mut referenced_packages = HashMap::new();
        for mut raw in deserialize_records(corpus) {
            if raw.kind != "meta" {
                let package = package_object_id(&raw);
                referenced_packages
                    .entry(package)
                    .or_insert_with(|| package_object_name(&raw));
            }

            // name + aliases + extra_keys all resolve to this record.
            let mut keys = raw.aliases.clone();
            keys.extend(raw.extra_keys.iter().cloned());

            match raw.kind.as_str() {
                "type" => {
                    let object_id = canonical_object_id(&raw);
                    let parent = raw
                        .parent
                        .as_deref()
                        .map(ObjectName::new)
                        .unwrap_or_else(|| ObjectName::new(object_id.name()));
                    let mut record = base_record(object_id.clone(), &mut raw, keys, "Type");
                    record.data = ObjectData::Type(TypeData { parent: None });
                    index.insert(record);
                    pending_type_parents.push((object_id, parent));
                }
                "function" | "methodFunction" | "operator" => {
                    let object_id = canonical_object_id(&raw);
                    let is_operator = raw.kind == "operator";
                    let default_class = if is_operator { "Keyword" } else { "Function" };
                    let mut record = base_record(object_id, &mut raw, keys, default_class);
                    let methods = raw
                        .methods
                        .into_iter()
                        .map(|method| PendingMethodTypes {
                            domain: method.domain.into_iter().map(ObjectName).collect(),
                            codomain: concrete_type_reference(method.typical_value.as_deref()),
                        })
                        .collect();
                    let kind = match raw.kind.as_str() {
                        "function" => CallableKind::Function,
                        "methodFunction" => CallableKind::MethodFunction,
                        "operator" => {
                            let operator = raw.operator.take().unwrap_or_else(|| {
                                panic!("operator record '{}' has no operator metadata", record.name)
                            });
                            CallableKind::Operator(OperatorInfo {
                                method_symbol: record.name.clone(),
                                forms: operator
                                    .forms
                                    .iter()
                                    .map(|form| capitalize_form(form))
                                    .collect(),
                                form_attributes: operator
                                    .attributes
                                    .into_iter()
                                    .map(|(form, attributes)| (form.into(), attributes))
                                    .collect(),
                            })
                        }
                        _ => unreachable!("callable branch only accepts callable record kinds"),
                    };
                    record.data = ObjectData::Callable(CallableInfo {
                        kind,
                        typical_value: None,
                        methods: Vec::new(),
                        options: raw.options.into_iter().map(CallableOption::from).collect(),
                        receives_sequence: record.class == ObjectName::new("MethodFunctionSingle"),
                    });
                    pending_callable_types.push(PendingCallableTypes {
                        callable: record.id.clone(),
                        typical_value: concrete_type_reference(raw.typical_value.as_deref()),
                        methods,
                    });
                    index.insert(record);
                }
                "symbol" | "object" | "table" | "package" => {
                    let object_id = canonical_object_id(&raw);
                    let default_class = if raw.kind == "package" {
                        "Package"
                    } else {
                        "Thing"
                    };
                    let record = base_record(object_id, &mut raw, keys, default_class);
                    index.insert(record);
                }
                "meta" => {
                    // Baseline of fresh-start loaded packages; bare package names.
                    index.default_loaded = raw.default_loaded;
                }
                // `package` records carry no per-symbol typecheck facts.
                _ => {}
            }
        }
        for (child, parent_name) in pending_type_parents {
            let parent = index.resolve_or_insert_type_reference(&parent_name);
            assert!(
                index
                    .object(parent.object())
                    .is_some_and(|record| record.type_info().is_some()),
                "type parent '{parent_name}' is not a type"
            );
            let record_index = index.record_index_by_id[&child];
            let type_info = index.records[record_index]
                .type_info_mut()
                .expect("pending type parent must belong to a type record");
            type_info.parent = Some(parent);
        }
        for pending in &pending_callable_types {
            if let Some(reference) = &pending.typical_value {
                index.resolve_or_insert_type_reference(reference);
            }
            for method in &pending.methods {
                if let Some(reference) = &method.codomain {
                    index.resolve_or_insert_type_reference(reference);
                }
            }
        }
        for pending in pending_callable_types {
            let typical_value = pending
                .typical_value
                .as_ref()
                .map(|reference| index.resolve_or_insert_type_reference(reference));
            let methods = pending
                .methods
                .into_iter()
                .map(|method| MethodSignature {
                    domain: method
                        .domain
                        .iter()
                        .map(|reference| index.resolve_or_insert_dispatch_reference(reference))
                        .collect(),
                    codomain: method
                        .codomain
                        .as_ref()
                        .map(|reference| index.resolve_or_insert_type_reference(reference)),
                })
                .collect();
            let callable = index
                .record_mut(&pending.callable)
                .and_then(Record::callable_mut)
                .expect("pending callable types must belong to a callable record");
            callable.typical_value = typical_value;
            callable.methods = methods;
        }
        for record in index.records() {
            referenced_packages
                .entry(record.package.clone())
                .or_insert_with(|| package_name_from_id(&record.package));
        }
        for (package, name) in referenced_packages {
            if index.object(&package).is_some() {
                continue;
            }
            index.insert(Record {
                id: package.clone(),
                name,
                class: ObjectName::new("Package"),
                package: package.clone(),
                data: ObjectData::Plain,
                protected: true,
                aliases: Vec::new(),
                markdown: None,
            });
        }
        index
    }

    fn insert(&mut self, record: Record) {
        self.insert_with_name_lookup(record, true);
    }

    fn insert_without_name_lookup(&mut self, record: Record) {
        self.insert_with_name_lookup(record, false);
    }

    fn insert_with_name_lookup(&mut self, record: Record, include_names: bool) {
        let object_id = record.id.clone();
        let record_index = self.records.len();
        let previous = self
            .record_index_by_id
            .insert(object_id.clone(), record_index);
        assert!(
            previous.is_none(),
            "object ID {object_id:?} inserted more than once"
        );
        if include_names {
            self.record_index_by_name
                .insert(ObjectName::new(object_id.name()), object_id.clone());
            self.record_index_by_name
                .insert(record.name.clone(), object_id.clone());
            for alias in &record.aliases {
                self.record_index_by_name
                    .entry(ObjectName::new(alias))
                    .or_insert_with(|| object_id.clone());
            }
        }
        self.records.push(record);
    }

    fn resolve_reference(&self, reference: &ObjectName) -> Option<ObjectId> {
        if reference.name().starts_with('$') {
            let canonical = ObjectId::new(reference.name());
            return self
                .record_index_by_id
                .contains_key(&canonical)
                .then_some(canonical);
        }
        self.object_id(reference)
    }

    fn resolve_or_insert_type_reference(&mut self, reference: &ObjectName) -> TypeId {
        if let Some(type_id) = self.opaque_types.get(reference) {
            return type_id.clone();
        }
        if let Some(object) = self.resolve_reference(reference) {
            if let Some(type_id) = self.type_id(&object) {
                return type_id;
            }
            return self.insert_opaque_type_reference(reference);
        }

        let id = if reference.name().starts_with('$') {
            ObjectId::new(reference.name())
        } else {
            ObjectId::new(format!("$Unresolved${}", reference.name()))
        };
        let display_name = deref_ref(reference.name());
        let package = reference
            .name()
            .strip_prefix('$')
            .and_then(|rest| rest.split_once('$'))
            .map_or_else(unresolved_package_id, |(package, _)| {
                ObjectId::new(format!("${package}${package}"))
            });
        self.insert(Record {
            id: id.clone(),
            name: ObjectName(display_name),
            class: ObjectName::new("Type"),
            package,
            data: ObjectData::Type(TypeData { parent: None }),
            protected: true,
            aliases: Vec::new(),
            markdown: None,
        });
        let type_id = self
            .type_id(&id)
            .expect("inserted placeholders are registered types");
        self.record_mut(&id)
            .and_then(Record::type_info_mut)
            .expect("inserted placeholder has type data")
            .parent = Some(type_id.clone());
        type_id
    }

    fn insert_opaque_type_reference(&mut self, reference: &ObjectName) -> TypeId {
        let id = ObjectId::new(format!(
            "$Unresolved${}",
            reference.name().trim_start_matches('$')
        ));
        self.insert_without_name_lookup(Record {
            id: id.clone(),
            name: ObjectName::new(deref_ref(reference.name())),
            class: ObjectName::new("Type"),
            package: unresolved_package_id(),
            data: ObjectData::Type(TypeData { parent: None }),
            protected: true,
            aliases: Vec::new(),
            markdown: None,
        });
        let type_id = self
            .type_id(&id)
            .expect("opaque placeholders are registered types");
        self.record_mut(&id)
            .and_then(Record::type_info_mut)
            .expect("opaque placeholder has type data")
            .parent = Some(type_id.clone());
        self.opaque_types.insert(reference.clone(), type_id.clone());
        type_id
    }

    fn resolve_or_insert_dispatch_reference(&mut self, reference: &ObjectName) -> ObjectId {
        if let Some(type_id) = self.opaque_types.get(reference) {
            return type_id.object().clone();
        }
        self.resolve_reference(reference).unwrap_or_else(|| {
            self.resolve_or_insert_type_reference(reference)
                .object()
                .clone()
        })
    }

    fn record_mut(&mut self, object_id: &ObjectId) -> Option<&mut Record> {
        let index = *self.record_index_by_id.get(object_id)?;
        self.records.get_mut(index)
    }

    pub fn object(&self, object_id: &ObjectId) -> Option<&Record> {
        self.record_index_by_id
            .get(object_id)
            .and_then(|index| self.records.get(*index))
    }

    pub fn object_id(&self, name: &ObjectName) -> Option<ObjectId> {
        self.record_index_by_name.get(name).cloned()
    }

    #[cfg(test)]
    pub fn record(&self, name: &ObjectName) -> Option<&Record> {
        let object = self.object_id(name)?;
        self.object(&object)
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn default_loaded_packages(&self) -> &[String] {
        &self.default_loaded
    }
}

/// Construct the canonical package-qualified symbol identity of a raw record.
fn canonical_object_id(raw: &RawRecord) -> ObjectId {
    let canonical_name = if raw.normalized_name.is_empty() {
        raw.name.as_str()
    } else {
        raw.normalized_name.as_str()
    };
    let package = raw
        .package
        .as_deref()
        .map(deref_ref)
        .unwrap_or_else(|| "Core".to_string());
    ObjectId::new(format!("${package}${canonical_name}"))
}

fn package_object_id(raw: &RawRecord) -> ObjectId {
    raw.package
        .as_deref()
        .map_or_else(core_package_id, ObjectId::new)
}

fn package_object_name(raw: &RawRecord) -> ObjectName {
    raw.package
        .as_deref()
        .map(deref_ref)
        .map(ObjectName)
        .unwrap_or_else(|| ObjectName::new("Core"))
}

fn core_package_id() -> ObjectId {
    ObjectId::new("$Core$Core")
}

fn unresolved_package_id() -> ObjectId {
    ObjectId::new("$Unresolved$Unresolved")
}

fn package_name_from_id(package: &ObjectId) -> ObjectName {
    ObjectName::new(deref_ref(package.name()))
}

impl Record {
    fn callable_mut(&mut self) -> Option<&mut CallableInfo> {
        match &mut self.data {
            ObjectData::Callable(callable) => Some(callable),
            ObjectData::Plain | ObjectData::Type(_) => None,
        }
    }

    fn type_info_mut(&mut self) -> Option<&mut TypeData> {
        match &mut self.data {
            ObjectData::Type(type_info) => Some(type_info),
            ObjectData::Plain | ObjectData::Callable(_) => None,
        }
    }
}

fn base_record(
    id: ObjectId,
    raw: &mut RawRecord,
    aliases: Vec<String>,
    default_class: &str,
) -> Record {
    Record {
        id,
        name: ObjectName(mem::take(&mut raw.name)),
        class: ObjectName(
            raw.class
                .take()
                .map(|class| deref_ref(&class))
                .unwrap_or_else(|| default_class.to_string()),
        ),
        package: package_object_id(raw),
        data: ObjectData::Plain,
        protected: raw.protected.unwrap_or(true),
        aliases,
        markdown: raw.markdown.take().filter(|markdown| !markdown.is_empty()),
    }
}

/// Dereference a `$Package$Name` corpus reference key to the bare name the type
/// system keys on (`$Core$ZZ` → `ZZ`). A key with no `$` prefix is an
/// unresolved/cross-package target and passes through unchanged. Normalized
/// names are package-free, so splitting on the first `$` after the leading one
/// is unambiguous.
fn deref_ref(key: &str) -> String {
    key.strip_prefix('$')
        .and_then(|rest| rest.split_once('$'))
        .map(|(_package, name)| name.to_string())
        .unwrap_or_else(|| key.to_string())
}

/// Dereference a corpus codomain key and enforce the monotone index invariant:
/// `Thing` and `Any` sit at the top of the M2 type lattice and carry no
/// typecheck information — returning them as a positive fact would pollute
/// inference. This maps them to `None` ("unknown"), preserving the
/// known-facts-only contract.
fn concrete_type_reference(raw_key: Option<&str>) -> Option<ObjectName> {
    let reference = raw_key.map(ObjectName::new)?;
    match deref_ref(reference.name()).as_str() {
        "Thing" | "Any" => None,
        _ => Some(reference),
    }
}

/// Wire-format optional argument metadata. Conversion into `CallableOption` keeps
/// Serde details at the JSONL boundary.
impl From<RawOptionSpec> for CallableOption {
    fn from(raw: RawOptionSpec) -> Self {
        CallableOption {
            name: ObjectName::new(&raw.key),
            possible_values: raw.possible_values.into_iter().map(ObjectName).collect(),
        }
    }
}

/// Corpus spelling of an operator form, converted at the loading boundary.
impl From<RawOperatorForm> for OperatorForm {
    fn from(form: RawOperatorForm) -> Self {
        match form {
            RawOperatorForm::Binary => Self::Binary,
            RawOperatorForm::Prefix => Self::Prefix,
            RawOperatorForm::Postfix => Self::Postfix,
            RawOperatorForm::Assignment => Self::Assignment,
        }
    }
}

/// `binary` → `Binary`, etc. The corpus uses lowercase operator forms; the LSP
/// keeps the capitalized vocabulary its operator-hover code matches on.
fn capitalize_form(form: &str) -> String {
    let mut chars = form.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> BuiltinIndex {
        BuiltinIndex::load(include_str!("./data/m2-index.jsonl"))
    }

    fn type_entry<'index>(index: &'index BuiltinIndex, name: &str) -> Option<&'index Record> {
        index
            .record(&ObjectName::new(name))
            .filter(|record| record.type_info().is_some())
    }

    fn callable<'index>(index: &'index BuiltinIndex, name: &str) -> Option<&'index Record> {
        index
            .record(&ObjectName::new(name))
            .filter(|record| record.callable().is_some())
    }

    fn type_id(index: &BuiltinIndex, name: &str) -> TypeId {
        index
            .type_id(&type_entry(index, name).expect("type should be indexed").id)
            .expect("type records produce validated identities")
    }

    fn type_name<'index>(index: &'index BuiltinIndex, type_id: &TypeId) -> &'index str {
        index
            .object(type_id.object())
            .expect("type identity should resolve")
            .name
            .name()
    }

    #[test]
    fn type_slots_preserve_opaque_identity_when_a_reference_names_a_symbol() {
        let corpus = concat!(
            r#"{"kind":"meta","default_loaded":["Core"]}"#,
            "\n",
            r#"{"kind":"symbol","name":"Graph","package":"$Foo$Foo"}"#,
            "\n",
            r#"{"kind":"function","name":"f","package":"$Core$Core","typical_value":"$Foo$Graph","methods":[{"domain":["$Foo$Graph"],"typicalValue":"$Foo$Graph"}]}"#,
        );
        let index = BuiltinIndex::load(corpus);
        let graph = index
            .object_id(&ObjectName::new("$Foo$Graph"))
            .expect("qualified symbol is indexed");
        let callable = callable(&index, "f")
            .and_then(Record::callable)
            .expect("function has callable facts");
        let opaque = callable
            .typical_value
            .as_ref()
            .expect("function has a typical type");

        assert_ne!(opaque.object(), &graph);
        assert_eq!(callable.methods[0].domain, [opaque.object().clone()]);
        assert_eq!(callable.methods[0].codomain.as_ref(), Some(opaque));
        assert_eq!(index.object_id(&ObjectName::new("Graph")), Some(graph));
        assert_eq!(
            index.object(opaque.object()).map(|record| &record.package),
            Some(&unresolved_package_id())
        );
    }

    #[test]
    fn load_parses_new_format_corpus() {
        let index = BuiltinIndex::load(include_str!("./data/m2-index.jsonl"));

        // A type record carries one canonical, typed parent edge.
        let zz = type_entry(&index, "ZZ").expect("ZZ type present");
        assert_eq!(zz.package.name(), "$Core$Core");
        assert_eq!(
            index.object(&zz.package).map(|package| package.name.name()),
            Some("Core")
        );
        let parent = zz
            .type_info()
            .expect("ZZ type facts")
            .parent
            .as_ref()
            .expect("ZZ parent");
        assert!(index.object(parent.object()).is_some());
        // markdown is now folded onto the entry (documented Core type).
        assert!(
            zz.markdown().is_some(),
            "ZZ should carry folded hover markdown"
        );

        // methodFunction record -> callable, with a deref'd codomain
        let beta = callable(&index, "Beta").expect("Beta callable present");
        let beta_info = beta.callable().expect("Beta callable facts");
        assert!(beta_info.is_method_function());
        assert!(beta_info.methods.iter().any(|method| method
            .codomain
            .as_ref()
            .map(|id| type_name(&index, id))
            == Some("RR")));

        // operator record -> callable + capitalized forms from the `operator` object
        let minus = callable(&index, "-").expect("- operator present");
        let minus_operator = minus.operator_info().expect("- operator facts");
        assert!(minus_operator.forms.contains(&"Binary".to_string()));
        assert!(minus_operator.forms.contains(&"Prefix".to_string()));

        let method_constructor = callable(&index, "method").expect("method function present");
        assert!(!method_constructor
            .callable()
            .expect("method callable facts")
            .is_method_function());
    }

    #[test]
    fn loads_types_and_callables() {
        let index = index();
        let type_count = index
            .records()
            .iter()
            .filter(|record| record.type_info().is_some())
            .count();
        let callable_count = index
            .records()
            .iter()
            .filter(|record| record.callable().is_some())
            .count();
        assert!(type_count > 100, "type lattice should be populated");
        assert!(callable_count > 500, "callables should be populated");
    }

    #[test]
    fn looks_up_callables_by_alias() {
        let index = index();
        // `gb` is reachable by its package-qualified alias too.
        assert!(callable(&index, "Core$gb").is_some());
        assert_eq!(
            callable(&index, "Core$gb").map(|record| record.name.0.as_str()),
            Some("gb")
        );
        let canonical_id = index
            .object_id(&ObjectName::new("gb"))
            .expect("gb has an object identity");
        assert_eq!(
            index.object_id(&ObjectName::new("Core$gb")),
            Some(canonical_id.clone()),
            "an alias resolves to the canonical object's identity"
        );
        assert_eq!(
            index.object(&canonical_id).map(|record| &record.name),
            Some(&ObjectName::new("gb"))
        );
    }

    #[test]
    fn parses_callable_signatures() {
        let index = index();
        let gb = callable(&index, "gb").expect("gb is a callable");
        assert!(gb.operator_info().is_none());
        // gb dispatches on Ideal/Module/Matrix, all returning a GroebnerBasis;
        // subtype matching uses canonical type records, while codomains stay on methods.
        assert!(gb
            .callable()
            .expect("gb callable facts")
            .methods
            .iter()
            .any(|method| {
                method.domain == [type_id(&index, "Ideal").object().clone()]
                    && method.codomain.as_ref().map(|id| type_name(&index, id))
                        == Some("GroebnerBasis")
            }));
    }

    #[test]
    fn operator_flexibility_is_per_form() {
        let index = index();

        let greater = index
            .record(&ObjectName::new(">"))
            .and_then(Record::operator_info)
            .expect("> operator should carry operator info");
        assert!(greater.is_flexible(OperatorForm::Prefix));
        assert!(!greater.is_flexible(OperatorForm::Binary));

        let minus = index
            .record(&ObjectName::new("-"))
            .and_then(Record::operator_info)
            .expect("- operator should carry operator info");
        assert!(minus.is_flexible(OperatorForm::Binary));
        assert!(minus.is_flexible(OperatorForm::Prefix));
    }

    #[test]
    fn parses_type_lattice_edges() {
        let index = index();
        // Every canonical type record carries one resolvable parent edge.
        if let Some(zz) = type_entry(&index, "ZZ") {
            let parent = zz
                .type_info()
                .expect("ZZ type facts")
                .parent
                .as_ref()
                .expect("ZZ parent");
            assert!(index.object(parent.object()).is_some());
        }
    }

    #[test]
    fn deref_ref_strips_package_qualifier_and_passes_bare_names_through() {
        assert_eq!(deref_ref("$Core$ZZ"), "ZZ");
        assert_eq!(deref_ref("$Core$RingElement"), "RingElement");
        assert_eq!(deref_ref("$Core$Core"), "Core"); // package/class refs too
        assert_eq!(deref_ref("ComplexMap"), "ComplexMap"); // unresolved, no prefix
        assert_eq!(deref_ref("RingElement"), "RingElement"); // already bare
    }

    #[test]
    fn no_callable_carries_thing_or_any_codomain() {
        let index = index();

        // Global invariant: no signature codomain and no callable typical_value
        // may be Some("Thing") or Some("Any").
        for callable in index
            .records()
            .iter()
            .filter(|record| record.callable().is_some())
        {
            for method in &callable.callable().expect("callable facts").methods {
                assert!(
                    !matches!(
                        method.codomain.as_ref().map(|id| type_name(&index, id)),
                        Some("Thing") | Some("Any")
                    ),
                    "callable '{}' has a Thing/Any signature codomain (domain={:?})",
                    callable.name,
                    &method.domain,
                );
            }
            assert!(
                !matches!(
                    callable
                        .callable()
                        .and_then(|info| info.typical_value.as_ref())
                        .map(|id| type_name(&index, id)),
                    Some("Thing") | Some("Any")
                ),
                "callable '{}' has a Thing/Any typical_value",
                callable.name,
            );
        }

        // Spot-check: `next` over domain `["Iterator"]` — raw corpus has
        // `$Core$Thing` — must be dropped to None after load.
        let next = callable(&index, "next").expect("next callable present");
        let next_iter_sig = next
            .callable()
            .expect("next callable facts")
            .methods
            .iter()
            .find(|method| method.domain == [type_id(&index, "Iterator").object().clone()])
            .expect("next(Iterator) signature present");
        assert_eq!(
            next_iter_sig.codomain, None,
            "next(Iterator) codomain must be None, not Thing"
        );
        assert_eq!(
            next.callable().expect("next callable facts").typical_value,
            None,
            "next typical_value must be None, not Thing"
        );
    }

    #[test]
    fn captures_default_loaded_from_meta_record() {
        let corpus = concat!(
            r#"{"kind":"meta","default_loaded":["Core","Classic","Polyhedra"]}"#,
            "\n",
            r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#,
        );
        let index = BuiltinIndex::load(corpus);
        assert_eq!(
            index.default_loaded_packages(),
            &["Core", "Classic", "Polyhedra"]
        );
        assert!(
            type_entry(&index, "ZZ").is_some(),
            "non-meta records still load"
        );
    }

    #[test]
    fn default_loaded_is_empty_without_meta_record() {
        let corpus = r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#;
        let index = BuiltinIndex::load(corpus);
        assert!(index.default_loaded_packages().is_empty());
    }

    #[test]
    #[should_panic(expected = "has no name")]
    fn load_panics_on_unnamed_non_meta_record() {
        BuiltinIndex::load(r#"{"kind":"type","package":"$Core$Core"}"#);
    }
}
