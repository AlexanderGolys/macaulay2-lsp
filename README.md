# Macaulay2 LSP

Experimental Rust language server for [Macaulay2](https://macaulay2.com/), built around Tree-sitter syntax analysis and generated runtime metadata from an installed `M2`.

The project is intentionally small right now: one active Rust crate, one generated builtin database, and enough LSP behavior to make Neovim testing practical.

## Features

- Full-document sync over stdio via `tower-lsp`.
- Tree-sitter parsing for basic syntax diagnostics.
- Local go-to-definition for symbols discovered in the current document.
- Hover and semantic tokens from generated Macaulay2 builtin metadata.
- Completion for builtin symbols using the split builtin name/detail database.

## Repository Layout

```text
Cargo.toml
src/
  main.rs
  analysis.rs
  typesystem.rs
  capabilities/
  data/
    m2-types.jsonl
    m2-docs.jsonl
```

## Requirements

- Rust stable toolchain with Cargo.
- Macaulay2 available as `M2` when regenerating builtin metadata.
- Network access for Cargo when fetching the pinned Git dependencies:
  - `tree-sitter`
  - `tree-sitter-macaulay2`

## Development

Run commands from the repository root.

```sh
cargo check
cargo fmt
cargo test
cargo build
cargo clippy
```

Use `cargo run` to start the language server on stdio. Editor configs should point at the built binary, for example:

```text
/home/flux/m2/macaulay2-lsp/target/debug/m2-ls
```

## Builtin Metadata

The server does not hardcode Macaulay2 builtins. Regenerate the split database from an installed `M2`:

```sh
M2 --script scripts/extract_builtins.m2 src/data/builtins.details.jsonl
```

This writes line-aligned files:

- `src/data/builtins.names`: compact symbol list for completion.
- `src/data/builtins.details.jsonl`: detailed records for hover, semantic tokens, and type metadata.

For extractor debugging without touching checked-in data:

```sh
M2 --script scripts/extract_builtins.m2 /tmp/builtins-debug.details.jsonl + % Ring
M2 --script scripts/extract_builtins.m2 --rich /tmp/builtins-rich.details.jsonl + % Ring
```

## Documentation

Contributor guidance lives in `CONTRIBUTIONS.md`. Local agent/project-memory routing may live in an ignored `AGENTS.md`.
