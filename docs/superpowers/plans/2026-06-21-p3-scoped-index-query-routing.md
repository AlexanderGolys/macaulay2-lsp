# P3: ScopedIndex Query Routing + Legacy Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route the on-demand query capabilities (hover, completion, goto-definition, type-hierarchy, workspace-symbols) through a non-materializing `ScopedIndex` view over the resident `PackagePartitionedIndex`, and delete the legacy disk-cache duality (`PackageIndexer`, the `package_indexes` runtime cache, `active_package_indexes`, `load_from_split*`).

**Architecture:** A `ScopedIndex<'a>` holds an ordered `Vec<(&str, &BuiltinData)>` of the currently-loaded packages' partitions (baseline/Core-first, then import order). Lookup methods (`get_record`, `resolve_call_*`, `is_subtype`) resolve first-match across that ordered list; search methods (`names_with_prefix`, `matching_names`) aggregate across all loaded partitions with dedup. No partition data is ever merged or rebuilt — importing/removing a package only changes which `&BuiltinData` references the view holds. `LoadedPackages` is memoized on the document snapshot (computed once per version from the already-parsed tree).

**Tech Stack:** Rust, `tower-lsp`, `tree-sitter-macaulay2`, package name `m2-ls` (test with `cargo test -p m2-ls`).

## Global Constraints

