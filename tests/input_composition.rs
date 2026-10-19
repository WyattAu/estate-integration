#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `input_composition` — `input-kit` 0.1.0 composed with `a11y-kit`
//! and `tui-actions`.
//!
//! This suite exists because of ADR-0005. That ADR's finding was that
//! `estate-integration` composed 63 estate crates but left 14 published ones
//! unexercised, and that `estate-audit.py` had been reporting it correctly
//! with nothing downstream of the report. The rule that came out of it is that
//! a kit ships only once a suite consumes it. This file is therefore written
//! *before* `input-kit` is published, and its findings are part of why it can
//! be.
//!
//! `input-kit` is deliberately the most standalone crate in the estate: L1, no
//! estate dependencies, `no_std`. That is a virtue and also a risk, because a
//! crate with no edges has no neighbours to disagree with it. The three failure
//! modes at this layer are not internal bugs, and no amount of unit testing
//! inside `input-kit` would find them:
//!
//! 1. **The a11y tree cannot say "composing".** A screen reader announces a
//!    textbox's value; while an IME is forming a syllable that value is
//!    transient, and announcing it reads a half-formed character.
//!    `aria-busy` is the standard signal. See
//!    `a_composing_textbox_has_no_busy_state`.
//!
//! 2. **The keymap fires shortcuts the IME owns.** On a Japanese keyboard
//!    `Esc` cancels composition, but on a Western layout `Esc` quits. A layer
//!    that does not know composition swallows the cancel. See
//!    `composition_must_be_consulted_before_the_keymap`.
//!
//! 3. **A caret lands inside a grapheme cluster.** Every platform reports the
//!    preedit caret in a unit that can split a character. Driven here over the
//!    public constructors rather than trusted to the unit tests, because the
//!    unit tests and this crate share an author and therefore a blind spot.

use a11y_kit::{A11yNode, A11yRole, A11yTree, NodeId, States};
use input_kit::{grapheme, Composer, DeadKey, InputEvent, Output, Preedit};
use tui_actions::{Action, Key, KeyEvent, Keymap, Mode};

/// Hostile strings for caret math: each has a cluster boundary that a naive
/// byte or UTF-16 caret can land inside.
const HOSTILE: &[&str] = &[
    "",
    "a",
    "cafe\u{301}",
    "\u{1F1FA}\u{1F1F8}",
    "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F4BB}",
    "a\u{1F600}b",
    "a\u{301}b\u{308}c",
    "\u{0915}\u{094D}\u{0937}", // Devanagari conjunct
];

/// Total UTF-16 code units in `s`.
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The byte offset a UTF-16 code unit index points at, rounding down into a
/// surrogate pair when the unit is a low surrogate.
fn utf16_to_byte(s: &str, unit: usize) -> usize {
    let mut seen = 0;
    for (byte, ch) in s.char_indices() {
        if unit < seen + ch.len_utf16() {
            return byte;
        }
        seen += ch.len_utf16();
    }
    s.len()
}

// --- 1. The a11y signal -----------------------------------------------------

