//! SQL definition extractor (`plan.md` §6.1).
//!
//! Job: locate procedure/function definitions in `*.sql` files and emit
//! `RawDef`s with schema, name, signature, body, line range.
//!
//! Also scans proc bodies for dynamic calls — bare `BEGIN proc_name(` blocks
//! that PL/SQL dynamic dispatch produces and `ALL_DEPENDENCIES` sometimes
//! misses (dynamic SQL, late-resolved refs). These emit `Calls` edges; the
//! oracle_dd dump remains the primary source, this fills the gaps.

use regex::Regex;
use walkdir::WalkDir;

use crate::extract::{Extraction, RawDef, RawDefKind, RawEdge};
use crate::model::EdgeKind;

/// PL/SQL keywords that can legitimately follow `BEGIN` and end with `(`
/// (control flow, DML) — never a proc call, so skipped by the dynamic-call
/// scanner to avoid bogus `Calls` edges.
const SQL_NOISE_WORDS: &[&str] = &[
    "IF",
    "FOR",
    "WHILE",
    "LOOP",
    "CASE",
    "ELSE",
    "ELSIF",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "INTO",
    "EXECUTE",
    "EXEC",
    "OPEN",
    "FETCH",
    "CLOSE",
    "DECLARE",
    "NULL",
    "EXIT",
    "RETURN",
    "RAISE",
    "COMMIT",
    "ROLLBACK",
    "BEGIN",
    "END",
    "EXCEPTION",
    "WHEN",
    "THEN",
    "LOCK",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
];

/// Scan `root` for `*.sql` files and extract procedure/function definitions.
pub fn extract(root: &std::path::Path, service: &str) -> anyhow::Result<Extraction> {
    let mut out = Extraction::default();
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
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let abs = path.to_string_lossy().to_string();
        let defs = definitions(&src, service, &abs, &rel);
        out.edges.extend(dynamic_call_edges(&defs));
        out.edges.extend(dynamic_table_edges(&defs));
        out.defs.extend(defs);
    }
    Ok(out)
}

/// Scan proc bodies for dynamic calls: `BEGIN proc_name(` patterns that
/// `ALL_DEPENDENCIES` misses (dynamic dispatch, late-bound refs). Emits
/// `Calls` edges. Comment-stripped to avoid false hits; PL/SQL control/DML
/// keywords are skipped so `BEGIN IF (` / `BEGIN FOR (` don't pollute.
fn dynamic_call_edges(defs: &[RawDef]) -> Vec<RawEdge> {
    static CALL_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = CALL_RE.get_or_init(|| {
        Regex::new(r"(?i)\bbegin\s+([A-Za-z_]\w*)\s*\(").expect("dynamic call regex")
    });
    let mut edges = Vec::new();
    for def in defs {
        let stripped = strip_comments(&def.body);
        let from = match &def.schema {
            Some(s) => format!("{s}.{}", def.title),
            None => def.title.clone(),
        };
        for cap in re.captures_iter(&stripped) {
            let name = cap[1].to_ascii_uppercase();
            if SQL_NOISE_WORDS.contains(&name.as_str()) || name == def.title {
                continue;
            }
            let m = cap.get(1).expect("capture group 1");
            let line_off = stripped[..m.start()].matches('\n').count();
            edges.push(RawEdge {
                kind: EdgeKind::Calls,
                from: from.clone(),
                to: name,
                path: def.path.clone(),
                line: def.line_start + line_off,
                from_kind: None,
            });
        }
    }
    edges
}