- **Non-materializing view (HARD, user-stated):** `ScopedIndex` MUST be an ordered list of `&BuiltinData` references walked per query — NEVER a merged/rebuilt `BuiltinData` or lattice per scope. All shipped partitions are resident from startup; importing/removing a package only toggles which references the view holds, never loads/unloads/rebuilds data.
- **Memoize per document version:** compute `LoadedPackages` once per snapshot version (reuse the snapshot's existing parse tree); per-edit recompute is O(loaded-package-count selection), never O(corpus).
- **Resolution order (from design spec):** baseline/Core-first, then import order, first-match wins. This UNIFIES today's inconsistent order (hover/nav currently do packages-first) to the spec's Core-first — a deliberate behavior change.
- **Monotone, known-facts-only:** absent = unknown; an import with no partition in the corpus resolves to nothing (no error). Never store `Thing`/`Any` as a positive codomain.
- **Fail fast:** corrupt corpus / missing Core partition panics (already enforced in `from_corpus`). Do not mask with defaults.
- **Semantic types, no primitive obsession:** keep `LoadedPackages` / `InstanceID` / package-name newtypes; do not pass bare `Vec<String>` where a named type exists.
- **typesystem.rs is live WIP:** additive/surgical edits only; do not restructure it.
- **Tests verify, no TDD-first retrofit:** delete or rewrite legacy-format tests rather than keep legacy loaders alive to serve them.
- **Build gate:** `cargo fmt`, `cargo test -p m2-ls` green, and `cargo clippy` with no new warnings beyond the pre-existing baseline (9).
- **Inference/diagnostics stay Core-only in this phase:** `analysis.rs` never consumed the package path; threading scope through parse-time analysis is out of scope here (deferred follow-up).

---

## File Structure

- `src/partitioned_index.rs` — **add** `ScopedIndex<'a>` + `PackagePartitionedIndex::scoped()`. Home of the scoped view (lives with the partitions it borrows).
- `src/document.rs` — **add** a memoized `LoadedPackages` to `DocumentSnapshot`, computed from its parse tree.
- `src/package_index.rs` — **shrink**: keep `SourceResolver`, `collect_imported_packages`, `package_source_string`; **delete** `PackageIndexer` + cache-dir helpers. Add a tree-reusing entrypoint for import collection.
- `src/capabilities/hover.rs`, `src/capabilities/navigation.rs`, `src/capabilities/type_hierarchy.rs` — **rewire** signatures from `(&BuiltinData, &[(String, BuiltinData)])` to `&ScopedIndex`.
- `src/main.rs` — **delete** `package_indexer`/`package_indexes` fields, `package_index()`, `active_package_indexes()`, the legacy `type_hierarchy_index` branch, `did_close` cache clear; build `ScopedIndex` per request.
- `src/typesystem.rs` — **delete** `load_from_split` + `load_from_split_with_type_facts` after test migration; add `BuiltinData::empty()` for the trivial test fixtures.

---

### Task 1: `ScopedIndex` view + `scoped()` constructor

**Files:**
- Modify: `src/partitioned_index.rs`
- Test: `src/partitioned_index.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `PackagePartitionedIndex { partitions: HashMap<String, BuiltinData>, default_loaded: Vec<String> }`, `LoadedPackages::as_slice(&self) -> &[String]`, `BuiltinData::{get_record, contains_name, is_subtype, resolve_call_return_type, resolve_call_return_type_with_options, resolve_call_signature_usage, names_with_prefix, matching_names}`.
- Produces:
  - `PackagePartitionedIndex::scoped<'a>(&'a self, loaded: &'a LoadedPackages) -> ScopedIndex<'a>`
  - `ScopedIndex<'a>` with:
    - `get_record_with_package(&self, name: &InstanceID) -> Option<(&'a str, Record)>`
    - `get_record(&self, name: &InstanceID) -> Option<Record>`
    - `contains_name(&self, name: &str) -> bool`
    - `is_subtype(&self, child: &InstanceID, parent: &InstanceID) -> bool`
    - `resolve_call_return_type(&self, callable: &str, argument_types: &[Option<String>]) -> Option<String>`
    - `resolve_call_return_type_with_options(&self, callable: &str, argument_types: &[Option<String>], literal_options: &[(String, String)]) -> Option<String>`
    - `resolve_call_signature_usage(&self, callable: &str, argument_types: &[Option<String>]) -> Option<SignatureUsage>`
    - `names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(&'a str, &'a str)>` (package, name; dedup by name, baseline-first)
    - `matching_names(&self, query: &str, limit: usize) -> Vec<(&'a str, &'a str)>`
    - `core(&self) -> &'a BuiltinData` (first partition — Core; for the few helpers that still need the raw Core data, e.g. `record_hover_with_package_and_usage`'s `builtins` arg)

- [ ] **Step 1: Write failing tests**

In `src/partitioned_index.rs` tests module, add:

```rust
#[test]
fn scoped_resolves_only_loaded_partitions() {
    let index = PackagePartitionedIndex::from_corpus(corpus());

    // Baseline only: Core resolves, JSON does not.
    let baseline = LoadedPackages::resolve(index.default_loaded(), "1 + 1");
    let scoped = index.scoped(&baseline);
    assert!(scoped.get_record(&InstanceID::new("ZZ")).is_some());
    assert!(scoped.get_record(&InstanceID::new("toJSON")).is_none());

    // Import JSON: now toJSON resolves, tagged with its package.
    let loaded = LoadedPackages::resolve(index.default_loaded(), "needsPackage \"JSON\"");
    let scoped = index.scoped(&loaded);
    let (pkg, record) = scoped
        .get_record_with_package(&InstanceID::new("toJSON"))
        .expect("toJSON resolves once JSON is loaded");
    assert_eq!(pkg, "JSON");
    assert_eq!(record.name.0, "toJSON");
}

#[test]
fn scoped_is_subtype_spans_package_to_core() {
    // A loaded package type whose flattened ancestor chain reaches a Core type
    // resolves is_subtype without any merged lattice (chain is stored per record).
    let index = PackagePartitionedIndex::from_corpus(corpus());
    let loaded = LoadedPackages::resolve(index.default_loaded(), "needsPackage \"JSON\"");
    let scoped = index.scoped(&loaded);
    // Every type is a subtype of Thing; reflexive check is the floor.
    assert!(scoped.is_subtype(&InstanceID::new("ZZ"), &InstanceID::new("Thing")));
}

#[test]
fn scoped_skips_imports_absent_from_corpus() {
    // Importing a package not in the corpus simply contributes nothing.
    let index = PackagePartitionedIndex::from_corpus(corpus());
    let loaded = LoadedPackages::resolve(index.default_loaded(), "needsPackage \"NoSuchPkg\"");
    let scoped = index.scoped(&loaded);
    assert!(scoped.get_record(&InstanceID::new("ZZ")).is_some());
}
```

Add `use crate::typesystem::InstanceID;` to the test module if not present.

- [ ] **Step 2: Run tests, verify they fail to compile**

Run: `cargo test -p m2-ls scoped_ 2>&1 | head -30`
Expected: compile error — no method `scoped` / no type `ScopedIndex`.

- [ ] **Step 3: Implement `ScopedIndex` + `scoped()`**

In `src/partitioned_index.rs`, change the imports line to also bring in the query types:

```rust
use crate::typesystem::{BuiltinData, InstanceID, Record, SignatureUsage};
```

Add `scoped()` to the `impl PackagePartitionedIndex` block (after `packages()`):

```rust
    /// A non-materializing view over the partitions loaded for a document, in
    /// resolution order (baseline/Core-first, then import order). Imports with no
    /// partition in the corpus are skipped. Borrows the partitions — nothing is
    /// copied or rebuilt; importing/removing a package only changes which
    /// references the view holds.
    pub fn scoped<'a>(&'a self, loaded: &'a LoadedPackages) -> ScopedIndex<'a> {
        let partitions = loaded
            .as_slice()
            .iter()
            .filter_map(|package| {
                self.partition(package)
                    .map(|data| (package.as_str(), data))
            })
            .collect();
        ScopedIndex { partitions }
    }
```

Add the view type at the end of the file (before `#[cfg(test)]`):

```rust
/// An ordered, borrowing view over the loaded packages' `BuiltinData`. Lookups
/// resolve first-match across the ordered partitions; searches aggregate across
/// all of them. Holds only references — never a merged or rebuilt index.
#[derive(Debug, Clone)]
pub(crate) struct ScopedIndex<'a> {
    partitions: Vec<(&'a str, &'a BuiltinData)>,
}

impl<'a> ScopedIndex<'a> {
    /// The Core partition — always the first loaded (baseline floor). Used by the
    /// few hover helpers that still take the raw Core `BuiltinData`.
    pub fn core(&self) -> &'a BuiltinData {
        self.partitions
            .first()
            .map(|(_, data)| *data)
            .expect("loaded set is never empty: Core is always the baseline")
    }

    pub fn get_record_with_package(&self, name: &InstanceID) -> Option<(&'a str, Record)> {
        self.partitions
            .iter()
            .find_map(|(package, data)| data.get_record(name).map(|record| (*package, record)))
    }

    pub fn get_record(&self, name: &InstanceID) -> Option<Record> {
        self.get_record_with_package(name).map(|(_, record)| record)
    }

    pub fn contains_name(&self, name: &str) -> bool {
        self.partitions.iter().any(|(_, data)| data.contains_name(name))
    }

    pub fn is_subtype(&self, child: &InstanceID, parent: &InstanceID) -> bool {
        // The partition owning `child` carries its full flattened ancestor chain,
        // so a single partition answers definitively; others return false (or the
        // reflexive child == parent). `any` therefore yields the correct edge.
        self.partitions
            .iter()
            .any(|(_, data)| data.is_subtype(child, parent))
    }

    pub fn resolve_call_return_type(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<String> {
        self.partitions
            .iter()
            .find_map(|(_, data)| data.resolve_call_return_type(callable, argument_types))
    }

    pub fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
        literal_options: &[(String, String)],
    ) -> Option<String> {
        self.partitions.iter().find_map(|(_, data)| {
            data.resolve_call_return_type_with_options(callable, argument_types, literal_options)
        })
    }

    pub fn resolve_call_signature_usage(
        &self,
        callable: &str,
        argument_types: &[Option<String>],
    ) -> Option<SignatureUsage> {
        self.partitions
            .iter()
            .find_map(|(_, data)| data.resolve_call_signature_usage(callable, argument_types))
    }

    /// Names across all loaded partitions starting with `prefix`, deduped by name
    /// (first occurrence wins, baseline-first), capped at `limit`. Each entry is
    /// `(package, name)` so callers can label provenance.
    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(&'a str, &'a str)> {
        self.aggregate_names(limit, |data, remaining| data.names_with_prefix(prefix, remaining))
    }

    pub fn matching_names(&self, query: &str, limit: usize) -> Vec<(&'a str, &'a str)> {
        self.aggregate_names(limit, |data, remaining| data.matching_names(query, remaining))
    }

    fn aggregate_names(
        &self,
        limit: usize,
        per_partition: impl Fn(&'a BuiltinData, usize) -> Vec<&'a str>,
    ) -> Vec<(&'a str, &'a str)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (package, data) in &self.partitions {
            if out.len() >= limit {
                break;
            }
            for name in per_partition(data, limit.saturating_sub(out.len())) {
                if seen.insert(name) {
                    out.push((*package, name));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        out
    }
}
```

Note: confirm `SignatureUsage` and `Record` are `pub` in `typesystem.rs` (they are — used across capability modules).

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p m2-ls scoped_ 2>&1 | tail -20`
Expected: the 3 new tests pass.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt && cargo clippy -p m2-ls 2>&1 | grep -c warning
git add src/partitioned_index.rs
git commit -m "Add non-materializing ScopedIndex view over loaded partitions"
```

---

### Task 2: Memoize `LoadedPackages` on the document snapshot

**Files:**
- Modify: `src/document.rs`
- Modify: `src/package_index.rs` (add tree-reusing import collector)
- Test: `src/document.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `DocumentSnapshot` (holds its parse tree + text + version), `collect_imported_packages(text)`, `PackagePartitionedIndex::default_loaded() -> &[String]`, `LoadedPackages::resolve(default_loaded, text)`.
- Produces:
  - `package_index::collect_imported_packages_in_tree(text: &str, tree: &tree_sitter::Tree) -> Vec<String>` (no fresh parser).
  - `DocumentSnapshot::loaded_packages(&self) -> &LoadedPackages` — memoized; computed at `from_text` / `apply_changes` time from the snapshot's tree and the supplied `default_loaded`.

**Decision to confirm during implementation:** `from_text` / `apply_changes` currently take `&BuiltinData`. They now also need `default_loaded: &[String]` to compute the baseline. Thread it through from `Backend` (which holds `partitioned.default_loaded()`).

- [ ] **Step 1: Add the tree-reusing collector**

In `src/package_index.rs`, refactor `collect_imported_packages` to delegate to a tree-taking variant so the snapshot can reuse its parse tree (the existing fresh-parser path stays for callers without a tree):

```rust
pub(crate) fn collect_imported_packages(text: &str) -> Vec<String> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_macaulay2::language())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    collect_imported_packages_in_tree(text, &tree)
}

