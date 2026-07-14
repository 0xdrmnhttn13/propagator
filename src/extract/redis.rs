//! Redis key-access extractor (Layer 3). Emits `ReadsKey`/`WritesKey` edges
//! (service → RedisKey) with the **key pattern** as node identity.
//!
//! ## Shared core vs. per-language front-end
//! Unlike the SQL sniffer (which runs on extracted string *content* and is
//! therefore language-agnostic), a Redis access has no SQL-like string to sniff
//! — the call *shape* is library-specific. So this module splits into:
//!   - **shared, agnostic core:** [`verb_class`] (read/write table, matched
//!     case-insensitively so `Get`/`get`/`GET` all resolve), [`normalize`] (the
//!     key-pattern ladder), edge emission, and the `RedisKey` model.
//!   - **per-language front-ends:** the call-site recognizer that finds a Redis
//!     call + its key argument. One per idiom:
//!       * **Go / go-redis** — `rdb.Verb(ctx, key, …)` (method verb). Full key
//!         ladder incl. local `var → pattern` resolution. Corpus-validated.
//!         Plus **auto-inferred project wrappers**: a service that hides go-redis
//!         behind its own helper (`repo.GeneralRedisGet(key)` →
//!         `rc.redis.Get(ctx, key)`) would otherwise go blind. [`discover_wrappers`]
//!         scans every method/func body in the service for a passthrough to a
//!         known verb whose key argument is one of the wrapper's own parameters,
//!         yielding `name → (verb, key-arg index)`. Call sites of those names are
//!         then recognized like native verbs — no per-project config. This is a
//!         *service-scoped* two-pass: discover across all files, then recognize.
//!       * **Rust / redis-rs** — `cmd("GET").arg(key)` and turbofish
//!         `con.get::<_,T>(key)`. Unit-validated (no Rust Redis in the corpus yet).
//!       * **C++ / hiredis** — `redisCommand(c, "GET key:%s", …)` (verb+key live
//!         in the printf format string). Unit-validated.
//!     Ambiguous bare-method forms (`con.get(k)` / `redis.get(k)`,
//!     indistinguishable from `map.get(k)`) are intentionally skipped.
//!
//! ## Key ladder
//! literal (`"test-key"`) → const/local var (`key := "sess:" + id` → `sess:*`)
//! → fmt (`Sprintf("order:%s")` / printf `"GET order:%s"` → `order:*`) →
//! dynamic (unresolved → honesty ledger, `Extraction::dynamic_redis_keys`). A
//! pattern with no literal anchor (`"%s:%d"` → `*:*`) is treated as dynamic — a
//! `*`-only node is noise, not identity.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use tree_sitter::Node;

use crate::extract::mq::Lang;
use crate::extract::{Extraction, RawEdge};
use crate::model::{EdgeKind, NodeKind};

/// Read verbs (value/observe, no mutation) — canonical UPPERCASE. Incoming
/// verbs are uppercased before lookup, so go-redis `HGetAll`, redis-rs `hgetall`
/// and a raw `HGETALL` command string all match.
const READ_VERBS: &[&str] = &[
    "GET",
    "GETEX",
    "GETRANGE",
    "MGET",
    "STRLEN",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "HGET",
    "HGETALL",
    "HMGET",
    "HKEYS",
    "HVALS",
    "HEXISTS",
    "HLEN",
    "LRANGE",
    "LINDEX",
    "LLEN",
    "LPOS",
    "SMEMBERS",
    "SISMEMBER",
    "SCARD",
    "SRANDMEMBER",
    "ZRANGE",
    "ZSCORE",
    "ZRANK",
    "ZCARD",
    "ZRANGEBYSCORE",
    "ZCOUNT",
    "KEYS",
    "SCAN",
    "BITCOUNT",
    "DUMP",
];

