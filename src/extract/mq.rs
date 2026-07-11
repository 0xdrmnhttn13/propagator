//! Message-queue (Redpanda/Kafka) produce/consume extractor (`plan.md` §6.2).
//!
//! Two-pass per file:
//!   1. Build a `const_map` of identifier → string literal for topic constants.
//!   2. Scan call-site patterns; args may be literals or idents from step 1.
//!      Unresolvable args (e.g. `fmt.Sprintf`) → recorded as `unresolved_topics`.
//!
//! Patterns are a living list; this is the Go/Rust/C++ Kafka-library baseline.

use std::collections::HashMap;
use std::path::Path;

use regex::Regex;
use walkdir::WalkDir;

use crate::extract::{Extraction, RawEdge};
use crate::model::EdgeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Go,
    Rust,
    Cpp,
}

impl Lang {
    pub fn from_ext(ext: &str) -> Option<Self> {
        match ext {
            "go" => Some(Self::Go),
            "rs" => Some(Self::Rust),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "h" => Some(Self::Cpp),
            _ => None,
        }
    }
}

/// Directories that never hold first-party code worth scanning.
pub fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "vendor" | "node_modules" | "target" | "build" | "third_party" | ".idea"
    )
}

/// Scan a code source root (Go/Rust/C++, auto-detected per extension).
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
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let abs = path.to_string_lossy().to_string();
        scan_file(lang, &src, service, &abs, &mut out);
    }
    Ok(out)
}

fn scan_file(lang: Lang, src: &str, service: &str, path: &str, out: &mut Extraction) {
    let const_map = collect_consts(lang, src);
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let lineno = i + 1;
        for topic in consume_matches(lang, line, &const_map) {
            push_edge(out, EdgeKind::Consumes, service, &topic, path, lineno);
        }
        for topic in publish_matches(lang, line, &const_map) {
            push_edge(out, EdgeKind::Publishes, service, &topic, path, lineno);
        }
    }
}

fn push_edge(
    out: &mut Extraction,
    kind: EdgeKind,
    service: &str,
    topic: &str,
    path: &str,
    line: usize,
) {
    if topic.is_empty() || topic == "__UNRESOLVED__" {
        out.unresolved_topics
            .push((format!("{service}:{}", path), line));
        return;
    }
    out.edges.push(RawEdge {
        kind,
        from: service.to_string(),
        to: topic.to_string(),
        path: path.to_string(),
        line,
        from_kind: None,
    });
}

/// Per-language pattern set, compiled once (real corpora are millions of
/// lines — per-line `Regex::new` is the difference between ms and minutes).
struct LangPatterns {
    consts: Vec<Regex>,
    consume: Vec<Regex>,
    publish: Vec<Regex>,
}

fn compile(pats: &[&str]) -> Vec<Regex> {
    pats.iter()
        .map(|p| Regex::new(p).expect("mq regex"))
        .collect()
}

fn patterns(lang: Lang) -> &'static LangPatterns {
    use std::sync::OnceLock;
    static GO: OnceLock<LangPatterns> = OnceLock::new();
    static RUST: OnceLock<LangPatterns> = OnceLock::new();
    static CPP: OnceLock<LangPatterns> = OnceLock::new();
    match lang {
        Lang::Go => GO.get_or_init(|| LangPatterns {
            consts: compile(&[
                r#"const\s+(\w+)\s*=\s*"([^"]+)""#,
                r#"\t(\w+)\s*=\s*"([a-z][\w\-.]+)""#,
            ]),
            consume: compile(&[
                // kgo.ConsumeTopics(...) option form + client.AddConsumeTopics(...).
                r"(?:kgo\.|\.Add)ConsumeTopics\((.*?)\)",
                r"\.ConsumePartition\(\s*([^,]+)",
                r"Consume\([^,]+,\s*\[\]string\{(.*?)\}",
                r#"\.Consume\([^,]+,\s*\[?([^)\]]*)"#,
            ]),
            publish: compile(&[
                r"kgo\.DefaultProduceTopic\(\s*([^)]*)",
                r#"(?:Record|ProducerMessage)\{\s*(?:Topic|topic):?\s*([^,}]*)"#,
            ]),
        }),
        Lang::Rust => RUST.get_or_init(|| LangPatterns {
            consts: compile(&[r#"const\s+(\w+):\s*&str\s*=\s*"([^"]+)""#]),
            consume: compile(&[r#"\.subscribe\(&\[(.*?)\]"#]),
            publish: compile(&[r"(?:FutureRecord|BaseRecord)::to\(\s*([^)]*)"]),
        }),
        Lang::Cpp => CPP.get_or_init(|| LangPatterns {
            consts: compile(&[
                r#"constexpr\s+(?:auto|const\s+char\*)\s+(\w+)\s*=\s*"([^"]+)""#,
                r#"static\s+const\s+std::string\s+(\w+)\s*=\s*"([^"]+)""#,
            ]),
            consume: compile(&[
                r#"\.subscribe\(\{(.*?)\}\)"#,
                r"TopicPartition::create\(\s*([^,]+)",
            ]),
            publish: compile(&[
                r"Topic::create\(\s*[^,]+,\s*([^,]+)",
                // Quoted literal kept intact (resolve_args dequotes) or bare ident.
                r#"\.produce\(\s*("[^"]+"|\w+)"#,
            ]),
        }),
    }
}

/// Step 1: collect identifier → string-literal topic constants.
fn collect_consts(lang: Lang, src: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for re in &patterns(lang).consts {
        for c in re.captures_iter(src) {
            if let (Some(id), Some(val)) = (c.get(1), c.get(2)) {
                map.insert(id.as_str().to_string(), val.as_str().to_string());
            }
        }
    }
    map
}

/// Resolve raw argument text (possibly `a, b` list) against the const map.
/// Returns resolved topics; anything not a literal/ident → unresolved marker.
/// Quoted literals are checked before ident sigils are stripped, so both
/// `"order-events"` and `&TOPIC_X` resolve.
fn resolve_args(raw: &str, const_map: &HashMap<String, String>) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|tok| {
            if let Some(s) = tok.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                return s.to_string();
            }
            let ident = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            match const_map.get(ident) {
                Some(v) => v.clone(),
                None => "__UNRESOLVED__".to_string(),
            }
        })
        .collect()
}

