---
name: build-m2-ls
description: Build and validate the Macaulay2 m2-ls Rust language server. Use when asked to format, check, lint, test, build, run, or install the server in macaulay2-lsp.
---

# Build m2-ls

For complete validation, run from the repository root in this order and stop at
the first failure:

```sh
cargo fmt
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

Do not install the server during formatting, checking, linting, testing, or
building. Installation changes the executable used by the user's editor and
requires an explicit install request.

Only when the user explicitly asks to install, run:

```sh
cargo install --path .
cp /home/flux/.cargo/bin/m2-ls /home/flux/.local/bin/m2-ls
```

After installation, verify that the installed and copied binaries match with
`sha256sum`.
