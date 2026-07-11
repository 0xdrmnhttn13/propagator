//! Heterogeneous graph traversal (`plan.md` §7).
//!
//! The "dampak mengalir" impact table is encoded as `EdgeKind::impact_forward()`
//! (true only for `Publishes`); all other kinds flow backward (to→from). This
//! keeps the per-edge logic branch-free and the table testable.

use std::collections::HashSet;

use crate::model::{Edge, EdgeId, EdgeKind, NodeId, NodeKind, Store};

#[derive(Debug, Clone)]
pub struct Reach {
    pub node: NodeId,
    pub via: EdgeId,
    pub depth: usize,
}

pub const DEFAULT_CAP: usize = 200;

/// Generic BFS. `decide(cur, edge)` returns the node to move to (or `None` to
/// skip that edge). Cycle-safe; capped at `DEFAULT_CAP` results.
fn bfs<F>(store: &Store, start: NodeId, max_depth: usize, mut decide: F) -> Vec<Reach>
where
    F: FnMut(NodeId, &Edge) -> Option<NodeId>,
{
    let mut visited = vec![false; store.nodes.len()];
    let mut out = Vec::new();
    if start >= store.nodes.len() {
        return out;
    }
    visited[start] = true;
    let mut queue: std::collections::VecDeque<(NodeId, usize)> = std::collections::VecDeque::new();
    queue.push_back((start, 0));

    while let Some((cur, depth)) = queue.pop_front() {
        if depth >= max_depth || out.len() >= DEFAULT_CAP {
            break;
        }
        let incident = store
            .graph
            .out
            .get(cur)
            .into_iter()
            .flatten()
            .chain(store.graph.inc.get(cur).into_iter().flatten());
        for &eid in incident {
            let e = &store.edges[eid];
            if e.from == e.to {
                continue;
            }
            let Some(next) = decide(cur, e) else { continue };
            if next == cur || next >= store.nodes.len() || visited[next] {
                continue;
            }
            visited[next] = true;
            out.push(Reach {
                node: next,
                via: eid,
                depth: depth + 1,
            });
            queue.push_back((next, depth + 1));
            if out.len() >= DEFAULT_CAP {
                break;
            }
        }
    }
    out
}

fn weak_skipped(kind: crate::model::EdgeKind, include_weak: bool) -> bool {
    !include_weak && kind == crate::model::EdgeKind::Owns
}

/// SQL-world tables that a change at `start` actually originates from — used by
/// the producer-provenance guard. A Table start is its own origin; a
/// Procedure/Function start contributes the tables it directly `Touches`.
/// Anything else (service/topic) yields an empty set, which leaves the guard
/// inert (pure-MQ impact is unaffected).
fn sql_origin_tables(store: &Store, start: NodeId) -> HashSet<NodeId> {
    let mut set = HashSet::new();
    if start >= store.nodes.len() {
        return set;
    }
    match store.nodes[start].kind {
        NodeKind::Table => {
            set.insert(start);
        }
        NodeKind::Procedure | NodeKind::Function => {
            for &eid in store.graph.out.get(start).into_iter().flatten() {
                let e = &store.edges[eid];
                if e.kind == EdgeKind::Touches && e.from == start {
                    set.insert(e.to);
                }
            }
        }
        _ => {}
    }
    set
}

/// Impact traversal: "who is impacted if `start` changes." Uses the impact-flow
/// table — `Publishes` flows forward, everything else backward. `include_weak`
/// toggles `Owns` participation.
///
/// **Producer-provenance guard:** the Service node is a promiscuous hub — it
/// wires every proc a service owns/invokes to every topic it publishes, so a
/// naive BFS reports every consumer of every service topic as impacted by every
/// table that service touches. When a `Publishes` edge carries a non-empty
/// `provenance` (declared in `topics.toml`), we only cross it from a SQL-world
/// start if one of the origin tables actually feeds that payload. Topics without
/// a provenance declaration are unconstrained (behaves exactly as before).
pub fn impact(store: &Store, start: NodeId, max_depth: usize, include_weak: bool) -> Vec<Reach> {
    let origin_tables = sql_origin_tables(store, start);
    bfs(store, start, max_depth, |cur, e| {
        if weak_skipped(e.kind, include_weak) {
            return None;
        }
        // Guard: crossing SQL-world → topic via a provenance-annotated Publishes
        // edge whose payload carries none of the changed tables → payload doesn't
        // relate; don't propagate to the topic (and thus its consumers).
        if e.kind == EdgeKind::Publishes
            && e.from == cur
            && !origin_tables.is_empty()
            && !e.provenance.is_empty()
            && !e.provenance.iter().any(|t| origin_tables.contains(t))
        {
            return None;
        }
        if e.kind.impact_forward() {
            (e.from == cur).then_some(e.to)
        } else {
            (e.to == cur).then_some(e.from)
        }
    })
}

