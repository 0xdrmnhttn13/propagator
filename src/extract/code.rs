//! Embedded-SQL extractor for application code (Go/Rust/C++) via tree-sitter.
//!
//! The regex extractors (`mq.rs`, `invokes.rs`) scan line-by-line. That is
//! structurally blind to the dominant real-world shape: SQL held in a
//! multi-line raw string literal (Go backtick, Rust `r#"…"#`, C++ `R"(…)"` and
//! adjacent-literal concatenation) where the verb and the table sit on
//! different physical lines. The precision audit (see memory `p0-precision-audit`)
//! measured 180 Go files using backtick raw strings and ~0 `Touches`-from-code
//! edges in the graph — application code that reads/writes a table directly
//! (not via a stored proc) was entirely invisible.
//!
//! This module parses each file once, walks the AST for **string-literal nodes**
//! (which capture the whole literal regardless of newlines / concatenation),
//! sniffs each for SQL *structure*, and emits:
//!   - `Touches` (service → table)  — NEW capability; `from_kind = Service`.
//!   - `Invokes` (service → proc)   — precise, from `BEGIN p(` / `CALL p` inside
//!     a real SQL string (vs. `invokes.rs`'s any-UPPER_SNAKE-on-a-keyword-line).
//!
//! Dynamic tables (`FROM %s`, Postgres `$1`, bind vars, `||` concat) can't be
//! resolved statically; they are counted (`Extraction::dynamic_sql_sites`) and
//! surfaced in the sync report rather than silently dropped — honesty ledger.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use tree_sitter::{Node, Parser};
use walkdir::WalkDir;

use crate::extract::mq::{Lang, skip_dir};
use crate::extract::{Extraction, RawEdge};
use crate::model::{EdgeKind, NodeKind};

/// Words a table/proc token might capture that are never a real object name —
/// SQL keywords and the dynamic-placeholder shapes. Guards against `FROM SELECT`
/// or a stray keyword being interned as a phantom Table node.
const NOISE: &[&str] = &[
    "DUAL",
    "SELECT",
    "WHERE",
    "SET",
    "VALUES",
    "AND",
    "OR",
    "AS",
    "ON",
    "BY",
    "GROUP",
    "ORDER",
    "INNER",
    "LEFT",
    "RIGHT",
    "OUTER",
    "JOIN",
    "FROM",
    "INTO",
    "UPDATE",
    "TABLE",
    "ONLY",
    "USING",
    "RETURNING",
    "BEGIN",
    "END",
    "NULL",
];

/// String-literal AST node kinds per grammar. Hitting one of these, we take the
/// whole node text and do NOT descend (so a C++ `concatenated_string` is taken
/// as one joined literal, not its fragments).
fn string_kinds(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Go => &["interpreted_string_literal", "raw_string_literal"],
        Lang::Rust => &["string_literal", "raw_string_literal"],
        Lang::Cpp => &[
            "string_literal",
            "raw_string_literal",
            "concatenated_string",
        ],
    }
}

fn language(lang: Lang) -> tree_sitter::Language {
    match lang {
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
    }
}

/// Scan a code source root, emitting embedded-SQL `Touches`/`Invokes` edges.
pub fn extract(root: &Path, service: &str) -> anyhow::Result<Extraction> {
    let mut out = Extraction::default();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            !e.file_type().is_dir() || !skip_dir(e.file_name().to_string_lossy().as_ref())
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Some(lang) = Lang::from_ext(ext) else {
            continue;
        };
        // Skip generated protobuf Rust (mostng_protobuf/.../rust) — never SQL.
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let abs = path.to_string_lossy().to_string();
        scan_file(lang, &src, service, &abs, &mut out);
    }
    Ok(out)
}

