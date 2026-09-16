#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 3, suite 2 — `idempotent_webhook`: webhookkit 2.0.0 +
//! idempotency-kit 0.1.0.
//!
//! One real webhook-consumer flow:
//!
//! 1. **verify** — an HMAC-SHA256 signature is computed locally (the
//!    `hmac`/`sha2` crates webhookkit itself builds on) and verified with
//!    webhookkit, once raw and once through the Stripe `t=…,v1=…`
//!    envelope;
//! 2. **derive** — the verified event id becomes an `IdempotencyKey`
//!    (deterministic BLAKE3, so retries converge on the same claim);
//! 3. **claim** — `MemoryStore` + `IdempotencyExecutor` process the
//!    delivery exactly once: duplicate deliveries replay the recorded
//!    response, and a failed first attempt releases the claim so the
//!    provider's retry succeeds.

use hmac::{Hmac, Mac};
use idempotency_kit::{Claim, IdempotencyExecutor, IdempotencyKey, IdempotencyStore, MemoryStore};
use sha2::Sha256;
use std::time::Duration;

/// The signing secret the provider and the consumer share (in real life:
/// fetched from the vault at startup).
const WEBHOOK_SECRET: &[u8] = b"whsec_estate_integ_round3";

/// Hex-encode a digest the way providers render `v1=` signatures.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compute HMAC-SHA256(payload, secret) locally — the provider side of
/// the handshake. Uses the same `hmac`/`sha2` pair webhookkit 2.0.0
/// builds on, so the graphs unify (estate feedback: webhookkit computes
/// but does not re-export a signer, so hosts reach for these crates
/// themselves to fake the provider).
fn sign(payload: &[u8], secret: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key size");
    mac.update(payload);
    hex_encode(&mac.finalize().into_bytes())
}

/// Raw `verify_hmac_sha256`: the locally-computed signature verifies, and
/// a one-byte tamper with the payload breaks it — the verification gate
/// that must pass before anything idempotent happens.
#[test]
fn hmac_signature_gate_accepts_valid_and_rejects_tampered() {
    let payload = br#"{"id":"evt_gate_1","type":"ping"}"#;
    let signature = sign(payload, WEBHOOK_SECRET);

    // Valid signature: Ok.
    webhookkit::verify_hmac_sha256(payload, WEBHOOK_SECRET, signature.as_bytes())
        .expect("valid signature verifies");

    // Tampered payload (what an attacker sends): the gate must close.
    let tampered = br#"{"id":"evt_gate_1","type":"ping","amount":999999}"#;
    let err = webhookkit::verify_hmac_sha256(tampered, WEBHOOK_SECRET, signature.as_bytes())
        .expect_err("tampered payload must not verify");
    assert!(
        matches!(err, webhookkit::WebhookError::InvalidSignature),
        "unexpected error: {err:?}"
    );

    // Wrong secret (key rotation midpoint mismatch): also closed.
    let err = webhookkit::verify_hmac_sha256(payload, b"whsec_other", signature.as_bytes())
        .expect_err("wrong secret must not verify");
    assert!(matches!(err, webhookkit::WebhookError::InvalidSignature));
}

