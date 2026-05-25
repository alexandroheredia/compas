# compas

> Ask, don't read.   
> TLDR: compas saves your agent from reading 3 or 4 extra irrelevant files every time it needs to understand your code. Saving you about 30% of tokens on code exploration and comprehension tasks.


**compas** is a local semantic search engine for your codebase. It indexes your code using embeddings + AST analysis, then answers natural language queries like "where is authentication handled?" with ranked, relevant code snippets, complete with file paths, line numbers, and call relationships.

It runs entirely on your machine (FastEmbed + embedded Qdrant Edge) and exposes its capabilities via MCP so AI agents (Copilot, Claude, Cursor) can search your code without burning tokens on irrelevant files.

> **For AI agents setting this up:** Read [`docs/SETUP.md`](docs/SETUP.md) - exact, copy-pasteable commands.

---

## What It Does

- **Semantic Search**: "how does caching work?" finds `CacheService.put()` even if the word "cache" never appears in the method name
- **Symbol Graph**: See who calls what. Prevents agents from rewriting code that already exists

Both are exposed as MCP tools that agents call directly.

## Quick Start

**Prerequisites:** Rust 1.75+

```bash
# 1. Build
git clone https://github.com/alexandroheredia/compas.git
cd compas
cargo build --release

# 2. Initialize a project
cd your-project
/path/to/compas/target/release/compas init

# 3. Index (first run downloads the embedding model)
/path/to/compas/target/release/compas index

# 4. Set up MCP in your editor
# See MCP Integration below — no daemon needed
```

## MCP Integration

**This is the primary way agents use compas.** Your editor launches `compas mcp` as a stdio tool server. No background daemon is required.

Add to your editor's MCP config:

**VS Code** (`~/Library/Application Support/Code/User/mcp.json`):

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

No wrapper script needed.

### Tools

**`search_codebase`**: Semantic search by meaning. Use this first.

```json
{ "query": "user authentication logic", "repo": "my-app", "limit": 10 }
```

**`get_symbol_graph`**: Trace call relationships.

```json
{ "symbol": "AuthService.login", "repo": "my-app" }
```

### Example Flow

**User:** "How does authentication work?"

**Agent:**

1. `search_codebase("user authentication password hashing", repo="my-app")`
2. Gets `AuthService.authenticate` as top result
3. `get_symbol_graph("AuthService.authenticate", repo="my-app")`
4. Sees called by `LoginScreen._handleSubmit`
5. Opens the confirmed file, no guessing, no wasted tokens.

## compas serve (HTTP debugging only)

`compas serve` provides an HTTP API on port 3001. It is **not required** for MCP or normal agent use. Use it only when you need:

- Manual `curl` testing
- Running the evaluation script (`scripts/evaluate_compas.py`)
- Multi-repo REST access for scripts

```bash
# Start when needed
/path/to/compas/target/release/compas serve

# Query
curl "http://localhost:3001/search?repo=repo-b&q=cache"

# Stop when done — it does not need to stay running
```

Repos are registered in `~/.config/compas/repos.json`.

## How It Works

1. **Parse**: Tree-sitter extracts methods, classes, and call relationships from the AST
2. **Chunk**: Each symbol becomes a chunk enriched with doc comments + source code
3. **Embed**: Chunks are embedded locally with FastEmbed and stored in a repo-local Qdrant Edge shard
4. **Graph**: Call relationships are persisted as JSON for fast lookup

Indexing is incremental, unchanged files are skipped on reindex.

**Why local-first:** Your code never leaves your machine. No API keys, no rate limits, no vendor lock-in.

## Language Support

Currently supports **Dart/Flutter** and **Rust**. TypeScript and Python support are on the roadmap.

> **Want to add a language?** Check the [contributing guide](CONTRIBUTING.md).

## Limitations

- Only Dart/Flutter and Rust are currently supported (other languages need a Tree-sitter grammar + chunker)
- Embedding model vocabulary gaps: "AI" may not match "Claude", "metadata" may not match "product info"
- Dynamic dispatch (e.g., `Function.call`) isn't traced in the graph

## Roadmap

- [x] Ignore files with something like `.compasignore`
- [x] Rust support
- [ ] TypeScript/JavaScript support
- [ ] Python support
- [ ] Graph-enriched chunk indexing
- [ ] Hybrid search (vector + full-text)

## License

MIT