/// Scan proc bodies for tables referenced inside `EXECUTE IMMEDIATE '<sql>'`
/// dynamic-SQL string literals. Oracle's `ALL_DEPENDENCIES` AND the outer parser
/// both miss these — the table name lives in a string literal, so a proc that
/// only touches a table via dynamic SQL shows no edge (the `USP_NEW_ORDER_V16`
/// → `TSTOCKINFO_DAYTRADE` case). Emits `Touches` edges.
///
/// Pragmatic scope: single literal strings (the common lookup/DML form). SQL
/// built from a variable (`execute immediate varStr`) can't be resolved
/// statically and is skipped; strings with `''`-escaped quotes truncate at the
/// first quote. Schema prefix (`RMS.TBL`) is stripped to the table name.
fn dynamic_table_edges(defs: &[RawDef]) -> Vec<RawEdge> {
    static EI_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `execute immediate` (optional `(`) then the first single-quoted literal;
    // `(?is)` so the literal may span multiple lines.
    let ei = EI_RE.get_or_init(|| {
        Regex::new(r"(?is)execute\s+immediate\s*\(?\s*'([^']*)'").expect("ei regex")
    });
    static TBL_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // Table after from / join / update / insert into / merge into, with an
    // optional `schema.` prefix that we strip (capture group = table name only).
    let tbl = TBL_RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(?:from|join|update|insert\s+into|merge\s+into)\s+(?:"?[A-Za-z][\w$#]*"?\s*\.\s*)?"?([A-Za-z][\w$#]*)"?"#,
        )
        .expect("tbl regex")
    });
    let mut edges = Vec::new();
    for def in defs {
        let from = match &def.schema {
            Some(s) => format!("{s}.{}", def.title),
            None => def.title.clone(),
        };
        for ei_cap in ei.captures_iter(&def.body) {
            let sql = &ei_cap[1];
            let m = ei_cap.get(0).expect("ei match 0");
            let line_off = def.body[..m.start()].matches('\n').count();
            for tcap in tbl.captures_iter(sql) {
                let name = tcap[1].to_ascii_uppercase();
                if SQL_NOISE_WORDS.contains(&name.as_str()) || name == "DUAL" || name == def.title {
                    continue;
                }
                edges.push(RawEdge {
                    kind: EdgeKind::Touches,
                    from: from.clone(),
                    to: name,
                    path: def.path.clone(),
                    line: def.line_start + line_off,
                    from_kind: None,
                });
            }
        }
    }
    edges
}

/// Strip `--` line comments and `/* */` block comments before keyword scanning,
/// so a `CREATE PROCEDURE` inside a comment doesn't trigger a false def.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        // line comment
        if bytes[i] == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // block comment
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            out.push(' ');
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Extract procedure/function definitions from a single SQL source string.
pub fn definitions(src: &str, service: &str, abs_path: &str, rel_path: &str) -> Vec<RawDef> {
    static DEF_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = DEF_RE.get_or_init(|| {
        // Optional `"quotes"` around schema and name: Oracle quoted identifiers
        // (`RMS."SPI_IDX_STOCKREPLY_V6"`) are the majority in some schemas; without
        // the `"?` the name group can't match `"` and mis-captures the schema as
        // the proc name (garbage `RMS` node), dropping the real def. The `[\w$#]+`
        // captures the inner name, so quotes are stripped from the groups.
        Regex::new(
            r#"(?i)create\s+(?:or\s+replace\s+)?(?:procedure|function)\s+(?:"?([\w$#]+)"?\s*\.\s*)?"?([\w$#]+)"?"#,
        )
        .expect("def regex")
    });

    let stripped = strip_comments(src);
    // Map def matches back to original source line numbers using the original
    // text (stripping preserves newlines, so line offsets line up).
    let mut starts: Vec<(usize, usize, Option<String>, String)> = Vec::new();
    for (line_idx, sline) in stripped.lines().enumerate() {
        if let Some(c) = re.captures(sline) {
            let schema = c.get(1).map(|m| m.as_str().to_ascii_uppercase());
            let name = c
                .get(2)
                .map(|m| m.as_str().to_ascii_uppercase())
                .unwrap_or_default();
            starts.push((line_idx + 1, line_idx + 1, schema, name));
        }
    }
    let _ = service;

    if starts.is_empty() {
        return Vec::new();
    }

    // Boundary: chunk runs from its start line to the line before the next def,
    // or EOF. For the body we re-slice the original src.
    let total_lines = src.lines().count();
    let mut defs = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    for (i, (start, _end, schema, name)) in starts.iter().enumerate() {
        let end = if i + 1 < starts.len() {
            starts[i + 1].0.saturating_sub(1)
        } else {
            total_lines
        };
        let body = lines[(*start - 1)..end.min(lines.len())].join("\n");
        let signature = extract_signature(&body);
        defs.push(RawDef {
            service: service.to_string(),
            path: abs_path.to_string(),
            title: name.clone(),
            schema: schema.clone(),
            line_start: *start,
            line_end: end,
            signature,
            body,
            kind: RawDefKind::Proc, // refined below by keyword in signature
        });
    }

    // Refine Proc vs Function from the stripped def line keyword.
    for (i, (line_idx, _, _, _)) in starts.iter().enumerate() {
        let sline = stripped.lines().nth(*line_idx - 1).unwrap_or("");
        if re.captures(sline).is_some() {
            let is_func = sline.to_ascii_lowercase().contains("function");
            if is_func {
                defs[i].kind = RawDefKind::Function;
            }
        }
    }

    // `rel_path` is unused for storage (we keep abs for evidence) but kept for
    // potential display filtering later.
    let _ = rel_path;
    defs
}

