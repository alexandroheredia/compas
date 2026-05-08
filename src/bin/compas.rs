use async_trait::async_trait;
use clap::{Parser, Subcommand};
use compas::{
    chunker::{dart::extract_calls, ChunkerRegistry},
    config::AppConfig,
    embedder::{ollama::OllamaEmbedder, EmbedMode, Embedder},
    graph::Graph,
    mcp::{self, state::McpAppState},
    server::{router, AppState, RepoState},
    store::{qdrant::QdrantStore, Store},
    watcher::{FileWatcher, Handler},
};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

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
    /// Start the REST server
    Serve,
    /// Start the MCP stdio server for agent integration
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
        if let Some(ext) = entry.path().extension() {
            let ext = ext.to_string_lossy().to_string();
            if matches!(
                ext.as_str(),
                "dart" | "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java"
            ) {
                *counts.entry(ext).or_insert(0) += 1;
            }
        }
    }

    let dominant = counts
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(e, _)| e.as_str());

    let (include, exclude) = match dominant {
        Some("dart") => (
            vec!["lib/**/*.dart", "test/**/*.dart"],
            vec!["**/*.g.dart", "build/**", ".dart_tool/**"],
        ),
        Some("ts") | Some("tsx") => (
            vec!["src/**/*.{ts,tsx}", "lib/**/*.{ts,tsx}"],
            vec!["node_modules/**", "dist/**", "build/**"],
        ),
        Some("rs") => (vec!["src/**/*.rs", "crates/**/*.rs"], vec!["target/**"]),
        Some("py") => (
            vec!["**/*.py"],
            vec!["venv/**", ".venv/**", "__pycache__/**"],
        ),
        Some("go") => (vec!["**/*.go"], vec!["vendor/**"]),
        _ => (vec!["src/**/*"], vec!["build/**", "dist/**"]),
    };

    let collection = repo_name.to_lowercase().replace(' ', "_").to_string();

    let yaml = format!(
        r#"repo:
  path: .
  include:
{}
  exclude:
{}

embedder:
  provider: ollama
  model: nomic-embed-text
  url: http://localhost:11434

store:
  provider: qdrant
  url: http://localhost:6333
  collection: {}

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
        collection,
    );

    std::fs::write(&config_path, yaml)?;

    // Register repo in global registry
    let mut registry = compas::config::RepoRegistry::load();
    let repo_name = repo_name.to_lowercase().replace(' ', "_");
    registry.register(&repo_name, &cwd);
    registry.save()?;
    println!("Registered repo '{}' in global registry", repo_name);

    // Write AGENTS.md so agents know to use compas
    let agents_md = r#"# AGENTS.md — Navigation Rules

> **Before opening any file you're uncertain about, search compas.**
> Do not guess file paths. Query first, read second.

## Compas (Local Semantic Search)

This repo is indexed by **compas** — a local semantic code-search engine.
It finds symbols by natural language meaning (not just filename matching) and knows the call graph.

### Tools Available

| Tool | Use When |
|------|----------|
| `search_codebase` | You need to find code by concept ("where is auth?", "how does caching work?") |
| `get_symbol_graph` | You need to trace callers/callees for a specific symbol |

### Trigger Phrases (USE COMPAS IMMEDIATELY)

- "I need to find..."
- "Where is..."
- "Which file..."
- "I'm not sure where..."
- "Let me check..."
- "I think it's in..."
- "Probably in..."
- "How does X work?"

### Workflow

1. **Search**: `search_codebase({ query: "...", limit: 10 })`
2. **Deepen** (optional): `get_symbol_graph({ symbol: "..." })`
3. **Read**: Open ONLY the file(s) compas confirmed

### When to Skip Compas

Only skip if you already know the **exact file path and line number**.

---

