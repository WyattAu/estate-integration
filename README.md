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
