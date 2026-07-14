---
status: accepted
date: 2026-07-12
supersedes:
---

# ADR-0001: Redis wrapper detection by auto-inference, not config

## Context
The Layer-3 redis extractor recognizes native go-redis call sites
(`rdb.Verb(ctx, key, …)`) and maps the key to a `RedisKey` node. But every
service in the corpus hides go-redis behind its own helper type — orderservice's
`RedisClientRepo.GeneralRedisGet(key)` / `GeneralRedisHGET(key, field)`,
riskmanagementservice's `Redis.HSetWithMapString(ctx, key, val)` /
`RedisCluster.HGET(ckey, field)`. The wrapper's method name is not a redis verb,
and its key argument sits at a service-specific position (sometimes arg 0 with no
`ctx`, sometimes arg 1). None of these call sites were recognized — orderservice
alone had 38 blind wrapper calls, so the bulk of its redis keyspace (including
`GTC:SetExpiryDatetoRedis:` + ClOrdId) was invisible.

Constraint that shaped the choice: **propagator must stay generic across every
service/repo it indexes.** A fix that only works for the growin services, or that
demands hand-maintained config per repo, defeats the point of a zero-config
impact tracer.

## Decision
**Auto-infer wrappers from their bodies — no config, no hardcoded names.** A
service-scoped pre-pass (`discover_wrappers` in `src/extract/redis.rs`) scans
every Go method/func declaration; when a body threads one of its own parameters
straight into a known go-redis verb call, it records
`method-name → (verb kind, key-arg index)`. Call sites of those names are then
recognized exactly like native verbs. This runs as a two-pass over each service
(`code.rs::extract` now parses every file once, then pass A discovers wrappers
across all files, pass B extracts), because a wrapper's definition and its call
sites usually live in different files.

## Alternatives considered
- **Hardcode the `GeneralRedis*` names in `redis.rs`:** rejected because it bakes
  one project's naming convention into the generic extractor. It would silently
  fail on the next service whose wrapper is named differently (`RedisCluster.HGET`,
  `DataSink.HSet`), and rot as conventions drift.
- **Config-driven wrapper list in `propagator.toml`** (explicit `method → verb →
  key_arg` mapping, or a `wrapper_prefixes` list): rejected because it pushes a
  maintenance burden onto every repo and every refactor — a new helper method
  silently goes blind until someone edits config. The verb tables in `redis.rs`
  are already generic; only the *name* is project-specific, and the name is
  recoverable from the code itself, so config would encode information already
  present in the AST.
- **Alias-chain / deeper key resolution instead of wrappers:** rejected as a
  non-fix — it addresses key *values*, not the unrecognized *call shape*. Bare
  runtime keys (`string(record.Headers[2].Value)`) still have no static pattern
  and belong in the dynamic ledger regardless.

## Consequences
- (+) Every service's wrapper keyspace surfaces with zero config; new helper
      methods are picked up automatically on the next sync. Resync lifted resolved
      `RedisKey` patterns ~7 → ~30 across services (`AUTOORDER:*`, `MARGINSTOCKS::*`,
      `PANIC_SERVICE:*`, `userservice::oaocache::*`, `GTC:SetExpiryDatetoRedis:*`).
- (+) The project-specific knowledge lives in the code being indexed, not in a
      side-channel that can drift out of sync with it.
- (-) `extract()` now holds every parsed file's `Tree` + source in memory for the
      duration of a service extract (needed for the two-pass). Fine at current repo
      sizes; a pathologically huge service could pressure memory.
- (-) Inference is heuristic: it only fires on a *direct* param→verb passthrough.
      A wrapper that transforms the key (`Get(ctx, "pfx:"+key)`), dispatches
      dynamically, or forwards through another wrapper is not inferred — it stays
      blind, with no signal that it was missed. Debt accepted: recall is silently
      partial for indirect wrappers.
- (-) Matching is by method name only (receiver type ignored), so two unrelated
      types sharing a method name would collide. Low risk at this scale, but a real
      soundness gap.
- (-) Newly-visible wrapper calls whose keys are unresolvable now increment the
      dynamic ledger (343 → 574) — honest, but the raw number looks like a
      regression until read as "previously silent, now counted".
