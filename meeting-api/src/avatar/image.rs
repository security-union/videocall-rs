/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! Decode, normalize and re-encode avatar images.

use std::io::Cursor;

use ::image::codecs::jpeg::JpegEncoder;
use ::image::imageops::{self, FilterType};
use ::image::metadata::Orientation;
use ::image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits,
    Rgb, RgbImage,
};
use sha2::{Digest, Sha256};
use videocall_meeting_types::avatar::{
    ALLOWED_AVATAR_UPLOAD_CONTENT_TYPES, AVATAR_OUTPUT_DIMENSION, MAX_AVATAR_INPUT_DIMENSION,
    MAX_AVATAR_STORED_BYTES, MIN_AVATAR_DIMENSION,
};

/// Content type of every image this module produces.
pub const AVATAR_CONTENT_TYPE: &str = "image/jpeg";

const MAX_DECODE_ALLOC: u64 = 16 * 1024 * 1024;
const BACKGROUND: [u8; 3] = [0x3a, 0x3a, 0x3a];
const JPEG_QUALITIES: [u8; 2] = [85, 70];

/// An accepted input format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    WebP,
}

impl ImageKind {
    /// Parses a `Content-Type` header value, ignoring parameters.
    pub fn from_content_type(value: &str) -> Option<Self> {
        let essence = value.split(';').next().unwrap_or_default().trim();
        ALLOWED_AVATAR_UPLOAD_CONTENT_TYPES
            .into_iter()
            .zip([Self::Png, Self::Jpeg, Self::WebP])
            .find(|(name, _)| essence.eq_ignore_ascii_case(name))
            .map(|(_, kind)| kind)
    }

    fn from_format(format: ImageFormat) -> Option<Self> {
        match format {
            ImageFormat::Png => Some(Self::Png),
            ImageFormat::Jpeg => Some(Self::Jpeg),
            ImageFormat::WebP => Some(Self::WebP),
            _ => None,
        }
    }

    fn format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::WebP => ImageFormat::WebP,
        }
    }
}

/// Why an image was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarImageError {
    /// Not a recognizable image, or a format outside PNG/JPEG/WebP with nothing declared.
    UnknownFormat,
    /// A recognizable format that differs from the declared `Content-Type`.
    TypeMismatch,
    /// Wider or taller than [`MAX_AVATAR_INPUT_DIMENSION`], or over the decode memory budget.
    TooLarge,
    /// Narrower or shorter than [`MIN_AVATAR_DIMENSION`].
    TooSmall,
    /// The bitstream did not decode.
    Corrupt,
    /// Even the lowest JPEG quality exceeded the size budget.
    EncodedTooLarge,
}

impl AvatarImageError {
    /// Stable reason code for logs.
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnknownFormat => "unknown_format",
            Self::TypeMismatch => "type_mismatch",
            Self::TooLarge => "too_large",
            Self::TooSmall => "too_small",
            Self::Corrupt => "corrupt",
            Self::EncodedTooLarge => "encoded_too_large",
        }
    }
}

/// Decodes `bytes`, flattens it to a still square, and re-encodes it as a JPEG.
///
/// With `declared`, the sniffed format must match it.
pub fn process_avatar(
    bytes: &[u8],
    declared: Option<ImageKind>,
) -> Result<Vec<u8>, AvatarImageError> {
    let sniffed = ::image::guess_format(bytes).map_err(|_| AvatarImageError::UnknownFormat)?;
    let kind = match (ImageKind::from_format(sniffed), declared) {
        (Some(kind), None) => kind,
        (Some(kind), Some(declared)) if kind == declared => kind,
        (_, Some(_)) => return Err(AvatarImageError::TypeMismatch),
        (None, None) => return Err(AvatarImageError::UnknownFormat),
    };
    let image = decode(bytes, kind)?;
    let (width, height) = (image.width(), image.height());
    if width.min(height) < MIN_AVATAR_DIMENSION {
        return Err(AvatarImageError::TooSmall);
    }
    let square = square_thumbnail(&flatten(image));
    encode_within(&square, MAX_AVATAR_STORED_BYTES)
}

/// Strong validator for a stored image: the first 16 bytes of its SHA-256, as hex.
pub fn etag_for(image: &[u8]) -> String {
    super::hex(&Sha256::digest(image)[..16])
}

fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_AVATAR_INPUT_DIMENSION);
    limits.max_image_height = Some(MAX_AVATAR_INPUT_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

fn decode(bytes: &[u8], kind: ImageKind) -> Result<DynamicImage, AvatarImageError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), kind.format());
    reader.limits(decode_limits());
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    decode_limits()
        .reserve(decoder.total_bytes())
        .map_err(decode_error)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn decode_error(error: ImageError) -> AvatarImageError {
    match error {
        ImageError::Limits(_) => AvatarImageError::TooLarge,
        _ => AvatarImageError::Corrupt,
    }
}

