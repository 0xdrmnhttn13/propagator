//! Invokes extractor (`plan.md` §6 — `Invokes` edges, Service → Procedure).
//!
//! Best-effort scan of Go/Rust/C++ for hard-coded procedure-name literals at
//! SQL call sites. Per the confirmed "mostly hard-coded literals" answer, this
//! gives real cross-world hops. Unresolved call sites (dynamic SQL / ORM) are
//! reported and lean on `Owns` as the always-on floor.
//!
//! Heuristic: a line containing a SQL-call keyword (Exec/Query/Prepare/Command/
//! execute/CALL/EXEC) AND a string literal whose inner token matches a
//! procedure-name shape (caps-snake, len≥3). Const-map indirection supported.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use regex::Regex;
use walkdir::WalkDir;

use crate::extract::mq::Lang;
use crate::extract::{Extraction, RawEdge};
use crate::model::EdgeKind;

/// Regex over a "looks like a stored proc name" token inside quotes.
const PROC_SHAPE: &str = r#""([A-Z][A-Z0-9_]{2,})""#;

/// Bare ALL-CAPS identifier (Go/Rust/C++ const used directly at a call site).
/// PascalCase method names (PrepareContext, QueryRow) never match because their
/// 2nd char onward is lowercase.
const PROC_BARE: &str = r"\b([A-Z][A-Z0-9_]{2,})\b";

/// Keywords that signal a SQL call site.
const CALL_KEYWORDS: &[&str] = &[
    "Exec",
    "Query",
    "QueryRow",
    "Prepare",
    "Get",
    "Select",
    ".execute(",
    ".query(",
    "prepare(",
    "CALL ",
    "EXEC ",
    "EXECUTE ",
    "Command(",
    "Run(",
];

pub fn extract(root: &Path, service: &str) -> anyhow::Result<Extraction> {
    let mut out = Extraction::default();
    let proc_re = Regex::new(PROC_SHAPE).expect("proc shape regex");
    let bare_re = Regex::new(PROC_BARE).expect("proc bare regex");
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            !e.file_type().is_dir()
                || !crate::extract::mq::skip_dir(e.file_name().to_string_lossy().as_ref())
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if Lang::from_ext(ext).is_none() {
            continue;
        }
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let abs = path.to_string_lossy().to_string();
        let const_map = collect_string_consts(&src);
        for (i, line) in src.lines().enumerate() {
            if !CALL_KEYWORDS.iter().any(|kw| line.contains(kw)) {
                continue;
            }
            let mut seen: HashSet<String> = HashSet::new();
            for m in proc_re.captures_iter(line) {
                let mut name = m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
                if let Some(v) = const_map.get(&name) {
                    name = v.clone();
                }
                if seen.insert(name.clone()) {
                    out.edges.push(RawEdge {
                        kind: EdgeKind::Invokes,
                        from: service.to_string(),
                        to: name,
                        path: abs.clone(),
                        line: i + 1,
                        from_kind: None,
                    });
                }
            }
            for m in bare_re.captures_iter(line) {
                let raw = m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
                if seen.contains(&raw) {
                    continue;
                }
                let name = const_map.get(&raw).cloned().unwrap_or(raw);
                if seen.insert(name.clone()) {
                    out.edges.push(RawEdge {
                        kind: EdgeKind::Invokes,
                        from: service.to_string(),
                        to: name,
                        path: abs.clone(),
                        line: i + 1,
                        from_kind: None,
                    });
                }
            }
        }
    }
    Ok(out)
}

fn collect_string_consts(src: &str) -> HashMap<String, String> {
    // Reuse a light version: `"IDENT" = "value"` style isn't common; we mainly
    // want top-of-file `const X = "USP_..."` so quoted-literal proc names that
    // are referenced indirectly can still resolve.
    let mut map = HashMap::new();
    static CONST_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = CONST_RE.get_or_init(|| {
        Regex::new(r#"(?:const|static)\s+(\w+)[^=]*=\s*"([A-Z][A-Z0-9_]{2,})""#)
            .expect("const proc regex")
    });
    for c in re.captures_iter(src) {
        if let (Some(id), Some(val)) = (c.get(1), c.get(2)) {
            map.insert(id.as_str().to_string(), val.as_str().to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_proc_literal_at_sql_call_site() {
        let ex = extract(Path::new("fixtures/go"), "risk-engine").unwrap();
        let inv: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Invokes)
            .collect();
        // bare const (PrepareContext) + quoted literal (Exec)
        assert_eq!(inv.len(), 2);
        let targets: Vec<&str> = inv.iter().map(|e| e.to.as_str()).collect();
        assert!(targets.contains(&"USP_NEW_ORDER_V16"), "bare const missed");
        assert!(
            targets.contains(&"SPI_CHECKBUYLIMIT"),
            "quoted literal missed"
        );
        assert!(inv.iter().all(|e| e.from == "risk-engine"));
        assert!(inv.iter().all(|e| e.line > 0));
    }
}
