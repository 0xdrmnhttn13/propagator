//! Sync orchestration: run extractors → intern nodes → resolve edges → persist.
//! See `plan.md` §6.3 (registry merge), §10 Wk1.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{Config, SourceKind, TopicsRegistry};
use crate::extract::{self, RawDef, RawDefKind, mq, owns, sql};
use crate::model::{
    Chunk, ChunkKind, Edge, EdgeId, EdgeKind, Graph, Meta, Node, NodeId, NodeKind, STORE_VERSION,
    ServiceCoverage, Store,
};

/// Optional data-dictionary dump path per SQL source, set via config-derived
/// convention `<store-dir>/<service>-dependencies.csv`.
pub struct SyncReport {
    pub services: usize,
    pub defs: usize,
    pub edges: usize,
    /// Call sites whose topic arg couldn't be resolved to a literal —
    /// candidates for `topics.toml`. `(service:path, line)`.
    pub unresolved: Vec<(String, usize)>,
    /// `Invokes` edges dropped because the target wasn't a real proc/func node
    /// (regex literal noise: env vars, SQL keywords, error constants).
    pub invokes_dropped: usize,
    /// Embedded-SQL table refs that were a runtime placeholder (`%s`, `$1`) —
    /// counted, not edged. Surfaced so a coverage hole in code SQL is declared.
    pub code_dynamic_tables: usize,
    /// Redis key args that couldn't be resolved to a literal-anchored pattern —
    /// counted, not edged (honesty ledger).
    pub redis_dynamic_keys: usize,
}

