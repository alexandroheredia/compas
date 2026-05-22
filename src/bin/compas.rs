use async_trait::async_trait;
use clap::{Parser, Subcommand};
use compas::{
    chunker::{
        dart::extract_semantic_references, extract_calls_for_language, language_for_path,
        ChunkerRegistry,
    },
    config::AppConfig,
    embedder::{build_embedder, EmbedMode},
    graph::Graph,
    mcp::{self, state::McpAppState},
    server::{router, AppState, RepoState},
    store::{edge::EdgeStore, Store},
    watcher::{FileWatcher, Handler},
};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

const EMBED_BATCH_SIZE: usize = 32;

const FLUTTER_LIFECYCLE_METHODS: &[&str] = &[
    "build",
    "createState",
    "initState",
    "dispose",
    "didChangeDependencies",
    "didUpdateWidget",
    "didChangeAppLifecycleState",
    "deactivate",
    "activate",
    "reassemble",
    "setState",
    "mount",
    "unmount",
];

const FLUTTER_CALLBACK_SUFFIXES: &[&str] = &[
    "onPressed",
    "onTap",
    "onChanged",
    "onSaved",
    "onSubmitted",
    "onEditingComplete",
];

/// Members that are dispatched by frameworks/runtime, not by direct call sites.
/// These should never be flagged as dead code because the absence of a reference
/// in source is meaningless — Flutter, Dart, json_serializable, etc. call them.
const FRAMEWORK_DISPATCHED_MEMBERS: &[&str] = &[
    // Flutter widget lifecycle
    "build",
    "createState",
    "initState",
    "dispose",
    "didChangeDependencies",
    "didUpdateWidget",
    "didChangeAppLifecycleState",
    "deactivate",
    "activate",
    "reassemble",
    "mount",
    "unmount",
    // InheritedWidget / InheritedNotifier
    "updateShouldNotify",
    "updateShouldNotifyDependent",
    // ChangeNotifier / Listenable
    "notifyListeners",
    "addListener",
    "removeListener",
    // Dart object protocol
    "toString",
    "toJson",
    "fromJson",
    "fromMap",
    "toMap",
    "fromSnapshot",
    "fromDoc",
    "fromDocument",
    "noSuchMethod",
    "hashCode",
    // App entrypoint
    "main",
    // Flutter render/paint hooks
    "paint",
    "shouldRepaint",
    "shouldRebuildSemantics",
    "performLayout",
    "performResize",
    // Stream/Future protocol
    "call",
];

fn is_framework_dispatched(member_name: &str) -> bool {
    if FRAMEWORK_DISPATCHED_MEMBERS.contains(&member_name) {
        return true;
    }
    // operator overloads (==, +, -, [], etc.) are dispatched by the runtime
    if member_name.starts_with("operator ") {
        return true;
    }
    false
}

#[derive(Debug, Clone)]
struct AuditDeclaration {
    file: String,
    display_file: String,
    symbol: String,
    kind: String,
}

#[derive(Debug, Clone)]
struct AuditReference {
    #[allow(dead_code)]
    caller_file: String,
    callee: String,
}

#[derive(Debug, Clone, Default)]
struct AuditFileAnalysis {
    declarations: Vec<AuditDeclaration>,
    references: Vec<AuditReference>,
    file_edges: Vec<(String, String)>,
    key_types: Vec<String>,
}

#[derive(Debug, Clone)]
struct DeadCodeCandidate {
    file: String,
    symbol: String,
    kind: String,
}

#[derive(Parser)]
#[command(name = "compas")]
#[command(about = "Your agent's compa. A local-first context engine for LLM agents.")]
struct Cli {
    #[arg(short, long, default_value = "compas.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize compas.yaml for the current repository
    Init,
    /// Index the repository
    Index,
    /// Optimize the local edge shard
    Optimize,
    /// Start the HTTP daemon for REST, eval scripts, and multi-repo access
    Serve,
    /// Start the MCP stdio tool server for editors and AI agents
    Mcp,
    /// Watch files and auto-reindex
    Watch,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => init_repo(),
        Commands::Serve => serve().await,
        Commands::Mcp => run_mcp().await,
        cmd => {
            let config = AppConfig::load(cli.config.to_str().unwrap())?;
            match cmd {
                Commands::Index => index_repo(config).await,
                Commands::Optimize => optimize_repo(config).await,
                Commands::Watch => watch(config).await,
                Commands::Init | Commands::Serve | Commands::Mcp => unreachable!(),
            }
        }
    }
}

