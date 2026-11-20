# Pattern adoption: estate-integration

Relationships to the pattern canon (pattern-standards), verified against code.
Format: pattern-standards/references/adoption-format.md. Composition-test
harness: src/ is a stub, suites live in tests/, one per estate capability.

| Pattern | Status | Evidence |
|---|---|---|
| T27 | adopted | tests/ledger_settlement.rs:9 (double-entry settlement posting through Ledger::post over a durable OutboxJournal) |
| T32 | adopted | tests/outbox_dispatch.rs:6 (transactional outbox: events appended to the store, flaky sender retried to completion) |
| T33 | adopted | tests/idempotent_webhook.rs:16 (duplicate deliveries replay the recorded response: delivery exactly once) |
| T12 | adopted | tests/worker_supervisor_drain.rs:8 (WorkerSupervisor with drain; late ticks coalesced, never stacked) |
| P54 | adopted | tests/idempotent_webhook.rs:9 (HMAC-SHA256 signature computed locally and verified before consumption) |