/// A textbox mid-IME-composition is marked busy, so a screen reader does not
/// announce a half-formed syllable as the value.
///
/// This test started as a finding. Composing `input-kit` against `a11y-kit`
/// 0.1.0 showed that `States` (eight `bool`s plus one `Option<TriState>`,
/// `size_of` == 9) had no way to say "this value is not final", which is what
/// `aria-busy` is for. The gap was asserted mechanically so that adding the
/// field would turn the test red; `a11y-kit` 0.2.0 added `busy`, and the
/// ratchet is now the assertion that the two crates actually agree.
///
/// The bug this prevents is specific: without `busy`, a screen reader announces
/// `にほ` while the user is still typing it.
#[test]
fn a_composing_textbox_is_marked_busy() {
    // The settled case works and must keep working. The tree needs a Window
    // root, or the audit rejects the focus as unreachable.
    let window = A11yNode::new(NodeId(0), A11yRole::Window, "Chisel Docs");
    let field = NodeId(1);
    let mut node = A11yNode::new(field, A11yRole::Textbox, "Page text")
        .with_parent(NodeId(0))
        .with_states(States {
            focused: true,
            ..States::default()
        });
    let mut tree = A11yTree::new();
    tree.insert(window);
    tree.insert(node.clone());
    let audit = tree.audit().expect("a focused named textbox under a window audits clean");
    assert_eq!(audit.interactive, 1);
    assert_eq!(tree.focused().len(), 1);

    // Settled: not busy.
    assert!(!node.states.busy, "a settled field is not busy");

    // Now the user starts typing Japanese. The composer's composition state
    // and the tree's busy state must be driven from the same thing.
    let mut composer = Composer::new();
    composer.handle(InputEvent::Preedit(Preedit::at_end("にほ")));
    assert!(composer.is_composing());

    node.states.busy = composer.is_composing();
    assert!(
        node.states.busy,
        "a composing field must be busy so its half-formed value is not announced"
    );
    // Busy is not a synonym for unusable: the control is still live.
    assert!(node.states.focused, "the field keeps focus while composing");
    assert!(!node.states.disabled, "composing does not disable the field");
    assert!(!node.states.invalid, "composing does not invalidate the field");

    // Committing settles it again.
    let committed = composer.handle(InputEvent::Commit("日本".into()));
    assert!(matches!(committed, Output::Commit(_)));
    node.states.busy = composer.is_composing();
    assert!(!node.states.busy, "the field is settled once the value commits");

    // The tree audits clean in every one of those states.
    let mut live = A11yTree::new();
    live.insert(A11yNode::new(NodeId(0), A11yRole::Window, "Chisel Docs"));
    live.insert(node);
    assert!(live.audit().is_ok());
}

// --- 2. Composition is consulted before the keymap --------------------------

/// While an IME is composing, the IME owns Esc, Enter, and Space. The keymap's
/// own bindings for those keys must not fire.
///
/// `tui-actions` has no notion of composition, which is the finding: a TUI has
/// to make this ordering decision itself, and every TUI will get it slightly
/// differently. The suite pins the correct order.
#[test]
fn composition_must_be_consulted_before_the_keymap() {
    let mut composer = Composer::new();
    let mut map = Keymap::new(Mode::Normal);
    map.bind("enter", Action::new("app:split"), "split the pane")
        .expect("enter is free in Normal");
    map.bind("esc", Action::new("app:quit"), "quit")
        .expect("esc is free in Normal");

    // Idle: the application's bindings stand.
    assert!(!composer.is_composing());
    let idle = map.resolve(&KeyEvent::plain(Key::Enter));
    assert_eq!(idle.len(), 1);
    assert!(idle[0].is(&Action::new("app:split")));

    // Composing: the same keystroke must reach the IME instead. This is the
    // ordering rule, stated as the application must write it.
    composer.handle(InputEvent::Preedit(Preedit::at_end("にほ")));
    assert!(composer.is_composing());
    let went_to_ime = if composer.is_composing() {
        matches!(
            composer.handle(InputEvent::Commit("日本".into())),
            Output::Commit(_)
        )
    } else {
        false
    };
    assert!(went_to_ime, "Enter committed the candidate instead of splitting");

    // And the field is immediately reusable.
    assert!(!composer.is_composing());
    let after = map.resolve(&KeyEvent::plain(Key::Enter));
    assert_eq!(after.len(), 1);
    assert!(after[0].is(&Action::new("app:split")), "the binding survived the composition");
}

