//! MCP server (`plan.md` §8) — 7 read-only tools with token-disciplined output
//! contracts. Server holds the store loaded once at startup (restart to pick
//! up a re-sync). All tools return compact text per the approved §8 contracts.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rmcp::{
    ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    schemars, tool, tool_router,
};
use serde::Deserialize;
use tokio::io::{stdin, stdout};

use crate::config::Config;
use crate::graph::{downstream, impact, kind_label, upstream};
use crate::model::{EdgeKind, Node, NodeKind, Store};

struct StoreCell {
    path: PathBuf,
    mtime: Option<SystemTime>,
    store: Arc<Store>,
}

pub struct PropagatorServer {
    cell: Mutex<StoreCell>,
}

impl PropagatorServer {
    /// Current store snapshot, transparently reloaded if `store.bin` changed on
    /// disk since the last call. Sync is ~2s and re-runs often in the feature
    /// flow (dd dumps, topics.toml), so this removes the "restart MCP" ritual.
    fn store(&self) -> Arc<Store> {
        let mut cell = self.cell.lock().unwrap();
        let disk_mtime = std::fs::metadata(&cell.path)
            .and_then(|m| m.modified())
            .ok();
        if disk_mtime != cell.mtime {
            if let Ok(fresh) = Store::load(&cell.path) {
                cell.store = Arc::new(fresh);
                cell.mtime = disk_mtime;
            }
        }
        cell.store.clone()
    }
}