/// Write verbs (mutate value/key/ttl) — canonical UPPERCASE.
const WRITE_VERBS: &[&str] = &[
    "SET",
    "SETEX",
    "SETNX",
    "SETXX",
    "MSET",
    "MSETNX",
    "GETSET",
    "GETDEL",
    "APPEND",
    "DEL",
    "UNLINK",
    "EXPIRE",
    "EXPIREAT",
    "PEXPIRE",
    "PERSIST",
    "RENAME",
    "RENAMENX",
    "SETRANGE",
    "SETBIT",
    "INCR",
    "INCRBY",
    "DECR",
    "DECRBY",
    "INCRBYFLOAT",
    "HSET",
    "HMSET",
    "HSETNX",
    "HDEL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LSET",
    "LREM",
    "LTRIM",
    "SADD",
    "SREM",
    "SPOP",
    "SMOVE",
    "ZADD",
    "ZREM",
    "ZINCRBY",
    "ZREMRANGEBYSCORE",
];

/// Shared: classify a verb (any case) as a read or write access, else `None`.
fn verb_class(verb: &str) -> Option<EdgeKind> {
    let up = verb.to_ascii_uppercase();
    if READ_VERBS.contains(&up.as_str()) {
        Some(EdgeKind::ReadsKey)
    } else if WRITE_VERBS.contains(&up.as_str()) {
        Some(EdgeKind::WritesKey)
    } else {
        None
    }
}

/// A service-scoped registry of auto-inferred redis wrapper methods:
/// `wrapper-method-name → (access kind, index of the key argument)`. Built by
/// [`discover_wrappers`] before call-site recognition runs.
pub type RedisWrappers = HashMap<String, (EdgeKind, usize)>;

/// Entry point. `root` is the already-parsed tree root (one parse per file,
/// shared with `code.rs`). Dispatches to the per-language front-end. `wrappers`
/// carries service-scoped inferred wrappers (Go only; empty for other langs).
pub fn scan(
    lang: Lang,
    root: Node,
    src: &[u8],
    service: &str,
    path: &str,
    wrappers: &RedisWrappers,
    out: &mut Extraction,
) {
    match lang {
        Lang::Go => scan_go(root, src, service, path, wrappers, out),
        Lang::Rust => scan_generic(root, src, service, path, out, rust_call),
        Lang::Cpp => scan_generic(root, src, service, path, out, cpp_call),
    }
}

/// Discovery pass (Go). Scan one file's method/function declarations for a body
/// that passes one of its own parameters as the key to a native go-redis verb
/// call, and record `name → (verb kind, param index)` into `out`. Run over every
/// Go file in a service before call-site recognition — wrapper definition and
/// use routinely live in different files of the same service.
pub fn discover_wrappers(root: Node, src: &[u8], out: &mut RedisWrappers) {
    if matches!(root.kind(), "method_declaration" | "function_declaration") {
        if let (Some(name), Some(params), Some(body)) = (
            root.child_by_field_name("name"),
            root.child_by_field_name("parameters"),
            root.child_by_field_name("body"),
        ) {
            if let Some((kind, key_node)) = first_go_redis_call(body, src) {
                let key = text(key_node, src);
                if let Some(idx) = param_names(params, src).iter().position(|p| *p == key) {
                    out.entry(text(name, src)).or_insert((kind, idx));
                }
            }
        }
    }
    let mut c = root.walk();
    for child in root.children(&mut c) {
        discover_wrappers(child, src, out);
    }
}

/// Positional parameter identifier names of a Go `parameter_list`. A grouped
/// declaration (`key, field string`) contributes both names in order; a variadic
/// (`fields ...string`) contributes its single name. Type nodes (`type_identifier`,
/// `qualified_type`, …) are not `identifier`, so they are skipped.
fn param_names(params: Node, src: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut c = params.walk();
    for decl in params.named_children(&mut c) {
        if matches!(
            decl.kind(),
            "parameter_declaration" | "variadic_parameter_declaration"
        ) {
            let mut cc = decl.walk();
            for ch in decl.named_children(&mut cc) {
                if ch.kind() == "identifier" {
                    names.push(text(ch, src));
                }
            }
        }
    }
    names
}

