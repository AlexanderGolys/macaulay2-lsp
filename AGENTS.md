# Repository Guidelines

Use the Obsidian project docs as project memory; keep this file focused on routing and day-to-day contributor rules.

## Project Manifest

```yaml
project: macaulay2-lsp
repo: /home/flux/m2/macaulay2-lsp
vault: /home/flux/obsidian/flux
docs_root: macaulay2-lsp
index: macaulay2-lsp/Index.md
roadmap: macaulay2-lsp/Roadmap.md
architecture: macaulay2-lsp/Architecture.md
features: macaulay2-lsp/Features
plans: macaulay2-lsp/Plans
decisions: macaulay2-lsp/Decisions
changelog: macaulay2-lsp/Changelog.md
canvases: macaulay2-lsp/Canvases
open_questions: macaulay2-lsp/Open Questions.md
```

## Project Structure

The active Rust language-server crate is `m2_ls/`. Server entrypoint and LSP wiring live in `m2_ls/src/main.rs`; scope/diagnostic analysis lives in `m2_ls/src/analysis.rs`; builtin metadata loading and token classification live in `m2_ls/src/typesystem.rs`. Runtime builtin data is split across `m2_ls/src/data/builtins.names` and `m2_ls/src/data/builtins.details.jsonl`; the files are line-aligned. Treat all `target/` directories as build output.

## Build and Test Commands

Run Rust commands from `m2_ls/`.

- `cargo check`: fast compile/type check.
- `cargo build`: build the LSP binary.
- `cargo run`: run the LSP server on stdio.
- `cargo fmt`: format Rust code.
- `cargo test`: run Rust tests.
- `M2 --script scripts/extract_builtins.m2 src/data/builtins.details.jsonl`: regenerate the line-aligned builtin names and detail files from installed M2 docs/runtime.
- `M2 --script scripts/extract_builtins.m2 /tmp/builtins-debug.details.jsonl + % Ring`: debug the extractor on a trimmed symbol list without touching the checked-in database.
- `M2 --script scripts/extract_builtins.m2 --rich /tmp/builtins-rich.details.jsonl + % Ring`: include full structured Hypertext docs for docs-pipeline experiments.

The Macaulay2 Tree-sitter parser is resolved from GitHub via Cargo, pinned in `m2_ls/Cargo.toml`.

## Documentation Rules

`m2_ls/src/problems.md` records upstream Macaulay2 bugs or surprising behavior, not this repository's issue list. Store durable roadmap, architecture, decisions, plans, and feature notes in the Obsidian docs listed in the manifest. Code wins when docs drift. The intended documentation flow is upstream/runtime extraction -> curated project docs -> compact hover/index data; do not treat rich JSONL scrapes as final hover sources.
