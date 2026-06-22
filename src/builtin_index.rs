//! The static builtin index parsed from `data/m2-index.jsonl` — the typecheck
//! source of truth (replacing the old runtime-scraped `Record`/`BuiltinData`).
//!
//! Two tables: a **type lattice** (`parent`/`ancestors`/`class`/`subtypes`) for
//! subtype checks, and **callable signatures** (`methods[{domain, codomain}]`)
//! for call-result inference. Per the index's design it is monotone — an absent
//! codomain means *unknown*, never `Thing`. See the `Static Typecheck Index`
//! decision.

// Forward-looking API: the lattice/signature queries are consumed by the type
// propagation stage; allow the unused-method warnings until then.
#![allow(dead_code)]

use std::collections::HashMap;

use serde::Deserialize;

/// A type's place in the M2 lattice.
#[derive(Debug, Clone)]
pub struct TypeEntry {
    pub name: String,
    /// Every alias this entry is also keyed by (`Core$ZZ`, …) — retained so the
    /// `Record`/docs maps mirror the same keys as the lookup tables, not just
    /// the primary name.
    pub aliases: Vec<String>,
    pub package: Option<String>,
    /// Meta-type — the instance-of axis (`ZZ`'s class is `Ring`).
    pub class: Option<String>,
    /// Immediate supertype — the is-a edge.
    pub parent: Option<String>,
    pub ancestors: Vec<String>,
    pub subtypes: Vec<String>,
    pub instances: Vec<String>,
    /// Rendered hover markdown for this entry, folded into the record by the
    /// corpus generator. `None` ⇒ undocumented (monotone: absent, not empty).
    pub markdown: Option<String>,
}

/// One installed method: an argument-type tuple and its result type.
#[derive(Debug, Clone)]
pub struct Signature {
    pub domain: Vec<String>,
    /// `None` ⇒ codomain undocumented; the checker stays silent rather than guess.
    pub codomain: Option<String>,
    /// Whether the codomain is the value's *exact* type rather than an upper
    /// bound. M2 cannot tell, so this is `false` everywhere unless we hand-mark
    /// it; when `true` it collapses the result from `codomain`'s subtree to the
    /// single type.
    pub exact: bool,
    pub options: Vec<OptionSpec>,
}

/// A named runtime value that is neither a type nor a callable — option keys
/// (`Strategy`, `Algorithm`), constants (`pi`, `true`, `infinity`), etc. Carries
/// no typecheck facts beyond its `class` (the instance-of axis).
#[derive(Debug, Clone)]
pub struct ObjectEntry {
    pub name: String,
    pub aliases: Vec<String>,
    pub package: Option<String>,
    pub class: Option<String>,
    /// Whether the symbol is `protect`ed (cannot be reassigned). `None` ⇒ the
    /// corpus did not record it; consumers fall back to the class-is-`Symbol`
    /// proxy so the absent-data case keeps today's behaviour.
    pub protected: Option<bool>,
    /// Rendered hover markdown, folded into the record. `None` ⇒ undocumented.
    pub markdown: Option<String>,
}

/// A function or operator and the signatures it dispatches on.
#[derive(Debug, Clone)]
pub struct CallableEntry {
    pub name: String,
    /// Every alias this entry is also keyed by (`Core$gb`, `→`, …).
    pub aliases: Vec<String>,
    pub package: Option<String>,
    pub class: Option<String>,
    pub is_operator: bool,
    /// Capitalized operator forms (`Binary`/`Prefix`/`Postfix`) collected across
    /// this callable's methods — drives operator hover label rendering.
    pub forms: Vec<String>,
    /// Per-form operator attributes from the corpus (`binary` → `["Flexible"]`,
    /// …); empty for non-operators. Drives the per-fixity flexibility check that
    /// decides whether `:=` may install a method on this operator.
    pub operator_attributes: HashMap<String, Vec<String>>,
    /// General codomain when documented apart from a specific signature.
    pub typical_value: Option<String>,
    pub options: Vec<OptionSpec>,
    pub signatures: Vec<Signature>,
    /// Rendered hover markdown, folded into the record. `None` ⇒ undocumented.
    pub markdown: Option<String>,
}

/// An optional argument: its key and (sparsely curated) value constraints.
#[derive(Debug, Clone, Deserialize)]
pub struct OptionSpec {
    pub key: String,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default, rename = "possibleValues")]
    pub possible_values: Vec<String>,
    #[serde(default, rename = "valueType")]
    pub value_type: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// Each record is keyed by its name *and* every alias (e.g. `gb`, `Core$gb`;
