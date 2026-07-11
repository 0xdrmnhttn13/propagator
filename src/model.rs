//! Core data model: chunks, nodes, edges, store.
//!
//! SQL node identity is `(schema, name)` canonical; topic/service identity is
//! the case-sensitive name as written. See `plan.md` §5.

use serde::{Deserialize, Serialize};

pub type ChunkId = usize;
pub type NodeId = usize;
pub type EdgeId = usize;

pub const STORE_VERSION: u32 = 5;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chunk {
    pub id: ChunkId,
    pub service: String,
    pub path: String,
    pub kind: ChunkKind,
    pub title: String,
    pub line_start: usize,
    pub line_end: usize,
    /// Signature line only (kept separate from body so `get_chunk` can serve
    /// signature-only cheaply — the dominant token-saver, see `plan.md` §8).
    pub signature: String,
    /// Full source body. Loaded lazily by tools; never returned by default.
    pub body: String,
    pub node: Option<NodeId>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChunkKind {
    SqlProc,
    SqlFunction,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub name: String,
    /// Canonical schema for SQL nodes (`Some("OMS")`); `None` for service/topic.
    pub schema: Option<String>,
    pub kind: NodeKind,
    pub chunk: Option<ChunkId>,
}

impl Node {
    /// Canonical lookup key. SQL → `SCHEMA.NAME`, else bare name (case-sensitive).
    pub fn key(&self) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}", s, self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NodeKind {
    Procedure,
    Function,
    Table,
    Service,
    Topic,
    /// Redis key. Identity is the **key pattern** as written — a literal
    /// (`test-key`) or a prefix + `*` for compositionally-built keys
    /// (`TRADELISTHIST::*`, `order:*:*`). Case-sensitive, never schema-split.
    RedisKey,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Edge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    /// Evidence: file path where the edge was found, or `<registry>` for
    /// `topics.toml`, or `<oracle-dd>` for `ALL_DEPENDENCIES`.
    pub path: String,
    /// 1-based line, `0` for registry/dictionary-sourced edges.
    pub line: usize,
    /// Producer-provenance (only meaningful for `Publishes`): the set of Table
    /// nodes whose data actually feeds this topic's payload. Declared in
    /// `topics.toml` (`[svc.provenance]`) because config-injected topics have no
    /// scannable call site. **Empty = unknown → impact BFS does not constrain
    /// crossing** (preserves old behavior). **Non-empty = whitelist → a SQL-world
    /// change only crosses into the topic if the changed table is in this set.**
    /// Kills the "consumes topic ⇒ impacted by every table upstream of producer"
    /// false positive (Service node is otherwise a promiscuous hub). See
    /// `feature-flow.md` §6 / the field-blind-topic-edge note.
    #[serde(default)]
    pub provenance: Vec<NodeId>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EdgeKind {
    /// procedure → procedure (SQL)
    Calls,
    /// procedure → table (SQL). Undifferentiated in v1; R/W split is first
    /// §12 quality upgrade — see `plan.md` §7.
    Touches,
    /// service → topic (MQ)
    Publishes,
    /// service → topic (MQ). Traversal direction inverted during impact BFS.
    Consumes,
    /// service → procedure/function/table (always-on, from `SourceCfg.service`).
    /// Weak edge — excluded from `get_impact` unless `include_weak=true`.
    Owns,
    /// service → procedure (best-effort, from hard-coded literal scan).
    Invokes,
    /// service → RedisKey (read verb: GET/HGET/MGET/EXISTS/…). Backward: a
    /// change in the key's shape/data impacts the reader.
    ReadsKey,
    /// service → RedisKey (write verb: SET/HSET/DEL/EXPIRE/…). Backward:
    /// grouped with readers so impact BFS reaches every service on the key.
    WritesKey,
}

impl EdgeKind {
    /// Whether impact flows *forward* along this edge kind (from→to) or
    /// *backward* (to→from). Encodes the "dampak mengalir" table
    /// (`plan.md` §7) as data, not scattered if-else.
    ///
    /// - Calls:       change in callee impacts caller    → backward
    /// - Touches:     change in table impacts touchers   → backward
    /// - Publishes:   change in producer impacts topic   → forward
    /// - Consumes:    change in topic impacts consumer   → backward
    /// - Owns:        change in member impacts owner     → backward (weak)
    /// - Invokes:     change in callee impacts caller    → backward
    #[must_use]
    pub fn impact_forward(self) -> bool {
        matches!(self, EdgeKind::Publishes)
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Store {
    pub version: u32,
    pub chunks: Vec<Chunk>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Provenance/trust info surfaced by `describe_corpus` so an agent can tell
    /// "genuinely empty" from "graph is incomplete here". See `feature-flow.md` §6.
    pub meta: Meta,
    /// Pre-built adjacency; recomputed on load.
    #[serde(skip)]
    pub graph: Graph,
}

/// Corpus provenance. Persisted so the MCP layer can report staleness and
/// coverage holes without re-running sync.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Meta {
    /// Unix seconds at sync time. `0` if unknown (older store).
    pub synced_at: u64,
    /// Unresolved MQ call sites — `(service:path, line)`, candidates for topics.toml.
    pub unresolved: Vec<(String, usize)>,
    /// Per SQL service: was an `ALL_DEPENDENCIES` dd dump present at sync?
    /// A `false` here means Calls/Touches edges for that service are missing,
    /// so an empty `get_impact` there must not be read as "nothing depends on it".
    pub sql_coverage: Vec<ServiceCoverage>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServiceCoverage {
    pub service: String,
    pub has_dd_dump: bool,
}

/// Graph adjacency. `out[i]` / `inc[i]` hold edge indices per node.
/// Skipped during (de)serialization; rebuilt by `Graph::build`.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Graph {
    pub out: Vec<Vec<EdgeId>>,
    pub inc: Vec<Vec<EdgeId>>,
}

impl Graph {
    pub fn build(node_count: usize, edges: &[Edge]) -> Self {
        let mut out = vec![Vec::new(); node_count];
        let mut inc = vec![Vec::new(); node_count];
        for e in edges {
            if e.from < node_count {
                out[e.from].push(e.id);
            }
            if e.to < node_count {
                inc[e.to].push(e.id);
            }
        }
        Self { out, inc }
    }
}
