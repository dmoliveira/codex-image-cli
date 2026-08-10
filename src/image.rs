use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;

use crate::{api::TokenUsage, cli::OutputFormat, report::AppError};

/// Cap each decoded artifact so a malicious or misconfigured endpoint cannot
/// exhaust local memory/disk. Users needing larger assets can choose JPEG/WebP
/// or lower resolution; the cap is intentionally documented.
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct ImageResponse {
    data: Vec<ImageData>,
}

#[derive(Debug, Deserialize)]
struct ImageData {
    b64_json: Option<String>,
}

pub fn decode_images(
    body: &[u8],
    expected_count: u8,
    format: OutputFormat,
) -> Result<Vec<Vec<u8>>, AppError> {
    let response: ImageResponse = serde_json::from_slice(body).map_err(|_| {
        AppError::invalid_response(
            "invalid_json_response",
            "The successful API response was not valid image JSON. The request may have been billed; do not retry automatically.",
        )
    })?;
    if response.data.len() != usize::from(expected_count) {
        return Err(AppError::invalid_response(
            "unexpected_image_count",
            format!(
                "The API returned {} image records but {} were requested. No files were published; the request may have been billed.",
                response.data.len(),
                expected_count
            ),
        ));
    }

    response
        .data
        .into_iter()
        .map(|image| decode_one(image.b64_json, format))
        .collect()
}

/// Extract response usage without making image decoding depend on the
/// optional accounting metadata. A malformed or usage-less response simply
/// returns no usage and is handled by the caller's normal response path.
pub fn extract_usage(body: &[u8]) -> Option<TokenUsage> {
    #[derive(Deserialize)]
    struct UsageEnvelope {
        usage: Option<TokenUsage>,
    }

    serde_json::from_slice::<UsageEnvelope>(body)
        .ok()
        .and_then(|response| response.usage)
}

fn decode_one(encoded: Option<String>, format: OutputFormat) -> Result<Vec<u8>, AppError> {
    let encoded = encoded.ok_or_else(|| {
        AppError::invalid_response(
            "missing_base64_image",
            "The API did not return base64 image data. URL-only artifacts are intentionally refused; the request may have been billed.",
        )
    })?;
    // A base64 string is at least 4/3 of its decoded payload. Check the input
    // first to avoid allocating a massive decoded buffer.
    if encoded.len() > MAX_IMAGE_BYTES.saturating_mul(4) / 3 + 4 {
        return Err(AppError::invalid_response(
            "image_too_large",
            "A returned image exceeded the local 32 MiB safety limit. No files were published; the request may have been billed.",
        ));
    }
    let decoded = STANDARD.decode(encoded.as_bytes()).map_err(|_| {
        AppError::invalid_response(
            "invalid_base64_image",
            "A returned image was not valid base64. No files were published; the request may have been billed.",
        )
    })?;
    if decoded.len() > MAX_IMAGE_BYTES {
        return Err(AppError::invalid_response(
            "image_too_large",
            "A returned image exceeded the local 32 MiB safety limit. No files were published; the request may have been billed.",
        ));
    }
    validate_image_bytes(decoded, format)
}

/// Decode one already-parsed image record without reserializing its JSON body.
pub fn decode_base64_image(
    encoded: Option<String>,
    format: OutputFormat,
) -> Result<Vec<u8>, AppError> {
    decode_one(encoded, format)
}

pub fn validate_image_bytes(bytes: Vec<u8>, format: OutputFormat) -> Result<Vec<u8>, AppError> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(AppError::invalid_response(
            "image_too_large",
            "A returned image exceeded the local 32 MiB safety limit. No files were published; the request may have been billed.",
        ));
    }
    if !matches_format(&bytes, format) {
        return Err(AppError::invalid_response(
            "image_format_mismatch",
            "A returned image did not match the requested container format. No files were published; the request may have been billed.",
        ));
    }
    Ok(bytes)
}

fn matches_format(bytes: &[u8], format: OutputFormat) -> bool {
    match format {
        OutputFormat::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        OutputFormat::Jpeg => {
            bytes.len() >= 4
                && bytes.starts_with(&[0xff, 0xd8, 0xff])
                && bytes.ends_with(&[0xff, 0xd9])
        }
        OutputFormat::Webp => {
            if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
                return false;
            }
            let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
            declared.saturating_add(8) <= bytes.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nmock";

    #[test]
    fn validates_png_payloads() {
        let body = serde_json::json!({
            "data": [{"b64_json": STANDARD.encode(PNG)}]
        });
        let images =
            decode_images(&serde_json::to_vec(&body).unwrap(), 1, OutputFormat::Png).unwrap();
        assert_eq!(images, vec![PNG.to_vec()]);
    }

    #[test]
    fn rejects_mismatched_magic_bytes_before_writing() {
        let body = serde_json::json!({
            "data": [{"b64_json": STANDARD.encode(b"not a PNG")}]
        });
        let error =
            decode_images(&serde_json::to_vec(&body).unwrap(), 1, OutputFormat::Png).unwrap_err();
        assert_eq!(error.code, "image_format_mismatch");
    }

    #[test]
    fn validates_jpeg_and_webp_containers() {
        assert!(matches_format(
            &[0xff, 0xd8, 0xff, 0x00, 0xff, 0xd9],
            OutputFormat::Jpeg
        ));
        assert!(matches_format(
            b"RIFF\x04\0\0\0WEBPdata",
            OutputFormat::Webp
        ));
        assert!(!matches_format(
            b"RIFF\x80\0\0\0WEBPdata",
            OutputFormat::Webp
        ));
    }
}
