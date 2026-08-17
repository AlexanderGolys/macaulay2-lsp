# Incremental analysis roadmap

The analysis engine should become a demand-driven semantic graph over
`m2-syn`, starting with typechecking. `m2-syn` owns typed Macaulay2 syntax and
generic traversal; this repository continues to own scopes, source-ordered
environments, object metadata, type relations, diagnostics, and LSP results.

## Readiness boundary

`m2-syn` already provides the pieces needed for a shadow typechecker:
`SourceFile`/`Expr` node hierarchies, source spans, `Visit`, `VisitMut`, `Fold`,
and Tree-sitter reconstruction. Before it can replace the editor's current
syntax input, two integration gaps need to be addressed there:

1. Add a loss-tolerant reconstruction path. `parse_file` currently rejects the
   complete typed file when Tree-sitter reports one error or missing node. An
   LSP must retain typed islands around incomplete code while the user types.
2. Allow reconstruction from the LSP's retained Tree-sitter tree, or let an
   `m2-syn` parser accept the previous tree and edits. Parsing independently in
   both crates would discard the incremental parse already maintained by
   `DocumentSnapshot`.

The grammar versions must also be unified before integration: this server is
currently pinned to `tree-sitter-macaulay2` 5.0.0 while the local `m2-syn`
workspace targets 6.0.0. Comment/trivia reconstruction is not a typechecking
blocker; the existing concrete tree can continue to serve formatting and
documentation features until `m2-syn` models trivia.

`m2-syn::Span` remains provenance, not semantic identity. Its byte offsets are
converted through `DocumentSource`; they are never used directly as UTF-16 LSP
positions.

## Target graph

The graph uses typed identifiers rather than strings or source positions as
keys:

```text
SourceId -> CellId -> SyntaxId -> ScopeId
                         |          |
                         v          v
                       ExprId --> BindingStateId
                         |          |
                         +------> EnvironmentId
                                      |
                                      v
                 ObjectId / TypeId / CallableObjectId / MethodId
                                      |
                                      v
                    TypeFact / CallFact / DiagnosticFact
```

`SyntaxId` and `CellId` belong to this semantic graph, not to `m2-syn`. They
are reconciled between document revisions from the incremental Tree-sitter
change ranges plus typed syntax shape. `EnvironmentId` identifies the lexical
scope and source-order knowledge visible at a query. Consequently `type_of` is
keyed by `(ExprId, EnvironmentId)`, preventing a result computed after an
import or reassignment from leaking into an earlier expression.

The durable inputs are:

- the typed syntax for each cell;
- scope and binding-state edges;
- source-ordered package inclusions and method installations;
- workspace exports; and
- the immutable builtin object/type catalog.

The primary derived queries are `binding_at`, `type_of`, `call_facts`,
`applicable_methods`, `type_definition`, `implementations`, and diagnostics.
LSP capabilities project these facts and do not walk syntax independently.

## Migration stages

### 1. Establish the syntax adapter

- Add `m2-syn` as a path dependency while its API is experimental.
- Assign one persistent semantic `SourceId` per open document.
- Lower the retained Tree-sitter tree into typed cells without reparsing.
- Keep the current `M2Node` path authoritative and compare the typed cell,
  assignment, binding, call, and control-flow shapes in integration tests.

Exit condition: every valid pipeline fixture lowers, incomplete edits preserve
the unaffected typed cells, and the adapter introduces no second parse.

### 2. Port type inference in shadow mode

- Move expression dispatch to an `m2_syn::visit::Visit` implementation.
- Preserve `InferredType`, `TypeKnowledge`, nominal `ObjectName`/`TypeId`, and
  the conservative `Unknown` behavior.
- Make external lookups deferred graph dependencies rather than callbacks
  hidden inside traversal.
- Run both typecheckers for the pipeline corpus and compare binding types, call
  facts, method codomains, and diagnostic evidence without changing LSP output.

Exit condition: the typed implementation is behavior-equivalent for source
types, rings, operators, method dispatch, package visibility, output
references, closures, and error-diverging branches.

### 3. Persist the semantic graph

- Store cells, scopes, bindings, binding states, calls, installations, and type
  facts in one analysis database owned by `DocumentSnapshot`.
- Record dependency edges while queries run and invalidate their reverse
  closure after edits.
- Begin at cell granularity: a changed cell invalidates its own facts and later
  cells whose source-order environment depends on its exports. Then refine to
  syntax and binding-state granularity without changing query APIs.
- Treat package imports, reassignments, method installations, and referenced
  output cells as explicit environment edges.

Exit condition: editing a function body does not rebuild unrelated cells, while
editing an exported binding or import invalidates every dependent result.

### 4. Move capability consumers

Switch hover, inlay hints, semantic tokens, signature help, completion,
declaration/definition, type definition, implementation, and diagnostics to
graph queries. Formatting, documentation extraction, and syntax-only folding
may remain on the concrete tree. Remove the old semantic Tree-sitter walkers
only after the last consumer moves.

Exit condition: there is one owner for every semantic fact and no capability
performs its own type or binding traversal.

### 5. Measure and harden

- Record per-update counts for retained cells, invalidated semantic nodes, and
  recomputed queries.
- Add edit-sequence tests for imports, reassignments, output references,
  method installations, workspace exports, and temporarily invalid syntax.
- Keep compiled-server tests as the behavior gate and add graph-level tests
  only for invalidation boundaries that cannot be observed through one LSP
  response.

The first implementation slice should be stages 1 and 2 only. It proves that
`m2-syn` is a sufficient typed boundary and that type behavior is unchanged
before persistence and invalidation complicate the migration.
