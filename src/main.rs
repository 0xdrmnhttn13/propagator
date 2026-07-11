//! Propagator CLI: sync / callers / deps / impact / topic / serve.

mod affected;
mod config;
mod extract;
mod graph;
mod mcp;
mod model;
mod store;
mod sync;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::graph::{downstream, impact, kind_label, upstream};
use crate::model::{EdgeKind, NodeKind, Store};

#[derive(Parser)]
#[command(
    name = "propagator",
    version,
    about = "Dependency & message-flow tracing"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the graph store from configured sources.
    Sync {
        /// Explicit propagator.toml to bind to (expands ~/). Without it, config
        /// is discovered from cwd. Lets a sync target an alternate store path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Who references SYMBOL (reverse traversal).
    Callers {
        name: String,
        #[arg(long, default_value_t = 1)]
        depth: usize,
    },
    /// What SYMBOL depends on (forward traversal).
    Deps {
        name: String,
        #[arg(long, default_value_t = 1)]
        depth: usize,
    },
    /// Blast radius: who is impacted if SYMBOL changes (impact-flow table).
    Impact {
        name: String,
        #[arg(long, default_value_t = 5)]
        depth: usize,
        #[arg(long)]
        include_weak: bool,
    },
    /// Producers/consumers of a topic.
    Topic { name: String },
    /// Corpus summary + coverage/honesty signals (topic-bridge provenance).
    Describe,
    /// Emit the ALL_DEPENDENCIES query for a schema (to run via SQLcl MCP).
    DdQuery { schema: String },
    /// Blast radius of uncommitted changes: pipe `git diff` on stdin.
    Affected {
        /// Exit 1 if impact reaches a service other than the origin.
        #[arg(long = "fail-on")]
        fail_on: Option<String>,
        /// Output format: md (default) or json.
        #[arg(long, default_value = "md")]
        out: String,
    },
    /// Start the MCP server (stdio).
    Serve {
        /// Explicit propagator.toml to bind to (expands ~/). Without it, the
        /// server discovers a config from its cwd — which may be the wrong repo.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Sync { config } => cmd_sync(config),
        Cmd::Callers { name, depth } => cmd_query(&name, depth, false, QueryKind::Callers),
        Cmd::Deps { name, depth } => cmd_query(&name, depth, false, QueryKind::Deps),
        Cmd::Impact {
            name,
            depth,
            include_weak,
        } => cmd_query(&name, depth, include_weak, QueryKind::Impact),
        Cmd::Topic { name } => cmd_topic(&name),
        Cmd::Describe => cmd_describe(),
        Cmd::DdQuery { schema } => {
            println!("{}", crate::extract::oracle_dd::query(&schema));
            Ok(())
        }
        Cmd::Affected { fail_on, out } => {
            affected::cli_run(fail_on.as_deref() == Some("cross-service"), out == "json")
        }
        Cmd::Serve { config } => mcp::serve(config),
    }
}

enum QueryKind {
    Callers,
    Deps,
    Impact,
}

fn load_store(cfg: &Config) -> Result<Store> {
    Store::load(&cfg.store.path)
}

fn cmd_describe() -> Result<()> {
    let (cfg, _) = Config::discover()?;
    let store = load_store(&cfg)?;
    let mut counts = store.node_kind_count();
    println!(
        "services={} procs={} funcs={} tables={} topics={}",
        counts.remove(&NodeKind::Service).unwrap_or(0),
        counts.remove(&NodeKind::Procedure).unwrap_or(0),
        counts.remove(&NodeKind::Function).unwrap_or(0),
        counts.remove(&NodeKind::Table).unwrap_or(0),
        counts.remove(&NodeKind::Topic).unwrap_or(0),
    );
    println!("topic bridges: {}", graph::topic_bridge_summary(&store));
    Ok(())
}

