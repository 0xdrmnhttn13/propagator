# propagator

**Dependency & message-flow tracing for trading infrastructure — right from your editor.**

> If I change `some_function`, what breaks? Propagator answers that
> question without you opening six repositories and tracing call chains
> by hand.

---

## What problem does this solve?

Modern trading systems are a tangle of **microservices** (Go / Rust / C++)
that talk over **Kafka / Redpanda topics**, backed by **Oracle stored
procedures** that call each other and touch hundreds of tables.

When you change one procedure, one topic payload, or one function, you need
to know the **blast radius** — everything downstream that could be affected.
Doing this manually means switching between repos, grepping for call sites,
tracing message flows, and hoping you didn't miss anything.

**Propagator builds a graph of all of that** so you can ask
"who depends on this?" or "what is impacted if this changes?" and get a
precise answer in milliseconds — either from the CLI or from inside your
AI coding assistant (Claude Code, OpenCode) via MCP.

---

## The 30-second mental model

Propagator scans your code and SQL and builds a **heterogeneous graph**:

```
                         ┌──────────┐
   Go/Rust/C++ code ───► │ Service  │──Publishes──►┌────────┐
   (Kafka produce/       └──────────┘              │ Topic  │
    consume)             ┌──────────┐◄──Consumes──►└────────┘
                         │ Service  │
                         └──────────┘

   Oracle PL/SQL  ───►  ┌──────────┐──Calls──►┌───────────┐
   (proc definitions)    │ Procedure│          │ Procedure │
                         └──────────┘──Touches►┌───────────┐
                                     └────────►│  Table    │
                                               └───────────┘
```

**Five node types:** `Service`, `Topic`, `Procedure`, `Function`, `Table`
**Eight edge types:** `Calls`, `Touches`, `Publishes`, `Consumes`, `Owns`, `Invokes`, `ReadsKey`, `WritesKey`

The graph is stored in a single binary file (`store.bin`). The MCP server
loads it once and **auto-reloads** when the file changes — no restart needed
after a re-sync.

---

## Quick start

### 1. Build

```bash
git clone https://github.com/0xdrmnhttn13/propagator.git
cd propagator
cargo install --path .
```

### 2. Configure — `propagator.toml`

Create this file in the root of your workspace (the folder that contains
your repos). Paths are resolved relative to this file, not your cwd.

```toml
[store]
path = ".propagator/store.bin"

# A Go/Rust/C++ repo → auto-detects Kafka produce/consume edges
[[sources]]
kind = "code"
path = "my-order-service"
service = "order-service"

# An Oracle PL/SQL repo → extracts procedure/function definitions
# (Calls/Touches edges come from a data-dictionary dump, see below)
[[sources]]
kind = "sql"
path = "my-risk-service"
service = "risk-service"
```

### 3. Sync — build the graph

```bash
cd ~/work              # wherever your propagator.toml lives
propagator sync
```

Output:
```
synced: services=3 defs=847 edges=1932 unresolved_topics=12 ...
store: ~/work/.propagator/store.bin
```

### 4. Query — find out what's connected

```bash
# Who calls func_buy (and who calls THEM)?
propagator callers func_buy --depth 2

# What does func_new_order depend on?
propagator deps func_new_order

# Blast radius: everything impacted if this table changes
propagator impact order --depth 5

# Who produces / consumes this Kafka topic?
propagator topic order-events
```

### 5. (Optional) Oracle data-dictionary dump

For Oracle `Calls` (proc → proc) and `Touches` (proc → table) edges,
propagator needs a dependency dump from `ALL_DEPENDENCIES`. It can emit
the SQL for you:

```bash
propagator dd-query MY_SCHEMA
```

Paste the output into SQLcl (connected to your Oracle), spool the result
to `~/work/.propagator/<service-name>-dependencies.csv`, then re-run
`propagator sync`. The CSV filename **must** use the service name from
your config (e.g. `risk-service-dependencies.csv`).

---

## Using it from your AI assistant (MCP)

This is where propagator shines. Register it as an MCP server and your
AI coding assistant can query the graph directly — no context switching.

### Claude Code (`.mcp.json` in your workspace root)

```json
{
  "mcpServers": {
    "propagator": {
      "command": "propagator",
      "args": ["serve", "--config", "/absolute/path/to/propagator.toml"]
    }
  }
}
```

