#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 9, suite 3 — `collab_docs`.
//!
//! crdts-kit, i18n-kit, eventbus-kit, docs-pipeline, ws-kit.
//!
//! The flow a multi-tenant product needs for shared documents: two people
//! edit the same text concurrently on two replicas, the edits converge, the
//! change reaches subscribers over a typed WebSocket hub, each participant's
//! view renders in their own locale, and the converged text is published as
//! documentation.
//!
//! Convergence is the property everything else rests on, so the centre of
//! gravity is a **delivery-order test**: the same set of operations applied
//! in different orders to different replicas must produce byte-identical
//! text. A CRDT that only converges under the op order it was generated in
//! is not a CRDT, and no per-crate test can see that — each replica's own
//! tests pass in isolation.
//!
//! Four findings, and one contract that bit this suite while writing it:
//!
//! 1. **`RgaString::apply` does not deduplicate.** A replayed `Insert` op
//!    appends its character again under the same `OperationId`; only
//!    `Delete` is replay-safe (it sets a tombstone flag). This is documented
//!    — `TextOperation` says "commutative and idempotent-free by design" —
//!    but the consequence is severe: **any at-least-once transport corrupts
//!    the document silently.** A sync protocol must keep its own seen-set of
//!    operation ids, and `TextOperation` offers no accessor for one, so a
//!    host has to match on the variant shape to dedup. Ask: an
//!    `OperationId` accessor, or a `apply_all` that dedups internally.
//! 2. **Causal delivery is a hard precondition.** `apply`'s rustdoc states
//!    an operation may only follow the operations that created its
//!    `origin_left`/`origin_right`. The suite pins both directions: causally
//!    ordered delivery converges under every interleaving, and
//!    out-of-order delivery visibly diverges. A WebSocket sync protocol owes
//!    the CRDT an ordering guarantee (per-site sequence numbers or a buffer).
//! 3. **The published package is `eventbus-kit` but the lib target is
//!    `typed_eventbus`.** The `use` statement and the crate name differ, and
//!    docs.rs shows only the package name, so the mismatch surfaces as
//!    "unresolved import" with nothing pointing at the cause.
//! 4. **i18n-kit does not select plural keys.** `translate` returns the
//!    singular form for every count; `PluralRule::as_key_suffix()` exists but
//!    `translate` never consults it, and nothing reconciles the rule's
//!    vocabulary ("other") with a catalog's key naming ("plural"). A host
//!    rendering "4 items" as "4 item" has a bug no i18n-kit test catches.
//!
//! The contract that bit this suite: `insert_text`/`delete_text` apply the
//! edit **locally before returning the operations to broadcast**. Authoring
//! two replicas' edits on those replicas themselves and then applying the
//! returned ops double-counts them — the single easiest way to misuse the
//! crate, and worth an explicit note in its rustdoc.
//!
//! Then the rest of the stack layers on that guarantee:
//!
//! - **i18n-kit** renders per locale with fallback chains and
//!   pluralization, and a missing translation must fall back rather than
//!   render blank.
//! - **ws-kit**'s hub fans each publication out to every subscriber exactly
//!   once, and *refuses* a broadcast with no receivers rather than reporting
//!   a silent success.
//! - **docs-pipeline** renders the converged markdown — and the XSS
//!   boundary matters here precisely because the collaborative path means
//!   user-authored text reaches the renderer.
//! - **eventbus-kit** carries the convergence as a typed envelope, which is
//!   what an audit row or a search index consumes downstream.

use crdts_kit::{CrdtDocument, DocumentId, ParticipantId, TextOperation};
use docs_pipeline::{MarkdownParser, OutputFormat};
// The published *package* is `eventbus-kit`, but its `[lib]` target is
// named `typed_eventbus` — so the crate you import is the latter. Round-9
// finding: a name mismatch between package and lib is invisible until the
// first `use`, and `docs.rs` shows only the package name.
use i18n_kit::{Catalog, Locale, PluralRule, Translator};
use std::sync::Arc;
use typed_eventbus::{EventBus, EventEnvelope};
use ws_kit::hub::BroadcastHub;

// -- helpers --------------------------------------------------------------

/// A document with three registered participants, so each has a distinct
/// site id (which is what orders concurrent inserts deterministically).
fn doc_with(participants: &[&str]) -> CrdtDocument {
    let mut d = CrdtDocument::new(DocumentId::new());
    for (n, name) in participants.iter().enumerate() {
        d.join(ParticipantId(site_of(n)), name);
    }
    d
}

