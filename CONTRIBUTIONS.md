# Contributing

This repository contains an experimental Rust language server for Macaulay2. The crate (`m2-ls`) lives at the repository root. Keep changes focused on the crate unless a task explicitly touches repository docs or metadata.

## Project Structure

- `src/main.rs`: LSP server setup, request handlers, semantic tokens, hover, completion, and go-to-definition wiring.
- `src/analysis.rs`: syntax and scope analysis over Tree-sitter parse trees.
- `src/typesystem.rs`: builtin metadata loading, type hierarchy helpers, and token classification.
- `src/client_capabilities.rs`: typed client capability negotiation and shared workspace refresh behavior.
- `src/settings.rs`: typed initialization and live workspace configuration.
- `src/capabilities/`: per-feature LSP handlers (hover, formatting, navigation, diagnostics, …).
- `src/data/m2-index.jsonl`: the checked-in builtin corpus — one JSON record per builtin object (types, callables, objects, option keys), used for hover, classification, and type inference.

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
cargo fmt -- --check
cargo test
cargo check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build
cargo package --allow-dirty
```

For LSP behavior changes, also test through an editor client when practical. Non-ASCII text matters: LSP positions are UTF-16, while Tree-sitter positions are byte-based.

## Builtin Metadata

`src/data/m2-index.jsonl` is a generated artifact: one JSON record per line,
produced from an installed Macaulay2's documentation and runtime metadata by the
extractor pipeline. The extractor is not yet shipped in this repository, so do
not hand-edit the corpus — regenerate it wholesale instead. The leading
`{"kind":"meta", ...}` record (the default-loaded package baseline) is
mandatory; the server fails fast at startup without it.

## Documentation Notes

`src/problems.md` records upstream Macaulay2 bugs or surprising behavior. It is not this repository's issue tracker.

Keep `README.md` user-facing and concise. Keep this file contributor-facing and actionable.