/// First native go-redis verb call in a subtree (pre-order), for wrapper body
/// inspection.
fn first_go_redis_call<'a>(node: Node<'a>, src: &[u8]) -> Option<(EdgeKind, Node<'a>)> {
    if node.kind() == "call_expression" {
        if let Some(hit) = go_redis_call(node, src) {
            return Some(hit);
        }
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if let Some(hit) = first_go_redis_call(child, src) {
            return Some(hit);
        }
    }
    None
}

fn emit(
    out: &mut Extraction,
    kind: EdgeKind,
    service: &str,
    pattern: String,
    path: &str,
    line: usize,
) {
    out.edges.push(RawEdge {
        kind,
        from: service.to_string(),
        to: pattern,
        path: path.to_string(),
        line,
        from_kind: Some(NodeKind::Service),
    });
}

// ---------------------------------------------------------------------------
// Go / go-redis front-end (with full local var ladder)
// ---------------------------------------------------------------------------

fn scan_go(
    root: Node,
    src: &[u8],
    service: &str,
    path: &str,
    wrappers: &RedisWrappers,
    out: &mut Extraction,
) {
    let mut vars: HashMap<String, String> = HashMap::new();
    collect_key_vars(root, src, &mut vars);
    walk_go_calls(root, src, &vars, wrappers, service, path, out);
}

/// Pass 1 — record identifiers assigned a resolvable key pattern.
fn collect_key_vars(node: Node, src: &[u8], vars: &mut HashMap<String, String>) {
    if matches!(
        node.kind(),
        "short_var_declaration" | "assignment_statement"
    ) {
        if let (Some(left), Some(right)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) {
            if left.named_child_count() == 1 && right.named_child_count() == 1 {
                let lhs = left.named_child(0).unwrap();
                let rhs = right.named_child(0).unwrap();
                if lhs.kind() == "identifier" {
                    if let Some(pat) = resolve_go_key(rhs, src, vars) {
                        vars.insert(text(lhs, src), pat);
                    }
                }
            }
        }
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        collect_key_vars(child, src, vars);
    }
}

fn walk_go_calls(
    node: Node,
    src: &[u8],
    vars: &HashMap<String, String>,
    wrappers: &RedisWrappers,
    service: &str,
    path: &str,
    out: &mut Extraction,
) {
    if node.kind() == "call_expression" {
        // Native go-redis verb first, then an inferred project wrapper.
        if let Some((kind, key_node)) =
            go_redis_call(node, src).or_else(|| go_wrapper_call(node, src, wrappers))
        {
            match resolve_go_key(key_node, src, vars) {
                Some(pattern) => emit(
                    out,
                    kind,
                    service,
                    pattern,
                    path,
                    node.start_position().row + 1,
                ),
                None => out.dynamic_redis_keys += 1,
            }
        }
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        walk_go_calls(child, src, vars, wrappers, service, path, out);
    }
}

/// An inferred wrapper call `recv.Wrapper(args…)` — verb + key-arg position come
/// from [`discover_wrappers`]. Unlike a native verb call the key need not follow
/// a `ctx` arg; its index is whatever the wrapper threads through to go-redis.
fn go_wrapper_call<'a>(
    call: Node<'a>,
    src: &[u8],
    wrappers: &RedisWrappers,
) -> Option<(EdgeKind, Node<'a>)> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "selector_expression" {
        return None;
    }
    let method = text(func.child_by_field_name("field")?, src);
    let &(kind, idx) = wrappers.get(&method)?;
    let key_node = call
        .child_by_field_name("arguments")?
        .named_child(idx as u32)?;
    Some((kind, key_node))
}

