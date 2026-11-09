//! Product image processing for `PUT /products/{id}/image` (spec 2.2 §8).
//!
//! The upload is sniffed from its bytes (the `Content-Type` header is not trusted): PNG, JPEG and
//! WebP are accepted, anything else is `422 invalid_image`. The image is decoded with size limits
//! (a decompression bomb is refused as `invalid_image`), scaled down to fit within
//! [`MAX_SIDE`]×[`MAX_SIDE`] keeping its aspect ratio (never scaled up), flattened onto white
//! (JPEG has no alpha) and re-encoded as JPEG, quality [`QUALITIES`] in turn until it is at most
//! [`MAX_STORED_BYTES`]; if even the lowest quality is too large, the image is scaled down by a
//! quarter and the qualities are tried again. The result is stored in the database
//! (`sultan_db::catalog::put_product_image`), so it syncs to branches with the catalog.
//!
//! Decided in C3: the stored format is always JPEG (`image/jpeg`). The `image` crate's WebP
//! encoder is lossless only, which rarely beats JPEG on photos.

use std::fmt::Write as _;
use std::io::Cursor;

use axum::http::StatusCode;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbImage};
use sha2::{Digest, Sha256};

use crate::error::ApiError;

/// Largest accepted upload: 10 MB (`413 payload_too_large` above).
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// The stored image fits within this many pixels on each side.
pub const MAX_SIDE: u32 = 512;
/// The stored image is at most this many bytes.
pub const MAX_STORED_BYTES: usize = 256 * 1024;
/// JPEG qualities tried in order.
pub const QUALITIES: [u8; 6] = [85, 75, 65, 55, 45, 35];
/// Media type of every stored image.
pub const STORED_MIME: &str = "image/jpeg";
/// Largest decoded side accepted (larger images are refused before decoding).
pub const MAX_DECODED_SIDE: u32 = 8_000;
/// Largest decoder allocation: 256 MB.
pub const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// A processed image, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    /// Width in pixels (≤ [`MAX_SIDE`]).
    pub width: u32,
    /// Height in pixels (≤ [`MAX_SIDE`]).
    pub height: u32,
    /// JPEG bytes (≤ [`MAX_STORED_BYTES`]).
    pub data: Vec<u8>,
}

/// `422 invalid_image`.
#[must_use]
pub fn invalid_image(detail: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_image", detail)
}

/// `413 payload_too_large`.
#[must_use]
pub fn too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        format!(
            "the image upload is larger than {} MB",
            MAX_UPLOAD_BYTES / 1024 / 1024
        ),
    )
}

/// Decodes, resizes and re-encodes an upload (see the module docs).
///
/// # Errors
/// `422 invalid_image` for anything that is not a decodable PNG, JPEG or WebP image.
pub fn process(bytes: &[u8]) -> Result<Processed, ApiError> {
    let format = image::guess_format(bytes)
        .map_err(|_| invalid_image("the body is not a PNG, JPEG or WebP image"))?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err(invalid_image("only PNG, JPEG and WebP images are accepted"));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODED_SIDE);
    limits.max_image_height = Some(MAX_DECODED_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|_| invalid_image("the image cannot be decoded"))?;
    let mut rgb = flatten(&fit(&decoded, MAX_SIDE));
    loop {
        for quality in QUALITIES {
            let data = encode(&rgb, quality)?;
            if data.len() <= MAX_STORED_BYTES {
                return Ok(Processed {
                    width: rgb.width(),
                    height: rgb.height(),
                    data,
                });
            }
        }
        let side = (rgb.width().max(rgb.height()) * 3 / 4).max(1);
        rgb = DynamicImage::ImageRgb8(rgb)
            .resize(side, side, FilterType::Triangle)
            .to_rgb8();
    }
}

/// [`process`] on the blocking thread pool: decoding and re-encoding are CPU-heavy and must not
/// stall the async runtime.
///
/// # Errors
/// As [`process`]; `500 internal` if the blocking task panicked.
pub async fn process_blocking(
    bytes: impl AsRef<[u8]> + Send + 'static,
) -> Result<Processed, ApiError> {
    tokio::task::spawn_blocking(move || process(bytes.as_ref()))
        .await
        .map_err(|_| ApiError::internal())?
}

/// Scales `image` down to fit within `side`×`side`, keeping the aspect ratio; never up.
fn fit(image: &DynamicImage, side: u32) -> DynamicImage {
    if image.width() > side || image.height() > side {
        image.resize(side, side, FilterType::Triangle)
    } else {
        image.clone()
    }
}

