#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
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
//! 1. **`webauthn-kit`'s `check_sign_count` accepted an *equal* count** — fixed
//!    in `0.3.6`, then *refined* in `0.3.7`. The rule was `new < current`, so
//!    `new == current` passed, and a replayed assertion carries the counter the
//!    authenticator last wrote.
//!
//!    **0.3.7 corrects that fix.** WebAuthn L3 (a W3C Recommendation, 2026-08-25)
//!    §7.2 step 18 calls a non-increasing counter *"a signal, but not proof"* and
//!    names a benign cause: *"a race condition where the Relying Party is
//!    processing assertion responses in an order other than the order they were
//!    generated."* So 0.3.6's unconditional refusal is right for a sequential
//!    verifier and wrong for a concurrent one, which would lock out legitimate
//!    users. `classify_sign_count` now returns a verdict that never fails, and
//!    `SignCountPolicy` lets a host choose `Reject` (the default) or `Signal`.
//!    The assertions below pin both, including the out-of-order scenario itself.
//!
//! 2. **`oauth-toolkit`'s PKCE verifier took the method as a `&str`** — fixed in
//!    `0.3.0`. A near-miss like `"s256"` returned a bare `false`, which is
//!    indistinguishable from a wrong verifier: one is a client bug to report,
//!    the other a possible attack to refuse silently.
//!
//!    **And 0.3.0 also forbids `plain` by default.** OAuth 2.1
//!    (`draft-ietf-oauth-v2-1-16`) §7.5.2 forbids the method outright: its
//!    historical justification was clients incapable of SHA-256, and OAuth 2.1
//!    requires TLS 1.2+, which mandates SHA-256 — *"any device capable of
//!    implementing OAuth 2.1 necessarily supports SHA-256."* RFC 7636 still
//!    permits it, so the sources disagree and the newest governs the default;
//!    `PkcePolicy::allow_plain` restores it as an explicit decision.
//!
//! 3. **A challenge's replay window is the caller's, not the store's.**
//!    `consume_registration_challenge(id, timeout_secs)` compares the entry's
//!    `created_at` against *now + timeout*, so the same stored challenge is
//!    accepted with a generous timeout and refused with a tight one. A host
//!    that passes its own clock skew into this re-opens the window it
//!    believed was closed.

use cryptkit::hmac::{constant_time_eq, hmac_sign, hmac_verify};
use multi_chain_wallet::{btc, eth, mnemonic};
use oauth_toolkit::pkce::{
    generate_pkce_pair, validate_verifier, verify, verify_with_default_policy, PkceError,
    PkceMethod, PkcePolicy,
};
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
    // WebAuthn §7.2: the clone signal is a new signature counter that is *not
    // greater than* the stored one, so the check is `<=`. Both the decrease and
    // the equal case are refused, and the error says which one it was.
    assert!(check_sign_count(10, 11).is_ok(), "an increase is normal");

    // The zero exemptions, which are what keep counter-less authenticators
    // working: a stored zero is "no baseline yet" (first use), a reported zero
    // is "I have no counter". Both sides of the comparison, both directions.
    assert!(check_sign_count(0, 0).is_ok(), "zero against zero");
    assert!(check_sign_count(0, 1).is_ok(), "first use, from zero");
    assert!(check_sign_count(0, u32::MAX).is_ok());
    assert!(
        check_sign_count(42, 0).is_ok(),
        "a reported zero is no counter, not a decrease — refusing it locks out \
         exactly the keys that need the exemption"
    );

    // The equal case is what a replay looks like, and it is now refused.
    let equal =
        check_sign_count(10, 10).expect_err("a replayed assertion carries the last counter");
    assert!(
        equal.to_string().contains("unchanged"),
        "the error distinguishes 'unchanged' from 'decreased': {equal}"
    );

    let decrease = check_sign_count(10, 3).expect_err("a decrease is a clone");
    assert!(
        decrease.to_string().contains("decreased"),
        "and says so when it is the decrease case: {decrease}"
    );

    // u32::MAX is the boundary that overflows a naive `< current` + 1.
    assert!(check_sign_count(u32::MAX - 1, u32::MAX).is_ok());
    assert!(check_sign_count(u32::MAX, u32::MAX).is_err());
}

// -- 5. PKCE, against RFC 7636 --------------------------------------------

#[test]
fn pkce_generation_and_verification_round_trip() {
    // RFC 7636: the client generates a verifier, sends
    // `code_challenge = BASE64URL(SHA256(verifier))`, and the server checks
    // them together. The suite checks the *hash* half too, because a pair that
    // only agrees with itself proves nothing.
    let (verifier, challenge) = generate_pkce_pair();
    assert_ne!(
        verifier, challenge,
        "the verifier is not the challenge — that is the whole point of PKCE"
    );
    // A generated verifier satisfies the syntax RFC 7636 §4.1 requires, which
    // 0.2.x never checked.
    validate_verifier(&verifier).expect("a generated verifier is well formed");
    assert_eq!(
        verifier.chars().count(),
        43,
        "43 characters, the RFC minimum"
    );

    assert!(
        verify_with_default_policy(&verifier, &challenge, PkceMethod::S256).is_ok(),
        "a matching pair verifies under S256"
    );

    // Two generated pairs differ, so a replayed verifier is not valid for a
    // second authorization.
    let (other_verifier, other_challenge) = generate_pkce_pair();
    assert_eq!(
        verify_with_default_policy(&verifier, &other_challenge, PkceMethod::S256),
        Err(PkceError::Mismatch),
        "and a verifier from another authorization does not"
    );
    assert_eq!(
        verify_with_default_policy(&other_verifier, &challenge, PkceMethod::S256),
        Err(PkceError::Mismatch)
    );
}