> Pinning `--config` to an absolute path makes the corpus deterministic —
> it won't accidentally follow the AI's current working directory.

### Seven MCP tools

| Tool | What it answers | When to use it |
|------|-----------------|----------------|
| `describe_corpus` | "What's in the graph?" — counts + samples + coverage signals | Call **once per session** to orient yourself |
| `get_symbol` | "What is this symbol?" — kind, schema, definition location | Verify a symbol exists before deeper queries |
| `get_callers` | "Who references this?" — reverse traversal | Find direct dependents (shallow) |
| `get_dependencies` | "What does this depend on?" — forward traversal | See what a proc/function touches |
| `get_impact` | "Full blast radius if this changes" — transitive, grouped by depth | **The main event.** Use this instead of chaining `get_callers` |
| `get_topic` | "Who produces / consumes this topic?" — with file:line evidence | Trace a Kafka topic end-to-end |
| `get_chunk` | "Show me the source of this symbol" — signature or body | Ground the AI in the real code before generating tests |

#### Example session

```
You:     I need to change the order table. What will be impacted?

AI:      → get_impact order (depth 5)

         d1: [new-order:Procedure via:Touches; amen:Procedure via:Touches]
         d2: [risk-service:Service via:Invokes; ...]
         d3: [out-topic:Topic via:Publishes]
         d4: [order-service:Service via:Consumes]

         "Changing order impacts 2 stored procedures, which propagate
          through risk-service to the out-topic topic, which
          is consumed by order-service."
```

---

## CLI command reference

```
propagator sync                       Build / rebuild the graph from all configured sources
propagator callers <SYMBOL> [-d N]     Who references SYMBOL? (reverse, depth N)
propagator deps <SYMBOL>     [-d N]    What does SYMBOL depend on? (forward, depth N)
propagator impact <SYMBOL>   [-d N]    Full blast radius of SYMBOL (transitive)
propagator topic <NAME>                Producers + consumers of a Kafka topic
propagator describe                    Corpus health: counts, coverage, honesty signals
propagator dd-query <SCHEMA>           Emit the Oracle ALL_DEPENDENCIES SQL to run in SQLcl
propagator affected [--fail-on X]      Blast radius of uncommitted git changes (pipe git diff on stdin)
propagator serve [--config FILE]       Start the MCP server on stdio
```

### Pre-commit gate: `propagator affected`

Pipe your `git diff` to see the blast radius of uncommitted changes before
you push:

```bash
git diff | propagator affected --fail-on cross-service
```

Exits non-zero if the impact crosses into another service — perfect for a
pre-push hook.

---

## Configuration reference

### `propagator.toml`

| Section | Field | Description |
|---------|-------|-------------|
| `[store]` | `path` | Where to write `store.bin` (relative to the config file) |
| `[[sources]]` | `kind` | `"code"` (Go/Rust/C++) or `"sql"` (Oracle PL/SQL) |
| | `path` | Repo directory (relative to the config file) |
| | `service` | Logical service name used as the graph node label |

### `topics.toml` — resolving dynamic topic names

Some Kafka topic names are built at runtime (`fmt.Sprintf`, env-based) and
can't be statically extracted. Those show up as **unresolved** after sync.
Declare them manually:

```toml
[order-service]
publishes = ["in-topic"]
consumes  = ["out-topic"]
```

### Producer provenance — preventing false-positive "hubs"

A known over-reporting risk: a `Service` node is a promiscuous hub. Impact
can flow `table → proc → Service → topic → consumer`, making it look like
*every* table touched by *any* proc in that service reaches *every* consumer.

Fix: declare which tables actually feed each topic's payload. Impact only
crosses the `Publishes` edge for tables in the list.

```toml
[risk-service]
publishes = ["out-topic"]

[risk-service.provenance]
"out-topic" = ["order", "trade"]
```

- **Non-empty list = whitelist** → tables outside the list do NOT flow to this topic's consumers.
- **Absent / empty = unknown** → no restriction (old behavior, zero regression). Opt-in per topic.

Run `propagator describe` and check the `topic bridges` line to see which
topics still lack provenance (labeled `COARSE`).

---

## How extraction works

