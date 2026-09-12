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

Hermetic CI: tempdirs, wiremock, in-process SQLite. No cloud credentials,
no external network.
```

## Suites

| Suite | Proves | Crates |
|---|---|---|
| `tests/auth_stack.rs` | register → login → JWT → authenticated request; wrong-password, expired-token, revoked-token, invalid-email rejection; key rotation; barbican HTTP guards | salting 1.2.1, tokenkit 0.4.0, validkit 1.3.0, barbican 0.2.1 |
| `tests/resilient_api.rs` | 429 on limit breach; breaker opens against failing upstream (wiremock); /readyz reflects dependency state; startup group separation; spans without a collector | breaker 2.0.0, healthkit 1.2.0, throttle-kit 1.1.1, otelkit 2.0.2 |
| `tests/mail_pipeline.rs` | invalid recipient rejected pre-send; vacation outcome produced; provider receipt recorded | mailkit 0.3.0, validkit 1.3.0, sieve-kit 0.2.1 |
| `tests/media_upload.rs` | duplicate upload dedup assertion; oversized-bomb rejection; variant count + EXIF orient | media-kit 0.2.1, blobkit 0.4.1, cas-kit 0.2.1, validkit 1.3.0 |
| `tests/sync_client.rs` | MockStore seam ingest; events persisted to SQLite; replay after restart-simulation | mail-sync-kit 0.1.0, eventbus-kit 0.3.5 |

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
