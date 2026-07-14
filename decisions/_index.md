# Decision Log — propagator

Architecture decisions for the **propagator** tool itself (the Rust
impact-tracer + MCP server in this repo). Decisions about the growin work
services live in a separate log under `~/work/decisions`.

| ADR | Title | Status | Date |
|-----|-------|--------|------|
| [0001](ADR-0001-redis-wrapper-auto-inference.md) | Redis wrapper detection by auto-inference, not config | accepted | 2026-07-12 |
| [0002](ADR-0002-add-command-source-onboarding.md) | `add source` command — CLI onboarding, not manual TOML editing | accepted | 2026-07-14 |

## How to use

1. **When to write:** a decision survives the 6-month test (you'll still care
   in 6 months) AND someone (you, teammate, future LLM session) might question
   or forget why. Architecture, data model, tradeoff, convention. NOT bug fixes
   or one-off choices.

2. **Scope:** this log is for propagator's own design (extractors, store, graph,
   MCP surface). Decisions about the growin services being indexed go in
   `~/work/decisions`.

3. **Workflow with PR feedback:**
   ```
   PR feedback: "why did you do X? should be Y"
      |
      v
   Read ADR → was Y in Alternatives?
      |
      ├── YES → ADR already explains why Y was rejected.
      │         LLM can push back with reasoning, or confirm
      │         the rejection still holds.
      │
      └── NO  → Y is genuinely new information.
                Evaluate honestly. If Y is better → amend ADR
                (supersede or add), adjust code. If not → add
                Y to Alternatives so the next person doesn't
                re-suggest it.
   ```

4. **Amending:** never delete an ADR. Supersede it:
   - Old ADR: `status: superseded`, add `superseded by: ADR-NNNN`
   - New ADR: `supersedes: ADR-NNNN`, explain what changed