*Generated by `compas init`. Update or remove this file as needed.*
"#;

    let agents_path = cwd.join("AGENTS.md");
    if !agents_path.exists() {
        std::fs::write(&agents_path, agents_md)?;
        println!("Created AGENTS.md in {:?}", cwd);
    } else {
        println!("AGENTS.md already exists. Skipping.");
    }

    println!("Created compas.yaml in {:?}", cwd);
    println!("Detected language: {}", dominant.unwrap_or("unknown"));
    println!("\nNext steps:");
    println!("  1. Start Qdrant:  docker-compose up -d");
    println!("  2. Start Ollama:  ollama serve");
    println!("  3. Index repo:    compas index");
    println!("  4. Start server:  compas serve");
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

async fn index_repo(config: AppConfig) -> anyhow::Result<()> {
    let embedder = Arc::new(OllamaEmbedder::new(
        &config.embedder.url,
        &config.embedder.model,
        config.embedder.query_prefix.clone().unwrap_or_default(),
        config.embedder.doc_prefix.clone().unwrap_or_default(),
    ));
    let store = Arc::new(QdrantStore::new(
        &config.store.url,
        &config.store.collection,
    ));
    store.init(embedder.dimensions()).await?;

    let registry = ChunkerRegistry::new();
    let chunker = registry
        .get("dart")
        .ok_or_else(|| anyhow::anyhow!("no dart chunker"))?;

    let repo_path = std::fs::canonicalize(&config.repo.path)?;

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
        if !path.extension().map(|e| e == "dart").unwrap_or(false) {
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

    // ── Load manifest and detect deleted files ──────────────────────────────
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
                && (chunk.kind == "method"
                    || chunk.kind == "function"
                    || chunk.kind == "constructor")
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

        let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
        let embeddings = match embedder.embed_batch(&texts, EmbedMode::Document).await {
            Ok(e) => e,
            Err(e) => {
                if let Some(ref bar) = pb {
                    bar.println(format!("⚠  skip embed error in {}: {}", rel_str, e));
                } else {
                    warn!("  skip embed error: {}", e);
                }
                failed += 1;
                if let Some(ref bar) = pb {
                    bar.inc(1);
                }
                continue;
            }
        };

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

        if let Ok(calls) = extract_calls(&content) {
            for (caller, callee) in &calls {
                graph.add_symbol(caller, path.to_str().unwrap(), "method");
                graph.add_call(caller, path.to_str().unwrap(), callee);
            }
        }

        processed += 1;
        total_chunks += chunks.len();
        new_manifest.insert(path_str, hash.clone());

        if let Some(ref bar) = pb {
            bar.inc(1);
        } else {
            info!("  ✓ done ({} chunks)", chunks.len());
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

    generate_audit(
        &graph,
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

    let dead_code_count = graph
        .all_nodes()
        .iter()
        .filter(|(_, n)| {
            !n.file.is_empty()
                && n.kind != "class"
                && n.kind != "mixin"
                && n.kind != "extension"
                && n.calls.is_empty()
                && n.called_by.is_empty()
        })
        .count();

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
    Ok(())
}

async fn run_mcp() -> anyhow::Result<()> {
    let registry = compas::config::RepoRegistry::load();

    if registry.repos.is_empty() {
        println!("No repos registered. Run 'compas init' in a repository first.");
        return Ok(());
    }

    let mut repos = HashMap::new();
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

        let store: Arc<dyn compas::store::Store> = Arc::new(QdrantStore::new(
            &config.store.url,
            &config.store.collection,
        ));
        let graph = Arc::new(Graph::new());
        let embedder: Arc<dyn compas::embedder::Embedder> = Arc::new(OllamaEmbedder::new(
            &config.embedder.url,
            &config.embedder.model,
            config.embedder.query_prefix.clone().unwrap_or_default(),
            config.embedder.doc_prefix.clone().unwrap_or_default(),
        ));

        let repo_path = std::fs::canonicalize(path)?;
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

        let store: Arc<dyn compas::store::Store> = Arc::new(QdrantStore::new(
            &config.store.url,
            &config.store.collection,
        ));
        let graph = Arc::new(Graph::new());
        let embedder: Arc<dyn compas::embedder::Embedder> = Arc::new(OllamaEmbedder::new(
            &config.embedder.url,
            &config.embedder.model,
            config.embedder.query_prefix.clone().unwrap_or_default(),
            config.embedder.doc_prefix.clone().unwrap_or_default(),
        ));

        let repo_path = std::fs::canonicalize(path)?;
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
    let store: Arc<dyn compas::store::Store> = Arc::new(QdrantStore::new(
        &config.store.url,
        &config.store.collection,
    ));
    let embedder: Arc<dyn compas::embedder::Embedder> = Arc::new(OllamaEmbedder::new(
        &config.embedder.url,
        &config.embedder.model,
        config.embedder.query_prefix.clone().unwrap_or_default(),
        config.embedder.doc_prefix.clone().unwrap_or_default(),
    ));
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
        if !path.extension().map(|e| e == "dart").unwrap_or(false) {
            return;
        }

        info!("reindexing {}", file_path);

        let content = match tokio::fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to read {}: {}", file_path, e);
                return;
            }
        };

        let chunker = match self.registry.get("dart") {
            Some(c) => c,
            None => {
                warn!("no dart chunker available");
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

        let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
        let embeddings = match self.embedder.embed_batch(&texts, EmbedMode::Document).await {
            Ok(e) => e,
            Err(e) => {
                warn!("embed failed for {}: {}", file_path, e);
                return;
            }
        };

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
        if let Ok(calls) = extract_calls(&content) {
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
    graph: &Graph,
    missing_docs: &[(String, String, String, usize)],
    files: usize,
    chunks: usize,
    failed: usize,
) {
    let nodes = graph.all_nodes();

    // Classify dead code candidates
    let flutter_lifecycle = [
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
    let flutter_callback_suffixes = [
        "onPressed",
        "onTap",
        "onChanged",
        "onSaved",
        "onSubmitted",
        "onEditingComplete",
    ];

    let mut dead_code: Vec<(String, String, String)> = vec![]; // (file, symbol, likely_false_positive)
    for (_key, node) in nodes.iter() {
        if node.file.is_empty()
            || node.kind == "class"
            || node.kind == "mixin"
            || node.kind == "extension"
        {
            continue;
        }
        if !node.calls.is_empty() || !node.called_by.is_empty() {
            continue;
        }
        let method_name = node.name.rsplit('.').next().unwrap_or(&node.name);
        let is_lifecycle = flutter_lifecycle.contains(&method_name);
        let is_callback = flutter_callback_suffixes
            .iter()
            .any(|s| method_name.ends_with(s) || method_name == s.trim_start_matches("on"));
        let is_private_test = method_name.starts_with('_') && node.kind == "method";
        let reason = if is_lifecycle {
            "Flutter lifecycle".to_string()
        } else if is_callback {
            "Likely callback".to_string()
        } else if is_private_test {
            "Private, likely callback".to_string()
        } else {
            "Review carefully".to_string()
        };
        let rel = std::path::Path::new(&node.file)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| node.file.clone());
        dead_code.push((rel, node.name.clone(), reason));
    }

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
    dead_code.sort_by(|a, b| a.0.cmp(&b.0));

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
        "These symbols have no inbound or outbound calls. Many are likely false positives\n",
    );
    md.push_str("(Flutter callbacks, lifecycle methods, getters accessed as properties). Review before removing.\n\n");
    if dead_code.is_empty() {
        md.push_str("*No dead code candidates found!* 🎉\n");
    } else {
        md.push_str("| File | Symbol | Assessment |\n");
        md.push_str("|------|--------|-------------|\n");
        for (file, symbol, reason) in &dead_code {
            md.push_str(&format!("| {} | {} | {} |\n", file, symbol, reason));
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
