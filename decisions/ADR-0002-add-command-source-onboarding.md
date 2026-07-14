---
status: accepted
date: 2026-07-14
supersedes:
---

# ADR-0002: `add source` command — CLI onboarding, not manual TOML editing

## Context
Onboarding a new repo into propagator currently means: open `propagator.toml`,
copy an existing `[[sources]]` block, paste it, edit `path` / `service` / `kind`,
save, then run `propagator sync`. Five manual edits for a three-field append —
and `kind` is the error-prone one: a user who doesn't remember that `kind` is
`"code"` for Go/Rust/C++ and `"sql"` for Oracle PL/SQL either guesses wrong
(the extractor silently produces nothing) or opens the README to check.

This is the same friction ADR-0001 addressed for redis wrappers: propagator's
core value is zero-config impact tracing, yet the very first step (registering
a source) is config-heavy manual work. The gap shows up every time a new
service is onboarded.

Constraint that shaped the choice: **the config file must remain human-readable
and hand-editable.** Whatever the command emits, a developer who opens
`propagator.toml` in their editor must see the same idiomatic TOML they would
have typed — not machine-generated noise that obscures intent.

## Decision
**Add a `propagator add source <path>` subcommand that appends a `[[sources]]`
block to `propagator.toml` and infers `kind` + `service` from the repo itself.**

- `kind` is auto-detected: presence of `.go` / `.rs` / `.cpp` / `.h` files →
  `"code"`; presence of `.sql` / `.pks` / `.pkb` / `.pkh` → `"sql"`. A mixed
  repo (both Go and SQL) defaults to `"code"` and prints a warning telling the
  user to split or override.
- `service` defaults to the repo's directory basename (`my-order-service/` →
  `my-order-service`), overridable with `--service <name>`.
- `--kind <code|sql>` exists as an explicit override for ambiguous repos.
- The command appends a single `[[sources]]` block with a leading blank line,
  preserving all existing content/comments/formatting above it (byte-stable
  prefix; append-only, never rewrites the whole file).

This keeps propagator's zero-config philosophy: point it at a repo and it
figures out what to extract, without baking any project-specific naming into
the tool.

## Alternatives considered
- **Manual edit only (status quo):** rejected because the `kind` field is the
  most common onboarding mistake and produces *silent* zero-extraction — the
  user runs `sync`, sees `defs=0`, and has no signal that they picked the wrong
  kind. Auto-detection removes the failure mode entirely.
- **Interactive wizard (prompt for each field):** rejected as over-engineering.
  A three-field append doesn't justify a TUI session; inference covers the
  common case and `--flag` overrides cover the rest. An interactive prompt also
  breaks scripting (`make onboard SERVICE=foo`).
- **Separate `propagator config` tool/subcommand group:** rejected as scope
  creep. Propagator's job is graph building, not general TOML editing. A single
  `add source` verb is the only config mutation that earns its place because it
  directly gates whether a repo enters the graph at all. `remove`, `list`,
  `enable`, `disable` are YAGNI until they're asked for.
- **Auto-scan workspace dir and register all repos without config:** rejected
  because it destroys determinism — `propagator.toml` is the source of truth
  that pins which repos are in the corpus. Silent auto-inclusion would make
  `sync` results non-reproducible (a cloned scratch repo would suddenly appear
  in the graph). The config file must be an explicit opt-in; the command just
  lowers the friction of opting in.

## Consequences
- (+) Onboarding a new repo becomes a single command; `kind` mistakes are
      eliminated by detection.
- (+) The appended TOML is identical to what a human would type, so the config
      stays a readable artifact — no "don't touch, it's generated" stigma.
- (+) Append-only means the command is safe to re-run; it never corrupts
      existing config. Duplicate detection (same `path`) should warn, though.
- (-) Auto-detection of `kind` is heuristic: a repo whose primary language
      differs from its extraction target (e.g. a Go repo that vendors SQL
      migrations) may detect wrong. The `--kind` override and the mixed-repo
      warning mitigate this, but a user who ignores the warning gets silent
      mis-extraction. Debt accepted: detection is a convenience, not a proof.
- (-) Appending TOML without a full parse/serialize round-trip means the
      command can't validate the *resulting* file is well-formed TOML until the
      next `sync`. If the user hand-edited the file into a broken state and
      then runs `add`, the append succeeds but `sync` fails later. Acceptable:
      the failure surfaces at sync, which is the right place.
- (-) `service` derivation from directory basename is a guess — it can collide
      with an existing service node in the graph (two repos → same service name
      → merged node). The `--service` flag is the escape hatch, but a silent
      merge is a real soundness gap at scale.
