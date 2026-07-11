//! `propagator affected` — blast radius of uncommitted/local changes.
//!
//! Pipe a unified diff (git diff) on stdin → Propagator maps changed line
//! ranges to graph symbols → runs `impact()` on each → reports who's affected.
//!
//! Two mapping strategies:
//! - **SQL files** (`*.sql`): chunks in the store have `[line_start, line_end]`.
//!   A changed hunk overlapping a chunk → that proc/func is the changed symbol.
//! - **Code files** (`.go/.rs/.cpp`): no per-function chunks. Instead, edges
//!   whose `path` matches the diff file identify what symbols that file touches
//!   (Invokes, Touches, Publishes, Consumes). Each such target is a changed
//!   symbol.
//!
//! Output: markdown table (default) or JSON (`--out json`). `--fail-on
//! cross-service` exits 1 if impact reaches a service other than the origin.

use std::collections::HashSet;
use std::io::Read;

use anyhow::Result;

use crate::graph::{Reach, impact, kind_label};
use crate::model::{EdgeKind, NodeId, NodeKind, Store};

/// One changed region from a unified diff.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub file: String,
    /// Changed line range (new side, 1-based, inclusive).
    pub start: usize,
    pub end: usize,
}

/// A symbol identified as changed by the diff.
#[derive(Debug, Clone)]
pub struct ChangedSymbol {
    pub node_id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    pub file: String,
    pub lines: String,
    /// How this symbol was identified: "chunk overlap" (SQL) or "edge path" (code).
    pub source: &'static str,
}

/// Full report: changed symbols + their aggregated blast radius.
pub struct AffectedReport {
    pub changed: Vec<ChangedSymbol>,
    /// (symbol_id, reaches) — impact per changed symbol.
    pub impacts: Vec<(NodeId, Vec<Reach>)>,
}

impl AffectedReport {
    /// All unique impacted node IDs across all changed symbols.
    pub fn impacted_nodes(&self, store: &Store) -> Vec<(NodeId, String, NodeKind, usize)> {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut out = Vec::new();
        for (_, reaches) in &self.impacts {
            for r in reaches {
                if seen.insert(r.node) {
                    let n = &store.nodes[r.node];
                    out.push((r.node, n.name.clone(), n.kind, r.depth));
                }
            }
        }
        out
    }

    /// Services in the blast radius, excluding the origin services.
    pub fn cross_service_targets(&self, store: &Store) -> Vec<String> {
        let origins: HashSet<NodeId> = self.changed.iter().map(|c| c.node_id).collect();
        let mut services = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::new();
        for (_, reaches) in &self.impacts {
            for r in reaches {
                let n = &store.nodes[r.node];
                if n.kind == NodeKind::Service && !origins.contains(&r.node) && seen.insert(r.node)
                {
                    services.push(n.name.clone());
                }
            }
        }
        services
    }

    pub fn render_md(&self, store: &Store) -> String {
        let mut out = String::new();

        // Changed symbols
        out.push_str("## Changed symbols\n");
        out.push_str("| file | symbol | kind | lines | source |\n");
        out.push_str("|---|---|---|---|---|\n");
        for c in &self.changed {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                short_path(&c.file),
                c.name,
                kind_label(c.kind),
                c.lines,
                c.source
            ));
        }

        // Blast radius
        let impacted = self.impacted_nodes(store);
        if impacted.is_empty() {
            out.push_str("\n(no impact beyond changed symbols)\n");
        } else {
            out.push_str(&format!("\n## Blast radius ({} nodes)\n", impacted.len()));
            out.push_str("| symbol | kind | depth |\n");
            out.push_str("|---|---|---|\n");
            for (_, name, kind, depth) in &impacted {
                out.push_str(&format!("| {} | {} | d{} |\n", name, kind_label(*kind), depth));
            }
        }

        // Cross-service
        let cross = self.cross_service_targets(store);
        if !cross.is_empty() {
            out.push_str(&format!(
                "\n## Cross-service impact\n{}\n",
                cross.iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        out
    }

    pub fn render_json(&self, store: &Store) -> String {
        let changed: Vec<serde_json::Value> = self
            .changed
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "kind": kind_label(c.kind),
                    "file": c.file,
                    "lines": c.lines,
                    "source": c.source,
                })
            })
            .collect();

        let impacted = self.impacted_nodes(store);
        let blast: Vec<serde_json::Value> = impacted
            .iter()
            .map(|(_, name, kind, depth)| {
                serde_json::json!({
                    "name": name,
                    "kind": kind_label(*kind),
                    "depth": depth,
                })
            })
            .collect();

        let cross = self.cross_service_targets(store);

        serde_json::json!({
            "changed": changed,
            "blast_radius": blast,
            "cross_service": cross,
        })
        .to_string()
    }
}

