# Contributing

This repository contains an experimental Rust language server for Macaulay2. The crate (`m2-ls`) lives at the repository root. Keep changes focused on the crate unless a task explicitly touches repository docs or metadata.

## Project Structure

- `src/main.rs`: LSP server setup, request handlers, semantic tokens, hover, completion, and go-to-definition wiring.
- `src/analysis.rs`: syntax and scope analysis over Tree-sitter parse trees.
- `src/typesystem.rs`: builtin metadata loading, type hierarchy helpers, and token classification.
- `src/capabilities/`: per-feature LSP handlers (hover, formatting, navigation, diagnostics, …).
- `src/data/m2-types.jsonl`, `src/data/m2-docs.jsonl`: line-aligned builtin records used for hover and classification.

Treat every `target/` directory as build output.

## Development Commands

Run Rust commands from the repository root.

```sh
cargo check
cargo fmt
cargo test
cargo build
cargo clippy
```

`cargo run` starts the LSP server on stdio. Editor integrations should normally point at `target/debug/m2-ls` after `cargo build`.

## Validation

Before committing Rust changes, run:

```sh
cargo fmt && cargo test && cargo build && cargo clippy
```

For LSP behavior changes, also test through an editor client when practical. Non-ASCII text matters: LSP positions are UTF-16, while Tree-sitter positions are byte-based.

## Builtin Metadata

Regenerate the checked-in builtin database from the repository root:

```sh
M2 --script scripts/extract_builtins.m2 src/data/builtins.details.jsonl
```

The generated `.names` and `.details.jsonl` files are line-aligned. Do not sort one without regenerating or updating the other consistently.

For extractor experiments, write to `/tmp`:

```sh
M2 --script scripts/extract_builtins.m2 /tmp/builtins-debug.details.jsonl + % Ring
M2 --script scripts/extract_builtins.m2 --rich /tmp/builtins-rich.details.jsonl + % Ring
```

## Documentation Notes

`src/problems.md` records upstream Macaulay2 bugs or surprising behavior. It is not this repository's issue tracker.

Keep `README.md` user-facing and concise. Keep this file contributor-facing and actionable.