/// Forward: "what does `start` depend on" — follows out-edges (cur==from → to).
pub fn downstream(
    store: &Store,
    start: NodeId,
    max_depth: usize,
    include_weak: bool,
) -> Vec<Reach> {
    bfs(store, start, max_depth, |cur, e| {
        if weak_skipped(e.kind, include_weak) {
            return None;
        }
        (e.from == cur).then_some(e.to)
    })
}

/// Reverse: "who references `start`" — follows in-edges (cur==to → from).
pub fn upstream(store: &Store, start: NodeId, max_depth: usize, include_weak: bool) -> Vec<Reach> {
    bfs(store, start, max_depth, |cur, e| {
        if weak_skipped(e.kind, include_weak) {
            return None;
        }
        (e.to == cur).then_some(e.from)
    })
}

/// Cross-world topic bridges: topics carrying both a producer (`Publishes`) and
/// a consumer (`Consumes`) edge. Returns `(bridges, guarded, coarse_names)` —
/// `guarded` = producer edge has non-empty `provenance`; `coarse_names` = the
/// rest, where impact may over-report consumers (promiscuous Service-hub). The
/// honesty counterpart to the sql-coverage signal.
pub fn topic_bridge_coverage(store: &Store) -> (usize, usize, Vec<String>) {
    let (mut bridges, mut guarded, mut coarse) = (0usize, 0usize, Vec::new());
    for n in store.nodes.iter().filter(|n| n.kind == NodeKind::Topic) {
        let (mut has_pub, mut has_con, mut has_prov) = (false, false, false);
        for &eid in store.graph.inc.get(n.id).into_iter().flatten() {
            let e = &store.edges[eid];
            match e.kind {
                EdgeKind::Publishes => {
                    has_pub = true;
                    has_prov |= !e.provenance.is_empty();
                }
                EdgeKind::Consumes => has_con = true,
                _ => {}
            }
        }
        if has_pub && has_con {
            bridges += 1;
            if has_prov {
                guarded += 1;
            } else {
                coarse.push(n.name.clone());
            }
        }
    }
    (bridges, guarded, coarse)
}

/// One-line human summary of `topic_bridge_coverage` for `describe_corpus` / CLI.
pub fn topic_bridge_summary(store: &Store) -> String {
    let (bridges, guarded, coarse) = topic_bridge_coverage(store);
    if bridges == 0 {
        "none".to_string()
    } else if coarse.is_empty() {
        format!("{bridges} producer→consumer, all provenance-guarded")
    } else {
        format!(
            "{bridges} producer→consumer; {guarded} guarded, {} COARSE (impact may over-report consumers — declare [svc.provenance] in topics.toml): [{}]",
            coarse.len(),
            coarse.join(", ")
        )
    }
}