/// RGB pixels, transparent areas blended onto white.
fn flatten(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [red, green, blue, alpha] = rgba.get_pixel(x, y).0;
        let alpha = u16::from(alpha);
        let blend = |channel: u8| {
            let mixed = u16::from(channel) * alpha + 255 * (255 - alpha);
            // At most 255 * 255 / 255 = 255.
            u8::try_from(mixed / 255).unwrap_or(u8::MAX)
        };
        image::Rgb([blend(red), blend(green), blend(blue)])
    })
}

fn encode(image: &RgbImage, quality: u8) -> Result<Vec<u8>, ApiError> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(image)
        .map_err(|_| ApiError::internal())?;
    Ok(out)
}

/// The `ETag` of stored image bytes: a quoted SHA-256 prefix.
#[must_use]
pub fn etag(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut tag = String::with_capacity(34);
    tag.push('"');
    for byte in &digest[..16] {
        let _ = write!(tag, "{byte:02x}");
    }
    tag.push('"');
    tag
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn png(image: &DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    /// Pseudo-random pixels: JPEG compresses them badly.
    fn noise(width: u32, height: u32) -> DynamicImage {
        let mut state: u32 = 0x1234_5678;
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |_, _| {
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            };
            image::Rgb([next(), next(), next()])
        }))
    }

    #[test]
    fn large_images_are_resized_and_small_ones_kept() {
        let wide = DynamicImage::ImageRgb8(RgbImage::new(2_000, 1_000));
        let out = process(&png(&wide)).unwrap();
        assert_eq!((out.width, out.height), (512, 256));
        assert!(out.data.len() <= MAX_STORED_BYTES);
        assert_eq!(image::guess_format(&out.data).unwrap(), ImageFormat::Jpeg);

        let small = DynamicImage::ImageRgb8(RgbImage::new(100, 40));
        let out = process(&png(&small)).unwrap();
        assert_eq!((out.width, out.height), (100, 40));
    }

    #[test]
    fn noisy_images_are_squeezed_under_the_byte_limit() {
        let out = process(&png(&noise(512, 512))).unwrap();
        assert!(out.data.len() <= MAX_STORED_BYTES, "{}", out.data.len());
        assert!(out.width <= 512 && out.height <= 512);
    }

    #[test]
    fn transparency_is_flattened_onto_white() {
        let mut rgba = RgbaImage::new(2, 1);
        rgba.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        rgba.put_pixel(1, 0, Rgba([0, 0, 0, 255]));
        let flat = flatten(&DynamicImage::ImageRgba8(rgba));
        assert_eq!(flat.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(flat.get_pixel(1, 0).0, [0, 0, 0]);
        let out = process(&png(&DynamicImage::ImageRgba8(RgbaImage::new(3, 3)))).unwrap();
        assert_eq!((out.width, out.height), (3, 3));
    }

    #[test]
    fn non_images_and_other_formats_are_rejected() {
        for bytes in [
            b"hello world".as_slice(),
            b"GIF89a\x01\x00\x01\x00".as_slice(),
            // A PNG signature with garbage after it.
            b"\x89PNG\r\n\x1a\nnot really".as_slice(),
        ] {
            let err = process(bytes).unwrap_err();
            assert_eq!((err.status().as_u16(), err.code()), (422, "invalid_image"));
        }
        assert_eq!(too_large().status().as_u16(), 413);
    }

    #[test]
    fn images_over_the_decode_limits_are_rejected() {
        for (width, height) in [(MAX_DECODED_SIDE + 1, 1), (1, MAX_DECODED_SIDE + 1)] {
            let bytes = png(&DynamicImage::ImageLuma8(image::GrayImage::new(
                width, height,
            )));
            let err = process(&bytes).unwrap_err();
            assert_eq!((err.status().as_u16(), err.code()), (422, "invalid_image"));
        }
        // At the side limit the image is decoded and scaled down.
        let edge = png(&DynamicImage::ImageLuma8(image::GrayImage::new(
            MAX_DECODED_SIDE,
            1,
        )));
        assert_eq!(process(&edge).unwrap().width, MAX_SIDE);
    }

    #[tokio::test]
    async fn processing_runs_on_the_blocking_pool() {
        let small = png(&DynamicImage::ImageRgb8(RgbImage::new(10, 4)));
        let out = process_blocking(small).await.unwrap();
        assert_eq!((out.width, out.height), (10, 4));
        let err = process_blocking(b"nope".to_vec()).await.unwrap_err();
        assert_eq!(err.code(), "invalid_image");
    }

    #[test]
    fn etag_is_stable_and_quoted() {
        let a = etag(b"abc");
        assert_eq!(a, etag(b"abc"));
        assert_ne!(a, etag(b"abd"));
        assert!(a.starts_with('"') && a.ends_with('"'));
        assert_eq!(a.len(), 34);
    }
}
