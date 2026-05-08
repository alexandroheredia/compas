# compas

> Ask, don't read.

**compas** is a local semantic search engine for your codebase. It indexes your code using embeddings + AST analysis, then answers natural language queries like "where is authentication handled?" with ranked, relevant code snippets, complete with file paths, line numbers, and call relationships.

It runs entirely on your machine (Ollama + Qdrant) and exposes its capabilities via MCP so AI agents (Copilot, Claude, Cursor) can search your code without burning tokens on irrelevant files.

> **For AI agents setting this up:** Read [`docs/SETUP.md`](docs/SETUP.md) - exact, copy-pasteable commands.

---

## What It Does

- **Semantic Search**: "how does caching work?" finds `CacheService.put()` even if the word "cache" never appears in the method name
- **Symbol Graph**: See who calls what. Prevents agents from rewriting code that already exists

Both are exposed as MCP tools that agents call directly.

## Quick Start

**Prerequisites:** Rust 1.75+, Docker, Ollama

```bash
# 1. Start Qdrant
docker compose up -d

# 2. Pull embedding model
ollama pull nomic-embed-text

# 3. Build
git clone https://github.com/alexandroheredia/compas.git
cd compas
cargo build --release

# 4. Initialize a project
cd your-project
/path/to/compas/target/release/compas init

# 5. Index
/path/to/compas/target/release/compas index

# 6. Start daemon
/path/to/compas/target/release/compas serve

# 7. Query
curl "http://localhost:3001/search?q=how+does+caching+work"
```

## MCP Integration

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

## Multi-Repo

`compas serve` is a global daemon. Index multiple repos, query them all from one process:

```bash
cd repo-a && compas init
cd repo-b && compas init
compas serve
curl "http://localhost:3001/search?repo=repo-b&q=cache"
```

Repos are registered in `~/.config/compas/repos.json`.

## How It Works

1. **Parse**: Tree-sitter extracts methods, classes, and call relationships from the AST
2. **Chunk**: Each symbol becomes a chunk enriched with doc comments + source code
3. **Embed**: Chunks are embedded via Ollama and stored in Qdrant
4. **Graph**: Call relationships are persisted as JSON for fast lookup

Indexing is incremental, unchanged files are skipped on reindex.

**Why local-first:** Your code never leaves your machine. No API keys, no rate limits, no vendor lock-in.

## Language Support

Currently **Dart/Flutter** only. TypeScript and Python support is on the roadmap.

> **Want to add a language?** Check the [contributing guide](CONTRIBUTING.md). It's ~200 lines of Rust to implement a new chunker.

## Limitations

- Dart/Flutter only (other languages need a Tree-sitter grammar + chunker)
- Embedding model vocabulary gaps: "AI" may not match "Claude", "metadata" may not match "product info"
- Dynamic dispatch (e.g., `Function.call`) isn't traced in the graph

## Roadmap

- [ ] Ignore files with something like `.compasignore`
- [ ] TypeScript/JavaScript support
- [ ] Python support
- [ ] Graph-enriched chunk indexing
- [ ] Hybrid search (vector + full-text)

## License

MIT
