# Single-File `m2-index.jsonl` Consolidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the two embedded corpus assets (`m2-types.jsonc` + `m2-docs.jsonl`) with the single combined `m2-index.jsonl` — one per-line JSONL file, `markdown` folded into each record, and a mandatory leading `meta` record carrying `default_loaded`.

**Architecture:** `BuiltinIndex::load` parses JSONL line-by-line (no array, no `//` header), records carry their own `markdown` (lifted onto each `TypeEntry`/`CallableEntry`/`ObjectEntry`), partitioning carries markdown to the right partition, `BuiltinData::from_index` builds each partition's docs map from entry markdown, and `PackagePartitionedIndex::from_corpus` takes a single corpus string and fails fast if the `meta` record is absent. The retired hover path (`load_docs_markdown*`, `DocRecord`, the two-arg `load_from_index`) is deleted. Everything is set-agnostic: the same code partitions today's 22 packages and a future ~300 with no change. `self.builtins` stays the Core partition; the legacy `PackageIndexer`/`load_from_split` runtime path is untouched (it is P3's concern).

**Tech Stack:** Rust, `tower-lsp`, `serde`/`serde_json`. Crate package name `m2-ls` (`cargo test -p m2-ls`). M2 corpus is a build-time static asset embedded via `include_str!`.

## Global Constraints

Every task's requirements implicitly include this section.

- **Set-agnostic.** No test may assert an exact package count, a fixed total record count, or that only Core exists. Assert structure (a named partition is present/absent; the partition is a true partition of the whole). The corpus has 22 home packages today (20 autoloaded + `Text` + `JSON` as deliberate non-default fixtures) and grows toward ~300; code and tests must not encode "22" or "Core-only".
- **Fail fast, no defensive coercion.** Panic on a malformed corpus line, on a non-`meta` record with an empty `name`, and on an absent `meta` record. Never default ill-typed input or silently skip a structurally broken record. (Blank lines are not records — skip those.)
- **Monotone, known-facts-only.** Absence = unknown. Omitted/empty `markdown` ⇒ no doc entry (not `""`). Never synthesize a positive fact. `Thing`/`Any` codomains stay dropped to `None` (existing `concrete_codomain` behavior — do not change it).
- **Semantic types.** `markdown` is `Option<String>` on each entry (`None` = undocumented), never an empty `String` sentinel.
- **`typesystem.rs` is live WIP.** Make only the additive/surgical edits this plan names. Do not tidy, rename, or restyle surrounding code.
- **Do not touch `PackageIndexer` / `load_from_split` / `active_package_indexes`.** The runtime per-package disk-cache path is retired in P3, not here.
- **Logic first, tests verify after.** Write the implementation, then the tests that check it. Delete tests of removed logic rather than retrofitting them to dead code.
- **Build gate.** `cargo fmt --all` clean, `cargo test -p m2-ls` fully green, and your diff introduces **no new** clippy warnings beyond `main`'s pre-existing baseline (currently 9, all in WIP files). Do not attempt to fix the pre-existing 9.
- **Core behavior unchanged.** `self.builtins` is the Core partition; every existing Core-symbol test (hover, inference, semantic tokens, document symbols, …) stays green.

---

## File Structure

- `src/data/m2-index.jsonl` — **created** (copied from `~/m2/fundocs/m2-index.jsonl`). The single embedded corpus. `src/data/m2-types.jsonc` and `src/data/m2-docs.jsonl` are **deleted**.
- `src/builtin_index.rs` — `BuiltinIndex::load` flips to JSONL; `markdown` lifted onto the three entry structs and `RawRecord`; non-`meta` name validation; tests updated.
- `src/typesystem.rs` — `DocRecord`, `load_docs_markdown`, `load_docs_markdown_by_package` deleted; `from_index` drops its `docs` parameter and builds docs from entry markdown; `load_from_index` becomes single-arg; the equivalence test is deleted; remaining test helpers updated.
- `src/partitioned_index.rs` — `from_corpus` becomes single-arg with mandatory-`meta` fail-fast; tests updated; non-default-partition coverage added (Task 2).
- `src/main.rs` — `Backend::new` reads the single file; test helper updated.
- All other `BuiltinData::load_from_index(...)` / `BuiltinIndex::load(...)` call sites (`document.rs`, `analysis.rs`, `capabilities/hover.rs`, `capabilities/document_symbols.rs`, `capabilities/semantic_tokens.rs`) — mechanically retargeted to the single file.

---

### Task 1: Flip the corpus to single-file JSONL (atomic format migration)

This is one task on purpose: changing `BuiltinIndex::load`'s input format and the `load_from_index`/`from_index`/`from_corpus` signatures is a compile-and-test coupling that cannot land green in smaller pieces (the moment the parser expects JSONL, every caller must feed it the new file, and the moment `load_from_index` is single-arg, every call site must change). The genuinely new *logic* is small (JSONL parse, markdown folding, name + meta validation); the bulk is mechanical call-site retargeting.

**Files:**
- Create: `src/data/m2-index.jsonl` (copy of `~/m2/fundocs/m2-index.jsonl`)
- Delete: `src/data/m2-types.jsonc`, `src/data/m2-docs.jsonl`
- Modify: `src/builtin_index.rs`, `src/typesystem.rs`, `src/partitioned_index.rs`, `src/main.rs`, `src/document.rs`, `src/analysis.rs`, `src/capabilities/hover.rs`, `src/capabilities/document_symbols.rs`, `src/capabilities/semantic_tokens.rs`

**Interfaces:**
- Produces:
  - `BuiltinIndex::load(corpus: &str) -> BuiltinIndex` — now parses per-line JSONL; panics on a malformed line or a non-`meta` record with empty `name`.
  - `TypeEntry.markdown: Option<String>`, `CallableEntry.markdown: Option<String>`, `ObjectEntry.markdown: Option<String>`.
  - `BuiltinData::from_index(index: &BuiltinIndex) -> BuiltinData` — **no `docs` parameter**; builds the docs map from entry markdown.
  - `BuiltinData::load_from_index(corpus: &str) -> BuiltinData` — **single argument**; `= Self::from_index(&BuiltinIndex::load(corpus))`.
  - `PackagePartitionedIndex::from_corpus(corpus: &str) -> PackagePartitionedIndex` — **single argument**; panics if `meta`/`default_loaded` is absent.
- Consumes: `BuiltinData::contains_name`, `BuiltinData::get_record`, `InstanceID::new` (unchanged).

---

- [ ] **Step 1: Copy the new corpus into the repo**

```bash
cp ~/m2/fundocs/m2-index.jsonl src/data/m2-index.jsonl
# sanity: line 1 is the meta record, remaining lines are objects
head -1 src/data/m2-index.jsonl | grep -q '"kind": "meta"' && echo "meta OK"
wc -l src/data/m2-index.jsonl   # expect 1742 (1 meta + 1741 objects)
```
Expected: `meta OK` and `1742 src/data/m2-index.jsonl`.

- [ ] **Step 2: Add `markdown` to `RawRecord` and the three entry structs**

In `src/builtin_index.rs`, add a field to `TypeEntry` (after `instances`):

```rust
    pub instances: Vec<String>,
    /// Rendered hover markdown for this entry, folded into the record by the
    /// corpus generator. `None` ⇒ undocumented (monotone: absent, not empty).
    pub markdown: Option<String>,
```

Add the same field to `ObjectEntry` (after `class`) and to `CallableEntry` (after `signatures`):

```rust
    /// Rendered hover markdown, folded into the record. `None` ⇒ undocumented.
    pub markdown: Option<String>,
```

Add to `RawRecord` (after the `default_loaded` field):

```rust
    #[serde(default)]
    markdown: Option<String>,
```

- [ ] **Step 3: Rewrite `BuiltinIndex::load` to parse JSONL with fail-fast validation**

Replace the whole body of `pub fn load(corpus: &str) -> Self` (from the JSONC strip-and-array-parse through the end of the record loop) with:

```rust
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
```

Note: `partition_by_package` clones whole entries, so `markdown` rides into each partition automatically — no change there. Update the module doc comment at the top of `src/builtin_index.rs` (line 1) from `data/m2-types.jsonc` to `data/m2-index.jsonl`.

- [ ] **Step 4: Drop the `docs` parameter from `from_index`; build docs from entry markdown**

In `src/typesystem.rs`, replace `pub fn from_index(index: &..., docs: HashMap<InstanceID, String>) -> Self` with:

```rust
    /// Build a `BuiltinData` from an already-parsed `BuiltinIndex`. Hover
    /// markdown is folded into each entry by the corpus generator, so the docs
    /// map is built here from the entries themselves — no separate docs asset.
    pub fn from_index(index: &crate::builtin_index::BuiltinIndex) -> Self {
        let type_lattice = TypeLattice::from_type_index(index);
        let type_facts = TypeFacts::from_type_index(index);

        let mut names = Vec::new();
        let mut name_to_index = HashMap::new();
        let mut records = Vec::new();
        let mut docs = HashMap::new();

        for entry in index.types() {
            let id = records.len();
            register_record_keys(&mut name_to_index, &entry.name, &entry.aliases, id);
            names.push(InstanceID::new(&entry.name));
            if let Some(md) = &entry.markdown {
                docs.entry(InstanceID::new(&entry.name))
                    .or_insert_with(|| md.clone());
            }
            records.push(record_from_type(entry));
        }
        for entry in index.callables() {
            let id = records.len();
            register_record_keys(&mut name_to_index, &entry.name, &entry.aliases, id);
            names.push(InstanceID::new(&entry.name));
            if let Some(md) = &entry.markdown {
                docs.entry(InstanceID::new(&entry.name))
                    .or_insert_with(|| md.clone());
            }
            records.push(record_from_callable(entry));
        }
        for entry in index.objects() {
            let id = records.len();
            register_record_keys(&mut name_to_index, &entry.name, &entry.aliases, id);
            names.push(InstanceID::new(&entry.name));
            if let Some(md) = &entry.markdown {
                docs.entry(InstanceID::new(&entry.name))
                    .or_insert_with(|| md.clone());
            }
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
```

- [ ] **Step 5: Make `load_from_index` single-arg; delete the retired hover-path code**

In `src/typesystem.rs`, replace `pub fn load_from_index(types_jsonl: &str, docs_jsonl: &str) -> Self` (and its doc comment) with:

```rust
    /// Build a `BuiltinData` over the whole combined corpus (`m2-index.jsonl`).
    /// Production routes through `PackagePartitionedIndex::from_corpus` and uses
    /// only the Core partition; this whole-corpus convenience is for tests.
    #[allow(dead_code)] // whole-corpus convenience; production uses from_corpus + Core partition
    pub fn load_from_index(corpus: &str) -> Self {
        Self::from_index(&crate::builtin_index::BuiltinIndex::load(corpus))
    }
```

Delete entirely: the `DocRecord` struct, `fn load_docs_markdown`, and `pub(crate) fn load_docs_markdown_by_package` (all three in `src/typesystem.rs`). Update the doc comments at `src/typesystem.rs` lines ~222, ~316, ~480, and on `TypeLattice`/`TypeFacts` builders that name `m2-types.jsonc`/`m2-docs.jsonl` to read `m2-index.jsonl` (factual file-name fixes only — do not restructure).

- [ ] **Step 6: Make `from_corpus` single-arg with mandatory-`meta` fail-fast**

In `src/partitioned_index.rs`, change the `use` line

```rust
use crate::typesystem::{load_docs_markdown_by_package, BuiltinData};
```

to

```rust
use crate::typesystem::BuiltinData;
```

and replace `pub fn from_corpus(types_jsonl: &str, docs_jsonl: &str) -> Self { ... }` with:

```rust
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
```

- [ ] **Step 7: Retarget `Backend::new` and every remaining call site to the single file**

In `src/main.rs`, `Backend::new`:

```rust
        let partitioned = PackagePartitionedIndex::from_corpus(include_str!("./data/m2-index.jsonl"));
```

Apply this canonical transform to **every** test call site (preserve each file's existing relative prefix — `./data/` for crate-root files, `../data/` for files under `src/capabilities/`):

```rust
// before
BuiltinData::load_from_index(
    include_str!("<PREFIX>/data/m2-types.jsonc"),
    include_str!("<PREFIX>/data/m2-docs.jsonl"),
)
// after
BuiltinData::load_from_index(include_str!("<PREFIX>/data/m2-index.jsonl"))
```

and for any direct index load:

```rust
// before
BuiltinIndex::load(include_str!("<PREFIX>/data/m2-types.jsonc"))
// after
BuiltinIndex::load(include_str!("<PREFIX>/data/m2-index.jsonl"))
```

Find every site to change with:

```bash
grep -rn 'm2-types\.jsonc\|m2-docs\.jsonl' src/
```

Expected sites (test helpers unless noted): `src/main.rs` (line ~1047), `src/document.rs` (~218), `src/analysis.rs` (×5), `src/capabilities/hover.rs` (×1), `src/capabilities/document_symbols.rs` (×2), `src/capabilities/semantic_tokens.rs` (×6), and the remaining `src/builtin_index.rs` / `src/partitioned_index.rs` / `src/typesystem.rs` sites handled in the next steps. After this step, `grep -rn 'm2-types\.jsonc\|m2-docs\.jsonl' src/` over non-comment lines must return nothing.

- [ ] **Step 8: Delete the old data files**

```bash
git rm src/data/m2-types.jsonc src/data/m2-docs.jsonl
```

- [ ] **Step 9: Fix the `builtin_index.rs` tests for the new format**

In `src/builtin_index.rs` tests:

- `fn index()` and the two `load_parses_new_format_corpus` / standalone `BuiltinIndex::load(include_str!(...))` sites: change `m2-types.jsonc` → `m2-index.jsonl`.
- In `load_parses_new_format_corpus`, add a folded-markdown assertion:

```rust
        // markdown is now folded onto the entry (documented Core type).
        assert!(zz.markdown.is_some(), "ZZ should carry folded hover markdown");
```

- Rewrite the synthetic-corpus tests from JSONC arrays to JSONL (one object per line, no `[`/`]`, no commas). Replace `captures_default_loaded_from_meta_record`:

```rust
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
```

Replace `default_loaded_is_empty_without_meta_record`:

```rust
    #[test]
    fn default_loaded_is_empty_without_meta_record() {
        let corpus = r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#;
        let index = BuiltinIndex::load(corpus);
        assert!(index.default_loaded().is_empty());
    }
```

Replace `partition_routes_records_to_their_home_package`:

```rust
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
```

- Replace `partition_of_real_corpus_yields_core` (Core is now a proper subset; assert a true partition instead):

```rust
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
```

- Add a fail-fast test for the unnamed non-meta record:

```rust
    #[test]
    #[should_panic(expected = "has no name")]
    fn load_panics_on_unnamed_non_meta_record() {
        BuiltinIndex::load(r#"{"kind":"type","package":"$Core$Core"}"#);
    }
```

- [ ] **Step 10: Fix the `typesystem.rs` tests**

In `src/typesystem.rs` tests:

- Delete the test `from_index_and_load_from_index_agree_on_core` (its oracle `load_from_index(types, docs)` and `load_docs_markdown` no longer exist).
- Delete the test that calls `load_docs_markdown_by_package` (around line ~1919) — the function is gone; its per-package routing is now covered by `from_corpus` in `partitioned_index.rs`.
- Retarget the remaining sites: `BuiltinIndex::load(include_str!("./data/m2-types.jsonc"))` → `m2-index.jsonl`; `BuiltinData::load_from_index(types, docs)` → `BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"))`.

- [ ] **Step 11: Fix the `partitioned_index.rs` tests (corpus helper + mandatory meta)**

In `src/partitioned_index.rs` tests, collapse the two corpus helpers to one and update calls:

```rust
    fn corpus() -> &'static str {
        include_str!("./data/m2-index.jsonl")
    }
```

(delete the `docs()` helper). Then:

- `from_corpus_builds_a_core_partition`: call `PackagePartitionedIndex::from_corpus(corpus())`.
- Delete `default_loaded_falls_back_to_present_packages_without_meta` (there is no fallback now) and replace it with a fail-fast test:

```rust
    #[test]
    #[should_panic(expected = "missing the mandatory leading `meta` record")]
    fn from_corpus_panics_without_meta() {
        // A corpus with object records but no meta line is corrupt.
        PackagePartitionedIndex::from_corpus(r#"{"kind":"type","name":"ZZ","package":"$Core$Core"}"#);
    }
```

- `default_loaded_uses_meta_record_when_present`: rewrite the synthetic corpus to JSONL + single-arg call:

```rust
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
```

- `loaded_packages_is_baseline_then_imports_deduped`: unchanged (it does not read the corpus).

- [ ] **Step 12: Format, build, and run the full suite**

```bash
cargo fmt --all
cargo test -p m2-ls
```
Expected: fmt makes no further changes; all tests pass (the count rises by the new fail-fast/markdown tests and falls by the deleted equivalence/doc-routing tests). Verify clippy adds nothing new:

```bash
cargo clippy --all-targets -p m2-ls 2>&1 | grep -c '^warning'
```
Expected: `9` (unchanged from `main`'s baseline).

- [ ] **Step 13: Commit**

```bash
git add -A
git commit -m "Consolidate corpus to single m2-index.jsonl (folded markdown, mandatory meta)"
```

---

### Task 2: Exercise the non-default partitions (Text/JSON fixtures)

The corpus deliberately ships two non-autoloaded packages (`Text`, `JSON`) so loaded-package scoping has real partitions to prove against. This task adds the coverage that locks in their semantics: they form their own partitions, stay out of the default baseline, stay out of Core (so `self.builtins` cannot resolve them), and `LoadedPackages` picks them up only when a document imports them. This is additive — tests only.

**Files:**
- Modify: `src/partitioned_index.rs` (tests module)

**Interfaces:**
- Consumes: `PackagePartitionedIndex::from_corpus`, `::partition`, `::default_loaded`; `LoadedPackages::resolve`, `::as_slice`; `BuiltinData::contains_name`.

- [ ] **Step 1: Write the non-default-partition tests**

Add to the `tests` module in `src/partitioned_index.rs`:

```rust
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

        // ...and absent from Core, so self.builtins (the Core partition) cannot
        // resolve it until P3 routes imports through loaded partitions.
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
```

- [ ] **Step 2: Run the new tests**

```bash
cargo test -p m2-ls partitioned_index
```
Expected: PASS, including `curated_extras_are_non_default_partitions` and `loaded_packages_picks_up_an_imported_extra`.

- [ ] **Step 3: Format and full suite**

```bash
cargo fmt --all
cargo test -p m2-ls
```
Expected: fmt clean; all tests pass.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "Cover non-default Text/JSON partitions as loaded-scoping fixtures"
```

---

## Self-Review

**1. Spec coverage** (against `docs/superpowers/specs/2026-06-21-loaded-package-scoping-design.md` "File & loading shape" + the P3 carryover in memory `typecheck-index-schema`):
- One file `m2-index.jsonl`, types+markdown merged → Task 1 (steps 1–8). ✓
- Single embedded file via `include_str!`, partition in memory → Task 1 (step 7, `Backend::new`). ✓
- Folded `default_loaded` metadata as baseline → Task 1 (step 6, mandatory). ✓
- "switch loader to per-line JSONL + validate non-meta name" → Task 1 (steps 3, 9). ✓
- "tighten the default_loaded fallback to fail-fast when meta becomes mandatory" → Task 1 (step 6). ✓
- Non-default packages reachable only when imported (the design's loaded-only proof, now on real Text/JSON) → Task 2. ✓
- Out of scope here (correctly deferred to P3): `ScopedIndex`, rewiring the ~20 query sites, retiring `PackageIndexer`/`load_from_split`. Not in any task. ✓

**2. Placeholder scan:** No "TBD"/"handle errors"/"similar to". Every code step shows complete code; mechanical call-site edits give the exact before/after transform plus the grep that enumerates them. ✓

**3. Type consistency:** `markdown: Option<String>` is consistent across `RawRecord`, `TypeEntry`, `CallableEntry`, `ObjectEntry`, and the `from_index` reader. `from_index(&BuiltinIndex)` (single arg) and `from_corpus(&str)` (single arg) and `load_from_index(&str)` (single arg) are used consistently in every updated call site and test. `contains_name`, `partition`, `default_loaded`, `as_slice` match their existing signatures. ✓

**Note on granularity:** Task 1 is larger than a typical bite-sized task because the parser-format and constructor-signature changes are a single compile/test-coupled unit — no smaller slice stays green. The new *logic* (steps 2–6) is small and is where review attention belongs; steps 7–11 are mechanical retargeting and test-format updates enumerated exactly.