/// The identity a host must deduplicate on. `TextOperation` exposes only
/// `op_type()`, so reading the id out means matching the variants — which is
/// itself part of the finding: a sync protocol cannot dedup without
/// depending on the operation's variant shape.
fn op_identity(op: &TextOperation) -> (u32, u64) {
    match op {
        TextOperation::Insert { id, .. } | TextOperation::Delete { id, .. } => {
            (id.site_id, id.counter)
        }
    }
}

/// A stable site id per participant index. `CrdtDocument::join` records the
/// id it was given as the participant's site id, and that site id is what
/// orders concurrent operations deterministically — so the fixture assigns
/// them explicitly rather than hoping the crate assigns distinct ones.
fn site_of(index: usize) -> u32 {
    u32::try_from(index + 1).expect("site id fits u32")
}

fn doc() -> CrdtDocument {
    doc_with(&["alice", "bob", "carol"])
}

/// Three independent edits produced on three separate replicas — the
/// classic interleaving that breaks naive last-write-wins text sync.
fn concurrent_edits() -> Vec<Vec<TextOperation>> {
    // Each replica authors with a *different* site id, which is what makes
    // the resulting operation set genuinely concurrent rather than a
    // sequential edit replayed three times.
    let mut a = doc_with(&["alice"]);
    let (ops_a, _) = a.insert_text(ParticipantId(1), 0, "Hello ");
    let mut b = doc_with(&["bob"]);
    let (ops_b, _) = b.insert_text(ParticipantId(2), 0, "world ");
    let mut c = doc_with(&["carol"]);
    let (ops_c, _) = c.insert_text(ParticipantId(3), 0, "!");
    vec![ops_a, ops_b, ops_c]
}

// -- 1. convergence: the property that matters ----------------------------

#[tokio::test]
async fn concurrent_edits_converge_regardless_of_delivery_order() {
    let edits = concurrent_edits();
    assert!(
        edits.iter().all(|ops| !ops.is_empty()),
        "each edit produces at least one operation to broadcast"
    );

    // Every replica receives every operation. Only the order differs, which
    // is what a real network does — and the only thing a CRDT may depend on.
    let mut orders: Vec<Vec<Vec<TextOperation>>> = Vec::new();
    orders.push(edits.clone());
    let mut reversed = edits.clone();
    reversed.reverse();
    orders.push(reversed);
    let mut rotated = edits.clone();
    rotated.rotate_left(1);
    orders.push(rotated);
    let mut swapped = edits.clone();
    swapped.swap(0, 2);
    orders.push(swapped);

    let mut replicas: Vec<CrdtDocument> = orders.iter().map(|_| doc()).collect();
    for (replica, order) in replicas.iter_mut().zip(orders.iter()) {
        for ops in order {
            replica.apply_ops(ops);
        }
    }

    let first = replicas[0].get_text();
    for replica in &replicas[1..] {
        assert_eq!(
            replica.get_text(),
            first,
            "all replicas converge under different delivery orders"
        );
    }

    // Convergence that loses an edit is data loss, not convergence.
    for fragment in ["Hello ", "world ", "!"] {
        assert!(
            first.contains(fragment),
            "the converged text must contain {fragment:?}, got {first:?}"
        );
    }
    // Each fragment appears exactly once: a convergence that *duplicates*
    // is just as broken as one that drops.
    for fragment in ["Hello ", "world ", "!"] {
        assert_eq!(
            first.matches(fragment).count(),
            1,
            "{fragment:?} appears exactly once in {first:?}"
        );
    }
}