fn init_repo() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let repo_name = cwd.file_name().unwrap_or_default().to_string_lossy();
    let config_path = cwd.join("compas.yaml");

    if config_path.exists() {
        println!("compas.yaml already exists. Delete it first if you want to regenerate.");
        return Ok(());
    }

    // Detect dominant language
    let mut counts: HashMap<String, usize> = HashMap::new();
    for entry in walkdir::WalkDir::new(&cwd).max_depth(3) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        // Skip dependency directories that can skew counts
        let path_str = path.to_string_lossy();
        if path_str.contains("/node_modules/")
            || path_str.contains("/.dart_tool/")
            || path_str.contains("/target/")
            || path_str.contains("/.venv/")
            || path_str.contains("/venv/")
            || path_str.contains("/vendor/")
        {
            continue;
        }
        if let Some(ext) = path.extension() {
            let ext = ext.to_string_lossy().to_string();
            if matches!(
                ext.as_str(),
                "dart" | "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java"
            ) {
                *counts.entry(ext).or_insert(0) += 1;
            }
        }
    }

    let mut dominant = counts
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(e, _)| e.as_str());

    // If this is a Flutter project (has pubspec.yaml), prefer Dart even if
    // JS/TS files outnumber it (e.g. node_modules-like contamination).
    let has_pubspec = cwd.join("pubspec.yaml").exists() || cwd.join("pubspec.yml").exists();
    if has_pubspec && counts.get("dart").copied().unwrap_or(0) > 0 {
        dominant = Some("dart");
    }

    let (include, exclude, compasignore_lines) = match dominant {
        Some("dart") => (
            vec!["lib/**/*.dart", "test/**/*.dart"],
            vec!["**/*.g.dart", "build/**", ".dart_tool/**"],
            vec![
                "# Flutter/Dart autogenerated files",
                "**/*.g.dart",
                "**/*.freezed.dart",
                "**/*.gr.dart",
                "**/app_localizations*.dart",
                "",
                "# Build artifacts",
                ".dart_tool/",
                "build/",
                "**/generated/",
            ],
        ),
        Some("ts") | Some("tsx") => (
            vec!["src/**/*.{ts,tsx}", "lib/**/*.{ts,tsx}"],
            vec!["node_modules/**", "dist/**", "build/**"],
            vec![
                "# TypeScript autogenerated files",
                "**/*.d.ts",
                "",
                "# Build artifacts",
                "node_modules/",
                "dist/",
                "build/",
            ],
        ),
        Some("rs") => (
            vec!["src/**/*.rs", "crates/**/*.rs"],
            vec!["target/**"],
            vec!["# Rust build artifacts", "target/", "Cargo.lock"],
        ),
        Some("py") => (
            vec!["**/*.py"],
            vec!["venv/**", ".venv/**", "__pycache__/**"],
            vec![
                "# Python cache and environments",
                "__pycache__/",
                "*.pyc",
                ".pytest_cache/",
                ".venv/",
                "venv/",
            ],
        ),
        Some("go") => (
            vec!["**/*.go"],
            vec!["vendor/**"],
            vec!["# Go vendor and generated", "vendor/", "*.gen.go"],
        ),
        _ => (
            vec!["src/**/*"],
            vec!["build/**", "dist/**"],
            vec!["# Build artifacts", "build/", "dist/"],
        ),
    };

    let yaml = format!(
        r#"repo:
  path: .
  include:
{}
  exclude:
{}

embedder:
  provider: fastembed
  model: nomic-ai/nomic-embed-text-v1.5

store:
  provider: edge
  path: .compas/edge-shard
  vector_name: default

server:
  host: 127.0.0.1
  port: "3001"

index:
  chunk_by: function
  watch: true
"#,
        include
            .iter()
            .map(|s| format!("    - \"{}\"", s))
            .collect::<Vec<_>>()
            .join("\n"),
        exclude
            .iter()
            .map(|s| format!("    - \"{}\"", s))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    std::fs::write(&config_path, yaml)?;

    // Write .compasignore
    let ignore_path = cwd.join(".compasignore");
    if !ignore_path.exists() {
        let ignore_content = compasignore_lines.join("\n") + "\n";
        std::fs::write(&ignore_path, ignore_content)?;
        println!("Created .compasignore in {:?}", cwd);
    } else {
        println!(".compasignore already exists. Skipping.");
    }

    // Register repo in global registry
    let mut registry = compas::config::RepoRegistry::load();
    let repo_name = repo_name.to_lowercase().replace(' ', "_");
    registry.register(&repo_name, &cwd);
    registry.save()?;
    println!("Registered repo '{}' in global registry", repo_name);

    // Write AGENTS.md so agents know to use compas
    let agents_md = r#"# AGENTS.md — Navigation Rules

## MANDATORY RULE

For ANY task where you do not already know the exact file path and line number, your FIRST action MUST be `search_codebase`.

NEVER use `list_dir`, `read_file`, or regex search for initial exploration.
NEVER browse the directory tree to get oriented before searching.
NEVER assume you know where code lives because of file names or folder structure.

## Compas (Local Semantic Search)

This repo is indexed by compas. It finds symbols by natural language meaning and knows the call graph. It is faster and more accurate than manual browsing.

### Tools

| Tool | Use When |
|------|----------|
| `search_codebase` | ALWAYS FIRST. Any time you need to locate, understand, or explore code. |
| `get_symbol_graph` | After search, when you need to trace callers/callees of a specific symbol. |

### Correct Workflow

1. Search: `search_codebase({ query: "...", limit: 10 })`
2. Deepen (optional): `get_symbol_graph({ symbol: "..." })`
3. Read: Open ONLY the exact file(s) and line ranges compas returned

### What NOT to do

WRONG: Reading `lib/foo/bar.dart` because "the logic is probably there."
RIGHT: `search_codebase({ query: "how does X work", limit: 10 })` then read only the confirmed results.

---

Generated by `compas init`. Update or remove this file as needed.
"#;

    let agents_path = cwd.join("AGENTS.md");
    if !agents_path.exists() {
        std::fs::write(&agents_path, agents_md)?;
        println!("Created AGENTS.md in {:?}", cwd);
    } else {
        println!("AGENTS.md already exists. Skipping.");
    }

    // Write compas skill and instructions into .github/ if the folder exists.
    // These are always overwritten so they stay in sync with the latest compas version.
    let github_dir = cwd.join(".github");
    if github_dir.exists() {
        let skill_md = r#"---
name: compas
description: "Codebase semantic search and symbol graph navigation. Use BEFORE opening any file you are not 100% certain about. Triggers on uncertainty: 'I need to find', 'where is', 'which file', 'I'm not sure where', 'let me check', 'I think it's in', 'probably in', 'looks like'. Enforces: query compas first, open only what compas confirms."
---

## MANDATORY RULE

For ANY task where you do not already know the exact file path and line number, your FIRST action MUST be `search_codebase`.

NEVER use `list_dir`, `read_file`, or regex search for initial exploration.
NEVER browse the directory tree to get oriented before searching.
NEVER assume you know where code lives because of file names or folder structure.

## ALWAYS Pass the repo Parameter

This is the most common failure mode. ALWAYS include `repo` in every `search_codebase` and `get_symbol_graph` call.

The MCP server cannot reliably auto-detect which repo you are in because some editors spawn MCP from internal directories, not the workspace root.

If you forget `repo`, you get:

    Error: missing 'repo' parameter and could not auto-detect from cwd.

## Tool: search_codebase

Parameters:
- query (required): Natural language. Describe what you want, not regex.
  Good: "user authentication with password hashing"
  Bad: "class.*Auth" — compas is semantic, not regex
  Bad: "getUserById" — use `get_symbol_graph` for exact symbols
- repo (required): ALWAYS pass this.
- limit (optional): Default 10. Use 15-20 for exploration.
- language (optional): Filter by language, e.g. "dart".

After receiving results:
1. Read the top 3-5 previews
2. Pick the most relevant symbol by score and name
3. Open ONLY that file
4. Do NOT open files that did not appear in results

## Tool: get_symbol_graph

Use AFTER finding a relevant symbol to trace its call chain.

Parameters:
- symbol (required): Symbol name, e.g. "AuthService.authenticate"
- file (optional): File path to disambiguate
- repo (required): ALWAYS pass this

## Two-Step Workflow

Step 1 — Search:
`search_codebase({ query: "how does X work", repo: "...", limit: 15 })`

Step 2 — Deepen (optional):
`get_symbol_graph({ symbol: "X", repo: "..." })`

Step 3 — Read:
Open the most relevant file confirmed by both tools.

## Pitfalls

| Pitfall | Fix |
|---------|-----|
| Forgetting repo parameter | ALWAYS pass repo in every call |
| Opening files based on assumptions | ALWAYS search compas first |
| Using exact symbol names in search_codebase | Use natural language; use get_symbol_graph for exact symbols |
| Skipping the graph | After finding a symbol, check get_symbol_graph for "how does it work?" |

Generated by compas init. Update or remove this file as needed.
"#;

        let skills_dir = github_dir.join("skills").join("compas");
        std::fs::create_dir_all(&skills_dir)?;
        let skill_path = skills_dir.join("SKILL.md");
        std::fs::write(&skill_path, skill_md)?;
        println!("Created .github/skills/compas/SKILL.md");

        let instructions_dir = github_dir.join("instructions");
        std::fs::create_dir_all(&instructions_dir)?;
        let instruction_path = instructions_dir.join("repo-navigation.instructions.md");
        let instructions_md = r#"---
name: repo-navigation
description: "Use when exploring, locating, or understanding code in this repository. Enforces search_codebase as the mandatory first action before any file read, directory listing, or regex search. Covers compas tool usage, correct workflow order, and anti-patterns to avoid."
applyTo: "**"
---

## MANDATORY RULE

For ANY task where you do not already know the exact file path and line number, your FIRST action MUST be `search_codebase`.

NEVER use `list_dir`, `read_file`, or regex search for initial exploration.
NEVER browse the directory tree to get oriented before searching.
NEVER assume you know where code lives because of file names or folder structure.

## Compas (Local Semantic Search)

This repo is indexed by compas. It finds symbols by natural language meaning and knows the call graph. It is faster and more accurate than manual browsing.

### Tools

| Tool | Use When |
|------|----------|
| `search_codebase` | ALWAYS FIRST. Any time you need to locate, understand, or explore code. |
| `get_symbol_graph` | After search, when you need to trace callers/callees of a specific symbol. |

### Correct Workflow

1. Search: `search_codebase({ query: "...", limit: 10 })`
2. Deepen (optional): `get_symbol_graph({ symbol: "..." })`
3. Read: Open ONLY the exact file(s) and line ranges compas returned

### What NOT to do

WRONG: Reading `lib/foo/bar.dart` because "the logic is probably there."
RIGHT: `search_codebase({ query: "how does X work", limit: 10 })` then read only the confirmed results.
"#;
        std::fs::write(&instruction_path, instructions_md)?;
        println!("Created .github/instructions/repo-navigation.instructions.md");
    }

    println!("Created compas.yaml in {:?}", cwd);
    println!("Detected language: {}", dominant.unwrap_or("unknown"));
    println!("\nNext step:");
    println!("  Index repo:  compas index");
    println!("\nThe embedding model downloads once on first index and is cached globally.");
    println!("\nTo make 'compas' available everywhere, copy the binary to your PATH:");
    println!("  cp /path/to/compas/target/release/compas /usr/local/bin/");

    Ok(())
}

fn hash_bytes(bytes: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    format!("{:x}", hasher.finish())
}

fn is_flutter_boilerplate(symbol: &str) -> bool {
    let method_name = symbol.rsplit('.').next().unwrap_or(symbol);
    FLUTTER_LIFECYCLE_METHODS.contains(&method_name)
}

struct CompasIgnore {
    matchers: Vec<globset::GlobMatcher>,
}

impl CompasIgnore {
    fn load(repo_path: &std::path::Path) -> Self {
        let path = repo_path.join(".compasignore");
        let mut matchers = Vec::new();

        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Ok(glob) = globset::Glob::new(line) {
                    matchers.push(glob.compile_matcher());
                }
                // Directory patterns (ending in /) should also match nested files
                if line.ends_with('/') {
                    let nested = format!("{}**", line);
                    if let Ok(glob) = globset::Glob::new(&nested) {
                        matchers.push(glob.compile_matcher());
                    }
                }
            }
        }

        Self { matchers }
    }

    fn is_ignored(&self, relative_path: &std::path::Path) -> bool {
        let path_str = relative_path.to_string_lossy();
        for matcher in &self.matchers {
            if matcher.is_match(&*path_str) {
                return true;
            }
        }
        false
    }
}

fn relative_display_path(repo_path: &Path, file: &Path) -> String {
    file.strip_prefix(repo_path)
        .unwrap_or(file)
        .to_string_lossy()
        .to_string()
}

fn audit_path_filename(file_path: &str) -> String {
    Path::new(file_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.to_string())
}

