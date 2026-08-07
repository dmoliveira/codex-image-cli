use std::{io::Read, time::Duration};

use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
    redirect::Policy,
};
use serde::Serialize;

use crate::{cli::GenerateArgs, endpoint::Endpoint, report::AppError, MODEL};

/// Bound the full JSON response before parsing it. This protects callers from
/// a compatible endpoint returning an unexpectedly large body.
pub const MAX_RESPONSE_BYTES: usize = 180 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

#[derive(Debug, Serialize)]
pub struct ImageGenerationRequest<'a> {
    model: &'static str,
    prompt: &'a str,
    n: u8,
    size: &'a str,
    quality: &'static str,
    background: &'static str,
    output_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_compression: Option<u8>,
    moderation: &'static str,
}

impl<'a> ImageGenerationRequest<'a> {
    pub fn from_args(prompt: &'a str, args: &'a GenerateArgs) -> Self {
        Self {
            model: MODEL,
            prompt,
            n: args.n,
            size: &args.size,
            quality: args.quality.as_api_value(),
            background: args.background.as_api_value(),
            output_format: args.format.as_api_value(),
            output_compression: args.compression,
            moderation: args.moderation.as_api_value(),
        }
    }
}

pub struct ApiClient {
    client: Client,
}

pub struct ApiResponse {
    pub body: Vec<u8>,
    pub request_id: Option<String>,
    pub status: u16,
}

impl ApiClient {
    pub fn new(timeout_seconds: u64) -> Result<Self, AppError> {
        let timeout = Duration::from_secs(timeout_seconds);
        let client = Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .redirect(Policy::none())
            // A proxy can silently receive an Authorization header. The CLI
            // uses a direct connection so the selected endpoint is the only
            // credential destination.
            .no_proxy()
            .user_agent(format!("codex-image-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| {
                AppError::preflight(
                    "http_client_unavailable",
                    "The local HTTP client could not be initialized; no image request was sent.",
                )
            })?;
        Ok(Self { client })
    }

    /// Sends exactly one POST. Transport and server failures are deliberately
    /// reported as indeterminate instead of retried: a billable generation may
    /// have been accepted even when no usable response reaches this process.
    pub fn generate(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        request: &ImageGenerationRequest<'_>,
    ) -> Result<ApiResponse, AppError> {
        let response = self
            .client
            .post(endpoint.url().clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .bearer_auth(api_key)
            .json(request)
            .send()
            .map_err(|_| {
                AppError::indeterminate(
                    "transport_outcome_unknown",
                    "The image POST may have been processed, but no complete response was received. Billing and generation outcome are unknown; do not retry automatically.",
                )
            })?;

        let status = response.status();
        let request_id = request_id(response.headers(), api_key);
        if status.is_redirection() {
            return Err(AppError::indeterminate(
                "redirect_outcome_unknown",
                "The endpoint returned a redirect after the image POST. Redirects are refused so OPENAI_API_KEY is never forwarded, but billing and generation outcome may be unknown; do not retry automatically.",
            )
            .with_http(status.as_u16(), request_id));
        }
        if status.is_client_error() {
            let code = safe_error_code(response, api_key);
            return Err(AppError::api_rejected(
                "api_rejected",
                format!(
                    "The API rejected the request (HTTP {}; {}).",
                    status.as_u16(),
                    code.unwrap_or_else(|| "no safe error code returned".to_owned())
                ),
                status.as_u16(),
                request_id,
            ));
        }
        if status.is_server_error() {
            return Err(AppError::indeterminate(
                "server_outcome_unknown",
                "The endpoint returned a server error after the image POST. Billing and generation outcome may be unknown; do not retry automatically.",
            )
            .with_http(status.as_u16(), request_id));
        }
        if !status.is_success() {
            return Err(AppError::indeterminate(
                "unexpected_http_status",
                "The endpoint returned an unexpected response after the image POST. Billing and generation outcome may be unknown; do not retry automatically.",
            )
            .with_http(status.as_u16(), request_id));
        }

        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(AppError::invalid_response(
                "response_too_large",
                "The successful response exceeded the local safety limit before it could be parsed. The image request may have been billed; do not retry automatically.",
            )
            .with_http(status.as_u16(), request_id));
        }

        let body = read_bounded(response, MAX_RESPONSE_BYTES).map_err(|_| {
            AppError::indeterminate(
                "response_read_outcome_unknown",
                "The image POST may have been processed, but the successful response could not be read completely. Billing and generation outcome are unknown; do not retry automatically.",
            )
            .with_http(status.as_u16(), request_id.clone())
        })?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(AppError::invalid_response(
                "response_too_large",
                "The successful response exceeded the local safety limit. The image request may have been billed; do not retry automatically.",
            )
            .with_http(status.as_u16(), request_id));
        }
        Ok(ApiResponse {
            body,
            request_id,
            status: status.as_u16(),
        })
    }
}

fn request_id(headers: &HeaderMap, api_key: &str) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .filter(|value| {
            value.len() <= 256
                && !value.contains(api_key)
                && value.chars().all(|character| !character.is_control())
        })
}

fn read_bounded(mut response: reqwest::blocking::Response, max: usize) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    response
        .by_ref()
        .take((max as u64).saturating_add(1))
        .read_to_end(&mut body)?;
    Ok(body)
}

#[derive(serde::Deserialize)]
struct ErrorEnvelope {
    error: Option<SafeError>,
}

#[derive(serde::Deserialize)]
struct SafeError {
    code: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Read an error response only to extract a short identifier. Arbitrary server
/// messages are intentionally not echoed because compatible endpoints can
/// reflect credentials or other sensitive text.
fn safe_error_code(response: reqwest::blocking::Response, api_key: &str) -> Option<String> {
    let mut body = Vec::new();
    let _ = response.take(MAX_ERROR_BODY_BYTES).read_to_end(&mut body);
    let envelope = serde_json::from_slice::<ErrorEnvelope>(&body).ok()?;
    let candidate = envelope.error.and_then(|error| error.code.or(error.kind))?;
    if candidate.contains(api_key)
        || candidate.len() > 80
        || !candidate.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        return None;
    }
    Some(candidate)
}

trait ErrorContext {
    fn with_http(self, http_status: u16, request_id: Option<String>) -> AppError;
}

impl ErrorContext for AppError {
    fn with_http(mut self, http_status: u16, request_id: Option<String>) -> AppError {
        self.set_http_status(http_status);
        self.set_request_id(request_id);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_gpt_image_two_and_omits_empty_compression() {
        let args = GenerateArgs {
            prompt: Some("test".to_owned()),
            prompt_file: None,
            n: 1,
            output_dir: ".".into(),
            name: None,
            prefix: None,
            format: crate::cli::OutputFormat::Png,
            size: "auto".to_owned(),
            quality: crate::cli::Quality::Auto,
            background: crate::cli::Background::Auto,
            compression: None,
            moderation: crate::cli::Moderation::Auto,
            overwrite: false,
            dry_run: false,
            timeout_seconds: 180,
            api_base_url: "https://api.openai.com/v1".to_owned(),
            dangerously_allow_api_key_to: None,
            allow_insecure_localhost: false,
        };
        let body = serde_json::to_value(ImageGenerationRequest::from_args("test", &args)).unwrap();
        assert_eq!(body["model"], "gpt-image-2");
        assert!(body.get("output_compression").is_none());
    }
}
