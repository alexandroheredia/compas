# Contributing to compas

Thanks for considering a contribution. This is a small project with a focused scope: local-first semantic search for codebases. We keep the bar high for simplicity and correctness.

## Getting Started

```bash
# 1. Clone
git clone https://github.com/alexandroheredia/compas.git
cd compas

# 2. Build
cargo build --release

# 3. Run tests
cargo test
```

## Before Submitting

Run these in order. CI will reject PRs that fail any step.

```bash
# 1. Auto-fix clippy lints (may reformat code)
cargo clippy --fix --allow-dirty --allow-staged -- -W clippy::all

# 2. Format (clippy --fix can change formatting)
cargo fmt

# 3. Run tests
cargo test

# 4. Verify release build compiles
cargo build --release
```

**Critical:** Always run `cargo fmt` after `cargo clippy --fix`. The fixer sometimes breaks long lines in ways that don't match `rustfmt`, causing CI format check failures even though the code compiles.

## What to Contribute

### High Priority

- **New language support** — TypeScript, Python, Go, Rust. See "Adding a Language" below.
- **Search quality improvements** — Better ranking, query expansion, hybrid search
- **Bug fixes** — Especially in chunker AST parsing or graph extraction

### Lower Priority

- **UI polish** — TUI improvements, output formatting
- **Documentation** — README, setup guides, examples

### Out of Scope

- Cloud-hosted backends (the project is local-first)
- LLM-based summarization (removed in favor of doc comment enrichment)
- New REST endpoints (keep the API minimal)

## Adding a Language

1. Create `src/chunker/<lang>.rs`
2. Implement the `Chunker` trait:
   - `language()` → return language name
   - `chunk(file_path, content)` → return `Vec<Chunk>`
3. Add `extract_calls(content)` → return `Vec<(caller, callee)>`
4. Register in `ChunkerRegistry::new()` in `src/chunker/mod.rs`
5. Add Tree-sitter grammar to `Cargo.toml`
6. Write unit tests in `src/chunker/<lang>_test.rs` or inline `#[cfg(test)]`

See `src/chunker/dart.rs` as the reference implementation.

## Pull Request Process

1. **Open an issue first** for significant changes (new language, architecture changes)
2. **One logical change per PR** — don't bundle unrelated fixes
3. **Include tests** for new functionality
4. **Update docs** if you change user-facing behavior (README, AGENTS.md, SETUP.md)
5. **Verify `cargo build --release`** succeeds before submitting

## Code Style

- **Rust:** Standard `rustfmt`. No custom configuration.
- **Errors are non-fatal:** The indexer continues past bad files. Never `?` early in the file loop.
- **Minimal changes:** Don't refactor unrelated code. Stay focused.
- **Graph keys:** Always use `filepath:symbolname` format. Use `symbol_key()` helper.
- **Chunk symbols:** Strip `_pN` suffix before graph registration (`strip_part_suffix`).

## Testing

### Unit Tests

```bash
# All tests
cargo test

# Specific test
cargo test test_extract_calls -- --nocapture

# Debug AST output
cargo test debug_dart_ast -- --nocapture
```

### Integration Tests

Use any initialized repo as your test target:

```bash
cd /path/to/your-project
/path/to/compas/target/release/compas index
/path/to/compas/target/release/compas serve
# In another terminal:
python3 /path/to/compas/scripts/evaluate_compas.py
```

The evaluation script runs 13 queries with gold-standard expected files. P@5 should be ≥ 0.85.

If you don't have a suitable repo, use any open source Flutter project (e.g., [flutter/samples](https://github.com/flutter/samples)) as a test fixture.

## Architecture Notes

- **Config:** `compas.yaml` in repo root. `AppConfig::load()` reads it.
- **Global registry:** `~/.config/compas/repos.json` for multi-repo access.
- **Graph persistence:** `.compas/graph.json` per repo.
- **Manifest:** `.compas/manifest.json` for incremental indexing hashes.
- **Max chunk size:** 6000 chars. Defined as `MAX_CHUNK_CHARS` in `src/chunker/dart.rs`.

## Communication

- **Issues:** Use GitHub issues for bugs and feature requests
- **Discussions:** Use GitHub discussions for questions and ideas
- **No DMs:** Keep everything in the open for visibility

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