/// go-redis `recv.Verb(ctx, key, …)` — `ctx`-first-arg discriminates it from
/// `myMap.Get(k)`. Returns access kind + key-arg node.
fn go_redis_call<'a>(call: Node<'a>, src: &[u8]) -> Option<(EdgeKind, Node<'a>)> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "selector_expression" {
        return None;
    }
    let kind = verb_class(&text(func.child_by_field_name("field")?, src))?;
    let args = call.child_by_field_name("arguments")?;
    if args.named_child_count() < 2 {
        return None;
    }
    let first = text(args.named_child(0)?, src).to_ascii_lowercase();
    if !(first.contains("ctx") || first.contains("context")) {
        return None;
    }
    Some((kind, args.named_child(1)?))
}

/// Go key ladder: literal / local var / `Sprintf` / `"pfx"+x` concat / dynamic.
fn resolve_go_key(node: Node, src: &[u8], vars: &HashMap<String, String>) -> Option<String> {
    match node.kind() {
        "interpreted_string_literal" | "raw_string_literal" => normalize(&unquote(node, src)),
        "identifier" => vars.get(&text(node, src)).cloned(),
        "call_expression" => sprintf_pattern(node, src),
        "binary_expression" => {
            leading_literal(node, src).and_then(|lit| normalize(&format!("{lit}*")))
        }
        _ => None,
    }
}

/// `fmt.Sprintf("order:%s", id)` → `order:*`.
fn sprintf_pattern(call: Node, src: &[u8]) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "selector_expression" {
        return None;
    }
    if text(func.child_by_field_name("field")?, src) != "Sprintf" {
        return None;
    }
    let first = call.child_by_field_name("arguments")?.named_child(0)?;
    if !matches!(
        first.kind(),
        "interpreted_string_literal" | "raw_string_literal"
    ) {
        return None;
    }
    normalize(&unquote(first, src))
}

/// Leftmost string-literal content of a `+`-concatenation.
fn leading_literal(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "interpreted_string_literal" | "raw_string_literal" => Some(unquote(node, src)),
        "binary_expression" => leading_literal(node.child_by_field_name("left")?, src),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rust + C++ front-ends: a call recognizer returns (kind, key-pattern) directly.
// No cross-statement var resolution (var key → dynamic) for these idioms yet.
// ---------------------------------------------------------------------------

type CallFn = for<'a> fn(Node<'a>, &[u8]) -> Option<(EdgeKind, Option<String>)>;

/// Generic walker: apply a per-language call recognizer to every call node.
/// The recognizer yields `Some((kind, Some(pattern)))` for a resolved key,
/// `Some((kind, None))` for a dynamic key (ledger), or `None` if not Redis.
fn scan_generic(
    node: Node,
    src: &[u8],
    service: &str,
    path: &str,
    out: &mut Extraction,
    recog: CallFn,
) {
    if node.kind() == "call_expression" {
        if let Some((kind, pattern)) = recog(node, src) {
            match pattern {
                Some(p) => emit(out, kind, service, p, path, node.start_position().row + 1),
                None => out.dynamic_redis_keys += 1,
            }
        }
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        scan_generic(child, src, service, path, out, recog);
    }
}

/// redis-rs. Two high-signal forms:
///   1. `cmd("GET").arg(key)` — this node is the `.arg(key)` call whose receiver
///      chain roots in `cmd("VERB")`; verb from the literal, key = first arg.
///   2. turbofish `con.get::<_, T>(key)` — `generic_function` marks it Redis;
///      verb = method name, key = first arg.
fn rust_call<'a>(call: Node<'a>, src: &[u8]) -> Option<(EdgeKind, Option<String>)> {
    let func = call.child_by_field_name("function")?;
    // Form 1: `<recv>.arg(key)` where recv chain roots in `cmd("VERB")`.
    if func.kind() == "field_expression" && text(func.child_by_field_name("field")?, src) == "arg" {
        if let Some(verb) = cmd_verb(func.child_by_field_name("value")?, src) {
            if let Some(kind) = verb_class(&verb) {
                let key = call
                    .child_by_field_name("arguments")?
                    .named_child(0)
                    .and_then(|n| rust_literal_key(n, src));
                return Some((kind, key));
            }
        }
    }
    // Form 2: turbofish method `con.get::<...>(key)`.
    if func.kind() == "generic_function" {
        let inner = func.child_by_field_name("function")?;
        if inner.kind() == "field_expression" {
            let kind = verb_class(&text(inner.child_by_field_name("field")?, src))?;
            let key = call
                .child_by_field_name("arguments")?
                .named_child(0)
                .and_then(|n| rust_literal_key(n, src));
            return Some((kind, key));
        }
    }
    None
}

