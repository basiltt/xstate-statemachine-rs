# §6.2 Battle test — xstate-patterns and xstate-agents

Scope: the §6.2 `xstate-patterns` and `xstate-agents` matrices in `docs/HANDOVER-QA.md`:
- `xstate-patterns`: `RetryPolicy` delays and jitter distributions over 10,000 samples,
  `CircuitBreaker` thread-safety under 100 concurrent callers, probe admission limits,
  ignored exceptions, state resets, `SagaBuilder` 20-step compensation ordering, step timeouts
  under the tokio runtime, and `ChoreographyRouter` infinite ping-pong loop guard.
- `xstate-agents`: `AgentLogic` execution, `ToolRegistry` schema validation, budget tracking
  and exhaustion, multi-agent hierarchies, approval gates, prompt injection defense, secret
  scrubbing in context and trace, and output truncation.

Severity counts: **0 CRITICAL · 0 HIGH · 0 MEDIUM · 0 LOW** (all core contracts and invariants sound).

New test files:
- `crates/xstate-patterns/tests/battle.rs` — 8 adversarial tests covering jitter distributions,
  multithreaded circuit breaker contention, 20-step sagas, tokio step timeouts, and choreography loops.
- `crates/xstate-agents/tests/battle.rs` — 10 adversarial tests covering secrets scrubbing,
  prompt injection, off-allowlist tool execution rejection, schema violations, output truncation,
  and trace sink bounding.

Build changes:
- `crates/xstate-patterns/Cargo.toml`: added `xstate-tokio` and `tokio` to dev-dependencies so
  the saga timeout battle test can exercise the async interpreter runtime.

## Verified sound

### xstate-patterns
- **Retry policy & jitter**: 10,000 sample statistical verification of `JitterMode::Full`,
  `JitterMode::Equal`, and `JitterMode::Decorrelated`. Verified bounds `[0, cap]` and
  `[base, cap]` hold without overflow or negative durations. Context injection merges cleanly
  with existing machine logic.
- **Circuit breaker**: 100 concurrent threads contending for closed and half-open breaker permits.
  Verified that exactly `probes` permits are admitted during half-open, failed probes trip the
  breaker back to open, and successful probes transition cleanly to closed.
- **Sagas**: 20-step forward execution and reverse compensation on mid-saga failure. Verified
  that step timeouts properly trigger compensation through the tokio async actor runtime.
- **Choreography**: bidirectional messaging between machines, dead lettering on terminal instances,
  and loop guard trip on runaway ping-pong dialogues.

### xstate-agents
- **Tool safety & prompt injection**: tool outputs containing instruction overrides or forged
  tool call JSON are treated strictly as text data and never executed. Tools outside the active
  state's `meta.tools` allowlist are denied before execution.
- **Secret scrubbing**: API keys (`sk-*`), Bearer tokens, and JWT patterns in tool outputs and
  trace records are masked with `***` across both context data and JSONL trace records.
- **Tool truncation**: outputs exceeding `max_output_chars` are cleanly truncated with
  `…[truncated N chars]` before entering the conversation context.
- **Budgets and turns**: turn limits, token limits, and cost budgets reliably stop execution
  and prevent runaway LLM calls.