fn consume_matches(lang: Lang, line: &str, const_map: &HashMap<String, String>) -> Vec<String> {
    matches_for(line, &patterns(lang).consume, const_map)
}

fn publish_matches(lang: Lang, line: &str, const_map: &HashMap<String, String>) -> Vec<String> {
    matches_for(line, &patterns(lang).publish, const_map)
}

fn matches_for(line: &str, regexes: &[Regex], const_map: &HashMap<String, String>) -> Vec<String> {
    let mut topics = Vec::new();
    for re in regexes {
        for c in re.captures_iter(line) {
            let Some(arg_grp) = c.get(1) else { continue };
            topics.extend(resolve_args(arg_grp.as_str(), const_map));
        }
    }
    topics
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn scan(lang: Lang, src: &str) -> Extraction {
        let mut out = Extraction::default();
        scan_file(lang, src, "svc", "f", &mut out);
        out
    }

    fn topics(out: &Extraction, kind: EdgeKind) -> Vec<(&str, usize)> {
        out.edges
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| (e.to.as_str(), e.line))
            .collect()
    }

    #[test]
    fn go_consume_publish_and_unresolved() {
        let src = r#"package risk
const TopicOrderEvents = "order-events"
const TopicRiskAlerts = "risk-alerts"

func run() {
	cl.AddConsumeTopics(TopicOrderEvents, "position-updates")
	dyn := fmt.Sprintf("orders-%s", region)
	cl.AddConsumeTopics(dyn)
	r := &kgo.Record{Topic: TopicRiskAlerts, Value: nil}
}
"#;
        let out = scan(Lang::Go, src);
        assert_eq!(
            topics(&out, EdgeKind::Consumes),
            vec![("order-events", 6), ("position-updates", 6)]
        );
        assert_eq!(topics(&out, EdgeKind::Publishes), vec![("risk-alerts", 9)]);
        // Sprintf-built ident must land in unresolved, not the graph.
        assert_eq!(out.unresolved_topics.len(), 1);
    }

    #[test]
    fn rust_const_indirection_and_literal() {
        let src = r#"
const TOPIC_ORDER_EVENTS: &str = "order-events";
fn go(p: &FutureProducer, c: &StreamConsumer) {
    let r = FutureRecord::to(TOPIC_ORDER_EVENTS);
    c.subscribe(&["fills"]).unwrap();
}
"#;
        let out = scan(Lang::Rust, src);
        assert_eq!(topics(&out, EdgeKind::Publishes), vec![("order-events", 4)]);
        assert_eq!(topics(&out, EdgeKind::Consumes), vec![("fills", 5)]);
        assert!(out.unresolved_topics.is_empty());
    }

    #[test]
    fn cpp_subscribe_and_produce() {
        let src = r#"
static const std::string kTopicRiskAlerts = "risk-alerts";
void run(cppkafka::Consumer& c, cppkafka::Producer& p) {
    c.subscribe({kTopicRiskAlerts, "trade-confirms"});
    p.produce("settlements", payload);
}
"#;
        let out = scan(Lang::Cpp, src);
        assert_eq!(
            topics(&out, EdgeKind::Consumes),
            vec![("risk-alerts", 4), ("trade-confirms", 4)]
        );
        assert_eq!(topics(&out, EdgeKind::Publishes), vec![("settlements", 5)]);
    }

    #[test]
    fn resolve_args_literal_ident_and_unresolved() {
        let mut map = HashMap::new();
        map.insert("TOPIC_X".to_string(), "topic-x".to_string());
        assert_eq!(
            resolve_args(r#""lit-a", TOPIC_X, &TOPIC_X, mystery"#, &map),
            vec!["lit-a", "topic-x", "topic-x", "__UNRESOLVED__"]
        );
    }

    #[test]
    fn extract_walks_fixture_dirs() {
        // Uses the real fixtures (cwd = crate root under cargo test).
        let go = extract(std::path::Path::new("fixtures/go"), "risk-engine").unwrap();
        assert!(
            go.edges
                .iter()
                .any(|e| e.kind == EdgeKind::Consumes && e.to == "order-events")
        );
        assert_eq!(go.unresolved_topics.len(), 1);

        let rs = extract(std::path::Path::new("fixtures/rust"), "order-gateway").unwrap();
        assert!(
            rs.edges
                .iter()
                .any(|e| e.kind == EdgeKind::Publishes && e.to == "order-events")
        );

        let cpp = extract(std::path::Path::new("fixtures/cpp"), "clearing-bridge").unwrap();
        assert!(
            cpp.edges
                .iter()
                .any(|e| e.kind == EdgeKind::Consumes && e.to == "risk-alerts")
        );
        assert!(
            cpp.edges
                .iter()
                .any(|e| e.kind == EdgeKind::Publishes && e.to == "settlements")
        );
    }

    proptest! {
        #[test]
        fn scan_never_panics(src in "\\PC*") {
            for lang in [Lang::Go, Lang::Rust, Lang::Cpp] {
                let mut out = Extraction::default();
                scan_file(lang, &src, "svc", "f", &mut out);
            }
        }
    }
}