fn human_age(synced_at: u64) -> String {
    if synced_at == 0 {
        return "unknown".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(synced_at);
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

// ---- Parameter structs (one per tool with args) ----

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct GetSymbolParams {
    /// Symbol name (bare or SCHEMA.NAME).
    name: String,
    /// Set true to include file:line definition location. Default false.
    #[serde(default)]
    include_path: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct DepthParams {
    name: String,
    /// Traversal depth. Default 1.
    #[serde(default)]
    depth: Option<usize>,
    /// Cap on returned rows. Default 50.
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ImpactParams {
    name: String,
    /// Traversal depth. Default 5.
    #[serde(default)]
    depth: Option<usize>,
    /// Cap on returned nodes. Default 200.
    #[serde(default)]
    max_results: Option<usize>,
    /// Include weak `Owns` edges. Default false.
    #[serde(default)]
    include_weak: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ChunkParams {
    name: String,
    /// Return signature only (default true — dominant token-saver).
    #[serde(default)]
    signature_only: Option<bool>,
    /// Max body lines when signature_only=false. Default 40.
    #[serde(default)]
    max_lines: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct NameParams {
    name: String,
}

#[tool_router(server_handler)]
impl PropagatorServer {
    #[tool(
        name = "describe_corpus",
        description = "What's in the corpus: service/proc/table/topic counts and samples. Call once per session."
    )]
    fn describe_corpus(&self) -> String {
        let s = self.store();
        let mut counts = s.node_kind_count();
        let sample = |k: NodeKind| -> String {
            s.nodes
                .iter()
                .filter(|n| n.kind == k)
                .take(5)
                .map(Node::key)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let m = &s.meta;
        let no_dd: Vec<&str> = m
            .sql_coverage
            .iter()
            .filter(|c| !c.has_dd_dump)
            .map(|c| c.service.as_str())
            .collect();
        let coverage = if no_dd.is_empty() {
            "complete".to_string()
        } else {
            format!(
                "MISSING dd dump for [{}] — Calls/Touches absent there; do NOT read an empty get_impact as \"nothing depends on it\"",
                no_dd.join(", ")
            )
        };
        // Cross-world topic bridges honesty signal (mirrors sql_coverage): a
        // producer→consumer topic without `provenance` may over-report its
        // consumers (promiscuous Service-hub). See graph::topic_bridge_summary.
        let topic_bridges = crate::graph::topic_bridge_summary(&s);
        format!(
            "services={} procs={} funcs={} tables={} topics={}\nsample procs: {}\nsample topics: {}\nsynced: {}\nunresolved MQ call sites: {} (candidates for topics.toml)\nSQL coverage: {}\ntopic bridges: {}\n",
            counts.remove(&NodeKind::Service).unwrap_or(0),
            counts.remove(&NodeKind::Procedure).unwrap_or(0),
            counts.remove(&NodeKind::Function).unwrap_or(0),
            counts.remove(&NodeKind::Table).unwrap_or(0),
            counts.remove(&NodeKind::Topic).unwrap_or(0),
            sample(NodeKind::Procedure),
            sample(NodeKind::Topic),
            human_age(m.synced_at),
            m.unresolved.len(),
            coverage,
            topic_bridges,
        )
    }

    #[tool(
        name = "get_symbol",
        description = "Identity + definition location of a symbol. For body use get_chunk."
    )]
    fn get_symbol(
        &self,
        Parameters(GetSymbolParams { name, include_path }): Parameters<GetSymbolParams>,
    ) -> String {
        let store = self.store();
        let n = match store.find_or_suggest(&name) {
            Ok(n) => n,
            Err(msg) => return msg,
        };
        let mut out = format!(
            "{} | {} | schema={}",
            n.key(),
            kind_label(n.kind),
            n.schema.as_deref().unwrap_or("-")
        );
        if include_path.unwrap_or(false) {
            if let Some(cid) = n.chunk {
                let c = &store.chunks[cid];
                out.push_str(&format!(
                    " | def {}:{}-{}",
                    c.path, c.line_start, c.line_end
                ));
            }
        }
        let inc = store.incident(n.id, false);
        let out_n = store.incident(n.id, true);
        out.push_str(&format!(" | in={} out={}", inc.len(), out_n.len()));
        out
    }

    #[tool(
        name = "get_callers",
        description = "Direct callers of a symbol (reverse traversal). For transitive blast radius use get_impact."
    )]
    fn get_callers(
        &self,
        Parameters(DepthParams {
            name,
            depth,
            max_results,
        }): Parameters<DepthParams>,
    ) -> String {
        let store = self.store();
        let n = match store.find_or_suggest(&name) {
            Ok(n) => n,
            Err(msg) => return msg,
        };
        let reaches = upstream(&store, n.id, depth.unwrap_or(1), false);
        format_reaches(&store, &reaches, max_results.unwrap_or(50))
    }

    #[tool(
        name = "get_dependencies",
        description = "What a symbol directly depends on (forward traversal)."
    )]
    fn get_dependencies(
        &self,
        Parameters(DepthParams {
            name,
            depth,
            max_results,
        }): Parameters<DepthParams>,
    ) -> String {
        let store = self.store();
        let n = match store.find_or_suggest(&name) {
            Ok(n) => n,
            Err(msg) => return msg,
        };
        let reaches = downstream(&store, n.id, depth.unwrap_or(1), false);
        format_reaches(&store, &reaches, max_results.unwrap_or(50))
    }

    #[tool(
        name = "get_impact",
        description = "Transitive blast radius (impact-flow table), grouped per depth. Crosses MQ<->SQL worlds via Owns/Invokes. Don't chain get_callers."
    )]
    fn get_impact(
        &self,
        Parameters(ImpactParams {
            name,
            depth,
            max_results,
            include_weak,
        }): Parameters<ImpactParams>,
    ) -> String {
        let store = self.store();
        let n = match store.find_or_suggest(&name) {
            Ok(n) => n,
            Err(msg) => return msg,
        };
        let reaches = impact(
            &store,
            n.id,
            depth.unwrap_or(5),
            include_weak.unwrap_or(false),
        );
        if reaches.is_empty() {
            return "(none)".into();
        }
        let cap = max_results.unwrap_or(200);
        let mut out = String::new();
        let mut shown = 0usize;
        let mut max_d = 0;
        for r in &reaches {
            max_d = max_d.max(r.depth);
        }
        for d in 1..=max_d {
            let mut row = Vec::new();
            for r in reaches.iter().filter(|r| r.depth == d) {
                if shown >= cap {
                    break;
                }
                let nn = &store.nodes[r.node];
                let e = &store.edges[r.via];
                row.push(format!(
                    "{}:{} via:{}",
                    nn.key(),
                    kind_label(nn.kind),
                    edge_label(e.kind)
                ));
                shown += 1;
            }
            if !row.is_empty() {
                out.push_str(&format!("d{d}: [{}]\n", row.join("; ")));
            }
        }
        if reaches.len() >= crate::graph::DEFAULT_CAP {
            out.push_str("(cap hit) narrow: lower depth or include_weak=false\n");
        }
        out.trim_end().to_string()
    }

    #[tool(
        name = "get_topic",
        description = "Producers and consumers of a topic with file:line evidence."
    )]
    fn get_topic(&self, Parameters(NameParams { name }): Parameters<NameParams>) -> String {
        let store = self.store();
        let n = match store.find_node(&name).filter(|n| n.kind == NodeKind::Topic) {
            Some(n) => n,
            None => {
                let sug = store.suggest(&name, 5);
                return if sug.is_empty() {
                    format!("topic not found: {name}")
                } else {
                    format!("topic not found: {name} — closest: {}", sug.join(", "))
                };
            }
        };
        let mut producers = Vec::new();
        let mut consumers = Vec::new();
        for e in store.incident(n.id, false) {
            let svc = &store.nodes[e.from].name;
            let loc = format!("{svc} ({}:{})", e.path, e.line);
            match e.kind {
                EdgeKind::Publishes => producers.push(loc),
                EdgeKind::Consumes => consumers.push(loc),
                _ => {}
            }
        }
        format!(
            "{name}\nproducers: {}\nconsumers: {}",
            producers.join("; "),
            consumers.join("; ")
        )
    }

    #[tool(
        name = "get_chunk",
        description = "Body for grounding a symbol. Default returns SIGNATURE only (~30 tok); set signature_only=false for full body (capped)."
    )]
    fn get_chunk(
        &self,
        Parameters(ChunkParams {
            name,
            signature_only,
            max_lines,
        }): Parameters<ChunkParams>,
    ) -> String {
        let store = self.store();
        let n = match store.find_or_suggest(&name) {
            Ok(n) => n,
            Err(msg) => return msg,
        };
        let Some(cid) = n.chunk else {
            return format!("{}: no source chunk", n.key());
        };
        let c = &store.chunks[cid];
        if signature_only.unwrap_or(true) {
            return c.signature.clone();
        }
        let cap = max_lines.unwrap_or(40);
        let body: String = c.body.lines().take(cap).collect::<Vec<_>>().join("\n");
        let suffix = if c.body.lines().count() > cap {
            "\n…truncated"
        } else {
            ""
        };
        format!(
            "-- {}:{}-{}\n{body}{suffix}",
            c.path, c.line_start, c.line_end
        )
    }
}

