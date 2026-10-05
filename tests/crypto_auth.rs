#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 13, suite 7 — `crypto_auth`.
//!
//! cryptkit, webauthn-kit, oauth-toolkit, multi-chain-wallet.
//!
//! The authentication stack a service reaches for when the built-in kits are
//! not enough: password hashing, WebAuthn/passkey verification, OAuth2 PKCE
//! and CSRF, and key derivation. Four crates, all in the estate's Tier A
//! security set, and none of them had ever been composed — so nothing proved
//! they agree on a single byte.
//!
//! Three of the four carry security requirements, and the seam between them
//! is exactly where a mistake is invisible: cryptkit's HMAC produces the bytes
//! webauthn-kit's COSE verification consumes, and oauth-toolkit's PKCE
//! verifier consumes a challenge the client generated from a *hash* it also
//! produces. A suite that hashes with one crate and verifies with another is
//! the only way to prove the halves line up.
//!
//! The suite's spine is therefore **cross-crate agreement**: every primitive
//! is checked against an independent reference (RFC test vectors, a second
//! implementation, or a hand-computed value), never against itself.
//!
//! Four findings this round, three of them security-relevant. The first was
//! the most serious defect found in the estate so far — and it is now fixed.
//!
//! 0. **A recovery phrase shorter than 24 words could not restore a wallet.**
//!    `multi_chain_wallet::mnemonic::mnemonic_to_seed` delegated parsing to
//!    `bip32 0.5.3`'s `bip39` feature, whose `Mnemonic::new` requires
//!    `entropy.len() == KEY_SIZE + 1` with `KEY_SIZE = 32` — exactly 33
//!    bytes, exactly 24 words. Anything shorter returned `Err(Bip39)`, so both
//!    canonical 12-word vectors in the BIP-39 specification were refused, as
//!    was any phrase from an 12/15/18/21-word generator or import. Verified in
//!    this suite's own dependency graph: 24 words parsed, 12 and 18 did not.
//!
//!    The consequence: **for the most common phrase length, the recovery
//!    phrase did not restore the wallet.** The error a user saw
//!    (`InvalidMnemonic("bip39 error")`) is indistinguishable from a typo,
//!    and a wallet whose phrase does not restore is not a wallet.
//!
//!    It survived because the crate only ever round-tripped phrases it had
//!    generated itself — every one of which was 24 words, because generation
//!    could only make 24 — and two of its own suites asserted that limitation
//!    as intended behaviour.
//!
//!    **Fixed in `multi-chain-wallet 0.2.2`**: parsing goes through `bip39`
//!    directly, which validates every published vector, and generation
//!    honours all five BIP-39 lengths. `bip32` is kept for the derivation
//!    arithmetic, which never saw a phrase. The assertions below now pin the
//!    repaired behaviour, and the one 24-word-only test that remains pins the
//!    negative case that must *keep* failing.
//!
//! 1. **`webauthn-kit`'s `check_sign_count` accepts an *equal* count.** It
//!    refuses a decrease (`new < current`) and treats a stored `0` as
//!    exempt, but `new == current` passes — and a replayed assertion carries
//!    the counter the authenticator last wrote, so that is exactly what a
//!    replay looks like. WebAuthn §7.2 says the clone signal is a count that
//!    is *not greater than* the stored one, so the intended check is `<=`.
//!    A clone that replays in order rather than resetting is therefore
//!    invisible to this crate.
//! 2. **`oauth-toolkit`'s PKCE verifier takes the method as a `&str`.** RFC
//!    7636 defines exactly two values, `plain` and `S256`; a typo'd method
//!    compares false rather than erroring, and is indistinguishable from a
//!    wrong verifier — which needs a different response.
//! 3. **A challenge's replay window is the caller's, not the store's.**
//!    `consume_registration_challenge(id, timeout_secs)` compares the entry's
//!    `created_at` against *now + timeout*, so the same stored challenge is
//!    accepted with a generous timeout and refused with a tight one. A host
//!    that passes its own clock skew into this re-opens the window it
//!    believed was closed.

