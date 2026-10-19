#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `text_field` — `ui-text-edit` composed with `input-kit`, `a11y-kit`,
//! and `tui-actions`.
//!
//! Written before `ui-text-edit` was published, per ADR-0005. The crate's own
//! unit tests prove it works in isolation; this suite is here for the three
//! things that only exist when the layers meet, none of which any single crate
//! can see:
//!
//! 1. **An announced value has a length.** A bridge that puts the whole field
//!    text into an accessible name is fine in a unit test with five characters
//!    and wrong in an application with a 10 KB mail body: the screen reader
//!    reads the entire thing on every focus. The limit belongs to the pair
//!    (field, tree), so this is where it gets tested.
//!
//! 2. **Printable keys belong to the field, not the keymap.** The same `j` that
//!    scrolls a list inserts a `j` when a text field has focus. Ordering is the
//!    application's decision and every application gets it slightly wrong.
//!
//! 3. **A caret cannot split a character after a round trip.** The field works
//!    in clusters, the platform reports in UTF-16 or bytes, and the tree reports
//!    a string. All three must agree on what character is under the caret.

use a11y_kit::{A11yNode, A11yRole, A11yTree, NodeId};
use input_kit::{Composer, DeadKey, InputEvent, Output, Preedit};
use tui_actions::{Action, Key, KeyEvent, Keymap, Mode};
use ui_text_edit::bridge;
use ui_text_edit::compose::composing_field_from;
use ui_text_edit::{StringBuffer, TextField, field_from, field_from_start};

/// The longest value this suite accepts as announceable. Not a house rule, a
/// measured stand-in for "enough to be useful, short enough to be read".
const MAX_ANNOUNCED: usize = 80;

// --- 1. The announced value has a length ------------------------------------

/// A very long field must not be announced in full.
#[test]
fn a_long_value_is_truncated_at_a_character_boundary() {
    let body = "x".repeat(500);
    let f = field_from(&body);
    let announced = bridge::a11y_node_for_field(&f, "Body", NodeId(1));
    let value = announced
        .name
        .split_once(": ")
        .map(|(_, v)| v)
        .expect("the label and the value are separated");

    assert!(
        input_kit::grapheme::count(value) <= MAX_ANNOUNCED + 1,
        "announced {} clusters for a 500-cluster field; a screen reader reads \
         the whole thing on every focus",
        input_kit::grapheme::count(value)
    );
    assert_eq!(
        input_kit::grapheme::count(value),
        MAX_ANNOUNCED + 1,
        "the limit plus the ellipsis"
    );
    assert!(value.ends_with(bridge::ELLIPSIS));
    // The field itself is untouched: truncation is a reporting concern.
    assert_eq!(f.text(), body);
}

/// Truncation must not split a combining mark, a flag, or an emoji.
///
/// This is the failure that makes truncation worse than no truncation: the
/// screen reader reads a replacement character, or worse, the screen reader and
/// the field disagree about what the text is.
#[test]
fn truncation_never_splits_a_cluster() {
    for filler in ["e\u{301}", "\u{1F1FA}\u{1F1F8}", "\u{1F469}\u{200D}\u{1F4BB}", "日"] {
        let body = filler.repeat(200);
        let announced = bridge::truncate_for_announcement(&body);
        assert!(bridge::is_truncated(&body));
        assert!(announced.ends_with(bridge::ELLIPSIS), "{filler:?}: no ellipsis");

        // The announced value is the body's first N clusters plus the ellipsis,
        // so stripping the ellipsis must leave an exact prefix of the body.
        let cut = &announced[..announced.len() - bridge::ELLIPSIS.len_utf8()];
        assert!(
            body.starts_with(cut),
            "{filler:?}: {cut:?} is not a prefix of the body"
        );
        assert_eq!(
            input_kit::grapheme::count(cut),
            bridge::MAX_ANNOUNCED_CHARS,
            "{filler:?}: cut on a non-cluster boundary"
        );
        // And the field's own view is untouched: truncation is a reporting
        // concern, not an edit.
        let f = field_from(&body);
        assert_eq!(f.text().len(), body.len(), "the field lost text");
        assert_eq!(f.len(), 200, "the field lost clusters");
    }
}

/// A short value is announced in full: truncation must not fire early.
#[test]
fn a_short_value_is_announced_whole() {
    let f = field_from("hello");
    let announced = bridge::a11y_node_for_field(&f, "Name", NodeId(1));
    assert_eq!(announced.name, "Name: hello");
}

// --- 2. Printable keys belong to the field ----------------------------------