/// Root a redis-rs builder chain: does `node` reduce to `cmd("VERB")` /
/// `redis::cmd("VERB")`? Returns the verb literal.
fn cmd_verb(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        // `cmd("GET")` or `redis::cmd("GET")`.
        "call_expression" => {
            let f = node.child_by_field_name("function")?;
            let name = match f.kind() {
                "identifier" => text(f, src),
                "scoped_identifier" => text(f.child_by_field_name("name")?, src),
                _ => return None,
            };
            if name != "cmd" {
                return None;
            }
            let first = node.child_by_field_name("arguments")?.named_child(0)?;
            Some(unquote(first, src))
        }
        // Peel a preceding `.arg(...)` in the chain to reach `cmd(...)`.
        "field_expression" => cmd_verb(node.child_by_field_name("value")?, src),
        _ => None,
    }
}

fn rust_literal_key(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "string_literal" | "raw_string_literal" => normalize(&unquote(node, src)),
        // `format!("order:{}", id)` → treat `{}` as `*`.
        "macro_invocation" => rust_format_key(node, src),
        _ => None,
    }
}

fn rust_format_key(node: Node, src: &[u8]) -> Option<String> {
    // Only `format!` / `formatcp!`-style: first token-tree string literal.
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() == "token_tree" {
            let mut cc = ch.walk();
            for t in ch.children(&mut cc) {
                if matches!(t.kind(), "string_literal" | "raw_string_literal") {
                    // Rust format uses `{}` placeholders → map to `*`.
                    let lit = unquote(t, src).replace("{}", "*");
                    return normalize(&lit);
                }
            }
        }
    }
    None
}

