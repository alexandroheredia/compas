# Changelog

All notable changes to this project will be documented in this file.

## compas 2026.5.9

`compas init` now generates a `.compasignore` file alongside `compas.yaml`, pre-filled
with sensible defaults for the detected language (Flutter, TypeScript, Rust, Python, Go).
It works like `.gitignore`: one glob pattern per line, `#` for comments, trailing `/`
matches the whole directory tree. Files matching any pattern are skipped during indexing
and cleaned out of Qdrant and the symbol graph on the next reindex.

The missing-docs audit no longer flags standard Flutter lifecycle methods (`build`,
`createState`, `initState`, `dispose`, and friends) as missing documentation. They are
framework boilerplate, not real findings.

The missing-docs audit now covers the entire codebase. Previously, files that were
unchanged since the last index were silently skipped during the audit pass.

## compas 2026.5.8

Initial release.

- Semantic search via Qdrant + Ollama embeddings
- Symbol call graph via Tree-sitter AST
- MCP server with `search_codebase` and `get_symbol_graph`
- Global multi-repo daemon (`compas serve`)
- Incremental indexing with content hashing
- Dart/Flutter chunker with doc comment enrichment
- Code audit report (missing docs, dead code candidates)
