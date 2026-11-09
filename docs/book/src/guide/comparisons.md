# Competitor and Architecture Comparisons

Choosing the right state machine, statechart, or orchestration engine depends heavily on your system's operational requirements: latency budgets, concurrency models, dynamic vs compile-time definitions, persistence needs, and distributed messaging.

This guide provides an honest, source-noted comparison between `xstate-statemachine-rs` and alternative libraries across the Rust, Python, JavaScript, and distributed workflow ecosystems.

---

## At a Glance: Feature & Capability Matrix

| Capability / Dimension | `xstate-statemachine-rs` | `statig` / `smlang` | `python-statemachine` | `XState v5` (TypeScript) | `Temporal` / `Step Functions` |
|:---|:---:|:---:|:---:|:---:|:---:|
| **Language & Ecosystem** | **Rust (MSRV 1.88+)** | Rust | Python | JavaScript / TypeScript | Polyglot (Go/Java/TS/Python) |
| **Microstep Latency** | **106 ns (p50)** | ~50–150 ns | 18.7 µs (177x slower) | 5–15 µs (50x slower) | 10–50 ms (Network bound) |
| **Steady-State Allocations** | **0 heap allocs** | 0 heap allocs | Multiple dicts/tuples | V8 object allocations | Network I/O serialization |
| **Dynamic JSON / Runtime Loading** | **Yes (XState v5 JSON)** | No (Compile-time only) | No (Class DSL only) | Yes | Yes (JSON / YAML) |
| **Visual Editor Integration** | **Yes (Stately Studio)** | No | No | Yes (Stately Studio) | Visual Workflow Designer |
| **Hierarchical (Compound) States** | **Yes** | Yes (`statig`) / Partial | Yes | Yes | Sub-workflows |
| **Parallel (Orthogonal) Regions** | **Yes** | No | Yes | Yes | Parallel branches |
| **History States (Shallow & Deep)** | **Yes** | No | Partial | Yes | State persistence |
| **Delayed Transitions (`after`)** | **Yes (Tokio / Simulated)** | Manual | Yes (`asyncio`) | Yes (Timers) | Durable sleep |
| **Invoked Services & Child Actors** | **Yes (`invoke`)** | No | Yes | Yes | Activity / Task calls |
| **Durable Persistence Stores** | **SQLite, Postgres, Redis, File** | No (Custom serialize) | SQLite, Postgres, Redis, File | Client-side / Custom | Server-side Database |
| **Event-Driven Architecture (EDA)** | **NATS, RabbitMQ, SQS, Redis** | No | NATS, RabbitMQ, SQS, Redis | Custom event targets | Task Queues / EventBridge |
| **Transactional Outbox & DLQ** | **Yes (At-least-once relay)** | No | Yes | No | Internal event queue |
| **Live Visual Inspector (SSE/WS)** | **Yes (`@statelyai/inspect`)** | No | Yes | Yes | Web Console |
| **HTTP Framework Integration** | **Yes (`axum` REST/SSE/WS)** | No | FastAPI, Flask, Django | Express, Next.js | gRPC / REST API |
| **LLM Agent Workflows** | **Yes (`xstate-agents`)** | No | Yes | `@statelyai/agent` | LangGraph / CrewAI |
| **Cross-Language Snapshot Parity** | **Yes (100% Python parity)** | No | Yes | Partial | Engine-specific |

---

## Detailed Comparisons

### 1. Versus Rust Compile-Time State Machines (`statig`, `smlang`, `sfsm`)

#### What they are:
Crates like `statig` and `smlang` define state machines entirely within the Rust type system using procedural macros, traits, and enum state representations.

#### Strengths of `statig` / `smlang`:
- **Compile-time Exhaustiveness:** Invalid state transitions or unhandled events can be rejected at compile time by the Rust compiler.
- **Embedded / `no_std` Support:** Minimal binary size suitable for microcontrollers and bare-metal environments.
- **Zero Overhead:** Transitions compile directly into native jumps and match statements.

#### Why choose `xstate-statemachine-rs` instead:
1. **Dynamic Topologies & JSON Schemas:** In real-world enterprise architectures, statecharts frequently originate from external sources: visual design in Stately Studio, customer-defined workflow graphs, database configs, or API payloads. `statig` cannot parse or construct a state machine at runtime.
2. **True Statechart Semantics:** `xstate-statemachine-rs` supports the full W3C SCXML / XState v5 specification: parallel orthogonal regions, shallow/deep history states, dynamic eventless `always` microsteps, and compound state hierarchies with least common ancestor (LCA) resolution.
3. **Turnkey Ecosystem:** Beyond raw transitions, `xstate-statemachine-rs` provides built-in durable persistence (Postgres, SQLite, Redis), CloudEvents messaging, Tokio actor mailboxes, and an Axum web integration.