pub(crate) fn collect_imported_packages_in_tree(
    text: &str,
    tree: &tree_sitter::Tree,
) -> Vec<String> {
    let root = tree.root_node();
    let mut packages = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = root.walk();
    let mut reached_root = false;
    while !reached_root {
        let node = cursor.node();
        if node.kind() == "string_literal" {
            if let Some(package_name) = package_source_string(text, node) {
                if seen.insert(package_name.to_string()) {
                    packages.push(package_name.to_string());
                }
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                reached_root = true;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
    packages
}
```

- [ ] **Step 2: Write failing test for memoized loaded set**

In `src/document.rs` tests, add (adapt the snapshot constructor call to the real `from_text` signature, including the new `default_loaded` arg):

```rust
#[test]
fn snapshot_caches_loaded_packages_from_its_tree() {
    let default_loaded = vec!["Core".to_string()];
    let builtins = crate::typesystem::BuiltinData::empty();
    let snapshot = DocumentSnapshot::from_text(
        "needsPackage \"JSON\"\n1 + 1".to_string(),
        &builtins,
        &default_loaded,
    )
    .expect("snapshot builds");
    let loaded = snapshot.loaded_packages();
    assert_eq!(loaded.as_slice(), &["Core".to_string(), "JSON".to_string()]);
}
```

(`BuiltinData::empty()` is added in Task 6/7; if running tasks in order, temporarily use `BuiltinData::load_from_index("")` here and switch to `empty()` when it lands.)

- [ ] **Step 3: Run test, verify it fails**

Run: `cargo test -p m2-ls snapshot_caches_loaded 2>&1 | head -20`
Expected: compile error (no `loaded_packages`; `from_text` arity mismatch).

- [ ] **Step 4: Implement memoized field**

In `src/document.rs`: add `use crate::partitioned_index::LoadedPackages;` and `use crate::package_index::collect_imported_packages_in_tree;`. Add a `loaded_packages: LoadedPackages` field to `DocumentSnapshot`. In `from_text(text, builtins, default_loaded)` compute it right after the tree is built:

```rust
let loaded_packages = LoadedPackages::resolve_in_tree(default_loaded, &text, &tree);
```

Add to `partitioned_index.rs` a tree-reusing resolver beside `resolve`:

```rust
    pub fn resolve_in_tree(
        default_loaded: &[String],
        text: &str,
        tree: &tree_sitter::Tree,
    ) -> Self {
        let mut ordered = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for package in default_loaded
            .iter()
            .cloned()
            .chain(crate::package_index::collect_imported_packages_in_tree(text, tree))
        {
            if seen.insert(package.clone()) {
                ordered.push(package);
            }
        }
        LoadedPackages(ordered)
    }
```

Add the accessor:

```rust
    pub(crate) fn loaded_packages(&self) -> &LoadedPackages {
        &self.loaded_packages
    }
```

In `apply_changes`, after re-parsing, recompute `self.loaded_packages` the same way (it depends on `default_loaded`, so `apply_changes` also takes `default_loaded: &[String]`).

- [ ] **Step 5: Update all `from_text` / `apply_changes` call sites**

`Backend` calls (main.rs lines ~230, 245, 589, 663) and every test that builds a snapshot must pass `default_loaded`. In `Backend`, use `self.partitioned.default_loaded()`. For the cross-file snapshot reads in `references`/`rename` (lines 589, 663), pass `self.partitioned.default_loaded()` too. For tests that build throwaway snapshots, pass `&[]` (empty baseline) or `&["Core".to_string()]` as appropriate.

- [ ] **Step 6: Run tests, fmt, clippy, commit**

Run: `cargo test -p m2-ls 2>&1 | tail -15`
Expected: all green (existing + new).

```bash
cargo fmt && cargo clippy -p m2-ls 2>&1 | grep -c warning
git add src/document.rs src/package_index.rs src/partitioned_index.rs src/main.rs
git commit -m "Memoize LoadedPackages per snapshot version from its parse tree"
```

---

### Task 3: Rewire `hover_response` through `ScopedIndex`

**Files:**
- Modify: `src/capabilities/hover.rs`
- Modify: `src/main.rs` (`hover` handler)
- Test: existing hover tests + `src/capabilities/hover.rs` tests

**Interfaces:**
- Consumes: `ScopedIndex` (Task 1), `DocumentSnapshot::loaded_packages` (Task 2), `record_hover_with_package`, `record_hover_with_package_and_usage`.
- Produces: `hover_response(document: &DocumentSnapshot, position: Position, scoped: &ScopedIndex) -> Option<Hover>`.

- [ ] **Step 1: Change `hover_response` signature + body**

Replace the two args `builtins: &BuiltinData, active_package_indexes: &[(String, BuiltinData)]` with `scoped: &ScopedIndex`. Replace the package loop + builtins fallback (lines 45–66) with unified scoped resolution:

```rust
    let Some((package, record)) = scoped.get_record_with_package(&InstanceID(node_text.to_string()))
    else {
        return None;
    };
    let signature_usage =
        call_signature_usage_for_hover(node, node_text, text, analysis, scoped);
    Some(record_hover_with_package_and_usage(
        &record,
        Some(package),
        scoped.core(),
        signature_usage.as_ref(),
    ))
```

`call_signature_usage_for_hover` (line ~114) takes `builtins: &BuiltinData`; change it to `scoped: &ScopedIndex` and call `scoped.resolve_call_signature_usage(...)` instead of `builtins.resolve_call_signature_usage(...)`. Update the `use` line: drop `BuiltinData` if now unused, add `use crate::partitioned_index::ScopedIndex;`.

Note: `record_hover_with_package_and_usage`'s third arg is the raw Core `BuiltinData` (used for option-value reverse-usage rendering) — pass `scoped.core()`.

- [ ] **Step 2: Update the `hover` handler in main.rs**

```rust
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let document = match self.documents.get(uri) {
            Some(document) => document,
            None => return Ok(None),
        };
        let scoped = self.partitioned.scoped(document.loaded_packages());
        Ok(hover_response(document.value(), position, &scoped))
    }
```

- [ ] **Step 3: Run hover tests, fmt, clippy, commit**

Run: `cargo test -p m2-ls hover 2>&1 | tail -15`
Expected: green (some test call sites updated in Task 7).

```bash
cargo fmt && cargo clippy -p m2-ls 2>&1 | grep -c warning
git add src/capabilities/hover.rs src/main.rs
git commit -m "Route hover through ScopedIndex (Core-first unified resolution)"
```

---

### Task 4: Rewire completion / workspace-symbols / goto-definition

**Files:**
- Modify: `src/capabilities/navigation.rs`
- Modify: `src/main.rs` (`completion`, `symbol`, `goto_definition` handlers)

**Interfaces:**
- Consumes: `ScopedIndex` (Task 1), its `names_with_prefix`/`matching_names` returning `(package, name)`, `get_record_with_package`.
- Produces:
  - `completion_response(text: &str, position: Position, scoped: &ScopedIndex) -> Option<CompletionResponse>`
  - `workspace_symbols_response(query: &str, scoped: &ScopedIndex, record_location: impl Fn(&Record) -> Option<Location>) -> Vec<SymbolInformation>`
  - `goto_definition_response(document, uri, position, scoped: &ScopedIndex, source_resolver, workspace_index, record_location) -> Option<GotoDefinitionResponse>`

- [ ] **Step 1: `completion_response`**

```rust
pub(crate) fn completion_response(
    text: &str,
    position: Position,
    scoped: &ScopedIndex,
) -> Option<CompletionResponse> {
    let prefix = symbol_prefix_at(text, position)?;
    let items = scoped
        .names_with_prefix(&prefix, 80)
        .into_iter()
        .map(|(package, name)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            // Label provenance only for non-baseline packages, matching prior UX.
            detail: (package != "Core").then(|| format!("Package: {package}")),
            ..Default::default()
        })
        .collect();
    Some(CompletionResponse::Array(items))
}
```

- [ ] **Step 2: `workspace_symbols_response`**

```rust
#[allow(deprecated)]
pub(crate) fn workspace_symbols_response(
    query: &str,
    scoped: &ScopedIndex,
    record_location: impl Fn(&Record) -> Option<Location>,
) -> Vec<SymbolInformation> {
    let mut symbols = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (package, name) in scoped.matching_names(query, 120) {
        if package == "Core" && !should_include_workspace_symbol("Core", name) {
            continue;
        }
        let Some(record) = scoped.get_record(&InstanceID(name.to_string())) else {
            continue;
        };
        let Some(location) = record_location(&record) else {
            continue;
        };
        if seen.insert(workspace_symbol_dedupe_key(package, name)) {
            symbols.push(SymbolInformation {
                name: name.to_string(),
                kind: record_symbol_kind(&record),
                tags: None,
                deprecated: None,
                location,
                container_name: Some(package.to_string()),
            });
        }
    }
    symbols
}
```

- [ ] **Step 3: `goto_definition_response`**

Replace `builtins` + `active_package_indexes` params with `scoped: &ScopedIndex`. Replace the package loop + builtins fallback (lines 171–183) with:

```rust
    if let Some(record) = scoped.get_record(&InstanceID(node_text.to_string())) {
        if let Some(location) = record_location(&record) {
            return Some(GotoDefinitionResponse::Scalar(location));
        }
    }
    None
```

Update the `use` line: add `use crate::partitioned_index::ScopedIndex;`; keep `BuiltinData` only if still referenced (the `("","")` test at line 396 is migrated in Task 7).

- [ ] **Step 4: Update main.rs handlers**

`completion`:
```rust
        let scoped = self.partitioned.scoped(document.loaded_packages());
        Ok(completion_response(document.text(), position, &scoped))
```

`symbol` (workspace symbols) — this previously searched the runtime `package_indexes` cache. Now it scopes to the baseline only (no open document gives a text to import from). Build a baseline-only loaded set:
```rust
        let loaded = LoadedPackages::resolve(self.partitioned.default_loaded(), "");
        let scoped = self.partitioned.scoped(&loaded);
        Ok(Some(workspace_symbols_response(
            query,
            &scoped,
            |record| self.record_location(record),
        )))
```

`goto_definition`:
```rust
        let scoped = self.partitioned.scoped(document.loaded_packages());
        Ok(goto_definition_response(
            document.value(),
            uri,
            position,
            &scoped,
            &self.source_resolver,
            &self.workspace_index,
            |record| self.record_location(record),
        ))
```

- [ ] **Step 5: Run tests, fmt, clippy, commit**

Run: `cargo test -p m2-ls completion 2>&1 | tail -10 && cargo test -p m2-ls navigation 2>&1 | tail -10`
Expected: green (test call sites updated in Task 7).

```bash
cargo fmt && cargo clippy -p m2-ls 2>&1 | grep -c warning
git add src/capabilities/navigation.rs src/main.rs
git commit -m "Route completion/workspace-symbols/goto-def through ScopedIndex"
```

---

### Task 5: Rewire type-hierarchy through `ScopedIndex`

**Files:**
- Modify: `src/main.rs` (`prepare_type_hierarchy`, `supertypes`, `subtypes`, and the `type_hierarchy_*` Backend helpers)

**Interfaces:**
- Consumes: `ScopedIndex`, `document.loaded_packages()`.
- Produces: type-hierarchy helpers resolve records via the scoped view; the `TypeHierarchyItem.data.package` round-trips so supertypes/subtypes re-derive scope.

**Wrinkle:** `supertypes`/`subtypes` receive only a `TypeHierarchyItem` (no document), so they cannot read `document.loaded_packages()`. They get the package name from `item.data.package`. Resolve the related (parent/subtype) record by: try the scoped view of `{baseline ∪ item-package}`, else fall back to the item's own package partition, else Core. Concretely, replace `type_hierarchy_index` / `type_hierarchy_related_record` (which used `self.package_index(...)` + `self.builtins`) with partition lookups:

- [ ] **Step 1: Replace `type_hierarchy_index`**

```rust
    fn type_hierarchy_index(&self, package: Option<&str>) -> Option<&BuiltinData> {
        self.partitioned.partition(package.unwrap_or("Core"))
    }
```

(Returns a borrow now; update `type_hierarchy_record` to clone the record out, not the index.)

- [ ] **Step 2: `type_hierarchy_record` + `type_hierarchy_related_record`**

```rust
    fn type_hierarchy_record(
        &self,
        package: Option<&str>,
        name: &str,
    ) -> Option<(String, Record)> {
        let index = self.type_hierarchy_index(package)?;
        let record = index.get_record(&InstanceID::new(name))?;
        record.type_info.as_ref()?;
        Some((package.unwrap_or("Core").to_string(), record))
    }

    fn type_hierarchy_related_record(
        &self,
        package: &str,
        name: &InstanceID,
    ) -> Option<(String, Record)> {
        if let Some(index) = self.partitioned.partition(package) {
            if let Some(record) = index.get_record(name) {
                return Some((package.to_string(), record));
            }
        }
        self.partitioned
            .partition("Core")
            .and_then(|core| core.get_record(name))
            .map(|record| ("Core".to_string(), record))
    }
```

Update `supertypes`/`subtypes` to use the new 2-tuple returns (drop the now-unused `index` binding; call `type_hierarchy_related_record(&package, parent_name)`).

- [ ] **Step 3: `prepare_type_hierarchy` via scoped view**

```rust
        let scoped = self.partitioned.scoped(document.loaded_packages());
        if let Some((package, record)) =
            scoped.get_record_with_package(&InstanceID::new(name))
        {
            if record.type_info.is_some() {
                return Ok(Some(vec![self.type_hierarchy_item(
                    package,
                    &record,
                    Some(uri.clone()),
                    Some(range),
                )]));
            }
        }
        Ok(None)
```

(Drop the document guard borrow before building `scoped` only if the borrow checker complains; `document.loaded_packages()` returns a borrow tied to the guard, which is fine within this scope.)

- [ ] **Step 4: Run tests, fmt, clippy, commit**

Run: `cargo test -p m2-ls type_hierarchy 2>&1 | tail -15`
Expected: green.

```bash
cargo fmt && cargo clippy -p m2-ls 2>&1 | grep -c warning
git add src/main.rs
git commit -m "Route type-hierarchy through partitioned index / ScopedIndex"
```

---

### Task 6: Delete the legacy disk-cache duality

**Files:**
- Modify: `src/main.rs` (remove fields + methods + handler bits)
- Modify: `src/package_index.rs` (remove `PackageIndexer` + cache-dir helpers)

**Interfaces:**
- Removes: `Backend.package_indexer`, `Backend.package_indexes`, `Backend::package_index`, `Backend::active_package_indexes`, the `did_close` `package_indexes.clear()`, the legacy `type_hierarchy_index` Core/`package_index` branch (already replaced in Task 5), `PackageIndexer`, `default_package_index_dir`, `names_path`, `details_path`, the `M2_LSP_PACKAGE_INDEX_DIR` env var.

- [ ] **Step 1: Strip `PackageIndexer` from `package_index.rs`**

Delete the `PackageIndexer` struct + its `impl` (lines ~64–94) and `default_package_index_dir` (lines ~96–103). Remove the now-unused `use std::fs;` if nothing else uses it (the `BuiltinData` import is still needed only if other code references it — check; if `package_index.rs` no longer references `BuiltinData`, drop that import too). Keep `SourceResolver`, `package_source_string`, `collect_imported_packages*`.

- [ ] **Step 2: Strip the Backend duality**

In `src/main.rs`:
- Remove the `package_indexer: PackageIndexer` and `package_indexes: DashMap<String, BuiltinData>` fields.
- Remove `Backend::package_index` and `Backend::active_package_indexes`.
- Remove `package_indexer`/`package_indexes` from `Backend::new`.
- In `did_close`, remove the `if self.documents.is_empty() { self.package_indexes.clear(); }` block.
- In `on_open`/`on_change`, remove the `let _ = self.active_package_indexes(document.text());` warm-up lines (the snapshot now memoizes its loaded set).
- Update the import: `use package_index::{collect_imported_packages, SourceResolver};` (drop `PackageIndexer`; drop `collect_imported_packages` too if no longer referenced in main.rs after Task 2 moved import-collection into the snapshot — verify and trim).
- Remove `use dashmap::DashMap;` only if `documents` no longer uses it (it does — keep).

- [ ] **Step 3: Delete the `package_indexer_loads_cached_line_aligned_package_records` test**

Remove that test (main.rs ~992–1018) — it exercised the deleted disk-cache loader. Per "tests verify, no retrofit," delete rather than adapt.

- [ ] **Step 4: Build, fmt, clippy, commit**

Run: `cargo build -p m2-ls 2>&1 | tail -20`
Expected: compiles (any remaining `load_from_split` test references are handled in Task 7; if build fails only on those, proceed to Task 7 then return).

```bash
cargo fmt && cargo clippy -p m2-ls 2>&1 | grep -c warning
git add src/main.rs src/package_index.rs
git commit -m "Remove legacy disk-cache duality (PackageIndexer, package_indexes, active_package_indexes)"
```

---

### Task 7: Migrate tests off `load_from_split*`, then delete the legacy loaders

**Files:**
- Modify: `src/typesystem.rs` (add `BuiltinData::empty()`; delete `load_from_split` + `load_from_split_with_type_facts`)
- Modify: every test using `load_from_split*` — `src/workspace_index.rs:196`, `src/capabilities/document_highlight.rs:169`, `src/capabilities/navigation.rs:396`, `src/capabilities/code_actions.rs:694`, `src/capabilities/hover.rs:383`, `src/capabilities/semantic_tokens.rs` (6 sites), `src/analysis.rs` (~6 sites), `src/main.rs` (record-hover tests, ~6 sites)

**Interfaces:**
- Produces: `BuiltinData::empty() -> Self` (no records; empty lattice/facts) — the replacement for `load_from_split("", "")`.

- [ ] **Step 1: Add `BuiltinData::empty()`**

In `src/typesystem.rs`, in `impl BuiltinData`:

```rust
    /// An empty index — no records, no facts. For tests that only need a
    /// `BuiltinData` placeholder (snapshots, semantic tokens with no builtins).
    pub fn empty() -> Self {
        Self::from_index(&crate::builtin_index::BuiltinIndex::default())
    }
```

(`BuiltinIndex` derives `Default`; confirm. `from_index` over an empty index yields empty maps/vecs and a default lattice.)

- [ ] **Step 2: Replace all `load_from_split("", "")` with `BuiltinData::empty()`**

These ~13 sites are pure placeholders. Mechanical replace. Run after:

Run: `grep -rn 'load_from_split("", "")' src/`
Expected: no matches.

- [ ] **Step 3: Migrate the rich-fixture tests**

The remaining `load_from_split(names, details)` / `load_from_split_with_type_facts(...)` calls (hover.rs:383, analysis.rs inference tests, main.rs `record_hover_*` tests, the `record_hover_includes_option_value_reverse_usage` in main.rs) build a `BuiltinData` from an inline OLD-format `Record` JSONL. Convert each to the **new** single-line corpus format and build via `BuiltinData::load_from_index(corpus)`, where `corpus` is a meta line + one object line per record. Example conversion for the `kernel` hover test:

```rust
let corpus = concat!(
    r#"{"kind":"meta","default_loaded":["Core"]}"#, "\n",
    r#"{"kind":"methodFunction","name":"kernel","package":"$Core$Core","markdown":"kernel of a map","signatures":[{"domain":["$Core$RingMap"],"codomain":"$Core$Ideal"}]}"#,
);
let builtins = BuiltinData::load_from_index(corpus);
```

For each migrated test, verify the assertions still describe the new record shape; where an assertion tested a field that only existed in the old `function_info`/`operator_info` JSON (e.g. `documented_methods` examples), reconstruct the equivalent new-format field per `~/m2/fundocs/docs/typecheck-schema.md`. If a test is purely about the *old loader's* parsing behavior (not reusable logic), delete it.

**This step is the heaviest — treat each fixture as its own micro-task.** Run the single test after each conversion:

Run: `cargo test -p m2-ls <test_name> -- --exact 2>&1 | tail -8`

- [ ] **Step 4: Delete `load_from_split` + `load_from_split_with_type_facts`**

Once `grep -rn load_from_split src/` returns nothing, delete both methods from `typesystem.rs` (lines ~1351–1397) and the now-unused `TypeFacts::load_jsonl` if it has no other caller (check first).

Run: `grep -rn 'load_from_split' src/`
Expected: no matches.

- [ ] **Step 5: Full suite, fmt, clippy, commit**

Run: `cargo test -p m2-ls 2>&1 | tail -15`
Expected: all green (count ≥ prior 187 minus the 1 deleted disk-cache test minus any deleted old-loader-only tests).

```bash
cargo fmt && cargo clippy -p m2-ls 2>&1 | grep -c warning
git add -A
git commit -m "Migrate tests to new corpus format; delete legacy load_from_split loaders"
```

---

### Task 8: Update docs + memory

**Files:**
- Modify: `docs/superpowers/specs/2026-06-21-loaded-package-scoping-design.md` (mark P3 done)
- Modify: memory `typecheck-index-schema.md` (record P3 complete, legacy removed)

- [ ] **Step 1: Mark P3 complete in the design spec** (Phasing section: P3 status → done, note inference deferred).
- [ ] **Step 2: Update memory** `typecheck-index-schema.md`: P3 done — ScopedIndex non-materializing view routing hover/completion/goto/type-hierarchy/workspace-symbols; legacy `PackageIndexer`/`package_indexes`/`active_package_indexes`/`load_from_split*` deleted; resolution unified Core-first; inference still Core-only (deferred). Note the commit.
- [ ] **Step 3: Commit**

```bash
git add docs/ && git commit -m "Mark P3 (ScopedIndex query routing) complete"
```

---

## Self-Review Notes

- **Spec coverage:** `ScopedIndex` (Task 1) = spec §Components.3; `LoadedPackages` memoization (Task 2) = spec §4 caching; rewire (Tasks 3–5) = spec "route inference/hover/navigation/completion/type-hierarchy"; legacy removal (Tasks 6–7) = spec "retire the parallel PackageIndexer-vs-builtins split." **Gap intentionally deferred:** spec lists "inference" among routed paths; this plan keeps `analysis.rs` Core-only (it never used the package path; scoping parse-time analysis needs an owned/Arc partition handle on the snapshot — separate phase). Flagged in Global Constraints.
- **Behavior change:** resolution order unifies to Core-first (was packages-first in hover/nav). Deliberate, per spec name-collision rule.
- **Non-materializing constraint:** honored — `ScopedIndex` holds `&BuiltinData` only; `is_subtype` works via per-record flattened ancestor chains, no merged lattice.
- **Type consistency:** `ScopedIndex` method names mirror `BuiltinData`'s; `get_record_with_package` returns `(&str, Record)` consumed identically in hover/type-hierarchy.