/// Heuristic signature: from the def keyword to the first ` AS ` / ` IS ` /
/// `RETURN` clause / end of first logical statement.
fn extract_signature(body: &str) -> String {
    let first_line = body.lines().next().unwrap_or("").trim();
    let lower = body.to_ascii_lowercase();
    // Try to cut at ` as ` / ` is ` / `return`.
    let combined = body.replace('\n', " ");
    let lower_combined = combined.to_ascii_lowercase();
    let cut = [" as ", " is ", " return "]
        .iter()
        .filter_map(|tok| lower_combined.find(tok))
        .min();
    let sig = match cut {
        Some(idx) => combined[..idx].trim().to_string(),
        None => first_line.to_string(),
    };
    let _ = lower;
    sig.chars().take(256).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const FIXTURE: &str = r#"CREATE OR REPLACE PROCEDURE USP_NEW_ORDER_V16 (
    p_order_id IN NUMBER
) AS
BEGIN
    SPI_CHECKBUYLIMIT(p_order_id);
END;
/

-- CREATE PROCEDURE COMMENTED_OUT (harus diabaikan)
/* CREATE FUNCTION ALSO_COMMENTED */

CREATE OR REPLACE PROCEDURE OMS.SPI_CHECKBUYLIMIT (
    p_order_id IN NUMBER
) AS
BEGIN
    NULL;
END;
/

CREATE OR REPLACE FUNCTION F_GET_TOTAL (p_order_id IN NUMBER) RETURN NUMBER AS
BEGIN
    RETURN 1;
END;
"#;

    #[test]
    fn extracts_defs_with_kind_schema_and_lines() {
        let defs = definitions(FIXTURE, "oms-core", "a.sql", "a.sql");
        assert_eq!(defs.len(), 3);

        assert_eq!(defs[0].title, "USP_NEW_ORDER_V16");
        assert_eq!(defs[0].kind, RawDefKind::Proc);
        assert_eq!(defs[0].schema, None);
        assert_eq!(defs[0].line_start, 1);

        assert_eq!(defs[1].title, "SPI_CHECKBUYLIMIT");
        assert_eq!(defs[1].schema.as_deref(), Some("OMS"));

        assert_eq!(defs[2].title, "F_GET_TOTAL");
        assert_eq!(defs[2].kind, RawDefKind::Function);

        // Chunk boundary: def 1 runs up to the line before def 2.
        assert!(defs[0].line_end < defs[1].line_start);
        assert!(defs[0].body.contains("SPI_CHECKBUYLIMIT(p_order_id)"));
    }

    #[test]
    fn extracts_quoted_identifier_proc() {
        // Oracle quoted-identifier form — the majority of RMS procs. Before the
        // `"?` fix this mis-captured name="RMS" and dropped the real def.
        let src = r#"CREATE OR REPLACE PROCEDURE RMS."SPI_IDX_STOCKREPLY_V6" (
    inSymbol IN VARCHAR2
) AS
BEGIN
    NULL;
