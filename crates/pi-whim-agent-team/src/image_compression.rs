use std::io::Cursor;

use image::{
    DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder, ImageFormat, ImageReader,
    Limits,
    codecs::{jpeg::JpegEncoder, webp::WebPEncoder},
    imageops::FilterType,
};

const MIN_JPEG_QUALITY: u8 = 40;
const MAX_JPEG_QUALITY: u8 = 90;
const SIZE_MARGIN: f64 = 0.92;
const MAX_ATTEMPTS: usize = 12;
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) struct CompressedImage {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub quality: Option<u8>,
    pub has_transparency: bool,
}

pub(crate) fn compress_to_limit(
    bytes: &[u8],
    mime_type: &str,
    limit: usize,
) -> Result<CompressedImage, String> {
    let format = format_for_mime(mime_type)
        .ok_or_else(|| format!("unsupported image format {mime_type}"))?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    let source = reader
        .decode()
        .map_err(|error| format!("could not decode {mime_type}: {error}"))?;
    let (original_width, original_height) = source.dimensions();
    let has_transparency =
        source.has_alpha() && source.to_rgba8().pixels().any(|pixel| pixel.0[3] < u8::MAX);
    let mut width = original_width;
    let mut height = original_height;

    for _ in 0..MAX_ATTEMPTS {
        let resized = (width != original_width || height != original_height)
            .then(|| source.resize(width, height, FilterType::Lanczos3));
        let candidate = resized.as_ref().unwrap_or(&source);
        let (candidate_width, candidate_height) = candidate.dimensions();
        let encoded = if has_transparency {
            let bytes = encode_lossless_webp(candidate)?;
            if bytes.len() <= limit {
                return Ok(CompressedImage {
                    bytes,
                    mime_type: "image/webp",
                    original_width,
                    original_height,
                    width: candidate_width,
                    height: candidate_height,
                    quality: None,
                    has_transparency: true,
                });
            }
            bytes
        } else {
            let lowest_quality = encode_jpeg(candidate, MIN_JPEG_QUALITY)?;
            if lowest_quality.len() <= limit {
                let (bytes, quality) = best_jpeg_within_limit(candidate, limit, lowest_quality)?;
                return Ok(CompressedImage {
                    bytes,
                    mime_type: "image/jpeg",
                    original_width,
                    original_height,
                    width: candidate_width,
                    height: candidate_height,
                    quality: Some(quality),
                    has_transparency: false,
                });
            }
            lowest_quality
        };
        let next = scaled_dimensions(candidate_width, candidate_height, encoded.len(), limit);
        if next == (candidate_width, candidate_height) {
            break;
        }
        (width, height) = next;
    }

    Err(format!(
        "could not compress the complete image below {limit} bytes"
    ))
}

fn best_jpeg_within_limit(
    image: &DynamicImage,
    limit: usize,
    initial: Vec<u8>,
) -> Result<(Vec<u8>, u8), String> {
    let mut best = (initial, MIN_JPEG_QUALITY);
    let mut low = MIN_JPEG_QUALITY + 1;
    let mut high = MAX_JPEG_QUALITY;
    while low <= high {
        let quality = low + (high - low) / 2;
        let encoded = encode_jpeg(image, quality)?;
        if encoded.len() <= limit {
            best = (encoded, quality);
            low = quality.saturating_add(1);
        } else {
            high = quality.saturating_sub(1);
        }
    }
    Ok(best)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let rgb = image.to_rgb8();
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, quality)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| format!("could not encode JPEG: {error}"))?;
    Ok(output)
}

fn encode_lossless_webp(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let rgba = image.to_rgba8();
    let mut output = Vec::new();
    WebPEncoder::new_lossless(&mut output)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|error| format!("could not encode lossless WebP: {error}"))?;
    Ok(output)
}

fn scaled_dimensions(width: u32, height: u32, encoded_bytes: usize, limit: usize) -> (u32, u32) {
    if width == 1 && height == 1 {
        return (width, height);
    }
    let estimated = ((limit as f64 / encoded_bytes as f64).sqrt() * SIZE_MARGIN).clamp(0.25, 0.9);
    let next_width = ((width as f64 * estimated).floor() as u32)
        .max(1)
        .min(width.saturating_sub(1).max(1));
    let next_height = ((height as f64 * estimated).floor() as u32)
        .max(1)
        .min(height.saturating_sub(1).max(1));
    (next_width, next_height)
}

fn format_for_mime(mime_type: &str) -> Option<ImageFormat> {
    match mime_type {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::WebP),
        "image/bmp" => Some(ImageFormat::Bmp),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage, codecs::png::PngEncoder};

    #[test]
    fn transparent_images_keep_alpha_and_full_aspect_ratio() {
        let image = RgbaImage::from_fn(160, 96, |x, y| {
            let alpha = if x < 40 || y < 16 { 0 } else { 255 };
            Rgba([
                x.wrapping_mul(37) as u8,
                y.wrapping_mul(53) as u8,
                x.wrapping_mul(y).wrapping_mul(11) as u8,
                alpha,
            ])
        });
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .unwrap();

        let compressed = compress_to_limit(&png, "image/png", 4 * 1024).unwrap();
        assert_eq!(compressed.mime_type, "image/webp");
        assert!(compressed.bytes.len() <= 4 * 1024);
        assert!(compressed.has_transparency);
        let ratio_error = (u64::from(compressed.width) * u64::from(compressed.original_height))
            .abs_diff(u64::from(compressed.height) * u64::from(compressed.original_width));
        assert!(
            ratio_error <= u64::from(compressed.original_width.max(compressed.original_height))
        );
        let decoded = image::load_from_memory_with_format(&compressed.bytes, ImageFormat::WebP)
            .unwrap()
            .to_rgba8();
        assert!(decoded.pixels().any(|pixel| pixel.0[3] < u8::MAX));
    }
}