fn flatten(image: DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.into_rgb8();
    }
    let rgba = image.into_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend = |c: u8, bg: u8| {
            let (c, bg, a) = (u32::from(c), u32::from(bg), u32::from(a));
            ((c * a + bg * (255 - a) + 127) / 255) as u8
        };
        Rgb([
            blend(r, BACKGROUND[0]),
            blend(g, BACKGROUND[1]),
            blend(b, BACKGROUND[2]),
        ])
    })
}

fn square_thumbnail(image: &RgbImage) -> RgbImage {
    let (width, height) = image.dimensions();
    let side = width.min(height);
    let square =
        imageops::crop_imm(image, (width - side) / 2, (height - side) / 2, side, side).to_image();
    let target = side.min(AVATAR_OUTPUT_DIMENSION);
    if target == side {
        square
    } else {
        imageops::resize(&square, target, target, FilterType::CatmullRom)
    }
}

fn encode_within(image: &RgbImage, max_bytes: usize) -> Result<Vec<u8>, AvatarImageError> {
    for quality in JPEG_QUALITIES {
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, quality)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .map_err(|_| AvatarImageError::Corrupt)?;
        if jpeg.len() <= max_bytes {
            return Ok(jpeg);
        }
    }
    Err(AvatarImageError::EncodedTooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::codecs::png::PngEncoder;
    use ::image::{ImageEncoder, RgbaImage};

    const PNG: &[u8] = include_bytes!("../../tests/fixtures/avatars/ok-64.png");
    const JPEG: &[u8] = include_bytes!("../../tests/fixtures/avatars/ok-64.jpg");
    const WEBP: &[u8] = include_bytes!("../../tests/fixtures/avatars/ok-64.webp");
    const WEBP_LOSSLESS: &[u8] = include_bytes!("../../tests/fixtures/avatars/ok-64-lossless.webp");
    const GIF: &[u8] = include_bytes!("../../tests/fixtures/avatars/ok-64.gif");
    const APNG: &[u8] = include_bytes!("../../tests/fixtures/avatars/anim.png");
    const ANIMATED_WEBP: &[u8] = include_bytes!("../../tests/fixtures/avatars/anim.webp");
    const BIG: &[u8] = include_bytes!("../../tests/fixtures/avatars/big-1100.png");
    const TINY: &[u8] = include_bytes!("../../tests/fixtures/avatars/tiny-8.png");
    const BOMB: &[u8] = include_bytes!("../../tests/fixtures/avatars/bomb-20000.png");
    const EXIF_ROTATED: &[u8] = include_bytes!("../../tests/fixtures/avatars/exif-rot6.jpg");

    fn decoded(jpeg: &[u8]) -> RgbImage {
        assert_eq!(&jpeg[..3], &[0xff, 0xd8, 0xff], "output must be a JPEG");
        ::image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg)
            .expect("output decodes")
            .into_rgb8()
    }

    fn png_of(image: &RgbaImage) -> Vec<u8> {
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .unwrap();
        out
    }

    fn close(actual: u8, expected: u8) -> bool {
        actual.abs_diff(expected) <= 6
    }

    #[test]
    fn every_accepted_format_is_reencoded_as_a_jpeg() {
        for (bytes, kind) in [
            (PNG, ImageKind::Png),
            (JPEG, ImageKind::Jpeg),
            (WEBP, ImageKind::WebP),
            (WEBP_LOSSLESS, ImageKind::WebP),
        ] {
            let out = process_avatar(bytes, Some(kind)).expect("fixture is accepted");
            assert_ne!(out, bytes);
            assert_eq!(decoded(&out).dimensions(), (64, 64), "{kind:?}");
        }
    }

    #[test]
    fn declared_type_must_match_the_bytes() {
        assert_eq!(
            process_avatar(JPEG, Some(ImageKind::Png)),
            Err(AvatarImageError::TypeMismatch)
        );
        assert_eq!(
            process_avatar(GIF, Some(ImageKind::Png)),
            Err(AvatarImageError::TypeMismatch)
        );
        assert_eq!(
            process_avatar(GIF, None),
            Err(AvatarImageError::UnknownFormat)
        );
    }

    #[test]
    fn markup_is_not_an_image() {
        for bytes in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"[..],
            b"<!DOCTYPE html><script>alert(1)</script>",
            b"",
        ] {
            assert_eq!(
                process_avatar(bytes, Some(ImageKind::Png)),
                Err(AvatarImageError::UnknownFormat)
            );
        }
    }

    #[test]
    fn dimensions_outside_the_bounds_are_refused() {
        assert_eq!(
            process_avatar(BIG, Some(ImageKind::Png)),
            Err(AvatarImageError::TooLarge)
        );
        assert_eq!(
            process_avatar(BOMB, Some(ImageKind::Png)),
            Err(AvatarImageError::TooLarge)
        );
        assert_eq!(
            process_avatar(TINY, Some(ImageKind::Png)),
            Err(AvatarImageError::TooSmall)
        );
    }

    #[test]
    fn truncated_input_is_corrupt_not_a_panic() {
        assert_eq!(
            process_avatar(&PNG[..PNG.len() / 2], Some(ImageKind::Png)),
            Err(AvatarImageError::Corrupt)
        );
        assert_eq!(
            process_avatar(&JPEG[..40], Some(ImageKind::Jpeg)),
            Err(AvatarImageError::Corrupt)
        );
    }

    #[test]
    fn animations_become_their_first_frame() {
        for (bytes, kind) in [(APNG, ImageKind::Png), (ANIMATED_WEBP, ImageKind::WebP)] {
            let out = decoded(&process_avatar(bytes, Some(kind)).unwrap());
            let [r, _, b] = out.get_pixel(32, 32).0;
            assert!(r > 150 && b < 100, "{kind:?} kept frame 1 (red): {r} {b}");
        }
    }

    #[test]
    fn trailing_bytes_do_not_survive() {
        let mut polyglot = PNG.to_vec();
        polyglot.extend_from_slice(b"<script>alert(1)</script>");
        let out = process_avatar(&polyglot, Some(ImageKind::Png)).unwrap();
        assert!(!out.windows(7).any(|w| w == b"<script"));
    }

    #[test]
    fn exif_is_applied_then_dropped() {
        assert!(EXIF_ROTATED.windows(4).any(|w| w == b"Exif"));
        let out = process_avatar(EXIF_ROTATED, Some(ImageKind::Jpeg)).unwrap();
        assert!(!out.windows(4).any(|w| w == b"Exif"));
        let image = decoded(&out);
        assert_eq!(image.dimensions(), (32, 32));
        let left = image.get_pixel(4, 16).0;
        let right = image.get_pixel(27, 16).0;
        assert!(left[2] > 150 && left[0] < 100, "left is blue: {left:?}");
        assert!(right[0] > 150 && right[2] < 100, "right is red: {right:?}");
    }

    #[test]
    fn transparency_is_flattened_onto_the_tile_background() {
        let transparent = RgbaImage::from_pixel(32, 32, ::image::Rgba([255, 0, 0, 0]));
        let out = decoded(&process_avatar(&png_of(&transparent), Some(ImageKind::Png)).unwrap());
        for pixel in out.pixels() {
            assert!(pixel.0.iter().all(|&c| close(c, 0x3a)), "{pixel:?}");
        }
    }

    #[test]
    fn large_inputs_are_center_cropped_and_downscaled() {
        let mut wide = RgbaImage::from_pixel(600, 300, ::image::Rgba([0, 0, 255, 255]));
        for y in 0..300 {
            for x in 150..450 {
                wide.put_pixel(x, y, ::image::Rgba([0, 255, 0, 255]));
            }
        }
        let out = decoded(&process_avatar(&png_of(&wide), Some(ImageKind::Png)).unwrap());
        assert_eq!(out.dimensions(), (256, 256));
        for (x, y) in [(2, 2), (253, 253), (128, 128)] {
            let [r, g, b] = out.get_pixel(x, y).0;
            assert!(close(r, 0) && close(g, 255) && close(b, 0), "({x},{y})");
        }
    }

    #[test]
    fn encoding_falls_back_to_a_lower_quality_then_gives_up() {
        let noise = RgbImage::from_fn(64, 64, |x, y| {
            let v = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) as u8;
            Rgb([v, v.rotate_left(3), v.rotate_left(5)])
        });
        let first = encode_within(&noise, usize::MAX).unwrap();
        let fallback = encode_within(&noise, first.len() - 1).unwrap();
        assert!(fallback.len() < first.len());
        assert_eq!(
            encode_within(&noise, fallback.len() - 1),
            Err(AvatarImageError::EncodedTooLarge)
        );
    }

    #[test]
    fn content_type_parsing_ignores_parameters_and_case_only() {
        assert_eq!(
            ImageKind::from_content_type("image/png"),
            Some(ImageKind::Png)
        );
        assert_eq!(
            ImageKind::from_content_type(" Image/JPEG ; q=1"),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(
            ImageKind::from_content_type("image/webp"),
            Some(ImageKind::WebP)
        );
        for rejected in [
            "image/jpg",
            "image/gif",
            "image/svg+xml",
            "multipart/form-data",
            "",
        ] {
            assert_eq!(ImageKind::from_content_type(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn etag_is_the_first_half_of_the_sha256() {
        assert_eq!(etag_for(b"abc"), "ba7816bf8f01cfea414140de5dae2223");
    }
}