/// Run a full sync into `store_path`.
pub fn run(cfg: &Config) -> Result<(Store, SyncReport)> {
    let mut store = Store {
        version: STORE_VERSION,
        ..Default::default()
    };
    let mut key_index: HashMap<String, NodeId> = HashMap::new();
    let mut bare_index: HashMap<String, NodeId> = HashMap::new();
    let mut all_defs: Vec<RawDef> = Vec::new();
    let mut raw_edges: Vec<extract::RawEdge> = Vec::new();
    let mut unresolved: Vec<(String, usize)> = Vec::new();
    let mut sql_coverage: Vec<ServiceCoverage> = Vec::new();
    let mut code_dynamic_tables = 0usize;
    let mut redis_dynamic_keys = 0usize;

    // --- Phase 1: SQL defs (per source) + oracle_dd edges (if dump present) ---
    let store_dir = cfg.store.path.parent().unwrap_or_else(|| Path::new("."));
    for src in &cfg.sources {
        if src.kind != SourceKind::Sql {
            continue;
        }
        let mut ex = sql::extract(&src.path, &src.service)?;
        raw_edges.extend(owns::owns_edges(&src.service, &ex.defs));
        raw_edges.extend(ex.edges.drain(..));
        all_defs.extend(ex.defs.drain(..));
        // oracle_dd dump (optional). Its presence is the Calls/Touches coverage
        // signal reported by describe_corpus.
        let dump = store_dir.join(format!("{}-dependencies.csv", slug(&src.service)));
        let has_dd_dump = dump.exists();
        if has_dd_dump {
            let dd = crate::extract::oracle_dd::extract_from_dump(&dump, &src.service)?;
            unresolved.extend(dd.unresolved_topics);
            raw_edges.extend(dd.edges);
        }
        if !sql_coverage.iter().any(|c| c.service == src.service) {
            sql_coverage.push(ServiceCoverage {
                service: src.service.clone(),
                has_dd_dump,
            });
        }
    }

    // Intern SQL proc/function nodes + chunks.
    for def in &all_defs {
        let chunk_id = store.chunks.len();
        let ckind = match def.kind {
            RawDefKind::Proc => ChunkKind::SqlProc,
            RawDefKind::Function => ChunkKind::SqlFunction,
        };
        store.chunks.push(Chunk {
            id: chunk_id,
            service: def.service.clone(),
            path: def.path.clone(),
            kind: ckind,
            title: def.title.clone(),
            line_start: def.line_start,
            line_end: def.line_end,
            signature: def.signature.clone(),
            body: def.body.clone(),
            node: None,
        });
        let nkind = match def.kind {
            RawDefKind::Proc => NodeKind::Procedure,
            RawDefKind::Function => NodeKind::Function,
        };
        let node_id = intern_def(
            &mut store,
            &mut key_index,
            &mut bare_index,
            def,
            nkind,
            Some(chunk_id),
        );
        store.chunks[chunk_id].node = Some(node_id);
    }

    // --- Phase 2: code sources — MQ + Invokes ---
    for src in &cfg.sources {
        if src.kind != SourceKind::Code {
            continue;
        }
        // ensure service node exists.
        intern(
            &mut store,
            &mut key_index,
            &mut bare_index,
            &src.service,
            NodeKind::Service,
            None,
        );
        let mq_ex = mq::extract(&src.path, &src.service)?;
        unresolved.extend(mq_ex.unresolved_topics);
        raw_edges.extend(mq_ex.edges);
        let inv = extract::invokes::extract(&src.path, &src.service)?;
        raw_edges.extend(inv.edges);
        // Embedded-SQL: Touches (service→table) + precise Invokes from real SQL.
        let code_ex = extract::code::extract(&src.path, &src.service)?;
        code_dynamic_tables += code_ex.dynamic_sql_sites;
        redis_dynamic_keys += code_ex.dynamic_redis_keys;
        raw_edges.extend(code_ex.edges);
    }

    // --- Phase 3: topics.toml registry merge ---
    // `Config::load` guarantees `topics` is set (defaults next to the config).
    let topics_path = cfg
        .topics
        .clone()
        .unwrap_or_else(|| PathBuf::from("topics.toml"));
    let registry = TopicsRegistry::load(&topics_path)?;
    // Producer-provenance declarations: (service, topic) -> feeding table names.
    // Attached to the resolved `Publishes` edge in Phase 4.5.
    let mut prov_decls: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (svc, edges) in &registry.services {
        for (topic, tables) in &edges.provenance {
            if !tables.is_empty() {
                prov_decls.insert((svc.clone(), topic.clone()), tables.clone());
            }
        }
    }
    // Registry only fills holes (`plan.md` §6.3): skip pairs regex already found.
    let seen: std::collections::HashSet<(EdgeKind, String, String)> = raw_edges
        .iter()
        .filter(|e| matches!(e.kind, EdgeKind::Publishes | EdgeKind::Consumes))
        .map(|e| (e.kind, e.from.clone(), e.to.clone()))
        .collect();
    for (svc, edges) in &registry.services {
        intern(
            &mut store,
            &mut key_index,
            &mut bare_index,
            svc,
            NodeKind::Service,
            None,
        );
        for (kind, topics) in [
            (EdgeKind::Publishes, &edges.publishes),
            (EdgeKind::Consumes, &edges.consumes),
        ] {
            for t in topics {
                if seen.contains(&(kind, svc.clone(), t.clone())) {
                    continue;
                }
                raw_edges.push(extract::RawEdge {
                    kind,
                    from: svc.clone(),
                    to: t.clone(),
                    path: "<registry>".into(),
                    line: 0,
                    from_kind: None,
                });
            }
        }
    }

    // --- Phase 4: resolve raw edges to NodeIds ---
    // Two passes. Pass 1 resolves every edge except `Invokes`, interning all real
    // procedure/function nodes (SQL defs + dd dump). Pass 2 admits an `Invokes`
    // edge only if its target already exists as a proc/func — dropping the
    // service→literal noise the regex scan can't distinguish from a real call
    // (env vars, SQL keywords, error constants). See plan-extractors.md Fase 0.
    let mut edge_id: EdgeId = 0;
    let mut seen: HashSet<(EdgeKind, NodeId, NodeId)> = HashSet::new();
    let mut invokes_dropped = 0usize;
    for invokes_pass in [false, true] {
        for re in &raw_edges {
            if (re.kind == EdgeKind::Invokes) != invokes_pass {
                continue;
            }
            if re.kind == EdgeKind::Invokes
                && !is_known_proc(&store, &key_index, &bare_index, &re.to)
            {
                invokes_dropped += 1;
                continue;
            }
            // `code.rs` stamps `from_kind = Service` for embedded-SQL `Touches`
            // (a service reaching a table from application code); otherwise infer.
            let from_kind = re.from_kind.unwrap_or(match re.kind {
                EdgeKind::Publishes
                | EdgeKind::Consumes
                | EdgeKind::Owns
                | EdgeKind::Invokes
                | EdgeKind::ReadsKey
                | EdgeKind::WritesKey => NodeKind::Service,
                _ => NodeKind::Procedure,
            });
            let to_kind = match re.kind {
                EdgeKind::Touches => NodeKind::Table,
                EdgeKind::Publishes | EdgeKind::Consumes => NodeKind::Topic,
                EdgeKind::ReadsKey | EdgeKind::WritesKey => NodeKind::RedisKey,
                EdgeKind::Owns | EdgeKind::Invokes | EdgeKind::Calls => NodeKind::Procedure,
            };
            let from_id = intern(
                &mut store,
                &mut key_index,
                &mut bare_index,
                &re.from,
                from_kind,
                None,
            );
            let to_id = intern(
                &mut store,
                &mut key_index,
                &mut bare_index,
                &re.to,
                to_kind,
                None,
            );
            // Skip degenerate self-edges and duplicates (e.g. dynamic-call scan
            // finding the same Calls edge that oracle_dd already provided).
            if from_id == to_id || !seen.insert((re.kind, from_id, to_id)) {
                continue;
            }
            store.edges.push(Edge {
                id: edge_id,
                from: from_id,
                to: to_id,
                kind: re.kind,
                path: re.path.clone(),
                line: re.line,
                provenance: Vec::new(),
            });
            edge_id += 1;
        }
    }

    // --- Phase 4.5: attach producer-provenance to Publishes edges ---
    // For each declared (service, topic), resolve the feeding table names to
    // existing Table NodeIds and stamp them onto the matching Publishes edge.
    // Names that don't resolve to a real Table node are dropped (no phantoms).
    // Computed read-only first to avoid borrowing `store.nodes` while mutating
    // `store.edges`.
    if !prov_decls.is_empty() {
        let mut assign: Vec<(usize, Vec<NodeId>)> = Vec::new();
        for (i, e) in store.edges.iter().enumerate() {
            if e.kind != EdgeKind::Publishes {
                continue;
            }
            let svc = store.nodes[e.from].name.clone();
            let topic = store.nodes[e.to].name.clone();
            let Some(tables) = prov_decls.get(&(svc, topic)) else {
                continue;
            };
            let ids: Vec<NodeId> = tables
                .iter()
                .filter_map(|t| resolve_table(&store, &key_index, &bare_index, t))
                .collect();
            if !ids.is_empty() {
                assign.push((i, ids));
            }
        }
        for (i, ids) in assign {
            store.edges[i].provenance = ids;
        }
    }

    store.graph = Graph::build(store.nodes.len(), &store.edges);

    store.meta = Meta {
        synced_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        unresolved: unresolved.clone(),
        sql_coverage,
    };

    let report = SyncReport {
        services: store
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Service)
            .count(),
        defs: store.chunks.len(),
        edges: store.edges.len(),
        unresolved,
        invokes_dropped,
        code_dynamic_tables,
        redis_dynamic_keys,
    };
    Ok((store, report))
}