| Source kind | Language | What's extracted | How |
|-------------|----------|------------------|-----|
| `code` | Go | Service + Publishes/Consumes | Regex on franz-go, sarama, rdkafka call sites |
| `code` | Rust | Service + Publishes/Consumes | Regex on rdkafka call sites |
| `code` | C++ | Service + Publishes/Consumes | Regex on cppkafka call sites |
| `sql` | PL/SQL | Procedure/Function/Table + Calls/Touches | Regex on `CREATE OR REPLACE` boundaries + Oracle dd dump |

Plus: `Invokes` edges (code calling stored procedures by string literal),
`Owns` edges (a service owns its procs/functions), and Redis `ReadsKey` /
`WritesKey` edges.

Dynamic SQL and runtime-built topic names are the known gaps — that's what
`topics.toml` fills in.

---

## Honesty about the graph

Propagator is deliberately transparent about two directions of error:

1. **Under-reporting** — dynamic SQL, config-injected topics, and missing
   Oracle dd dumps mean some edges are absent. An empty `get_impact` result
   is a *weak signal*, not proof of safety. The `describe_corpus` tool
   flags services missing dd dumps.

2. **Over-reporting** — the Service-hub problem described above. Use
   `provenance` to guard it. `describe_corpus` flags unguarded topic bridges.

---

## How this fits with the other tools

Propagator is part of a three-tool platform:

