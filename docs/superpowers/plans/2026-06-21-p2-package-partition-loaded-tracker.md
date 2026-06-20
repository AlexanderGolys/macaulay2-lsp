# P2 — Package Partition + Loaded-Package Tracker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a package-partitioned in-memory index (`PackagePartitionedIndex`) and a `LoadedPackages` tracker — the data structures that let later work scope every typecheck/hover/navigation query to the packages a document actually loads — without yet rewiring any query site.

**Architecture:** The single embedded corpus is parsed once into a `BuiltinIndex`, then partitioned by each record's home `package` into a `HashMap<String, BuiltinData>` (one fully-built `BuiltinData` per package). A leading `meta` record supplies the default-loaded baseline (`loadedPackages` at a fresh M2 start); when absent (today's Core-only corpus), the baseline falls back to the packages present. `LoadedPackages` is a pure function of document text: `default_loaded ∪ collect_imported_packages(text)`, baseline-first. `self.builtins` is re-sourced from the Core partition so the new path and the old path cannot drift.

**Tech Stack:** Rust, `tower-lsp`, `tree-sitter-macaulay2`, `serde_json`. Crate package name is `m2-ls` (tests run with `cargo test -p m2-ls`).

## Global Constraints

These bind every task. Copied from the design spec, project CLAUDE.md, and the user's standing preferences.

- **No test-first (TDD) here.** The user's standing rule overrides the skill's TDD default: write the logic first, then write tests that verify it. Do not retrofit logic to a pre-written test; if an existing test encodes old behavior, delete/replace it rather than contort the code. (Tests still gate each task — they just come after the implementation, in the same commit.)
- **Build = fmt + clippy.** Every task's "verify" step runs `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test -p m2-ls`. All three must pass before commit.
- **Fail fast, no defensive coercion.** Trust types; `expect`/`panic` on broken invariants (e.g. a missing Core partition) rather than silently defaulting ill-typed input. Do not wrap in `try`/defaults to mask bugs.
- **Semantic types, no primitive obsession.** Package names and the loaded set get named types where they carry a role (`LoadedPackages`), not bare `Vec<String>` passed around untyped.
- **Monotone, known-facts-only.** Absent = unknown = the checker stays silent. Never synthesize a positive fact (a codomain, a partition membership) that the corpus does not state. A package with no partition simply has no data — never an error.
- **Reference dereferencing.** Corpus reference keys are `$Package$Name`; the existing `deref_ref` (`builtin_index.rs`) strips them to bare names. Package *field* values are also `$Package$Name` and are already deref'd at load (`TypeEntry.package == Some("Core")` for a `$Core$Core` field). The `meta` record's `default_loaded` entries are **bare package names** (`"Core"`, `"Classic"`) — no deref.
- **`typesystem.rs` is live WIP — do not tidy it.** Add the functions this plan specifies; do not refactor surrounding code for style, rename existing items, or "clean up" anything you are not explicitly told to change.
- **Do not rewire query sites.** P2 builds structures only. Inference (`analysis.rs`), hover, completion, navigation, semantic tokens, and type-hierarchy keep consuming `self.builtins` / `active_package_indexes` exactly as today. Routing them through a scoped view is P3.

---

## File Structure

- `src/package_index.rs` — **modify.** Add `importFrom` to the package-import trigger set in `package_source_string`.
- `src/builtin_index.rs` — **modify.** Capture the `meta` record's `default_loaded`; add `BuiltinIndex::partition_by_package`.
- `src/typesystem.rs` — **modify (additively).** Add `package` to `DocRecord`; add `load_docs_markdown_by_package`; factor `load_from_index`'s post-parse half into a reusable `BuiltinData::from_index`.
- `src/partitioned_index.rs` — **create.** `PackagePartitionedIndex` (partition map + baseline) and the `LoadedPackages` semantic type.
- `src/main.rs` — **modify.** Register the new module; build a `PackagePartitionedIndex` in `Backend::new`; re-source `self.builtins` from its Core partition; add a `loaded_packages(text)` method. No query rewiring.

---

## Task 1: `importFrom` package-import trigger

Add `importFrom` (string-first-arg form) to the calls that `collect_imported_packages` treats as loading a package. Verified against M2 v1.26.05: `importFrom(String, List)` and `importFrom(String, String)` both reference a package by name; the existing `is_first_named_child` guard already ensures only the *first* string (the package) matches, not the symbol-name strings in the second argument. The `importFrom(Package, …)` / `importFrom_Core {…}` forms take a Package object (no string), so they contribute nothing — correct and automatic.