/// Human-readable kind label for output formatting.
pub fn kind_label(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Procedure => "Procedure",
        NodeKind::Function => "Function",
        NodeKind::Table => "Table",
        NodeKind::Service => "Service",
        NodeKind::Topic => "Topic",
        NodeKind::RedisKey => "RedisKey",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EdgeKind, Graph, Node, Store};
    use proptest::prelude::*;

    /// Build a store with `n` nodes and the given `(from, to, kind)` edges.
    fn store_with(n: usize, spec: &[(usize, usize, EdgeKind)]) -> Store {
        let nodes = (0..n)
            .map(|id| Node {
                id,
                name: format!("N{id}"),
                schema: None,
                kind: NodeKind::Procedure,
                chunk: None,
            })
            .collect();
        let edges: Vec<Edge> = spec
            .iter()
            .enumerate()
            .map(|(id, &(from, to, kind))| Edge {
                id,
                from,
                to,
                kind,
                path: String::new(),
                line: 0,
                provenance: Vec::new(),
            })
            .collect();
        let graph = Graph::build(n, &edges);
        Store {
            version: 1,
            chunks: Vec::new(),
            nodes,
            edges,
            graph,
            meta: Default::default(),
        }
    }

    fn reached(reaches: &[Reach]) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = reaches.iter().map(|r| r.node).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn diamond_visits_each_node_once() {
        // 0→1, 0→2, 1→3, 2→3 (Calls). Downstream(0) = {1,2,3}, node 3 once.
        let s = store_with(
            4,
            &[
                (0, 1, EdgeKind::Calls),
                (0, 2, EdgeKind::Calls),
                (1, 3, EdgeKind::Calls),
                (2, 3, EdgeKind::Calls),
            ],
        );
        let r = downstream(&s, 0, 10, false);
        assert_eq!(reached(&r), vec![1, 2, 3]);
    }

    #[test]
    fn cycle_terminates() {
        // 0→1→2→0 recursive proc chain.
        let s = store_with(
            3,
            &[
                (0, 1, EdgeKind::Calls),
                (1, 2, EdgeKind::Calls),
                (2, 0, EdgeKind::Calls),
            ],
        );
        assert_eq!(reached(&upstream(&s, 0, 100, false)), vec![1, 2]);
        assert_eq!(reached(&downstream(&s, 0, 100, false)), vec![1, 2]);
        assert_eq!(reached(&impact(&s, 0, 100, false)), vec![1, 2]);
    }

    #[test]
    fn self_loop_terminates() {
        let s = store_with(2, &[(0, 0, EdgeKind::Calls), (0, 1, EdgeKind::Calls)]);
        assert_eq!(reached(&downstream(&s, 0, 100, false)), vec![1]);
    }

    #[test]
    fn impact_crosses_worlds() {
        // 0=proc TORDER-toucher chain start: proc1 Calls proc0 (impact 0 → 1),
        // svc2 Invokes proc0, svc2 Publishes topic3, svc4 Consumes topic3.
        // impact(proc0) must walk: 1 (caller), 2 (invoker), 3 (its topic), 4 (consumer).
        let s = store_with(
            5,
            &[
                (1, 0, EdgeKind::Calls),
                (2, 0, EdgeKind::Invokes),
                (2, 3, EdgeKind::Publishes),
                (4, 3, EdgeKind::Consumes),
            ],
        );
        let r = impact(&s, 0, 10, false);
        assert_eq!(reached(&r), vec![1, 2, 3, 4]);
        // Depths: callers/invokers at d1, topic at d2, consumer at d3.
        let depth_of = |n: NodeId| r.iter().find(|x| x.node == n).unwrap().depth;
        assert_eq!(depth_of(2), 1);
        assert_eq!(depth_of(3), 2);
        assert_eq!(depth_of(4), 3);
    }

    #[test]
    fn impact_of_topic_reaches_consumers_not_producers() {
        // svc0 Publishes topic1; svc2 Consumes topic1.
        let s = store_with(
            3,
            &[(0, 1, EdgeKind::Publishes), (2, 1, EdgeKind::Consumes)],
        );
        assert_eq!(reached(&impact(&s, 1, 10, false)), vec![2]);
    }

    /// Producer-provenance guard: a table change must NOT reach a topic's
    /// consumers when the topic's payload doesn't carry that table — even though
    /// the producing service touches the table and publishes the topic (the
    /// promiscuous Service-hub false positive). Mirrors the real
    /// TSTOCKINFO_DAYTRADE → autoorderservice case.
    fn provenance_store(topic_provenance: Vec<NodeId>) -> Store {
        // 0=TSTOCKINFO_DAYTRADE(Table) 1=SPI(Proc) 2=rms(Service)
        // 3=POSTRMS(Topic) 4=autoorder(Service) 5=TORDER(Table)
        let kinds = [
            NodeKind::Table,
            NodeKind::Procedure,
            NodeKind::Service,
            NodeKind::Topic,
            NodeKind::Service,
            NodeKind::Table,
        ];
        let nodes = kinds
            .iter()
            .enumerate()
            .map(|(id, &kind)| Node {
                id,
                name: format!("N{id}"),
                schema: None,
                kind,
                chunk: None,
            })
            .collect();
        let mk = |id: usize, from, to, kind, provenance| Edge {
            id,
            from,
            to,
            kind,
            path: String::new(),
            line: 0,
            provenance,
        };
        let edges = vec![
            mk(0, 1, 0, EdgeKind::Touches, vec![]), // SPI Touches TSTOCKINFO_DAYTRADE
            mk(1, 2, 1, EdgeKind::Invokes, vec![]), // rms Invokes SPI
            mk(2, 2, 3, EdgeKind::Publishes, topic_provenance), // rms Publishes POSTRMS
            mk(3, 4, 3, EdgeKind::Consumes, vec![]), // autoorder Consumes POSTRMS
        ];
        let graph = Graph::build(6, &edges);
        Store {
            version: crate::model::STORE_VERSION,
            chunks: Vec::new(),
            nodes,
            edges,
            graph,
            meta: Default::default(),
        }
    }

    #[test]
    fn provenance_blocks_unrelated_topic_crossing() {
        // POSTRMS fed only by TORDER(5) — a change to TSTOCKINFO_DAYTRADE(0)
        // must not reach the topic(3) nor its consumer autoorder(4).
        let s = provenance_store(vec![5]);
        let r = reached(&impact(&s, 0, 10, false));
        assert_eq!(r, vec![1, 2], "topic + consumer must be pruned");
        assert!(!r.contains(&4), "autoorder is a false positive here");
    }

    #[test]
    fn provenance_allows_related_and_unannotated_topic() {
        // If the payload DOES carry the changed table → cross as normal.
        let related = reached(&impact(&provenance_store(vec![0]), 0, 10, false));
        assert_eq!(related, vec![1, 2, 3, 4]);
        // Empty provenance = unknown → unconstrained (old behavior preserved).
        let unannotated = reached(&impact(&provenance_store(vec![]), 0, 10, false));
        assert_eq!(unannotated, vec![1, 2, 3, 4]);
    }

    #[test]
    fn topic_bridge_coverage_flags_unprovenanced() {
        // POSTRMS(3) has producer rms(2) + consumer autoorder(4): a bridge.
        // With provenance → guarded; without → coarse.
        let (b, g, coarse) = topic_bridge_coverage(&provenance_store(vec![5]));
        assert_eq!((b, g), (1, 1));
        assert!(coarse.is_empty());
        let (b, g, coarse) = topic_bridge_coverage(&provenance_store(vec![]));
        assert_eq!((b, g), (1, 0));
        assert_eq!(coarse, vec!["N3".to_string()]);
    }

    #[test]
    fn weak_owns_excluded_unless_requested() {
        // svc0 Owns proc1.
        let s = store_with(2, &[(0, 1, EdgeKind::Owns)]);
        assert!(impact(&s, 1, 10, false).is_empty());
        assert_eq!(reached(&impact(&s, 1, 10, true)), vec![0]);
    }

    #[test]
    fn depth_limits_traversal() {
        // 0→1→2 chain; depth 1 stops at 1.
        let s = store_with(3, &[(0, 1, EdgeKind::Calls), (1, 2, EdgeKind::Calls)]);
        assert_eq!(reached(&downstream(&s, 0, 1, false)), vec![1]);
    }

    proptest! {
        /// BFS terminates and respects the cap on arbitrary graphs (incl. cycles).
        #[test]
        fn bfs_always_terminates(
            n in 1usize..40,
            edges in proptest::collection::vec((0usize..40, 0usize..40, 0u8..6), 0..200),
            start in 0usize..40,
            depth in 0usize..50,
        ) {
            let spec: Vec<(usize, usize, EdgeKind)> = edges
                .into_iter()
                .filter(|&(f, t, _)| f < n && t < n)
                .map(|(f, t, k)| {
                    let kind = match k {
                        0 => EdgeKind::Calls,
                        1 => EdgeKind::Touches,
                        2 => EdgeKind::Publishes,
                        3 => EdgeKind::Consumes,
                        4 => EdgeKind::Owns,
                        _ => EdgeKind::Invokes,
                    };
                    (f, t, kind)
                })
                .collect();
            let s = store_with(n, &spec);
            for r in [
                impact(&s, start, depth, true),
                upstream(&s, start, depth, false),
                downstream(&s, start, depth, false),
            ] {
                prop_assert!(r.len() <= DEFAULT_CAP);
                // No node reported twice.
                let mut seen = std::collections::HashSet::new();
                for reach in &r {
                    prop_assert!(seen.insert(reach.node));
                    prop_assert!(reach.node != start);
                }
            }
        }
    }
}