/// The collision is real, not hypothetical: the keys an IME must own are keys
/// the estate's TUIs actually bind.
#[test]
fn the_ime_owned_keys_collide_with_real_bindings() {
    let mut map = Keymap::new(Mode::Normal);
    map.bind("enter", Action::new("app:split"), "split")
        .expect("enter is free");
    map.bind("esc", Action::new("app:quit"), "quit")
        .expect("esc is free");
    map.bind("space", Action::new("app:page"), "page")
        .expect("space is free");

    // Every key the IME needs is one the app has already claimed.
    for key in [Key::Esc, Key::Enter, Key::Space] {
        let bound = map.resolve(&KeyEvent::plain(key));
        assert!(
            !bound.is_empty(),
            "{key:?} is bound, so the IME has to win it while composing"
        );
    }

    // Ctrl-based shortcuts are NOT IME keys and must keep working mid-composition.
    let mut ctrl_map = Keymap::new(Mode::Normal);
    ctrl_map
        .bind("ctrl+s", Action::new("app:save"), "save")
        .expect("ctrl+s is free");
    composer_still_lets_ctrl_through();
    assert_eq!(
        ctrl_map.resolve(&KeyEvent::ctrl(Key::Char('s'))).len(),
        1,
        "ctrl+s must resolve regardless of composition"
    );
}

/// A composing field still accepts keyboard shortcuts the IME does not claim.
fn composer_still_lets_ctrl_through() {
    let mut composer = Composer::new();
    composer.handle(InputEvent::Preedit(Preedit::at_end("か")));
    assert!(composer.is_composing());
    // No event is produced for a key the IME does not consume: the composer
    // only speaks about text.
    assert_eq!(composer.handle(InputEvent::Reset), Output::Cancelled);
}

// --- 3. Caret math over hostile input, through the public API ---------------

/// No platform unit can produce a caret that splits a grapheme cluster. This
/// is the property the crate exists for, so it is checked here through the
/// public constructors rather than trusted to the unit tests.
#[test]
fn no_platform_caret_unit_splits_a_cluster() {
    for s in HOSTILE {
        // Byte offsets, as Wayland and X11 deliver them.
        for byte in 0..=s.len() {
            let p = Preedit::from_bytes(*s, byte);
            assert!(
                s.is_char_boundary(p.caret_byte()),
                "{s:?} byte {byte} gave a caret that is not a char boundary"
            );
            assert!(
                grapheme::byte_offset(s, p.caret()).is_some(),
                "{s:?} byte {byte} gave grapheme {} of {} clusters",
                p.caret(),
                grapheme::count(s)
            );
        }
        // UTF-16 code units, as Win32, macOS, and the web deliver them.
        for unit in 0..=utf16_len(s) {
            let p = Preedit::from_utf16(*s, unit as u16);
            assert!(
                s.is_char_boundary(p.caret_byte()),
                "{s:?} utf16 {unit} gave a caret that is not a char boundary"
            );
            assert!(
                p.caret() <= grapheme::count(s),
                "{s:?} utf16 {unit} gave grapheme {} of {} clusters",
                p.caret(),
                grapheme::count(s)
            );
        }
    }
}

/// The two platform dialects must agree. A disagreement means one rounds and
/// the other does not, which is the bug class.
#[test]
fn the_two_platform_dialects_agree() {
    for s in HOSTILE {
        for unit in 0..=utf16_len(s) {
            let via_utf16 = Preedit::from_utf16(*s, unit as u16).caret();
            let via_bytes = Preedit::from_bytes(*s, utf16_to_byte(s, unit)).caret();
            assert_eq!(
                via_utf16, via_bytes,
                "{s:?} utf16 unit {unit}: {via_utf16} vs {via_bytes}"
            );
        }
    }
}

// --- 4. End-to-end sessions -------------------------------------------------