/// Does `name` already resolve to a real Procedure/Function node? Used to gate
/// `Invokes` edges so service→literal noise (env vars, SQL keywords, error
/// constants) never becomes a phantom proc. Interning must be complete for all
/// real procs before this is called (Phase 4 pass 1). See plan-extractors.md Fase 0.
/// Resolve a (possibly `SCHEMA.`-qualified) table name to an existing Table
/// NodeId. Returns `None` if the name isn't a known Table node — so a typo or a
/// table outside the corpus is dropped rather than interned as a phantom.
/// Used to ground `topics.toml` provenance declarations (Phase 4.5).
fn resolve_table(
    store: &Store,
    key_index: &HashMap<String, NodeId>,
    bare_index: &HashMap<String, NodeId>,
    name: &str,
) -> Option<NodeId> {
    let up = name.to_ascii_uppercase();
    let bare = up.rsplit('.').next().unwrap_or(&up).to_string();
    key_index
        .get(&up)
        .or_else(|| bare_index.get(&bare))
        .copied()
        .filter(|&id| store.nodes[id].kind == NodeKind::Table)
}

fn is_known_proc(
    store: &Store,
    key_index: &HashMap<String, NodeId>,
    bare_index: &HashMap<String, NodeId>,
    name: &str,
) -> bool {
    let bare = name.rsplit('.').next().unwrap_or(name).to_ascii_uppercase();
    key_index
        .get(&name.to_ascii_uppercase())
        .or_else(|| bare_index.get(&bare))
        .map(|&id| {
            matches!(
                store.nodes[id].kind,
                NodeKind::Procedure | NodeKind::Function
            )
        })
        .unwrap_or(false)
}

/// Intern a definition node (has a chunk). Canonical key `SCHEMA.NAME`.
#[allow(clippy::too_many_arguments)]
fn intern_def(
    store: &mut Store,
    key_index: &mut HashMap<String, NodeId>,
    bare_index: &mut HashMap<String, NodeId>,
    def: &RawDef,
    kind: NodeKind,
    chunk: Option<usize>,
) -> NodeId {
    let schema = def.schema.clone();
    let name = def.title.to_ascii_uppercase();
    let key = match &schema {
        Some(s) => format!("{}.{}", s.to_ascii_uppercase(), name),
        None => name.clone(),
    };
    if let Some(&id) = key_index.get(&key) {
        return id;
    }
    let id = store.nodes.len();
    store.nodes.push(Node {
        id,
        name,
        schema,
        kind,
        chunk,
    });
    key_index.insert(key.clone(), id);
    bare_index
        .entry(def.title.to_ascii_uppercase())
        .or_insert(id);
    id
}