fn normalize_relative_uri(base_file: &Path, uri: &str) -> String {
    base_file
        .parent()
        .unwrap_or(base_file)
        .join(uri)
        .components()
        .collect::<std::path::PathBuf>()
        .to_string_lossy()
        .to_string()
}

fn package_name(repo_path: &Path) -> Option<String> {
    let pubspec_path = repo_path.join("pubspec.yaml");
    let content = std::fs::read_to_string(pubspec_path).ok()?;
    content.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("name:")
            .map(|name| name.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|name| !name.is_empty())
    })
}

fn normalize_import_uri(
    repo_path: &Path,
    base_file: &Path,
    uri: &str,
    package_name: Option<&str>,
) -> Option<String> {
    if uri.starts_with("dart:") {
        return None;
    }
    if let Some(rest) = uri.strip_prefix("package:") {
        let (package, relative) = rest.split_once('/')?;
        if Some(package) != package_name {
            return None;
        }
        return Some(
            repo_path
                .join("lib")
                .join(relative)
                .to_string_lossy()
                .to_string(),
        );
    }
    Some(
        repo_path
            .join(normalize_relative_uri(base_file, uri))
            .to_string_lossy()
            .to_string(),
    )
}

fn build_audit_analysis(
    repo_path: &Path,
    manifest: &std::collections::HashMap<String, String>,
) -> AuditFileAnalysis {
    let mut analysis = AuditFileAnalysis::default();
    let mut seen_decls = HashSet::new();
    let repo_package_name = package_name(repo_path);

    for path_str in manifest.keys() {
        let file_path = Path::new(path_str);
        let content = match std::fs::read_to_string(file_path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let display_file = relative_display_path(repo_path, file_path);

        let chunker = compas::chunker::dart::DartChunker;
        let chunks = match compas::chunker::Chunker::chunk(&chunker, path_str, &content) {
            Ok(chunks) => chunks,
            Err(_) => continue,
        };

        for chunk in chunks {
            let symbol = strip_part_suffix(&chunk.symbol);
            let is_filename_placeholder =
                chunk.kind == "declaration" && symbol == audit_path_filename(path_str);
            if chunk.kind == "file" || symbol.contains("unknown") || is_filename_placeholder {
                continue;
            }
            if seen_decls.insert(format!("{}:{}:{}", path_str, symbol, chunk.kind)) {
                analysis.declarations.push(AuditDeclaration {
                    file: path_str.clone(),
                    display_file: display_file.clone(),
                    symbol,
                    kind: chunk.kind,
                });
            }
        }

        if let Ok(semantic) = extract_semantic_references(&content) {
            analysis
                .references
                .extend(
                    semantic
                        .references
                        .into_iter()
                        .map(|(_, callee)| AuditReference {
                            caller_file: path_str.clone(),
                            callee,
                        }),
                );
            analysis.key_types.extend(semantic.key_types);
            analysis
                .file_edges
                .extend(semantic.import_uris.into_iter().filter_map(|uri| {
                    normalize_import_uri(repo_path, file_path, &uri, repo_package_name.as_deref())
                        .map(|target| (path_str.clone(), target))
                }));
        }
    }

    analysis.key_types.sort();
    analysis.key_types.dedup();
    analysis.file_edges.sort();
    analysis.file_edges.dedup();
    analysis
}

#[allow(dead_code)]
fn reachable_files(
    manifest: &std::collections::HashMap<String, String>,
    analysis: &AuditFileAnalysis,
) -> HashSet<String> {
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    for path in manifest.keys() {
        adjacency.entry(path.clone()).or_default();
    }
    for reference in &analysis.references {
        if let Some(target) = resolve_reference_file(&reference.callee, &analysis.declarations) {
            adjacency
                .entry(reference.caller_file.clone())
                .or_default()
                .push(target);
        }
    }
    for (source, target) in &analysis.file_edges {
        adjacency
            .entry(source.clone())
            .or_default()
            .push(target.clone());
        adjacency.entry(target.clone()).or_default();
    }

    let mut inbound_counts: HashMap<String, usize> = HashMap::new();
    for (source, targets) in &adjacency {
        inbound_counts.entry(source.clone()).or_insert(0);
        for target in targets {
            *inbound_counts.entry(target.clone()).or_insert(0) += 1;
        }
    }

    let mut queue = VecDeque::new();
    let mut reachable = HashSet::new();
    for path in manifest.keys() {
        if inbound_counts.get(path).copied().unwrap_or(0) == 0 {
            queue.push_back(path.clone());
        }
    }

    while let Some(path) = queue.pop_front() {
        if !reachable.insert(path.clone()) {
            continue;
        }
        if let Some(targets) = adjacency.get(&path) {
            for target in targets {
                if manifest.contains_key(target) {
                    queue.push_back(target.clone());
                }
            }
        }
    }

    reachable
}

#[allow(dead_code)]
fn resolve_reference_file(callee: &str, declarations: &[AuditDeclaration]) -> Option<String> {
    declarations
        .iter()
        .find(|decl| decl.symbol == callee || decl.symbol.rsplit('.').next() == Some(callee))
        .map(|decl| decl.file.clone())
}

fn classify_dead_code_candidates(analysis: &AuditFileAnalysis) -> Vec<DeadCodeCandidate> {
    // Build a global bare-name set of every callee referenced anywhere.
    // If a name appears in any reference, every declaration with that bare name
    // is considered live. This is intentionally permissive: false negatives
    // (missing a real dead method that shares a name with a live one) are far
    // less costly than the false positives we get from precise matching when
    // Dart's dynamic dispatch hides receiver types.
    let mut referenced_names: HashSet<String> = HashSet::new();
    for reference in &analysis.references {
        // Record both the full callee and its last segment.
        referenced_names.insert(reference.callee.clone());
        if let Some(bare) = reference.callee.rsplit('.').next() {
            referenced_names.insert(bare.to_string());
        }
    }

    let key_types: HashSet<String> = analysis.key_types.iter().cloned().collect();
    let mut candidates = vec![];

    for declaration in &analysis.declarations {
        // Skip declaration-level wrappers and types — we only care about callable members.
        if matches!(
            declaration.kind.as_str(),
            "class" | "mixin" | "extension" | "enum" | "file" | "typedef"
        ) {
            continue;
        }

        // Skip filename-placeholder declarations from the chunker.
        if declaration.kind == "declaration"
            && declaration.symbol == audit_path_filename(&declaration.file)
        {
            continue;
        }
        if declaration.symbol.contains("unknown") {
            continue;
        }

        let member_name = declaration
            .symbol
            .rsplit('.')
            .next()
            .unwrap_or(&declaration.symbol);
        let enclosing_type = declaration
            .symbol
            .split('.')
            .next()
            .unwrap_or(&declaration.symbol);

        // Hard suppressions: framework dispatch.
        if is_framework_dispatched(member_name) {
            continue;
        }

        // operator == / hashCode are framework-dispatched anyway, but as a belt-and-braces
        // also suppress them whenever the enclosing class is used as a Map/Set key.
        if (member_name == "operator ==" || member_name == "hashCode")
            && key_types.contains(enclosing_type)
        {
            continue;
        }

        // Skip private members entirely. They are by definition internal to the file
        // and the audit produces too much noise on `_buildRow`-style helpers.
        if member_name.starts_with('_') {
            continue;
        }

        // Skip getters and setters. They are read via property syntax that we cannot
        // reliably distinguish from random identifier reads, so the signal is weak.
        if declaration.kind == "getter_setter" {
            continue;
        }

        // Skip constructors named like codegen entrypoints (fromJson, fromMap, etc.).
        // These are matched above in is_framework_dispatched, but the constructor symbol
        // is `Class.fromJson` so check the member_name explicitly too.
        if matches!(declaration.kind.as_str(), "constructor")
            && matches!(
                member_name,
                "fromJson" | "fromMap" | "fromSnapshot" | "fromDoc" | "fromDocument" | "fromString"
            )
        {
            continue;
        }

        // Skip the unnamed default constructor (symbol like `Class.Class`) when the class
        // itself is referenced anywhere by name. A class being instantiated (`Foo()`)
        // shows up in references as the bare name `Foo`.
        if declaration.kind == "constructor"
            && member_name == enclosing_type
            && referenced_names.contains(enclosing_type)
        {
            continue;
        }

        // Skip Flutter callback-style members (onPressed, onTap, etc.) — these are passed
        // as widget parameters and dispatched by the framework.
        if FLUTTER_CALLBACK_SUFFIXES.contains(&member_name) {
            continue;
        }

        // Final liveness check: is the bare name referenced anywhere?
        if referenced_names.contains(member_name) {
            continue;
        }

        candidates.push(DeadCodeCandidate {
            file: declaration.display_file.clone(),
            symbol: declaration.symbol.clone(),
            kind: declaration.kind.clone(),
        });
    }

    candidates.sort_by(|a, b| a.file.cmp(&b.file).then(a.symbol.cmp(&b.symbol)));
    candidates
}

async fn index_repo(config: AppConfig) -> anyhow::Result<()> {
    let embedder = build_embedder(&config.embedder)?;
    let repo_path = std::fs::canonicalize(&config.repo.path)?;
    let store = Arc::new(EdgeStore::new(
        repo_path.join(&config.store.path),
        &config.store.vector_name,
    ));
    store.init(embedder.dimensions()).await?;

    let registry = ChunkerRegistry::new();

    // Load .compasignore patterns
    let compas_ignore = CompasIgnore::load(&repo_path);

    // Load existing graph
    let graph = Graph::new();
    let graph_path = repo_path.join(".compas").join("graph.json");
    if let Err(e) = graph.load(&graph_path) {
        debug!("no existing graph to load: {}", e);
    }

    // ── First pass: discover files and compute hashes ───────────────────────
    let mut files_with_hashes: Vec<(std::path::PathBuf, String)> = Vec::new();
    for entry in walkdir::WalkDir::new(&repo_path) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("walkdir error: {}", e);
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(&repo_path).unwrap_or(path);
        if !should_include(relative, &config.repo.include, &config.repo.exclude) {
            continue;
        }
        if compas_ignore.is_ignored(relative) {
            continue;
        }
        if language_for_path(path).is_none() {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                warn!("skip read error for {}: {}", path.display(), e);
                continue;
            }
        };
        let hash = hash_bytes(content.as_bytes());
        files_with_hashes.push((path.to_path_buf(), hash));
    }

    // ── Load manifest and detect deleted / newly-ignored files ──────────────
    let manifest_path = repo_path.join(".compas").join("manifest.json");
    let old_manifest: std::collections::HashMap<String, String> =
        std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

    let mut new_manifest = old_manifest.clone();
    let current_paths: std::collections::HashSet<String> = files_with_hashes
        .iter()
        .map(|(p, _)| p.to_string_lossy().to_string())
        .collect();

    let mut deleted_files = Vec::new();
    for path in old_manifest.keys() {
        if !current_paths.contains(path) {
            deleted_files.push(path.clone());
        }
    }

    // Clean up deleted files from store and graph
    for path in &deleted_files {
        if let Err(e) = store.delete_by_file(path).await {
            warn!("failed to delete chunks for removed file {}: {}", path, e);
        }
        graph.remove_by_file(path);
        new_manifest.remove(path);
    }

    let changed_count = files_with_hashes
        .iter()
        .filter(|(p, h)| old_manifest.get(&p.to_string_lossy().to_string()) != Some(h))
        .count();

    let use_tui = std::env::var("RUST_LOG").is_err() && std::io::stderr().is_terminal();

    if use_tui {
        println!(
            "Indexing {}  ({} files, {} changed, {} deleted)",
            repo_path.display(),
            files_with_hashes.len(),
            changed_count,
            deleted_files.len()
        );
    } else {
        info!(
            "indexing {:?} ({} files, {} changed, {} deleted)",
            repo_path,
            files_with_hashes.len(),
            changed_count,
            deleted_files.len()
        );
    }

    let pb = if use_tui {
        let bar = ProgressBar::new(files_with_hashes.len() as u64);
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        bar.set_message("starting...");
        Some(bar)
    } else {
        None
    };

    let start = Instant::now();
    let mut processed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut total_chunks = 0usize;
    let mut chunks_without_docs: Vec<(String, String, String, usize)> = vec![];
    let mut processed_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (path, hash) in &files_with_hashes {
        let path_str = path.to_string_lossy().to_string();
        let relative = path.strip_prefix(&repo_path).unwrap_or(path);
        let rel_str = relative.to_string_lossy().to_string();

        // Skip unchanged files
        if old_manifest.get(&path_str) == Some(hash) {
            if let Some(ref bar) = pb {
                bar.set_message(format!("skipping {}", rel_str));
                bar.inc(1);
            } else {
                debug!("skipping unchanged file: {}", rel_str);
            }
            skipped += 1;
            continue;
        }

        if let Some(ref bar) = pb {
            bar.set_message(rel_str.clone());
        } else {
            info!("→ {}", rel_str);
        }
        let relative = path.strip_prefix(&repo_path).unwrap_or(path);
        let rel_str = relative.to_string_lossy().to_string();

        if let Some(ref bar) = pb {
            bar.set_message(rel_str.clone());
        } else {
            info!("→ {}", rel_str);
        }

        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                if let Some(ref bar) = pb {
                    bar.println(format!("⚠  skip read error in {}: {}", rel_str, e));
                } else {
                    warn!("  skip read error: {}", e);
                }
                failed += 1;
                if let Some(ref bar) = pb {
                    bar.inc(1);
                }
                continue;
            }
        };

        let line_count = content.lines().count();
        if line_count > 1000 {
            if let Some(ref bar) = pb {
                bar.println(format!("⚠  large file: {} ({} lines)", rel_str, line_count));
            } else {
                warn!("  ⚠️  large file: {} lines", line_count);
            }
        }

        let language = match language_for_path(path) {
            Some(language) => language,
            None => continue,
        };

        let chunker = match registry.get(language) {
            Some(chunker) => chunker,
            None => {
                if let Some(ref bar) = pb {
                    bar.println(format!("warning: no {} chunker available", language));
                    bar.inc(1);
                } else {
                    warn!("no {} chunker available", language);
                }
                failed += 1;
                continue;
            }
        };

        let chunks = match chunker.chunk(path.to_str().unwrap(), &content) {
            Ok(c) => c,
            Err(e) => {
                if let Some(ref bar) = pb {
                    bar.println(format!("⚠  skip chunk error in {}: {}", rel_str, e));
                } else {
                    warn!("  skip chunk error: {}", e);
                }
                failed += 1;
                if let Some(ref bar) = pb {
                    bar.inc(1);
                }
                continue;
            }
        };

        if chunks.is_empty() {
            if let Some(ref bar) = pb {
                bar.inc(1);
            } else {
                info!("  0 chunks, skipping");
            }
            continue;
        }

        for chunk in &chunks {
            let chunk_lines = chunk.line_end.saturating_sub(chunk.line_start);
            if chunk_lines > 200 {
                if let Some(ref bar) = pb {
                    bar.println(format!(
                        "⚠  long {}: {} ({} lines)",
                        chunk.kind, chunk.symbol, chunk_lines
                    ));
                } else {
                    warn!(
                        "  ⚠️  long {}: {} ({} lines)",
                        chunk.kind, chunk.symbol, chunk_lines
                    );
                }
            }
            if !chunk.content.starts_with("///")
                && !chunk.content.starts_with("//!")
                && (chunk.kind == "method"
                    || chunk.kind == "function"
                    || chunk.kind == "constructor")
                && !is_flutter_boilerplate(&chunk.symbol)
            {
                chunks_without_docs.push((
                    rel_str.clone(),
                    chunk.symbol.clone(),
                    chunk.kind.clone(),
                    chunk.line_start,
                ));
            }
        }

        if let Err(e) = store.delete_by_file(path.to_str().unwrap()).await {
            if let Some(ref bar) = pb {
                bar.println(format!(
                    "⚠  failed to delete old chunks in {}: {}",
                    rel_str, e
                ));
            } else {
                warn!("  failed to delete old chunks: {}", e);
            }
        }

        // Embed in smaller batches to limit peak memory during inference.
        let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
        let mut embed_failed = false;
        for chunk_batch in chunks.chunks(EMBED_BATCH_SIZE) {
            let texts: Vec<String> = chunk_batch.iter().map(|c| c.content.clone()).collect();
            match embedder.embed_batch(&texts, EmbedMode::Document).await {
                Ok(batch_embeddings) => embeddings.extend(batch_embeddings),
                Err(e) => {
                    if let Some(ref bar) = pb {
                        bar.println(format!("⚠  skip embed error in {}: {}", rel_str, e));
                    } else {
                        warn!("  skip embed error: {}", e);
                    }
                    failed += 1;
                    embed_failed = true;
                    break;
                }
            }
        }
        if embed_failed {
            if let Some(ref bar) = pb {
                bar.inc(1);
            }
            continue;
        }

        if let Err(e) = store.upsert(&chunks, &embeddings).await {
            if let Some(ref bar) = pb {
                bar.println(format!("⚠  skip upsert error in {}: {}", rel_str, e));
            } else {
                warn!("  skip upsert error: {}", e);
            }
            failed += 1;
            if let Some(ref bar) = pb {
                bar.inc(1);
            }
            continue;
        }

        // Remove old symbols for this file before adding new ones
        graph.remove_by_file(path_str.as_str());

        for chunk in &chunks {
            let base_symbol = strip_part_suffix(&chunk.symbol);
            graph.add_symbol(&base_symbol, &chunk.file_path, &chunk.kind);
        }

        if let Ok(calls) = extract_calls_for_language(language, &content) {
            for (caller, callee) in &calls {
                graph.add_symbol(caller, path.to_str().unwrap(), "method");
                graph.add_call(caller, path.to_str().unwrap(), callee);
            }
        }

        processed += 1;
        total_chunks += chunks.len();
        new_manifest.insert(path_str.clone(), hash.clone());
        processed_paths.insert(path_str);

        if let Some(ref bar) = pb {
            bar.inc(1);
        } else {
            info!("  ✓ done ({} chunks)", chunks.len());
        }
    }

    // Scan unprocessed (unchanged) files for missing docs so audit is complete
    for path_str in new_manifest.keys() {
        if processed_paths.contains(path_str) {
            continue;
        }
        let path = std::path::Path::new(path_str);
        let relative = path.strip_prefix(&repo_path).unwrap_or(path);
        let rel_str = relative.to_string_lossy().to_string();

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let language = match language_for_path(path) {
            Some(language) => language,
            None => continue,
        };

        let chunker = match registry.get(language) {
            Some(chunker) => chunker,
            None => continue,
        };

        let chunks = match chunker.chunk(path_str, &content) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for chunk in &chunks {
            if !chunk.content.starts_with("///")
                && !chunk.content.starts_with("//!")
                && (chunk.kind == "method"
                    || chunk.kind == "function"
                    || chunk.kind == "constructor")
                && !is_flutter_boilerplate(&chunk.symbol)
            {
                chunks_without_docs.push((
                    rel_str.clone(),
                    chunk.symbol.clone(),
                    chunk.kind.clone(),
                    chunk.line_start,
                ));
            }
        }
    }

    if let Some(bar) = pb {
        bar.finish_and_clear();
    }

    let elapsed = start.elapsed();

    graph.create_phantom_nodes();
    if !use_tui {
        info!("created phantom nodes for external symbols");
    }

    tokio::fs::create_dir_all(graph_path.parent().unwrap())
        .await
        .ok();
    graph.save(&graph_path)?;

    let dart_manifest: std::collections::HashMap<String, String> = new_manifest
        .iter()
        .filter(|(path, _)| language_for_path(Path::new(path)) == Some("dart"))
        .map(|(path, hash)| (path.clone(), hash.clone()))
        .collect();

    let audit_analysis = build_audit_analysis(&repo_path, &dart_manifest);
    let dead_code_candidates = classify_dead_code_candidates(&audit_analysis);

    generate_audit(
        &graph,
        &dead_code_candidates,
        &chunks_without_docs,
        processed,
        total_chunks,
        failed,
    );

    // Save manifest
    let manifest_json = serde_json::to_string_pretty(&new_manifest)?;
    tokio::fs::write(&manifest_path, manifest_json).await.ok();

    // ── Pretty summary ─────────────────────────────────────────────────────
    let secs = elapsed.as_secs();
    let mins = secs / 60;
    let rem_secs = secs % 60;
    let time_str = if mins > 0 {
        format!("{}m {:02}s", mins, rem_secs)
    } else {
        format!("{}s", rem_secs)
    };

    let repo_name = repo_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());

    let dead_code_count = dead_code_candidates.len();

    println!();
    println!("     @@@@@@@   @@@@@@   @@@@@@@@@@   @@@@@@@    @@@@@@    @@@@@@   ");
    println!("     @@@@@@@@  @@@@@@@@  @@@@@@@@@@@  @@@@@@@@  @@@@@@@@  @@@@@@@   ");
    println!("     !@@       @@!  @@@  @@! @@! @@!  @@!  @@@  @@!  @@@  !@@       ");
    println!("     !@!       !@!  @!@  !@! !@! !@!  !@!  @!@  !@!  @!@  !@!       ");
    println!("     !@!       @!@  !@!  @!! !!@ @!@  @!@@!@!   @!@!@!@!  !!@@!!    ");
    println!("     !!!       !@!  !!!  !@!   ! !@!  !!@!!!    !!!@!!!!   !!@!!!   ");
    println!("     :!!       !!:  !!!  !!:     !!:  !!:       !!:  !!!       !:!  ");
    println!("     :!:       :!:  !:!  :!:     :!:  :!:       :!:  !:!      !:!   ");
    println!("      ::: :::  ::::: ::  :::     ::    ::       ::   :::  :::: ::   ");
    println!("      :: :: :   : :  :    :      :     :         :   : :  :: : :    ");
    println!();
    println!("    {} indexed in {}", repo_name, time_str);
    println!();
    println!("    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!(
        "     {} changed  ·  {} skipped  ·  {} deleted",
        processed,
        skipped,
        deleted_files.len()
    );
    println!("     {} chunks  ·  {} failed", total_chunks, failed);
    println!("    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!(
        "    ⚠️  {} symbols missing doc comments",
        chunks_without_docs.len()
    );
    println!("    🪦  {} dead code candidates", dead_code_count);
    println!();
    println!("    📊 Graph  → {}", graph_path.display());
    println!("    📋 Audit  → .compas/audit.md");
    println!();

    if !use_tui {
        info!("indexing complete");
    }

    println!("Optimizing edge shard...");
    let optimized = store.optimize()?;
    if optimized {
        println!("✓ Edge shard optimized");
    } else {
        println!("✓ Edge shard already optimized");
    }

    Ok(())
}