/// Start the stdio MCP server. Loads the store once at startup, then hot-reloads
/// on file change. `config` pins an explicit `propagator.toml`; `None` falls back
/// to cwd-based discovery.
pub fn serve(config: Option<PathBuf>) -> Result<()> {
    let (cfg, _) = match config {
        Some(p) => Config::load_explicit(&p)?,
        None => Config::discover()?,
    };
    let path = cfg.store.path.clone();
    let store = Store::load(&path)?;
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let server = PropagatorServer {
        cell: Mutex::new(StoreCell {
            path,
            mtime,
            store: Arc::new(store),
        }),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let service = server.serve((stdin(), stdout())).await?;
        service.waiting().await?;
        Ok::<_, anyhow::Error>(())
    })
}

fn format_reaches(store: &Store, reaches: &[crate::graph::Reach], cap: usize) -> String {
    if reaches.is_empty() {
        return "(none)".into();
    }
    reaches
        .iter()
        .take(cap)
        .map(|r| {
            let n = &store.nodes[r.node];
            let e = &store.edges[r.via];
            format!(
                "{}:{} via:{} ({}:{})",
                n.key(),
                kind_label(n.kind),
                edge_label(e.kind),
                e.path,
                e.line
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
fn _router_type_check() -> ToolRouter<PropagatorServer> {
    PropagatorServer::tool_router()
}
