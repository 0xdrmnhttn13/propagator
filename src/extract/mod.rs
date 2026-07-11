//! Source extractors. Each returns definitions/edges; `sync.rs` assembles the
//! final store. See `plan.md` §6.

pub mod code;
pub mod invokes;
pub mod mq;
pub mod oracle_dd;
pub mod owns;
pub mod redis;
pub mod sql;

use crate::model::{EdgeKind, NodeKind};

/// Result of scanning one source root. Edges reference node *names* (resolved
/// to NodeIds later in `sync.rs`) — keeps extractors decoupled from the store.
#[derive(Debug, Default)]
pub struct Extraction {
    /// `(name, schema, kind)` triples for procedure/function/table definitions.
    pub defs: Vec<RawDef>,
    /// Edges by endpoint names.
    pub edges: Vec<RawEdge>,
    /// Topic literals that couldn't be resolved to a string literal.
    pub unresolved_topics: Vec<(String, usize)>,
    /// Embedded-SQL table positions that resolved to a runtime placeholder
    /// (`FROM %s`, `$1`, `:tbl`) rather than a literal name — counted, not
    /// edged (honesty ledger). Only populated by `code.rs`.
    pub dynamic_sql_sites: usize,
    /// Redis key arguments that couldn't be resolved to a literal-anchored
    /// pattern (unresolved var, `*`-only format) — counted, not edged. Populated
    /// by `redis.rs`.
    pub dynamic_redis_keys: usize,
}

#[derive(Debug, Clone)]
pub struct RawDef {
    pub service: String,
    pub path: String,
    pub title: String,
    pub schema: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: String,
    pub body: String,
    pub kind: RawDefKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawDefKind {
    Proc,
    Function,
}

#[derive(Debug, Clone)]
pub struct RawEdge {
    pub kind: EdgeKind,
    pub from: String, // service name (for MQ/Owns/Invokes) or proc name
    pub to: String,   // topic name or proc/table name
    pub path: String,
    pub line: usize,
    /// Override for the `from` endpoint's node kind. `None` = infer from
    /// `kind` in `sync.rs` (SQL `Touches`/`Calls` → Procedure; MQ → Service).
    /// Set to `Some(Service)` by `code.rs` for embedded-SQL `Touches`, where a
    /// *service* (not a proc) reaches a table directly from application code.
    pub from_kind: Option<NodeKind>,
}
