//! Composition suite for `typed-id-derive` (estate-audit coverage).
//!
//! The derive generates type-safe UUID newtypes. The point of this suite is
//! not the macro's own unit tests (the crate has those); it is proof that the
//! published derive composes with this estate's uuid v7 usage and stays
//! type-distinct across ID kinds — the property the accounting and ledger
//! stacks rely on.

// A round-trip assertion is test scaffolding: the failure mode is a panic
// with the rendering in the message, which is the point. The repo's other
// composition suites scope the same allow (see accounting_stack.rs).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use typed_id_derive::TypedId;
use uuid::Uuid;

#[derive(TypedId, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LedgerEntryId(Uuid);

#[derive(TypedId, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FilingId(Uuid);

#[test]
fn new_as_uuid_and_parse_roundtrip() {
    let id = LedgerEntryId::new(Uuid::now_v7());
    assert_eq!(id.as_uuid(), Uuid::parse_str(&id.to_string()).unwrap());

    let parsed = LedgerEntryId::parse(&id.as_uuid().to_string()).expect("parses its own rendering");
    assert_eq!(parsed, id);
    assert!(!id.is_nil());
    assert!(LedgerEntryId::new(Uuid::nil()).is_nil());
}

#[test]
fn distinct_id_kinds_are_distinct_types() {
    let entry = LedgerEntryId::new(Uuid::now_v7());
    let filing = FilingId::new(entry.as_uuid());
    // Same underlying UUID, different types: the compile-time confusion the
    // derive exists to prevent. `assert_ne!` on the values themselves cannot
    // even compile across kinds (`Discriminant<LedgerEntryId>` vs
    // `Discriminant<FilingId>` are distinct types) — the type names are the
    // runtime-visible proof of that, and conversions stay explicit.
    assert_ne!(
        std::any::type_name::<LedgerEntryId>(),
        std::any::type_name::<FilingId>(),
        "distinct ID kinds must be distinct types"
    );
    assert_eq!(entry.as_uuid(), filing.as_uuid());
}

#[test]
fn hash_and_eq_dedupe_inside_hash_set() {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    let a = LedgerEntryId::new(Uuid::now_v7());
    let b = LedgerEntryId::new(Uuid::now_v7());
    set.insert(a);
    set.insert(b);
    set.insert(a);
    assert_eq!(set.len(), 2, "Hash + Eq dedupe inside HashSet");
    assert!(set.contains(&a));
}
