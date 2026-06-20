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
captures transitive deps) and emits it as a leading metadata record in the index
file; the LSP reads it as the baseline.

`LoadedPackages(document) = default_loaded ∪ collect_imported_packages(text)`,
ordered baseline-first then import-order.

## Components

### 1. `PackagePartitionedIndex`
Built once at startup from the embedded `m2-index.jsonl`. Holds
`HashMap<Package, BuiltinData>` — each `BuiltinData` is exactly one package's
records / lattice / facts. The `$Package$Name` qualifier on each record routes
it into the right partition at load time. (No lazy disk cache — see the
single-file loading strategy below.)

### 2. `LoadedPackages` (semantic type)
The ordered in-scope package set for a document — the "tracker." Promoted from
the ad-hoc `Vec<String>` that `active_package_indexes` returns today. Source:
the folded `default_loaded` metadata ∪ `collect_imported_packages(text)`.

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

### 4. Import lifecycle (add / remove)
`LoadedPackages` is a **pure function of the document text**, never a mutated
list — so adding or removing an import needs no dedicated event handling:

- **On change**, re-derive `LoadedPackages` from the current text
  (`default_loaded ∪ collect_imported_packages`). An added `needsPackage` shows
  up in the set; a removed one drops out. Reuse the snapshot's existing parse
  tree — `collect_imported_packages` currently spins up its own parser, which
  this work should fix.
- **No load/unload I/O:** all shipped packages' partitions are resident (one
  embedded file), so importing = include that partition in the scoped view,
  removing = exclude it. Toggling scope, not loading data.
- **Import absent from the corpus:** the set holds a name with no partition ⇒ no
  data, symbols don't resolve (monotone, no error); removing it is a no-op.
- **Caching:** memoize `LoadedPackages` / `ScopedIndex` per document **version**;
  recompute only when the version changes. (Strict improvement over today, which
  recomputes `active_package_indexes(text)` per request.)

## File & loading shape (fundocs output)

**Partitioning is an in-memory concern, not a file-layout one.** Every record
carries its home `package` (and refs are `$Package$Name`), so per-package
partitions are built in memory from *any* file granularity. Read-once means the
parse cost is identical regardless of how many files the data is split across —
splitting would only ever save binary size / peak memory, never time. The corpus
is small (Core ≈ 5.5 MB types+docs, already embedded today; the realistic
multi-package ceiling is low tens of MB since we ship the default set plus a
curated few, not all ~300 packages). So:

- **One file `m2-index.jsonl`** — every shipped package's `CompiledObject`s with
  **types and `markdown` merged per object**. The separate `m2-docs.jsonl` goes
  away (docs live on each record). This still honors the types+docs merge
  decision; it just declines the per-package *file* split as premature.
- **Default-loaded baseline metadata folded into the file** — a single leading
  metadata record carrying `default_loaded` (M2's fresh-start `loadedPackages`,
  capturing transitive deps). No separate manifest file; the baseline rides with
  the corpus it describes.
- Reference keys stay `$Package$Name`; the LSP derefs to bare names for
  intra-partition storage and uses the qualifier to route records to partitions.

### LSP loading strategy: single embedded file, partition in memory
- **Embed** the single `m2-index.jsonl` via `include_str!` (status quo for the
  current Core assets) — zero-config, binary works standalone, no data directory
  or deploy change.
- **Partition in memory** by each record's `package` into the
  `PackagePartitionedIndex`; the leading metadata record's `default_loaded` names
  seed the baseline. No `build.rs` embed table and no lazy disk-loading machinery
  are needed (one `include_str!`).
- A document importing a package absent from the corpus simply gets no data for
  it (monotone: absent = unknown, its symbols don't resolve — no error).

### Revisit trigger (deferred — YAGNI)
*If* fundocs ever extracts enough packages that the embedded blob bloats the
binary, switch to a default-blob (embedded) + extras-blob (lazy-loaded from a
data dir) split. Because partitioning is already in-memory, that is a pure
**storage** change with zero impact on `PackagePartitionedIndex` / `ScopedIndex`
/ scoping.

## Phasing

- **P1 — Format migration.** Rewrite `builtin_index.rs::BuiltinIndex::load` and
  all stale `record_from_*` / `TypeFacts::from_type_index` /
  `TypeLattice::from_type_index` builders to the new format: parse JSONL/JSONC,
  deref `$Package$Name` (retaining home package), map new kinds
  (`methodFunction`→callable, `symbol`/`table`→object), read operator `forms`
  from the top-level `operator` object (re-capitalized `Binary`/`Prefix`/
  `Postfix`), pull `markdown` into the docs map. **Bootstraps now** on the
  existing Core-only file so the migration isn't blocked on fundocs.
- **P2 — Partition + `LoadedPackages`.** Partition the single loaded file into
  per-package `BuiltinData` by each record's `package`; read the folded
  `default_loaded` metadata as the baseline; promote the `LoadedPackages` tracker
  type.
- **P3 — `ScopedIndex` + rewire queries.** Route inference / hover / navigation /
  completion / type-hierarchy through the scoped view; retire the parallel
  `PackageIndexer`-vs-`builtins` split into the one partitioned structure.
- **P4 — fundocs (separate, user-side).** Extract >Core packages; emit the
  single `m2-index.jsonl` with `markdown` folded per record and a leading
  `default_loaded` metadata record (from `loadedPackages`). Tracked in
  `~/m2/fundocs/LSP_COMBINED_INDEX_SPEC.md` (to be updated for the single-file
  shape + folded default-loaded metadata).

## Testing

- Synthetic 2-package fixture: a fake `TestPkg` type parented to a Core type, in
  its own partition. Loaded ⇒ its methods resolve and `is_subtype` spans to Core;
  **not** loaded ⇒ the same lookups return nothing. This is the core
  loaded-only proof.
- `LoadedPackages` baseline includes the folded `default_loaded` set;
  `needsPackage` adds to it; an unimported non-default package stays out.
- The existing 167 tests stay green through P1–P3 (Core behavior unchanged).

## Out of scope (YAGNI)

- Fully-qualified `InstanceID` everywhere (rejected: too invasive; partition +
  scoped resolution covers it).
- Following `load`/`input` edges for transitive package loading — only
  `needsPackage`/`loadPackage`/`debug` (what `collect_imported_packages` already
  detects).
- Versioned / multiple-installed-version package handling.