**Files:**
- Modify: `src/package_index.rs:130` and `src/package_index.rs:136` (the two `matches!` trigger sets in `package_source_string`)
- Test: `src/package_index.rs` (inline `#[cfg(test)]` module — add if absent)

**Interfaces:**
- Consumes: `collect_imported_packages(text: &str) -> Vec<String>` (existing, `package_index.rs:140`)
- Produces: no signature change; broadened detection only.

- [ ] **Step 1: Broaden both trigger sets**

In `src/package_index.rs`, `package_source_string`, change **both** `matches!` calls (the `sequence`-branch one near line 130 and the binary-branch one near line 136) from:

```rust
.filter(|name| matches!(*name, "needsPackage" | "loadPackage" | "debug"))
```

to:

```rust
.filter(|name| matches!(*name, "needsPackage" | "loadPackage" | "debug" | "importFrom"))
```

(`importFrom` only ever appears in the multi-arg `sequence` branch in practice, but adding it to both keeps the trigger set single-sourced and consistent.)

- [ ] **Step 2: Add verifying tests**

Append to `src/package_index.rs` (inside the existing `#[cfg(test)] mod tests`, or add this module if none exists):

```rust
#[cfg(test)]
mod import_trigger_tests {
    use super::collect_imported_packages;

    #[test]
    fn import_from_string_form_adds_the_package() {
        let pkgs = collect_imported_packages(r#"importFrom("FooPkg", {"barSym", "bazSym"})"#);
        assert_eq!(pkgs, vec!["FooPkg".to_string()]);
    }

    #[test]
    fn import_from_does_not_capture_symbol_name_strings() {
        // The second-argument symbol strings must NOT be treated as packages.
        let pkgs = collect_imported_packages(r#"importFrom("FooPkg", "barSym")"#);
        assert_eq!(pkgs, vec!["FooPkg".to_string()]);
    }

    #[test]
    fn import_from_package_object_form_adds_nothing() {
        // `importFrom_Core {...}` / `importFrom(Core, ...)` take a Package object,
        // not a string — no package name to detect.
        let pkgs = collect_imported_packages("importFrom_Core {\"raw\"}");
        assert!(pkgs.is_empty(), "Package-object form must add nothing, got {pkgs:?}");
    }

    #[test]
    fn existing_triggers_still_detected() {
        let pkgs = collect_imported_packages("needsPackage \"A\"\nloadPackage \"B\"\ndebug \"C\"");
        assert_eq!(pkgs, vec!["A".to_string(), "B".to_string(), "C".to_string()]);
    }
}
```

- [ ] **Step 3: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test -p m2-ls import_trigger`
Expected: clippy clean; the four new tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/package_index.rs
git commit -m "Detect importFrom(String, ...) as a package import"
```

---

## Task 2: Capture the `meta` record's `default_loaded` baseline

`BuiltinIndex::load` currently discards unknown record kinds (`_ => {}`). Teach it to read a leading `{"kind":"meta","default_loaded":[...]}` record and expose the list. The current `m2-types.jsonc` has no such record, so `default_loaded()` returns empty today — the partitioned index (Task 5) supplies the fallback.

**Files:**
- Modify: `src/builtin_index.rs` (`RawRecord`, `BuiltinIndex` struct, `load`, accessors, tests)

**Interfaces:**
- Produces:
  - `BuiltinIndex.default_loaded: Vec<String>` (private field)
  - `pub fn default_loaded(&self) -> &[String]`

- [ ] **Step 1: Add the field to `RawRecord`**

In `src/builtin_index.rs`, add to `struct RawRecord` (near the other `#[serde(default)]` fields, ~line 283):

```rust
    #[serde(default)]
    default_loaded: Vec<String>,
```

- [ ] **Step 2: Add the field + accessor to `BuiltinIndex`**

Add to `pub struct BuiltinIndex` (the `#[derive(Debug, Default)]` struct, ~line 96):

```rust
    default_loaded: Vec<String>,
```

Add the accessor in the `impl BuiltinIndex` block (next to `types()`/`callables()`):

```rust
    /// Packages M2 loads at a fresh start (`loadedPackages`), read from the
    /// corpus's leading `meta` record. Empty when the corpus carries no `meta`
    /// record (today's Core-only file) — callers supply the fallback baseline.
    pub fn default_loaded(&self) -> &[String] {
        &self.default_loaded
    }
```

