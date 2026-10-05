# estate-integration

Cross-crate dogfooding suite for the WyattAu crate estate: the estate's
crates used **together** the way real users combine them. Each suite is a
real running system (`tests/` proves it, `examples/` shows it).

> **This repo proves the crates compose.** Every suite wires 2–5 published
> estate crates into one flow with exact-pinned dependencies, so any failure
> is attributable to real drift between crates — never to a moving version.
> This repo stays GitHub-only by design: integration suites are bins/examples,
> not a library, and are never published to crates.io.

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
| `tests/auth_stack.rs` | register → login → JWT → authenticated request; wrong-password, expired-token, revoked-token, invalid-email rejection; key rotation; barbican HTTP guards | salting 1.2.1, tokenkit 0.4.0, validkit 1.3.0, barbican 0.2.1 |
| `tests/resilient_api.rs` | 429 on limit breach; breaker opens against failing upstream (wiremock); /readyz reflects dependency state; startup group separation; spans without a collector | breaker 2.0.1, healthkit 1.2.0, throttle-kit 1.1.1, otelkit 2.0.2 |
| `tests/mail_pipeline.rs` | invalid recipient rejected pre-send; vacation outcome produced; provider receipt recorded | mailkit 0.3.0, validkit 1.3.0, sieve-kit 0.2.1 |
| `tests/media_upload.rs` | duplicate upload dedup assertion; oversized-bomb rejection; variant count + EXIF orient | media-kit 0.2.1, blobkit 0.4.1, cas-kit 0.2.1, validkit 1.3.0 |
| `tests/sync_client.rs` | MockStore seam ingest; events persisted to SQLite; replay after restart-simulation | mail-sync-kit 0.1.0, eventbus-kit 0.3.5 |
| `tests/config_secrets.rs` | file → env → in-process-override precedence ladder (deep merge); `Sensitive<String>` redacted through the loaded struct's `Debug`/`Display`; `load_strict` names the unknown top-level key; lenient `load` still accepts the same file | config-kit 0.1.0 |
| `tests/idempotent_webhook.rs` | HMAC-SHA256 gate (raw + Stripe `t=…,v1=…` envelope, locally signed); event id → `IdempotencyKey`; first delivery processes, duplicate hits Replay without re-executing; failed attempt releases the claim and the retry succeeds | webhookkit 2.0.0, idempotency-kit 0.1.0 |
| `tests/outbox_dispatch.rs` | flaky sender (2 failures then success) trips the dispatcher's breaker; open circuit pauses without burning attempt budget; half-open probe closes; all 5 events dispatched with per-event attempts recorded; graceful shutdown < 2 s | outbox-kit 0.1.0, breaker 2.0.1 |
| `tests/percentile_report.rs` | tracker quantiles equal the sorted `nearest_rank` reference and are monotone; markdown row snapshot; criterion-shaped `estimates.json`/`sample.json` fixture gated to PASS, then to a typed `BudgetExceeded` FAIL; budget TOML rejects typo'd keys | percentile-kit 0.1.0 |
| `tests/chaos_resilience.rs` | axum service under `chaos_layer` (seeded 30 % throttle + scripted error at index 7); 100 requests match the precomputed schedule exactly; recorder counts exact; same seed reproduces bit-for-bit; paused-clock latency fault | chaos-kit 0.1.0 (+ axum/tower) |
| `tests/metrics_scrape.rs` | healthkit checks drive a metrics-kit registry (per-method counter, inflight gauge, duration histogram); rendered registry validated as Prometheus 0.0.4 by an inline parser: TYPE lines, cumulative buckets, `+Inf` == `_count`, `_sum` consistent | metrics-kit 0.1.0, healthkit 1.2.0 |
| `tests/telemetry_bootstrap.rs` | one-call bootstrap: the configured metrics budget caps registration through `Telemetry::metrics()`; labeled counter + gauge + histogram render a valid Prometheus 0.0.4 exposition (inline parser); double init is the typed `AlreadyInitialized`, `shutdown` is idempotent, and `RUST_LOG` overrides the configured directive (asserted via `build_subscriber` + `max_level_hint`) | telemetry-init 0.1.0, metrics-kit 0.1.0 |
| `tests/worker_supervisor_drain.rs` | three jobs (fast-succeeding; slow-overrun that coalesces instead of stacking; always-failing rollup that turns `Degraded` at its failure budget and keeps running) run on the shared shutdown-kit `ShutdownGuard`, drain < 2 s, and the name-ordered `RunReport` is exact; default no-lease leadership fires (`MemoryLease` always wins) while a never-winning host lease records skips — never failures; same-seed jitter windows identical across supervisors | worker-kit 0.1.0 (no default features — see round-4 findings), shutdown-kit 0.3.0 |
| `tests/clock_precision.rs` | calibrated clock reads are monotonic and typed (TSC where the host offers an invariant counter, `Instant` fallback where it does not); one measured workload into BOTH a clock-kit `LatencyRing` and a percentile-kit `PercentileTracker` — every statistic identical (nearest-rank agreement across the L1/L2 boundary); observed quantiles gated by a committed-style budget (PASS) and an inflated window rejected with the typed `BudgetExceeded` naming the metric | clock-kit 0.1.0 (`tsc`), percentile-kit 0.1.0 |
| `tests/book_feed.rs` | FIX 4.4 market-data feed: `FixBuilder` encode → `FixMessage::parse` + checksum verify → typed mapping to book-kit `Command`s (`AddBid`/`AddAsk`/`Execute`) → `replay` under `GapPolicy::Fail` → depth exactness after partial and full fills, checksum-identical to direct application; truncation, body corruption, and journal gaps are all typed failures | wire-kit 0.1.0, book-kit 0.1.0 |
| `tests/hw_pinned_hotpath.rs` | pin to the current core (`pin_current_core`, fail-closed mask read-back), measure a fixed hot loop with the calibrated clock into a `LatencyRing`, assert tolerance-bounded stability (P99 ≤ 10× median, max ≤ 40×, no half-to-half drift — never exact equality on shared CI), cross-check the calibrated p50 against `Instant`, restore and verify the original affinity; typed failures (empty mask, out-of-range core) and `CpuSet` round-trips | hw-kit 0.1.0 (`libc`), clock-kit 0.1.0 (`tsc`) |
| `uring_proxy.rs` | a real echo proxy on an `io_uring` engine thread (listener → completion-driven accept → `ReadFixed` → gate → `WriteFixed` → close): breaker-first admission answers `CIRCUIT_OPEN` before capacity is spent, an exhausted GCRA budget answers `THROTTLED retry_after_ms=<n>`, two scripted backend failures trip the circuit, the half-open probe recovers; exact decision ledger + breaker counters; token/probe/pool contracts hold on any host (ring creation may be denied by the kernel/seccomp — the flagship test reports the skip) | uring-kit 0.1.0, breaker 2.0.1, throttle-kit 1.1.1 |
| `tests/ledger_settlement.rs` | full settlement flow: double-entry post (debit A, credit B) mirrored through `OutboxJournal` into an outbox-kit store (envelope id == posting id); idempotency-gated retry (`DuplicatePosting` with the original id; key reuse with a fresh request → `InvalidPosting`); dispatcher delivers exactly once; crash recovery replays the undelivered tail into a fresh ledger; `RejectNegative` refuses an overdraw before anything records; audit chain verifies (genesis-aware) with hash-linked entries | ledger-kit 0.1.0, outbox-kit 0.1.0, idempotency-kit 0.1.0 |
| `tests/policy_gateway.rs` | policy-gated HTTP: a `fetch_kit::middleware::Middleware` evaluates a Rego bundle over the request document (method/path/headers) — compliant requests proceed to a wiremock upstream, violating ones short-circuit before `Next::run` (no network, no retry budget spent), evaluation errors and a poisoned engine reject fail-closed; verdicts are attributed per rule; null-safety of the input document | policy-kit 0.1.0, fetch-kit 0.2.0 (+ reqwest 0.13 — see round-5 findings) |
| `tests/chaos_worker.rs` | a worker-kit supervisor job whose work unit is a tower service under `ChaosLayer` (seeded 5 ms latency on every call, scripted errors at two consecutive indexes): the failure budget trips the degradation latch — observed live, because a post-outage success clears the flag (round-5 finding) — the worker keeps firing past degradation and drains < 2 s; the same seed reproduces the identical fault sequence across two independent sessions; recorder counts are exact | chaos-kit 0.1.0, worker-kit 0.1.0 (no default features) |
| `tests/config_tenant.rs` | per-tenant resolution: base config + tenant override files through `ConfigBuilder` layers — deep merge (one nested knob overridden, siblings kept), the same key resolving differently per tenant, secrets redacted through every render of the merged load, and `load_strict` naming a typo'd tenant key | config-kit 0.1.0 |
| `tests/telemetry_pipeline.rs` | the full observability pipeline in one process: `Telemetry::init` → register counter/gauge/histogram → six stage latencies into BOTH the metrics-kit histogram and a percentile-kit tracker → scrape validated as Prometheus 0.0.4 (inline parser; `_count`/`_sum`/`+Inf` consistent with the tracker's truth) → budget gate PASS + outlier FAIL → idempotent shutdown flush | telemetry-init 0.1.0, metrics-kit 0.1.0, percentile-kit 0.1.0 |
| `tests/outbox_dispatch_metrics.rs` | dispatch with a metric per attempt (`outbox_dispatch_total{outcome=delivered/failed/paused}`, `outbox_pending` gauge) through a breaker-wrapped sender under failure injection: the open circuit's sheds are visible as `paused` in the parsed render and the breaker's transitions appear in the dispatch report — and the two views count the same attempts (sender-level breaker burn bounded; round-5 finding) | outbox-kit 0.1.0, metrics-kit 0.1.0, breaker 2.0.1 |
| `tests/calibration_stack.rs` | the whole automotive calibration pipeline: A2L description (addresses, datatypes, limits, `COMPU_METHOD` coefficients) + DBC bit layout (`start_bit`/`bit_length`/`byte_order`/`scale`) bound host-side; XCP `CONNECT` negotiates the byte order and `MAX_CTO`, which then parameterises every `SET_MTA`/`UPLOAD`/DAQ frame built at the A2L addresses; a DAQ DTO crosses `can_core` in both directions (command → SocketCAN bytes → `CanFrame` → packet) and back out through `dbc-parse` `decode_raw` and the A2L conversion; the write path frames a limit-checked value for `DOWNLOAD`; the slave `ERR` taxonomy, truncated buffers, and an oversized packet are all typed failures | a2l-parse 0.1.0, dbc-parse 0.1.0, can-core 0.1.0, xcp-core 0.1.0 |
| `tests/spreadsheet_engine.rs` | the product layer end to end: literals and formulas, a quarterly model (SUM over ranges, IF, margin ratios), a 200-cell dependency chain and a 4,000-formula cone — each edit asserted through the invariant that **`recalculate_incremental` and `recalculate` agree cell-for-cell**; cross-sheet and quoted-sheet references (including a dangling sheet that heals when the sheet appears), formula→literal edge removal, error-value propagation, cycle detection with members, volatile tracking, XLSX round-trip that preserves a *live* dependency graph, and `VLOOKUP` over 100 rows | sheet-engine 0.1.0, formula-lang 0.1.1 |

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

- tokenkit 0.4.0 flattens every `jsonwebtoken` error into
  `JwtError::DecodingFailed(String)` — `JwtError::Expired` is never
  constructed, so hosts cannot distinguish expiry from other failures.
- barbican 0.2.1 depends on tokenkit 0.1, semver-incompatible with tokenkit
  0.4 in the same graph; composition goes through barbican's tokenkit-free
  surface (`BearerToken`, `auth_middleware_fn`).
- healthkit 1.2.0 registers checks through a blocking lock — build the
  registry before entering the Tokio runtime.
- mail-sync-kit 0.1.0 ships the `MailStore` trait seam but no mock; this
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
- chaos-kit 0.1.0's `ChaosLayer` requires `S::Error: From<ChaosError>`;
  axum `Router`'s error type is `Infallible`, so hosts bridge with a
  one-line `map_err`. A provided `From<Infallible>`-friendly adapter (or
  a blanket impl over `Borrow`) would make `Router::layer(chaos_layer(..))
  compile out of the box.
- webhookkit 2.0.0 verifies signatures but exposes no signer (its
  `compute_hmac_sha256` is `pub(crate)`, test-only), so hosts faking the
  provider side reach for `hmac`/`sha2` directly — the same crates, but a
  `sign_hmac_sha256` re-export would drop two dev-dependencies.
- healthkit 1.2.0 and metrics-kit 0.1.0 compose cleanly but neither knows
  about the other: the check-run → counter wiring is host code. A
  `checks_executed_total` family in healthkit (or a metrics sink) would
  remove the duplication every host writes.
- config-kit 0.1.0 env typing infers `i64` for integers; a `u16` port is
  deserialized from the merged view and a mismatch surfaces as `Parse`
  attributed to the last file layer (empty path = `(merged)`) rather than
  to the env layer that supplied the value.
- percentile-kit 0.1.0's `check_budgets` hard-requires `estimates.json`
  per budgeted bench (P99 budgets are optional only when `sample.json` is
  missing) — hosts gating raw samples without criterion bootstrap output
  cannot use the gate as-is.

Round-4 notes (the 2026-09-25 wave — telemetry-init, worker-kit):

- **worker-kit 0.1.0's manifest re-arms the breaker × `timeout` graph
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
- worker-kit 0.1.0's `RunReport` drops the skip/pause counters.
  `JobStatus` records leader `skips` and breaker `paused`, but the
  end-of-run `JobRunSummary` (`src/supervisor.rs:66–77`) keeps only
  fires/failures/degraded/`last_error` — from the report alone a host
  cannot distinguish "leader-locked job that skipped all night" from
  "job that never ticked". This suite polls live `status()` before
  shutdown to observe skips. Adding `skips`/`paused` to the summary
  would make leadership and breaker behavior auditable after the run.
- telemetry-init 0.1.0 gives hosts no way to verify the *installed*
  filter. After `Telemetry::init`, the resolved directive (`RUST_LOG`
  vs config) is invisible — the handle carries only the metrics
  registry — so hosts verifying their bootstrap (as this suite does)
  must go through `build_subscriber(..).max_level_hint()`, a sibling
  pipeline that was never installed (`src/telemetry.rs:175–228` builds
  both from the same `build_pipeline`, but only `init`'s copy is
  global). A `Telemetry::max_level_hint()` — or returning the resolved
  directive — would close the verification gap.
- telemetry-init 0.1.0's `build_subscriber` returns
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