fn cmd_sync(config: Option<PathBuf>) -> Result<()> {
    let (cfg, _cfg_path) = match config {
        Some(p) => Config::load_explicit(&p)?,
        None => Config::discover()?,
    };
    let (store, report) = sync::run(&cfg)?;
    store.save(&cfg.store.path)?;
    eprintln!(
        "synced: services={} defs={} edges={} unresolved_topics={} invokes_noise_dropped={} code_dynamic_tables={} redis_dynamic_keys={}",
        report.services,
        report.defs,
        report.edges,
        report.unresolved.len(),
        report.invokes_dropped,
        report.code_dynamic_tables,
        report.redis_dynamic_keys
    );
    if !report.unresolved.is_empty() {
        eprintln!("unresolved call sites (isi topics.toml untuk ini):");
        for (loc, line) in report.unresolved.iter().take(20) {
            eprintln!("  {loc}:{line}");
        }
        if report.unresolved.len() > 20 {
            eprintln!("  … dan {} lagi", report.unresolved.len() - 20);
        }
    }
    eprintln!("store: {}", cfg.store.path.display());
    Ok(())
}

fn cmd_query(name: &str, depth: usize, include_weak: bool, kind: QueryKind) -> Result<()> {
    let (cfg, _) = Config::discover()?;
    let store = load_store(&cfg)?;
    let node = match store.find_or_suggest(name) {
        Ok(n) => n,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };
    let reaches = match kind {
        QueryKind::Callers => upstream(&store, node.id, depth, include_weak),
        QueryKind::Deps => downstream(&store, node.id, depth, include_weak),
        QueryKind::Impact => impact(&store, node.id, depth, include_weak),
    };
    if reaches.is_empty() {
        println!("(none)");
        return Ok(());
    }
    match kind {
        QueryKind::Impact => {
            // Group by depth.
            let mut max_d = 0;
            for r in &reaches {
                max_d = max_d.max(r.depth);
            }
            for d in 1..=max_d {
                let row: Vec<String> = reaches
                    .iter()
                    .filter(|r| r.depth == d)
                    .map(|r| {
                        let n = &store.nodes[r.node];
                        let e = &store.edges[r.via];
                        format!(
                            "{}:{} via:{}",
                            n.key(),
                            kind_label(n.kind),
                            edge_label(e.kind)
                        )
                    })
                    .collect();
                if !row.is_empty() {
                    println!("d{d}: [{}]", row.join("; "));
                }
            }
        }
        QueryKind::Callers | QueryKind::Deps => {
            for r in reaches {
                let n = &store.nodes[r.node];
                let e = &store.edges[r.via];
                println!(
                    "{}:{} via:{} ({}:{})",
                    n.key(),
                    kind_label(n.kind),
                    edge_label(e.kind),
                    e.path,
                    e.line
                );
            }
        }
    }
    Ok(())
}

fn cmd_topic(name: &str) -> Result<()> {
    let (cfg, _) = Config::discover()?;
    let store = load_store(&cfg)?;
    let node = store
        .find_node(name)
        .filter(|n| n.kind == NodeKind::Topic)
        .with_context(|| format!("topic not found: {name}"))?;
    let mut producers = Vec::new();
    let mut consumers = Vec::new();
    for e in store.incident(node.id, false) {
        match e.kind {
            EdgeKind::Publishes => producers.push(format!(
                "{} ({}:{})",
                store.nodes[e.from].name, e.path, e.line
            )),
            EdgeKind::Consumes => consumers.push(format!(
                "{} ({}:{})",
                store.nodes[e.from].name, e.path, e.line
            )),
            _ => {}
        }
    }
    println!("{name}");
    println!("producers: {}", producers.join("; "));
    println!("consumers: {}", consumers.join("; "));
    Ok(())
}

fn edge_label(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Calls => "Calls",
        EdgeKind::Touches => "Touches",
        EdgeKind::Publishes => "Publishes",
        EdgeKind::Consumes => "Consumes",
        EdgeKind::Owns => "Owns",
        EdgeKind::Invokes => "Invokes",
        EdgeKind::ReadsKey => "ReadsKey",
        EdgeKind::WritesKey => "WritesKey",
    }
}

#[allow(dead_code)]
fn cfg_dir() -> PathBuf {
    PathBuf::from(".")
}
