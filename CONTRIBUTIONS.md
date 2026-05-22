# Contributing

This repository contains an experimental Rust language server for Macaulay2. Keep changes focused on the active `m2_ls/` crate unless a task explicitly touches repository docs or metadata.

## Project Structure

- `m2_ls/src/main.rs`: LSP server setup, request handlers, semantic tokens, hover, completion, and go-to-definition wiring.
- `m2_ls/src/analysis.rs`: syntax and scope analysis over Tree-sitter parse trees.
- `m2_ls/src/typesystem.rs`: builtin metadata loading, type hierarchy helpers, and token classification.
- `m2_ls/src/data/builtins.names`: compact builtin symbol list for live lookup.
- `m2_ls/src/data/builtins.details.jsonl`: line-aligned builtin records used for hover and classification.
- `m2_ls/scripts/extract_builtins.m2`: Macaulay2 runtime/doc extractor for regenerating builtin data.

Treat every `target/` directory as build output.

## Development Commands

Run Rust commands from `m2_ls/`.

```sh
cargo check
cargo fmt
cargo test
cargo build
cargo clippy
```

`cargo run` starts the LSP server on stdio. Editor integrations should normally point at `m2_ls/target/debug/m2_ls` after `cargo build`.

## Validation

Before committing Rust changes, run:

```sh
cargo fmt && cargo test && cargo build && cargo clippy
```

For LSP behavior changes, also test through an editor client when practical. Non-ASCII text matters: LSP positions are UTF-16, while Tree-sitter positions are byte-based.

## Builtin Metadata

Regenerate the checked-in builtin database from `m2_ls/`:

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

`m2_ls/src/problems.md` records upstream Macaulay2 bugs or surprising behavior. It is not this repository's issue tracker.

Keep `README.md` user-facing and concise. Keep this file contributor-facing and actionable.