/// hiredis `redisCommand(ctx, "VERB keytemplate …", …)`. Verb + key template
/// live in the printf format string (arg after the context). redis-plus-plus
/// bare-method form is intentionally not matched (ambiguous).
fn cpp_call<'a>(call: Node<'a>, src: &[u8]) -> Option<(EdgeKind, Option<String>)> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "identifier" {
        return None;
    }
    let name = text(func, src);
    if !matches!(
        name.as_str(),
        "redisCommand" | "redisCommandArgv" | "redisAsyncCommand"
    ) {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    // First string-literal arg = the command format string.
    let mut c = args.walk();
    let fmt = args.children(&mut c).find(|n| {
        matches!(
            n.kind(),
            "string_literal" | "raw_string_literal" | "concatenated_string"
        )
    })?;
    let content = unquote(fmt, src);
    let mut toks = content.split_whitespace();
    let verb = toks.next()?;
    let kind = verb_class(verb)?;
    // Key template = token after the verb (printf `%s`/`%d` → `*`).
    let key = toks.next().and_then(normalize);
    Some((kind, key))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Shared normalizer: printf/Sprintf verbs (`%s`, `%02d`) → `*`. Returns `None`
/// when the result has no literal anchor (all `*`/punctuation) — a `*`-only key
/// is noise, not identity, so it drops to the dynamic ledger.
fn normalize(raw: &str) -> Option<String> {
    static FMT: OnceLock<Regex> = OnceLock::new();
    let fmt = FMT.get_or_init(|| Regex::new(r"%[-+ #0-9.*]*[a-zA-Z]").expect("fmt regex"));
    let mut pat = fmt.replace_all(raw, "*").into_owned();
    while pat.contains("**") {
        pat = pat.replace("**", "*");
    }
    let pat = pat.trim().to_string();
    pat.chars().any(|c| c.is_alphanumeric()).then_some(pat)
}

/// Strip string delimiters (`"…"`, `` `…` ``, Rust `r#"…"#`, `f"…"`).
fn unquote(node: Node, src: &[u8]) -> String {
    let t = text(node, src);
    let t = t.trim();
    // Rust raw string sigils.
    let t = t.trim_start_matches(['r', 'b', 'f']);
    let t = t.trim_matches('#');
    let bytes = t.as_bytes();
    if bytes.len() >= 2 {
        let (f, l) = (bytes[0], bytes[bytes.len() - 1]);
        if (f == b'"' && l == b'"') || (f == b'`' && l == b'`') {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn scan_lang(lang: Lang, tsl: tree_sitter::Language, src: &str) -> Extraction {
        let mut parser = Parser::new();
        parser.set_language(&tsl).unwrap();
        let tree = parser.parse(src, None).unwrap();
        // Discover wrappers over the same file first (mirrors the service-wide
        // pre-pass), then recognize call sites.
        let mut wrappers = RedisWrappers::default();
        if matches!(lang, Lang::Go) {
            discover_wrappers(tree.root_node(), src.as_bytes(), &mut wrappers);
        }
        let mut out = Extraction::default();
        scan(
            lang,
            tree.root_node(),
            src.as_bytes(),
            "svc",
            "f",
            &wrappers,
            &mut out,
        );
        out
    }
    fn go(src: &str) -> Extraction {
        scan_lang(Lang::Go, tree_sitter_go::LANGUAGE.into(), src)
    }
    fn rust(src: &str) -> Extraction {
        scan_lang(Lang::Rust, tree_sitter_rust::LANGUAGE.into(), src)
    }
    fn cpp(src: &str) -> Extraction {
        scan_lang(Lang::Cpp, tree_sitter_cpp::LANGUAGE.into(), src)
    }
    fn e(out: &Extraction, k: EdgeKind) -> Vec<&str> {
        out.edges
            .iter()
            .filter(|e| e.kind == k)
            .map(|e| e.to.as_str())
            .collect()
    }

    #[test]
    fn go_literal_concat_sprintf_and_dynamic() {
        let out = go(r#"package p
func f(rdb *redis.Client, ctx context.Context, id string) {
    rdb.Get(ctx, "test-key")
    rdb.Set(ctx, "test-key", 1, 0)
    k := "TRADELISTHIST::" + id
    rdb.Get(ctx, k)
    rdb.HGetAll(ctx, fmt.Sprintf("order:%s:detail", id))
    rdb.Get(ctx, fmt.Sprintf("%s:%d", id, id))
    rdb.Expire(ctx, unknownVar, 0)
}"#);
        assert!(e(&out, EdgeKind::ReadsKey).contains(&"test-key"));
        assert!(e(&out, EdgeKind::WritesKey).contains(&"test-key"));
        assert!(e(&out, EdgeKind::ReadsKey).contains(&"TRADELISTHIST::*"));
        assert!(e(&out, EdgeKind::ReadsKey).contains(&"order:*:detail"));
        assert_eq!(out.dynamic_redis_keys, 2, "%s:%d + unknownVar → ledger");
        assert_eq!(out.edges[0].from_kind, Some(NodeKind::Service));
    }

    #[test]
    fn go_inferred_wrapper_calls_resolve() {
        // A service hides go-redis behind `GeneralRedis*` helpers. Discovery must
        // infer verb + key-arg index from the passthrough body, so call sites
        // (key as arg 0, no ctx; and ctx-first Del with key as arg 1) resolve.
        let out = go(r#"package p
func (rc *RedisClientRepo) GeneralRedisGet(key string) (string, error) {
    return rc.redis.Get(context.Background(), key).Result()
}
func (rc *RedisClientRepo) GeneralRedisHGET(key, field string) string {
    result, _ := rc.redis.HGet(context.Background(), key, field).Result()
    return result
}
func (rc *RedisClientRepo) GeneralRedisDel(ctx context.Context, key string) {
    rc.redis.Del(ctx, key)
}
func use(odu *Svc, id string) {
    redisKey := "GTC:SetExpiryDatetoRedis:" + id
    odu.r.GeneralRedisGet(redisKey)
    odu.r.GeneralRedisHGET("STOCKRECOMMENDATION::static", "f")
    odu.r.GeneralRedisDel(ctx, "auto_order_"+id)
}"#);
        let reads = e(&out, EdgeKind::ReadsKey);
        assert!(
            reads.contains(&"GTC:SetExpiryDatetoRedis:*"),
            "wrapper Get with local-var key missed: {reads:?}"
        );
        assert!(
            reads.contains(&"STOCKRECOMMENDATION::static"),
            "wrapper HGET literal key missed: {reads:?}"
        );
        assert!(
            e(&out, EdgeKind::WritesKey).contains(&"auto_order_*"),
            "ctx-first wrapper Del (key at arg 1) missed"
        );
    }

    #[test]
    fn go_non_wrapper_method_not_inferred() {
        // A same-shaped helper that never threads a param into a redis verb must
        // NOT be registered as a wrapper — no phantom key edges from `Load`.
        let out = go(r#"package p
func (s *S) Load(key string) string { return s.cache[key] }
func use(s *S) { s.Load("not-a-redis-key") }"#);
        assert!(out.edges.is_empty(), "non-redis helper wrongly inferred");
    }

    #[test]
    fn go_non_redis_map_get_ignored() {
        let out = go("package p\nfunc f(m map[string]int) { _ = m.Get(\"x\") }");
        assert!(out.edges.is_empty());
    }

    #[test]
    fn rust_cmd_arg_and_turbofish() {
        let out = rust(
            r#"fn f(con: &mut Connection) {
    let _: () = redis::cmd("SET").arg("user:42").arg(1).query(con).unwrap();
    let _: String = con.get::<_, String>("session:abc").unwrap();
}"#,
        );
        assert!(
            e(&out, EdgeKind::WritesKey).contains(&"user:42"),
            "cmd-arg SET missed"
        );
        assert!(
            e(&out, EdgeKind::ReadsKey).contains(&"session:abc"),
            "turbofish get missed"
        );
    }

    #[test]
    fn rust_format_key_pattern() {
        let out = rust(
            r#"fn f(con: &mut Connection, id: u64) {
    let _: () = redis::cmd("HSET").arg(format!("order:{}", id)).query(con).unwrap();
}"#,
        );
        assert!(
            e(&out, EdgeKind::WritesKey).contains(&"order:*"),
            "format! key missed"
        );
    }

    #[test]
    fn cpp_hiredis_format_string() {
        let out = cpp(r#"void f(redisContext* c) {
    redisReply* r = (redisReply*)redisCommand(c, "GET session:%s", id);
    redisCommand(c, "HSET order:%s field %s", oid, val);
}"#);
        assert!(
            e(&out, EdgeKind::ReadsKey).contains(&"session:*"),
            "hiredis GET missed"
        );
        assert!(
            e(&out, EdgeKind::WritesKey).contains(&"order:*"),
            "hiredis HSET missed"
        );
    }

    #[test]
    fn all_langs_never_panic() {
        for (lang, l) in [
            (Lang::Go, tree_sitter_go::LANGUAGE.into()),
            (Lang::Rust, tree_sitter_rust::LANGUAGE.into()),
            (Lang::Cpp, tree_sitter_cpp::LANGUAGE.into()),
        ] {
            let _ = scan_lang(lang, l, "\u{0} garbage ``` \"unterminated");
        }
    }
}
