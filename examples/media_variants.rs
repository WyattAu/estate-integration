#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Living example: bytes → sniff → variants → content address.
//!
//! ```sh
//! cargo run --example media_variants
//! ```

use media_kit::encode::OutFormat;
use media_kit::resize::Fit;
use media_kit::variants::{Variant, VariantSet};

fn main() {
    // In-memory test image (no fixtures on disk).
    let img = image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8, y as u8, 128]));
    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new(&mut jpeg);
    use image::ImageEncoder;
    encoder
        .write_image(&img, 64, 64, image::ExtendedColorType::Rgb8)
        .expect("encode");

    let key = validkit::ObjectKey::parse("uploads/photo.jpg").expect("key");
    let format = media_kit::sniff::sniff(&jpeg).expect("sniffable");
    println!("upload {key} sniffed as {format:?}");

    let set = VariantSet::new()
        .with(Variant::new("thumb", Fit::MaxSide(32), OutFormat::Jpeg(80)))
        .with(Variant::new("small", Fit::Width(48), OutFormat::Png));
    let decoded = image::load_from_memory(&jpeg).expect("decode");
    for (name, _format, bytes) in set.generate(&decoded).expect("variants") {
        println!("variant {name}: {} bytes", bytes.len());
    }

    let (_tmp, cas) = cas_kit::store::BlobStore::open_in_memory().expect("cas");
    let hash = cas.put_blob(&jpeg).expect("put");
    println!("content address: {hash}");
}
