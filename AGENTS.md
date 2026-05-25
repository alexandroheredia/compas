# AGENTS.md — Developer Workflow

This file is for AI agents (and human collaborators) working on the `compas` codebase.

## Critical Rules

### Communication

- When explaining technical behavior to the user, prefer plain language first.
- Explain what happened, why it matters, and what was verified in simple terms.

### Git Operations

**NEVER perform git operations without explicit user approval.**

- Do NOT run `git commit`, `git push`, `git add`, `git reset`, `git rebase`, or any git mutation command unless the user explicitly asks for it
- Do NOT stage files, create branches, or merge without confirmation
- Writing commit messages is fine — but deliver them as text for the user to copy, don't execute the commit yourself
- The user controls the git history. Respect that boundary.

## Project Structure

```
compas/
├── src/
│   ├── bin/compas.rs          # CLI entry point (init, index, serve, mcp, watch)
│   ├── chunker/
│   │   ├── mod.rs             # Chunker trait + registry
│   │   ├── dart.rs            # Dart/Flutter AST parser (Tree-sitter)
│   │   └── dart_test.rs       # Unit tests for AST parsing
│   ├── config.rs              # compas.yaml deserialization
│   ├── embedder/
│   │   ├── mod.rs             # Embedder trait + factory
│   │   └── fastembed.rs       # FastEmbed embedder implementation
│   ├── graph.rs               # Symbol graph (nodes + edges, JSON persistence)
│   ├── middleware.rs           # Axum request logging middleware
│   ├── models.rs              # Chunk, SearchResult, SymbolNode structs
│   ├── mcp/
│   │   ├── mod.rs             # MCP module exports
│   │   ├── server.rs          # JSON-RPC stdio server
│   │   ├── state.rs           # Shared state (config, store, graph, embedder)
│   │   ├── tools.rs           # Tool handlers (search, graph, summary)
│   │   └── types.rs           # JSON-RPC types + MCP schema types
│   ├── server.rs              # REST API (health, search, graph, summary)
│   ├── store/
│   │   ├── mod.rs             # Store trait
│   │   ├── edge.rs            # Embedded Qdrant Edge store
│   └── watcher.rs             # File watcher with debounced reindexing
├── scripts/
│   ├── evaluate_compas.py     # Gold standard eval (13 queries, P@5 by difficulty)
│   └── test_embedding_strategy.py  # Embedding strategy comparison
├── Cargo.toml
└── README.md
```

## Build Commands

```bash
# Development build
cargo build

# Release build (use this for testing)
cargo build --release

# Run tests
cargo test

# Run a specific test
cargo test test_extract_calls -- --nocapture

# Lint
cargo clippy -- -W clippy::all

# Format
cargo fmt
```

> **Rule: After every code change, run the full hygiene cycle and iterate until everything passes.** Do not move on to the next task until all of these succeed:
> 1. `cargo fmt` (format check)
> 2. `cargo clippy -- -W clippy::all` (lint)
> 3. `cargo build --release` (or `cargo build` if only running tests)
> 4. `cargo test`

## Testing Workflow

1. **Unit tests** go in `src/chunker/dart_test.rs` or inline `#[cfg(test)]` modules
2. **Integration tests** are automated where possible and manual for real-repo smoke tests: build → index a repo → query via curl or MCP
3. **Use any initialized repo** as a test target (e.g., a Flutter project)
4. **Evaluation script** is `scripts/evaluate_compas.py` — 13 queries with gold-standard expected files, P@5 by difficulty

Quick integration test (uses HTTP server for evaluation script):

```bash
cd /path/to/your-project
/path/to/compas/target/release/compas index
/path/to/compas/target/release/compas serve
# In another terminal:
python3 /path/to/compas/scripts/evaluate_compas.py
```

### `compas init` Side Effects

`compas init` creates two files in the target repo:

