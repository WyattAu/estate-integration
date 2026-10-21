#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `chisel_platform` — the platform side of Chisel Docs, composed.
//!
//! `chisel-assets` (content-addressed document store), `chisel-auth` (session
//! tokens and roles), and `chisel-kernel` (settings and validated ids) are the
//! three crates every Chisel service stands on. They were published and
//! composed only transitively, which meant their own error paths were walked
//! by nobody.
//!
//! The chain is the point: `chisel-auth` issues tokens naming a
//! `chisel_kernel::UserId`, so the two crates must agree on what a user *is*;
//! and `chisel-assets` stores what the editor saves, so its content addressing
//! must be trustworthy on its own.

use chisel_assets::AssetStore;
use chisel_auth::{Capability, Role, SessionService};
use chisel_kernel::UserId;

fn user(name: &str) -> UserId {
    UserId::try_from(name.to_owned()).expect("a plain name is a valid user id")
}

// --- chisel-assets ----------------------------------------------------------

/// The store is content-addressed: identical bytes deduplicate to one
/// document, the address is stable, and removal is real.
#[test]
fn the_asset_store_deduplicates_and_removes() {
    let store = AssetStore::open_in_memory().expect("in-memory store opens");

    let first = store.put(b"same bytes").expect("put");
    let second = store.put(b"same bytes").expect("put again");
    assert_eq!(first.hash(), second.hash(), "identical bytes, different hashes");
    assert_eq!(first.address(), second.address());

    assert!(store.contains(&first));
    let read_back = store.read(&first).expect("read");
    assert_eq!(read_back, b"same bytes");

    store.remove(&first).expect("remove");
    // The crate's own removal test asserts via `is_empty`, which is the
    // documented observable; a second remove of the same address is a no-op.
    assert!(store.is_empty().expect("is_empty"), "the store is empty after removal");
    store.remove(&first).expect("a second remove is a no-op");
}

/// Distinct bytes get distinct addresses: a hash collision here would silently
/// substitute one document for another.
#[test]
fn distinct_bytes_get_distinct_addresses() {
    let store = AssetStore::open_in_memory().expect("opens");
    let a = store.put(b"document A").expect("put A");
    let b = store.put(b"document B").expect("put B");
    assert_ne!(a.hash(), b.hash());
    assert_ne!(a.address(), b.address());
    assert_eq!(store.read(&a).expect("read A"), b"document A");
    assert_eq!(store.read(&b).expect("read B"), b"document B");
}

// --- chisel-kernel ----------------------------------------------------------

/// A validated id refuses empty and whitespace names: the whole point of the
/// newtype is that the rest of the platform cannot hold an invalid user id.
#[test]
fn a_user_id_refuses_empty_and_whitespace() {
    assert!(UserId::try_from(String::new()).is_err(), "empty accepted");
    assert!(UserId::try_from("   ".to_owned()).is_err(), "whitespace accepted");
    let ok = UserId::try_from("alice".to_owned()).expect("a plain name is valid");
    assert_eq!(ok.as_str(), "alice");
}

// --- chisel-auth ------------------------------------------------------------

/// The role hierarchy, asserted from the crate's own `permits` match: every
/// role reads, Editor and Owner edit/redact/sign, and only Owner manages
/// members or deletes. In a PDF editor, redaction is an editing operation --
/// the first draft of this test asserted otherwise and the crate said no.
#[test]
fn the_role_hierarchy_permits_and_denies() {
    // Every role can read everything: Read and ReadAudit are unconditional.
    for role in [Role::Auditor, Role::Reviewer, Role::Editor, Role::Owner] {
        assert!(role.permits(Capability::Read), "{role:?} reads");
        assert!(role.permits(Capability::ReadAudit), "{role:?} reads the audit trail");
    }
    // The edit-grade capabilities go to Editor and Owner.
    for cap in [Capability::Edit, Capability::Redact, Capability::Sign] {
        assert!(Role::Editor.permits(cap), "Editor {cap:?}");
        assert!(Role::Owner.permits(cap), "Owner {cap:?}");
        assert!(!Role::Auditor.permits(cap), "Auditor cannot {cap:?}");
        assert!(!Role::Reviewer.permits(cap), "Reviewer cannot {cap:?}");
    }
    // Management and deletion are Owner-only.
    for cap in [Capability::ManageMembers, Capability::Delete] {
        assert!(Role::Owner.permits(cap), "Owner {cap:?}");
        assert!(!Role::Editor.permits(cap), "Editor cannot {cap:?}");
    }
}

/// A session's full lifecycle: issue names the user and role, validate returns
/// claims that match, and refresh issues a new access token for the same
/// session.
#[test]
fn a_session_issues_validates_and_refreshes() {
    let service =
        SessionService::new("test-signing-secret", "chisel-test", "chisel-doc")
            .expect("a configured service");

    let tokens = service.issue(&user("alice"), Role::Editor).expect("issue");
    assert!(!tokens.access.is_empty());
    assert!(!tokens.refresh.is_empty());

    let claims = service.validate(&tokens.access).expect("validate");
    assert_eq!(claims.role(), Some(Role::Editor));
    assert!(claims.permits(Capability::Edit));
    assert!(
        claims.permits(Capability::Redact),
        "in a PDF editor, redaction is an editing operation: an Editor redacts"
    );
    assert!(!claims.permits(Capability::ManageMembers), "an Editor cannot manage members");

    // Refresh: same session id, a new access token.
    let refreshed = service.refresh(&tokens.refresh).expect("refresh");
    assert_eq!(refreshed.sid, tokens.sid, "the session id survives the refresh");
    let claims_again = service.validate(&refreshed.access).expect("validate refreshed");
    assert_eq!(claims_again.role(), Some(Role::Editor));
    assert!(
        claims_again.permits(Capability::Redact),
        "the refreshed claims keep the role's capabilities"
    );
}

/// An unconfigured service fails at construction, not at first login: the
/// crate's own doc names this as deliberate, so it is asserted.
#[test]
fn an_unconfigured_service_fails_at_construction() {
    assert!(SessionService::new("", "issuer", "audience").is_err(), "empty secret");
    assert!(SessionService::new("secret", "", "audience").is_err(), "empty issuer");
    assert!(SessionService::new("secret", "issuer", "").is_err(), "empty audience");
}

/// A tampered token is refused. Signing exists to make this hold; a suite that
/// only tests the happy path would pass with the signature check commented out.
#[test]
fn a_tampered_token_is_refused() {
    let service =
        SessionService::new("test-signing-secret", "chisel-test", "chisel-doc")
            .expect("configures");
    let tokens = service.issue(&user("bob"), Role::Reviewer).expect("issue");

    let mut tampered = tokens.access.clone();
    let last = tampered.pop().unwrap_or('x');
    tampered.push(if last == 'a' { 'b' } else { 'a' });
    assert!(
        service.validate(&tampered).is_err(),
        "a character-swapped token validated"
    );

    // A token from a different secret is refused too.
    let other = SessionService::new("a-different-secret", "chisel-test", "chisel-doc")
        .expect("configures");
    let foreign = other.issue(&user("bob"), Role::Reviewer).expect("issue");
    assert!(
        service.validate(&foreign.access).is_err(),
        "a token signed by another secret validated"
    );
}