/// Intern a generic node by string key. Creates if missing.
///
/// Identity is kind-aware (`plan.md` §5): Service/Topic keys are case-sensitive
/// as written and never schema-split (Kafka topics may contain dots); SQL kinds
/// are uppercased, a `SCHEMA.NAME` prefix is split off, and lookups fall back
/// to the bare name so dictionary edges resolve to extracted defs.
fn intern(
    store: &mut Store,
    key_index: &mut HashMap<String, NodeId>,
    bare_index: &mut HashMap<String, NodeId>,
    key: &str,
    kind: NodeKind,
    chunk: Option<usize>,
) -> NodeId {
    if matches!(
        kind,
        NodeKind::Service | NodeKind::Topic | NodeKind::RedisKey
    ) {
        if let Some(&id) = key_index.get(key) {
            return id;
        }
        let id = store.nodes.len();
        store.nodes.push(Node {
            id,
            name: key.to_string(),
            schema: None,
            kind,
            chunk,
        });
        key_index.insert(key.to_string(), id);
        return id;
    }

    let (schema, bare) = match key.split_once('.') {
        Some((s, n)) => (Some(s.to_ascii_uppercase()), n.to_ascii_uppercase()),
        None => (None, key.to_ascii_uppercase()),
    };
    let canon = match &schema {
        Some(s) => format!("{s}.{bare}"),
        None => bare.clone(),
    };
    if let Some(&id) = key_index.get(&canon) {
        return id;
    }
    if let Some(&id) = bare_index.get(&bare) {
        return id;
    }
    let id = store.nodes.len();
    store.nodes.push(Node {
        id,
        name: bare.clone(),
        schema,
        kind,
        chunk,
    });
    key_index.insert(canon, id);
    bare_index.entry(bare).or_insert(id);
    id
}

fn slug(s: &str) -> String {
    s.replace(|c: char| !c.is_alphanumeric() && c != '-', "_")
}