/// Parse a unified diff into `(file, start, end)` hunks (new-side line ranges).
///
/// Handles `diff --git a/x b/x` + `--- a/x` / `+++ b/x` headers, and `@@ -old,count +new,count @@`
/// hunk markers. Only the new (right) side is used — that's what matters for
/// "what's in the working tree now."
pub fn parse_diff(diff: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut current_file: Option<String> = None;

    for line in diff.lines() {
        // `+++ b/path` — the new file. `+++ /dev/null` = deletion; skip hunks.
        if let Some(rest) = line.strip_prefix("+++ ") {
            current_file = if rest == "/dev/null" {
                None
            } else {
                Some(strip_prefix(rest).to_string())
            };
            continue;
        }
        if line.starts_with("--- ") {
            // Paired with +++ above; we take the +++ side.
            continue;
        }
        // `diff --git` resets; no file until next +++.
        if line.starts_with("diff ") {
            current_file = None;
            continue;
        }
        // `@@ -old_s,old_c +new_s,new_c @@`
        let Some(file) = &current_file else {
            continue;
        };
        if let Some(hunk) = parse_hunk_header(line, file) {
            hunks.push(hunk);
        }
    }
    hunks
}

/// Extract new-side line range from `@@ -o,oc +n,nc @@`.
fn parse_hunk_header(line: &str, file: &str) -> Option<Hunk> {
    let at = line.strip_prefix("@@ ")?;
    // Find the ` +` that starts the new-side spec (after the old-side `-o,oc`).
    let plus_pos = at.find(" +")?;
    let rest = &at[plus_pos + 2..]; // skip " +"
    let end_ctx = rest.find(" @@").unwrap_or(rest.len());
    let spec = &rest[..end_ctx];
    let (start_str, count_str) = spec.split_once(',')?;
    let start: usize = start_str.parse().ok()?;
    let count: usize = count_str.parse().ok()?;
    // Range is [start, start+count-1]; count=0 means insertion point only.
    let end = if count == 0 {
        start
    } else {
        start + count - 1
    };
    Some(Hunk {
        file: file.to_string(),
        start,
        end,
    })
}

/// Strip `b/` prefix from diff path (and any leading `./`).
fn strip_prefix(path: &str) -> &str {
    let p = path.strip_prefix("b/").unwrap_or(path);
    p.strip_prefix("./").unwrap_or(p)
}

/// Map diff hunks to graph symbols.
///
/// Strategy:
/// 1. **Chunks** (SQL procs/funcs): find chunks whose path matches a hunk file
///    AND whose `[line_start, line_end]` overlaps the hunk's `[start, end]`.
/// 2. **Edges** (code files): find edges whose `path` matches a hunk file.
///    The `to` nodes are symbols the file touches — those are changed.
pub fn map_to_symbols(store: &Store, hunks: &[Hunk]) -> Vec<ChangedSymbol> {
    let mut out = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();

    for hunk in hunks {
        // --- Strategy 1: chunk overlap (SQL) ---
        for chunk in &store.chunks {
            if !path_matches(&chunk.path, &hunk.file) {
                continue;
            }
            // Overlap check: ranges intersect?
            if hunk.end < chunk.line_start || hunk.start > chunk.line_end {
                continue;
            }
            if let Some(node_id) = chunk.node {
                if seen.insert(node_id) {
                    let n = &store.nodes[node_id];
                    out.push(ChangedSymbol {
                        node_id,
                        name: n.key(),
                        kind: n.kind,
                        file: chunk.path.clone(),
                        lines: format!("{}-{}", hunk.start, hunk.end),
                        source: "chunk overlap",
                    });
                }
            }
        }

        // --- Strategy 2: edge path match (code) ---
        for edge in &store.edges {
            if !path_matches(&edge.path, &hunk.file) {
                continue;
            }
            // Skip Owns (structural, never "changed" by a code edit).
            if edge.kind == EdgeKind::Owns {
                continue;
            }
            // The `from` node is what the file belongs to (usually a Service).
            // The `to` node is what it reaches (proc, table, topic, redis key).
            // For impact purposes, the symbol that "changed" is the `from` node —
            // a change in the code file affects what that service does.
            //
            // But if `from` is a Service and we already captured it (common case),
            // we only want one entry per service per file. We also want to capture
            // the specific targets (Invokes proc X) as context.
            if seen.insert(edge.from) {
                let n = &store.nodes[edge.from];
                out.push(ChangedSymbol {
                    node_id: edge.from,
                    name: n.key(),
                    kind: n.kind,
                    file: edge.path.clone(),
                    lines: format!("{}-{}", hunk.start, hunk.end),
                    source: "edge path",
                });
            }
        }
    }

    out
}

