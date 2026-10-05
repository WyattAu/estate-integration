# Dogfood coverage — what `estate-integration` actually proves

Generated from this repo's `Cargo.toml` (73 exact pins) against
`engineering-standards/scripts/estate-audit.py` (the manifest). Refresh both
after any publish or pin change; this file is the human view of that report.

The 161 crates published under the account divide three ways:

- **71 composed** — pulled by a suite here and exercised against their
  neighbours, in 33 suites.
- **75 exempt** — product monorepos, UI components and dormant families that
  carry their own tests (listed below).
- **16 debt** — published, working, and never proven to work alongside
  anything. This is the only number worth watching.

> Coverage is not a vanity metric here. Every suite exists to catch
> cross-crate drift that per-crate tests cannot see, and it has: the
> outbox-kit × breaker `timeout` compile break, the ledger-kit × outbox-kit
> major conflict, the decimal-money 0.2/1.1 split, a suspended actor that
> can never be resumed, a shared-memory ring one stalled reader wedges
> forever, and a BIP-39 implementation that rejects every published test
> vector are all findings only a composing suite can produce.

## Composed (71)

| area | crates |
|---|---|
| app | `policy-kit` |
| auth | `accessctl`, `barbican`, `cryptkit`, `multi-chain-wallet`, `oauth-toolkit`, `salting`, `scim-kit`, `tamper-audit`, `tokenkit`, `webauthn-kit` |
| conc | `actor-kit`, `book-kit`, `clock-kit`, `hw-kit`, `shared-state`, `shm-rings`, `slab-pool`, `uring-kit` |
| data | `a2l-parse`, `api-paginate`, `api-types`, `blobkit`, `can-core`, `cas-kit`, `crdts-kit`, `dbc-parse`, `docs-pipeline`, `error-classify`, `error-codes`, `eventbus-kit`, `http-errors`, `i18n-kit`, `json-envelope`, `media-kit`, `typed-id-derive`, `typed-id-new`, `validkit`, `xcp-core` |
| money | `billing-kit`, `decimal-money`, `double-entry`, `formula-lang`, `ledger-kit`, `sheet-core`, `sheet-engine` |
| net | `breaker`, `fetch-kit`, `idempotency-kit`, `mail-sync-kit`, `mailkit`, `outbox-kit`, `resilient-fetch`, `sieve-kit`, `throttle-kit`, `webhookkit`, `wire-kit`, `ws-barbican`, `ws-kit` |
| obsv | `chaos-kit`, `config-kit`, `envstack`, `flag-kit`, `healthkit`, `metrics-kit`, `otel-stack`, `otelkit`, `percentile-kit`, `pid-manager`, `shutdown-kit`, `telemetry-init`, `worker-kit` |

## Coverage debt (16)

| area | count | crates |
|---|---|---|
| app | 1 | `model-router` |
| conc | 2 | `chronoshift`, `poolkit` |
| data | 11 | `cache-pal`, `contact-miner`, `delta-kit`, `dsp-core`, `error-classify-derive`, `font-parse`, `geo-kit`, `simd-tokenizer`, `tantivy-helper`, `validkit-derive` |
| money | 1 | `sheet-xlsx` |
| net | 1 | `uds-kit` |

Ten of the eleven `data` entries are parsers, extractors and derive macros with
no shared surface; `cache-pal` is the one worth wiring, since it is the only
crate here that competes for memory with `shm-rings`. `chronoshift` is
similarly real: `clock-kit` is already composed, and two clocks in one process
is a bug factory.

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
| 12 | `api_errors` + `flags_and_lifecycle` — error envelope and process ownership | 12 |
| 13 | `crypto_auth` — HMAC + WebAuthn + PKCE + CSRF + wallet | 4 |
| 14 | `accounting_core` — the new immutable double-entry ledger, composed against `ledger-kit` | 1 |

Debt fell **53 → 32 → 24 → 20 → 16** over rounds 7–14.

## Exempt by design (75)

Some families are covered by their own test suites rather than by an
estate-integration suite, and the audit is told so explicitly in
`COVERAGE_EXEMPT`:

- `suture-*` (36) — product monorepo under evaluation, has its own suites
- `leptos-*` (8), `ply*` (5), `polyfont-*` (9) — UI and charting, exercised by
  `crdt-demo` and the site; `polyfont-*` is dormant since 2026-05
- `vane-*` (11) — separate product monorepo with its own suite
- `crawlkit-*` (2) — separate product monorepo with its own suite
- `resilient-fetch`, `loop-retry` — superseded and renamed; still pinned
- sibling crates pulled transitively (derive macros, renamed packages)

## Reading the findings

Each round appends to this repo's README under **Integration findings**.
Every finding names the crate, the version, what breaks, and an ask. They
are filed as prose rather than tracked as tickets because each one needs a
reproduction, and the reproduction *is* the suite.
