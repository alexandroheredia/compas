# AI Agent Setup Guide for compas

**Purpose:** This document is designed for AI agents to read and execute. It contains exact, copy-pasteable commands to set up compas from scratch. If you are a human reading this, the [README](../README.md) is more pleasant.

---

## What compas Does

compas is a local semantic search engine for codebases. It indexes your code using embeddings + AST analysis, then exposes two MCP tools that you can call:

1. **`search_codebase`**: Find code by natural language meaning (not just string matching)
2. **`get_symbol_graph`**: See who calls what (call graph navigation)

It runs entirely on the user's machine using FastEmbed (local embeddings) and embedded Qdrant Edge (vector database).

---

## Prerequisites Check

Before starting, verify these are installed and running:

### 1. Rust

```bash
rustc --version
# Expected: 1.75.0 or higher
```

If missing, install via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

### 3. Git

```bash
git --version
```

---

## Step-by-Step Setup

### Step 1: Clone and Build compas

```bash
# Clone the repository
git clone https://github.com/alexandroheredia/compas.git
cd compas

# Build the release binary
cargo build --release

# Verify the binary exists
ls -la target/release/compas
```

The binary path is: `{repo_root}/target/release/compas`
(Example: `/Users/yourname/GitHub/compas/target/release/compas`)

> **Note:** The embedding model (`nomic-ai/nomic-embed-text-v1.5`) downloads automatically on first `compas index` via FastEmbed. No manual model setup is required.

### Step 2: Configure MCP in the Editor

The compas binary exposes an MCP server over stdio. Configure your editor to launch it.

#### VS Code

Create or edit the MCP configuration file:

**File:** `~/Library/Application Support/Code/User/mcp.json` (macOS)
**File:** `%APPDATA%\Code\User\mcp.json` (Windows)
**File:** `~/.config/Code/User/mcp.json` (Linux)

```json
{
  "servers": {
    "compas": {
      "type": "stdio",
      "command": "{path_to_compas_binary}",
      "args": ["mcp"]
    }
  }
}
```

Replace `{path_to_compas_binary}` with the absolute path from Step 1.

Example:

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

#### Claude Desktop

Edit the Claude Desktop configuration:

**File:** `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS)

```json
{
  "mcpServers": {
    "compas": {
      "command": "{path_to_compas_binary}",
      "args": ["mcp"]
    }
  }
}
```

#### Cursor

Cursor reads the same MCP config as VS Code. Use the same `mcp.json` file location.

### Step 4: Initialize a Repository

Navigate to the project you want to index and run `compas init`:

```bash
cd /path/to/your/project
{path_to_compas_binary} init
```

This creates two files:

- `compas.yaml`: Configuration (language, include/exclude patterns)
- `AGENTS.md`: Instructions for AI agents working in this repo

It also registers the repo in the global registry at `~/.config/compas/repos.json`.

### Step 5: Index the Repository

```bash
cd /path/to/your/project
{path_to_compas_binary} index
```

Expected output (example):

```
Indexing /path/to/my-repo  (52 files, 0 changed, 0 deleted)

     @@@@@@@   @@@@@@   @@@@@@@@@@   @@@@@@@    @@@@@@    @@@@@@
     ...

    my-repo indexed in 24s

    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
     52 changed  ·  0 skipped  ·  0 deleted
     520 chunks  ·  0 failed
    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    ⚠️  107 symbols missing doc comments
    🪦  21 dead code candidates

    📊 Graph  → /path/to/my-repo/.compas/graph.json
    📋 Audit  → .compas/audit.md