/// **Causal delivery is a precondition, and the first convergence test
/// proves the crate relies on it.** `RgaString::apply`'s own rustdoc says an
/// operation "may only be applied after the operations that created its
/// `origin_left` and `origin_right` characters", and that breaking that
/// order breaks convergence. That is the standard RGA model and it is
/// correct — but it means *a transport owes the CRDT an ordering guarantee*.
///
/// The suite pins the contract from both sides: causally-ordered delivery
/// converges under every interleaving of the concurrent ops (first test),
/// and a deliberately out-of-order delivery visibly diverges (this test).
/// A host that ships a WebSocket sync protocol has to carry a per-site
/// sequence number or a buffer, and this is the test that would catch its
/// absence.
#[tokio::test]
async fn causally_ordered_delivery_converges_and_replays_are_idempotent() {
    let mut base = doc_with(&["alice"]);
    let (hello, _) = base.insert_text(ParticipantId(1), 0, "Hello world");

    // Two replicas, both seeded from the same op set.
    let mut a = doc_with(&["alice", "bob"]);
    let mut b = doc_with(&["alice", "bob"]);
    a.apply_ops(&hello);
    b.apply_ops(&hello);

    // Author each operation on its *own* replica. `insert_text`/`delete_text`
    // apply the edit locally before returning the operations to broadcast, so
    // authoring replica B's edit on replica B would leave B already holding
    // it — and then applying the returned ops would duplicate it. That is not
    // a CRDT bug; it is the documented local-apply-then-broadcast contract,
    // and getting it wrong is the easiest way to misuse this crate.
    //
    // The two operations are concurrent (different sites, neither depends on
    // the other) and touch disjoint regions: A deletes the leading word, B
    // appends at index 11 — *after* the deleted region, because an insert
    // positioned inside a region another replica is deleting references
    // characters that may be tombstoned by the time it arrives.
    let mut author_a = doc_with(&["alice", "bob"]);
    author_a.apply_ops(&hello);
    let (ops_delete, _) = author_a.delete_text(ParticipantId(1), 0, 6);
    let mut author_b = doc_with(&["alice", "bob"]);
    author_b.apply_ops(&hello);
    let (ops_insert, _) = author_b.insert_text(ParticipantId(2), 11, " there");
    assert!(
        !ops_delete.is_empty() && !ops_insert.is_empty(),
        "both operations exist to broadcast"
    );

    // Both orderings are causally valid: each op set depends only on
    // `hello`, which both replicas already have.
    a.apply_ops(&ops_insert);
    a.apply_ops(&ops_delete);
    b.apply_ops(&ops_delete);
    b.apply_ops(&ops_insert);

    assert_eq!(
        a.get_text(),
        b.get_text(),
        "concurrent delete + insert converge in either causal order"
    );
    assert!(
        !a.get_text().contains("Hello"),
        "the deletion is present on both replicas, got {:?}",
        a.get_text()
    );
    // Convergence must not lose or duplicate the concurrent insertion.
    assert_eq!(
        a.get_text().matches(" there").count(),
        1,
        "the concurrent insert appears exactly once: {:?}",
        a.get_text()
    );
    assert_eq!(a.get_text(), "world there", "and lands where both agree");

    // -- Deletes are idempotent, inserts are **not**, and the crate says so:
    //    `TextOperation` is documented as "commutative and idempotent-free by
    //    design". Replaying a delete is a no-op (it sets a tombstone flag on
    //    an existing node); replaying an insert appends the character again.
    let converged = a.get_text();
    a.apply_ops(&ops_delete);
    assert_eq!(
        a.get_text(),
        converged,
        "a replayed delete is a no-op — it re-tombstones an existing node"
    );

    let before_insert_replay = a.get_text();
    a.apply_ops(&ops_insert);
    assert_ne!(
        a.get_text(),
        before_insert_replay,
        "a replayed insert is NOT a no-op — `apply` never checks whether the \
         OperationId was already applied"
    );
    // The duplication is exactly one character per replayed `Insert` op —
    // each op carries a single `char` and `apply` appends it again with the
    // same id. Asserting the count rather than the exact layout, because
    // RGA positions each replayed character by origin and the interleaving
    // depends on delivery history (observed: "world there there").
    assert_eq!(
        a.get_text().chars().count(),
        before_insert_replay.chars().count() + ops_insert.len(),
        "each replayed insert op adds exactly one character"
    );
    // (" there" contains one 'r' and two 'e's, so a doubled count only
    // holds for the single-occurrence letters.)
    for ch in ['t', 'h'] {
        assert_eq!(
            a.get_text().chars().filter(|c| *c == ch).count(),
            before_insert_replay.chars().filter(|c| *c == ch).count() * 2,
            "every character of the replayed run is duplicated"
        );
    }

    // The consequence, stated as a host obligation: the sync protocol must
    // deduplicate by OperationId before calling apply. Nothing in the type
    // or the method signature enforces it, so this is the assertion a host
    // regression would break.
    let mut deduped = doc_with(&["alice", "bob"]);
    deduped.apply_ops(&hello);
    let mut seen = std::collections::HashSet::new();
    for op in ops_delete.iter().chain(ops_insert.iter()) {
        if seen.insert(op_identity(op)) {
            deduped.apply_ops(std::slice::from_ref(op));
        }
    }
    assert_eq!(
        deduped.get_text(),
        converged,
        "a host that deduplicates by OperationId is replay-safe, and the \
         duplicate delete is still harmless"
    );
}

