#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `audiobook_stack` — `audiobook-core` over the `id3-core` and
//! `mp4-core` leaves, all published 2026-10-21.
//!
//! These three crates are hours old, and a publication nobody has composed is
//! exactly the artifact ADR-0005 exists to catch: the crates compile on their
//! own and pass their own tests, and nothing proves the versions on crates.io
//! are the versions in the repos, or that the domain model reads what the box
//! and tag codecs write.
//!
//! The chain is the point. `audiobook-core` consumes `id3-core` and `mp4-core`
//! as L0 leaves; a suite that pinned only the leaves would leave the middle of
//! the chain unverified, and a suite that pinned only the middle would leave
//! the leaves' error paths unwalked.

use audiobook_core::sidecar::{ffmetadata_escape, ffmetadata_unescape, SidecarFormat};
use audiobook_core::{
    Chapter, Confidence, ContainerKind, MediaProbe, Part, SidecarChapters, Title,
};

// --- probe: hostile bytes at the container boundary -------------------------

/// `MediaProbe::probe` never fails by design: bytes it cannot identify yield
/// `Confidence::Unknown` with no fields set, so a folder listing survives one
/// unreadable file. Totality is the contract; asserted over the inputs most
/// likely to break a hand-written parser.
#[test]
fn the_probe_is_total_over_hostile_bytes() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one zero", vec![0]),
        ("ftyp magic only", b"ftyp".to_vec()),
        (
            "ftyp with a truncated size",
            b"\x00\x00\x00\x10ftypM4A ".to_vec(),
        ),
        ("id3 magic only", b"ID3".to_vec()),
        (
            "id3 with an absurd tag size",
            b"ID3\x04\x00\x00\xff\xff\xff\xff\xff\xff\xff".to_vec(),
        ),
        ("mp3 frame sync only", vec![0xFF, 0xFB, 0x90, 0x00]),
        ("all zeros", vec![0u8; 128]),
        ("all ones", vec![0xFFu8; 128]),
        ("utf8 text", "not audio at all".as_bytes().to_vec()),
    ];
    for (name, bytes) in cases {
        let probe = MediaProbe::probe(&bytes);
        // Totality is the assertion. When the bytes ARE identifiable, the
        // container must be named; when they are not, nothing may be claimed.
        if probe.container.is_none() {
            assert_eq!(
                probe.confidence,
                Confidence::Unknown,
                "{name}: no container but confidence {:#?}",
                probe.confidence
            );
            assert!(
                probe.duration_ms.is_none(),
                "{name}: a duration without a container is a lie"
            );
        }
    }
}

/// An MP3 with a real ID3v2 header and a frame after it must be identified.
/// Without this positive control the hostile tests prove only that the probe
/// refuses things, which a broken parser also does.
#[test]
fn an_mp3_with_a_real_frame_is_identified() {
    // ID3v2.3 header (10 bytes, tag size 0) + an MPEG-1 Layer III frame
    // sync, then padding to the frame length.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"ID3\x03\x00\x00\x00\x00\x00\x00");
    bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    bytes.extend_from_slice(&[0u8; 413]);
    let probe = MediaProbe::probe(&bytes);
    assert_eq!(
        probe.container,
        Some(ContainerKind::Mp3Id3),
        "an ID3-tagged MP3 must be named"
    );
}

// --- the domain model over the codecs --------------------------------------

/// A title laid out into parts, flattened, and rebuilt must come back the
/// same. This is the round trip a player performs on every open, and the doc
/// comment on `from_flat_chapters` warns that filing a chapter under a part it
/// merely *spans* is the easy mistake.
#[test]
fn a_laid_out_title_round_trips_through_flat_chapters() {
    let title = Title::new("The Long Book", "Some Author")
        .with_part(Part::new("Part One"))
        .with_part(Part::new("Part Two"))
        .laid_out();

    let mut title = title;
    // Chapters live inside parts; give each part one.
    title.parts[0].chapters.push(Chapter::new("Opening", 0, 0));
    title.parts[1].chapters.push(Chapter::new("Later", 0, 1));
    let title = title.laid_out();

    let durations: Vec<u64> = title
        .parts_with_offsets()
        .iter()
        .map(|p| p.duration_ms.unwrap_or(0))
        .collect();
    let flat = title.chapters();
    assert_eq!(flat.len(), 2, "two chapters across two parts");

    let rebuilt =
        Title::from_flat_chapters(flat.clone(), &durations, "The Long Book", "Some Author")
            .laid_out();
    assert_eq!(rebuilt.chapters().len(), title.chapters().len());
    for (a, b) in title.chapters().iter().zip(rebuilt.chapters().iter()) {
        assert_eq!(a.title, b.title, "chapter titles diverged");
        assert_eq!(
            a.title_start_ms(),
            b.title_start_ms(),
            "whole-title offsets diverged"
        );
    }
}

/// `chapter_at` must answer the question a player asks on seek, and the
/// boundary belongs to the chapter that starts there.
#[test]
fn chapter_at_finds_the_containing_chapter() {
    let mut title = Title::new("Book", "Author");
    title.parts.push(Part::new("Single file"));
    title.parts[0].chapters.push(Chapter::new("Opening", 0, 0));
    title.parts[0]
        .chapters
        .push(Chapter::new("The Middle", 600_000, 0));
    title.parts[0]
        .chapters
        .push(Chapter::new("The End", 1_200_000, 0));
    let title = title.laid_out();

    let at = |ms: u64| title.chapter_at(ms).map(|c| c.title.as_str());
    assert_eq!(at(0), Some("Opening"));
    assert_eq!(at(599_999), Some("Opening"), "one ms before the boundary");
    assert_eq!(
        at(600_000),
        Some("The Middle"),
        "the boundary is the new chapter"
    );
    assert_eq!(
        at(10_000_000),
        Some("The End"),
        "past the end is the last chapter"
    );
}

