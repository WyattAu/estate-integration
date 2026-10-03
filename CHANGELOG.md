# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

### Added
- Round-5 composition suites — ten flows proving the consumer-less
  crates compose (all exact-pinned at their published versions, verified
  via the crates.io API):
  - `tests/clock_precision.rs`: clock-kit (TSC-calibrated) +
    percentile-kit — calibrated reads monotonic and typed; one measured
    workload into both a `LatencyRing` and a `PercentileTracker` with
    bit-identical nearest-rank statistics; committed-style budget PASS
    and a typed `BudgetExceeded` FAIL naming the metric.
  - `tests/book_feed.rs`: wire-kit + book-kit — FIX 4.4 `35=X` encode →
    parse/checksum → typed `Command` mapping → `replay` (`GapPolicy`)
    → depth exactness, replay checksum-identical to direct application;
    truncation, corruption, and journal gaps are typed failures.
  - `tests/hw_pinned_hotpath.rs`: hw-kit (`pin_current_core`, fail-closed
    mask read-back) + clock-kit — a pinned hot loop measured into a
    `LatencyRing`, tolerance-bounded stability (P99 ≤ 10× median, no
    half-to-half drift), calibrated-vs-`Instant` cross-check, affinity
    restored and verified; typed pin failures and `CpuSet` round-trips.
  - `tests/uring_proxy.rs`: uring-kit + breaker + throttle-kit — a real
    echo proxy on an `io_uring` engine thread under breaker-first /
    GCRA-second admission (CIRCUIT_OPEN pause, THROTTLED retry_after,
    two scripted failures trip, half-open probe recovers); exact
    decision ledger; token/probe/pool contracts hold on any host.
  - `tests/ledger_settlement.rs`: ledger-kit + outbox-kit +
    idempotency-kit — settlement post → `OutboxJournal` durable mirror
    (envelope id == posting id) → `DuplicatePosting`/`InvalidPosting`
    retry semantics → dispatcher delivers once → `restore` replays the
    undelivered tail into a fresh ledger → genesis-aware chain verify;
    `RejectNegative` refuses an overdraw before anything records.
  - `tests/policy_gateway.rs`: policy-kit + fetch-kit — a
    `fetch_kit::middleware::Middleware` evaluates a Rego bundle over the
    request document; violations short-circuit before `Next::run` (no
    network, no retry budget), poisoned engines fail closed; verdicts
    attributed per rule; null-safety asserted.
  - `tests/chaos_worker.rs`: chaos-kit + worker-kit — a supervisor job
    whose work unit is a tower service under a seeded `ChaosLayer`
    (latency every call, two consecutive scripted outages): the failure
    budget trips the degradation latch (observed live — see findings),
    the worker survives and drains < 2 s, and the same seed reproduces
    the identical fault sequence across two sessions; recorder counts
    exact.
  - `tests/config_tenant.rs`: config-kit per-tenant resolution — base +
    tenant override files, deep merge (one nested knob overridden,
    siblings kept), per-tenant differences, secrets redacted through the
    merged load, `load_strict` naming the typo.
  - `tests/telemetry_pipeline.rs`: telemetry-init + metrics-kit +
    percentile-kit — init → register → record into both the histogram
    and the tracker → 0.0.4-valid scrape (inline parser, histogram
    aggregates consistent with the tracker) → budget PASS + outlier
    FAIL → idempotent shutdown flush.
  - `tests/outbox_dispatch_metrics.rs`: outbox-kit + metrics-kit +
    breaker — dispatch with a metric per attempt
    (`outbox_dispatch_total{outcome}`, `outbox_pending`) through a
    breaker-wrapped sender under failure injection; the open circuit's
    sheds appear as `paused` in the parsed render and the transitions in
    the dispatch report, and the two views count the same attempts.
- Round-5 integration findings (README): fetch-kit 0.2.0's middleware
  stack rides reqwest 0.13 against the estate's 0.12 (hosts implementing
  the trait need a renamed second reqwest); policy-kit's regorus
  unconditionally pulls the date-stamped `vstd` Verus pre-release;
  clock-kit's process-wide mono floor fights per-clock calibrations
  (zero-interval flattening); worker-kit's degradation latch clears on
  the next success and is invisible in `RunReport`; outbox-kit cannot
  express "paused" from a sender-level gate (attempt burn during
  open-circuit windows); hw-kit `CpuId`/`CoreId` round-trip friction;
  uring-kit's accept retrieve/re-arm wrinkle.

### Changed
- Round-4 dogfooding suites for the 2026-09-25 wave:
  - `tests/telemetry_bootstrap.rs`: telemetry-init one-call bootstrap
    with a metrics budget → register counter/gauge/histogram through
    `Telemetry::metrics()` → render → validate as Prometheus 0.0.4 with
    an inline parser; typed `AlreadyInitialized` on double init;
    idempotent `shutdown`; `RUST_LOG` override asserted via
    `build_subscriber().max_level_hint()`. `otlp` stays off (hermetic).
  - `tests/worker_supervisor_drain.rs`: worker-kit supervisor with three
    jobs (fast-succeeding; slow-overrun coalesced, never stacked;
    always-failing rollup → `Degraded` past its budget), run on the
    shared shutdown-kit `ShutdownGuard`, drained < 2 s, `RunReport`
    name-ordered and exact; leadership via the `leader` feature
    (`MemoryLease` default fires, a never-winning host lease records
    skips, never failures); same-seed jitter windows identical across
    supervisors.
