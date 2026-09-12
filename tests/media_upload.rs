#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Scenario 4 — `media_upload`: validkit + media-kit + blobkit + cas-kit.
//!
//! Upload flow: validate the object key → sniff the bytes (bomb-guard via
//! byte/dimension limits) → EXIF auto-orient → parallel variants → store
//! each variant in blobkit → content-address the original via cas-kit so an
//! exact-duplicate second upload stores nothing new.

use bytes::Bytes;
use media_kit::encode::OutFormat;
use media_kit::meta::Limits;
use media_kit::resize::{Filter, Fit};
use media_kit::variants::VariantSet;

/// Render a small in-memory JPEG (no fixtures, no network).
fn test_jpeg(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new(&mut buf);
    use image::ImageEncoder;
    encoder
        .write_image(&img, width, height, image::ExtendedColorType::Rgb8)
        .expect("jpeg encode");
    buf
}

fn variant_set() -> VariantSet {
    VariantSet::new()
        .with(media_kit::variants::Variant::new(
            "thumb",
            Fit::MaxSide(32),
            OutFormat::Jpeg(80),
        ))
        .with(
            media_kit::variants::Variant::new("small", Fit::Width(48), OutFormat::Png)
                .filter(Filter::Triangle),
        )
}

/// One upload: key validation → sniff → limits → EXIF orient (inside
/// `generate_from_bytes_with_limits`) → parallel variants → blob store.
/// Returns the variant outputs for the caller to persist.
fn process_upload(
    object_key: &str,
    bytes: &[u8],
    limits: &Limits,
) -> Result<Vec<(String, OutFormat, Vec<u8>)>, String> {
    // Boundary: validkit object-key rules (no `..`, no leading `/`).
    let key = validkit::ObjectKey::parse(object_key).map_err(|e| format!("bad key: {e}"))?;
    // Sniff before decode: non-images never reach the decoder.
    let format = media_kit::sniff::sniff(bytes).ok_or_else(|| "unsniffable bytes".to_string())?;
    assert_eq!(format, media_kit::sniff::Format::Jpeg);
    let _ = key;
    variant_set()
        .generate_from_bytes_with_limits(bytes, limits)
        .map_err(|e| format!("pipeline: {e}"))
}

/// Duplicate uploads dedup: the second identical byte stream stores nothing
/// new — cas-kit reports `AlreadyExists` and the blob count is unchanged.
#[tokio::test]
async fn duplicate_upload_dedup_assertion() {
    use blobkit::store::BlobStore;

    let bytes = test_jpeg(64, 64);
    let outputs = process_upload("uploads/photo.jpg", &bytes, &Limits::default()).expect("upload");
    assert_eq!(outputs.len(), 2, "two variants generated");

    let blobs = blobkit::memory::MemoryStore::new();
    for (name, _format, data) in &outputs {
        let key = blobkit::types::ObjectKey::new(format!("uploads/photo.jpg/{name}")).expect("key");
        blobs
            .put(key, Bytes::from(data.clone()))
            .await
            .expect("put");
    }

    let (_tmp, cas) = cas_kit::store::BlobStore::open_in_memory().expect("cas");
    let hash_first = cas.put_blob(&bytes).expect("first put");
    let count_first = cas.blob_count().expect("count");
    assert_eq!(count_first, 1);

    // Exact-dupe second upload: content address matches, nothing new stored.
    let hash_second = cas.put_blob(&bytes).expect("idempotent put");
    assert_eq!(hash_first, hash_second, "same bytes → same address");
    assert_eq!(cas.blob_count().expect("count"), 1, "no new blob stored");
    assert!(
        cas.put_blob_new(&bytes).is_err(),
        "put_blob_new rejects the duplicate"
    );

    // Variants roundtrip byte-identical through blobkit.
    for (name, _format, data) in &outputs {
        let key = blobkit::types::ObjectKey::new(format!("uploads/photo.jpg/{name}")).expect("key");
        let back = blobs.get(&key).await.expect("get");
        assert_eq!(&back[..], &data[..], "variant {name} roundtrips");
    }

    // Different bytes address differently.
    let other = test_jpeg(64, 65);
    let hash_other = cas.put_blob(&other).expect("put");
    assert_ne!(hash_first, hash_other);
    assert_eq!(cas.blob_count().expect("count"), 2);
}

/// Oversized inputs are rejected by the bomb-guard before decode: a tiny
/// byte budget fails, and a tiny dimension budget fails on a large image.
#[test]
fn oversized_bomb_rejection() {
    let bytes = test_jpeg(64, 64);

    let tiny_bytes = Limits {
        max_bytes: 16,
        ..Limits::default()
    };
    let err = variant_set()
        .generate_from_bytes_with_limits(&bytes, &tiny_bytes)
        .expect_err("16-byte budget must reject");
    assert!(
        matches!(err, media_kit::MediaError::TooLarge { .. }),
        "byte bomb guard: {err}"
    );

    let big = test_jpeg(256, 256);
    let tiny_dims = Limits {
        max_width: 8,
        max_height: 8,
        ..Limits::default()
    };
    let err = variant_set()
        .generate_from_bytes_with_limits(&big, &tiny_dims)
        .expect_err("8px budget must reject");
    assert!(
        matches!(err, media_kit::MediaError::DimensionsTooLarge { .. }),
        "dimension bomb guard: {err}"
    );

    // Garbage bytes never reach the decoder.
    assert!(variant_set()
        .generate_from_bytes_with_limits(b"definitely not an image", &Limits::default())
        .is_err());
}

/// Variant count + EXIF orientation: the parallel `generate` path yields one
/// output per variant, and EXIF orientation application composes (a 90°
/// rotation swaps dimensions).
#[test]
fn variant_count_and_exif_orient() {
    use image::DynamicImage;

    let bytes = test_jpeg(64, 48);
    // Our generated JPEG carries no EXIF — orientation is None, pipeline
    // leaves pixels untouched.
    let orientation = media_kit::exif::read_orientation(&bytes).expect("exif read");
    assert_eq!(orientation, None);

    let img = image::load_from_memory(&bytes).expect("decode");
    let outputs = variant_set().generate(&img).expect("parallel variants");
    assert_eq!(outputs.len(), 2, "one output per variant");
    let names: Vec<_> = outputs.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"thumb") && names.contains(&"small"));

    // EXIF orient primitive: Rotate90Cw swaps width/height.
    let wide = DynamicImage::new_rgb8(64, 32);
    let rotated = media_kit::exif::Orientation::Rotate90Cw.apply(&wide);
    assert_eq!((rotated.width(), rotated.height()), (32, 64));

    // Invalid object keys are rejected at the boundary.
    assert!(validkit::ObjectKey::parse("../escape.jpg").is_err());
    assert!(validkit::ObjectKey::parse("/absolute.jpg").is_err());
    assert!(validkit::ObjectKey::parse("uploads/photo.jpg").is_ok());
}
