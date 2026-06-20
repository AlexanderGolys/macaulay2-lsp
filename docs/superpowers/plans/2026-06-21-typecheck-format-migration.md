# Typecheck Index Format Migration (P1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the LSP's static builtin loader from the old `m2-types.jsonl` schema to fundocs's new compressed format, retaining each record's home package so later phases can partition and scope by it.

**Architecture:** Replace the line-delimited old-format parse in `builtin_index.rs` with a JSONC-array parse of the new corpus: strip the `//` header, deref `$Package$Name` reference keys to bare names, map the new `kind` vocabulary, read operator metadata from the top-level `operator` object. Hover markdown continues to load from the existing `m2-docs.jsonl` (unchanged) — single-file consolidation is a later phase. This is P1 of three; it bootstraps on the existing Core-only new-format file with no fundocs dependency.

**Tech Stack:** Rust, `serde`/`serde_json`, `tree-sitter-macaulay2`.

## Global Constraints

- Monotone, known-facts-only: never store `Thing`/`Any` as a positive codomain; an absent codomain stays `None`. (Copied from the typecheck-index design.)
- Fail-fast: do not mask broken records with defaults; a record that does not match the schema is skipped (same as today's `let Ok(raw) = … else continue`), not coerced.
- Self-documenting names; no primitive obsession in new helpers.
- Run `cargo fmt` and `cargo clippy` as part of building; both must be clean before a commit.
- Full design: `docs/superpowers/specs/2026-06-21-loaded-package-scoping-design.md`.

---

### Task 1: `deref_ref` reference-key helper

The new format encodes every cross-reference as `$Package$Name` (or a bare name when unresolved). The whole type system keys on bare `InstanceID`s, so references must be dereferenced to bare names at load. This helper is the single point that does it.

**Files:**
- Modify: `src/builtin_index.rs` (add the free function near the other module helpers, e.g. just above `register_keys`)
- Test: `src/builtin_index.rs` (the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `fn deref_ref(key: &str) -> String` — `$Core$ZZ` → `ZZ`, `$Core$Core` → `Core`, bare `ComplexMap` → `ComplexMap`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/builtin_index.rs`:

```rust
#[test]
fn deref_ref_strips_package_qualifier_and_passes_bare_names_through() {
    assert_eq!(deref_ref("$Core$ZZ"), "ZZ");
    assert_eq!(deref_ref("$Core$RingElement"), "RingElement");
    assert_eq!(deref_ref("$Core$Core"), "Core"); // package/class refs too
    assert_eq!(deref_ref("ComplexMap"), "ComplexMap"); // unresolved, no prefix
    assert_eq!(deref_ref("RingElement"), "RingElement"); // already bare
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p m2-ls deref_ref_strips -- --nocapture`
Expected: FAIL to compile — `cannot find function deref_ref`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/builtin_index.rs`:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p m2-ls deref_ref_strips`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -q
git add src/builtin_index.rs
git commit -m "builtin_index: add deref_ref for \$Package\$Name keys"
```

---

### Task 2: Migrate `RawRecord`/`RawMethod` + `load()` to the new format

This is the atomic migration: the deserialization structs, the `load()` parse path, and the embedded data file change together (Rust cannot compile a half-migrated loader, and the new structs cannot parse the old file). After this task the index is built from `m2-types.jsonc`.

**Files:**
- Create: `src/data/m2-types.jsonc` (copied from fundocs)
- Delete: `src/data/m2-types.jsonl`
- Modify: `src/builtin_index.rs:106-172` (`load`), `:221-260` (`RawRecord`/`RawMethod`), and add `RawOperator`
- Modify (path swap only): every `include_str!("…/m2-types.jsonl")` site — `src/builtin_index.rs`, `src/document.rs`, `src/typesystem.rs`, `src/main.rs`, `src/analysis.rs`, `src/capabilities/hover.rs`, `src/capabilities/document_symbols.rs`, `src/capabilities/semantic_tokens.rs`
- Test: `src/builtin_index.rs` (`tests` module)

**Interfaces:**
- Consumes: `deref_ref` (Task 1); existing `TypeEntry`/`CallableEntry`/`ObjectEntry`/`Signature`/`OptionSpec`/`register_keys`/`collect_forms`.
- Produces: a `BuiltinIndex::load(&str)` that parses the new JSONC-array format. `TypeEntry`/`CallableEntry`/`ObjectEntry` field shapes are unchanged (downstream `typesystem.rs` builders keep working); only their *source* parse changes. `OptionSpec` field names already match the new schema (`possibleValues`/`valueType`).

- [ ] **Step 1: Bring in the new-format data file**

```bash
cp ~/m2/fundocs/m2-types.jsonc src/data/m2-types.jsonc
head -4 src/data/m2-types.jsonc   # confirm: // header, then "[", then records
```

- [ ] **Step 2: Write the failing loader test**

Add to the `tests` module in `src/builtin_index.rs` (asserts the four schema shifts: JSONC parse, deref, new kinds, operator forms):

```rust
#[test]
fn load_parses_new_format_corpus() {
    let index = BuiltinIndex::load(include_str!("./data/m2-types.jsonc"));

    // type record: parent/ancestors deref'd to bare names
    let zz = index.type_entry("ZZ").expect("ZZ type present");
    assert_eq!(zz.package.as_deref(), Some("Core")); // $Core$Core -> Core
    assert!(zz.ancestors.iter().all(|a| !a.starts_with('$')));

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
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p m2-ls load_parses_new_format_corpus`
Expected: FAIL — the old `RawRecord` cannot parse the array/new fields (panic from `include_str!` of a now-missing path once swapped, or assertion failures).

- [ ] **Step 4: Replace the deserialization structs**

In `src/builtin_index.rs`, replace `RawRecord` and `RawMethod` (around `:221-260`) and add `RawOperator`:

```rust
#[derive(Debug, Deserialize)]
struct RawRecord {
    kind: String,
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
}
```

- [ ] **Step 5: Rewrite `load()` for the new format**

Replace `BuiltinIndex::load` (`:106-172`) with:

```rust
pub fn load(corpus: &str) -> Self {
    let mut index = BuiltinIndex::default();
    // JSONC: strip the `//` header lines, then parse the single JSON array.
    let body: String = corpus
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let Ok(records) = serde_json::from_str::<Vec<RawRecord>>(&body) else {
        return index;
    };

    for raw in records {
        // name + aliases + extra_keys all resolve to this record.
        let mut keys = raw.aliases.clone();
        keys.extend(raw.extra_keys.iter().cloned());

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
                        codomain: method.typical_value.as_deref().map(deref_ref),
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
                    typical_value: raw.typical_value.as_deref().map(deref_ref),
                    options: raw.options,
                    signatures,
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
                });
            }
            // `package` and any future `meta` record carry no per-symbol facts.
            _ => {}
        }
    }
    index
}
```

- [ ] **Step 6: Replace `collect_forms` with `capitalize_form`**

`collect_forms` read the old per-method `form`; it is now dead. Remove it and add the single-token capitalizer it is replaced by (near `deref_ref`):

```rust
/// `binary` → `Binary`, etc. The corpus uses lowercase operator forms; the LSP
/// keeps the capitalized vocabulary its operator-hover code matches on.
fn capitalize_form(form: &str) -> String {
    let mut chars = form.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
```

- [ ] **Step 7: Swap the data path at every `include_str!` site**

```bash
grep -rl 'm2-types\.jsonl' src/ | xargs sed -i 's/m2-types\.jsonl/m2-types.jsonc/g'
rm src/data/m2-types.jsonl
grep -rn 'm2-types\.jsonl' src/ || echo "no old-path references remain"
```

- [ ] **Step 8: Run the loader test, then the full suite**

Run: `cargo test -p m2-ls load_parses_new_format_corpus`
Expected: PASS.
Run: `cargo test -p m2-ls`
Expected: all tests PASS (the 167 existing + the 2 new). Investigate any failure before committing — a real behavior regression, not a test to relax.

- [ ] **Step 9: Commit**

```bash
cargo fmt && cargo clippy -q
git add -A src/
git commit -m "builtin_index: parse fundocs new-format corpus (\$pkg\$ keys, new kinds, operator object)"
```

---

### Task 3: Regression guard — hover + inference unchanged on the new corpus

The migration must not change observable behavior for Core. Markdown still loads from `m2-docs.jsonl` (bare-name keyed; the new records' primary `name` is bare, so keys still line up). This task locks that in with a behavior test, separate from the loader unit test so a reviewer can reject a silent hover/inference regression on its own.

**Files:**
- Test: `src/typesystem.rs` (`tests` module — alongside the existing `builtins`-loading tests near `:1730`)

**Interfaces:**
- Consumes: `BuiltinData::load_from_index(include_str!("./data/m2-types.jsonc"), include_str!("./data/m2-docs.jsonl"))` (the two-arg loader is unchanged; only the first file's format changed).

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/typesystem.rs`:

```rust
#[test]
fn new_corpus_preserves_hover_and_subtype_facts() {
    let builtins = BuiltinData::load_from_index(
        include_str!("./data/m2-types.jsonc"),
        include_str!("./data/m2-docs.jsonl"),
    );

    // hover markdown still resolves by bare name
    assert!(builtins.doc_markdown(&InstanceID::new("ideal")).is_some());

    // a known subtype edge survives the deref (ZZ is-a Ring's ancestor chain)
    assert!(builtins.is_subtype(&InstanceID::new("ZZ"), &InstanceID::new("Thing")));

    // a known method codomain resolves (ideal of a … → Ideal is documented)
    assert!(builtins.contains_name("ideal"));
}
```

- [ ] **Step 2: Run test to verify it fails (then passes)**

Run: `cargo test -p m2-ls new_corpus_preserves_hover_and_subtype_facts`
Expected: with Task 2 merged, this PASSES immediately. If it FAILS, the migration dropped a fact — fix `load()`/builders, do not weaken the assertion. (If asserting before Task 2 is merged: FAIL to compile on the `.jsonc` path.)

- [ ] **Step 3: Commit**

```bash
cargo fmt && cargo clippy -q
git add src/typesystem.rs
git commit -m "test: guard hover + subtype facts across the format migration"
```

---

## Self-Review

- **Spec coverage (P1 scope):** new JSONC parse ✅ (Task 2 Step 5); `$Package$Name` deref ✅ (Task 1, applied in Task 2); new kinds `methodFunction`/`symbol`/`table` ✅ (Task 2 Step 5 match arms); operator `forms` from the top-level object, re-capitalized ✅ (Tasks 2 Steps 4/6); markdown still loaded (from `m2-docs.jsonl`) ✅ (Task 3); home `package` retained on every entry ✅ (Task 2 — `deref_ref` on `package`, feeds P2 partitioning). `extra_keys` folded into lookup keys ✅ (Task 2 Step 5).
- **Type consistency:** `deref_ref`/`capitalize_form` signatures match their call sites; `TypeEntry`/`CallableEntry`/`ObjectEntry`/`Signature` shapes unchanged so `typesystem.rs` builders need no edits.
- **Placeholders:** none — every code/command step is concrete.

## Subsequent phases (separate plans, authored after P1 lands)

These need P1's concrete output (the new `BuiltinIndex` and the retained `package`) before their task code can be written without speculation:

- **P2 — Partition + `LoadedPackages`.** Split the loaded records into
  `PackagePartitionedIndex` (`HashMap<Package, BuiltinData>`) keyed by each
  record's home `package`; read the folded `default_loaded` baseline (once the
  combined `m2-index.jsonl` exists; until then, baseline = the 20-name set the
  meta record will carry); add the `LoadedPackages` semantic type
  (`default_loaded ∪ collect_imported_packages`, reusing the snapshot's parse
  tree); add `importFrom` to `package_source_string`'s trigger set.
- **P3 — `ScopedIndex` + rewire queries.** Add the delegating scoped view over
  the loaded partitions (baseline/Core-first resolution); route inference, hover,
  navigation, completion, and type-hierarchy through it; retire the parallel
  `PackageIndexer`-vs-`builtins` split. Synthetic 2-package fixture proves
  loaded ⇒ resolves, not-loaded ⇒ silent.
- **Single-file consolidation (with fundocs P4).** When fundocs emits
  `m2-index.jsonl` (one file, leading `meta` record, folded `markdown`), switch
  the loader to JSONL line parsing + the meta record, fold markdown from each
  record, and drop the separate `m2-docs.jsonl` `include_str!`.