/// `->`, `→`, `Core$->`), all pointing at one pooled entry.
#[derive(Debug, Default)]
pub struct BuiltinIndex {
    types: Vec<TypeEntry>,
    type_keys: HashMap<String, usize>,
    callables: Vec<CallableEntry>,
    callable_keys: HashMap<String, usize>,
    objects: Vec<ObjectEntry>,
    object_keys: HashMap<String, usize>,
    default_loaded: Vec<String>,
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
            let raw: RawRecord = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("malformed corpus line: {e}\n{line}"));

            // Every non-meta record must name a symbol; an unnamed one is a
            // corrupt corpus, not a record to skip.
            if raw.kind != "meta" && raw.name.is_empty() {
                panic!("corpus record of kind '{}' has no name: {line}", raw.kind);
            }

            // name + aliases + extra_keys all resolve to this record.
            let mut keys = raw.aliases.clone();
            keys.extend(raw.extra_keys.iter().cloned());
            let markdown = raw.markdown.filter(|m| !m.is_empty());

            match raw.kind.as_str() {
                "type" => {
                    let id = index.types.len();
                    register_keys(&mut index.type_keys, &raw.name, &keys, id);
                    index.types.push(TypeEntry {
                        name: raw.name,
                        aliases: keys,
                        package: raw.package.as_deref().map(deref_ref),
                        class: raw.class.as_deref().map(deref_ref),
                        parent: raw.parent.as_deref().map(deref_ref),
                        ancestors: raw.ancestors.iter().map(|a| deref_ref(a)).collect(),
                        subtypes: raw.subtypes.iter().map(|s| deref_ref(s)).collect(),
                        instances: raw.instances.iter().map(|i| deref_ref(i)).collect(),
                        markdown,
                    });
                }
                "function" | "methodFunction" | "operator" => {
                    let id = index.callables.len();
                    register_keys(&mut index.callable_keys, &raw.name, &keys, id);
                    let forms = raw
                        .operator
                        .as_ref()
                        .map(|op| op.forms.iter().map(|f| capitalize_form(f)).collect())
                        .unwrap_or_default();
                    let operator_attributes = raw
                        .operator
                        .as_ref()
                        .map(|op| op.attributes.clone())
                        .unwrap_or_default();
                    let signatures = raw
                        .methods
                        .into_iter()
                        .map(|method| Signature {
                            domain: method.domain.iter().map(|d| deref_ref(d)).collect(),
                            codomain: concrete_codomain(method.typical_value.as_deref()),
                            exact: method.exact,
                            options: method.options,
                        })
                        .collect();
                    index.callables.push(CallableEntry {
                        name: raw.name,
                        aliases: keys,
                        package: raw.package.as_deref().map(deref_ref),
                        class: raw.class.as_deref().map(deref_ref),
                        is_operator: raw.kind == "operator",
                        forms,
                        operator_attributes,
                        typical_value: concrete_codomain(raw.typical_value.as_deref()),
                        options: raw.options,
                        signatures,
                        markdown,
                    });
                }
                "symbol" | "object" | "table" => {
                    let id = index.objects.len();
                    register_keys(&mut index.object_keys, &raw.name, &keys, id);
                    index.objects.push(ObjectEntry {
                        name: raw.name,
                        aliases: keys,
                        package: raw.package.as_deref().map(deref_ref),
                        class: raw.class.as_deref().map(deref_ref),
                        protected: raw.protected,
                        markdown,
                    });
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

    pub fn type_entry(&self, name: &str) -> Option<&TypeEntry> {
        self.type_keys.get(name).map(|&id| &self.types[id])
    }

    pub fn callable(&self, name: &str) -> Option<&CallableEntry> {
        self.callable_keys.get(name).map(|&id| &self.callables[id])
    }

    /// The pooled type records (one per distinct type, aliases aside). The
    /// `TypeLattice` builder consumes these.
    pub fn types(&self) -> &[TypeEntry] {
        &self.types
    }

    /// The pooled callable records. The `TypeFacts` builder consumes these.
    pub fn callables(&self) -> &[CallableEntry] {
        &self.callables
    }

    pub fn object(&self, name: &str) -> Option<&ObjectEntry> {
        self.object_keys.get(name).map(|&id| &self.objects[id])
    }

    /// The pooled object records (option keys, constants, …). They carry no
    /// typecheck facts but are surfaced as `Record`s for hover/classification.
    pub fn objects(&self) -> &[ObjectEntry] {
        &self.objects
    }

    /// Packages M2 loads at a fresh start (`loadedPackages`), read from the
    /// corpus's leading `meta` record. Empty when the corpus carries no `meta`
    /// record (today's Core-only file) — callers supply the fallback baseline.
    pub fn default_loaded(&self) -> &[String] {
        &self.default_loaded
    }

    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    pub fn callable_count(&self) -> usize {
        self.callables.len()
    }

    /// Partition this index into one self-contained `BuiltinIndex` per home
    /// package. Each sub-index owns its records and freshly-built key maps, so
    /// lookups within a partition behave exactly like a single-package load.
    /// Entries with no package bucket under `"Core"` (the loaded-set floor).
    /// `default_loaded` is corpus-global and is not propagated to sub-indexes.
    pub fn partition_by_package(&self) -> HashMap<String, BuiltinIndex> {
        let mut partitions: HashMap<String, BuiltinIndex> = HashMap::new();
        let bucket =
            |package: &Option<String>| package.clone().unwrap_or_else(|| "Core".to_string());

        for entry in &self.types {
            let part = partitions.entry(bucket(&entry.package)).or_default();
            let id = part.types.len();
            register_keys(&mut part.type_keys, &entry.name, &entry.aliases, id);
            part.types.push(entry.clone());
        }
        for entry in &self.callables {
            let part = partitions.entry(bucket(&entry.package)).or_default();
            let id = part.callables.len();
            register_keys(&mut part.callable_keys, &entry.name, &entry.aliases, id);
            part.callables.push(entry.clone());
        }
        for entry in &self.objects {
            let part = partitions.entry(bucket(&entry.package)).or_default();
            let id = part.objects.len();
            register_keys(&mut part.object_keys, &entry.name, &entry.aliases, id);
            part.objects.push(entry.clone());
        }
        partitions
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

/// Register a pooled entry under its name and each alias. The name wins on a
/// collision; an alias never clobbers an already-registered key.
fn register_keys(keys: &mut HashMap<String, usize>, name: &str, aliases: &[String], id: usize) {
    keys.insert(name.to_string(), id);
    for alias in aliases {
        keys.entry(alias.clone()).or_insert(id);
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
    instances: Vec<String>,
    #[serde(default)]
    typical_value: Option<String>,
    #[serde(default)]
    options: Vec<OptionSpec>,
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
    #[serde(default)]
    exact: bool,
    #[serde(default)]
    options: Vec<OptionSpec>,
}

/// Operator syntactic metadata: forms are lowercase in the corpus
/// (`binary`/`prefix`/`postfix`/`assignment`); the LSP keeps the capitalized
/// vocabulary (`Binary`/…) used by `record_lsp.rs` and `typesystem.rs`.
#[derive(Debug, Deserialize)]
struct RawOperator {
    #[serde(default)]
    forms: Vec<String>,
    /// Per-form operator attributes, e.g. `{"binary": ["Flexible"], "prefix": […]}`.
    /// `Flexible` marks the forms that accept runtime method installation.
    #[serde(default)]
    attributes: HashMap<String, Vec<String>>,
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
        assert!(zz.ancestors.iter().all(|a| !a.starts_with('$')));
        // markdown is now folded onto the entry (documented Core type).
        assert!(
            zz.markdown.is_some(),
            "ZZ should carry folded hover markdown"
        );

        // methodFunction record -> callable, with a deref'd codomain
        let beta = index.callable("Beta").expect("Beta callable present");
        assert!(beta
            .signatures
            .iter()
            .any(|s| s.codomain.as_deref() == Some("RR"))); // $Core$RR -> RR

        // operator record -> callable + capitalized forms from the `operator` object
        let minus = index.callable("-").expect("- operator present");
        assert!(minus.is_operator);
        assert!(minus.forms.contains(&"Binary".to_string()));
        assert!(minus.forms.contains(&"Prefix".to_string()));
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
            index.callable("Core$gb").map(|c| c.name.as_str()),
            Some("gb")
        );
    }

    #[test]
    fn parses_callable_signatures() {
        let index = index();
        let gb = index.callable("gb").expect("gb is a callable");
        assert!(!gb.is_operator);
        // gb dispatches on Ideal/Module/Matrix, all returning a GroebnerBasis;
        // subtype matching / codomain lookup live in TypeLattice / TypeFacts.
        assert!(gb
            .signatures
            .iter()
            .any(|s| s.domain == ["Ideal"] && s.codomain.as_deref() == Some("GroebnerBasis")));
    }

    #[test]
    fn parses_type_lattice_edges() {
        let index = index();
        // The parser carries each type's ancestor chain for TypeLattice to consume.
        if let Some(zz) = index.type_entry("ZZ") {
            assert!(!zz.ancestors.is_empty());
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
        for callable in index.callables() {
            for sig in &callable.signatures {
                assert!(
                    !matches!(sig.codomain.as_deref(), Some("Thing") | Some("Any")),
                    "callable '{}' has a Thing/Any signature codomain (domain={:?})",
                    callable.name,
                    sig.domain,
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
            .signatures
            .iter()
            .find(|s| s.domain == ["Iterator"])
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
        let parts = index.partition_by_package();

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
        let parts = index.partition_by_package();
        // Core is always present (the loaded-set floor).
        assert!(parts.contains_key("Core"), "Core partition present");
        // Every record lands in exactly one partition — no loss, no duplication.
        let part_types: usize = parts.values().map(|p| p.type_count()).sum();
        let part_callables: usize = parts.values().map(|p| p.callable_count()).sum();
        assert_eq!(part_types, index.type_count());
        assert_eq!(part_callables, index.callable_count());
    }
}