1. `compas.yaml` — configuration
2. `AGENTS.md` — agent instructions telling them to use compas (query first, read second)

If `AGENTS.md` already exists, `compas init` skips it so it doesn't overwrite custom instructions.

## HTTP Server (Debugging Only)

`compas serve` provides an HTTP API on port 3001. It is **not required** for MCP or normal agent use.

Use it only when you need:
- Manual `curl` testing
- Running the evaluation script (`scripts/evaluate_compas.py`)
- Multi-repo REST access for scripts

### How it works

1. `compas init` registers the repo in `~/.config/compas/repos.json`
2. `compas serve` loads all registered repos and creates isolated state per repo
3. API clients pass `?repo=<name>` to select which repo to query
4. If only one repo is registered, it becomes the default (no `repo` param needed)

### Starting the server

```bash
# Foreground (for testing)
compas serve

# Then query:
curl "http://localhost:3001/search?repo=my-repo&q=auth"
```

Stop it when done — it does not need to stay running.

### Restarting

`compas serve` auto-detects if another instance is already running on the same port. It kills the old process and starts fresh. Just run it again:

```bash
compas serve          # kills old instance, starts new one
```

### Environment variables

```bash
COMPAS_HOST=127.0.0.1  # bind address (default: 127.0.0.1)
COMPAS_PORT=3001       # bind port (default: 3001)
```

### API changes

All REST endpoints now accept a `repo` parameter:

```bash
# List registered repos
curl http://localhost:3001/repos

# Search a specific repo
curl 'http://localhost:3001/search?repo=my-repo&q=auth%20logic'

# Graph query for a specific repo
curl 'http://localhost:3001/graph?repo=my-repo&symbol=AuthService.login'
```

### MCP Configuration (VS Code, Claude Desktop, etc.)

No wrapper script needed. Point your MCP client directly at the `compas` binary:

```json
{
  "servers": {
    "compas": {
      "type": "stdio",
      "command": "/path/to/compas/target/release/compas",
      "args": ["mcp"]
    }
  }
}
```

The MCP server auto-detects which repo to query from the current working directory (set by the editor). It falls back to the default repo if only one is registered. If multiple repos exist and auto-detection fails, pass `"repo": "<name>"` in the tool arguments.

## Adding a New Language

1. Create `src/chunker/<lang>.rs`
2. Implement `Chunker` trait:
   - `language()` → return language name
   - `chunk(file_path, content)` → return `Vec<Chunk>`
3. Add `extract_calls(content)` → return `Vec<(caller, callee)>`
4. Register in `ChunkerRegistry::new()` in `src/chunker/mod.rs`
5. Add Tree-sitter grammar to `Cargo.toml`
6. Write unit tests for AST parsing

## Key Conventions

- **Errors are non-fatal.** The indexer continues past bad files. Never `?` early in the file loop.
- **Config is `compas.yaml`** in the repo root. `AppConfig::load()` reads it.
- **Graph keys are `filepath:symbolname`**. Always use `symbol_key()` helper.
- **Chunk symbols strip `_pN` suffix** before graph registration (see `strip_part_suffix`).
- **Max chunk size is 6000 chars.** Defined as `MAX_CHUNK_CHARS` in `src/chunker/dart.rs`.
- **Chunk enrichment is `{doc_comments}\n{filename} {symbol}\n{source_code}`**. This replaces the old LLM summarizer.
- **Tree-sitter wraps some sigs in `method_signature`.** Use `unwrap_method_signature()` to get the inner `factory_constructor_signature`, `getter_signature`, `setter_signature`, etc.

## Common Pitfalls

