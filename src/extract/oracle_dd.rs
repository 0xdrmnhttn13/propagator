//! Oracle data-dictionary extractor (`plan.md` §6 — dictionary-primary SQL edges).
//!
//! Sourcing `Calls`/`Touches` from `ALL_DEPENDENCIES` is more reliable than
//! regex over `.sql` files: no comment false-positives, no schema collisions,
//! reflects compiled objects. Propagator stays a zero-network binary, so this
//! module consumes a *dump* (CSV) produced by running `query()` via SQLcl MCP,
//! rather than holding an Oracle driver.
//!
//! The query also scans `ALL_SOURCE` for dynamic-SQL calls (`'Begin PROC('`
//! string-literal dispatch via `EXECUTE IMMEDIATE`) that `ALL_DEPENDENCIES`
//! never records — these are the "dynamic only" procs that vanish from the
//! dependency graph.
//!
//! CSV columns (header required): name,type,ref_name,ref_type

use std::path::Path;

use crate::extract::{Extraction, RawEdge};
use crate::model::EdgeKind;

/// The SQL to run in your dev/PT env (via SQLcl MCP) to produce the dump.
/// UNIONs `ALL_DEPENDENCIES` (static refs) with an `ALL_SOURCE` REGEXP scan
/// for dynamic-SQL calls that the dictionary misses.
#[must_use]
pub fn query(schema: &str) -> String {
    let sch = schema.replace('\'', "''");
    format!(
        "SELECT name, type, referenced_name AS ref_name, referenced_type AS ref_type \
         FROM all_dependencies \
         WHERE owner = UPPER('{sch}') \
           AND type IN ('PROCEDURE','FUNCTION','PACKAGE','PACKAGE BODY') \
           AND referenced_type IN ('PROCEDURE','FUNCTION','TABLE','VIEW','PACKAGE') \
           AND referenced_owner = UPPER('{sch}') \
         UNION \
         SELECT DISTINCT s.name, s.type, \
                UPPER(REGEXP_SUBSTR(s.text, \
                   'Begin[[:space:]]+([A-Za-z_][A-Za-z0-9_]*)', 1, 1, 'i', 1)) AS ref_name, \
                'PROCEDURE' AS ref_type \
         FROM all_source s \
         WHERE s.owner = UPPER('{sch}') \
           AND s.type IN ('PROCEDURE','FUNCTION','PACKAGE BODY') \
           AND REGEXP_LIKE(s.text, \
                   'Begin[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\\(', 'i') \
           AND UPPER(REGEXP_SUBSTR(s.text, \
                   'Begin[[:space:]]+([A-Za-z_][A-Za-z0-9_]*)', 1, 1, 'i', 1)) \
               NOT IN ('IF','FOR','WHILE','LOOP','CASE','SELECT','INSERT','UPDATE','DELETE',\
                        'NULL','DECLARE','END','EXCEPTION','EXECUTE','RETURN','RAISE','COMMIT',\
                        'ROLLBACK','THEN','MERGE','INTO','ELSE','ELSIF','WHEN','LOCK','CREATE',\
                        'DROP','ALTER','TRUNCATE','OPEN','FETCH','CLOSE')",
        sch = sch
    )
}

/// Parse a `ALL_DEPENDENCIES` CSV dump into `Calls`/`Touches` edges.
///
/// Edge mapping:
/// - referenced_type TABLE/VIEW → `Touches`
/// - referenced_type PROCEDURE/FUNCTION/PACKAGE → `Calls`
///
/// Schema is taken from the config source `service` mapping (caller stamps it),
/// so node identity becomes `SCHEMA.NAME` consistently with `sql.rs`.
pub fn extract_from_dump(path: &Path, schema: &str) -> anyhow::Result<Extraction> {
    let mut out = Extraction::default();
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read dd dump {}: {e}", path.display()))?;
    let sch = schema.to_ascii_uppercase();

    let mut lines = raw.lines();
    let header = lines.next().unwrap_or("").to_ascii_lowercase();
    if !header.contains("name") || !header.contains("ref_name") {
        anyhow::bail!(
            "dd dump {} header unexpected: expected name,type,ref_name,ref_type",
            path.display()
        );
    }
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line
            .split(',')
            .map(|c| c.trim().trim_matches('"'))
            .collect();
        if cols.len() < 4 {
            continue;
        }
        let name = cols[0].to_ascii_uppercase();
        let ref_name = cols[2].to_ascii_uppercase();
        let ref_type = cols[3].to_ascii_uppercase();
        let kind = match ref_type.as_str() {
            "TABLE" | "VIEW" => EdgeKind::Touches,
            "PROCEDURE" | "FUNCTION" | "PACKAGE" => EdgeKind::Calls,
            _ => continue,
        };
        let from_key = format!("{sch}.{name}");
        out.edges.push(RawEdge {
            kind,
            from: from_key,
            to: format!("{sch}.{ref_name}"),
            path: "<oracle-dd>".to_string(),
            line: 0,
            from_kind: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dump_into_calls_and_touches() {
        let ex = extract_from_dump(
            Path::new("fixtures/dd/oms-core-dependencies.csv"),
            "oms-core",
        )
        .unwrap();
        assert_eq!(ex.edges.len(), 5);
        assert!(ex.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "OMS-CORE.USP_NEW_ORDER_V16"
                && e.to == "OMS-CORE.SPI_CHECKBUYLIMIT"
        }));
        assert!(
            ex.edges
                .iter()
                .any(|e| e.kind == EdgeKind::Touches && e.to == "OMS-CORE.TORDER")
        );
        assert!(ex.edges.iter().all(|e| e.path == "<oracle-dd>"));
    }

    #[test]
    fn rejects_bad_header() {
        let p = std::env::temp_dir().join(format!("prop_dd_bad_{}.csv", std::process::id()));
        std::fs::write(&p, "kolom,ngawur\na,b\n").unwrap();
        assert!(extract_from_dump(&p, "x").is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn query_unions_dependencies_with_dynamic_source_scan() {
        let q = query("RMS");
        assert!(q.contains("all_dependencies"), "missing static deps");
        assert!(q.contains("UNION"), "missing UNION");
        assert!(q.contains("all_source"), "missing dynamic-SQL source scan");
        assert!(
            q.contains("REGEXP_LIKE"),
            "missing Begin proc( regex for dynamic calls"
        );
        assert!(q.contains("'IF'"), "missing PL/SQL keyword noise filter");
    }
}