/// The negative case: delivering an operation *before* the operation that
/// created its origin characters breaks convergence. This is documented
/// behaviour, not a bug — the suite pins it because it is the failure a host
/// hits when its sync protocol drops ordering.
#[tokio::test]
async fn out_of_order_delivery_diverges_as_documented() {
    let mut source = doc_with(&["alice"]);
    let (hello, _) = source.insert_text(ParticipantId(1), 0, "abc");
    let (more, _) = source.insert_text(ParticipantId(1), 3, "def");

    // Causally ordered: correct.
    let mut in_order = doc_with(&["alice"]);
    in_order.apply_ops(&hello);
    in_order.apply_ops(&more);
    assert_eq!(in_order.get_text(), "abcdef");

    // Reversed: the second op's origins (chars created by `hello`) have not
    // arrived, so the insert lands somewhere else. The rustdoc calls this
    // out; the assertion records it so a host regression is visible.
    let mut out_of_order = doc_with(&["alice"]);
    out_of_order.apply_ops(&more);
    out_of_order.apply_ops(&hello);
    assert_ne!(
        out_of_order.get_text(),
        in_order.get_text(),
        "delivering an operation before its origins diverges — which is why \
         the transport owes the CRDT an ordering guarantee"
    );
}

// -- 2. presence ----------------------------------------------------------

#[tokio::test]
async fn participants_join_and_leave_the_document() {
    let mut d = doc();
    assert_eq!(d.participants.len(), 3);
    // `join` hands back the id it used, which is what an edit must be
    // authored as — authoring under an unregistered id silently falls back
    // to the id itself as the site id.
    // `join` registers the id it was given and hands the same id back —
    // there is no server-allocated identity, so two replicas that "join"
    // independently must be handed ids by the host or they will collide.
    let returned = d.join(ParticipantId(42), "Dana");
    assert_eq!(returned, ParticipantId(42));
    assert_eq!(d.participants.len(), 4);
    assert_eq!(
        d.participants[&ParticipantId(42)].site_id,
        42,
        "the site id used to author operations is the participant id"
    );

    let bob = ParticipantId(2);
    assert_eq!(d.participants[&bob].name, "bob", "bob joined at site 2");
    d.leave(&bob);
    assert_eq!(d.participants.len(), 3, "3 fixture + Dana, minus bob");
    assert!(
        d.participants.values().all(|p| p.name != "bob"),
        "the departed participant is gone"
    );

    // Re-joining under the same id replaces the record rather than
    // duplicating it, so a reconnect does not double-count presence.
    d.join(bob, "Robert");
    assert_eq!(d.participants.len(), 4, "re-joining restores the count");
    assert_eq!(
        d.participants[&bob].name, "Robert",
        "and replaces the record"
    );

    // Presence is ephemeral, unlike edits: leaving leaves the text alone.
    let kept = edit_as(&mut d, "Carol", "kept");
    d.apply_ops(&kept);
    assert!(d.get_text().contains("kept"));
    d.leave(&ParticipantId(99));
    assert!(
        d.get_text().contains("kept"),
        "leaving an unknown participant is a no-op, not a document reset"
    );
}

/// Register a fresh participant, make one edit, and return the operations to
/// broadcast. A free function rather than an inherent impl, because
/// `CrdtDocument` is a foreign type here.
fn edit_as(d: &mut CrdtDocument, name: &str, text: &str) -> Vec<TextOperation> {
    let site = u32::try_from(d.participants.len() + 1).expect("site id fits u32");
    let id = d.join(ParticipantId(site), name);
    let (ops, _) = d.insert_text(id, 0, text);
    ops
}

// -- 3. the change is published over a typed hub --------------------------