/// Parse one file and emit edges from its embedded SQL. Parser failures (binary
/// junk, unsupported syntax) are non-fatal — the file is simply skipped.
fn scan_file(lang: Lang, src: &str, service: &str, path: &str, out: &mut Extraction) {
    let mut parser = Parser::new();
    if parser.set_language(&language(lang)).is_err() {
        return;
    }
    let Some(tree) = parser.parse(src, None) else {
        return;
    };
    let bytes = src.as_bytes();
    // Redis key access shares this single parse (one parse per file). Its call
    // shape is library-specific, so it runs its own recognizer on the tree.
    crate::extract::redis::scan(lang, tree.root_node(), bytes, service, path, out);

    let mut strings: Vec<(String, usize)> = Vec::new();
    collect_strings(tree.root_node(), bytes, string_kinds(lang), &mut strings);

    for (raw, line) in strings {
        let sql = normalize(&raw);
        if !looks_like_sql(&sql) {
            continue;
        }
        for tbl in table_refs(&sql) {
            out.edges.push(edge(
                EdgeKind::Touches,
                service,
                &tbl,
                path,
                line,
                Some(NodeKind::Service),
            ));
        }
        for proc in proc_calls(&sql) {
            out.edges
                .push(edge(EdgeKind::Invokes, service, &proc, path, line, None));
        }
        out.dynamic_sql_sites += dynamic_table_count(&sql);
    }
}

fn edge(
    kind: EdgeKind,
    from: &str,
    to: &str,
    path: &str,
    line: usize,
    from_kind: Option<NodeKind>,
) -> RawEdge {
    RawEdge {
        kind,
        from: from.to_string(),
        to: to.to_string(),
        path: path.to_string(),
        line,
        from_kind,
    }
}

/// Recursively collect string-literal node text + start line; do not descend
/// into a string node (keeps concatenations whole).
fn collect_strings(node: Node, src: &[u8], kinds: &[&str], out: &mut Vec<(String, usize)>) {
    if kinds.contains(&node.kind()) {
        out.push((string_content(node, src), node.start_position().row + 1));
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_strings(child, src, kinds, out);
    }
}

/// Inner text of a string-literal node, delimiter-free. Prefers `*content*`
/// child nodes (`string_content` etc.) which every grammar exposes for the
/// literal body — this strips raw-string sigils (`r#"…"#`), quotes and backticks
/// uniformly and fuses C++ concatenated fragments. Falls back to the raw node
/// text if no content child exists.
fn string_content(node: Node, src: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_content(node, src, &mut parts);
    if parts.is_empty() {
        node.utf8_text(src).unwrap_or("").to_string()
    } else {
        parts.join(" ")
    }
}

fn collect_content(node: Node, src: &[u8], parts: &mut Vec<String>) {
    if node.kind().contains("content") {
        if let Ok(t) = node.utf8_text(src) {
            parts.push(t.to_string());
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_content(child, src, parts);
    }
}

/// Join a (possibly concatenated / raw) literal into a single whitespace-normal
/// string for sniffing. Strips the string delimiters (`"`, backtick) so C++
/// adjacent literals fuse; single quotes inside SQL (`'S'`) are left intact.
fn normalize(raw: &str) -> String {
    let joined: String = raw
        .chars()
        .map(|c| if c == '"' || c == '`' { ' ' } else { c })
        .collect();
    // Collapse runs of whitespace (incl. newlines) so multi-line SQL is one line.
    joined.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A literal is SQL only if it has a leading SQL verb — kills the English
/// false positive `"Insert watchlistgroup service problems."` (no `insert into`)
/// while keeping `SELECT … FROM …`, `INSERT INTO …`, `UPDATE …`, `BEGIN p(`.
fn looks_like_sql(s: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:with\s|select\s|insert\s+into\s|update\s+[a-z_\x22]|delete\s+from\s|merge\s+into\s|begin\s|call\s|exec(?:ute)?\s)")
            .expect("looks_like_sql regex")
    })
    .is_match(s)
}

fn table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // table after from/join/into/update, optional `schema.` prefix stripped.
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(?:from|join|into|update)\s+(?:"?[A-Za-z_][\w$#]*"?\s*\.\s*)?"?([A-Za-z_][\w$#]*)"?"#,
        )
        .expect("table regex")
    })
}

/// Distinct table names referenced by this SQL string (uppercased for node
/// identity — matches SQL-world Oracle nodes; Postgres lower-case names uppercase
/// to a stable key too). Placeholders (`%s`, `$1`) never match the ident group.
fn table_refs(sql: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in table_re().captures_iter(sql) {
        let name = c[1].to_ascii_uppercase();
        if NOISE.contains(&name.as_str()) {
            continue;
        }
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

fn proc_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `BEGIN p(` / `CALL p` / `EXEC[UTE] p` — the embedded stored-proc call forms.
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:begin|call|exec(?:ute)?)\s+([A-Za-z_][\w$#]*)\s*\(?")
            .expect("proc regex")
    })
}