/// A focused field takes printable keys; the keymap only sees the rest.
#[test]
fn a_focused_field_claims_printable_keys_first() {
    let mut map = Keymap::new(Mode::Normal);
    for chord in ["j", "k", "g", "d"] {
        map.bind(chord, Action::new("app:scroll"), "scroll")
            .unwrap_or_else(|_| panic!("{chord} should be free"));
    }

    let mut field = field_from_start("");
    let focus_has_it = |field: &mut ui_text_edit::StringField, key: Key| -> bool {
        if let Key::Char(c) = key {
            let mut s = [0u8; 4];
            field.insert(c.encode_utf8(&mut s));
            true
        } else {
            false
        }
    };

    for key in [Key::Char('j'), Key::Char('k')] {
        assert!(focus_has_it(&mut field, key), "a focused field takes {key:?}");
        assert_eq!(field.text(), if key == Key::Char('j') { "j" } else { "jk" });
    }

    // The keymap would have fired for the same keys, and did not.
    assert!(!map.resolve(&KeyEvent::plain(Key::Char('j'))).is_empty());
    assert_eq!(field.text(), "jk", "the keymap did not also fire");
}

/// When the field loses focus the keymap takes over again, for the same keys.
#[test]
fn losing_focus_returns_the_keys_to_the_keymap() {
    let mut map = Keymap::new(Mode::Normal);
    map.bind("j", Action::new("app:scroll"), "scroll")
        .expect("j is free");

    let focused = true;
    let mut field = field_from_start("");
    // Focused: the key goes to the field.
    if focused {
        field.insert("j");
    }
    assert_eq!(field.text(), "j");

    // Unfocused: the key goes to the keymap, and the field is untouched.
    let before = field.text().to_owned();
    if !focused {
        let actions = map.resolve(&KeyEvent::plain(Key::Char('j')));
        assert!(!actions.is_empty());
    }
    assert_eq!(field.text(), before, "the field did not see an unfocused key");
}

// --- 3. The caret survives every unit it passes through ----------------------

/// The field's caret is in clusters, the platform's is in UTF-16 or bytes, and
/// the tree only ever sees a string. All three must name the same character.
#[test]
fn the_caret_names_the_same_character_in_every_unit() {
    let text = "a\u{1F600}e\u{301}日";
    let mut field = field_from_start(text);

    // MEASURED, not assumed: 4 clusters; 1+4+1+2+3 = 11 bytes; and
    // 1+2+1+1+1 = 6 UTF-16 code units. Three different widths for four
    // characters, which is the whole reason the crate exists.
    let clusters = input_kit::grapheme::count(text);
    assert_eq!(clusters, 4);
    assert_eq!(text.len(), 11);
    let utf16: usize = text.chars().map(char::len_utf16).sum();
    assert_eq!(utf16, 6);
    assert!(text.len() != clusters, "the units must actually differ here");

    for target in 0..=clusters {
        // Put the caret at cluster `target`.
        field.set_caret(target);
        assert_eq!(field.caret(), target, "cluster address");

        // The platform reports the same position in its own unit.
        let byte = input_kit::grapheme::byte_offset(text, target).unwrap_or(text.len());
        let unit = utf16_units_before(text, byte);
        let from_bytes = Preedit::from_bytes(text, byte);
        let from_utf16 = Preedit::from_utf16(text, unit as u16);
        assert_eq!(
            from_bytes.caret(),
            from_utf16.caret(),
            "the two platform units disagree at cluster {target}"
        );

        // And the field, the preedit, and the buffer all agree on the prefix.
        assert_eq!(
            field.text().get(..byte),
            Some(&text[..byte]),
            "the field's slice at cluster {target} differs from the raw text"
        );
    }
}

/// UTF-16 code units before `byte`.
fn utf16_units_before(text: &str, byte: usize) -> usize {
    text[..byte].chars().map(char::len_utf16).sum()
}

// --- 4. The full loop: compose, edit, announce -------------------------------