| Tool | Question it answers | Method |
|------|---------------------|--------|
| **propagator** | What is **definitely** connected? | Deterministic graph traversal |
| [**braket**](https://github.com/0xdrmnhttn13/braket) | What is **probably** relevant? | Semantic similarity (vector search) |
| [**interferometer**](https://github.com/0xdrmnhttn13/interferometer) | What does my AI workflow **cost**? | Token usage tracking from session logs |

Use propagator when you need **exact** dependency / blast-radius answers.
Use braket when you're searching by **meaning** ("where is the settlement
fee calculated?") rather than by symbol name.

---

## Is it worth it? — cost is not the whole story

When you A/B propagator against a no-graph baseline (e.g. `mcp` vs `nomcp`
arms via `interferometer bench`), the token/cost delta is only half the
answer. **A cheaper arm that failed the task is not a win** — it just
spent less to be wrong (missed a transitive caller, shallow blast radius,
wrong topic producer).

Always pair the cost numbers with a success judgment:

```bash
interferometer bench judge <job-id> success   # or partial | fail
```

Two rules when reading a benchmark table:

1. **Exclude non-done jobs from the averages.** Failed / timed-out runs
   drag the mean down and make the cheaper arm look falsely efficient.
   Confirm the row counts (`n`) match *completed* jobs, not all attempts.
2. **Cheapest ≠ best.** An arm can be 30 % cheaper and still land
   `partial` / `fail` on the judged outcome. Trust the verdict, not just
   `Δcost%`. Comparing tokens alone is misleading when the frugal arm
   didn't actually finish the task.

Call a configuration "worth it" only when it is *both* cheaper (or
comparable) *and* at least as correct on the judged result.

### A/B results — MCP (propagator + braket) vs no-MCP (grep / code reading)

Three representative tasks, judged `status=done` only (failed/timed-out
runs excluded per rule 1 above). *Negative Δ = MCP is cheaper.*

| task | arm | n | avg cost | Δcost% | avg cache_read | cache_hit | verdict |
|------|-----|---|----------|--------|----------------|-----------|---------|
| cross-service-impact | mcp | 4 | $0.6146 | **−20.1 %** ✅ | 661 687 | 94 % | cheaper |
| cross-service-impact | nomcp | 1 | $0.7689 | — | 1 223 839 | 96 % | — |
| orderservice-callers | mcp | 5 | $0.2706 | **−31.8 %** ✅ | 309 094 | 94 % | cheaper |
| orderservice-callers | nomcp | 5 | $0.3966 | — | 557 097 | 93 % | — |
| risk-limit-search | mcp | 5 | $0.3984 | **−32.8 %** ✅ | 462 125 | 92 % | cheaper |
| risk-limit-search | nomcp | 5 | $0.5926 | — | 633 396 | 91 % | — |

MCP is cheaper by 20–33 % on every task while producing comparable
output tokens (±15 %), confirming that graph traversal replaces
broad-grep exploration with fewer, higher-signal tool calls.

The savings show up in **cache reads** too: MCP arms re-read 27–46 %
fewer cached prompt tokens per task (`avg cache_read`). Fewer, targeted
MCP calls mean a smaller, tighter context window — so less of the system
prompt + earlier turns needs to be replayed from the prompt cache on each
turn. Both arms keep a high cache-hit rate (91–96 %), so the MCP win is
*less volume to re-read*, not *better cache warmth*.

#### Deep dive — `cross-service-impact` (the counter-intuitive one)

This task is worth a closer look because it breaks the naive "fewer
tokens = cheaper" expectation. Full per-arm breakdown:

| metric | mcp | nomcp | Δ (mcp vs nomcp) |
|--------|-----|-------|------------------|
| n (done runs) | 4 | 1 | — |
| avg out (tokens) | 11 546 | 8 140 | **+41.8 %** (mcp talks *more*) |
| avg in (tokens) | 53 | 181 | **−70.7 %** |
| avg cache_read (tokens) | 661 687 | 1 223 839 | **−45.9 %** |
| cache_hit | 94 % | 96 % | −2 pp |
| avg tools | 25.8 | 31.0 | −16.8 % |
| avg turns | 26.8 | 32.0 | −16.3 % |
| avg cost | $0.6146 | $0.7689 | **−20.1 %** |

**The interesting part:** the MCP arm is *cheaper* even though it emits
**41.8 % more output tokens** (11 546 vs 8 140). The cheaper arm is
actually doing *more* work — writing up a fuller structured blast radius.

How can more output cost less? Because the bill is dominated by
**cache reads**, not output. The nomcp arm re-reads **1.22 M** cached
tokens per task; MCP re-reads only **662 k** — a **561 k-token** cut.
At the prompt-cache read rate, those avoided re-reads swamp the extra
output the model produces. In short: *output is the cheap direction;
cache-replayed context is the expensive one.*

Why does MCP accumulate less cache? Two compounding effects:

1. **Smaller tool results.** `avg in` is 53 tokens for MCP vs 181 for
   nomcp. A `get_impact` call returns a packed graph answer; a grep /
   read-file call dumps raw source back into context. Every turn that
   appended result stays in the window and gets **re-read from cache on
   every subsequent turn**. Smaller appends → a context that grows
   slower → less to replay each turn.
2. **Fewer turns.** 26.8 vs 32.0. Fewer round-trips mean the cumulative
   context is replayed fewer times total.

`cache_hit` is actually *lower* for MCP (94 % vs 96 %) — so this is not
"better cache warmth". It is purely **less volume re-read**, which is
what the cost curve rewards.

> ⚠️ **Caveat on `n`.** Only **1** nomcp run reached `status=done` for
> this task (the others missed callers / timed out). Its numbers are a
> single sample, so treat the nomcp column here as indicative, not
> stable. The two simpler tasks (n = 5 vs 5) corroborate the same
> direction with firmer samples.

---

## Decision log (ADRs)

An **ADR (Architecture Decision Record)** is a short document that captures
**why** a technical decision was made — not what the code does, but the
reasoning behind the choice. Without it, six months later nobody remembers
why option A was chosen over B or C, and a reviewer suggests "should be Y"
when Y was already considered and rejected.

Each ADR has four parts:

| Part | Question it answers |
|------|---------------------|
| **Context** | What problem forced this decision? What constraints were active? |
| **Decision** | What we chose — concrete, not "we should consider X" |
| **Alternatives** | What was rejected and **why**. The most valuable section — it's the rebuttal for future reviewer suggestions |
| **Consequences** | What got easier (+) and what debt we accepted (−). The negatives are the valuable part |

ADRs are **never deleted**. When a decision changes, the old ADR is marked
`superseded` and a new one is written explaining what changed — so the
history of reasoning stays intact. In short: **ADR is technical memory.**

Architectural decisions for propagator itself live in [`decisions/`](decisions/_index.md).

| ADR | Title | Status |
|-----|-------|--------|
| [0001](decisions/ADR-0001-redis-wrapper-auto-inference.md) | Redis wrapper detection by auto-inference, not config | accepted |
| [0002](decisions/ADR-0002-add-command-source-onboarding.md) | `add source` command — CLI onboarding, not manual TOML editing | accepted |

When reviewing or adjusting code, check the relevant ADR first — if a reviewer's
suggestion matches a rejected alternative, the ADR already has the rebuttal.
Template and workflow are in [`decisions/_index.md`](decisions/_index.md).

---

## License

MIT