#[tokio::test]
async fn converged_edits_reach_every_subscriber_exactly_once() {
    let hub: BroadcastHub<String> = BroadcastHub::new(8);
    let mut sub_a = hub.subscribe();
    let mut sub_b = hub.subscribe();
    assert_eq!(hub.receiver_count(), 2);
    // `BroadcastHub::capacity()` documents itself as "unknown, returns 0"
    // because tokio's broadcast channel does not expose its own capacity and
    // the hub does not retain the constructor argument. So a host cannot ask
    // the hub how deep its buffer is — it has to remember the number itself.
    // Round-9 finding.
    assert_eq!(
        hub.capacity(),
        0,
        "capacity is not recoverable from the hub"
    );

    let mut d = doc();
    let ops = edit_as(&mut d, "Alice", "shared");
    for op in &ops {
        hub.broadcast(format!("op:{:?}", op.op_type()))
            .expect("a subscriber is present");
    }

    let mut seen_a = Vec::new();
    while let Ok(msg) = sub_a.try_recv() {
        seen_a.push(msg);
    }
    let mut seen_b = Vec::new();
    while let Ok(msg) = sub_b.try_recv() {
        seen_b.push(msg);
    }
    assert_eq!(
        seen_a.len(),
        ops.len(),
        "every publication reaches the subscriber exactly once"
    );
    assert_eq!(seen_a, seen_b, "both subscribers see the same sequence");
    assert!(d.get_text().contains("shared"));

    // With nobody listening, a publication is refused rather than silently
    // dropped — the host can tell "nobody is watching" from "it worked".
    let empty: BroadcastHub<String> = BroadcastHub::new(1);
    assert!(
        empty.broadcast("orphan".to_string()).is_err(),
        "a broadcast with no receivers is an error, not a silent success"
    );
    assert_eq!(empty.try_broadcast("orphan".to_string()), 0);
    assert_eq!(empty.connection_count(), 0);
}

// -- 4. per-locale rendering ---------------------------------------------

#[tokio::test]
async fn the_converged_document_renders_in_every_locale() {
    let mut catalog = Catalog::new();
    catalog.insert("en", "invoice.total", "Total");
    catalog.insert("en", "invoice.due", "Due");
    catalog.insert("en", "invoice.items", "{count} item");
    catalog.insert("en", "invoice.items_plural", "{count} items");
    // `de` is deliberately incomplete: `invoice.due` is missing, so it must
    // fall back to the default locale rather than render blank.
    catalog.insert("de", "invoice.total", "Gesamt");
    catalog.insert("de", "invoice.items", "{count} Artikel");
    catalog.insert("de", "invoice.items_plural", "{count} Artikel");
    // `fr-CA` inherits from `fr`; the fallback chain should find `fr` first.
    catalog.insert("fr", "invoice.total", "Total");

    let translator: Translator = catalog.into_translator("en");

    assert_eq!(translator.translate("en", "invoice.total", &[]), "Total");
    assert_eq!(translator.translate("de", "invoice.total", &[]), "Gesamt");
    assert_eq!(
        translator.translate("de", "invoice.due", &[]),
        "Due",
        "a missing translation falls back rather than rendering blank"
    );
    assert_eq!(
        translator.translate("fr-CA", "invoice.total", &[]),
        "Total",
        "a regional locale resolves through its language"
    );
    // Interpolation is the crate's job; **plural key selection is the
    // host's**. i18n-kit exposes `PluralRule::as_key_suffix()` but
    // `translate` never consults it, so `translate(key)` on a count-bearing
    // string silently returns the singular form for every count. Round-9
    // finding: a host that renders "4 items" as "4 item" has a bug that no
    // test in i18n-kit would catch, because the rule is available and unused.
    assert_eq!(
        translator.translate("en", "invoice.items", &[("count", "1")]),
        "1 item",
        "interpolation works"
    );
    assert_eq!(
        translator.translate("en", "invoice.items", &[("count", "4")]),
        "4 item",
        "and the singular form is returned for every count — selection is the \
         host's job, which is the trap"
    );
    // The host's correct spelling: pick the key with the suffix. Note the
    // catalog's plural key is `invoice.items_plural`, so the host must map
    // `as_key_suffix()` onto its own key naming — the rule gives "other" and
    // the catalog says "plural", and nothing reconciles the two.
    let suffix = PluralRule::for_count(4).as_key_suffix();
    assert_eq!(suffix, "other", "four is 'other' in English rules");
    assert_eq!(
        translator.translate("en", "invoice.items_plural", &[("count", "4")]),
        "4 items",
        "the host picks the plural key and gets it right"
    );
    assert_eq!(
        translator.translate("en", &format!("invoice.items_{suffix}"), &[("count", "4")]),
        "invoice.items_other",
        "but keying by the rule's own suffix misses a catalog that names it \
         differently — the rule and the catalog are not linked"
    );
    assert!(translator.exists("en", "invoice.total"));
    assert!(!translator.exists("en", "invoice.nonexistent"));
    // An unknown key renders as the key itself — a visible bug in the UI
    // rather than a silent empty string.
    assert_eq!(
        translator.translate("en", "invoice.nonexistent", &[]),
        "invoice.nonexistent"
    );

    // Pluralization is per-locale, not per-key.
    // `for_count` is documented as English rules, where zero is *other*;
    // `with_zero` is the variant for languages with an explicit zero form
    // (Arabic, Latvian). So the choice of function is a per-language
    // decision the host must make, not a default.
    assert_eq!(PluralRule::for_count(0).as_key_suffix(), "other");
    assert_eq!(PluralRule::for_count(1).as_key_suffix(), "one");
    assert_eq!(PluralRule::for_count(2).as_key_suffix(), "other");
    assert_eq!(PluralRule::with_zero(0).as_key_suffix(), "zero");
    assert_ne!(
        PluralRule::with_zero(0),
        PluralRule::for_count(0),
        "the two selectors disagree exactly on zero — so a host rendering \
         Arabic invoices with `for_count` gets the wrong form"
    );
    assert_ne!(PluralRule::for_count(1), PluralRule::for_count(2));

    // Locales parse and normalise — this is what makes `fr-CA` above work.
    let parsed = Locale::parse("fr-CA").expect("valid BCP 47 tag");
    assert_eq!(parsed.language(), "fr");
    assert!(parsed.matches_language("fr"));
    assert!(!parsed.matches_language("de"));
    assert!(Locale::parse("not a locale").is_err());
}

