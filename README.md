# Macaulay2 LSP

A Rust language server for [Macaulay2](https://macaulay2.com/), built on
Tree-sitter syntax analysis plus a generated database of Macaulay2 builtin
metadata (types, methods, options, documentation).

**Status: alpha.** The core capabilities below are implemented and tested, but
the type system and some heuristics are still evolving. Expect rough edges.

## Features

- Incremental document synchronization over stdio.
- Push diagnostics from Tree-sitter syntax and semantic analysis, including
  Macaulay2-specific checks: invalid method installations (non-flexible
  operators, no-effect installs, `=` vs `:=`, arity), parallel-assignment
  arity, and option-key conventions. Diagnostic messages double as M2-idiom
  hints.
- Completion for local symbols, Macaulay2 keywords, builtins, and imported
  package names.
- Hover documentation, signatures, and inferred types for both local
  symbols and indexed builtin/imported-package objects.
- Signature help for builtin and local methods with active-parameter tracking.
- Full-document semantic tokens for local, builtin, imported, and cross-file
  workspace symbols.
- Go to definition for local, workspace, builtin, package, and documentation
  targets.
- Find references within a lexical scope or across indexed workspace files.
- Document highlights for bindings, control-flow keywords, delimiters, and
  other related syntax.
- Prepare rename and rename for local and workspace symbols, including
  backtick documentation references.
- Hierarchical document symbols for bindings, assignments, and functions.
- Workspace symbol search across indexed Macaulay2 files.
- Type hierarchy preparation, supertypes, and subtypes for source and indexed
  types.
- Type and parameter inlay hints, with optional per-expression type hints.
- Quick-fix and refactor code actions for method codomains, ambiguous member
  access, raw strings, conditionals, `try`, and `else if` structure.
- Whole-document formatting with configurable indentation, line wrapping,
  control-flow layout, operator spacing, and semicolon layout.
- Folding ranges for parsed blocks and consecutive comment sections.
- Workspace indexing with live-buffer precedence and watched-file refresh.
- Runtime configuration updates with diagnostic republishing and negotiated
  inlay-hint refresh.
- Expression type inference for literals, collections, applications, operator
  dispatch, and control flow such as `if`, `try`, `for`, and `while`.

## Requirements

- Rust 1.85 or newer with Cargo.
- A `tree-sitter-macaulay2` grammar (fetched as a pinned dependency by Cargo).
- Macaulay2 itself is only needed at runtime by your editor; the builtin
  metadata is checked in.

## Build and install

Install the latest crates.io release:

```sh
cargo install m2-ls
```

To build the current source checkout instead:

```sh
cargo build --release
install -m755 target/release/m2-ls ~/.local/bin/m2-ls
```

The binary is `m2-ls` (hyphen). Point your editor's LSP client at the installed
path.

## Editor setup (Neovim, native LSP)

```lua
vim.lsp.config['m2-ls'] = {
  cmd = { vim.fn.expand('~/.local/bin/m2-ls') },
  filetypes = { 'macaulay2' },
  root_markers = { '.git' },
  settings = {
    ['m2-ls'] = {
      diagnostics = {
        enabled = true,
        disabled = {},
      },
      formatting = {
        indentWidth = 4,
        useTabs = false,
        softLineWidth = 100,
        hardLineWidth = 100,
        controlFlowLayout = 'multilineCompactElse',
        compactFactorOperators = false,
        breakAfterSemicolon = true,
      },
      inlayHints = {
        expressionTypes = false,
      },
    },
  },
}
vim.lsp.enable('m2-ls')
```

Restart the client after rebuilding (`:LspRestart m2-ls`) so it picks up a new
binary.

## Settings

Settings may be sent under the `m2-ls` or `macaulay2` section of
`workspace/didChangeConfiguration`, or directly through
`initializationOptions`. Changes apply without restarting the server. When the
client advertises `workspace.inlayHint.refreshSupport`, changing inlay-hint
settings also refreshes hints already visible in the editor.

| Setting | Default | Effect |
| --- | --- | --- |
| `diagnostics.enabled` | `true` | Enables or suppresses all published diagnostics. |
| `diagnostics.disabled` | `[]` | Suppresses selected rules by name or code, such as `unused-binding` or `E07`. |
| `formatting.indentWidth` | client value | Overrides the LSP formatting request's `tabSize`. |
| `formatting.useTabs` | client value | Overrides the LSP formatting request's `insertSpaces`. |
| `formatting.softLineWidth` | `100` | Preferred width used to choose among safe parsed line-break positions. |
| `formatting.hardLineWidth` | `100` | Triggers wrapping when exceeded; `null` or `0` disables wrapping. |
| `formatting.maxLineWidth` | unset | Compatibility setting that makes the soft and hard widths equal; `null` or `0` disables both. |
| `formatting.controlFlowLayout` | `multilineCompactElse` | Formats parsed control clauses as `compact`, `multiline`, or `multilineCompactElse`; the last form keeps the final `else value` together. |
| `formatting.compactFactorOperators` | `false` | Uses compact products such as `2*x`; the default is the conventional `2 * x`. |
| `formatting.breakAfterSemicolon` | `true` | Places the following statement on a new line; `false` keeps it inline with one space. |
| `inlayHints.expressionTypes` | `false` | Adds inferred types for expressions in addition to calm binding hints. |

Diagnostic names and stable codes are listed in `src/diagnostic_registry.rs`.
Invalid diagnostic selectors reject the settings update and leave the previous
configuration active.

## Builtin metadata

The server does not hardcode Macaulay2 builtins. They live in a single
checked-in corpus, `src/data/m2-index.jsonl` — one JSON record per builtin
object (class, methods, options, semantic-token class, and folded hover
markdown). The immutable catalog is partitioned by home package; each document
records its `needsPackage`/`importFrom` inclusions once per text version. Package
objects become visible only after their inclusion and shadow ordinary names in
inclusion/definition order; indexed aliases remain available for explicit
disambiguation. The corpus is a generated artifact produced from an installed
Macaulay2's documentation.

## Repository layout

```text
Cargo.toml
src/
  main.rs                 LSP server entry point and request handlers
  analysis.rs             scopes, bindings, installations, type inference
  builtin_index.rs        canonical builtin records and corpus loading
  typesystem.rs           type relations, dispatch, signatures
  diagnostic_registry.rs  the single registry of every diagnostic
  document.rs             per-document snapshot and incremental edits
  object_registry.rs      shared object catalog + loaded package registry
  workspace_index.rs      cross-file global definition index
  capabilities/           one module per LSP capability
  data/
    m2-index.jsonl        generated builtin corpus
```

## Development

Run from the repository root:

```sh
cargo fmt
cargo clippy --tests
cargo test
cargo build
```

The build is expected to be warning-free. See `CONTRIBUTIONS.md` for contributor
guidance.