- Round-4 integration findings (README): worker-kit's breaker dep
  re-arms the `timeout` feature conflict that breaks outbox-kit's
  compile graph-wide; `RunReport` drops skip/pause counters;
  telemetry-init exposes no way to verify the installed filter; the
  `build_subscriber` return type is not `Debug`.
- Round-3 dogfooding suites for the 2026-09-16 kit wave (all exact-pinned
  at their published 0.1.0):
  - `tests/config_secrets.rs`: config-kit layered load (file → env →
    override precedence), `Sensitive<T>` redaction through `Debug`/
    `Display`, `load_strict` unknown-key denial.
  - `tests/idempotent_webhook.rs`: webhookkit HMAC/Stripe verification →
    idempotency-kit claim (First/Replay/release-and-retry).
  - `tests/outbox_dispatch.rs`: outbox-kit dispatcher with a flaky sender
    — breaker trip, open-circuit pause without attempt burn, half-open
    probe recovery, 5/5 dispatched, shutdown < 2 s.
  - `tests/percentile_report.rs`: tracker quantiles vs sorted reference,
    markdown snapshot, criterion fixture + committed budgets PASS and
    typed `BudgetExceeded` FAIL.
  - `tests/chaos_resilience.rs`: chaos-kit tower layer over axum with a
    seeded throttle/error schedule; 100 requests match the schedule
    exactly; same-seed reproduction; paused-clock latency fault.
  - `tests/metrics_scrape.rs`: healthkit checks driving a metrics-kit
    registry, rendered and validated as Prometheus 0.0.4 by an inline
    parser.

### Changed
- CI: adopt the org-level `cargo vet` gate — the shared rust-kit workflow
  gained an unconditional `vet` job after round 3; the store is
  bootstrapped with `cargo vet init` (current lockfile as the vetted
  baseline, 435 locked crates), `cargo vet --locked` green locally.
- breaker pin 2.0.0 → 2.0.1 (test-only release over 2.0.0; current exact
  pin — the round-3 outbox suite configures the dispatcher's breaker with
  breaker 2.0.1's own `CircuitBreakerConfig`/`BackoffStrategy` types).
- New dependencies (exact-pinned estate kits): config-kit, idempotency-kit,
  outbox-kit, percentile-kit, chaos-kit, metrics-kit, webhookkit; dev-deps
  `hmac`/`sha2` to compute the webhook signatures webhookkit verifies.
- Round 4 adds (exact-pinned estate kits): telemetry-init 0.1.0 (default
  features; `otlp` stays off), worker-kit 0.1.0 (`default-features =
  false`, features `leader` — the default `breaker` feature forces
  breaker's `timeout` feature graph-wide and breaks outbox-kit 0.1.0's
  compile; see the README round-4 findings), shutdown-kit 0.3.0;
  metrics-kit 0.1.0 was already pinned.
- Round 5 adds (exact-pinned estate kits, versions verified via the
  crates.io API): clock-kit 0.1.0 (`tsc` feature), book-kit 0.1.0,
  wire-kit 0.1.0, hw-kit 0.1.0 (`libc` feature), uring-kit 0.1.0,
  ledger-kit 0.1.0 (default: audit+outbox), policy-kit 0.1.0 (default:
  json), fetch-kit 0.2.0 (default: json+rustls-tls+retry — hermetic);
  plus `http` 1 and a renamed `reqwest-edge` (=0.13) dependency for the
  fetch-kit middleware seam (see the README round-5 findings).

## [0.1.0] - 2026-09-12

### Added
- `tests/auth_stack.rs`: register (validkit email → salting Argon2id+pepper
  hash) → login (verify → tokenkit JWT mint) → authenticated request
  (decode + revocation check); wrong-password, expired-token,
  revoked-token, and invalid-email rejection; HS256/RS256/ES256 roundtrips;
  key rotation via `kid`; barbican extractor + middleware HTTP guards.
- `tests/resilient_api.rs`: axum + throttle-kit per-IP tower layer (429s),
  breaker 2.x tower layer opening against a wiremock 500 upstream,
  healthkit livez/readyz/startup-group probes with dependency-driven
  readiness, otelkit per-request spans with no collector.
- `tests/mail_pipeline.rs`: validkit recipient gate (rejected pre-send),
  mailkit MIME multipart + attachment build, sieve-kit keep/vacation/flag
  plan evaluation, wiremock SendGrid send with receipt.
- `tests/media_upload.rs`: validkit object-key gate, media-kit
  sniff/bomb-guard/EXIF-orient/parallel variants, blobkit variant storage,
  cas-kit dedup (exact-dupe second upload stores nothing new).
- `tests/sync_client.rs`: in-memory `MockStore` implementing the
  mail-sync-kit `MailStore` seam, sync events over a persistent eventbus,
  SQLite durability, replay after restart-simulation.
- `examples/`: one runnable living-doc example per suite.
- Pinned exact estate versions: salting 1.2.1, tokenkit 0.4.0, validkit
  1.3.0, barbican 0.2.1, breaker 2.0.0, healthkit 1.2.0, throttle-kit 1.1.1,
  otelkit 2.0.2, mailkit 0.3.0, sieve-kit 0.2.1, media-kit 0.2.1, blobkit
  0.4.1, cas-kit 0.2.1, mail-sync-kit 0.1.0, eventbus-kit 0.3.5.