async fn optimize_repo(config: AppConfig) -> anyhow::Result<()> {
    let repo_path = std::fs::canonicalize(&config.repo.path)?;
    let optimized = optimize_edge_shard(&config)?;

    if optimized {
        println!("Optimized edge shard for {}", repo_path.display());
    } else {
        println!(
            "Edge shard for {} did not require optimization",
            repo_path.display()
        );
    }

    Ok(())
}

fn optimize_edge_shard(config: &AppConfig) -> anyhow::Result<bool> {
    let repo_path = std::fs::canonicalize(&config.repo.path)?;
    let store = EdgeStore::new(
        repo_path.join(&config.store.path),
        &config.store.vector_name,
    );
    store.optimize()
}

async fn run_mcp() -> anyhow::Result<()> {
    let registry = compas::config::RepoRegistry::load();

    if registry.repos.is_empty() {
        println!("No repos registered. Run 'compas init' in a repository first.");
        return Ok(());
    }

    let mut repos = HashMap::new();
    let mut embedder_cache: HashMap<
        compas::config::EmbedderConfig,
        Arc<dyn compas::embedder::Embedder>,
    > = HashMap::new();
    for (name, path) in registry.list() {
        let config_path = std::path::Path::new(path).join("compas.yaml");
        if !config_path.exists() {
            warn!(
                "compas.yaml not found for repo '{}' at {}, skipping",
                name, path
            );
            continue;
        }

        let config = match AppConfig::load(config_path.to_str().unwrap()) {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to load config for repo '{}': {}", name, e);
                continue;
            }
        };

        let repo_path = std::fs::canonicalize(path)?;
        let embedder = match embedder_cache.get(&config.embedder) {
            Some(e) => Arc::clone(e),
            None => {
                let e = build_embedder(&config.embedder)?;
                embedder_cache.insert(config.embedder.clone(), Arc::clone(&e));
                e
            }
        };
        let edge_store = Arc::new(EdgeStore::new(
            repo_path.join(&config.store.path),
            &config.store.vector_name,
        ));
        // MCP startup only loads repo descriptors. The shard is opened on demand
        // when a tool call actually targets this repo.
        let store: Arc<dyn compas::store::Store> = edge_store;
        let graph = Arc::new(Graph::new());
        let graph_path = repo_path.join(".compas").join("graph.json");
        if let Err(e) = graph.load(&graph_path) {
            warn!("no existing graph loaded for repo '{}': {}", name, e);
        }

        repos.insert(
            name.clone(),
            compas::mcp::state::RepoState {
                store,
                graph,
                embedder,
            },
        );
        info!("loaded repo '{}' for MCP", name);
    }

    if repos.is_empty() {
        println!("No valid repos could be loaded.");
        return Ok(());
    }

    let default_repo = if repos.len() == 1 {
        repos.keys().next().cloned()
    } else {
        None
    };

    let state = Arc::new(McpAppState {
        repos,
        default_repo,
    });

    mcp::server::run_stdio_server(state).await
}

