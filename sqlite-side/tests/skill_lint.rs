#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `skill_lint` — `skillkit` composed: the SKILL.md parser, the manifest
//! validator, and the content-addressed store.
//!
//! `skillkit-core` parses and lints; `skillkit-store` archives by hash. The
//! property that makes them one crate family rather than two: a skill that
//! lints clean archives to a stable address, and the same bytes always archive
//! to the same address. That is what lets `OmniSkill-template` be validated in
//! CI and then stored without renaming.
//!
//! `parse_skill_md` is on the path from untrusted input — a SKILL.md is
//! authored by anyone — so its totality is asserted explicitly.

use skillkit_core::{parse_skill_md, validate_manifest, Manifest, Severity};
use std::fs;
use std::path::PathBuf;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("estate-skillkit-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A well-formed skill parses to a manifest with no findings.
const GOOD_SKILL: &str = "---\nname: hello-world\ndescription: Says hello to a named caller.\n---\n\n# Hello\n\nPrints a greeting for the caller named in the argument.\n\nUse it when a script needs a friendly line of output.\n";

#[test]
fn a_well_formed_skill_parses_clean() {
    let (manifest, findings) = parse_skill_md(GOOD_SKILL);
    assert!(manifest.is_some(), "a well-formed skill yields a manifest");
    assert!(
        findings.iter().all(|f| f.severity != Severity::Error),
        "no error findings, got {findings:?}"
    );
    let m = manifest.expect("checked above");
    assert_eq!(m.name, "hello-world");
    assert_eq!(m.description, "Says hello to a named caller.");
}

/// The parser is on the path from untrusted input: whatever the text, it
/// returns a decisive answer and never panics.
#[test]
fn the_parser_is_total_over_hostile_text() {
    for text in [
        "",
        "---",
        "---\n",
        "---\nnot frontmatter\n",
        "---\nname: \ndescription: \n---\n",
        "---\nname: UPPER-CASE\ndescription: x\n---\n",
        "---\nunknown-field: 1\nname: a\ndescription: b\n---\n",
        "---\nname: a\ndescription: b\n---\n\x00\x01\x02",
        "---\nname: a\ndescription: b\n---\nno trailing newline",
        &"x".repeat(100_000),
    ] {
        // (Option<Manifest>, Vec<Finding>) — both arms are legitimate. A panic
        // is not.
        let (manifest, findings) = parse_skill_md(text);
        // A manifest is only Some when frontmatter was extracted; validation
        // of whatever was extracted must itself be total.
        if let Some(m) = manifest {
            let _ = validate_manifest(&m);
        }
        // Findings always carry a code, so a caller can match.
        for f in findings {
            assert!(!f.code.is_empty(), "a finding without a code: {f:?}");
            assert!(!f.message.is_empty(), "a finding without a message: {f:?}");
        }
    }
}

/// A manifest with a bad name produces an error finding, not a silent pass:
/// the lint exists to catch exactly this.
#[test]
fn an_invalid_name_is_an_error_finding() {
    let manifest = Manifest {
        name: "Not A Valid Name".to_owned(),
        description: "Something.".to_owned(),
        license: None,
        allowed_tools: Vec::new(),
    };
    let findings = validate_manifest(&manifest);
    assert!(
        findings.iter().any(|f| f.severity == Severity::Error),
        "an invalid name must be an error, got {findings:?}"
    );
}

// --- the store --------------------------------------------------------------

/// The store is content-addressed: the same bytes archive to the same address,
/// which is what makes `has` a membership test rather than a lookup.
#[test]
fn the_store_is_content_addressed() {
    let root = temp_dir("addressed");
    let a = skillkit_store::put(&root, b"same bytes").expect("put");
    let b = skillkit_store::put(&root, b"same bytes").expect("put again");
    assert_eq!(a, b, "identical bytes must address identically");
    assert!(skillkit_store::has(&root, &a));

    let c = skillkit_store::put(&root, b"different bytes").expect("put other");
    assert_ne!(a, c, "different bytes must address differently");
    assert!(skillkit_store::has(&root, &c));
    assert!(!skillkit_store::has(&root, "deadbeef"), "an absent hash is absent");
}

/// A skill that lints clean archives, and the archive round-trips. This is the
/// two halves of the family meeting: the parser's output feeds the store.
#[test]
fn a_clean_skill_archives_and_round_trips() {
    let (manifest, findings) = parse_skill_md(GOOD_SKILL);
    assert!(manifest.is_some(), "fixture must be clean for this test");
    assert!(!findings.iter().any(|f| f.severity == Severity::Error));

    let root = temp_dir("round-trip");
    let address = skillkit_store::put(&root, GOOD_SKILL.as_bytes()).expect("archives");
    assert!(skillkit_store::has(&root, &address), "the archive is findable");

    // Reading it back gives the same bytes, so re-parsing gives the same
    // manifest. That is the whole reason to store rather than re-fetch.
    let store = cas_kit::store::BlobStore::new(&root).expect("the same CAS the skill kit uses");
    let hash = cas_kit::Hash::from_hex(&address).expect("the address is a cas hash");
    let read_back = store.get_blob(&hash).expect("read back");
    assert_eq!(read_back, GOOD_SKILL.as_bytes());
    let (manifest_again, _) = parse_skill_md(
        std::str::from_utf8(&read_back).expect("stored text is utf8"),
    );
    assert_eq!(manifest_again, manifest, "the same bytes re-parse identically");
}

/// `lint_dir` walks a directory: a missing directory is an Err, not a panic.
#[test]
fn lint_dir_reports_a_missing_directory() {
    let missing = std::env::temp_dir().join(format!("estate-skillkit-absent-{}", std::process::id()));
    let _ = fs::remove_dir_all(&missing);
    assert!(
        skillkit_core::lint_dir(&missing).is_err(),
        "a directory that does not exist must be an Err"
    );
}

/// A directory with a real SKILL.md lints through the same code the archive
/// path uses, so the two cannot drift.
#[test]
fn lint_dir_walks_a_real_skill_directory() {
    let dir = temp_dir("lint");
    fs::write(dir.join("SKILL.md"), GOOD_SKILL).expect("write fixture");
    let findings = skillkit_core::lint_dir(&dir).expect("lints");
    assert!(
        !findings.iter().any(|f| f.severity == Severity::Error),
        "the good fixture must lint clean, got {findings:?}"
    );
}