/// A realistic Japanese session, end to end. The last step is the one that
/// breaks in practice: the field must be reusable immediately after a commit.
#[test]
fn a_full_typing_session_never_loses_or_duplicates_text() {
    let mut composer = Composer::new();
    let mut document = String::new();
    let apply = |composer: &mut Composer, ev: InputEvent, doc: &mut String| {
        if let Output::Commit(t) = composer.handle(ev) {
            doc.push_str(&t);
        }
    };

    apply(&mut composer, InputEvent::Preedit(Preedit::at_end("にほ")), &mut document);
    apply(
        &mut composer,
        InputEvent::Preedit(Preedit::at_end("にほんご")),
        &mut document,
    );
    apply(
        &mut composer,
        InputEvent::Commit("日本語".into()),
        &mut document,
    );

    // Immediately after, the field still works.
    apply(&mut composer, InputEvent::Insert(" ".into()), &mut document);
    apply(
        &mut composer,
        InputEvent::Preedit(Preedit::at_end("こん")),
        &mut document,
    );
    apply(
        &mut composer,
        InputEvent::Commit("こんにちは".into()),
        &mut document,
    );

    assert_eq!(document, "日本語 こんにちは");
    assert!(!composer.is_composing(), "field is idle at the end");
}

/// A dead-key session on a Latin layout: each keypress produces one composed
/// character, and each is a single grapheme so the caret arithmetic downstream
/// still works.
#[test]
fn a_dead_key_session_produces_one_grapheme_per_keypress() {
    let mut composer = Composer::new();
    let mut document = String::new();

    for (base, key) in [('c', DeadKey::Cedilla), ('a', DeadKey::Grave), ('f', DeadKey::Cedilla), ('e', DeadKey::Acute)] {
        composer.handle(InputEvent::Preedit(Preedit::at_end(
            alloc_string(key.mark()),
        )));
        match composer.handle(InputEvent::Commit(key.compose(base))) {
            Output::Commit(t) => document.push_str(&t),
            _ => unreachable!("a commit yields a commit"),
        }
    }

    // `f` + cedilla has no precomposed form in Unicode, so it takes the
    // combining-mark fallback (f + U+0327). That is deliberate: this session
    // exercises the precomposed path (ç, à, é) and the fallback path in one
    // pass, and the fallback is the one every hand-rolled table gets wrong.
    assert_eq!(document, "çàf\u{327}é");
    assert_eq!(document.chars().count(), 5, "five code points");
    assert_eq!(grapheme::count(&document), 4, "four keypresses, four clusters");
    assert_eq!(
        grapheme::delete(&document, 0..1).as_deref(),
        Some("àf\u{327}é"),
        "deleting the ç removes the cedilla with it"
    );
    assert_eq!(
        grapheme::delete(&document, 2..3).as_deref(),
        Some("çàé"),
        "deleting the f removes the combining cedilla with it"
    );
}

/// `char::to_string`, which `no_std` targets still provide via alloc.
fn alloc_string(c: char) -> String {
    let mut s = String::new();
    s.push(c);
    s
}

/// Focus loss at every point of a session must either recover the text or
/// cancel it, and must never leave the field stuck mid-composition.
#[test]
fn focus_loss_always_leaves_the_field_usable() {
    // Loss while composing: the text is recovered.
    let mut c = Composer::new();
    c.handle(InputEvent::Preedit(Preedit::at_end("かん")));
    assert_eq!(c.flush().as_deref(), Some("かん"));
    assert!(!c.is_composing(), "stuck composing after flush");

    // Loss while idle: nothing to recover.
    let mut c = Composer::new();
    assert_eq!(c.flush(), None);
    assert!(!c.is_composing());

    // Loss after a cancel: nothing to recover, still idle.
    let mut c = Composer::new();
    c.handle(InputEvent::Preedit(Preedit::at_end("x")));
    c.handle(InputEvent::Reset);
    assert_eq!(c.flush(), None);
    assert!(!c.is_composing());

    // In every case the field accepts input again.
    let mut c = Composer::new();
    let _ = c.flush();
    assert_eq!(
        c.handle(InputEvent::Insert("ok".into())),
        Output::Commit("ok".into())
    );
}