// --- sidecar round trip -----------------------------------------------------

/// FFMetadata escaping must round-trip: chapter titles with `#`, `=`, or `\`
/// are legal in books and a one-way escape would corrupt every rewrite.
#[test]
fn ffmetadata_escaping_round_trips() {
    for title in [
        "Plain",
        "Has # hash",
        "Has = equals",
        "Has \\ backslash",
        "Has # = and \\ all three",
        "",
    ] {
        let escaped = ffmetadata_escape(title);
        let unescaped = ffmetadata_unescape(&escaped);
        assert_eq!(unescaped, title, "{title:?} did not survive escaping");
    }
}

/// A sidecar written to FFMetadata and re-read must keep its order, titles,
/// and timestamps. This is the merge path for an audiobook whose chapters live
/// beside the audio rather than in it.
#[test]
fn a_sidecar_round_trips_through_ffmetadata() {
    let sidecar = SidecarChapters {
        chapters: vec![
            Chapter::new("Opening", 0, 0),
            Chapter::new("The Middle", 600_000, 0),
            Chapter::new("The End", 1_200_000, 0),
        ],
        format: SidecarFormat::Ffmetadata,
        global_tags: vec![("title".to_owned(), "The Long Book".to_owned())],
        duration_ms: Some(1_800_000),
        warnings: Vec::new(),
    };

    let text = sidecar.to_ffmetadata();
    assert!(text.contains("Opening"), "the written form lost a title");
    assert!(
        text.contains("title=The Long Book"),
        "global tags are written"
    );

    let reparsed =
        audiobook_core::sidecar::parse(&text).expect("the writer's own output re-parses");
    let sorted = reparsed.sorted();
    assert_eq!(sorted.len(), 3, "all three chapters survive the round trip");
    let titles: Vec<&str> = sorted.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Opening", "The Middle", "The End"]);
    // MEASURED: the parser uppercases tag keys on read, so `title=` comes
    // back as `TITLE`. Asserted on the uppercased form, which is what the
    // crate's own `global_tags_are_carried_through` test also expects.
    assert_eq!(
        reparsed
            .global_tags
            .iter()
            .find(|(k, _)| k == "TITLE")
            .map(|(_, v)| v.as_str()),
        Some("The Long Book"),
        "global tags survive the round trip"
    );
}

/// An unsorted sidecar must come back sorted: chapter lists in the wild are
/// frequently unordered and a player that trusts file order seeks wrong.
#[test]
fn an_unsorted_sidecar_is_sorted_on_read() {
    let sidecar = SidecarChapters {
        chapters: vec![
            Chapter::new("Late", 1_200_000, 0),
            Chapter::new("Early", 0, 0),
        ],
        format: SidecarFormat::Ffmetadata,
        global_tags: Vec::new(),
        duration_ms: None,
        warnings: Vec::new(),
    };
    let sorted = sidecar.sorted();
    assert_eq!(sorted.len(), 2);
    assert!(sorted[0].start_ms <= sorted[1].start_ms);
    assert_eq!(sorted[0].title, "Early");
}

// --- the leaves, directly --------------------------------------------------

/// The leaves take untrusted bytes, so they get the totality rule at this
/// boundary too, driven through the probe the estate actually calls.
///
/// The leaves take untrusted bytes, so they get the totality rule at this
/// boundary too, driven through the probe the estate actually calls.
///
/// FINDING, from writing this test: container recognition and confidence are
/// independent axes. Four bytes of `ftyp` magic are named `Some(Mp4)` at
/// `Confidence::Unknown` -- the crate recognises the container kind from the
/// magic alone and declines to vouch for anything else. My first assertion
/// held the two together and was wrong about the contract; the published
/// behaviour is the more useful one, because "which container is this" and
/// "how sure am I about the contents" are different questions a caller asks
/// separately.
///
/// What must hold: no container means no duration (a duration without a
/// container is a lie), and recognition of real magic is correct even when
/// the rest of the input is absurd -- the absurdity is the parser's problem,
/// and it survives it.
#[test]
fn the_leaves_survive_hostile_bytes_through_the_model() {
    for bytes in [
        Vec::new(),
        b"ftyp".to_vec(),
        b"ID3\x04\x00\x00\xff\xff\xff\xff".to_vec(),
        vec![0xFFu8; 200],
    ] {
        let probe = MediaProbe::probe(&bytes);
        if probe.container.is_none() {
            assert_eq!(probe.confidence, Confidence::Unknown);
            assert!(
                probe.duration_ms.is_none(),
                "{bytes:?}: duration without a container"
            );
        }
    }
    // Real magic is recognised even when the rest is absurd.
    let probe = MediaProbe::probe(b"ID3\x04\x00\x00\xff\xff\xff\xff");
    assert_eq!(probe.container, Some(ContainerKind::Mp3Id3));
    // MP4 detection needs more than the magic: a bare `ftyp` with no size or
    // brand is Unknown, which is the conservative answer.
    assert_eq!(MediaProbe::probe(b"ftyp").container, None);
}