- [ ] **Step 3: Capture the meta record in `load`**

In `BuiltinIndex::load`, replace the catch-all arm:

```rust
                // `package` and any future `meta` record carry no per-symbol facts.
                _ => {}
```

with:

```rust
                "meta" => {
                    // Baseline of fresh-start loaded packages; bare package names.
                    index.default_loaded = raw.default_loaded;
                }
                // `package` records carry no per-symbol typecheck facts.
                _ => {}
```

- [ ] **Step 4: Add verifying tests**

Add to the `#[cfg(test)] mod tests` in `src/builtin_index.rs`:

```rust
    #[test]
    fn captures_default_loaded_from_meta_record() {
        let corpus = r#"[
            {"kind":"meta","default_loaded":["Core","Classic","Polyhedra"]},
            {"kind":"type","name":"ZZ","package":"$Core$Core"}
        ]"#;
        let index = BuiltinIndex::load(corpus);
        assert_eq!(index.default_loaded(), &["Core", "Classic", "Polyhedra"]);
        assert!(index.type_entry("ZZ").is_some(), "non-meta records still load");
    }

    #[test]
    fn default_loaded_is_empty_without_meta_record() {
        let corpus = r#"[{"kind":"type","name":"ZZ","package":"$Core$Core"}]"#;
        let index = BuiltinIndex::load(corpus);
        assert!(index.default_loaded().is_empty());
    }
```

- [ ] **Step 5: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test -p m2-ls default_loaded`
Expected: clippy clean; both tests PASS; pre-existing builtin_index tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add src/builtin_index.rs
git commit -m "Read default-loaded baseline from corpus meta record"
```

---

## Task 3: `BuiltinIndex::partition_by_package`

Split a parsed `BuiltinIndex` into one sub-index per home package, each a self-contained `BuiltinIndex` with its own key maps. Entries with no package (`None`) bucket under `"Core"` — today's corpus is entirely Core, and Core is the floor of the loaded set. `default_loaded` is a corpus-global fact and is **not** copied into sub-indexes (the partitioned wrapper in Task 5 owns it).

**Files:**
- Modify: `src/builtin_index.rs` (add `partition_by_package`; tests)

**Interfaces:**
- Consumes: `TypeEntry`/`CallableEntry`/`ObjectEntry` (all `#[derive(Clone)]`), `register_keys` (private, same module)
- Produces: `pub fn partition_by_package(&self) -> std::collections::HashMap<String, BuiltinIndex>`

- [ ] **Step 1: Implement `partition_by_package`**

Add to `impl BuiltinIndex` in `src/builtin_index.rs`:

```rust
    /// Partition this index into one self-contained `BuiltinIndex` per home
    /// package. Each sub-index owns its records and freshly-built key maps, so
    /// lookups within a partition behave exactly like a single-package load.
    /// Entries with no package bucket under `"Core"` (the loaded-set floor).
    /// `default_loaded` is corpus-global and is not propagated to sub-indexes.
    pub fn partition_by_package(&self) -> HashMap<String, BuiltinIndex> {
        let mut partitions: HashMap<String, BuiltinIndex> = HashMap::new();
        let bucket = |package: &Option<String>| package.clone().unwrap_or_else(|| "Core".to_string());

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
```

