//! Canonical in-memory representation of `data/m2-index.jsonl`.
//!
//! Every builtin is stored once as a [`Record`]. Type, callable, operator, and
//! option data are capabilities of that record rather than separate object
//! populations. Name, alias, and package lookup are indexes over this one
//! population. The private `Raw*` types below are only the Serde boundary.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt::{Display, Formatter, Result};

use serde::Deserialize;

/// Stable identifier for an indexed M2 object or type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceID(pub String);

impl InstanceID {
    /// Construct an identifier from an unqualified or package-qualified name.
    pub fn new(name: &str) -> Self {
        InstanceID(name.to_string())
    }
}

impl Display for InstanceID {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
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

/// One executable example attached to a corpus record or method signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeExample(pub String);

/// The canonical metadata for one builtin M2 object.
#[derive(Debug, Clone)]
pub struct Record {
    pub name: InstanceID,
    pub class: InstanceID,
    pub examples: Vec<CodeExample>,
    pub package: Option<String>,
    pub source_file: Option<String>,
    pub typical_value: Option<String>,
    pub function_info: Option<FunctionInfo>,
    pub option_info: Option<OptionInfo>,
    pub operator_info: Option<OperatorInfo>,
    pub type_info: Option<TypeInfo>,
    pub protected: Option<bool>,
    aliases: Vec<String>,
    pub(crate) markdown: Option<String>,
}

/// Callable metadata derived from the callable's corpus record.
#[derive(Debug, Clone)]
pub struct FunctionInfo {
    /// Whether the indexed callable accepts runtime method installations.
    /// This comes directly from the corpus record kind, not its runtime class.
    pub is_method_function: bool,
    pub methods: Vec<MethodSignature>,
}

/// The documented options accepted by a callable.
#[derive(Debug, Clone)]
pub struct OptionInfo {
    pub options: Vec<MethodOption>,
}

/// One option key and its structured value constraints.
#[derive(Debug, Clone)]
pub struct MethodOption {
    pub name: InstanceID,
    pub(crate) possible_values: Vec<InstanceID>,
}

/// Reverse option relationships indexed from the canonical callable records.
#[derive(Debug, Clone, Default)]
pub(crate) struct OptionFacts {
    pub(crate) option_value_usages: HashMap<InstanceID, Vec<OptionValueUsage>>,
    pub(crate) option_values_by_slot: HashMap<OptionSlot, Vec<InstanceID>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OptionSlot {
    pub(crate) callable: InstanceID,
    pub(crate) option: InstanceID,
}

/// A callable/option slot that admits a particular indexed option value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OptionValueUsage {
    pub(crate) callable: InstanceID,
    pub(crate) option: InstanceID,
}

impl OptionFacts {
    fn from_records(records: &[Record]) -> Self {
        let mut facts = OptionFacts::default();
        for record in records {
            if record.function_info.is_none() {
                continue;
            }
            let callable_id = record.name.clone();
            for option in record
                .option_info
                .iter()
                .flat_map(|option_info| &option_info.options)
            {
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

/// An installed method domain: callable name followed by its argument types.
#[derive(Debug, Clone)]
pub struct MethodSignature {
    pub signature: Vec<InstanceID>,
    pub(crate) codomain: Option<InstanceID>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub struct OperatorForm(String);

impl Borrow<str> for OperatorForm {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Parser and runtime metadata for an operator-backed callable.
#[derive(Debug, Clone)]
pub struct OperatorInfo {
    pub method_symbol: InstanceID,
    pub forms: Vec<String>,
    pub form_attributes: HashMap<OperatorForm, Vec<String>>,
}

/// The operator attribute marking a form as accepting runtime method
/// installation (`X op Y := …`).
const FLEXIBLE_ATTRIBUTE: &str = "Flexible";

impl OperatorInfo {
    /// Whether this operator accepts a method installed on the given form
    /// (`"binary"`/`"prefix"`/`"postfix"`) — i.e. that form is `Flexible`.
    pub fn is_flexible(&self, form: &str) -> bool {
        self.form_attributes
            .get(form)
            .is_some_and(|attributes| attributes.iter().any(|a| a == FLEXIBLE_ATTRIBUTE))
    }
}

/// Direct hierarchy facts for an indexed M2 type.
#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub subtypes: Vec<InstanceID>,
    pub parent_type: Option<InstanceID>,
    pub(crate) ancestors: Vec<InstanceID>,
}

/// Canonical record population plus name/alias lookup and corpus-global metadata.
#[derive(Debug, Clone, Default)]
pub struct BuiltinIndex {
    records: Vec<Record>,
    record_index_by_name: HashMap<InstanceID, usize>,
    default_loaded: Vec<String>,
}

/// Builtin records together with the derived typechecking facts computed from
/// that same population.
#[derive(Debug, Clone)]
pub struct BuiltinData {
    pub(crate) index: BuiltinIndex,
    pub(crate) option_facts: OptionFacts,
}

impl BuiltinData {
    pub(crate) fn from_index(index: BuiltinIndex) -> Self {
        let option_facts = OptionFacts::from_records(index.records());
        BuiltinData {
            index,
            option_facts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PackageName(String);

impl PackageName {
    fn from_record(record: &Record) -> Self {
        Self(record.package.as_deref().unwrap_or("Core").to_string())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for PackageName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl BuiltinIndex {
    pub fn load(corpus: &str) -> Self {
        let mut index = BuiltinIndex::default();
        // JSONL: one JSON object per physical line (markdown newlines are
        // escaped inside the JSON string, so a record never spans lines).
        for line in corpus.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut raw: RawRecord = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("malformed corpus line: {e}\n{line}"));

            // Every non-meta record must name a symbol; an unnamed one is a
            // corrupt corpus, not a record to skip.
            if raw.kind != "meta" && raw.name.is_empty() {
                panic!("corpus record of kind '{}' has no name: {line}", raw.kind);
            }

            // name + aliases + extra_keys all resolve to this record.
            let mut keys = raw.aliases.clone();
            keys.extend(raw.extra_keys.iter().cloned());

            match raw.kind.as_str() {
                "type" => {
                    let mut record = base_record(&mut raw, keys, "Type");
                    let parent_type = raw.parent.as_deref().map(deref_ref).map(InstanceID);
                    let mut ancestors: Vec<_> = raw
                        .ancestors
                        .iter()
                        .map(|ancestor| InstanceID(deref_ref(ancestor)))
                        .collect();
                    if let Some(parent) = &parent_type {
                        ancestors.push(parent.clone());
                    }
                    ancestors.sort();
                    ancestors.dedup();
                    record.type_info = Some(TypeInfo {
                        parent_type,
                        ancestors,
                        subtypes: raw
                            .subtypes
                            .iter()
                            .map(|s| InstanceID(deref_ref(s)))
                            .collect(),
                    });
                    index.insert(record);
                }
                "function" | "methodFunction" | "operator" => {
                    let is_operator = raw.kind == "operator";
                    let is_method_function = raw.kind == "methodFunction";
                    let default_class = if is_operator { "Keyword" } else { "Function" };
                    let mut record = base_record(&mut raw, keys, default_class);
                    let methods = raw
                        .methods
                        .into_iter()
                        .map(|method| {
                            let mut signature = Vec::with_capacity(method.domain.len() + 1);
                            signature.push(record.name.clone());
                            signature.extend(
                                method
                                    .domain
                                    .iter()
                                    .map(|domain| InstanceID(deref_ref(domain))),
                            );
                            MethodSignature {
                                signature,
                                codomain: concrete_codomain(method.typical_value.as_deref())
                                    .map(InstanceID),
                            }
                        })
                        .collect();
                    record.typical_value = concrete_codomain(raw.typical_value.as_deref());
                    record.function_info = Some(FunctionInfo {
                        is_method_function,
                        methods,
                    });
                    if !raw.options.is_empty() {
                        record.option_info = Some(OptionInfo {
                            options: raw.options.into_iter().map(MethodOption::from).collect(),
                        });
                    }
                    if let Some(operator) = raw.operator {
                        record.operator_info = Some(OperatorInfo {
                            method_symbol: record.name.clone(),
                            forms: operator
                                .forms
                                .iter()
                                .map(|form| capitalize_form(form))
                                .collect(),
                            form_attributes: operator.attributes,
                        });
                    }
                    index.insert(record);
                }
                "symbol" | "object" | "table" => {
                    let record = base_record(&mut raw, keys, "Thing");
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
        index
    }

    fn insert(&mut self, record: Record) {
        let id = self.records.len();
        self.record_index_by_name.insert(record.name.clone(), id);
        for alias in &record.aliases {
            self.record_index_by_name
                .entry(InstanceID::new(alias))
                .or_insert(id);
        }
        self.records.push(record);
    }

    pub fn record(&self, name: &InstanceID) -> Option<&Record> {
        self.record_index_by_name
            .get(name)
            .and_then(|index| self.records.get(*index))
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    #[cfg(test)]
    pub fn type_entry(&self, name: &str) -> Option<&Record> {
        self.record(&InstanceID::new(name))
            .filter(|record| record.type_info.is_some())
    }

    #[cfg(test)]
    pub fn callable(&self, name: &str) -> Option<&Record> {
        self.record(&InstanceID::new(name))
            .filter(|record| record.function_info.is_some())
    }

    /// Packages M2 loads at a fresh start (`loadedPackages`), read from the
    /// corpus's leading `meta` record. Empty when the corpus carries no `meta`
    /// record (today's Core-only file) — callers supply the fallback baseline.
    #[cfg(test)]
    pub fn default_loaded(&self) -> &[String] {
        &self.default_loaded
    }

    #[cfg(test)]
    pub fn type_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.type_info.is_some())
            .count()
    }

    #[cfg(test)]
    pub fn callable_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.function_info.is_some())
            .count()
    }

    /// Partition this index into one self-contained `BuiltinIndex` per home
    /// package. Each sub-index owns its records and freshly-built key maps, so
    /// lookups within a partition behave exactly like a single-package load.
    /// Entries with no package bucket under `"Core"` (the loaded-set floor).
    /// `default_loaded` is corpus-global and is not propagated to sub-indexes.
    pub(crate) fn into_package_partitions(
        self,
    ) -> (HashMap<PackageName, BuiltinIndex>, Vec<String>) {
        let mut partitions = HashMap::new();
        for record in self.records {
            partitions
                .entry(PackageName::from_record(&record))
                .or_insert_with(BuiltinIndex::default)
                .insert(record);
        }
        (partitions, self.default_loaded)
    }
}

fn base_record(raw: &mut RawRecord, aliases: Vec<String>, default_class: &str) -> Record {
    Record {
        name: InstanceID(std::mem::take(&mut raw.name)),
        class: InstanceID(
            raw.class
                .take()
                .map(|class| deref_ref(&class))
                .unwrap_or_else(|| default_class.to_string()),
        ),
        examples: Vec::new(),
        package: raw.package.take().map(|package| deref_ref(&package)),
        source_file: None,
        typical_value: None,
        function_info: None,
        option_info: None,
        operator_info: None,
        type_info: None,
        protected: raw.protected,
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
fn concrete_codomain(raw_key: Option<&str>) -> Option<String> {
    let name = raw_key.map(deref_ref)?;
    match name.as_str() {
        "Thing" | "Any" => None,
        _ => Some(name),
    }
}

#[derive(Debug, Deserialize)]
struct RawRecord {
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    extra_keys: Vec<String>,
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    ancestors: Vec<String>,
    #[serde(default)]
    subtypes: Vec<String>,
    #[serde(default)]
    typical_value: Option<String>,
    #[serde(default)]
    options: Vec<RawOptionSpec>,
    #[serde(default)]
    methods: Vec<RawMethod>,
    #[serde(default)]
    operator: Option<RawOperator>,
    #[serde(default)]
    default_loaded: Vec<String>,
    #[serde(default)]
    protected: Option<bool>,
    #[serde(default)]
    markdown: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawMethod {
    #[serde(default)]
    domain: Vec<String>,
    #[serde(default, rename = "typicalValue")]
    typical_value: Option<String>,
}

/// Wire-format optional argument metadata. Conversion into `MethodOption` keeps
/// Serde details at the JSONL boundary.
#[derive(Debug, Deserialize)]
struct RawOptionSpec {
    key: String,
    #[serde(default, rename = "possibleValues")]
    possible_values: Vec<String>,
}

impl From<RawOptionSpec> for MethodOption {
    fn from(raw: RawOptionSpec) -> Self {
        MethodOption {
            name: InstanceID::new(&raw.key),
            possible_values: raw.possible_values.into_iter().map(InstanceID).collect(),
        }
    }
}

/// Operator syntactic metadata: forms are lowercase in the corpus
/// (`binary`/`prefix`/`postfix`/`assignment`); the LSP keeps the capitalized
/// vocabulary (`Binary`/…) used by `record_lsp.rs` and `typesystem.rs`.
#[derive(Debug, Deserialize)]
struct RawOperator {
    #[serde(default)]
    forms: Vec<String>,
    #[serde(default)]
    attributes: HashMap<OperatorForm, Vec<String>>,
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

    #[test]
    fn load_parses_new_format_corpus() {
        let index = BuiltinIndex::load(include_str!("./data/m2-index.jsonl"));

        // type record: parent/ancestors deref'd to bare names
        let zz = index.type_entry("ZZ").expect("ZZ type present");
        assert_eq!(zz.package.as_deref(), Some("Core")); // $Core$Core -> Core
        assert!(zz
            .type_info
            .as_ref()
            .expect("ZZ type facts")
            .ancestors
            .iter()
            .all(|ancestor| !ancestor.0.starts_with('$')));
        // markdown is now folded onto the entry (documented Core type).
        assert!(
            zz.markdown.is_some(),
            "ZZ should carry folded hover markdown"
        );

        // methodFunction record -> callable, with a deref'd codomain
        let beta = index.callable("Beta").expect("Beta callable present");
        let beta_info = beta.function_info.as_ref().expect("Beta callable facts");
        assert!(beta_info.is_method_function);
        assert!(beta_info
            .methods
            .iter()
            .any(|method| method.codomain.as_ref().map(AsRef::as_ref) == Some("RR"))); // $Core$RR -> RR

        // operator record -> callable + capitalized forms from the `operator` object
        let minus = index.callable("-").expect("- operator present");
        let minus_operator = minus.operator_info.as_ref().expect("- operator facts");
        assert!(minus_operator.forms.contains(&"Binary".to_string()));
        assert!(minus_operator.forms.contains(&"Prefix".to_string()));

        let method_constructor = index.callable("method").expect("method function present");
        assert!(
            !method_constructor
                .function_info
                .as_ref()
                .expect("method callable facts")
                .is_method_function
        );
    }

    #[test]
    fn loads_types_and_callables() {
        let index = index();
        assert!(index.type_count() > 100, "type lattice should be populated");
        assert!(
            index.callable_count() > 500,
            "callables should be populated"
        );
    }

    #[test]
    fn looks_up_callables_by_alias() {
        let index = index();
        // `gb` is reachable by its package-qualified alias too.
        assert!(index.callable("Core$gb").is_some());
        assert_eq!(
            index.callable("Core$gb").map(|c| c.name.0.as_str()),
            Some("gb")
        );
    }

    #[test]
    fn parses_callable_signatures() {
        let index = index();
        let gb = index.callable("gb").expect("gb is a callable");
        assert!(gb.operator_info.is_none());
        // gb dispatches on Ideal/Module/Matrix, all returning a GroebnerBasis;
        // subtype matching uses canonical type records, while codomains stay on methods.
        assert!(gb
            .function_info
            .as_ref()
            .expect("gb callable facts")
            .methods
            .iter()
            .any(|method| {
                method.signature[1..] == [InstanceID::new("Ideal")]
                    && method.codomain.as_ref().map(AsRef::as_ref) == Some("GroebnerBasis")
            }));
    }

    #[test]
    fn parses_type_lattice_edges() {
        let index = index();
        // Each canonical type record carries its normalized ancestor chain.
        if let Some(zz) = index.type_entry("ZZ") {
            assert!(!zz
                .type_info
                .as_ref()
                .expect("ZZ type facts")
                .ancestors
                .is_empty());
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

    /// Monotone index invariant: `Thing` and `Any` are the top of the M2 type
    /// lattice and carry no information — storing them as a positive codomain
    /// fact would pollute inference and hover. The loader must drop them to `None`.
    #[test]
    fn no_callable_carries_thing_or_any_codomain() {
        let index = index();

        // Global invariant: no signature codomain and no callable typical_value
        // may be Some("Thing") or Some("Any").
        for callable in index
            .records()
            .iter()
            .filter(|record| record.function_info.is_some())
        {
            for method in &callable
                .function_info
                .as_ref()
                .expect("callable facts")
                .methods
            {
                assert!(
                    !matches!(
                        method.codomain.as_ref().map(AsRef::as_ref),
                        Some("Thing") | Some("Any")
                    ),
                    "callable '{}' has a Thing/Any signature codomain (domain={:?})",
                    callable.name,
                    &method.signature[1..],
                );
            }
            assert!(
                !matches!(
                    callable.typical_value.as_deref(),
                    Some("Thing") | Some("Any")
                ),
                "callable '{}' has a Thing/Any typical_value",
                callable.name,
            );
        }

        // Spot-check: `next` over domain `["Iterator"]` — raw corpus has
        // `$Core$Thing` — must be dropped to None after load.
        let next = index.callable("next").expect("next callable present");
        let next_iter_sig = next
            .function_info
            .as_ref()
            .expect("next callable facts")
            .methods
            .iter()
            .find(|method| method.signature[1..] == [InstanceID::new("Iterator")])
            .expect("next(Iterator) signature present");
        assert_eq!(
            next_iter_sig.codomain, None,
            "next(Iterator) codomain must be None, not Thing"
        );
        assert_eq!(
            next.typical_value, None,
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
        assert_eq!(index.default_loaded(), &["Core", "Classic", "Polyhedra"]);
        assert!(
            index.type_entry("ZZ").is_some(),
            "non-meta records still load"
        );
    }

    #[test]
    fn default_loaded_is_empty_without_meta_record() {
        let corpus = r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#;
        let index = BuiltinIndex::load(corpus);
        assert!(index.default_loaded().is_empty());
    }

    #[test]
    #[should_panic(expected = "has no name")]
    fn load_panics_on_unnamed_non_meta_record() {
        BuiltinIndex::load(r#"{"kind":"type","package":"$Core$Core"}"#);
    }

    #[test]
    fn partition_routes_records_to_their_home_package() {
        let corpus = concat!(
            r#"{"kind":"type","name":"ZZ","package":"$Core$Core","ancestors":["$Core$Thing"]}"#,
            "\n",
            r#"{"kind":"type","name":"FooType","package":"$FooPkg$FooPkg","parent":"$Core$ZZ"}"#,
            "\n",
            r#"{"kind":"function","name":"fooFn","package":"$FooPkg$FooPkg"}"#,
        );
        let index = BuiltinIndex::load(corpus);
        let (parts, _) = index.into_package_partitions();

        let core = parts.get("Core").expect("Core partition present");
        assert!(core.type_entry("ZZ").is_some());
        assert!(
            core.type_entry("FooType").is_none(),
            "FooType is not in Core"
        );

        let foo = parts.get("FooPkg").expect("FooPkg partition present");
        assert!(foo.type_entry("FooType").is_some());
        assert!(foo.callable("fooFn").is_some());
        assert!(foo.type_entry("ZZ").is_none(), "ZZ is not in FooPkg");
    }

    #[test]
    fn partition_of_real_corpus_is_a_true_partition() {
        let index = BuiltinIndex::load(include_str!("./data/m2-index.jsonl"));
        let type_count = index.type_count();
        let callable_count = index.callable_count();
        let (parts, _) = index.into_package_partitions();
        // Core is always present (the loaded-set floor).
        assert!(parts.contains_key("Core"), "Core partition present");
        // Every record lands in exactly one partition — no loss, no duplication.
        let part_types: usize = parts.values().map(|p| p.type_count()).sum();
        let part_callables: usize = parts.values().map(|p| p.callable_count()).sum();
        assert_eq!(part_types, type_count);
        assert_eq!(part_callables, callable_count);
    }
}
