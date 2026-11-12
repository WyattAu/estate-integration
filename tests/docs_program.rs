//! Suite: `docs_program` — the docs extension program, published
//! 2026-11-10/11: `docs-compiler` (L3) composing `docs-symbols`,
//! `docs-witness`, and `docs-proof`, over `docs-core` (L2), with
//! `docs-search` (L0) beside the pipeline for the client runtime.
//!
//! Five crates, one week old, and each passes its own suite. What nothing
//! else proves is that the *published* versions resolve a document
//! through each other: the compiler's fence output feeding docs-core's
//! renderer, symbol spans flowing into a search index, witness output
//! binding to a certificate-grade hash. The pipeline test is the M3/M4
//! close criterion from docs-core SPEC.md, run here against released
//! artifacts instead of workspace siblings.
//!
//! Run-one finding (2026-11-12): `docs-symbols` 0.1.0 built against
//! tree-sitter 0.26 while `docs-pipeline` 0.1.4's `tree-sitter-highlight`
//! 0.25 needs tree-sitter 0.25 — two majors of a `links =` crate cannot
//! share a graph, so the five-crate resolve failed outright. Fixed the
//! same day as docs-symbols 0.1.1 (tree-sitter 0.25, no code changes),
//! which is the whole argument for composing.
//!
//! Lean is deliberately absent from this suite: the `lean-check` fence
//! needs a real toolchain, and it is exercised by docs-compiler's
//! dedicated `lean-oracle` CI job against a pinned
//! `leanprover/lean4:v4.35.0` fixture. Everything else about the
//! pipeline is proven here.

// Estate-suite convention: fixtures are built in temp roots, not
// committed; expectations carry context.
#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use docs_compiler::{compile, BuildContext};
use docs_search::IndexBuilder;
use std::path::PathBuf;

const TOOLCHAIN: &str = "leanprover/lean4:v4.35.0";

/// A build root with the two content fixtures the pipeline consumes.
fn pipeline_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root: PathBuf = dir.path().to_path_buf();

    std::fs::write(
        root.join("engine.rs"),
        "pub mod engine {\n    pub fn spin_up() -> bool { true }\n}\n",
    )
    .expect("engine.rs");

    let wat = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write"
        (func $fd_write (param i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "\10\00\00\00\05\00\00\00")
      (data (i32.const 16) "hello")
      (func (export "_start")
        i32.const 1
        i32.const 0
        i32.const 1
        i32.const 48
        call $fd_write
        drop))
    "#;
    std::fs::write(root.join("hello.wasm"), wat::parse_str(wat).expect("wat")).expect("wasm");
    (dir, root)
}

/// The whole point: the published compiler resolves island,
/// include-symbol, and witness fences through the published crates, and
/// docs-core renders the result. The lean-check fence is proven by
/// docs-compiler's own oracle job; this suite pins the lean-free
/// pipeline that ships to every page.
#[test]
fn the_published_pipeline_resolves_directives_through_each_other() {
    let (_guard, root) = pipeline_root();
    let doc = r#"# Released pipeline

```island nav-search client=visible
{"count": 2}
```

```include-symbol file="engine.rs" symbol="engine::spin_up"
```

```witness wasm="hello.wasm" assert_stdout="hello" assert_exit=0
```

Footer.
"#;
    let html = compile(doc, &BuildContext::new(&root)).expect("the released artifacts compose");
    assert!(html.contains("<docs-island"), "docs-core island emission");
    assert!(html.contains("docs-symbol"), "docs-symbols extraction emitted");
    assert!(html.contains("dsym-rust-"), "anchor id flowed through");
    assert!(html.contains("pub fn spin_up"), "exact span embedded");
    assert!(html.contains("docs-witness"), "witness block emitted");
    assert!(html.contains("hello"), "witness stdout embedded");
    assert!(html.contains("<h1>Released pipeline</h1>"), "prose rendered");
}

/// Cross-crate data flow: a symbol span extracted by docs-symbols is
/// exactly what docs-search indexes and finds. Two crates agreeing on
/// the same bytes is the composition contract.
#[test]
fn extracted_symbols_flow_into_the_search_index() {
    let source = "pub mod engine {\n    pub fn spin_up() -> bool { true }\n}\n";
    let span = docs_symbols::extract(source, docs_symbols::Language::Rust, "engine::spin_up")
        .expect("extracts");

    let mut index = IndexBuilder::new();
    index.add_doc(&span.code);
    index.add_doc("unrelated prose about tulips");
    let bytes = index.build().expect("indexes");
    let idx = docs_search::Index::from_bytes(&bytes).expect("decodes");

    let hits = idx.query("spin_up", 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc, 0);
    // And the same span's anchor ID is what a page would carry.
    assert!(span.anchor_id.starts_with("dsym-rust-"));
}

/// An execution attestation: docs-witness captures a run, docs-proof's
/// byte-exact hash binds the attestation to those exact bytes, and the
/// verifier fails the moment the output drifts. This is the REQ-W x
/// REQ-P composition a page badge would stand on.
#[test]
fn witness_output_binds_to_a_certificate_grade_hash() {
    let (_guard, root) = pipeline_root();
    let wasm = std::fs::read(root.join("hello.wasm")).expect("fixture wasm");
    let w = docs_witness::run(&wasm, &docs_witness::WitnessConfig::default())
        .expect("witness runs");

    let cert = docs_proof::CertificateBuilder::new(
        "witness_hello_output",
        &String::from_utf8_lossy(&w.stdout),
        TOOLCHAIN,
    )
    .verified();

    assert!(docs_proof::verify(&cert, &String::from_utf8_lossy(&w.stdout), TOOLCHAIN).is_ok());
    // One byte of stdout drift breaks the attestation.
    assert!(
        docs_proof::verify(
            &cert,
            &format!("{}x", String::from_utf8_lossy(&w.stdout)),
            TOOLCHAIN
        )
        .is_err()
    );
}

/// The pins in Cargo.toml claim exact published versions; the VERSION
/// constants are the crates' own admission of what they are. If these
/// diverge from the pins, this suite has been testing artifacts nobody
/// ships.
#[test]
fn the_pins_and_the_crates_agree() {
    assert_eq!(docs_compiler::VERSION, "0.1.0");
    assert_eq!(docs_search::VERSION, "0.1.0");
    assert_eq!(docs_symbols::VERSION, "0.1.1");
    assert_eq!(docs_witness::VERSION, "0.1.0");
    assert_eq!(docs_proof::VERSION, "0.1.0");
    assert_eq!(docs_core::VERSION, "0.1.1");
}
