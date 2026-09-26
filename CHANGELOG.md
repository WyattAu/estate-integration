# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

### Added
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