async fn serve() -> anyhow::Result<()> {
    let registry = compas::config::RepoRegistry::load();

    if registry.repos.is_empty() {
        println!("No repos registered. Run 'compas init' in a repository first.");
        return Ok(());
    }

    let mut repos = HashMap::new();
    let mut embedder_cache: HashMap<
        compas::config::EmbedderConfig,
        Arc<dyn compas::embedder::Embedder>,
    > = HashMap::new();
    for (name, path) in registry.list() {
        let config_path = std::path::Path::new(path).join("compas.yaml");
        if !config_path.exists() {
            warn!(
                "compas.yaml not found for repo '{}' at {}, skipping",
                name, path
            );
            continue;
        }

        let config = match AppConfig::load(config_path.to_str().unwrap()) {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to load config for repo '{}': {}", name, e);
                continue;
            }
        };

        let repo_path = std::fs::canonicalize(path)?;
        let embedder = match embedder_cache.get(&config.embedder) {
            Some(e) => Arc::clone(e),
            None => {
                let e = build_embedder(&config.embedder)?;
                embedder_cache.insert(config.embedder.clone(), Arc::clone(&e));
                e
            }
        };
        let edge_store = Arc::new(EdgeStore::new(
            repo_path.join(&config.store.path),
            &config.store.vector_name,
        ));
        // Daemon startup only loads repo descriptors. The shard is opened on
        // demand when a request actually targets this repo.
        let store: Arc<dyn compas::store::Store> = edge_store;
        let graph = Arc::new(Graph::new());
        let graph_path = repo_path.join(".compas").join("graph.json");
        if let Err(e) = graph.load(&graph_path) {
            warn!("no existing graph loaded for repo '{}': {}", name, e);
        }

        repos.insert(
            name.clone(),
            RepoState {
                config,
                store,
                graph,
                embedder,
            },
        );
        info!("loaded repo '{}' from {}", name, path);
    }

    if repos.is_empty() {
        println!("No valid repos could be loaded.");
        return Ok(());
    }

    let default_repo = if repos.len() == 1 {
        repos.keys().next().cloned()
    } else {
        None
    };

    let state = Arc::new(AppState {
        repos,
        default_repo,
    });

    // Use a fixed port for the global daemon (ignore per-repo config)
    let host = std::env::var("COMPAS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("COMPAS_PORT").unwrap_or_else(|_| "3001".into());

    // Auto-restart: if port is in use, kill the existing compas process
    let addr = format!("{}:{}", host, port);
    if let Ok(output) = std::process::Command::new("lsof")
        .args(["-i", &format!(":{}", port), "-t"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for pid_str in stdout.lines() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                let my_pid = std::process::id();
                if pid != my_pid {
                    println!("Port {} is in use by PID {}. Restarting...", port, pid);
                    let _ = std::process::Command::new("kill").arg(pid_str).output();
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        }
    }

    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(
        "compas daemon listening on {} (serving {} repo(s))",
        listener.local_addr()?,
        state.repos.len()
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn watch(config: AppConfig) -> anyhow::Result<()> {
    let repo_path = std::fs::canonicalize(&config.repo.path)?;
    let embedder = build_embedder(&config.embedder)?;
    let edge_store = Arc::new(EdgeStore::new(
        repo_path.join(&config.store.path),
        &config.store.vector_name,
    ));
    edge_store.init(embedder.dimensions()).await?;
    let store: Arc<dyn compas::store::Store> = edge_store;
    let embedder: Arc<dyn compas::embedder::Embedder> = embedder;
    let handler = ReindexHandler {
        config,
        store,
        embedder,
        registry: ChunkerRegistry::new(),
    };
    FileWatcher::watch(&repo_path, handler).await
}

struct ReindexHandler {
    config: AppConfig,
    store: Arc<dyn compas::store::Store>,
    embedder: Arc<dyn compas::embedder::Embedder>,
    registry: ChunkerRegistry,
}

#[async_trait]
impl Handler for ReindexHandler {
    async fn on_change(&self, file_path: &str) {
        let path = std::path::Path::new(file_path);

        if !path.is_file() {
            return;
        }

        let repo_path = match std::fs::canonicalize(&self.config.repo.path) {
            Ok(p) => p,
            Err(_) => return,
        };
        let relative = path.strip_prefix(&repo_path).unwrap_or(path);
        if !should_include(
            relative,
            &self.config.repo.include,
            &self.config.repo.exclude,
        ) {
            return;
        }
        let language = match language_for_path(path) {
            Some(language) => language,
            None => return,
        };

        info!("reindexing {}", file_path);

        let content = match tokio::fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to read {}: {}", file_path, e);
                return;
            }
        };

        let chunker = match self.registry.get(language) {
            Some(c) => c,
            None => {
                warn!("no {} chunker available", language);
                return;
            }
        };

        let chunks = match chunker.chunk(file_path, &content) {
            Ok(c) => c,
            Err(e) => {
                warn!("chunk failed for {}: {}", file_path, e);
                return;
            }
        };

        if let Err(e) = self.store.delete_by_file(file_path).await {
            warn!("failed to delete old chunks for {}: {}", file_path, e);
        }

        if chunks.is_empty() {
            info!("no chunks found in {}", file_path);
            return;
        }

        let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
        for chunk_batch in chunks.chunks(EMBED_BATCH_SIZE) {
            let texts: Vec<String> = chunk_batch.iter().map(|c| c.content.clone()).collect();
            match self.embedder.embed_batch(&texts, EmbedMode::Document).await {
                Ok(batch_embeddings) => embeddings.extend(batch_embeddings),
                Err(e) => {
                    warn!("embed failed for {}: {}", file_path, e);
                    return;
                }
            }
        }

        if let Err(e) = self.store.upsert(&chunks, &embeddings).await {
            warn!("upsert failed for {}: {}", file_path, e);
            return;
        }

        // Update graph
        let repo_path = match std::fs::canonicalize(&self.config.repo.path) {
            Ok(p) => p,
            Err(e) => {
                warn!("failed to canonicalize repo path: {}", e);
                return;
            }
        };
        let graph_path = repo_path.join(".compas").join("graph.json");
        let graph = Graph::new();
        if let Err(e) = graph.load(&graph_path) {
            debug!("no existing graph to load: {}", e);
        }

        graph.remove_by_file(file_path);
        for chunk in &chunks {
            let base_symbol = strip_part_suffix(&chunk.symbol);
            graph.add_symbol(&base_symbol, &chunk.file_path, &chunk.kind);
        }

        // Extract call relationships from the AST
        if let Ok(calls) = extract_calls_for_language(language, &content) {
            for (caller, callee) in &calls {
                graph.add_symbol(caller, file_path, "method");
                graph.add_call(caller, file_path, callee);
            }
        }

        if let Err(e) = graph.save(&graph_path) {
            warn!("failed to save graph: {}", e);
        }

        info!("reindexed {} ({} chunks)", file_path, chunks.len());
    }

    async fn on_delete(&self, file_path: &str) {
        info!("deleting {}", file_path);

        if let Err(e) = self.store.delete_by_file(file_path).await {
            warn!("failed to delete chunks for {}: {}", file_path, e);
        }

        let repo_path = match std::fs::canonicalize(&self.config.repo.path) {
            Ok(p) => p,
            Err(e) => {
                warn!("failed to canonicalize repo path: {}", e);
                return;
            }
        };
        let graph_path = repo_path.join(".compas").join("graph.json");
        let graph = Graph::new();
        if let Err(e) = graph.load(&graph_path) {
            debug!("no existing graph to load: {}", e);
        }

        graph.remove_by_file(file_path);

        if let Err(e) = graph.save(&graph_path) {
            warn!("failed to save graph: {}", e);
        }

        info!("deleted {}", file_path);
    }
}

fn generate_audit(
    _graph: &Graph,
    dead_code: &[DeadCodeCandidate],
    missing_docs: &[(String, String, String, usize)],
    files: usize,
    chunks: usize,
    failed: usize,
) {
    // Deduplicate missing docs by base symbol (strip _pN suffix from split chunks).
    // A large function split into _p1, _p2, _p3 should only appear once in the audit.
    let mut seen_symbols: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut deduped_docs: Vec<(String, String, String, usize)> = vec![];
    for (file, symbol, kind, line) in missing_docs.iter() {
        let base = strip_part_suffix(symbol);
        if seen_symbols.insert(format!("{}:{}", file, base)) {
            deduped_docs.push((file.clone(), base, kind.clone(), *line));
        }
    }
    let mut missing_docs = deduped_docs;
    missing_docs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut md = String::new();
    md.push_str("# Compas Codebase Audit\n\n");
    md.push_str(&format!(
        "**Files indexed:** {} | **Chunks:** {} | **Failed:** {}\n\n",
        files, chunks, failed
    ));
    md.push_str(&format!(
        "**Symbols missing doc comments:** {} | **Dead code candidates:** {}\n\n",
        missing_docs.len(),
        dead_code.len()
    ));

    // Doc comments section — formatted as a self-contained AI agent prompt
    md.push_str("---\n\n");
    md.push_str("# PROMPT FOR YOUR AGENT: Add Dart Doc Comments\n\n");
    md.push_str(
        "Copy this entire section below to your AI agent. Do not edit the prompt itself.\n\n",
    );
    md.push_str("## Role\n\n");
    md.push_str("You are a **Dart code documentation specialist**. Your sole task is to add `///` doc comments to the functions listed below.\n\n");
    md.push_str("## Why This Matters\n\n");
    md.push_str("These functions are **invisible to semantic search** because they lack `///` doc comments.\n");
    md.push_str("This codebase is indexed by an AI search engine that embeds doc comments alongside code.\n");
    md.push_str("When a developer searches \"where is the login authentication logic?\", the engine matches\n");
    md.push_str("their query against these doc comments. **No comment = no match = the function might as well not exist.**\n\n");
    md.push_str("## How to Write Good Doc Comments\n\n");
    md.push_str(
        "Write **1-3 lines** maximum. Include keywords a developer would actually search for.\n\n",
    );
    md.push_str("**Examples:**\n");
    md.push_str("```dart\n");
    md.push_str(
        "/// Authenticates the user against the OAuth provider and stores the JWT token.\n",
    );
    md.push_str("/// Called during app startup and after token refresh.\n");
    md.push_str("Future<void> authenticateUser() async { ... }\n");
    md.push_str("```\n\n");
    md.push_str("```dart\n");
    md.push_str("/// Converts a Supabase JSON map into a [User] model.\n");
    md.push_str("/// Handles null safety and default values for optional fields.\n");
    md.push_str("factory User.fromSupabase(Map<String, dynamic> json) { ... }\n");
    md.push_str("```\n\n");
    md.push_str("**Rules for wording:**\n");
    md.push_str("- Use the **domain vocabulary** from the codebase (e.g., \"auth\", \"cache\", \"payment\")\n");
    md.push_str("- Mention **what the function returns** for getters and factory constructors\n");
    md.push_str("- Mention **when/where it is called** if it's a lifecycle or callback method\n");
    md.push_str(
        "- Mention **side effects** (e.g., \"updates local cache\", \"writes to Supabase\")\n",
    );
    md.push_str("- **Do NOT** describe implementation details (\"loops over list\", \"uses a forEach\")\n\n");
    md.push_str("## CRITICAL CONSTRAINTS — DO NOT VIOLATE\n\n");
    md.push_str("1. **DO NOT modify any code.** Only add `///` lines immediately before the function declaration.\n");
    md.push_str("2. **DO NOT change signatures, logic, imports, or formatting.**\n");
    md.push_str("3. **DO NOT use `//` comments.** Only `///` doc comments are indexed.\n");
    md.push_str(
        "4. **DO NOT add comments inside function bodies.** Only top-of-function doc comments.\n",
    );
    md.push_str("5. **DO NOT delete, move, or rename any functions.**\n");
    md.push_str("6. **Keep each comment under 200 characters.** Prefer 1-2 lines.\n");
    md.push_str("7. **DO NOT add comments to Flutter `build()` methods** unless they contain complex business logic.\n\n");
    md.push_str("## IMPORTANT — TAKE THIS SERIOUSLY\n\n");
    md.push_str("This is a **production codebase**. Vague, generic, or placeholder comments (e.g., \"This method does something\")\n");
    md.push_str("will get you fired.\n\n");
    md.push_str("A very important search tool will depend on these comments, you will prioritize quality over speed. No one will be expecting for you finish fast, take your time.\n");
    md.push_str("Write comments that **you** would find useful 6 months from now when you're debugging at 2am.\n\n");
    md.push_str("If you are unsure what a function does, infer from:\n");
    md.push_str("- Its name and parameters\n");
    md.push_str("- The class it belongs to\n");
    md.push_str("- Other functions called inside its body\n");
    md.push_str("- The file it lives in (e.g., `book_service.dart` implies database/network operations)\n\n");
    md.push_str("---\n\n");
    md.push_str("## Functions Requiring Doc Comments\n\n");
    md.push_str("Work through this list file by file. For each entry, locate the function at the given line and add a `///` doc comment.\n\n");
    if missing_docs.is_empty() {
        md.push_str("*All methods and functions have doc comments!* 🎉\n\n");
    } else {
        md.push_str("| File | Symbol | Kind | Approx. Line |\n");
        md.push_str("|------|--------|------|-------------|\n");
        for (file, symbol, kind, line) in &missing_docs {
            md.push_str(&format!(
                "| {} | {} | {} | ~{} |\n",
                file, symbol, kind, line
            ));
        }
        md.push('\n');
    }
    md.push_str("---\n\n");

    // Dead code section
    md.push_str("## Potentially Dead Code\n\n");
    md.push_str(
        "These declarations have no inbound semantic references after framework dispatch,\n",
    );
    md.push_str(
        "Dart protocol, getter, and codegen suppressions. Each entry is worth a human review;\n",
    );
    md.push_str("private helpers, lifecycle methods, and serialization hooks are excluded.\n\n");
    if dead_code.is_empty() {
        md.push_str("*No dead code candidates found!* 🎉\n");
    } else {
        md.push_str("| File | Symbol | Kind |\n");
        md.push_str("|------|--------|------|\n");
        for candidate in dead_code {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                candidate.file, candidate.symbol, candidate.kind
            ));
        }
    }

    // Write to .compas/audit.md
    if let Ok(repo_path) = std::fs::canonicalize(std::env::current_dir().unwrap_or_default()) {
        let audit_path = repo_path.join(".compas").join("audit.md");
        if let Ok(dir) = audit_path
            .parent()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no parent"))
        {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(&audit_path, &md) {
            Ok(_) => info!("wrote audit report to .compas/audit.md"),
            Err(e) => warn!("failed to write audit report: {}", e),
        }
    }
}

