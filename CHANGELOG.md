# Changelog

All notable changes to this project will be documented in this file.

## [1.0.0] - 2026-08-27

### Added

- Added source-aware declaration and implementation navigation across the
  workspace, including method installations and local lambda assignments.
- Added exact-point and upper-closure type ranges with normalized union and
  intersection operations.
- Added contextual completion patterns for package imports, types after `new`,
  callable option keys and values, and symbol prefixes with only a few possible
  endings.

### Changed

- Migrated analysis and CST traversal to the typed `m2-syn` 0.2 interface.
- Made completion a pattern-to-query pipeline with shared symbol sources,
  visibility, type filtering, ranking, de-duplication, and prefix edits.
- Tightened method-installation, source-order, scope, and package-visibility
  analysis throughout editor capabilities.
- Updated to `tree-sitter-macaulay2` 6.1 and removed local dependency links from
  release builds.

### Fixed

- Preserved compatibility with the grammar's renamed binary-expression operand
  field.
- Suppressed broad completion lists in ordinary expression positions.

## [0.1.1] - 2026-08-01

### Changed

- Renamed the GitHub repository to `m2-ls` and updated package metadata.
- Documented every implemented language-server capability.
- Removed the GitHub Actions workflow in favor of local release validation.

## [0.1.0] - 2026-08-01

### Added

- Tree-sitter-backed Macaulay2 parsing, diagnostics, formatting, and semantic tokens.
- Completion, hover, signature help, navigation, rename, symbols, highlights, and type hierarchy.
- Source-aware type inference, method dispatch, package visibility, and inlay hints.
- Generated builtin and package metadata with documentation and method signatures.
- Full-process JSON-RPC coverage for core language-server workflows.

[1.0.0]: https://github.com/AlexanderGolys/m2-ls/releases/tag/v1.0.0
[0.1.1]: https://github.com/AlexanderGolys/m2-ls/releases/tag/v0.1.1
[0.1.0]: https://github.com/AlexanderGolys/m2-ls/releases/tag/v0.1.0