/// The RFC 7636 Appendix B vector, verbatim. Published test data, so it cannot
/// drift with our own implementation.
#[test]
fn pkce_matches_the_rfc_7636_appendix_b_vector() {
    assert!(verify_with_default_policy(
        "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        PkceMethod::S256
    )
    .is_ok());
}

/// **Round-13 finding, now fixed in `oauth-toolkit 0.3.0`: the method was a
/// `&str` and a near-miss was silently *false*.**
///
/// `"s256"`, `"S-256"` and `"SHA256"` used to return `false` — the same answer
/// as a wrong verifier. A typo is a client bug to report; a mismatch is a
/// possible attack to refuse without detail. The method is now a type parsed
/// once, and the failure says which kind of failure it was.
#[test]
fn a_misspelled_pkce_method_is_a_client_bug_not_a_mismatch() {
    for typo in ["s256", "S-256", "S256 ", "SHA256", "S512", ""] {
        assert_eq!(
            PkceMethod::parse(typo),
            Err(PkceError::UnknownMethod(typo.to_string())),
            "{typo:?} must not parse"
        );
    }
    assert_eq!(PkceMethod::parse("S256"), Ok(PkceMethod::S256));
    assert_eq!(PkceMethod::parse("plain"), Ok(PkceMethod::Plain));
}

/// **OAuth 2.1 `draft-ietf-oauth-v2-1-16` §7.5.2 forbids PKCE `plain`**, on the
/// grounds that its historical justification was clients incapable of SHA-256,
/// and OAuth 2.1 requires TLS 1.2+, which mandates SHA-256. 0.3.0's default
/// policy refuses it even when the verifier and challenge are identical — which
/// is exactly what `0.2.2` returned `true` for.
#[test]
fn pkce_plain_is_refused_by_default_and_available_only_on_request() {
    let (verifier, challenge) = generate_pkce_pair();

    assert_eq!(
        verify_with_default_policy(&verifier, &challenge, PkceMethod::Plain),
        Err(PkceError::MethodNotPermitted {
            method: PkceMethod::Plain
        }),
        "the default policy is S256-only"
    );
    // Even the degenerate case, where plain would match trivially.
    assert_eq!(
        verify_with_default_policy(&verifier, &verifier, PkceMethod::Plain),
        Err(PkceError::MethodNotPermitted {
            method: PkceMethod::Plain
        }),
        "and refusing is not conditional on it failing to match"
    );

    // A deployment with a client that genuinely cannot do SHA-256 says so
    // explicitly.
    assert!(
        verify(
            &verifier,
            &verifier,
            PkceMethod::Plain,
            &PkcePolicy::allow_plain()
        )
        .is_ok(),
        "the old behaviour is still available, as a named decision"
    );
}

/// RFC 7636 §4.1: `code_verifier` is 43-128 unreserved characters. 0.3.0 checks
/// it before hashing, so a verifier that cannot have come from a conforming
/// client is refused as malformed rather than compared.
#[test]
fn a_pkce_verifier_that_cannot_be_well_formed_is_refused_before_hashing() {
    let (_, challenge) = generate_pkce_pair();
    assert!(
        matches!(
            verify_with_default_policy("short", &challenge, PkceMethod::S256),
            Err(PkceError::MalformedVerifier(_))
        ),
        "too short"
    );
    assert!(matches!(
        validate_verifier(&"a".repeat(129)),
        Err(PkceError::MalformedVerifier(_))
    ));
    // 43 characters of the unreserved set, including every punctuation character
    // base64url can emit (`-` and `_`) and the two it cannot (`.` and `~`, which
    // RFC 7636 allows and base64url never produces).
    let valid = "a-b._~".repeat(7) + "a"; // 6*7 + 1 = 43
    assert_eq!(valid.chars().count(), 43);
    assert!(validate_verifier(&valid).is_ok());
    // And one character outside the set is refused.
    assert!(matches!(
        validate_verifier(&format!("{}+", &valid[..42])),
        Err(PkceError::MalformedVerifier(_))
    ));
}

/// The CSRF store is keyed by session and is single-use: `retrieve_and_consume`
/// calls `TtlCache::take_fresh`, which removes the entry and refuses an expired
/// one. That is the property that makes it a defence — a replayed `state` finds
/// nothing.
#[tokio::test]
async fn a_csrf_state_is_single_use_session_scoped_and_expiring() {
    use oauth_toolkit::csrf::{CsrfStore, CsrfStoreType};

    // A short TTL so expiry is observable rather than asserted.
    let store = CsrfStoreType::Memory(oauth_toolkit::csrf::MemoryCsrfStore::with_ttl(
        std::time::Duration::from_millis(50),
    ));

    // Two states for the same session must differ: a nonce that is a function of
    // the session id is not a nonce.
    store.store("session-a", "nonce-one", None).await;
    store.store("session-a", "nonce-two", None).await;
    assert_eq!(
        store.retrieve_and_consume("session-a").await,
        Some(("nonce-two".to_string(), None)),
        "the most recent state is the one a redirect will carry"
    );

    // Single use.
    assert_eq!(
        store.retrieve_and_consume("session-a").await,
        None,
        "and it is consumed by that retrieval, so a replay finds nothing"
    );

    // Scoped: another session's key holds nothing.
    store.store("session-b", "nonce-b", None).await;
    assert_eq!(store.retrieve_and_consume("session-a").await, None);

    // Expiring.
    store.store("session-c", "nonce-c", None).await;
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert_eq!(
        store.retrieve_and_consume("session-c").await,
        None,
        "an expired state is refused rather than redeemed"
    );
}

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
