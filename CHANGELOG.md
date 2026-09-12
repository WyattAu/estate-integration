# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

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
