# Dogfood coverage — what `estate-integration` actually proves

Generated from this repo's `Cargo.toml` (the exact pins) against
`engineering-standards/scripts/estate-audit.py` (the manifest). Refresh both
after any publish or pin change; this file is the human view of that report.

**56 crates composed** across 30 suites, out of 161 published under the
account. The other 32 are *coverage debt*: published, working, and
never proven to work alongside their neighbours.

> Coverage is not a vanity metric here. Every suite exists to catch
> cross-crate drift that per-crate tests cannot see, and it has: the
> outbox-kit × breaker `timeout` compile break, the ledger-kit × outbox-kit
> major conflict, the decimal-money 0.2/1.1 split, a suspended actor that
> can never be resumed, and a shared-memory ring one stalled reader wedges
> forever are all findings only a composing suite can produce.

## Composed

| area | crates |
|---|---|
| app | `policy-kit` |
| auth | `accessctl`, `barbican`, `salting`, `scim-kit`, `tamper-audit`, `tokenkit` |
| conc | `actor-kit`, `book-kit`, `clock-kit`, `hw-kit`, `shared-state`, `shm-rings`, `slab-pool`, `uring-kit` |
| data | `a2l-parse`, `blobkit`, `can-core`, `cas-kit`, `crdts-kit`, `dbc-parse`, `docs-pipeline`, `eventbus-kit`, `i18n-kit`, `media-kit`, `validkit`, `xcp-core` |
| money | `billing-kit`, `decimal-money`, `formula-lang`, `ledger-kit`, `sheet-core`, `sheet-engine` |
| net | `breaker`, `fetch-kit`, `idempotency-kit`, `loop-retry`, `mail-sync-kit`, `mailkit`, `outbox-kit`, `resilient-fetch`, `sieve-kit`, `throttle-kit`, `webhookkit`, `wire-kit`, `ws-barbican`, `ws-kit` |
| obsv | `chaos-kit`, `config-kit`, `healthkit`, `metrics-kit`, `otelkit`, `percentile-kit`, `shutdown-kit`, `telemetry-init`, `worker-kit` |

## Coverage debt

| area | debt | crates |
|---|---|---|
| app | 1 | `model-router` |
| auth | 4 | `cryptkit`, `multi-chain-wallet`, `oauth-toolkit`, `webauthn-kit` |
| conc | 2 | `chronoshift`, `poolkit` |
| data | 18 | `api-paginate`, `api-types`, `cache-pal`, `contact-miner`, `delta-kit`, `dsp-core`, `error-classify`, `error-classify-derive`, `error-codes`, `font-parse`, `geo-kit`, `http-errors`, `json-envelope`, `simd-tokenizer`, `tantivy-helper`, `typed-id-derive`, `typed-id-new`, `validkit-derive` |
| money | 1 | `sheet-xlsx` |
| net | 2 | `axum-stack`, `uds-kit` |
| obsv | 4 | `envstack`, `flag-kit`, `otel-stack`, `pid-manager` |
| **total** | **32** | |

## Retiring debt

Each round adds suites and closes debt. Counts are crates newly composed:

| round | suites | newly composed |
|---|---|---|
| 1–2 | `auth_stack`, `resilient_api`, `mail_pipeline`, `media_upload`, `sync_client` | 19 |
| 3 | `config_secrets`, `config_tenant`, `idempotent_webhook`, `outbox_dispatch`, `percentile_report`, `chaos_resilience`, `metrics_scrape` | 7 |
| 4 | `telemetry_bootstrap`, `telemetry_pipeline`, `worker_supervisor_drain` | 3 |
| 5 | `clock_precision`, `book_feed`, `hw_pinned_hotpath`, `uring_proxy`, `ledger_settlement`, `policy_gateway`, `chaos_worker`, `outbox_dispatch_metrics` | 9 |
| 6 | pin refresh (17 stale exact pins) + de-flake two runner-sensitive suites | 0 |
| 7 | `money_stack` — the accounting core | 6 |
| 8 | `auth_provision` — SCIM + RBAC + session + audit | 5 |
| 9 | `collab_docs` — CRDT convergence + i18n + publication | 4 |
| 10 | `systems_substrate` — actors + pool + shared memory + cache | 4 |
| 11 | `calibration_stack`, `spreadsheet_engine` (product-layer round) | 12 |

Debt fell from **53 → 32** over rounds 7–11.

## Exempt by design

Some families are covered by their own test suites rather than by an
estate-integration suite, and the audit is told so explicitly in
`COVERAGE_EXEMPT`:

- `leptos-*` — UI components, exercised by `crdt-demo` and the site
- `vane-*`, `crawlkit-*`, `suture-*` — product monorepos with their own suites
- `polyfont-*` — dormant since 2026-05
- `ply*` — wasm-first charting
- `resilient-fetch` — superseded by `fetch-kit`, kept pinned for the migration
- sibling crates pulled transitively (derive macros, renamed packages)

## Reading the findings

Each round appends to this repo's README under **Integration findings**.
Every finding names the crate, the version, what breaks, and an ask. They
are filed as prose rather than tracked as tickets because each one needs a
reproduction, and the reproduction *is* the suite.