(`TypeEntry.aliases` already holds the name's full key set, so `register_keys` reproduces the same lookup behavior per partition. `HashMap` is already imported at the top of the file.)

- [ ] **Step 2: Add verifying tests**

Add to `#[cfg(test)] mod tests` in `src/builtin_index.rs`:

```rust
    #[test]
    fn partition_routes_records_to_their_home_package() {
        let corpus = r#"[
            {"kind":"type","name":"ZZ","package":"$Core$Core","ancestors":["$Core$Thing"]},
            {"kind":"type","name":"FooType","package":"$FooPkg$FooPkg","parent":"$Core$ZZ"},
            {"kind":"function","name":"fooFn","package":"$FooPkg$FooPkg"}
        ]"#;
        let index = BuiltinIndex::load(corpus);
        let parts = index.partition_by_package();

        let core = parts.get("Core").expect("Core partition present");
        assert!(core.type_entry("ZZ").is_some());
        assert!(core.type_entry("FooType").is_none(), "FooType is not in Core");

        let foo = parts.get("FooPkg").expect("FooPkg partition present");
        assert!(foo.type_entry("FooType").is_some());
        assert!(foo.callable("fooFn").is_some());
        assert!(foo.type_entry("ZZ").is_none(), "ZZ is not in FooPkg");
    }

    #[test]
    fn partition_of_real_corpus_yields_core() {
        let index = BuiltinIndex::load(include_str!("./data/m2-types.jsonc"));
        let parts = index.partition_by_package();
        let core = parts.get("Core").expect("Core partition present");
        // The whole Core corpus lands in the single Core partition.
        assert_eq!(core.type_count(), index.type_count());
        assert_eq!(core.callable_count(), index.callable_count());
    }
```

- [ ] **Step 3: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test -p m2-ls partition`
Expected: clippy clean; both tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/builtin_index.rs
git commit -m "Partition the builtin index by home package"
```

---

## Task 4: Per-package docs + `BuiltinData::from_index`

Two additive changes in `typesystem.rs`: (a) let docs be bucketed by their `package` field so each partition gets only its own hover pages, and (b) factor the post-parse half of `load_from_index` into `BuiltinData::from_index(&BuiltinIndex, docs)` so a `BuiltinData` can be built from an already-partitioned sub-index without re-serializing JSON. `load_from_index` keeps working (it routes through `from_index`).

**Files:**
- Modify: `src/typesystem.rs` (`DocRecord`, `load_docs_markdown_by_package`, `BuiltinData::from_index`, `load_from_index`; tests)

**Interfaces:**
- Consumes: `BuiltinIndex` (`builtin_index.rs`), `record_from_type`/`record_from_callable`/`record_from_object`/`register_record_keys` (private, same module), `TypeLattice::from_type_index`, `TypeFacts::from_type_index`
- Produces:
  - `pub(crate) fn load_docs_markdown_by_package(jsonl: &str) -> HashMap<String, HashMap<InstanceID, String>>`
  - `pub fn from_index(index: &crate::builtin_index::BuiltinIndex, docs: HashMap<InstanceID, String>) -> BuiltinData`

- [ ] **Step 1: Give `DocRecord` a package field**

In `src/typesystem.rs`, in the `load_docs_markdown` helper, extend the local `DocRecord` struct (currently `name` + `markdown`, ~line 1354) to also read `package`:

```rust
    struct DocRecord {
        name: String,
        #[serde(default)]
        package: Option<String>,
        #[serde(default)]
        markdown: String,
    }
```

(`load_docs_markdown` ignores `package` and is unchanged otherwise; the new field is consumed by the by-package variant below. If `DocRecord` is function-local, lift it to a module-level `#[derive(serde::Deserialize)]` struct so both helpers can deserialize into it; otherwise duplicate the same struct inside the new function. Prefer lifting — DRY.)

- [ ] **Step 2: Add `load_docs_markdown_by_package`**

Add next to `load_docs_markdown` in `src/typesystem.rs`:

```rust
/// Like `load_docs_markdown`, but bucketed by each record's `package` so every
/// partition receives only its own hover pages. Records with no package bucket
/// under `"Core"` (the loaded-set floor). Keyed by package name.
pub(crate) fn load_docs_markdown_by_package(jsonl: &str) -> HashMap<String, HashMap<InstanceID, String>> {
    let mut by_package: HashMap<String, HashMap<InstanceID, String>> = HashMap::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<DocRecord>(line) else {
            continue;
        };
        if doc.markdown.is_empty() {
            continue;
        }
        let package = doc.package.clone().unwrap_or_else(|| "Core".to_string());
        by_package
            .entry(package)
            .or_default()
            .entry(InstanceID::new(&doc.name))
            .or_insert(doc.markdown);
    }
    by_package
}
```

(If `DocRecord` was lifted to module scope in Step 1, this compiles against it. The `markdown` field is moved out of `doc`, matching `load_docs_markdown`'s pattern.)

- [ ] **Step 3: Factor out `BuiltinData::from_index`**

In `impl BuiltinData`, add `from_index` and rewrite `load_from_index` to call it:

```rust
    /// Build a `BuiltinData` from an already-parsed `BuiltinIndex` and a docs
    /// map. This is the post-parse half of `load_from_index`, split out so the
    /// package-partitioned index can build one `BuiltinData` per partition from
    /// a sub-index without re-serializing JSON.
    pub fn from_index(
        index: &crate::builtin_index::BuiltinIndex,
        docs: HashMap<InstanceID, String>,
    ) -> Self {
        let type_lattice = TypeLattice::from_type_index(index);
        let type_facts = TypeFacts::from_type_index(index);

        let mut names = Vec::new();
        let mut name_to_index = HashMap::new();
        let mut records = Vec::new();

        for entry in index.types() {
            let id = records.len();
            register_record_keys(&mut name_to_index, &entry.name, &entry.aliases, id);
            names.push(InstanceID::new(&entry.name));
            records.push(record_from_type(entry));
        }
        for entry in index.callables() {
            let id = records.len();
            register_record_keys(&mut name_to_index, &entry.name, &entry.aliases, id);
            names.push(InstanceID::new(&entry.name));
            records.push(record_from_callable(entry));
        }
        for entry in index.objects() {
            let id = records.len();
            register_record_keys(&mut name_to_index, &entry.name, &entry.aliases, id);
            names.push(InstanceID::new(&entry.name));
            records.push(record_from_object(entry));
        }

        BuiltinData {
            names,
            name_to_index,
            records,
            docs,
            type_facts,
            type_lattice,
        }
    }

    pub fn load_from_index(types_jsonl: &str, docs_jsonl: &str) -> Self {
        let index = crate::builtin_index::BuiltinIndex::load(types_jsonl);
        Self::from_index(index_ref(&index), load_docs_markdown(docs_jsonl))
    }
```

Note: `from_index` takes `&BuiltinIndex`, so the last line is simply:

```rust
    pub fn load_from_index(types_jsonl: &str, docs_jsonl: &str) -> Self {
        let index = crate::builtin_index::BuiltinIndex::load(types_jsonl);
        Self::from_index(&index, load_docs_markdown(docs_jsonl))
    }
```

(Use this second form — there is no `index_ref` helper; that was only to highlight `from_index` borrows. Delete the original body of `load_from_index` entirely; it now lives in `from_index`.)

- [ ] **Step 4: Add verifying tests**

Add to the `#[cfg(test)]` module in `src/typesystem.rs`:

```rust
    #[test]
    fn from_index_and_load_from_index_agree_on_core() {
        // load_from_index must remain byte-for-byte equivalent to building from
        // a parsed index + global docs — the refactor changes structure, not output.
        let types = include_str!("./data/m2-types.jsonc");
        let docs = include_str!("./data/m2-docs.jsonl");
        let direct = BuiltinData::load_from_index(types, docs);

        let index = crate::builtin_index::BuiltinIndex::load(types);
        let via_from_index = BuiltinData::from_index(&index, load_docs_markdown(docs));

        assert_eq!(direct.names, via_from_index.names);
        assert_eq!(direct.records.len(), via_from_index.records.len());
        assert!(via_from_index.get_record(&InstanceID::new("ZZ")).is_some());
    }

    #[test]
    fn docs_partition_buckets_by_package() {
        let jsonl = concat!(
            r#"{"name":"foo","package":"Core","markdown":"# foo"}"#,
            "\n",
            r#"{"name":"bar","package":"FooPkg","markdown":"# bar"}"#,
        );
        let by_pkg = load_docs_markdown_by_package(jsonl);
        assert!(by_pkg["Core"].contains_key(&InstanceID::new("foo")));
        assert!(by_pkg["FooPkg"].contains_key(&InstanceID::new("bar")));
        assert!(!by_pkg["Core"].contains_key(&InstanceID::new("bar")));
    }
```

(If `BuiltinData.names`/`.records` are private and not reachable from the test module, assert equivalence through the public API instead: compare `direct.get_record(&InstanceID::new("ZZ"))` and `via_from_index.get_record(...)` for a handful of representative names — `ZZ`, `gb`, `ideal` — and `direct.contains_name(..)` parity. Do not add public accessors solely for the test.)

- [ ] **Step 5: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test -p m2-ls`
Expected: clippy clean; the two new tests PASS; **all pre-existing tests still PASS** (the refactor is behavior-preserving).

- [ ] **Step 6: Commit**

```bash
git add src/typesystem.rs
git commit -m "Factor BuiltinData::from_index and per-package docs loading"
```

---

## Task 5: `PackagePartitionedIndex` + `LoadedPackages`

The new module ties Tasks 2–4 together: parse once, partition, build a `BuiltinData` per package, and record the default-loaded baseline. Also defines `LoadedPackages`, the ordered in-scope set derived purely from document text. Nothing here is queried yet (P3) — `#![allow(dead_code)]` covers the forward-looking API, matching `builtin_index.rs`.

**Files:**
- Create: `src/partitioned_index.rs`
- Test: inline `#[cfg(test)]` in the same file

**Interfaces:**
- Consumes: `BuiltinIndex::load`, `BuiltinIndex::partition_by_package`, `BuiltinIndex::default_loaded`, `BuiltinData::from_index`, `load_docs_markdown_by_package`, `collect_imported_packages`
- Produces:
  - `pub(crate) struct PackagePartitionedIndex` with:
    - `pub fn from_corpus(types_jsonl: &str, docs_jsonl: &str) -> Self`
    - `pub fn partition(&self, package: &str) -> Option<&BuiltinData>`
    - `pub fn default_loaded(&self) -> &[String]`
    - `pub fn packages(&self) -> impl Iterator<Item = &str>`
  - `pub(crate) struct LoadedPackages(Vec<String>)` with:
    - `pub fn resolve(default_loaded: &[String], text: &str) -> Self`
    - `pub fn as_slice(&self) -> &[String]`

- [ ] **Step 1: Create the module**

Create `src/partitioned_index.rs`:

```rust
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
```

- [ ] **Step 2: Add verifying tests**

Append to `src/partitioned_index.rs`:

```rust
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
        assert!(core.get_record(&crate::typesystem::InstanceID::new("ZZ")).is_some());
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
        assert_eq!(loaded.as_slice(), &["Core".to_string(), "FooPkg".to_string()]);
    }
}
```

- [ ] **Step 3: Register the module**

In `src/main.rs`, add alongside the other `mod` declarations:

```rust
mod partitioned_index;
```

(Place it in the existing module-declaration block near the top, in alphabetical position next to `mod package_index;`.)

- [ ] **Step 4: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test -p m2-ls partitioned_index`
Expected: clippy clean (no dead-code warnings — `#![allow(dead_code)]` covers the not-yet-consumed API); the four tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/partitioned_index.rs src/main.rs
git commit -m "Add PackagePartitionedIndex and LoadedPackages tracker"
```

---

## Task 6: Wire the partition into `Backend` and re-source `self.builtins`

Build a `PackagePartitionedIndex` in `Backend::new` and make `self.builtins` the Core partition, so the two construction paths cannot drift — the existing test suite becomes the faithfulness proof. Add a `loaded_packages(text)` convenience. **No query site is rewired** (P3).

**Files:**
- Modify: `src/main.rs` (the `Backend` struct, `Backend::new`, add `loaded_packages`)

**Interfaces:**
- Consumes: `PackagePartitionedIndex::{from_corpus, partition, default_loaded}`, `LoadedPackages::resolve`
- Produces:
  - `Backend.partitioned: PackagePartitionedIndex` (field)
  - `fn loaded_packages(&self, text: &str) -> LoadedPackages` (method on `Backend`)

- [ ] **Step 1: Add the field**

In `src/main.rs`, add to the `Backend` struct (next to `builtins: BuiltinData`, ~line 61):

```rust
    partitioned: PackagePartitionedIndex,
```

Add the import near the other crate-internal `use`s:

```rust
use crate::partitioned_index::{LoadedPackages, PackagePartitionedIndex};
```

- [ ] **Step 2: Build it in `Backend::new` and re-source `builtins`**

Replace the `builtins` construction (currently at `src/main.rs:73`):

```rust
        let builtins = BuiltinData::load_from_index(
            include_str!("./data/m2-types.jsonc"),
            include_str!("./data/m2-docs.jsonl"),
        );
```

with:

```rust
        let partitioned = PackagePartitionedIndex::from_corpus(
            include_str!("./data/m2-types.jsonc"),
            include_str!("./data/m2-docs.jsonl"),
        );
        // `self.builtins` is the Core partition — the new partitioned path and
        // the legacy single-blob path share one source so they cannot drift.
        // Core is always present (it is the loaded-set floor); its absence is a
        // corrupt corpus, so fail fast.
        let builtins = partitioned
            .partition("Core")
            .expect("Core partition present in builtin corpus")
            .clone();
```

Then add `partitioned` to the struct initializer (next to `builtins,`):

```rust
            partitioned,
```

- [ ] **Step 3: Add the `loaded_packages` method**

Add to `impl Backend` (near `active_package_indexes`, ~line 101):

```rust
    /// The ordered in-scope package set for a document: the partitioned index's
    /// default-loaded baseline plus the document's imports. Pure function of the
    /// text. Not yet consulted by query routing (P3) — provided so that work has
    /// a single source for "what is loaded here".
    fn loaded_packages(&self, text: &str) -> LoadedPackages {
        LoadedPackages::resolve(self.partitioned.default_loaded(), text)
    }
```

- [ ] **Step 4: Add a verifying test**

Add to the test module in `src/main.rs` (if `Backend::new` is reachable from tests; many tower-lsp servers expose it as `pub(crate)`). If `Backend` is awkward to construct in a unit test (it holds a `Client`), skip the construction test and rely on the suite-wide green bar plus this targeted check of the field plumbing through a small helper test in `partitioned_index.rs` already covered in Task 5. In that case, add no test here and note it in the task report.

If `Backend::new` *is* test-constructible, add:

```rust
    #[test]
    fn builtins_equals_core_partition() {
        let backend = /* construct Backend via its test constructor */;
        assert!(backend.builtins.get_record(&InstanceID::new("ZZ")).is_some());
        assert!(backend
            .partitioned
            .partition("Core")
            .unwrap()
            .get_record(&InstanceID::new("ZZ"))
            .is_some());
        assert_eq!(backend.loaded_packages("").as_slice(), &["Core".to_string()]);
    }
```

(Do not fabricate a `Client`; if there is no existing test constructor for `Backend`, do not invent one — the suite-wide green bar already proves the Core partition drives every Core-dependent test. State which path you took in the report.)

- [ ] **Step 5: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test -p m2-ls`
Expected: clippy clean; **the entire pre-existing suite stays green** (this is the proof that the Core partition faithfully reproduces the old `self.builtins`); any new test PASSES.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "Source self.builtins from the Core partition; add loaded_packages"
```

---

## Roadmap (not part of P2)

- **P3 — `ScopedIndex` + rewire queries.** Introduce a delegating scoped view over the loaded partitions (baseline/Core-first, then import-order) exposing the `BuiltinData` query API; route inference, hover, completion, navigation, semantic tokens, and type-hierarchy through it (the ~20 call sites mapped in the design); retire the `PackageIndexer`-vs-`builtins` split and `BuiltinData::load_from_split` once the legacy `.names`/`.details` path is gone. The synthetic 2-package fixture (TestPkg parented to a Core type) proves loaded ⇒ resolves + `is_subtype` spans to Core, not-loaded ⇒ silent.
- **Single-file consolidation (with fundocs P4).** When fundocs emits `m2-index.jsonl` (one file, leading `meta` record, `markdown` folded per record), switch the loader to JSONL line parsing, read `markdown` from each record, and drop the separate `m2-docs.jsonl` `include_str!`. `default_loaded` then comes from the real `meta` record and the Task 5 fallback stops being exercised.

## Self-Review

- **Spec coverage.** Design §Components: `PackagePartitionedIndex` (Task 5), `LoadedPackages` (Tasks 5–6), import lifecycle as a pure function of text (Task 5 `resolve`, Task 6 `loaded_packages`). Design §Phasing P2: partition by record package (Task 3), read folded `default_loaded` (Task 2 + Task 5 fallback), promote the tracker (Tasks 5–6). Design §"package-import triggers ... plus `importFrom`" (Task 1). `ScopedIndex` and query rewiring are explicitly deferred to P3 (roadmap) per the phasing — not a gap.
- **Placeholder scan.** No "TBD"/"handle edge cases"/"similar to Task N". Task 6 Step 4 is conditional by necessity (it depends on whether `Backend` is test-constructible, which the implementer must check); both branches are spelled out with the rule "do not fabricate a `Client`".
- **Type consistency.** `from_index(&BuiltinIndex, HashMap<InstanceID, String>)` (Task 4) is consumed verbatim in Task 5. `partition_by_package() -> HashMap<String, BuiltinIndex>` (Task 3) consumed in Task 5. `default_loaded() -> &[String]` (Task 2) consumed in Task 5. `LoadedPackages::resolve(&[String], &str)` / `as_slice()` (Task 5) consumed in Task 6. `load_docs_markdown_by_package` is `pub(crate)` (Task 4) and called cross-module in Task 5. Bucket-default package is `"Core"` consistently in Tasks 3, 4, 5.