END;
"#;
        let defs = definitions(src, "riskmanagementservice", "a.sql", "a.sql");
        assert_eq!(defs.len(), 1, "quoted proc def not extracted");
        assert_eq!(defs[0].title, "SPI_IDX_STOCKREPLY_V6");
        assert_eq!(defs[0].schema.as_deref(), Some("RMS"));
        assert_eq!(defs[0].kind, RawDefKind::Proc);
    }

    #[test]
    fn quoted_and_unquoted_names_both_extract() {
        // Mixed file: unquoted + quoted-schema + quoted-name variants all resolve.
        let src = r#"CREATE OR REPLACE PROCEDURE RMS.PLAIN_PROC AS BEGIN NULL; END;
/
CREATE OR REPLACE PROCEDURE "RMS"."QUOTED_BOTH" AS BEGIN NULL; END;
/
CREATE OR REPLACE FUNCTION RMS."QUOTED_FN" RETURN NUMBER AS BEGIN RETURN 1; END;
"#;
        let defs = definitions(src, "svc", "a.sql", "a.sql");
        let names: Vec<&str> = defs.iter().map(|d| d.title.as_str()).collect();
        assert!(names.contains(&"PLAIN_PROC"));
        assert!(names.contains(&"QUOTED_BOTH"));
        assert!(names.contains(&"QUOTED_FN"));
        assert!(!names.contains(&"RMS"), "garbage RMS node still produced");
        assert_eq!(
            defs.iter().find(|d| d.title == "QUOTED_FN").unwrap().kind,
            RawDefKind::Function
        );
    }

    #[test]
    fn commented_out_defs_are_ignored() {
        let defs = definitions(FIXTURE, "svc", "a.sql", "a.sql");
        assert!(defs.iter().all(|d| d.title != "COMMENTED_OUT"));
        assert!(defs.iter().all(|d| d.title != "ALSO_COMMENTED"));
    }

    #[test]
    fn signature_cut_at_as_keyword() {
        let defs = definitions(FIXTURE, "svc", "a.sql", "a.sql");
        assert!(
            defs[0]
                .signature
                .starts_with("CREATE OR REPLACE PROCEDURE USP_NEW_ORDER_V16")
        );
        assert!(!defs[0].signature.contains("BEGIN"));
    }

    #[test]
    fn dynamic_calls_caught_in_fixture_body() {
        let defs = definitions(FIXTURE, "oms-core", "a.sql", "a.sql");
        let edges = dynamic_call_edges(&defs);
        // USP_NEW_ORDER_V16 calls SPI_CHECKBUYLIMIT via a BEGIN block.
        assert!(edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "USP_NEW_ORDER_V16"
                && e.to == "SPI_CHECKBUYLIMIT"
        }));
        // SPI_CHECKBUYLIMIT body is `BEGIN NULL;` (noise), F_GET_TOTAL is
        // `BEGIN RETURN 1;` (no paren) → exactly one edge.
        assert_eq!(edges.len(), 1, "expected exactly one dynamic-call edge");
    }

    #[test]
    fn dynamic_calls_skip_keywords_self_and_comments() {
        let src = r#"CREATE OR REPLACE PROCEDURE USP_SELF AS
BEGIN
    -- BEGIN SPI_COMMENTED(val);
    IF (x > 0) THEN NULL; END IF;
    BEGIN USP_SELF(val); END;
    BEGIN SPI_TARGET(val); END;
END;
"#;
        let defs = definitions(src, "svc", "a.sql", "a.sql");
        let edges = dynamic_call_edges(&defs);
        let targets: Vec<&str> = edges.iter().map(|e| e.to.as_str()).collect();
        assert!(targets.contains(&"SPI_TARGET"), "real dynamic call missed");
        assert!(
            !targets.contains(&"IF"),
            "control-flow keyword not filtered"
        );
        assert!(!targets.contains(&"USP_SELF"), "self-call not filtered");
        assert!(
            !targets.contains(&"SPI_COMMENTED"),
            "commented-out call not stripped"
        );
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn dynamic_table_edges_from_execute_immediate() {
        // Real shape from USP_NEW_ORDER_V16: multi-line dynamic SELECT with a
        // `/*+ hint */`, table only visible inside the string literal.
        let src = r#"CREATE OR REPLACE PROCEDURE RMS.USP_X AS
    varMultiplier number := 1;
BEGIN
    if v = 9 then
      execute immediate
        'select /*+ FIRST_ROWS(1) */ nvl(max(MULTIPLIER),1)
           from TSTOCKINFO_DAYTRADE where SYMBOL = :a'
      into varMultiplier using varSymbol;
    end if;
END;
"#;
        let defs = definitions(src, "svc", "a.sql", "a.sql");
        let edges = dynamic_table_edges(&defs);
        assert!(
            edges.iter().any(|e| e.kind == EdgeKind::Touches
                && e.from == "RMS.USP_X"
                && e.to == "TSTOCKINFO_DAYTRADE"),
            "dynamic table ref inside execute immediate missed"
        );
    }

    #[test]
    fn dynamic_table_edges_variants_and_limits() {
        let src = r#"CREATE OR REPLACE PROCEDURE P AS
BEGIN
    execute immediate 'insert into RMS.TAUDIT(x) values(:1)' using v;
    execute immediate 'update TCONFIG set y=:1';
    execute immediate 'select 1 from dual';
    execute immediate varStr;
END;
"#;
        let defs = definitions(src, "svc", "a.sql", "a.sql");
        let tos: Vec<String> = dynamic_table_edges(&defs)
            .iter()
            .map(|e| e.to.clone())
            .collect();
        assert!(
            tos.contains(&"TAUDIT".to_string()),
            "schema-qualified insert missed"
        );
        assert!(
            tos.contains(&"TCONFIG".to_string()),
            "dynamic update missed"
        );
        assert!(!tos.contains(&"DUAL".to_string()), "DUAL should be skipped");
        // `execute immediate varStr` (no literal) yields nothing — unresolvable.
    }

    proptest! {
        #[test]
        fn definitions_never_panics(src in "\\PC*") {
            let _ = definitions(&src, "svc", "x.sql", "x.sql");
        }

        #[test]
        fn strip_comments_preserves_line_count(src in "\\PC*") {
            let stripped = strip_comments(&src);
            prop_assert_eq!(stripped.matches('\n').count(), src.matches('\n').count());
        }
    }
}
