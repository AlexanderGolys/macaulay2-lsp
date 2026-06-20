# Design: loaded-package scoping over a package-partitioned index

**Date:** 2026-06-21
**Status:** approved (pending spec review)
**Branch:** main

## Problem

The LSP typechecker reads its builtin knowledge from a single global
`BuiltinData` built from `src/data/m2-types.jsonl`. Two problems:

1. **Stale format.** fundocs now emits a new compressed corpus
   (`$Package$Name` reference keys, `methodFunction`/`symbol`/`table` kinds, a
   top-level `operator` object, per-record folded `markdown`). The loader and
   every `record_from_*` / `TypeFacts` / `TypeLattice` builder still parse the
   old shape.
2. **No package scoping.** The index is Core-only and the inference path
   (`analysis.rs`) consults only `self.builtins`. When the corpus becomes
   multi-package, nothing stops inference/hover from using a type or method from
   a package the current document has **not** loaded. M2 resolves names within
   the set of loaded packages; the LSP must mirror that — **only ever use
   indexed data from currently-loaded packages.**

## Decision

Partition the index **by home package** and scope every query to the set of
packages loaded in the current document. Chosen model: **per-package
partitioned indexes** (one `BuiltinData` per package) with a delegating scoped
view — scoping then falls out for free, because an unloaded package's facts are
never in the consulted set.

## The loaded-package baseline

The baseline in-scope set is **not** `{Core}`. There are two related numbers
(verified on M2 v1.26.05):

- `Core#"preloaded packages"` (explicit config) = **17**: `Complexes, Classic,
  ConwayPolynomials, Elimination, IntegralClosure, InverseSystems, Isomorphism,
  LLLBases, MinimalPrimes, OnlineLookup, PackageCitations, PrimaryDecomposition,
  ReesAlgebra, Saturation, SimpleDoc, TangentCone, Varieties`.
- `loadedPackages` at a fresh `-q` start = **20** = those 17 **+ Core** **+
  Polyhedra & Truncations** (transitive dependencies of preloaded packages).

The baseline is the **20** — the fresh-start `loadedPackages` — because a user
can reference the transitively-loaded packages (Polyhedra, Truncations) without
an explicit `needsPackage`. Sourcing from the 17-entry preload config would
wrongly exclude those. This set is version/build-dependent, so it is **not
hardcoded**: fundocs introspects `loadedPackages` on a fresh `-q` start (which
captures transitive deps) and emits it as manifest metadata; the LSP reads it as
the baseline.

`LoadedPackages(document) = default_loaded ∪ collect_imported_packages(text)`,
ordered baseline-first then import-order.

## Components

### 1. `PackagePartitionedIndex`
Built at startup. Holds `default: HashMap<Package, BuiltinData>` for the
embedded default-loaded packages, plus a lazily populated cache for
non-default packages loaded from disk on demand. Each `BuiltinData` is exactly
one package's records / lattice / facts. The `$Package$Name` qualifier on each
record routes it into the right partition at load time.

### 2. `LoadedPackages` (semantic type)
The ordered in-scope package set for a document — the "tracker." Promoted from
the ad-hoc `Vec<String>` that `active_package_indexes` returns today. Source:
the manifest's default-loaded set ∪ `collect_imported_packages(text)`.

### 3. `ScopedIndex<'a>`
A view over the ordered `&BuiltinData` of the currently-loaded packages. Exposes
the **same query API** the inference path already calls
(`resolve_call_return_type_with_options`, `resolve_call_return_type`,
`is_subtype`, `get_record`, completion/hover lookups) and delegates across the
loaded partitions with **baseline-first, then import-order** resolution
(first match wins). Call sites change from `self.builtins.foo(..)` to
`scoped.foo(..)`. Cross-package edges (a package type whose parent is a Core
type) resolve because `is_subtype` walks the ancestor chain across the loaded
set; references deref to bare names and resolve against the scoped set, mirroring
M2's loaded-package name resolution.

**Name-collision rule:** if two loaded packages define the same bare name,
baseline/Core-first then import-order wins. Documented; rare for types.

## File & loading shape (fundocs output)

- **Per-package files**, one per package (e.g. `Core.jsonl`, `Varieties.jsonl`),
  each containing that package's `CompiledObject`s with **types and `markdown`
  merged per object**. This honors the earlier types+docs merge decision (split
  on the *package* axis, not the types-vs-docs axis) and matches the partition
  model 1:1. The separate `m2-docs.jsonl` goes away — docs live in each package
  file.
- **A manifest** (`manifest.json`): the default-loaded package list (from M2's
  `loadedPackages`) and the set of available packages.
- Reference keys stay `$Package$Name`; the LSP derefs to bare names for
  intra-partition storage and uses the qualifier to route records to partitions.

### LSP loading strategy: embed default-loaded, lazy-load the rest
- **Embed** the default-loaded package files via `include_str!` so they are
  always available, zero-config, and the binary works standalone.
- **Lazy-load** non-default packages on demand from a data directory when a
  document imports them, reusing the existing `PackageIndexer` disk-loading path.
  Absent ⇒ that package's data is simply unavailable (monotone: absent = unknown,
  its symbols don't resolve — no error).
- The embedded default set is the single source of truth via a **`build.rs`**
  that reads `manifest.json` and generates the `include_str!` table, so the
  embedded list can't drift from M2's actual default set. (Fallback: a static
  list asserted against the manifest at startup.)

## Phasing

- **P1 — Format migration.** Rewrite `builtin_index.rs::BuiltinIndex::load` and
  all stale `record_from_*` / `TypeFacts::from_type_index` /
  `TypeLattice::from_type_index` builders to the new format: parse JSONL/JSONC,
  deref `$Package$Name` (retaining home package), map new kinds
  (`methodFunction`→callable, `symbol`/`table`→object), read operator `forms`
  from the top-level `operator` object (re-capitalized `Binary`/`Prefix`/
  `Postfix`), pull `markdown` into the docs map. **Bootstraps now** on the
  existing Core-only file so the migration isn't blocked on fundocs.
- **P2 — Partition + `LoadedPackages`.** Split load into per-package
  `BuiltinData`; add the manifest + default-loaded baseline; promote the
  `LoadedPackages` tracker type.
- **P3 — `ScopedIndex` + rewire queries.** Route inference / hover / navigation /
  completion / type-hierarchy through the scoped view; retire the parallel
  `PackageIndexer`-vs-`builtins` split into the one partitioned structure.
- **P4 — fundocs (separate, user-side).** Extract >Core packages; emit
  per-package files + manifest with folded `markdown`. Tracked in
  `~/m2/fundocs/LSP_COMBINED_INDEX_SPEC.md` (to be updated for per-package +
  manifest + default-loaded metadata).

## Testing

- Synthetic 2-package fixture: a fake `TestPkg` type parented to a Core type, in
  its own partition. Loaded ⇒ its methods resolve and `is_subtype` spans to Core;
  **not** loaded ⇒ the same lookups return nothing. This is the core
  loaded-only proof.
- `LoadedPackages` baseline includes the manifest default set; `needsPackage`
  adds to it; an unimported non-default package stays out.
- The existing 167 tests stay green through P1–P3 (Core behavior unchanged).

## Out of scope (YAGNI)

- Fully-qualified `InstanceID` everywhere (rejected: too invasive; partition +
  scoped resolution covers it).
- Following `load`/`input` edges for transitive package loading — only
  `needsPackage`/`loadPackage`/`debug` (what `collect_imported_packages` already
  detects).
- Versioned / multiple-installed-version package handling.
