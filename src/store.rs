//! Binary store persistence (bincode 3, serde feature) + lookup helpers.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::model::{Edge, Graph, Node, NodeId, NodeKind, STORE_VERSION, Store};

impl Store {
    /// Load from `path`, rebuild adjacency, validate version.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            anyhow::bail!(
                "store not found at {} — run `propagator sync` first",
                path.display()
            );
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("read store {}", path.display()))?;
        let mut store: Store = bincode::deserialize(&bytes)
            .with_context(|| format!("decode store {}", path.display()))?;
        if store.version != STORE_VERSION {
            anyhow::bail!(
                "store version mismatch: file={}, expected={}. Re-run `propagator sync`.",
                store.version,
                STORE_VERSION
            );
        }
        store.graph = Graph::build(store.nodes.len(), &store.edges);
        Ok(store)
    }

    /// Serialize to `path` (creates parent dirs).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let bytes = bincode::serialize(self).context("encode store")?;
        std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Find a node by canonical key (`SCHEMA.NAME` for SQL, bare name otherwise).
    /// Case-insensitive on the bare-name form for SQL to be forgiving, but the
    /// stored key stays canonical UPPERCASE per `plan.md` §5.
    #[must_use]
    pub fn find_node(&self, query: &str) -> Option<&Node> {
        // Exact key match first.
        if let Some(n) = self
            .nodes
            .iter()
            .find(|n| n.key().eq_ignore_ascii_case(query))
        {
            return Some(n);
        }
        // Bare name match (for topic/service, or unqualified SQL name).
        self.nodes
            .iter()
            .find(|n| n.name.eq_ignore_ascii_case(query))
    }

    /// Closest node keys to `query` for a not-found response. Share-token
    /// scoring (`plan.md` §8): token overlap dominates, substring and shared
    /// prefix break ties. Returns up to `limit` keys, best first.
    #[must_use]
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<String> {
        let q = query.to_ascii_uppercase();
        let qtok = tokenize(&q);
        let mut scored: Vec<(i32, String)> = Vec::new();
        for n in &self.nodes {
            let key = n.key();
            let ku = key.to_ascii_uppercase();
            let shared = tokenize(&ku).iter().filter(|t| qtok.contains(*t)).count() as i32;
            let mut score = shared * 4;
            if ku.contains(&q) || q.contains(&ku) {
                score += 3;
            }
            score += common_prefix_len(ku.as_bytes(), q.as_bytes()).min(6) as i32;
            if score > 0 {
                scored.push((score, key));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.dedup_by(|a, b| a.1 == b.1);
        scored.into_iter().take(limit).map(|(_, k)| k).collect()
    }

    /// `find_node`, or a formatted `not found` line carrying closest matches.
    pub fn find_or_suggest(&self, query: &str) -> std::result::Result<&Node, String> {
        match self.find_node(query) {
            Some(n) => Ok(n),
            None => {
                let sug = self.suggest(query, 5);
                if sug.is_empty() {
                    Err(format!("not found: {query}"))
                } else {
                    Err(format!("not found: {query} — closest: {}", sug.join(", ")))
                }
            }
        }
    }

    #[must_use]
    pub fn node_kind_count(&self) -> HashMap<NodeKind, usize> {
        let mut m = HashMap::new();
        for n in &self.nodes {
            *m.entry(n.kind).or_insert(0) += 1;
        }
        m
    }

    /// Edges incident to `n` in a given direction (`true` = outgoing).
    #[must_use]
    pub fn incident(&self, n: NodeId, outgoing: bool) -> Vec<&Edge> {
        let idxs = if outgoing {
            &self.graph.out
        } else {
            &self.graph.inc
        };
        idxs.get(n)
            .map(|v| v.iter().map(|&id| &self.edges[id]).collect())
            .unwrap_or_default()
    }
}

/// Split an identifier into uppercase alphanumeric tokens (on `_ . -` and
/// other non-alphanumerics). `USP_NEW_ORDER` → `[USP, NEW, ORDER]`.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_uppercase())
        .collect()
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EdgeKind, Node};

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("prop_store_{}_{name}.bin", std::process::id()))
    }

    fn sample() -> Store {
        let nodes = vec![
            Node {
                id: 0,
                name: "A".into(),
                schema: None,
                kind: NodeKind::Service,
                chunk: None,
            },
            Node {
                id: 1,
                name: "t".into(),
                schema: None,
                kind: NodeKind::Topic,
                chunk: None,
            },
        ];
        let edges = vec![Edge {
            id: 0,
            from: 0,
            to: 1,
            kind: EdgeKind::Publishes,
            path: "x".into(),
            line: 1,
            provenance: Vec::new(),
        }];
        Store {
            version: STORE_VERSION,
            chunks: Vec::new(),
            graph: Graph::build(2, &edges),
            nodes,
            edges,
            meta: Default::default(),
        }
    }

    #[test]
    fn roundtrip_rebuilds_graph() {
        let p = tmp("roundtrip");
        sample().save(&p).unwrap();
        let loaded = Store::load(&p).unwrap();
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.edges.len(), 1);
        // graph is #[serde(skip)] — must be rebuilt on load.
        assert_eq!(loaded.graph.out[0], vec![0]);
        assert_eq!(loaded.graph.inc[1], vec![0]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let p = tmp("version");
        let mut s = sample();
        s.version = STORE_VERSION + 1;
        s.save(&p).unwrap();
        assert!(Store::load(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn find_node_prefers_canonical_key() {
        let s = sample();
        assert!(s.find_node("t").is_some());
        assert!(s.find_node("A").is_some());
        assert!(s.find_node("zzz").is_none());
    }

    fn proc(id: usize, name: &str) -> Node {
        Node {
            id,
            name: name.into(),
            schema: Some("OMS".into()),
            kind: NodeKind::Procedure,
            chunk: None,
        }
    }

    fn store_with_procs(names: &[&str]) -> Store {
        let nodes = names.iter().enumerate().map(|(i, n)| proc(i, n)).collect();
        Store {
            version: STORE_VERSION,
            chunks: Vec::new(),
            graph: Graph::default(),
            nodes,
            edges: Vec::new(),
            meta: Default::default(),
        }
    }

    #[test]
    fn suggest_ranks_by_shared_tokens() {
        let s = store_with_procs(&["USP_NEW_ORDER", "USP_NEW_ORDER_V16", "SPI_CHECKBUYLIMIT"]);
        let sug = s.suggest("USP_NEW_ORDR", 5);
        // Both USP_NEW_ORDER* share NEW+USP tokens; unrelated proc ranks last/absent.
        assert_eq!(sug[0], "OMS.USP_NEW_ORDER");
        assert!(sug.contains(&"OMS.USP_NEW_ORDER_V16".to_string()));
        assert!(!sug.contains(&"OMS.SPI_CHECKBUYLIMIT".to_string()));
    }

    #[test]
    fn find_or_suggest_carries_closest_on_miss() {
        let s = store_with_procs(&["USP_NEW_ORDER"]);
        let err = s.find_or_suggest("USP_NEW_ORDR").unwrap_err();
        assert!(err.starts_with("not found: USP_NEW_ORDR — closest:"));
        assert!(err.contains("USP_NEW_ORDER"));
    }

    #[test]
    fn find_or_suggest_ok_on_hit() {
        let s = store_with_procs(&["USP_NEW_ORDER"]);
        assert!(s.find_or_suggest("usp_new_order").is_ok());
    }
}
