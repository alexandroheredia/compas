---
name: compas
description: "Codebase semantic search and symbol graph navigation. Use BEFORE opening any file you are not 100% certain about. Triggers on uncertainty: 'I need to find', 'where is', 'which file', 'I'm not sure where', 'let me check', 'I think it's in', 'probably in', 'looks like'. Enforces: query compas first, open only what compas confirms."
---

You are an AI agent using compas, a local semantic search engine for codebases. Follow these rules exactly.

## Rule 1: Search First, Open Second

Before opening ANY file you are not 100% certain about, call `search_codebase`.

BAD: "The auth logic is probably in lib/services/auth.dart" → opens file → wrong file.
GOOD: Call `search_codebase` → get ranked results → open confirmed file.

### When to Use `search_codebase`

| Trigger Phrase          | Example                                       |
| ----------------------- | --------------------------------------------- |
| "I need to find..."     | "I need to find where books are cached"       |
| "Where is..."           | "Where is the Hardcover metadata model?"      |
| "Which file..."         | "Which file handles AI description cleanup?"  |
| "I'm not sure where..." | "I'm not sure where the provider setup lives" |
| "Let me check..."       | "Let me check how caching works"              |
| "I think it's in..."    | "I think it's in the service layer"           |
| "Probably in..."        | "Probably in `lib/models/`"                   |
| "Looks like..."         | "Looks like there's a cache somewhere"        |
| "How does X work?"      | "How does the app cache book metadata?"       |
| "Find the code for..."  | "Find the code for the book card UI"          |

**Also use when:**

- You are about to run `grep`, `find`, or `rg` to locate a symbol
- You need cross-file relationships (who calls this function?)
- You are exploring an unfamiliar area of the codebase

**Skip `search_codebase` ONLY** when you already know the exact file path and line number.

---

## Rule 2: ALWAYS Pass the `repo` Parameter

This is the most common failure mode. ALWAYS include `"repo"` in every `search_codebase` and `get_symbol_graph` call.

The MCP server cannot reliably auto-detect which repo you are in because some editors spawn MCP from internal directories, not the workspace root.

If you forget `repo`, you get:

```
Error: missing 'repo' parameter and could not auto-detect from cwd. Available repos: repo-a, repo-b
```

How to find the repo name:

- Check the workspace folder name
- Ask the user if unsure
- Repo names are case-insensitive

---

## Tool: `search_codebase`

```json
{
  "query": "user authentication with password hashing",
  "repo": "my-app",
  "limit": 10
}
```

**Parameters:**

- `query` (required): Natural language. Describe what you want, not regex.
  - ✅ Good: `"user authentication with password hashing"`
  - ❌ Bad: `"class.*Auth"` — compas is semantic, not regex
  - ❌ Bad: `"getUserById"` — use `get_symbol_graph` for exact symbols
- `repo` (required): ALWAYS pass this.
- `limit` (optional): Default 10. Use 15–20 for exploration.
- `language` (optional): Filter by language, e.g. `"dart"`.

**After receiving results:**

1. Read the top 3–5 previews
2. Pick the most relevant symbol by score and name
3. Open ONLY that file
4. Do NOT open files that did not appear in results

---

## Tool: `get_symbol_graph`

Use AFTER finding a relevant symbol to trace its call chain.

```json
{
  "symbol": "AuthService.authenticate",
  "repo": "my-app"
}
```

**Parameters:**

- `symbol` (required): Symbol name, e.g. `"AuthService.authenticate"` or `"UserProvider"`
- `file` (optional): File path to disambiguate if multiple symbols share a name
- `repo` (required): ALWAYS pass this

**Use this when:**

- The user asks "how does X work?"
- You need to know what calls a function (impact analysis)
- You need related functions that the search missed

---

## Two-Step Workflow

For discovery tasks, always do both steps:

**Step 1 — Search:**

```json
{
  "query": "how does the app cache user sessions",
  "repo": "my-app",
  "limit": 15
}
```

Returns: `SessionCache.store`, `UserService.login`, `AuthProvider.signIn`

**Step 2 — Deepen:**

```json
{ "symbol": "SessionCache.store", "repo": "my-app" }
```

Returns: Called by `UserService.login`. Calls `_encryptToken`, `SharedPreferences.setString`.

**Step 3 — Read:**
Open the most relevant file confirmed by both tools.

---

## If Search Returns Nothing

1. Rephrase with synonyms:
   - `"AI"` → `"Claude"` or `"OpenAI"`
   - `"metadata"` → `"product details"` or `"book info"`
   - `"cache"` → `"storage"` or `"offline"`
2. Try a broader query with a higher limit
3. Fall back to text search tools ONLY after compas fails — log the miss

If MCP search fails with `failed to open WAL` or `Resource temporarily unavailable`, make sure `compas serve` is running. MCP can fall back to the daemon for search, but only if the daemon is available.

---

## Pitfalls

| Pitfall                                       | Fix                                                                      |
| --------------------------------------------- | ------------------------------------------------------------------------ |
| Forgetting `repo` parameter                   | ALWAYS pass `repo` in every call                                         |
| Opening files based on assumptions            | ALWAYS search compas first                                               |
| Using exact symbol names in `search_codebase` | Use natural language; use `get_symbol_graph` for exact symbols           |
| Ignoring low scores (< 0.6)                   | Rephrase the query — vocabulary mismatch                                 |
| Skipping the graph                            | After finding a symbol, check `get_symbol_graph` for "how does it work?" |
| Giving up after one query                     | Rephrase and retry before reaching for grep                              |

---

## Decision Tree

```
User asks about code location or behavior?
  ├─ Do you know the EXACT file and line? → Open directly
  └─ Any uncertainty at all?
      ├─ search_codebase(query, repo="...")
      ├─ Relevant symbol found?
      │   ├─ User asks "how does it work?" → get_symbol_graph(symbol, repo="...")
      │   └─ Open confirmed file
      └─ No relevant symbol?
          ├─ Rephrase query (synonyms)
          ├─ Retry search_codebase
          └─ Still nothing → grep (last resort)
```

---

## Example: Good vs Bad

**BAD:**

> User: "How does authentication work?"
> Agent: Thinks "Auth is in `lib/services/auth_service.dart`"
> Agent: Opens `lib/services/auth_service.dart` — does not exist
> Agent: Tries `lib/auth/auth_service.dart` — wrong again
> Agent: Runs grep for "auth" across 50 files — overwhelming noise

**GOOD:**

> User: "How does authentication work?"
> Agent: Calls `search_codebase("user authentication password login", limit=15, repo="my-app")`
> Agent: Gets `AuthService.authenticateUser` as #1 result
> Agent: Calls `get_symbol_graph("AuthService.authenticateUser", repo="my-app")`
> Agent: Opens `lib/services/auth_service.dart` confirmed by both tools
