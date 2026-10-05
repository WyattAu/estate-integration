# estate-integration

Cross-crate dogfooding suite for the WyattAu crate estate: the estate's
crates used **together** the way real users combine them. Each suite is a
real running system (`tests/` proves it, `examples/` shows it).

> **This repo proves the crates compose.** Every suite wires 2–5 published
> estate crates into one flow with exact-pinned dependencies, so any failure
> is attributable to real drift between crates — never to a moving version.
> This repo stays GitHub-only by design: integration suites are bins/examples,
> not a library, and are never published to crates.io.

Coverage of the published estate, and what is still unproven, is tracked
in [DOGFOOD-COVERAGE.md](DOGFOOD-COVERAGE.md) — 56 of 161 published crates
composed across 30 suites, 32 in debt.

## Architecture

```text
                        estate-integration 0.1.0
                        (exact-pinned estate deps)
            ┌──────────────────┬──────────────────┬──────────────────┐
            │   auth-stack     │  resilient-api   │  mail-pipeline   │
            │                  │                  │                  │
  register  │ validkit ──► salting (argon2id+pepper)                │
  login     │ salting verify ──► tokenkit mint (JWT)                │
  request   │ barbican BearerToken ──► tokenkit decode ──► revoke?  │
            │                  │                  │                  │
            │                  │  throttle-kit ──► axum /limited (429)
            │                  │  breaker ──► wiremock upstream (open)
            │                  │  healthkit ──► /livez /readyz /startupz
            │                  │  otelkit span per request          │
            │                  │                  │                  │
            │                  │                  │  validkit ──► mailkit MIME
            │                  │                  │  sieve-kit plan ──► keep/
            │                  │                  │    vacation/flag            │
            │                  │                  │  sendgrid ──► wiremock (202)│
            └──────────────────┴──────────────────┴──────────────────┘
            ┌──────────────────┬──────────────────┐
            │  media-upload    │   sync-client    │
            │                  │                  │
  upload    │ validkit key ──► media-kit sniff/bomb-guard           │
  variants  │ EXIF-orient ──► parallel variants ──► blobkit store   │
  dedup     │ cas-kit address ──► dupe upload stores nothing        │
            │                  │                  │
            │                  │  mail-sync-kit MockStore seam      │
            │                  │  engine events ──► eventbus persist│
            │                  │  sqlite ──► replay after restart   │
            └──────────────────┴──────────────────┘
            ┌──────────────────┬──────────────────┬──────────────────┐
            │  config-secrets  │ webhook-consumer │  outbox-dispatch │
            │                  │                  │                  │
  startup    │ TOML file ──► ConfigBuilder (file→env→overrides)      │
  redact     │ api_key: Sensitive<String> ──► [REDACTED] in Debug    │
  strict     │ load_strict ──► typo'd key named in the error         │
            │                  │                  │                  │
            │  webhookkit HMAC gate ──► IdempotencyKey(event id)    │
            │  MemoryStore First ──► process / Replay ──► skip      │
            │  error path ──► release ──► retry succeeds            │
            │                  │                  │                  │
            │  outbox-kit Dispatcher ──► breaker trip on burst      │
            │  open: pause (no attempt burn) ──► half-open probe    │
            │  ──► closed: 5/5 dispatched, shutdown < 2 s           │
            └──────────────────┴──────────────────┴──────────────────┘
            ┌──────────────────┬───────────────────────────────────┐
            │  latency-gate    │  chaos-and-telemetry              │
            │                  │                                   │
  percentiles│ PercentileTracker ──► quantiles vs sorted ref        │
  budget     │ criterion fixture ──► percentile-budgets PASS/FAIL   │
            │                  │                                   │
  chaos      │ chaos_layer(seeded 30% throttle + error@7) ──► axum  │
            │  100 requests ──► outcomes == schedule (determinism) │
  scrape     │ healthkit checks ──► metrics-kit registry ──►        │
            │  Prometheus 0.0.4 exposition (inline naive parser)   │
            └──────────────────┴───────────────────────────────────┘
            ┌──────────────────┬───────────────────────────────────┐
            │  telemetry-boot  │  worker-supervisor-drain          │
            │                  │                                   │
            │  bootstrap       │  Telemetry::init(metrics budget)  │
            │                  │  gives the metrics-kit Arc        │
            │                  │  counter/gauge/histogram render,  │
            │                  │  then an inline parser            │
            │                  │  validates Prometheus 0.0.4: TYPE │
            │                  │  lines, cumulative                │
            │                  │  buckets, +Inf == _count, _sum    │
            │                  │  exact                            │
            │                  │  double-init = typed              │
            │                  │  AlreadyInitialized; shutdown     │
            │                  │  idempotent; RUST_LOG beats the   │
            │                  │  configured level                 │
            │                  │                                   │
            │  3 jobs          │  fast-succeeding / slow-overrun   │
            │                  │  coalesced /                      │
            │  + drain         │  failing-past-budget = Degraded;  │
            │                  │  shared                           │
            │                  │  ShutdownGuard, drain < 2 s,      │
            │                  │  RunReport exact                  │
            │                  │                                   │
            │  leader +        │  no lease = MemoryLease fires;    │
            │                  │  denied lease =                   │
            │  jitter          │  skips, never failures; same      │
            │                  │  seeds, same                      │
            │                  │  first-fire windows               │
            └──────────────────┴───────────────────────────────────┘
 ┌────────────┬───────────────┬──────────────┬──────────────────────┐
 │ clock-     │ book-feed     │ uring-proxy  │ ledger-settlement    │
 │ precision  │               │              │                      │
 │ calibrated │ FIX 35=X:     │ io_uring     │ post → OutboxJournal │
 │ clock →    │ encode →      │ accept →     │ → idempotent retry   │
 │ LatencyRing│ parse →       │ ReadFixed →  │ → dispatch → tail    │
 │ == tracker │ Command map   │ breaker +    │ replay → verify_chain│
 │ (nearest-  │ → replay →    │ GCRA gate →  │ (genesis-aware);     │
 │ rank both) │ depth +       │ echo/close   │ RejectNegative       │
 │ → budget   │ checksum      │ → exact      │ refuses before       │
 │ PASS/FAIL  │ determinism   │ decision     │ recording            │
 │            │               │ ledger       │                      │
 ├────────────┼───────────────┼──────────────┼──────────────────────┤
 │ hw-pinned- │ policy-       │ chaos-worker │ telemetry-pipeline   │
 │ hotpath    │ gateway       │              │                      │
 │ pin core → │ Rego over the │ supervisor   │ init → metrics →     │
 │ calibrated │ request doc → │ job under    │ scrape (0.0.4        │
 │ hot loop → │ compliant     │ seeded chaos │ parser) → percentile │
 │ tolerance- │ proceeds to   │ → degradation│ budget → outlier     │
 │ bounded    │ wiremock,     │ latch (live) │ FAIL → shutdown      │
 │ stability  │ violations    │ → survives → │ flush                │
 │ → restore  │ shed before   │ seed-reproduced
 │ affinity   │ the network   │ across runs  │ + dispatch metrics:  │
 │            │               │              │ paused/delivered/    │
 │ + config-  │               │              │ failed in the render │
 │ tenant:    │               │              │ == the dispatch      │
 │ base ⊕     │               │              │ report               │
 │ tenant,    │               │              │                      │
 │ deep merge │               │              │                      │
 └────────────┴───────────────┴──────────────┴──────────────────────┘

Hermetic CI: tempdirs, wiremock, in-process SQLite. No cloud credentials,
no external network.
```