#[allow(dead_code)]
fn ensure_dir(p: &Path) -> Result<PathBuf> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    Ok(p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SourceCfg, StoreCfg};

    /// Full sync over the fixture corpus (cwd = crate root under cargo test).
    fn fixture_cfg() -> Config {
        let src = |kind, path: &str, service: &str| SourceCfg {
            kind,
            path: PathBuf::from(path),
            service: service.to_string(),
        };
        Config {
            store: StoreCfg {
                path: PathBuf::from(".propagator/store.bin"),
            },
            sources: vec![
                src(SourceKind::Sql, "fixtures/sql", "oms-core"),
                src(SourceKind::Code, "fixtures/go", "risk-engine"),
                src(SourceKind::Code, "fixtures/rust", "order-gateway"),
                src(SourceKind::Code, "fixtures/cpp", "clearing-bridge"),
            ],
            topics: Some(PathBuf::from("topics.toml")),
        }
    }

    fn edge_exists(store: &Store, kind: EdgeKind, from: &str, to: &str) -> bool {
        store.edges.iter().any(|e| {
            e.kind == kind
                && store.nodes[e.from].name.eq_ignore_ascii_case(from)
                && store.nodes[e.to].name.eq_ignore_ascii_case(to)
        })
    }

    #[test]
    fn full_sync_builds_cross_world_graph() {
        let (store, report) = run(&fixture_cfg()).unwrap();

        // Topic identity stays lowercase as written (plan §5).
        let t = store.find_node("order-events").unwrap();
        assert_eq!(t.name, "order-events");
        assert_eq!(t.kind, NodeKind::Topic);

        assert!(edge_exists(
            &store,
            EdgeKind::Publishes,
            "order-gateway",
            "order-events"
        ));
        assert!(edge_exists(
            &store,
            EdgeKind::Consumes,
            "risk-engine",
            "order-events"
        ));
        assert!(edge_exists(
            &store,
            EdgeKind::Invokes,
            "risk-engine",
            "USP_NEW_ORDER_V16"
        ));
        // oracle-dd edges resolve to the def node, not a schema-prefixed twin.
        assert!(edge_exists(
            &store,
            EdgeKind::Calls,
            "USP_NEW_ORDER_V16",
            "SPI_CHECKBUYLIMIT"
        ));
        // Owns floor exists for every SQL def.
        assert!(edge_exists(
            &store,
            EdgeKind::Owns,
            "oms-core",
            "USP_NEW_ORDER_V16"
        ));
        // Registry-only service merged in.
        assert!(edge_exists(
            &store,
            EdgeKind::Consumes,
            "settlement-svc",
            "settlements"
        ));

        // The dd Calls edge attaches to the node that owns the extracted chunk.
        let usp = store.find_node("USP_NEW_ORDER_V16").unwrap();
        assert!(usp.chunk.is_some());
        assert_eq!(
            store
                .nodes
                .iter()
                .filter(|n| n.name == "USP_NEW_ORDER_V16")
                .count(),
            1,
            "dd edge must not create a duplicate node"
        );

        assert_eq!(report.unresolved.len(), 1); // the Sprintf topic
        assert_eq!(report.defs, 3);
    }

    #[test]
    fn registry_does_not_duplicate_regex_edges() {
        let (store, _) = run(&fixture_cfg()).unwrap();
        let n = store
            .edges
            .iter()
            .filter(|e| {
                e.kind == EdgeKind::Consumes
                    && store.nodes[e.from].name == "risk-engine"
                    && store.nodes[e.to].name == "order-events"
            })
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn intern_is_kind_aware() {
        let mut store = Store::default();
        let mut ki = HashMap::new();
        let mut bi = HashMap::new();
        // Dotted topic stays whole and case-sensitive.
        let t = intern(
            &mut store,
            &mut ki,
            &mut bi,
            "orders.v1",
            NodeKind::Topic,
            None,
        );
        assert_eq!(store.nodes[t].name, "orders.v1");
        assert_eq!(store.nodes[t].schema, None);
        // SQL key splits schema and uppercases.
        let p = intern(
            &mut store,
            &mut ki,
            &mut bi,
            "oms.usp_x",
            NodeKind::Procedure,
            None,
        );
        assert_eq!(store.nodes[p].name, "USP_X");
        assert_eq!(store.nodes[p].schema.as_deref(), Some("OMS"));
        // Bare-name lookup resolves to the same node.
        let p2 = intern(
            &mut store,
            &mut ki,
            &mut bi,
            "USP_X",
            NodeKind::Procedure,
            None,
        );
        assert_eq!(p, p2);
    }

    #[test]
    fn full_sync_invokes_only_real_procs() {
        let (store, report) = run(&fixture_cfg()).unwrap();
        // Every Invokes edge must land on a real proc/func node — no phantom
        // targets from the literal scan.
        for e in store.edges.iter().filter(|e| e.kind == EdgeKind::Invokes) {
            assert!(
                matches!(
                    store.nodes[e.to].kind,
                    NodeKind::Procedure | NodeKind::Function
                ),
                "Invokes target {} is not a proc/func",
                store.nodes[e.to].name
            );
        }
        // Real one survived; report exposes the drop counter (0+ is fine here —
        // fixtures are clean, but the field must be wired).
        assert!(edge_exists(
            &store,
            EdgeKind::Invokes,
            "risk-engine",
            "USP_NEW_ORDER_V16"
        ));
        let _ = report.invokes_dropped;
    }

    #[test]
    fn is_known_proc_gates_noise() {
        let mut store = Store::default();
        store.nodes.push(Node {
            id: 0,
            name: "USP_NEW_ORDER_V16".into(),
            schema: Some("OMS".into()),
            kind: NodeKind::Procedure,
            chunk: None,
        });
        store.nodes.push(Node {
            id: 1,
            name: "TORDER".into(),
            schema: Some("OMS".into()),
            kind: NodeKind::Table,
            chunk: None,
        });
        let mut key_index = HashMap::new();
        key_index.insert("OMS.USP_NEW_ORDER_V16".to_string(), 0usize);
        key_index.insert("OMS.TORDER".to_string(), 1usize);
        let mut bare_index = HashMap::new();
        bare_index.insert("USP_NEW_ORDER_V16".to_string(), 0usize);
        bare_index.insert("TORDER".to_string(), 1usize);

        // Real proc, case-insensitive.
        assert!(is_known_proc(
            &store,
            &key_index,
            &bare_index,
            "usp_new_order_v16"
        ));
        // Noise the old scan would have admitted.
        assert!(!is_known_proc(
            &store,
            &key_index,
            &bare_index,
            "ACCESS_TOKEN_SECRET"
        ));
        assert!(!is_known_proc(&store, &key_index, &bare_index, "SELECT"));
        // A real object but wrong kind (table) is not a proc.
        assert!(!is_known_proc(&store, &key_index, &bare_index, "TORDER"));
    }
}