fn proc_calls(sql: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in proc_re().captures_iter(sql) {
        let name = c[1].to_ascii_uppercase();
        if NOISE.contains(&name.as_str()) {
            continue;
        }
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

/// Count table positions that resolve to a runtime placeholder rather than a
/// literal name (`FROM %s`, `UPDATE $1`, `INTO :tbl`). Honesty-ledger signal.
fn dynamic_table_count(sql: &str) -> usize {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:from|join|into|update)\s+(%s|\$\d+|:\w+|\?)").expect("dyn regex")
    })
    .find_iter(sql)
    .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(lang: Lang, src: &str) -> Extraction {
        let mut out = Extraction::default();
        scan_file(lang, src, "svc", "f.x", &mut out);
        out
    }

    fn touches(out: &Extraction) -> Vec<&str> {
        out.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Touches)
            .map(|e| e.to.as_str())
            .collect()
    }
    fn invokes(out: &Extraction) -> Vec<&str> {
        out.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Invokes)
            .map(|e| e.to.as_str())
            .collect()
    }

    #[test]
    fn go_single_line_select_touches_table() {
        let src = r#"package repo
const q = "SELECT MTBIACCNO, CLIENTNAME FROM TCLIENTINFO t WHERE CLIENTID = :clientId"
"#;
        let out = scan(Lang::Go, src);
        assert!(
            touches(&out).contains(&"TCLIENTINFO"),
            "table from single-line SELECT missed"
        );
        // from_kind is Service for embedded-SQL touches.
        assert_eq!(out.edges[0].from_kind, Some(NodeKind::Service));
    }

    #[test]
    fn go_multiline_backtick_sql_touches_table() {
        // The shape a line scanner CANNOT catch: verb and table on different lines.
        let src = "package repo\nconst q = `\n  INSERT INTO gtc_orders (\n    id, client_id\n  ) VALUES ($1, $2)`\n";
        let out = scan(Lang::Go, src);
        assert!(
            touches(&out).contains(&"GTC_ORDERS"),
            "multi-line backtick table missed"
        );
    }

    #[test]
    fn go_embedded_proc_call_invokes() {
        // The daytrade caller: Prepare("BEGIN SPI_SET_DAYTRADE_PRICE(:1,:2,:3); END;").
        let src = r#"func f() { stmt, _ := db.Prepare("BEGIN SPI_SET_DAYTRADE_PRICE(:1, :2, :3); END;") }"#;
        let out = scan(Lang::Go, src);
        assert!(
            invokes(&out).contains(&"SPI_SET_DAYTRADE_PRICE"),
            "embedded proc call missed"
        );
        // BEGIN/END are noise, must not become proc targets.
        assert!(!invokes(&out).contains(&"END"));
    }

    #[test]
    fn english_error_constant_is_not_sql() {
        // Real false-positive from the corpus — must produce no edges.
        let src = r#"const ERR = "Insert watchlistgroup service problems.""#;
        let out = scan(Lang::Go, src);
        assert!(
            out.edges.is_empty(),
            "English string mis-detected as SQL: {:?}",
            touches(&out)
        );
    }

    #[test]
    fn dynamic_table_counted_not_edged() {
        let src = r#"const q = "SELECT * FROM %s WHERE CLIENTID = :1""#;
        let out = scan(Lang::Go, src);
        assert!(
            touches(&out).is_empty(),
            "%s placeholder must not become a table"
        );
        assert_eq!(
            out.dynamic_sql_sites, 1,
            "dynamic site not counted for ledger"
        );
    }

    #[test]
    fn rust_raw_string_and_cpp_concat() {
        let rs = scan(
            Lang::Rust,
            "fn f() { let q = r#\"UPDATE TORDER SET x = 1 WHERE id = $1\"#; }",
        );
        assert!(
            touches(&rs).contains(&"TORDER"),
            "rust raw string table missed"
        );

        // C++ adjacent-literal concatenation: verb and table in separate fragments.
        let cpp = scan(
            Lang::Cpp,
            "void f() { auto q = \"SELECT id \" \"FROM TSETTLEMENT WHERE x=1\"; }",
        );
        assert!(
            touches(&cpp).contains(&"TSETTLEMENT"),
            "cpp concatenated-literal table missed"
        );
    }

    #[test]
    fn parser_never_panics_on_junk() {
        for lang in [Lang::Go, Lang::Rust, Lang::Cpp] {
            let _ = scan(lang, "\u{0}\u{1}not real code ```` \"unterminated");
        }
    }
}