## Suites

| Suite | Proves | Crates |
|---|---|---|
| `tests/auth_stack.rs` | register → login → JWT → authenticated request; wrong-password, expired-token, revoked-token, invalid-email rejection; key rotation; barbican HTTP guards | salting 2.0.0, tokenkit 0.4.1, validkit 1.3.1, barbican 0.2.1 |
| `tests/resilient_api.rs` | 429 on limit breach; breaker opens against failing upstream (wiremock); /readyz reflects dependency state; startup group separation; spans without a collector | breaker 2.0.1, healthkit 1.3.1, throttle-kit 2.0.0, otelkit 2.0.3 |
| `tests/mail_pipeline.rs` | invalid recipient rejected pre-send; vacation outcome produced; provider receipt recorded | mailkit 0.3.1, validkit 1.3.1, sieve-kit 0.2.1 |
| `tests/media_upload.rs` | duplicate upload dedup assertion; oversized-bomb rejection; variant count + EXIF orient | media-kit 0.2.1, blobkit 0.4.2, cas-kit 0.2.1, validkit 1.3.1 |
| `tests/sync_client.rs` | MockStore seam ingest; events persisted to SQLite; replay after restart-simulation | mail-sync-kit 0.1.1, eventbus-kit 0.3.5 |
| `tests/config_secrets.rs` | file → env → in-process-override precedence ladder (deep merge); `Sensitive<String>` redacted through the loaded struct's `Debug`/`Display`; `load_strict` names the unknown top-level key; lenient `load` still accepts the same file | config-kit 0.1.1 |
| `tests/idempotent_webhook.rs` | HMAC-SHA256 gate (raw + Stripe `t=…,v1=…` envelope, locally signed); event id → `IdempotencyKey`; first delivery processes, duplicate hits Replay without re-executing; failed attempt releases the claim and the retry succeeds | webhookkit 2.2.0, idempotency-kit 0.1.0 |
| `tests/outbox_dispatch.rs` | flaky sender (2 failures then success) trips the dispatcher's breaker; open circuit pauses without burning attempt budget; half-open probe closes; all 5 events dispatched with per-event attempts recorded; graceful shutdown < 2 s | outbox-kit 0.1.0, breaker 2.0.1 |
| `tests/percentile_report.rs` | tracker quantiles equal the sorted `nearest_rank` reference and are monotone; markdown row snapshot; criterion-shaped `estimates.json`/`sample.json` fixture gated to PASS, then to a typed `BudgetExceeded` FAIL; budget TOML rejects typo'd keys | percentile-kit 0.1.0 |
| `tests/chaos_resilience.rs` | axum service under `chaos_layer` (seeded 30 % throttle + scripted error at index 7); 100 requests match the precomputed schedule exactly; recorder counts exact; same seed reproduces bit-for-bit; paused-clock latency fault | chaos-kit 0.1.1 (+ axum/tower) |
| `tests/metrics_scrape.rs` | healthkit checks drive a metrics-kit registry (per-method counter, inflight gauge, duration histogram); rendered registry validated as Prometheus 0.0.4 by an inline parser: TYPE lines, cumulative buckets, `+Inf` == `_count`, `_sum` consistent | metrics-kit 0.2.0, healthkit 1.3.1 |
| `tests/telemetry_bootstrap.rs` | one-call bootstrap: the configured metrics budget caps registration through `Telemetry::metrics()`; labeled counter + gauge + histogram render a valid Prometheus 0.0.4 exposition (inline parser); double init is the typed `AlreadyInitialized`, `shutdown` is idempotent, and `RUST_LOG` overrides the configured directive (asserted via `build_subscriber` + `max_level_hint`) | telemetry-init 0.1.1, metrics-kit 0.2.0 |
| `tests/worker_supervisor_drain.rs` | three jobs (fast-succeeding; slow-overrun that coalesces instead of stacking; always-failing rollup that turns `Degraded` at its failure budget and keeps running) run on the shared shutdown-kit `ShutdownGuard`, drain < 2 s, and the name-ordered `RunReport` is exact; default no-lease leadership fires (`MemoryLease` always wins) while a never-winning host lease records skips — never failures; same-seed jitter windows identical across supervisors | worker-kit 0.3.0 (no default features — see round-4 findings), shutdown-kit 0.3.2 |
| `tests/clock_precision.rs` | calibrated clock reads are monotonic and typed (TSC where the host offers an invariant counter, `Instant` fallback where it does not); one measured workload into BOTH a clock-kit `LatencyRing` and a percentile-kit `PercentileTracker` — every statistic identical (nearest-rank agreement across the L1/L2 boundary); observed quantiles gated by a committed-style budget (PASS) and an inflated window rejected with the typed `BudgetExceeded` naming the metric | clock-kit 0.1.0 (`tsc`), percentile-kit 0.1.0 |
| `tests/book_feed.rs` | FIX 4.4 market-data feed: `FixBuilder` encode → `FixMessage::parse` + checksum verify → typed mapping to book-kit `Command`s (`AddBid`/`AddAsk`/`Execute`) → `replay` under `GapPolicy::Fail` → depth exactness after partial and full fills, checksum-identical to direct application; truncation, body corruption, and journal gaps are all typed failures | wire-kit 0.1.0, book-kit 0.1.0 |
| `tests/hw_pinned_hotpath.rs` | pin to the current core (`pin_current_core`, fail-closed mask read-back), measure a fixed hot loop with the calibrated clock into a `LatencyRing`, assert **bulk** stability (≥3/4 of runs within 10× the median, median within 2× the fastest run, trimmed mean over a 3× band tracking it, no half-to-half drift, absolute clock bound) — pinning removes migration cost, which inflates every run, but not the kernel's right to steal a timeslice, which inflates a few (round-6 notes), cross-check the calibrated p50 against `Instant`, restore and verify the original affinity; typed failures (empty mask, out-of-range core) and `CpuSet` round-trips | hw-kit 0.1.0 (`libc`), clock-kit 0.1.0 (`tsc`) |
| `uring_proxy.rs` | a real echo proxy on an `io_uring` engine thread (listener → completion-driven accept → `ReadFixed` → gate → `WriteFixed` → close): breaker-first admission answers `CIRCUIT_OPEN` before capacity is spent, an exhausted GCRA budget answers `THROTTLED retry_after_ms=<n>`, two scripted backend failures trip the circuit, the half-open probe recovers; exact decision ledger + breaker counters; token/probe/pool contracts hold on any host (ring creation may be denied by the kernel/seccomp — the flagship test reports the skip) | uring-kit 0.1.0, breaker 2.0.1, throttle-kit 2.0.0 |
| `tests/ledger_settlement.rs` | full settlement flow: double-entry post (debit A, credit B) mirrored through `OutboxJournal` into an outbox-kit store (envelope id == posting id); idempotency-gated retry (`DuplicatePosting` with the original id; key reuse with a fresh request → `InvalidPosting`); dispatcher delivers exactly once; crash recovery replays the undelivered tail into a fresh ledger; `RejectNegative` refuses an overdraw before anything records; audit chain verifies (genesis-aware) with hash-linked entries | ledger-kit 0.1.0, outbox-kit 0.1.0, idempotency-kit 0.1.0 |
| `tests/policy_gateway.rs` | policy-gated HTTP: a `fetch_kit::middleware::Middleware` evaluates a Rego bundle over the request document (method/path/headers) — compliant requests proceed to a wiremock upstream, violating ones short-circuit before `Next::run` (no network, no retry budget spent), evaluation errors and a poisoned engine reject fail-closed; verdicts are attributed per rule; null-safety of the input document | policy-kit 0.1.0, fetch-kit 0.2.0 (+ reqwest 0.13 — see round-5 findings) |
| `tests/chaos_worker.rs` | a worker-kit supervisor job whose work unit is a tower service under `ChaosLayer` (seeded 5 ms latency on every call, scripted errors at two consecutive indexes): the failure budget trips the degradation latch — observed live, because a post-outage success clears the flag (round-5 finding) — the worker keeps firing past degradation and drains < 2 s; the same seed reproduces the identical fault sequence across two independent sessions; recorder counts are exact | chaos-kit 0.1.0, worker-kit 0.1.0 (no default features) |
| `tests/config_tenant.rs` | per-tenant resolution: base config + tenant override files through `ConfigBuilder` layers — deep merge (one nested knob overridden, siblings kept), the same key resolving differently per tenant, secrets redacted through every render of the merged load, and `load_strict` naming a typo'd tenant key | config-kit 0.1.0 |
| `tests/telemetry_pipeline.rs` | the full observability pipeline in one process: `Telemetry::init` → register counter/gauge/histogram → six stage latencies into BOTH the metrics-kit histogram and a percentile-kit tracker → scrape validated as Prometheus 0.0.4 (inline parser; `_count`/`_sum`/`+Inf` consistent with the tracker's truth) → budget gate PASS + outlier FAIL → idempotent shutdown flush | telemetry-init 0.1.0, metrics-kit 0.1.0, percentile-kit 0.1.0 |
| `tests/outbox_dispatch_metrics.rs` | dispatch with a metric per attempt (`outbox_dispatch_total{outcome=delivered/failed/paused}`, `outbox_pending` gauge) through a breaker-wrapped sender under failure injection: the open circuit's sheds are visible as `paused` in the parsed render and the breaker's transitions appear in the dispatch report — and the two views count the same attempts (sender-level breaker burn bounded; round-5 finding) | outbox-kit 0.1.0, metrics-kit 0.1.0, breaker 2.0.1 |
| `tests/calibration_stack.rs` | the whole automotive calibration pipeline: A2L description (addresses, datatypes, limits, `COMPU_METHOD` coefficients) + DBC bit layout (`start_bit`/`bit_length`/`byte_order`/`scale`) bound host-side; XCP `CONNECT` negotiates the byte order and `MAX_CTO`, which then parameterises every `SET_MTA`/`UPLOAD`/DAQ frame built at the A2L addresses; a DAQ DTO crosses `can_core` in both directions (command → SocketCAN bytes → `CanFrame` → packet) and back out through `dbc-parse` `decode_raw` and the A2L conversion; the write path frames a limit-checked value for `DOWNLOAD`; the slave `ERR` taxonomy, truncated buffers, and an oversized packet are all typed failures | a2l-parse 0.1.0, dbc-parse 0.1.0, can-core 0.1.0, xcp-core 0.1.0 |
| `tests/spreadsheet_engine.rs` | the product layer end to end: literals and formulas, a quarterly model (SUM over ranges, IF, margin ratios), a 200-cell dependency chain and a 4,000-formula cone — each edit asserted through the invariant that **`recalculate_incremental` and `recalculate` agree cell-for-cell**; cross-sheet and quoted-sheet references (including a dangling sheet that heals when the sheet appears), formula→literal edge removal, error-value propagation, cycle detection with members, volatile tracking, XLSX round-trip that preserves a *live* dependency graph, and `VLOOKUP` over 100 rows | sheet-engine 0.1.0, formula-lang 0.1.1 |
| `tests/chaos_worker.rs` | a worker-kit supervisor job whose work unit is a tower service under `ChaosLayer` (seeded 5 ms latency on every call, scripted errors at two consecutive indexes): the failure budget trips the degradation latch — observed live, because a post-outage success clears the flag (round-5 finding) — the worker keeps firing past degradation and drains < 2 s; the same seed reproduces the identical fault sequence across two independent sessions; recorder counts are exact | chaos-kit 0.1.1, worker-kit 0.3.0 (no default features) |
| `tests/config_tenant.rs` | per-tenant resolution: base config + tenant override files through `ConfigBuilder` layers — deep merge (one nested knob overridden, siblings kept), the same key resolving differently per tenant, secrets redacted through every render of the merged load, and `load_strict` naming a typo'd tenant key | config-kit 0.1.1 |
| `tests/telemetry_pipeline.rs` | the full observability pipeline in one process: `Telemetry::init` → register counter/gauge/histogram → six stage latencies into BOTH the metrics-kit histogram and a percentile-kit tracker → scrape validated as Prometheus 0.0.4 (inline parser; `_count`/`_sum`/`+Inf` consistent with the tracker's truth) → budget gate PASS + outlier FAIL → idempotent shutdown flush | telemetry-init 0.1.1, metrics-kit 0.2.0, percentile-kit 0.1.0 |
| `tests/outbox_dispatch_metrics.rs` | dispatch with a metric per attempt (`outbox_dispatch_total{outcome=delivered/failed/paused}`, `outbox_pending` gauge) through a breaker-wrapped sender under failure injection: the open circuit's sheds are visible as `paused` in the parsed render and the breaker's transitions appear in the dispatch report — and the two views count the same attempts (the sender-level breaker burn is bounded by each event's derived `max_attempts + 1` ceiling, and the gauge watcher is awaited before the render is read, so neither assertion races the runner; round-5 + round-6 notes) | outbox-kit 0.1.0, metrics-kit 0.2.0, breaker 2.0.1 |
| `tests/money_stack.rs` | the **accounting core**: billing-kit `Price` → exact tax/gross → double-entry posting into ledger-kit → outbox-mirrored durable journal → replay into a fresh ledger → the same figures written into a formula-driven spreadsheet workbook and exported to XLSX. A `Price::gross()` posts with *no conversion* (same money type), largest-remainder allocation sums exactly, FX conversion is explicit, an idempotent replay changes nothing, and the exported statement agrees with the books | billing-kit 0.2.0, decimal-money 1.1.1, ledger-kit 0.1.0, outbox-kit 0.1.0, sheet-engine 0.1.0, sheet-core 0.1.0, formula-lang 0.1.1 |
| `tests/auth_provision.rs` | the provisioning + session stack: a SCIM user serializes to RFC 7644 shape (`userName` on the wire, never `user_name`) and round-trips; filters select from a provisioned set on SCIM attribute names; a list response reports `totalResults` independently of page size; accessctl's hardcoded role hierarchy and its Cedar policy set reach the same verdicts; ws-barbican extracts a token from header, registered query key and cookie with header precedence, refusing an unregistered key; and every action lands in a hash-linked audit chain that verifies | accessctl 0.1.0 (`cedar`), scim-kit 0.1.0, tamper-audit 0.2.0, ws-kit 0.4.2, ws-barbican 0.1.2 |
| `tests/collab_docs.rs` | collaborative documents end to end: three replicas authored independently converge on byte-identical text under four delivery orders (each fragment present exactly once, none lost or duplicated); concurrent delete + insert converge in either causal order; replayed deletes are no-ops while replayed inserts **duplicate**; out-of-order delivery visibly diverges; presence join/leave/re-join; a `BroadcastHub` fans each publication to every subscriber once and *refuses* a broadcast with no receivers; i18n fallback chains (`fr-CA` → `fr` → `en`), plural-rule selectors, and locale parsing; markdown renders from collaboratively edited text with `<script>` stripped; the convergence ships as a typed event envelope | crdts-kit 0.1.0, i18n-kit 0.1.3, eventbus-kit 0.3.5 (`typed_eventbus`), docs-pipeline 0.1.4, ws-kit 0.4.2 |
| `tests/systems_substrate.rs` | the single-binary service substrate: TTL cache expiry is measured from insertion (a hot key still expires) and `take_fresh` is the atomic read-and-remove a work queue needs; a readiness gate that latches and is revoked only deliberately; a lock-free slab pool whose guards are borrow-checked, whose exhaustion is a typed `None`, and which returns every slot on drop; a shared-memory SPMC ring that refuses to overwrite unread data and reports per-reader cursors; and actors exchanging prioritised messages on a work-stealing scheduler | actor-kit 0.2.3, slab-pool 0.1.0, shared-state 0.1.1, shm-rings 0.2.1 |

## Run

```sh
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cargo run --example auth_register_login   # + 4 more examples/
```

## Conventions

edition 2021 · rust 1.85 · MIT OR Apache-2.0 · `#![forbid(unsafe_code)]` ·
exact-pinned deps · hermetic tests only.

Every suite wires 2–5 estate crates into **one real flow**; every test
documents its intent in a doc comment and asserts the *outcome* of the
flow (state transitions, counters, rendered artifacts), never just that
an API call returned. Round-3 suites add: deterministic chaos asserted
against a precomputed schedule, and scrapes validated by an inline
format parser — no new network dependencies beyond the estate kits.
Round-4 suites add: telemetry bootstraps validated at the exposition
level, and a supervisor flow reported exactly (fires/failures/`Degraded`)
with a sub-2-second drain — leadership exercised in-process, no Redis.
Round-5 suites add: hardware-timing measurement gated by percentile
budgets, a wire→book FIX pipeline with typed corruption failures, a
real `io_uring` echo proxy under breaker+GCRA admission, the full
settlement flow (post → durable mirror → idempotent retry → dispatch →
tail-replay recovery → chain verify), policy-gated HTTP that sheds
before the network, chaos-driven supervisor degradation observed live
and reproduced across sessions, per-tenant deep-merged config, the
end-to-end telemetry pipeline, and dispatch metrics that agree with the
dispatch report attempt-for-attempt. Round-6 suites add: the first
*product*-level dogfooding — a four-crate automotive pipeline bound
host-side (A2L × DBC × XCP × CAN) and a spreadsheet engine whose every
edit is judged by an incremental-vs-full-recalculation equivalence
invariant rather than by an expected value alone.

## Integration findings

Dogfooding notes filed back to the estate (pinned in test comments):

- tokenkit 0.4.1 flattens every `jsonwebtoken` error into
  `JwtError::DecodingFailed(String)` — `JwtError::Expired` is never
  constructed, so hosts cannot distinguish expiry from other failures.
- barbican 0.2.1 depends on tokenkit 0.1, semver-incompatible with tokenkit
  0.4 in the same graph; composition goes through barbican's tokenkit-free
  surface (`BearerToken`, `auth_middleware_fn`).
- healthkit 1.3.1 registers checks through a blocking lock — build the
  registry before entering the Tokio runtime.
- mail-sync-kit 0.1.1 ships the `MailStore` trait seam but no mock; this
  repo's `MockStore` is the reference host implementation.

Round-3 notes (the 2026-09-16 kit wave):

- **outbox-kit 0.1.0 × breaker `timeout` feature cannot coexist in one
  graph.** outbox-kit's dispatcher matches `CircuitBreakerError` without
  a `Timeout`/wildcard arm, while breaker exposes `Timeout` behind the
  additive `timeout` feature — so any host that enables `timeout`
  anywhere fails to compile outbox-kit's `dispatch` feature (Cargo
  feature unification is graph-wide). This repo dropped the feature it
  previously enabled; outbox-kit should either match exhaustively with a
  wildcard or the variant should not be feature-gated.
- outbox-kit 0.1.0's `MemoryStore` keeps dispatched events as bare ids
  (`mark_dispatched` drops the envelope), so a host cannot ask "how many
  attempts did event X take" after success — the suite records attempts
  at the sender. A `dispatched_count`/audit view on the store would close
  the observability gap.
- chaos-kit 0.1.1's `ChaosLayer` requires `S::Error: From<ChaosError>`;
  axum `Router`'s error type is `Infallible`, so hosts bridge with a
  one-line `map_err`. A provided `From<Infallible>`-friendly adapter (or
  a blanket impl over `Borrow`) would make `Router::layer(chaos_layer(..))
  compile out of the box.
- webhookkit 2.2.0 verifies signatures but exposes no signer (its
  `compute_hmac_sha256` is `pub(crate)`, test-only), so hosts faking the
  provider side reach for `hmac`/`sha2` directly — the same crates, but a
  `sign_hmac_sha256` re-export would drop two dev-dependencies.
- healthkit 1.3.1 and metrics-kit 0.2.0 compose cleanly but neither knows
  about the other: the check-run → counter wiring is host code. A
  `checks_executed_total` family in healthkit (or a metrics sink) would
  remove the duplication every host writes.
- config-kit 0.1.1 env typing infers `i64` for integers; a `u16` port is
  deserialized from the merged view and a mismatch surfaces as `Parse`
  attributed to the last file layer (empty path = `(merged)`) rather than
  to the env layer that supplied the value.
- percentile-kit 0.1.0's `check_budgets` hard-requires `estimates.json`
  per budgeted bench (P99 budgets are optional only when `sample.json` is
  missing) — hosts gating raw samples without criterion bootstrap output
  cannot use the gate as-is.

Round-4 notes (the 2026-09-25 wave — telemetry-init, worker-kit):

- **worker-kit 0.3.0's manifest re-arms the breaker × `timeout` graph
  conflict that round 3 documented for outbox-kit.** worker-kit pins
  breaker with `features = ["timeout"]` on its breaker dependency, so
  the feature unifies graph-wide into every host that also pulls
  outbox-kit 0.1.0 — and outbox-kit's `Dispatcher::process`
  (`src/dispatch.rs:247–271`) matches `CircuitBreakerError` without a
  `Timeout`/wildcard arm: the composition fails with E0004
  (`non-exhaustive patterns: Err(CircuitBreakerError::Timeout) not
  covered`; reproduced in this repo before working around it).
  worker-kit's own runner is already hardened —
  `JobRunner::invoke_through_breaker` (`src/runner.rs:333–341`) carries
  a wildcard arm whose comment cites "the outbox-kit 0.1.0 regression
  class" — but the manifest choice ships the same bomb to hosts. Ask:
  drop the explicit `timeout` feature from worker-kit's breaker dep
  (nothing in worker-kit consumes the variant; the wildcard arm already
  covers it), or ship the missing arm in outbox-kit. This repo runs
  worker-kit `default-features = false` (breaker off) to keep both
  crates in one graph; the failure-budget → `Degraded` flow under test
  lives in the runner, not the breaker, so the suite survives.
- worker-kit 0.3.0's `RunReport` drops the skip/pause counters.
  `JobStatus` records leader `skips` and breaker `paused`, but the
  end-of-run `JobRunSummary` (`src/supervisor.rs:66–77`) keeps only
  fires/failures/degraded/`last_error` — from the report alone a host
  cannot distinguish "leader-locked job that skipped all night" from
  "job that never ticked". This suite polls live `status()` before
  shutdown to observe skips. Adding `skips`/`paused` to the summary
  would make leadership and breaker behavior auditable after the run.
- telemetry-init 0.1.1 gives hosts no way to verify the *installed*
  filter. After `Telemetry::init`, the resolved directive (`RUST_LOG`
  vs config) is invisible — the handle carries only the metrics
  registry — so hosts verifying their bootstrap (as this suite does)
  must go through `build_subscriber(..).max_level_hint()`, a sibling
  pipeline that was never installed (`src/telemetry.rs:175–228` builds
  both from the same `build_pipeline`, but only `init`'s copy is
  global). A `Telemetry::max_level_hint()` — or returning the resolved
  directive — would close the verification gap.
- telemetry-init 0.1.1's `build_subscriber` returns
  `Box<dyn Subscriber + Send + Sync>`, which is not `Debug`, so
  `Result::unwrap_err()` does not compile in host tests (E0277; the
  `Telemetry` handle carries a manual non-exhaustive `Debug` for
  exactly this reason). Hosts match by hand. A light newtype around
  the boxed subscriber with a `Debug` impl would keep the typed-error
  ergonomics the crate promises elsewhere.

Round-5 notes (the estate-integration composition round — clock-kit,
book-kit, wire-kit, hw-kit, uring-kit, ledger-kit, policy-kit,
fetch-kit):

- **fetch-kit 0.2.0's native middleware stack rides reqwest 0.13 while
  the rest of the estate sits on reqwest 0.12.** The two majors coexist
  in one Cargo graph and do NOT unify, so a host implementing
  `fetch_kit::middleware::Middleware` must add a *second* reqwest —
  renamed at the manifest (`reqwest-edge = { package = "reqwest",
  version = "=0.13" }`) — merely to name `Request`/`Response` in the
  trait signature (this repo does exactly that). Ask: re-export the
  reqwest/http types the trait needs (`fetch_kit::http`,
  `fetch_kit::Request`) so hosts can implement the seam without a
  direct reqwest dependency, or hold the estate on one reqwest major.
- **policy-kit 0.1.0's Rego engine (regorus 0.12) unconditionally
  depends on `vstd`** — the Verus standard library, a date-stamped
  `0.0.0-2026-08-23-0033` pre-release from crates.io. Every policy-kit
  consumer inherits a moving 0.0.0 nightly pin (supply-chain and
  reproducibility exposure), and the audit backlog balloons
  (vstd/verus_* alone are six figures of lines in `cargo vet` terms).
  Ask: regorus should feature-gate the verus-backed components, or
  policy-kit should select a feature set that excludes them.
- **clock-kit's monotonic floor is process-wide but calibrations are
  per-clock.** Two independently calibrated `CalibratedClock`s in one
  process fight over the floor: the clock whose ns-per-tick factor came
  out smaller reads below the floor the other set, and its intervals
  flatten to zero (`tests/clock_precision.rs` reproduced this
  systematically before sharing one calibration). Ask: scope the floor
  to the calibration (or expose a shared-calibration registry) so
  multiple clocks in one process compose.
- **worker-kit's degradation latch is transient and invisible in the
  report.** The failure budget counts *consecutive* failures and the
  next success clears `degraded` (`src/runner.rs`); a chaos-style
  intermittently-failing job is degraded only inside the failure
  window, so the end-of-run `RunReport` says `degraded: false` even
  though the budget tripped — the suite had to watch live status on a
  2 ms poll to catch it. Ask: a `degradations` (times-tripped or
  high-water) counter in `JobRunSummary`, complementing the round-4
  skip/pause finding.
- **outbox-kit cannot express "paused" from a sender-level gate.**
  `DispatchError::Delivery` is the only failure outcome, so a host that
  wraps its sender in its own breaker turns every open-circuit shed
  into a failed delivery — each poll during the outage burns an attempt
  (bounded by `max_attempts`; the dispatcher's *built-in* breaker pauses
  without burning, round 3). `tests/outbox_dispatch_metrics.rs` bounds
  the burn instead of asserting a tidy per-event story. Ask: a
  `DispatchError::Paused`-style outcome (or an attempt-exempt error
  class) so sender-level admission control can say "not attempted".
- hw-kit 0.1.0's `current_set().iter()` yields `CpuId` while the pin
  entry points take `CoreId` (`pin_current_core`), and the round-trip is
  manual (`CoreId::new(u16::try_from(cpu.0)...)`) — a `CoreId: TryFrom<
  CpuId>` impl (or a `current_core()` helper returning the core the
  thread last ran on) would smooth the pin-to-where-you-already-are
  path. Minor; everything else about the affinity API is fail-closed
  and pleasant.
- uring-kit 0.1.0 is low-level by design and the completion loop is
  fully drivable from host code (the suite builds a working echo proxy
  on it), but the accept path has a wrinkle: accept completions land in
  an internal map and are retrieved by calling `accept()` again, which
  *also* re-arms implicitly inside `poll` — hosts must know not to
  re-arm per call (the docs say so, but an explicit `rearm`/`retrieve`
  split would make the contract unmissable).
- **a2l-parse 0.1.0 and dbc-parse 0.1.0 cannot be combined without a
  host-side factor inversion, and the A2L file can hide the whole
  scaling.** `dbc_parse::Signal::decode_raw` already returns the
  *scaled* (engineering) value — the method name says "raw", but the
  scale/offset are applied inside — while an A2L `COMPU_METHOD` is
  defined against raw counts. So the host must invert the DBC factor to
  feed the A2L method. Worse, an `IDENTICAL` `COMPU_METHOD` (the natural
  spelling for a pass-through) applies *no* scaling at all: the suite's
  `engine_speed` is 1/16-per-count in the DBC and `IDENTICAL` in the
  A2L, so a host that trusted the A2L description alone reports 51200
  "rpm". Neither crate is wrong and neither can know about the other,
  but the failure mode is silent. Ask: an `A2lError`/lint-free
  `a2l_parse::CompuMethod::is_identity()` plus doc-level guidance that
  an `IDENTICAL` method means "the DBC owns the scaling", so hosts can
  assert the arrangement instead of discovering it in the field.
- **A2L hex literals take no `_` digit separators.** `0x7200_0000`
  lexes as the number `0x7200` followed by the identifier `_0000`
  (`_` is an ident byte in `a2l-parse`'s lexer), which surfaces much
  later as a confusing `UnexpectedToken` — in the suite, `expected:
  "MAX_DIFF"` three blocks downstream. The estate's own Rust fixtures
  write `0x7200_0000` everywhere, so this is an easy mistake to make.
  Not a bug (A2L has no such syntax) but worth a line in the crate
  docs' error examples.
- **xcp-core 0.1.0's `ERR` PID is `0xFE`, and `Expect` must not
  shadow it.** An ERR packet is `[0xFE, code]`; a positive response is
  `[0xFF, …]`. `parse_response_with` checks the ERR PID before
  dispatching on `Expect`, which is right — a host that pattern-matched
  `bytes[0] == 0xFF` as "a response" would misread the error code as
  CONNECT payload. Pinned by a test that runs the same ERR buffer
  through three different `Expect` values and gets the same typed
  error every time.
- **sheet-engine 0.1.0 cannot report a cycle it is not re-evaluating.**
  `recalculate` clears the dirty set even when it returns
  `CircularReference`, so a subsequent `recalculate_incremental(&[])`
  has an empty work set and returns `Ok` — the cycle is still in the
  workbook but is silent. An edit *inside* the cycle is still reported,
  so the practical rule is: a host that only drives the incremental path
  must run one full `recalculate` after loading a workbook. Ask: a
  `has_circular_reference()` predicate, or retaining the cycle members
  so the incremental path re-reports them.
- **formula-lang 0.1.1 has no `RAND`/`RANDBETWEEN`, and its volatile
  set is exactly `TODAY`, `NOW`, `OFFSET`.** `=RAND()` evaluates to an
  error *value*, not a random number, and is (correctly) not volatile —
  there is no value to refresh. Excel treats both as volatile, so a host
  that offers a RAND button will show `#VALUE!` and no host that reads
  the crate docs alone will expect that. The suite pins the real
  boundary with `formula_lang::is_volatile` over five volatile and four
  non-volatile expressions, and asserts the unimplemented functions
  surface as typed error values.
- **sheet-engine 0.1.0 stores an empty input string as empty *text*,
  not as `Empty`.** `set_cell(s, r, c, "")` yields
  `Some(Value::Text(String::new()))`, which does not compare equal to
  `Value::Empty`. A host that wants to distinguish "written blank" from
  "never written" must use `get_value(..) == None` for the latter. Not
  wrong — but the `Empty` variant invites the assumption that it is what
  a blank cell holds.

Round-6 notes (pin refresh to the 2026-10 estate — 17 exact pins moved,
all suites green after the fixes below):

This round exists because `WyattAu/engineering-standards/estate.yml` +
`scripts/estate-audit.py` flagged **17 pins that no longer matched the
version crates.io serves**. Every bump below was applied and every suite
re-run; four needed real work, and one pin is deliberately held back.

- **outbox-kit 0.1.0 breaks ledger-kit 0.1.0 — the pin is held at 0.1.0.**
  outbox-kit 0.1.0 replaced `DispatcherConfig::batch_size` with an
  adaptive `FetchBatch { min, max, park_after }` and moved `OutboxStore`
  to a new trait definition, while `ledger-kit 0.1.0` declares
  `outbox-kit = "^0.1"`. Pinning 0.2.0 puts two majors of outbox-kit in
  one graph and `OutboxJournal::with_inner_and_store` fails with E0308
  (expected `outbox_kit::store::OutboxStore`, found the 0.2 trait).
  **Ask: ledger-kit must ship an outbox-kit 0.2 consumer** (and outbox-kit
  should document `FetchBatch` as a breaking `DispatchConfig` change) —
  the accounting product's durable journal cannot move until it does.
  Everything else in the estate can take 0.2.0 today.
- **worker-kit 0.3.0 → 0.3.0 added four `JobSpec` fields** (`fire_at_start`,
  `drain_pass`, `use_breaker`, `on_degraded`). All three `JobSpec`
  literals across `chaos_worker.rs` and `worker_supervisor_drain.rs` were
  updated with the documented defaults, so the semantics the suites assert
  are unchanged. Two of the new fields close round-4 gaps directly:
  `on_degraded` is the hook the round-4 `RunReport`-drops-degradation ask
  asked for, and `drain_pass` is the outbox-flush-on-shutdown pass. The
  round-4 finding stands for the *summary* (`JobRunSummary` still drops
  skip/pause/degradation counters) — this release adds the input, not the
  report.
- **salting 2.0.0 → 2.0.0 is a major bump with a compatible call shape**
  for this suite: `auth_stack.rs` compiles and passes unchanged. Worth
  noting in the standards' favour that the 2.0 line was published before
  this pin moved (2026-09-16), i.e. the release existed and only the
  consumer lagged — which is exactly the drift class the manifest audit
  now catches on a schedule.
- **healthkit 1.3.1, metrics-kit 0.2.0, webhookkit 2.2.0, telemetry-init
  0.1.1, config-kit 0.1.1, chaos-kit 0.1.1, mailkit 0.3.1, mail-sync-kit
  0.1.1, otelkit 2.0.3, validkit 1.3.1, blobkit 0.4.2, tokenkit 0.4.1,
  throttle-kit 2.0.0, shutdown-kit 0.3.2** all took the pin bump with no
  source change. Two are worth flagging: throttle-kit 2.0.0 (the round-3
  `timeout`-feature conflict with outbox-kit is *not* re-armed — its
  default features still leave `timeout` off, which is why
  `outbox_dispatch.rs` compiles unchanged at the new major) and
  shutdown-kit 0.3.2.

Two suites carried **flaky assertions that were testing the runner, not
the crate**. Both were made deterministic without weakening what they
prove:

- `tests/hw_pinned_hotpath.rs` asserted `p99 <= 10x median` and
  `max <= 40x median`. On a 6-core host at load 48 that fails on a single
  stolen timeslice (observed: p50 151µs, p99 13.1ms) while the bulk is
  tight to 0.03% (min 30852ns, p50 30861ns). Pinning removes *migration*
  cost, which inflates every run; it cannot stop the kernel from
  descheduling the thread, which inflates a few. The assertions now
  measure bulk: ≥3/4 of runs within 10× the median, median within 2× the
  fastest run, a trimmed mean over a 3× band tracking the median, no
  half-to-half drift, and an absolute clock sanity bound (max ≤ 10_000×
  median) so a bad TSC read still fails loudly.
- `tests/outbox_dispatch_metrics.rs` bounded total attempts at a
  hand-tuned 60 and read the `outbox_pending` gauge from a polling
  watcher without awaiting it. Both raced the scheduler: the total is a
  function of how long the open window runs (observed 61 with per-event
  counts of 11–13, all far inside the 50-attempt budget), and the gauge's
  last write can trail the store's drain by one 5ms sleep. The bound is
  now derived — no event may exceed its `max_attempts + 1` ceiling, since
  outbox-kit parks an event at `NEVER` once its budget is spent — and the
  watcher handle is awaited before the render is read. 60 consecutive runs
  green afterwards.

Round-7 notes (the accounting core — `tests/money_stack.rs`):

This is the first composition of the estate's money half, and it is the
suite the planned accounting product depends on. Seven crates, one flow:
`Price` → exact tax → double-entry posting → outbox-mirrored journal →
crash replay → the same figures in a formula-driven workbook → XLSX export.
Three findings, in descending order of how much they matter to a product:

- **The estate's money crate was split across two majors, so a price could
  not be posted. — FIXED.** billing-kit 0.1.1 declared `decimal-money = "^0.2"`
  while ledger-kit 0.1.0 declares `^1.1`. Cargo does not unify across a major
  boundary, so one graph held two `Currency` enums and two `CurrencyAmount`
  types — `Price::gross()` was not a `MonetaryAmount`, and the accounting
  product's central flow (invoice → posting) needed a hand-written conversion
  between them.

  **billing-kit 0.2.0 was published in response to this finding** (from this
  repo's own master, which already aligned on `decimal-money = "1"`). The
  suite now pins the closure rather than the defect:
  `let as_ledger: MonetaryAmount = price.gross();` is a move, not a
  conversion, and it stops compiling if the majors ever split again. That was
  the single highest-value unblock in the estate, and it is closed.
- **ledger-kit's balance signs are inverted from the accounting
  convention.** The fold is *credit adds, debit subtracts*, so a customer
  receivable that grows reads **negative** — where an accountant expects an
  asset debit to increase it. The fold itself is total, derivable and
  auditable, which is the part that matters; but every statement, trial
  balance and P&L in the product needs a chart-of-accounts mapping table
  (assets/expenses negative, liabilities/equity/revenue positive) as host
  logic. Ask: a `normal_balance(AccountClass)` helper or a documented
  `debit_is_credit()` on the crate would put the convention in one place
  instead of every consumer's.
- **`BalancePolicy::RejectNegative` cannot open a book.** The policy
  projects the *debited* side, and a debit subtracts, so on a chart whose
  accounts all start at zero *every* posting is refused — there is no first
  posting, and because the policy is per-ledger rather than per-account, a
  host cannot later say "protect cash, let the receivable float". The suite
  pins all three behaviours. A product must therefore open its books under
  `AllowNegative` (the default) and enforce solvency on the accounts it
  cares about itself.

Composition facts the product can rely on:

- An outbox-mirrored posting replays into a byte-equal ledger, including a
  VAT figure with four decimal places (`396.6627` — a value no binary float
  holds), and a second restore is a no-op, so replay is idempotent.
- Writing ledger-derived numbers into *formula* cells and recalculating
  reproduces exactly the figures the ledger derived, and survives an XLSX
  round trip. The statement an accountant receives cannot disagree with the
  books, because the books wrote the cells the formulas read.
- `decimal-money`'s `Add` is fallible rather than panicking on a currency
  mismatch, allocation uses the largest-remainder method so parts sum to the
  original exactly, and rounding only happens under an explicit
  `RoundingPolicy` (HalfUp vs HalfEven on `0.005` differ, as they must).

One documentation gap worth noting: sheet-engine's public API is 0-based
`(row, col)` while formula text is 1-based (Excel's convention), which the
crate documents in its own rustdoc but a consumer wiring a generated report
meets immediately. It cost this suite a round of off-by-one errors, so the
cross-reference is here for the next one.


Round-8 notes (the provisioning + session stack — `tests/auth_provision.rs`):

The multi-tenant story an accounting firm needs on day one: an IdP pushes
users in over SCIM, accessctl binds each to a role, ws-barbican authenticates
the live transport, and tamper-audit records what happened. Seven crates
composed for the first time. Four findings:

- **tamper-audit made `AuditLog` async in a patch release.** 0.1.0's
  `append`/`query`/`verify_chain` are synchronous; 0.2.0's are `async`.
  Consumers written against 0.1.0 do not fail to compile in a way that
  points at the cause — they fail on `.await` on a non-future, or silently
  block a runtime thread. The suite pins 0.2.0. Ask: the next breaking
  change to this API wants a 0.3.0, since the ecosystem read 0.2.0 as a
  patch.
- **The audit chain is seeded with a genesis `log.created` entry.** So
  `len()`, `total_entries` and any pagination are one higher than the host
  appended, and — the sharper edge — the *first host entry* chains from the
  genesis hash rather than from 64 zeros. An independent verifier that
  assumes "first entry chains from zero" rejects every log the crate
  produces, which is the kind of bug that only shows up in an audit export
  months later. Ask: expose `is_genesis()` (or the genesis entry) on
  `AuditEntry` so verifiers can anchor correctly, and document the
  off-by-one in `len()`.
- **`VerificationResult` carries no boolean verdict.** Validity is
  `broken_at.is_none()` plus `valid_entries == total_entries`, with the
  reason only in `error: Option<String>`. A host that checks
  `result.valid` — the obvious spelling — does not compile, which is fine;
  but the crate's own docs invite the wrong shape. An
  `is_valid()` accessor would make the intent unmissable.
- **accessctl has two independent authorization mechanisms.** The
  hardcoded `RoleHierarchy::check_permission` match and the Cedar
  `PolicySet` (behind the `cedar` feature, which is on by default) are not
  tied together. An operator editing policies and a host calling
  `check_permission` can reach opposite verdicts on the same question, and
  nothing in the crate's surface says so. The suite asserts they agree on
  the default hierarchy — that agreement is currently a coincidence of
  design, not an invariant. Ask: either generate the hardcoded table *from*
  the policy set, or document that they are alternative backends a host
  picks between.

Composition facts worth keeping:

- scim-kit's serde renames are correct and complete: `userName`,
  `externalId`, `totalResults`, `startIndex`, `itemsPerPage` on the wire,
  never the snake_case field names. Filters match on the SCIM attribute
  names, so `active eq false` survives the rename — the property an IdP
  actually depends on.
- `ScimListResponse::total_results` is genuinely independent of
  `resources.len()`, which is the RFC 7644 §3.4.2 contract; confusing the
  two is the classic SCIM pagination bug.
- ws-barbican's extraction precedence is header > registered query key >
  cookie, and an *unregistered* query key is ignored rather than honoured —
  so a link-crafted `?token=` cannot inject a credential. That is the right
  default and worth having pinned.


Round-9 notes (the collaborative-document stack — `tests/collab_docs.rs`):

Shared documents with real-time sync, per-locale rendering, and publication.
Five crates composed for the first time. Four findings, and one contract that
cost this suite a debugging round:

- **`RgaString::apply` does not deduplicate, so any at-least-once transport
  corrupts the document silently.** A replayed `Insert` op appends its
  character again under the *same* `OperationId`; only `Delete` is
  replay-safe, because it sets a tombstone flag on an existing node. This is
  documented — `TextOperation` is described as "commutative and
  idempotent-free by design" — but the consequence deserves more than a
  doc comment: a WebSocket sync protocol must keep its own seen-set of
  operation ids, and `TextOperation` exposes no accessor for the id, so a
  host has to match on the variant shape (`Insert { id, .. }` /
  `Delete { id, .. }`) to deduplicate at all. The suite pins both halves:
  replaying a delete changes nothing, replaying an insert adds exactly one
  character per op. Ask: an `OperationId` accessor, or an `apply_all` that
  takes a seen-set.
- **Causal delivery is a hard precondition**, stated correctly in
  `apply`'s rustdoc: an operation may only be delivered after the operations
  that created its `origin_left`/`origin_right`. The suite pins both
  directions — causally ordered delivery converges under every interleaving,
  and reversing the order visibly diverges. This is standard RGA behaviour
  and correct, but it means the *transport* owes the CRDT an ordering
  guarantee (per-site sequence numbers, or a reorder buffer), and nothing in
  the types says so.
- **The published package `eventbus-kit` has a lib target named
  `typed_eventbus`.** The `use` statement is `use typed_eventbus::…` while
  the dependency is `eventbus-kit`, and docs.rs shows only the package name,
  so the mismatch surfaces as "unresolved import" with nothing pointing at
  the cause. Ask: align the two names.
- **i18n-kit never selects a plural key.** `translate` returns whatever the
  plain key resolves to, so `translate("en", "invoice.items", count=4)`
  yields "4 item". `PluralRule::as_key_suffix()` exists and is correct
  (`for_count` for English rules, `with_zero` for Arabic/Latvian), but
  `translate` does not consult it, and nothing reconciles the rule's
  vocabulary ("other") with a catalog's key naming ("plural"). A host that
  renders "4 items" as "4 item" ships a bug that no i18n-kit test can catch,
  because the rule is available and unused. Ask: `translate_count(key, n)`,
  or document the host obligation at `PluralRule`.

The contract that bit this suite: `CrdtDocument::insert_text` and
`delete_text` apply the edit **locally before returning the operations to
broadcast**. Authoring two replicas' concurrent edits on those replicas
themselves and then applying the returned op sets double-counts them — which
looks exactly like the missing-dedup bug above, and cost a round of
debugging before the authoring fixture was moved to separate replicas. It is
the easiest way to misuse the crate and deserves an explicit note in its
rustdoc.


Round-10 notes (the single-binary service substrate — `tests/systems_substrate.rs`):

The layer everything else in the estate assumes composes: actors on a
work-stealing scheduler, a lock-free pool for hot-path buffers, a
shared-memory ring between processes, and a TTL cache in front. Four crates,
first composition. **Three of the findings are bugs, not sharp edges**, and
none is visible to a per-crate test:

- **A suspended actor can never be resumed or stopped.** `worker_loop`'s
  dispatch has three arms: `Running | Creating` processes the message and
  calls `handle_state_change_for` (which is what applies Pause / Resume /
  Stop), `Suspended` **re-queues** whatever it is handed, and `_ => {}` drops
  everything. So a suspended worker never processes the `Resume` signal that
  would lift the suspension — it re-queues it instead, and the only arm that
  can leave `Suspended` is unreachable while suspended. `resume().await` and
  `stop().await` both return `Ok` and change nothing; the mailbox keeps
  accepting messages that are never processed. Only dropping the scheduler
  (which clears the running flag) releases the actor. Ask: the `Suspended`
  arm must let control signals through, or `Suspended` needs a mailbox for
  signals distinct from ordinary messages. This is the most severe finding
  in the estate so far — it is reachable from two documented handle methods
  and it is a permanent deadlock.
- **A shared-memory ring stalls permanently on an unconsumed reader slot.**
  `try_push` refuses when `write_idx - slowest_read_idx >= capacity`, and
  `slowest_read_idx` folds over *every* provisioned cursor with `u64::MIN`.
  A consumer that is merely slow — paused, not yet reading, or crashed —
  pins its cursor at 0 and the producer is refused for good. Reproduced
  exactly: capacity 64, two provisioned readers, reader 0 drains all 64
  values, `try_push` still returns `false`; the same sequence with a
  fully-drained single reader recovers.

  This one is the crate's **documented contract** ("an unused reader slot is
  a permanently slow reader"), so it is not a bug — the suite now pins both
  halves as behaviour rather than as a defect. What the docs do *not* say is
  the failure mode a host must design around: there is no reader timeout, no
  reclaim, and no `Drop` on the reader handle, so a crashed consumer wedges
  the producer until the ring is destroyed and recreated. A supervisor
  handling a ring should know that. Ask: a `Drop` that releases the cursor,
  or an explicit `unregister_reader`, would turn "destroy the ring" into
  "restart the reader".
- **Lifecycle transitions are asynchronous with no completion signal.**
  `start().await` returning `Ok` means the message was *accepted*; the
  registry only flips to `Running` when a worker dequeues it. A host that
  gates on `is_running()` right after `start()` reads `Creating`, and there
  is no `await transition` primitive to reach for. Ask: a `wait_for_state`
  on the handle, or make the lifecycle calls synchronous with the registry.
- **`shared-state` never re-exports `TtlCache`.** The module is `pub mod
  ttl` with a full implementation, but `lib.rs` re-exports only `ReadyGate`,
  so `use shared_state::TtlCache` fails with an unresolved import that points
  nowhere near the real path (`shared_state::ttl::TtlCache`). Ask: re-export
  it, since it is the crate's most obviously useful type.

Composition facts worth keeping:

- `SlabPool::alloc` returns a guard whose lifetime is tied to the pool
  borrow, so the borrow checker — not a runtime check — is what makes a
  lock-free pool sound. Exhaustion is a typed `None`, a zero-capacity pool is
  refused at construction, and dropping the guards returns every slot.
- `TtlCache` measures expiry from insertion, so a hot key still expires.
  That is the safe default (a sliding window would hide a stale entry under
  constant reads), and `take_fresh` is the atomic read-and-remove a
  single-consumer queue needs — doing it with `get` + `remove` would hand the
  same job to two workers.
- `ReadyGate` is not `Clone`, so a host holding it in both a health endpoint
  and a shutdown hook needs an `Arc` around it. Readiness latches, and
  revocation is always deliberate — which is what you want in front of a load
  balancer.
- `ActorBuilder::spawn` takes `&Arc<ActorScheduler>`, so the scheduler is
  shareable and usable from a Tokio service.

Integration trap worth recording: the scheduler's workers are OS threads that
each need a Tokio runtime context to poll actor futures. Under
`#[tokio::test]` (a current-thread runtime) the workers never get one, the
actors stay in `Creating`, and the test *hangs* rather than failing
informatively. `actor_kit::rt().block_on(..)` on a plain `#[test]` is the
shape that works — and the crate ships `rt()` for exactly this, but the
requirement is invisible until a suite trips over it.