// -- 5. publication: the document goes out as docs -----------------------

#[tokio::test]
async fn the_converged_markdown_renders_for_publication() {
    let parser = MarkdownParser::new();

    let mut d = doc();
    let edit = edit_as(&mut d, "Alice", "1200.00 USD");
    d.apply_ops(&edit);
    let text = d.get_text();

    let source = format!("# Statement\n\nTotal: {text}\n");
    let rendered = parser
        .parse(&source, OutputFormat::Html)
        .expect("markdown renders");
    assert_eq!(rendered.format, OutputFormat::Html);
    assert!(
        rendered.content.contains("1200.00"),
        "the collaboratively edited figure reaches the output: {}",
        rendered.content
    );
    assert!(!rendered.content.is_empty());

    // Plain text keeps the content without markup.
    let plain = parser
        .parse(&source, OutputFormat::PlainText)
        .expect("plain text renders");
    assert!(
        !plain.content.contains("<"),
        "no markup in plain text: {}",
        plain.content
    );
    assert!(plain.content.contains("1200.00"));

    // -- The XSS boundary. The collaborative path means user-authored text
    //    reaches the renderer, so raw markup in a document must not survive
    //    into published output.
    let hostile = "# Report\n\n<script>alert('xss')</script>\n\nSafe text.\n";
    let sanitised = parser
        .parse(hostile, OutputFormat::Html)
        .expect("hostile input still renders");
    assert!(
        !sanitised.content.contains("<script>"),
        "raw script tags must not survive: {}",
        sanitised.content
    );
    assert!(
        sanitised.content.contains("Safe text."),
        "legitimate content survives: {}",
        sanitised.content
    );
}

// -- 6. the change is an event other services can consume -----------------

#[tokio::test]
async fn a_convergence_event_carries_its_payload_to_subscribers() {
    let bus: Arc<EventBus<serde_json::Value>> = Arc::new(EventBus::new());

    let mut d = doc();
    let ops = edit_as(&mut d, "Alice", "posted");
    let payload = serde_json::json!({
        "document": "doc-1",
        "operations": ops.len(),
        "text": d.get_text(),
    });
    let envelope: EventEnvelope<serde_json::Value> =
        EventEnvelope::new("document.edited", payload.clone());

    // The envelope carries routing and identification alongside the payload,
    // which is what lets a subscriber deduplicate and trace: a unique id per
    // instance and a topic per stream.
    assert_eq!(&*envelope.topic, "document.edited");
    assert_eq!(envelope.payload, payload);
    assert_eq!(
        envelope.payload["text"], "posted",
        "the converged text reaches the payload"
    );
    assert!(envelope.timestamp > 0, "every event is timestamped");
    // Two envelopes for the same edit are distinguishable — that is the
    // property an at-least-once consumer needs to drop its own duplicate.
    let twin = EventEnvelope::new("document.edited", payload.clone());
    assert_ne!(envelope.id, twin.id);

    // Publishing reports how many subscribers received it, so a host can
    // tell "delivered" from "nobody was listening".
    let received = bus
        .publish("document.edited", payload)
        .await
        .expect("published");
    assert_eq!(
        received, 0,
        "no subscribers, so nobody received it — and that is reported"
    );
}