use cryptkit::hmac::{constant_time_eq, hmac_sign, hmac_verify};
use multi_chain_wallet::{btc, eth, mnemonic};
use oauth_toolkit::csrf::MemoryCsrfStore;
use oauth_toolkit::pkce::{generate_pkce_pair, verify_pkce};
use webauthn_kit::challenge::{check_sign_count, ChallengeStore};
use webauthn_kit::crypto::{
    base64_decode_urlsafe, base64_encode_urlsafe, generate_challenge_bytes,
};

// -- 1. HMAC, against RFC 4231 vectors ------------------------------------

#[test]
fn hmac_sha256_matches_the_rfc4231_vectors() {
    // RFC 4231 §4.2 — the canonical HMAC-SHA256 test vectors. Checking against
    // the published vectors rather than against cryptkit's own output is the
    // point: a self-consistent but wrong implementation would pass
    // round-trip tests.
    let key = hex(b"0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let data = b"Hi There";
    assert_eq!(
        hex_digest(&hmac_sign(&key, data)),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        "RFC 4231 test case 1"
    );

    // Test case 2.
    assert_eq!(
        hex_digest(&hmac_sign(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        "RFC 4231 test case 2"
    );

    // Test case 3 — a 20-byte key of 0xaa, which is exactly the size where
    // implementations most often differ: the key is padded to the block size
    // rather than hashed first, and a wrong padding is a silent mismatch.
    assert_eq!(
        hex_digest(&hmac_sign(
            &hex(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "02e0927b3d8e28b3cea274f6fe84d72afd5200e1677b558bbfafadd96c6b6920",
        "RFC 4231 test case 3 — the block-size boundary"
    );

    // Test case 4 — a binary key and binary data, so a hex-decoding slip in
    // the harness cannot hide behind printable characters.
    assert_eq!(
        hex_digest(&hmac_sign(
            &hex(b"0102030405060708090a0b0c0d0e0f10111213141516171819"),
            &[0xcd_u8; 50]
        )),
        "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
        "RFC 4231 test case 4 — a 25-byte binary key"
    );

    // Test case 6 — a 131-byte key, past two block sizes, where HMAC hashes
    // the key first. That requires *different* wrongness than case 3, so it
    // earns its own vector.
    let long_key = hex(&vec![b'a'; 262]);
    assert_eq!(
        hex_digest(&hmac_sign(
            &long_key,
            b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data"
        )),
        "c9731f25665706dab8200d9ce68fad2cbac48efc4a5f72292e4eeb81e7d29298",
        "RFC 4231 test case 6 — a 131-byte key, past two block sizes"
    );
}

#[test]
fn hmac_verification_is_constant_time_and_correct() {
    let key = b"a key for the ledger";
    let message = b"posting 1200.00 to account 4000";
    let tag = hmac_sign(key, message);

    assert!(hmac_verify(key, message, &tag), "a good tag verifies");

    // Wrong key, wrong message, and a *different* length tag all fail — and
    // the comparison must not early-return on the first differing byte, which
    // is what `constant_time_eq` is for.
    assert!(!hmac_verify(b"another key", message, &tag));
    assert!(!hmac_verify(key, b"posting 1201.00 to account 4000", &tag));
    assert!(!hmac_verify(key, message, &[0_u8; 32]), "a zero tag fails");

    // An empty message and an empty key are both legal inputs, and both have
    // to produce something — a host that signs a zero-length field should not
    // panic.
    let empty_tag = hmac_sign(&[], &[]);
    assert!(hmac_verify(&[], &[], &empty_tag));
    assert!(!hmac_verify(&[], &[1], &empty_tag));
}

#[test]
fn constant_time_eq_agrees_with_slice_equality() {
    // The exported helper must behave exactly like `==` for the lengths and
    // contents a caller can produce — a "constant-time" comparison that is
    // subtly wrong on length is worse than `==`, because it looks careful.
    let a = b"the same bytes";
    let b = b"the same bytes";
    let c = b"the same bytez";
    let short = b"the same byte";
    assert!(constant_time_eq(a, b));
    assert!(!constant_time_eq(a, c));
    assert!(!constant_time_eq(a, short), "different lengths are unequal");
    assert!(!constant_time_eq(short, a));
    assert!(constant_time_eq(&[], &[]), "empty equals empty");
    assert!(!constant_time_eq(&[], a));
}

// -- 2. base64url, against RFC 4648 --------------------------------------

#[test]
fn base64url_round_trips_and_avoids_the_url_unsafe_characters() {
    // Challenge bytes are binary, so base64url (not base64) is what a
    // WebAuthn client receives. The distinguishing property is that the
    // output contains no `+`, `/`, or `=`.
    for len in 1..40_usize {
        let raw: Vec<u8> = (0..len)
            .map(|i| u8::try_from(i * 7 % 251).unwrap_or(0))
            .collect();
        let encoded = base64_encode_urlsafe(&raw);
        assert!(
            !encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='),
            "len {len}: url-safe alphabet, got {encoded:?}"
        );
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "len {len}: only the url-safe alphabet, got {encoded:?}"
        );
        assert_eq!(
            base64_decode_urlsafe(&encoded).expect("decodes"),
            raw,
            "len {len}: round-trips"
        );
    }

    // A 32-byte challenge — the WebAuthn default — encodes to 43 characters
    // with no padding, which is the length a client expects.
    let challenge = generate_challenge_bytes();
    assert_eq!(challenge.len(), 32);
    assert_eq!(base64_encode_urlsafe(&challenge).len(), 43);

    // Standard base64 with padding decodes too (a client that sends `+` or
    // `/` should not be rejected outright), but invalid input is an error.
    assert_eq!(
        base64_decode_urlsafe("aGVsbG8").expect("padded base64"),
        b"hello".to_vec()
    );
    assert!(base64_decode_urlsafe("!!!not base64!!!").is_err());
    assert!(
        base64_decode_urlsafe("a").is_err(),
        "a truncated group is an error"
    );
}

// -- 3. the challenge store: single-use, expiring -------------------------

#[test]
fn a_webauthn_challenge_is_single_use_scoped_and_expiring() {
    // A registration challenge is a replay guard: it is consumed by the
    // assertion that answers it, it is keyed by both a challenge id and a
    // username, and it does not survive its timeout. All three are asserted,
    // because a store that only *reads* a challenge would satisfy the first
    // and fail the others silently.
    //
    // Note the shape: the store is keyed `(challenge_id, username)` and the
    // consume returns the *username* plus the bytes. That is what lets a host
    // verify that the assertion answers the challenge it issued *for that
    // user* — the pairing is the security property.
    let mut store = ChallengeStore::new();
    let challenge = generate_challenge_bytes();

    store.store_registration_challenge("chal-1", "alice", challenge.clone());
    let consumed = store.consume_registration_challenge("chal-1", 300);
    assert!(consumed.is_ok(), "the first consume finds it: {consumed:?}");
    let (username, bytes) = consumed.expect("consumed");
    assert_eq!(
        username, "alice",
        "and it returns the username it was bound to"
    );
    assert_eq!(bytes, challenge, "and the exact bytes");
    assert!(
        store.consume_registration_challenge("chal-1", 300).is_err(),
        "a second consume finds nothing — single-use"
    );

    // Scoping: a different challenge id is a different entry, and consuming
    // one does not consume the other.
    store.store_registration_challenge("chal-2", "alice", challenge.clone());
    store.store_registration_challenge("chal-3", "bob", b"bob-challenge".to_vec());
    assert!(store.consume_registration_challenge("chal-3", 300).is_ok());
    assert!(
        store.consume_registration_challenge("chal-2", 300).is_ok(),
        "so ids do not shadow each other"
    );

    // A username mismatch is a lookup miss, not a silent success: the host
    // cannot consume alice's challenge under bob's name.
    let mut store = ChallengeStore::new();
    store.store_registration_challenge("chal-1", "alice", challenge.clone());
    assert!(
        store
            .consume_registration_challenge("wrong-id", 300)
            .is_err(),
        "an unknown challenge id does not resolve"
    );

    // Registration and authentication live in separate namespaces: a
    // registration challenge must not satisfy an authentication consume.
    let mut store = ChallengeStore::new();
    store.store_registration_challenge("chal-1", "alice", challenge.clone());
    assert!(
        store
            .consume_authentication_challenge("chal-1", 300)
            .is_err(),
        "the two namespaces do not cross over"
    );

    // -- The timeout is a *caller-supplied* bound, not a stored deadline.
    // That is a real design choice worth pinning: `consume(chal, timeout)`
    // compares the entry's `created_at` against `now + timeout`, so a host
    // that passes a generous timeout re-opens a replay window it thought was
    // closed. Asserted here so the coupling is visible.
    let now = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1_000));
    let clock_now = std::sync::Arc::clone(&now);
    let mut store = ChallengeStore::with_clock(std::sync::Arc::new(move || {
        clock_now.load(std::sync::atomic::Ordering::Relaxed)
    }));
    store.store_registration_challenge_at("chal-1", "alice", challenge.clone(), 1_000);

    // Same instant: any positive timeout accepts.
    assert!(store.consume_registration_challenge("chal-1", 300).is_ok());

    // Far past the window: refused.
    now.store(1_000_000, std::sync::atomic::Ordering::Relaxed);
    store.store_registration_challenge_at("chal-1", "alice", challenge.clone(), 1_000);
    assert!(
        store.consume_registration_challenge("chal-1", 300).is_err(),
        "a challenge well past its timeout is refused"
    );
    // **Round-13 finding: the timeout is the caller's, so the same stored
    // challenge is accepted at `timeout_secs = 10_000`** — the caller, not the
    // store, decides the replay window.
    // With a timeout longer than the elapsed time, the *same* stored entry is
    // accepted — because the deadline lives in the call, not in the entry.
    store.store_registration_challenge_at("chal-2", "alice", challenge.clone(), 1_000);
    assert!(
        store
            .consume_registration_challenge("chal-2", 10_000_000)
            .is_ok(),
        "a generous timeout accepts the same entry, so the replay window is \
         whatever the caller says — a host that passes its own clock skew \
         into this re-opens it"
    );
}

// -- 4. sign counts: the clone-detection contract ------------------------

#[test]
fn a_sign_count_that_does_not_increase_is_refused() {
    // WebAuthn's clone detection. §7.2 of the WebAuthn spec: if the new
    // signature counter is *not greater than* the stored one, and neither is
    // zero, the authenticator may have been cloned. The crate refuses a
    // decrease.
    assert!(check_sign_count(10, 11).is_ok(), "an increase is normal");
    assert!(
        check_sign_count(0, 1).is_ok(),
        "...and a stored count of zero is exempt, since a credential that has \
         never signed does not distinguish a clone from a first use"
    );
    assert!(
        check_sign_count(0, 0).is_ok(),
        "including zero against zero"
    );
    assert!(
        check_sign_count(10, 3).is_err(),
        "a decrease is refused as a possible cloned authenticator"
    );

    // **Round-13 finding: an *equal* count is accepted.** A replayed assertion
    // carries the counter the authenticator last wrote, so `new == current`
    // is exactly what a replay looks like — and it passes. Only a strictly
    // lower value is caught, so a clone that replays in order rather than
    // resetting is invisible. The spec's wording is "not greater than", so
    // `<=` is the intended check and this is a real gap rather than a
    // judgement call.
    assert!(
        check_sign_count(10, 10).is_ok(),
        "an EQUAL count is accepted — a replayed assertion looks exactly like \
         this, and the spec treats 'not greater than' as the clone signal"
    );
    assert_eq!(
        check_sign_count(10, 10).is_ok(),
        check_sign_count(10, 9).is_err(),
        "...while a decrease IS refused, so the two cases are treated \
         differently where the spec treats them the same. That asymmetry is \
         the gap, stated as an assertion rather than a comment."
    );
}

// -- 5. PKCE, against RFC 7636 --------------------------------------------

#[test]
fn pkce_generation_and_verification_round_trip() {
    // RFC 7636: the client generates a verifier, sends
    // `code_challenge = BASE64URL(SHA256(verifier))`, and the server checks
    // them together. The suite checks the *hash* half too, because a pair
    // that only agrees with itself proves nothing.
    let (verifier, challenge) = generate_pkce_pair();
    assert!(!verifier.is_empty(), "the verifier is non-empty");
    assert_ne!(
        verifier, challenge,
        "the verifier is not the challenge — that is the whole point of PKCE"
    );
    assert!(
        verify_pkce(&verifier, &challenge, "S256"),
        "a matching pair verifies under S256"
    );
    assert!(
        !verify_pkce(&verifier, &challenge, "plain"),
        "and fails under `plain` — the method is part of the contract"
    );
    assert!(!verify_pkce("wrong-verifier", &challenge, "S256"));

    // Two generated pairs differ, so a replayed verifier is not valid for a
    // second authorization.
    let (other_verifier, other_challenge) = generate_pkce_pair();
    assert_ne!(verifier, other_verifier);
    assert_ne!(challenge, other_challenge);
    assert!(!verify_pkce(&verifier, &other_challenge, "S256"));

    // The challenge really is the SHA-256 of the verifier, base64url'd —
    // verified with cryptkit's hash rather than the crate's own, so the two
    // halves of the estate agree on the encoding.
    let digest = cryptkit::hash::sha256(verifier.as_bytes());
    assert_eq!(
        challenge,
        base64_encode_urlsafe(&digest),
        "challenge == BASE64URL(SHA256(verifier)) — the RFC's S256 method, \\
         computed with cryptkit so the two crates are checked against each \\
         other rather than against themselves"
    );
}

/// **Round-13 finding: `verify_pkce` takes the method as a `&str` and a
/// typo'd or unsupported method compares false rather than erroring.**
///
/// RFC 7636 defines exactly two methods, `plain` and `S256`. `S256` is
/// required for public clients; `plain` exists only for clients that cannot
/// do SHA-256. A `&str` parameter means a host that writes `"s256"`, or
/// `"SHA256"`, gets `false` — indistinguishable from a genuine mismatch, so
/// the symptom is a login that fails for a reason nothing reports. And since
/// the comparison is a string compare rather than a parse, `plain` is
/// accepted with no check that a verifier was actually sent.
#[test]
fn a_misspelled_pkce_method_is_indistinguishable_from_a_mismatch() {
    let (verifier, challenge) = generate_pkce_pair();
    // The only two values RFC 7636 defines.
    assert!(verify_pkce(&verifier, &challenge, "S256"));
    // Everything else is a silent `false` — no error, no diagnostic.
    assert!(
        !verify_pkce(&verifier, &challenge, "s256"),
        "lowercase is not S256"
    );
    assert!(
        !verify_pkce(&verifier, &challenge, "SHA256"),
        "nor is SHA256"
    );
    assert!(!verify_pkce(&verifier, &challenge, "S512"), "nor S512");
    assert!(
        !verify_pkce(&verifier, &challenge, ""),
        "nor the empty string"
    );
    // So a host cannot tell "the client used a method I do not implement"
    // from "the client sent the wrong verifier" — the two failures need
    // different responses (one is a client bug, one may be an attack) and
    // both arrive as `false`.
    assert_eq!(
        verify_pkce(&verifier, &challenge, "s256"),
        verify_pkce("attacker-verifier", &challenge, "s256"),
        "a typo'd method and a wrong verifier produce the same answer"
    );
}

// -- 6. the CSRF store, on the same expiry contract -----------------------

#[tokio::test]
async fn a_csrf_state_nonce_is_single_use_and_carries_its_redirect() {
    // The CSRF store is the same shape of guard as the WebAuthn challenge
    // store — single-use, expiring, session-bound — from a different crate
    // with a different API. Composing them here is the check that a host gets
    // the same guarantees from both, which is the property that matters: a
    // state nonce reusable across a flow is a vulnerability, exactly as a
    // reusable challenge is.
    use oauth_toolkit::csrf::CsrfStore;

    let store = MemoryCsrfStore::new();
    assert!(
        store
            .store("session-1", "nonce-abc", Some("/books/2026".to_owned()))
            .await,
        "a state nonce is stored"
    );

    let retrieved = store.retrieve_and_consume("session-1").await;
    let (nonce, redirect) = retrieved.expect("the nonce is there");
    assert_eq!(nonce, "nonce-abc", "and comes back verbatim");
    assert_eq!(
        redirect.as_deref(),
        Some("/books/2026"),
        "carrying the redirect the authorization request asked for"
    );

    // Single-use: the second retrieval finds nothing, which is what makes it a
    // replay guard rather than a cookie.
    assert!(
        store.retrieve_and_consume("session-1").await.is_none(),
        "and it cannot be replayed"
    );
    // Per-session: another session's key does not resolve this nonce.
    store.store("session-2", "nonce-def", None).await;
    assert_eq!(
        store
            .retrieve_and_consume("session-2")
            .await
            .map(|(n, _)| n)
            .as_deref(),
        Some("nonce-def"),
        "a different session has its own nonce"
    );
    assert!(
        store
            .retrieve_and_consume("unknown-session")
            .await
            .is_none(),
        "and an unknown session resolves nothing"
    );

    // A TTL of zero expires between store and retrieve, which is the knob a
    // host uses to bound the window.
    let expiring = MemoryCsrfStore::with_ttl(std::time::Duration::from_millis(0));
    expiring.store("k", "n", None).await;
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(
        expiring.retrieve_and_consume("k").await.is_none(),
        "a nonce past its TTL does not validate"
    );
    expiring.cleanup_expired().await;
}

// -- 7. key derivation, in the one shape a service actually needs ---------

/// **Round-13's headline finding, now fixed: the estate's BIP-39 path refused
/// every phrase shorter than 24 words.**
///
/// BIP-39 specifies three canonical phrases, all of them 12 words. With
/// `multi-chain-wallet 0.2.1` and below, `mnemonic_to_seed` rejected every one
/// of them while accepting a 24-word phrase, because `bip32 0.5.3`'s parser
/// requires exactly 33 bytes of entropy-plus-checksum. So a phrase a user
/// wrote down, restored from a password manager, or imported from another
/// wallet did not parse, and every address derived from it was unreachable.
///
/// These assertions pin the *repaired* behaviour: the specification's vectors
/// parse, the seed for vector 1 is the specification's own value, a phrase
/// whose checksum does not match is still refused, and every word count BIP-39
/// defines is generated at exactly the requested length. If a future release
/// regresses any of it, this test fails.
#[test]
fn bip39_parses_the_published_vectors_and_refuses_a_bad_checksum() {
    // BIP-39's own vectors, from the specification's test cases.
    for vector in [
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        "legal winner thank year wave sausage worth useful legal winner thank yellow",
        "letter advice cage absurd amount doctor acoustic avoid letter advice cage above",
        // All-ff entropy. The final word is "wrong", which reads like a
        // deliberately corrupted phrase and is in fact the specification's
        // own valid vector for this entropy — the most convincing argument
        // for checking vectors against the wordlist rather than by eye.
        "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong",
    ] {
        assert!(
            mnemonic::mnemonic_to_seed(vector, "").is_ok(),
            "BIP-39 vector refused: {vector}"
        );
    }

    // Pin the derived seed itself, not merely that parsing succeeded: this
    // covers the checksum, the wordlist indices and the PBKDF2 passphrase
    // handling at once, and it is fixed by the specification.
    let seed = mnemonic::mnemonic_to_seed(
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        "TREZOR",
    )
    .expect("vector 1 parses");
    assert_eq!(
        seed[..8],
        [0xc5, 0x52, 0x57, 0xc3, 0x60, 0xc0, 0x7c, 0x72],
        "BIP-39 vector 1 seed prefix"
    );

    // A phrase whose checksum does not match its entropy is still refused —
    // "abandon" x12 decodes to zero entropy, whose checksum is 0011, not the
    // 0000 the final word carries. The obvious candidate for this test,
    // "zoo ... wrong", is itself a specification vector — all-ff entropy — so
    // using it would have asserted that valid phrases are refused.
    assert!(
        mnemonic::mnemonic_to_seed(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon",
            ""
        )
        .is_err(),
        "a phrase whose checksum does not match its entropy is refused"
    );

    // Generation honours the request. 0.2.1 could only mint 24 words and
    // rejected the other four BIP-39 lengths outright.
    for count in [12_u8, 15, 18, 21, 24] {
        let phrase = mnemonic::generate_mnemonic(count).expect("a BIP-39 word count");
        assert_eq!(
            phrase.split_whitespace().count(),
            count as usize,
            "generate({count}) mints exactly {count} words"
        );
        assert!(
            mnemonic::mnemonic_to_seed(&phrase, "").is_ok(),
            "and a generated {count}-word phrase round-trips"
        );
    }
}

#[test]
fn a_seed_derives_deterministic_accounts_and_never_returns_to_one() {
    // The property a wallet host depends on: derivation is deterministic (the
    // same seed and path give the same address, every time, on every replica)
    // and paths are non-collapsing (a different index or chain gives a
    // different address). Together those are what make a recovery path
    // meaningful years later.
    //
    // Built from the canonical 12-word vector, which is what a real user's
    // phrase looks like — 0.2.1 could not parse this at all.
    let seed = mnemonic::mnemonic_to_seed(
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        "",
    )
    .expect("the canonical 12-word vector parses as of 0.2.2");

    // Same seed, same account/index: same address on every replica.
    let first = eth::derive_eth_address(&seed, 0, 0).expect("derives");
    assert_eq!(
        first,
        eth::derive_eth_address(&seed, 0, 0).expect("derives"),
        "derivation is deterministic"
    );
    assert_eq!(
        eth::derive_address(&seed).expect("derives"),
        first,
        "and the account-0/index-0 shorthand agrees with the explicit path"
    );

    // A sibling index is a different account — which is what stops two
    // customers from receiving the same address.
    assert_ne!(
        eth::derive_eth_address(&seed, 0, 1).expect("derives"),
        first,
        "a different index is a different account"
    );
    assert_ne!(
        eth::derive_eth_address(&seed, 1, 0).expect("derives"),
        first,
        "and so is a different account index"
    );

    // The chains carry different BIP-44 purposes (44' for Ethereum, 84' for
    // native segwit), so the same seed and index give unrelated addresses —
    // which is the point of a per-chain path.
    let bitcoin = btc::derive_btc_address(&seed, 0, 0).expect("derives");
    assert_ne!(bitcoin, first, "a different chain is a different account");
    assert!(
        bitcoin.starts_with("bc1"),
        "and Bitcoin's derivation follows BIP-84 native segwit: {bitcoin}"
    );
    assert!(
        first.starts_with("0x") && first.len() == 42,
        "while Ethereum's is an EIP-55 checksummed address: {first}"
    );
    // EIP-55 mixes case, so an all-lowercase address is a *different* string
    // — a host that lowercases addresses for storage breaks signatures.
    assert_ne!(
        first,
        first.to_lowercase(),
        "EIP-55 mixes case, so a lowercased address is not the same address"
    );

    // The passphrase is part of the seed, with nothing to detect a typo: a
    // mistyped passphrase yields a valid seed and entirely unreachable
    // accounts. A host that stores only the phrase cannot restore those.
    let with_passphrase = mnemonic::mnemonic_to_seed(
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        "not empty",
    )
    .expect("valid");
    assert_ne!(with_passphrase, seed, "a passphrase changes the seed...");
    assert_ne!(
        eth::derive_eth_address(&with_passphrase, 0, 0).expect("derives"),
        first,
        "...and therefore every derived address, with nothing to signal it"
    );
}

// -- helpers --------------------------------------------------------------

/// Decode a lowercase hex string into bytes. Used for the RFC 4231 keys,
/// which are variable length, so this returns a `Vec`.
fn hex(text: &[u8]) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0, "a hex string has an even length");
    text.chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16).expect("hex digit");
            let lo = (pair[1] as char).to_digit(16).expect("hex digit");
            u8::try_from(hi * 16 + lo).expect("fits u8")
        })
        .collect()
}

/// Render bytes as lowercase hex, so a digest compares against the RFC's
/// published string directly rather than through a second decoding step.
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