/// Does a store path (possibly absolute) match a diff path (relative)?
/// Matches if the store path ends with the diff path, or vice versa.
fn path_matches(store_path: &str, diff_path: &str) -> bool {
    store_path.ends_with(diff_path) || diff_path.ends_with(store_path)
}

/// Run impact on all changed symbols and produce the report.
pub fn run(store: &Store, hunks: &[Hunk]) -> AffectedReport {
    let changed = map_to_symbols(store, hunks);
    let mut impacts = Vec::new();
    for c in &changed {
        let reaches = impact(store, c.node_id, 5, false);
        if !reaches.is_empty() {
            impacts.push((c.node_id, reaches));
        }
    }
    AffectedReport { changed, impacts }
}

/// Read stdin, parse diff, map to symbols, run impact. CLI entry point.
pub fn cli_run(fail_on_cross_service: bool, json: bool) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if input.is_empty() {
        anyhow::bail!("no diff on stdin — pipe: git diff | propagator affected");
    }
    let hunks = parse_diff(&input);
    if hunks.is_empty() {
        println!("no changed hunks in diff");
        return Ok(());
    }
    let (cfg, _) = crate::config::Config::discover()?;
    let store = crate::model::Store::load(&cfg.store.path)?;
    let report = run(&store, &hunks);

    if json {
        println!("{}", report.render_json(&store));
    } else {
        print!("{}", report.render_md(&store));
    }

    if fail_on_cross_service {
        let cross = report.cross_service_targets(&store);
        if !cross.is_empty() {
            eprintln!(
                "\nFAIL: cross-service impact to [{}] — review before push",
                cross.join(", ")
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Shorten a path for display: keep last 2 components.
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.rsplitn(3, '/').collect();
    parts.iter().rev().copied().collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_diff() {
        let diff = "\
diff --git a/foo.sql b/foo.sql
index 1234567..abcdefg 100644
--- a/foo.sql
+++ b/foo.sql
@@ -10,3 +10,5 @@
 old line
+new line
+another new line
 context
@@ -30,2 +32,3 @@
 ctx
+inserted
 ctx
";
        let hunks = parse_diff(diff);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file, "foo.sql");
        assert_eq!(hunks[0].start, 10);
        assert_eq!(hunks[0].end, 14); // 10 + 5 - 1
        assert_eq!(hunks[1].file, "foo.sql");
        assert_eq!(hunks[1].start, 32);
        assert_eq!(hunks[1].end, 34); // 32 + 3 - 1
    }

    #[test]
    fn parse_diff_skips_dev_null_deletion() {
        let diff = "\
diff --git a/deleted.sql b/deleted.sql
deleted file mode 100644
--- a/deleted.sql
+++ /dev/null
@@ -1,3 +0,0 @@
-old
-stuff
-here
diff --git a/kept.go b/kept.go
--- a/kept.go
+++ b/kept.go
@@ -5,1 +5,2 @@
 ctx
+added
";
        let hunks = parse_diff(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, "kept.go");
    }

    #[test]
    fn parse_diff_handles_renames() {
        let diff = "\
diff --git a/old.go b/new.go
similarity index 90%
rename from old.go
rename to new.go
--- a/old.go
+++ b/new.go
@@ -3,1 +3,2 @@
 ctx
+added
";
        let hunks = parse_diff(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, "new.go");
    }

    #[test]
    fn path_matches_suffix() {
        assert!(path_matches(
            "/home/user/work/growin_rms/migration/current/foo.sql",
            "migration/current/foo.sql"
        ));
        assert!(path_matches("foo.go", "foo.go"));
        assert!(!path_matches("/a/b/c.sql", "d.sql"));
    }

    #[test]
    fn hunk_range_single_line() {
        let hunk = parse_hunk_header("@@ -100,1 +105,1 @@", "f.sql").unwrap();
        assert_eq!(hunk.start, 105);
        assert_eq!(hunk.end, 105);
    }

    #[test]
    fn empty_diff_yields_empty() {
        assert!(parse_diff("").is_empty());
        assert!(parse_diff("not a diff\njust text\n").is_empty());
    }
}