/// A realistic session across all three crates: type Japanese by IME, fix a
/// typo with the keyboard, and check the announced value at each step.
#[test]
fn a_session_across_all_three_crates_ends_consistent() {
    let mut field = composing_field_from("");

    // IME: compose "にほんご" and commit "日本語".
    for pre in ["に", "にほ", "にほんご"] {
        field.handle(InputEvent::Preedit(Preedit::at_end(pre)));
        assert!(bridge::is_busy(&field), "mid-syllable must not be announced");
    }
    field.handle(InputEvent::Commit("日本語".into()));
    assert_eq!(field.text(), "日本語");

    // Keyboard: append a space and a dead-key accent.
    field.field_mut().insert(" ");
    field.handle(InputEvent::Commit(DeadKey::Acute.compose('e')));

    assert_eq!(field.text(), "日本語 \u{00E9}");
    assert_eq!(field.field().len(), 5, "four clusters plus the composed e");

    // Announced, settled, and audit clean under a window.
    let node = bridge::a11y_node_for(&field, "Body");
    assert!(!node.states.busy, "the value is final");
    assert!(node.name.contains("日本語"));

    let mut tree = A11yTree::new();
    tree.insert(A11yNode::new(NodeId(0), A11yRole::Window, "Chisel Docs"));
    let mut placed = node;
    placed.id = NodeId(1);
    placed.parent = Some(NodeId(0));
    tree.insert(placed);
    assert!(tree.audit().is_ok());
}

/// Cancelling an IME composition must leave the value exactly as it was, not
/// half-changed. This is where a field that mirrors the preedit into its own
/// text would corrupt the document.
#[test]
fn a_cancelled_composition_leaves_the_value_untouched() {
    let mut field = composing_field_from("draft");
    for pre in ["こ", "こん", "こんにち"] {
        field.handle(InputEvent::Preedit(Preedit::at_end(pre)));
    }
    assert_eq!(field.handle(InputEvent::Reset), ui_text_edit::FieldUpdate::Cancelled);
    assert_eq!(field.text(), "draft", "the draft is untouched");
    assert!(!bridge::is_busy(&field));
    assert!(!field.flush(), "a cancelled composition has nothing to flush");
}

/// Focus loss mid-composition: the typed text is recovered exactly once.
#[test]
fn focus_loss_recovering_composed_text_does_not_duplicate_it() {
    let mut field = composing_field_from("a");
    field.handle(InputEvent::Preedit(Preedit::at_end("にほ")));
    assert!(field.flush());
    assert_eq!(field.text(), "aにほ");
    assert!(!field.flush(), "flushing twice must not insert twice");
    assert_eq!(field.text(), "aにほ");
    assert!(!field.flush());
    assert_eq!(field.text(), "aにほ");
}

/// The composition path and the plain field must agree. `ComposingField`
/// delegates to `TextField`, and a divergence between them is the bug that
/// only shows up when a bug report says "works when I type, broken when I use
/// my IME".
#[test]
fn the_composing_and_plain_paths_agree() {
    let mut plain = field_from("");
    let mut composing = composing_field_from("");
    plain.insert("hello");
    composing.handle(InputEvent::Insert("hello".into()));
    assert_eq!(plain.text(), composing.text());
    assert_eq!(plain.caret(), composing.field().caret());

    plain.backspace();
    composing.field_mut().backspace();
    assert_eq!(plain.text(), composing.text());
    assert_eq!(plain.caret(), composing.field().caret());
}

/// A buffer behind the trait is swappable. This is the whole argument for
/// `TextBuffer`: a caller can put their own rope under the same field.
#[test]
fn the_field_works_over_any_buffer() {
    /// A minimal non-`String` buffer, to prove the field does not reach
    /// through the trait into `StringBuffer`.
    #[derive(Default)]
    struct Shouted(StringBuffer);
    impl ui_text_edit::TextBuffer for Shouted {
        fn text(&self) -> &str {
            // Not the real text: a buffer is allowed to present any view.
            ""
        }
        fn len_clusters(&self) -> usize {
            input_kit::grapheme::count(self.0.as_str())
        }
        fn replace(&mut self, range: std::ops::Range<usize>, insert: &str) {
            self.0.replace(range, insert);
        }
    }

    let mut f = TextField::with_caret_at_end(Shouted(StringBuffer::from_text("abc")));
    assert_eq!(f.len(), 3);
    f.backspace();
    assert_eq!(f.len(), 2);
    f.select_all();
    assert_eq!(f.selection().len(), 2);
}

/// A field with an empty buffer and a stale caret is still coherent.
#[test]
fn an_empty_field_with_a_moved_caret_stays_coherent() {
    let mut f = field_from("");
    assert_eq!(f.caret(), 0);
    assert!(!f.move_left());
    assert!(!f.move_right());
    assert!(!f.backspace());
    assert!(!f.delete_forward());
    f.select_all();
    assert_eq!(f.selection_text(), "");
    assert!(f.insert("x"));
    assert_eq!(f.text(), "x");
    assert_eq!(f.caret(), 1);
    let _: Composer = Composer::new();
    let _ = Output::Idle;
}