/// Strip the `_pN` suffix added by the chunker when splitting large chunks.
/// This ensures graph symbols match the names returned by extract_calls.
fn strip_part_suffix(symbol: &str) -> String {
    // Match patterns like `foo_p1`, `bar_p12` at the end of the symbol
    if let Some(pos) = symbol.rfind("_p") {
        let suffix = &symbol[pos + 2..];
        if suffix.parse::<u32>().is_ok() {
            return symbol[..pos].to_string();
        }
    }
    symbol.to_string()
}

fn should_include(path: &std::path::Path, include: &[String], exclude: &[String]) -> bool {
    let path_str = path.to_string_lossy();

    for ex in exclude {
        if let Ok(glob) = globset::Glob::new(ex) {
            if glob.compile_matcher().is_match(&*path_str) {
                return false;
            }
        }
    }

    if include.is_empty() {
        return true;
    }

    for inc in include {
        if let Ok(glob) = globset::Glob::new(inc) {
            if glob.compile_matcher().is_match(&*path_str) {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("compas-cli-test-{name}-{nanos}"))
    }

    async fn wait_for_server(port: &str) {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/health");
        for _ in 0..50 {
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("server did not become ready on port {port}");
    }

    fn decl(file: &str, symbol: &str, kind: &str) -> AuditDeclaration {
        AuditDeclaration {
            file: file.into(),
            display_file: file.into(),
            symbol: symbol.into(),
            kind: kind.into(),
        }
    }

    fn reference(caller_file: &str, callee: &str) -> AuditReference {
        AuditReference {
            caller_file: caller_file.into(),
            callee: callee.into(),
        }
    }

    #[test]
    fn test_bare_name_match_credits_all_same_named_declarations() {
        // If `loadUsers` is called anywhere, every declaration named `loadUsers`
        // is considered live — even if the call is on a variable whose type we
        // cannot resolve (the common Dart case with Riverpod providers).
        let analysis = AuditFileAnalysis {
            declarations: vec![
                decl(
                    "lib/management/service_user_management.dart",
                    "UserManagementService.loadUsers",
                    "method",
                ),
                decl(
                    "lib/admin/service_admin.dart",
                    "AdminService.loadUsers",
                    "method",
                ),
            ],
            references: vec![reference(
                "lib/management/provider_user_management.dart",
                "loadUsers",
            )],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected bare-name match to credit all `loadUsers` declarations, got: {candidates:?}"
        );
    }

    #[test]
    fn test_lifecycle_methods_are_hard_suppressed() {
        let analysis = AuditFileAnalysis {
            declarations: vec![
                decl("lib/screen.dart", "MyScreen.createState", "method"),
                decl("lib/screen.dart", "_MyScreenState.initState", "method"),
                decl("lib/screen.dart", "_MyScreenState.build", "method"),
                decl(
                    "lib/widget.dart",
                    "MyInherited.updateShouldNotify",
                    "method",
                ),
            ],
            references: vec![],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected lifecycle methods to be suppressed, got: {candidates:?}"
        );
    }

    #[test]
    fn test_main_function_is_suppressed() {
        let analysis = AuditFileAnalysis {
            declarations: vec![decl("lib/main.dart", "main", "function")],
            references: vec![],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected `main` to be suppressed, got: {candidates:?}"
        );
    }

    #[test]
    fn test_codegen_protocol_methods_are_suppressed() {
        let analysis = AuditFileAnalysis {
            declarations: vec![
                decl("lib/model.dart", "User.fromJson", "constructor"),
                decl("lib/model.dart", "User.toJson", "method"),
                decl("lib/model.dart", "User.toString", "method"),
                decl("lib/model.dart", "User.hashCode", "getter_setter"),
                decl("lib/model.dart", "User.operator ==", "method"),
            ],
            references: vec![],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected protocol/codegen methods to be suppressed, got: {candidates:?}"
        );
    }

    #[test]
    fn test_private_members_are_suppressed() {
        let analysis = AuditFileAnalysis {
            declarations: vec![
                decl("lib/widget.dart", "MyWidget._buildRow", "method"),
                decl("lib/util.dart", "_internalHelper", "function"),
            ],
            references: vec![],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected private members to be suppressed, got: {candidates:?}"
        );
    }

    #[test]
    fn test_default_constructor_skipped_when_class_referenced() {
        // `HolidayService()` shows up in references as just `HolidayService`,
        // and the default constructor is keyed as `HolidayService.HolidayService`.
        let analysis = AuditFileAnalysis {
            declarations: vec![decl(
                "lib/service.dart",
                "HolidayService.HolidayService",
                "constructor",
            )],
            references: vec![reference("lib/provider.dart", "HolidayService")],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected default constructor to be alive when class is referenced, got: {candidates:?}"
        );
    }

    #[test]
    fn test_truly_unreferenced_method_is_flagged() {
        let analysis = AuditFileAnalysis {
            declarations: vec![decl("lib/abandoned.dart", "AbandonedService.run", "method")],
            references: vec![reference("lib/other.dart", "somethingElse")],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert_eq!(
            candidates.len(),
            1,
            "Expected exactly one candidate, got: {candidates:?}"
        );
        assert_eq!(candidates[0].symbol, "AbandonedService.run");
    }

    #[test]
    fn test_filename_placeholder_declarations_are_skipped() {
        let analysis = AuditFileAnalysis {
            declarations: vec![AuditDeclaration {
                file: "lib/core/providers/provider_auth.dart".into(),
                display_file: "lib/core/providers/provider_auth.dart".into(),
                symbol: "provider_auth.dart".into(),
                kind: "declaration".into(),
            }],
            references: vec![],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected no candidates, got: {candidates:?}"
        );
    }

    #[test]
    fn test_getter_setter_kind_is_skipped() {
        let analysis = AuditFileAnalysis {
            declarations: vec![decl("lib/svc.dart", "Service.someGetter", "getter_setter")],
            references: vec![],
            file_edges: vec![],
            key_types: vec![],
        };

        let candidates = classify_dead_code_candidates(&analysis);
        assert!(
            candidates.is_empty(),
            "Expected getter_setter to be suppressed, got: {candidates:?}"
        );
    }

    #[test]
    fn test_normalize_import_uri_returns_manifest_style_absolute_paths() {
        let repo_path = Path::new("/repo");
        let base_file = Path::new("/repo/lib/main.dart");

        assert_eq!(
            normalize_import_uri(
                repo_path,
                base_file,
                "package:tyoajanseuranta/core/providers/provider_auth.dart",
                Some("tyoajanseuranta"),
            ),
            Some("/repo/lib/core/providers/provider_auth.dart".to_string())
        );
        assert_eq!(
            normalize_import_uri(repo_path, base_file, "auth/screen_login.dart", None),
            Some("/repo/lib/auth/screen_login.dart".to_string())
        );
    }

    #[test]
    fn test_reachable_files_follow_package_local_imports() {
        // Reachability is no longer used by dead-code classification, but the
        // helper is kept around for potential future "orphan files" reporting.
        let analysis = AuditFileAnalysis {
            declarations: vec![],
            references: vec![],
            file_edges: vec![(
                "/repo/lib/main.dart".into(),
                "/repo/lib/core/providers/provider_auth.dart".into(),
            )],
            key_types: vec![],
        };

        let manifest = HashMap::from([
            ("/repo/lib/main.dart".to_string(), "hash1".to_string()),
            (
                "/repo/lib/core/providers/provider_auth.dart".to_string(),
                "hash2".to_string(),
            ),
        ]);
        let reachable = reachable_files(&manifest, &analysis);

        assert!(reachable.contains("/repo/lib/main.dart"));
        assert!(reachable.contains("/repo/lib/core/providers/provider_auth.dart"));
    }

    #[tokio::test]
    async fn test_init_index_and_search_over_http() {
        let _guard = test_lock().lock().unwrap();

        let repo_dir = unique_temp_path("repo");
        let home_dir = unique_temp_path("home");
        std::fs::create_dir_all(repo_dir.join("lib")).unwrap();
        std::fs::create_dir_all(&home_dir).unwrap();
        std::fs::write(
            repo_dir.join("lib").join("auth_service.dart"),
            r#"class AuthService {
  Future<String> login(String email, String password) async {
    return email;
  }
}
"#,
        )
        .unwrap();

        let original_dir = std::env::current_dir().unwrap();
        let original_home = std::env::var_os("HOME");
        let original_port = std::env::var_os("COMPAS_PORT");

        let port = ((SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            % 1000)
            + 31000)
            .to_string();

        std::env::set_current_dir(&repo_dir).unwrap();
        std::env::set_var("HOME", &home_dir);
        std::env::set_var("COMPAS_PORT", &port);

        init_repo().unwrap();

        let config_path = repo_dir.join("compas.yaml");
        let config_text = std::fs::read_to_string(&config_path).unwrap();
        let updated = config_text
            .replace("provider: fastembed", "provider: test")
            .replace("model: nomic-ai/nomic-embed-text-v1.5", "model: test");
        std::fs::write(&config_path, updated).unwrap();

        let config = AppConfig::load(config_path.to_str().unwrap()).unwrap();
        index_repo(config).await.unwrap();

        let serve_handle = tokio::spawn(async { serve().await.unwrap() });
        wait_for_server(&port).await;

        let response: Value =
            reqwest::get(format!("http://127.0.0.1:{port}/search?q=authentication"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();

        let results = response["results"].as_array().unwrap();
        assert!(
            !results.is_empty(),
            "expected search results, got {response}"
        );
        let symbols: Vec<&str> = results
            .iter()
            .filter_map(|result| result["chunk"]["symbol"].as_str())
            .collect();
        assert!(
            symbols.contains(&"AuthService") || symbols.contains(&"AuthService.login"),
            "expected auth symbols in results, got {symbols:?}"
        );

        serve_handle.abort();
        let _ = serve_handle.await;

        std::env::set_current_dir(original_dir).unwrap();
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_port {
            Some(value) => std::env::set_var("COMPAS_PORT", value),
            None => std::env::remove_var("COMPAS_PORT"),
        }

        std::fs::remove_dir_all(&repo_dir).unwrap();
        std::fs::remove_dir_all(&home_dir).unwrap();
    }

    #[tokio::test]
    async fn test_optimize_edge_shard_succeeds_for_initialized_repo() {
        let repo_dir = unique_temp_path("optimize");
        std::fs::create_dir_all(repo_dir.join(".compas")).unwrap();

        let config = AppConfig {
            repo: compas::config::RepoConfig {
                path: repo_dir.to_string_lossy().to_string(),
                include: vec!["lib/**/*.dart".into()],
                exclude: vec![],
            },
            embedder: compas::config::EmbedderConfig {
                provider: "fastembed".into(),
                model: "nomic-ai/nomic-embed-text-v1.5".into(),
                query_prefix: None,
                doc_prefix: None,
            },
            store: compas::config::StoreConfig {
                provider: "edge".into(),
                path: ".compas/edge-shard".into(),
                vector_name: "default".into(),
            },
            server: compas::config::ServerConfig {
                host: "127.0.0.1".into(),
                port: "3001".into(),
            },
            index: compas::config::IndexConfig {
                chunk_by: "function".into(),
                watch: true,
            },
        };

        let store = EdgeStore::new(repo_dir.join(&config.store.path), &config.store.vector_name);
        store.init(4).await.unwrap();
        drop(store);

        let _ = optimize_edge_shard(&config).unwrap();

        std::fs::remove_dir_all(&repo_dir).unwrap();
    }

    #[test]
    fn test_watch_include_patterns_match_nested_dart_files() {
        assert!(should_include(
            Path::new("lib/services/auth_service.dart"),
            &["lib/**/*.dart".into()],
            &[]
        ));
        assert!(!should_include(
            Path::new("build/generated/auth_service.g.dart"),
            &["lib/**/*.dart".into()],
            &["**/*.g.dart".into(), "build/**".into()]
        ));
    }
}