Optimizing edge shard...
✓ Edge shard optimized
```

### Step 6: Start the HTTP Server (Optional — Debugging Only)

`compas serve` is **not required** for MCP or normal agent use. Start it only when you need HTTP access for manual testing, scripts, or the evaluation script:

```bash
{path_to_compas_binary} serve
```

Expected output:

```
Server running on http://127.0.0.1:3001
```

Stop it when done — it does not need to stay running.

Important distinction:

- `compas mcp` = stdio tool server used by editors/agents via MCP (**primary mode**)
- `compas serve` = HTTP server on port 3001 for REST, scripts, and manual debugging only

If you only need editor/agent tool calls, your editor will launch `compas mcp` for you from the MCP config. You do not normally need to type `compas mcp` by hand.

### Step 7: Verify Everything Works

#### Test MCP

If using VS Code, reload the window to pick up the new MCP config:

```
Cmd+Shift+P → "Developer: Reload Window"
```

Then ask your agent: "Search the codebase for authentication logic"

The agent should call `search_codebase` and receive ranked results.

#### Test REST API

```bash
# List registered repos
curl http://localhost:3001/repos

# Search a specific repo
curl "http://localhost:3001/search?repo=your-repo-name&q=authentication"

# Get call graph for a symbol
curl "http://localhost:3001/graph?repo=your-repo-name&symbol=AuthService.login"
```

---

## Multi-Repo Setup

To index multiple repositories:

```bash
# Repo 1
cd /path/to/repo1
{path_to_compas_binary} init
{path_to_compas_binary} index

# Repo 2
cd /path/to/repo2
{path_to_compas_binary} init
{path_to_compas_binary} index
```

Query via MCP with `repo` parameter (no daemon needed):

```json
{
  "query": "cache logic",
  "repo": "repo2",
  "limit": 10
}
```

If you need HTTP access for scripts or manual testing, start `compas serve` temporarily:

```bash
{path_to_compas_binary} serve
curl "http://localhost:3001/search?repo=repo2&q=cache+logic"
```

---

## Troubleshooting

### "repo 'X' not found"

- The repo name is case-sensitive in the registry but case-insensitive in lookups (fixed in latest version)
- Check available repos: `curl http://localhost:3001/repos`
- Verify the repo was registered: `cat ~/.config/compas/repos.json`

### "configuration file compas.yaml not found"

- Run `compas init` in the repo root first
- The MCP server no longer requires `compas.yaml` in cwd (loads from global registry)

### MCP server not starting

- Verify the binary path in `mcp.json` is absolute and correct
- Check binary permissions: `chmod +x target/release/compas`
- Reload VS Code after any config changes

### Search returns no results

- Verify indexing completed: check `.compas/graph.json` exists
- Verify the shard exists: check `{repo}/.compas/edge-shard/`
- Reindex if needed: `{path_to_compas_binary} index`

---

## File Reference

| File                           | Purpose                                     |
| ------------------------------ | ------------------------------------------- |
| `~/.config/compas/repos.json`  | Global registry of initialized repos        |
| `{repo}/compas.yaml`           | Per-repo configuration                      |
| `{repo}/.compas/edge-shard/`   | Embedded Qdrant Edge shard                  |
| `{repo}/.compas/graph.json`    | Symbol call graph                           |
| `{repo}/.compas/audit.md`      | Code quality report                         |
| `{repo}/.compas/manifest.json` | Incremental indexing manifest (file hashes) |

---

## Quick Reference: compas Commands

| Command        | Purpose                                                |
| -------------- | ------------------------------------------------------ |
| `compas init`  | Initialize a repo (creates config, registers globally) |
| `compas index` | Index/reindex the current repo                         |
| `compas optimize` | Optimize the embedded edge shard                    |
| `compas serve` | Start the HTTP daemon for REST, scripts, evals, and multi-repo access |
| `compas mcp`   | Start the MCP stdio tool server used by editors and AI agents |
| `compas watch` | Watch files and auto-reindex (experimental)            |

---

## Environment Variables

| Variable      | Default     | Purpose                                          |
| ------------- | ----------- | ------------------------------------------------ |
| `COMPAS_HOST` | `127.0.0.1` | REST server bind address                         |
| `COMPAS_PORT` | `3001`      | REST server port                                 |
| `RUST_LOG`    | unset       | Set to `info` for verbose logging (disables TUI) |

---

## Need Help?

Open an issue on Github and let's talk.
