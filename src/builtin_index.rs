//! The static builtin index parsed from `data/m2-types.jsonl` — the typecheck
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
    /// General codomain when documented apart from a specific signature.
    pub typical_value: Option<String>,
    pub options: Vec<OptionSpec>,
    pub signatures: Vec<Signature>,
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
}

impl BuiltinIndex {
    pub fn load(jsonl: &str) -> Self {
        let mut index = BuiltinIndex::default();
        for line in jsonl.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(raw) = serde_json::from_str::<RawRecord>(line) else {
                continue;
            };
            match raw.kind.as_str() {
                "type" => {
                    let id = index.types.len();
                    register_keys(&mut index.type_keys, &raw.name, &raw.aliases, id);
                    index.types.push(TypeEntry {
                        name: raw.name,
                        aliases: raw.aliases,
                        package: raw.package,
                        class: raw.class,
                        parent: raw.parent,
                        ancestors: raw.ancestors,
                        subtypes: raw.subtypes,
                        instances: raw.instances,
                    });
                }
                "function" | "operator" => {
                    let id = index.callables.len();
                    register_keys(&mut index.callable_keys, &raw.name, &raw.aliases, id);
                    let forms = collect_forms(&raw.methods);
                    let signatures = raw
                        .methods
                        .into_iter()
                        .map(|method| Signature {
                            domain: method.domain,
                            codomain: method.typical_value,
                            exact: method.exact,
                            options: method.options,
                        })
                        .collect();
                    index.callables.push(CallableEntry {
                        name: raw.name,
                        aliases: raw.aliases,
                        package: raw.package,
                        class: raw.class,
                        is_operator: raw.kind == "operator",
                        forms,
                        typical_value: raw.typical_value,
                        options: raw.options,
                        signatures,
                    });
                }
                "object" => {
                    let id = index.objects.len();
                    register_keys(&mut index.object_keys, &raw.name, &raw.aliases, id);
                    index.objects.push(ObjectEntry {
                        name: raw.name,
                        aliases: raw.aliases,
                        package: raw.package,
                        class: raw.class,
                    });
                }
                // The `package` record carries no per-symbol facts.
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

    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    pub fn callable_count(&self) -> usize {
        self.callables.len()
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
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
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
    /// `binary` / `prefix` / `postfix` for operator methods; absent otherwise.
    #[serde(default)]
    form: Option<String>,
}

/// The distinct operator forms across a callable's methods, capitalized to the
/// `OperatorInfo.forms` vocabulary (`Binary`/`Prefix`/`Postfix`).
fn collect_forms(methods: &[RawMethod]) -> Vec<String> {
    let mut forms = Vec::new();
    for method in methods {
        let Some(form) = method.form.as_deref() else {
            continue;
        };
        let label = match form {
            "binary" => "Binary",
            "prefix" => "Prefix",
            "postfix" => "Postfix",
            _ => continue,
        };
        if !forms.iter().any(|existing| existing == label) {
            forms.push(label.to_string());
        }
    }
    forms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> BuiltinIndex {
        BuiltinIndex::load(include_str!("./data/m2-types.jsonl"))
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
}