/// The full delivery flow: Stripe-envelope verification → event id →
/// idempotency key → atomic claim. First delivery processes, the
/// duplicate replays the recorded response without re-executing, and the
/// whole thing composes without glue beyond the event-id bytes.
#[tokio::test]
async fn webhook_delivery_processes_once_and_replays_duplicates() {
    // -- The provider's delivery: Stripe envelope, locally signed. The
    //    timestamp is "now" so webhookkit's default 300 s tolerance holds.
    let body = serde_json::json!({
        "id": "evt_estate_0001",
        "type": "payment_intent.succeeded",
        "data": { "amount": 4200 }
    })
    .to_string();
    let timestamp = chrono::Utc::now().timestamp();
    let signed_payload = format!("{timestamp}.{body}");
    let sig_header = format!(
        "t={timestamp},v1={}",
        sign(signed_payload.as_bytes(), WEBHOOK_SECRET)
    );

    // -- The consumer: verify, then lift the event id out of the payload.
    let event = webhookkit::verify_stripe_webhook(&body, &sig_header, "whsec_estate_integ_round3")
        .expect("stripe envelope verifies");
    assert_eq!(event.event_type, "payment_intent.succeeded");
    let event_id = event
        .payload
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("event id on the verified payload")
        .to_owned();

    // -- Derive the claim key from the verified event id: deterministic,
    //    so a provider retry (or a crash + restart) lands on the same key.
    let key = IdempotencyKey::derive("webhook-delivery", event_id.as_bytes()).expect("valid scope");
    assert!(key.as_str().starts_with("webhook-delivery:"));

    let store = MemoryStore::new();
    let processed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // First delivery: claims First, runs the handler, records the
    // response.
    let first = IdempotencyExecutor::execute(&store, &key, Duration::from_secs(300), {
        let processed = std::sync::Arc::clone(&processed);
        move || {
            let processed = std::sync::Arc::clone(&processed);
            async move {
                processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok::<_, std::convert::Infallible>(b"{\"status\":\"applied\"}".to_vec())
            }
        }
    })
    .await
    .expect("first delivery processes");
    assert_eq!(first, b"{\"status\":\"applied\"}");

    // At-least-once redelivery (provider timeout replay): the claim
    // returns Replay with the recorded response — the handler must not
    // run twice.
    let replay = match store.claim(&key, Duration::from_secs(300)).await.unwrap() {
        Claim::Replay(bytes) => bytes,
        other => panic!("duplicate delivery must replay, got {other:?}"),
    };
    // Via the executor the same holds, and the handler stays at 1 run.
    let replayed = IdempotencyExecutor::execute(&store, &key, Duration::from_secs(300), || async {
        Ok::<_, std::convert::Infallible>(b"sentinel: must not run".to_vec())
    })
    .await
    .expect("duplicate delivery replays");
    assert_eq!(replay.as_slice(), b"{\"status\":\"applied\"}");
    assert_eq!(replayed, first, "replay returns the original bytes");
    assert_eq!(
        processed.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "exactly-once processing across the duplicate"
    );
}

/// The error path: a failing first attempt releases the claim (instead of
/// wedging the key as InFlight for the whole TTL), so the provider's
/// retry runs the handler and succeeds.
#[tokio::test]
async fn failed_attempt_releases_claim_and_retry_succeeds() {
    let key = IdempotencyKey::derive("webhook-delivery", b"evt_retry_0001").expect("valid scope");
    let store = MemoryStore::new();

    // First attempt fails (dependency down): ExecutionFailed, claim gone.
    let err = IdempotencyExecutor::execute(&store, &key, Duration::from_secs(300), || async {
        Err::<Vec<u8>, _>("ledger unreachable")
    })
    .await
    .expect_err("first attempt fails");
    assert!(
        matches!(err, idempotency_kit::IdempotencyError::ExecutionFailed(ref m) if m == "ledger unreachable"),
        "unexpected error: {err:?}"
    );
    assert!(store.is_empty(), "failed execution must release the claim");

    // The provider retries: claim is First again, handler runs, response
    // is recorded for any further redelivery.
    let retry = IdempotencyExecutor::execute(&store, &key, Duration::from_secs(300), || async {
        Ok::<_, std::convert::Infallible>(b"{\"status\":\"applied\"}".to_vec())
    })
    .await
    .expect("retry after release succeeds");
    assert_eq!(retry, b"{\"status\":\"applied\"}");

    // A third delivery inside the TTL replays the retry's response.
    let again = IdempotencyExecutor::execute(&store, &key, Duration::from_secs(300), || async {
        Ok::<_, std::convert::Infallible>(b"sentinel".to_vec())
    })
    .await
    .expect("post-retry redelivery replays");
    assert_eq!(again, retry);
}