- **Tree-sitter node kinds vary by grammar version.** The Dart grammar uses `class_declaration` not `class_definition`. Always verify with `cargo test debug_dart_ast -- --nocapture`.
- **Dart `class_member` nodes wrap all method signatures.** The outer node is always `method_signature`; the real kind (factory constructor, getter, etc.) is inside. Use `unwrap_method_signature()`.
- **MCP stdio server auto-detects repos from cwd.** No wrapper script needed — point `mcp.json` directly at the `compas` binary with `"args": ["mcp"]`.
- **Edge shard vector dims must match the embedding model.** If you switch from `nomic-ai/nomic-embed-text-v1.5` (FastEmbed) to another model, delete `.compas/edge-shard` and reindex. Dimensions are determined at runtime on first embed.

## Search Ranking System

Search quality is a combination of **semantic similarity** (Qdrant Edge vector search) plus **post-search re-ranking** applied in both `src/mcp/tools.rs` and `src/server.rs`.

### Current Boosts & Penalties

Applied to raw Qdrant results before deduplication:

| Signal                                   | Boost | Rationale                                                                                                                        |
| ---------------------------------------- | ----- | -------------------------------------------------------------------------------------------------------------------------------- |
| Query token in **symbol name**           | +0.12 | Direct name matches should outrank coincidental similarity                                                                       |
| Query token in **file path**             | +0.10 | File-level relevance (e.g. `product_service.dart` for "product" queries)                                                         |
| **Class** kind                           | +0.05 | Canonical definitions are usually the best starting point                                                                        |
| **Method** kind                          | +0.02 | Slight preference over fields/getters                                                                                            |
| Graph caller/callee contains query token | +0.10 | Cross-reference boost: a symbol called by `saveMetadata` is relevant to "metadata" even if the symbol itself says "product info" |
| **Private helper** (`_`-prefixed)        | −0.15 | Penalise internal plumbing unless the query explicitly asks for "private"/"helper"/"internal"                                    |

### Deduplication

- Collapse part-chunks (`_p1`, `_p2`) by deduplicating on `(file_path, stripped_symbol)`.
- Cap at **3 symbols per file** to preserve diversity across the codebase.

### Default Limits

- MCP `search_codebase`: **10** (was 5)
- REST `/search`: **15** (was 5)

## Known Limitations

1. **Semantic vocabulary gaps.** The embedding model (`nomic-ai/nomic-embed-text-v1.5` via FastEmbed) does not bridge certain synonym pairs:
   - "metadata" ↔ "product info" (`ProductService.getInfoById` is invisible to "metadata" queries)
   - "AI" ↔ "Claude" (`AIService` is invisible to "AI" queries)
     These are fundamental to the model, not fixable with query-time heuristics.

2. **Private helpers with literal keyword matches.** `_getNextMetadataId` contains both query tokens but is semantically irrelevant. The −0.15 penalty pushes it down but doesn't eliminate it entirely when the raw score is high.

3. **Flat score distribution on semantically similar chunks.** Multiple methods from the same model class (e.g. `Product.toJson`, `Product.toString`) cluster tightly because their embeddings are nearly identical.

## Future Improvements (Backlog)

1. **Graph-enriched chunk indexing** — At index time, append `/// Called by: X, Y` and `/// Calls: Z` to each chunk's content. This would make `ProductService.getInfoById`'s chunk mention `_formatData` and `_fetchByUrl`, giving it stronger semantic ties to "metadata" and "product". **Requires reindexing.**

2. **Hybrid search** — Combine vector similarity with Qdrant's full-text payload index on `symbol` and `file_path`. This would catch exact file-name matches even when semantic similarity drifts.

3. **Query expansion / synonym injection** — Detect vocabulary gaps (e.g. "AI" → "Claude", "metadata" → "book info") and inject synonyms into the query embedding or perform parallel searches.

4. **Graph-driven result expansion** — After returning top-N results, optionally include 1-hop callers/callees in the response. This helps agents discover the full call chain without multiple round-trips.

5. **Per-query adaptive boosting** — Learn optimal boost weights per query type (e.g. "how does X work" queries benefit more from graph boost, while "where is Y defined" queries benefit more from symbol-name boost). Could be calibrated via the evaluation script.