---

### 2. Versus `XState v5` (JavaScript / TypeScript)

#### What it is:
Created by Stately.ai, XState is the gold standard for statechart runtimes in JavaScript and TypeScript, driving both client-side UI workflows and Node.js server architectures.

#### Interoperability:
`xstate-statemachine-rs` complies strictly with XState v5 JSON semantics. You can design machines in Stately Studio, export the JSON, and run it in Rust with identical transition behavior (verified against real XState on 53 conformance cases).

#### Why choose Rust over Node.js XState:
1. **Throughput & Latency:** While XState v5 in Node.js typically achieves ~60,000 to 100,000 transitions/second (10–15 µs/op), `xstate-statemachine-rs` executes in **106 ns** (over 9.4 million transitions/second)—a **50x–90x performance advantage**.
2. **Memory Footprint:** In Rust, steady-state transitions perform 0 heap allocations, and an active interpreter occupies only ~240–380 bytes. In Node.js / V8, object graphs, closures, and garbage collector tracking consume kilobytes per machine instance.
3. **Multi-Threaded Parallelism:** Node.js is bound to a single thread event loop. `xstate-tokio` leverages work-stealing multi-threaded Tokio runtimes across all available CPU cores.

---

### 3. Versus Python State Machine Libraries (`python-statemachine`, `transitions`)

#### What they are:
- `python-statemachine`: A clean, Pythonic declarative class-based state machine library with good support for simple transitions.
- `transitions`: A flexible, widely-used library that mixes state machine behavior directly into existing Python classes.

#### How `xstate-statemachine-rs` compares:
1. **Performance:** `xstate-statemachine-rs` is **177x faster** than Python state machines (106 ns vs 18.7 µs).
2. **PyO3 Drop-in Acceleration:** Python codebases can simply `pip install xstate-statemachine` and import `xstate_rs` to run the Rust engine inside CPython, immediately achieving **2.54 million events/sec (29.7x speedup)** without rewriting Python application logic.
3. **Statechart Formats:** Neither `transitions` nor `python-statemachine` supports the XState v5 JSON specification, visual editing in Stately Studio, or byte-identical cross-language snapshots.

---

### 4. Versus Distributed Orchestrators (`Temporal`, `AWS Step Functions`)

#### What they are:
Distributed workflow engines that guarantee execution durability across network boundaries by logging every step, retry, and timer to a centralized database or cluster.

#### Architectural Differences:

| Requirement | Distributed Orchestrator (`Temporal`) | In-Process Statechart (`xstate-statemachine-rs`) |
|---|---|---|
| **Step Latency** | 10 ms – 100 ms (Network RPC & consensus) | **106 ns – 1 µs** (In-memory execution) |
| **Throughput** | Hundreds to low thousands of steps/sec | **Millions of steps/sec** |
| **Infrastructure Overhead** | Dedicated cluster, database, workers | **Single lightweight library dependency** |
| **Workflow Lifespan** | Months to years | Milliseconds to days |
| **Use Case Fit** | Cross-microservice billing pipelines | High-throughput services, trading, gaming, IoT, agents |

`xstate-statemachine-rs` with `xstate-persistence` gives you **durable state, crash-recovery, and idempotency** with the sub-microsecond speed of an in-process Rust runtime, avoiding the infrastructure complexity of heavy orchestrator clusters.

---

### 5. Versus LLM Orchestration Frameworks (`LangGraph`, `CrewAI`)

#### What they are:
Graph-based frameworks designed specifically to orchestrate LLM agents, tool calls, and multi-agent coordination.

#### Why `xstate-agents` is different:
- **"The Model Proposes, The Chart Decides":** In typical agent frameworks, the LLM determines control flow by emitting next-step identifiers, leading to runaway loops, hallucinated tools, or security policy violations.
- In `xstate-agents`, the agent is constrained by a formal statechart:
  - **Strict Tool Allow-Lists:** In state `browsing`, the model cannot invoke `database_delete`.
  - **Token & Budget Guards:** Transitions to next steps fail if budget thresholds are exceeded.
  - **Durable Human Approval:** High-risk transitions yield into an `awaiting_approval` state that persists to PostgreSQL until an authenticated operator dispatches an `APPROVE` event.
  - **Deterministic Testing:** Run end-to-end agent tests offline in CI using `FakeModel` without making external API calls or incurring costs.
