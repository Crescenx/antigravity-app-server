use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{
    ImageDecoder, ImageFormat, ImageReader, codecs::jpeg::JpegEncoder, imageops::FilterType,
};
use serde_json::{Value, json};
use std::io::Cursor;

// Match the pinned native ReadFileTool's media budgets for user image inputs too.
const MAX_SIDE: u32 = 2576;
const MAX_ENCODED_BYTES: usize = 5 * 1024 * 1024;

pub fn image_part(bytes: Vec<u8>, declared_mime: Option<&str>) -> Result<Value> {
    ensure!(
        bytes.len() <= 20_000_000,
        "image exceeds 20,000,000 input bytes"
    );
    let format = image::guess_format(&bytes).context("unsupported image format")?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        ),
        "unsupported image format"
    );
    let mime = format.to_mime_type();
    ensure!(
        declared_mime.is_none_or(|m| m == mime),
        "image MIME type does not match its contents"
    );
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().context("invalid image")?;
    let (width, height) = decoder.dimensions();
    ensure!(
        u64::from(width) * u64::from(height) <= 64_000_000,
        "image exceeds 64 million pixels; crop or resize it first"
    );
    if width.max(height) <= MAX_SIDE && bytes.len().div_ceil(3) * 4 <= MAX_ENCODED_BYTES {
        return Ok(json!({"media":{"mimeType":mime,"data":STANDARD.encode(&bytes)}}));
    }
    let orientation = decoder.orientation()?;
    let mut source = image::DynamicImage::from_decoder(decoder).context("decoding image")?;
    source.apply_orientation(orientation);
    let mut side = MAX_SIDE.min(source.width().max(source.height()));
    loop {
        let resized = source.resize(side, side, FilterType::Lanczos3);
        let mut encoded = Cursor::new(Vec::new());
        // Preserve PNG transparency; other formats follow the native JPEG fallback.
        let output_mime = if format == ImageFormat::Png {
            resized.write_to(&mut encoded, ImageFormat::Png)?;
            "image/png"
        } else {
            JpegEncoder::new_with_quality(&mut encoded, 85).encode_image(&resized.to_rgb8())?;
            "image/jpeg"
        };
        let data = STANDARD.encode(encoded.into_inner());
        if data.len() <= MAX_ENCODED_BYTES {
            let animation = if format == ImageFormat::Gif {
                "; first GIF frame"
            } else {
                ""
            };
            return Ok(json!({"media":{"mimeType":output_mime,"data":data,
                "description":format!("Image resized from {width}x{height} to {}x{} for model input{animation}",resized.width(),resized.height())}}));
        }
        ensure!(
            side > 256,
            "image cannot fit the 5 MiB encoded budget; crop it first"
        );
        side = (side * 3 / 4).max(256);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png(width: u32, height: u32, noisy: bool) -> Vec<u8> {
        let mut seed = 17_u32;
        let img = image::RgbImage::from_fn(width, height, |_, _| {
            image::Rgb(std::array::from_fn(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if noisy { seed as u8 } else { 127 }
            }))
        });
        let mut bytes = Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }
    #[test]
    fn small_images_preserve_bytes_and_invalid_mime_is_rejected() {
        let bytes = png(16, 12, false);
        let part = image_part(bytes.clone(), Some("image/png")).unwrap();
        assert_eq!(
            STANDARD
                .decode(part["media"]["data"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        assert!(image_part(bytes, Some("image/jpeg")).is_err());
        assert!(image_part(b"%PDF-invalid-image".to_vec(), Some("image/png")).is_err());
    }
    #[test]
    fn user_images_obey_native_dimension_and_encoded_budgets() {
        for (width, height, noisy) in [(2577, 40, false), (1400, 1400, true)] {
            let source = png(width, height, noisy);
            if noisy {
                assert!(source.len().div_ceil(3) * 4 > MAX_ENCODED_BYTES);
            }
            let part = image_part(source, None).unwrap();
            let data = part["media"]["data"].as_str().unwrap();
            assert!(data.len() <= MAX_ENCODED_BYTES);
            let img = image::load_from_memory(&STANDARD.decode(data).unwrap()).unwrap();
            assert!(img.width() <= MAX_SIDE && img.height() <= MAX_SIDE);
            if noisy {
                assert!(img.width() < width);
            }
            assert!(
                part["media"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("resized")
            );
        }
    }
}
