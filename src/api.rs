use std::{io::Read, time::Duration};

use reqwest::{
    blocking::multipart::{Form, Part},
    blocking::Client,
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
    redirect::Policy,
    Method,
};
use serde::{Deserialize, Serialize};

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
        Self::from_args_with_count(prompt, args, args.n)
    }

    pub fn from_args_with_count(prompt: &'a str, args: &'a GenerateArgs, count: u8) -> Self {
        Self {
            model: MODEL,
            prompt,
            n: count,
            size: &args.size,
            quality: args.quality.as_api_value(),
            background: args.background.as_api_value(),
            output_format: args.format.as_api_value(),
            output_compression: args.compression,
            moderation: args.moderation.as_api_value(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BatchCreateRequest<'a> {
    pub input_file_id: &'a str,
    pub endpoint: &'static str,
    pub completion_window: &'static str,
    pub output_expires_after: OutputExpiresAfter,
}

#[derive(Debug, Serialize)]
pub struct OutputExpiresAfter {
    pub anchor: &'static str,
    pub seconds: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BatchInfo {
    pub id: String,
    pub status: String,
    pub input_file_id: String,
    pub endpoint: Option<String>,
    pub completion_window: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub request_counts: Option<BatchRequestCounts>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BatchRequestCounts {
    pub completed: u32,
    pub failed: u32,
    pub total: u32,
}

#[derive(Debug, Deserialize)]
pub struct FileInfo {
    pub id: String,
    pub purpose: Option<String>,
}

pub struct ApiClient {
    client: Client,
}

#[derive(Debug)]
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

    pub fn upload_batch_input(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        input: Vec<u8>,
    ) -> Result<ApiResponse, AppError> {
        let url = endpoint.files_url()?;
        let part = Part::bytes(input)
            .file_name("batch-input.jsonl")
            .mime_str("application/jsonl")
            .map_err(|_| {
                AppError::preflight(
                    "multipart_unavailable",
                    "The batch input could not be prepared.",
                )
            })?;
        let form = Form::new().text("purpose", "batch").part("file", part);
        self.send_request(
            Method::POST,
            url,
            api_key,
            RequestPayload::Multipart(form),
            RequestKind::BillablePost,
            MAX_BATCH_RESPONSE_BYTES,
        )
    }

    pub fn create_batch(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        request: &BatchCreateRequest<'_>,
    ) -> Result<ApiResponse, AppError> {
        self.send_json(
            Method::POST,
            endpoint.batches_url()?,
            api_key,
            request,
            RequestKind::BillablePost,
            MAX_BATCH_RESPONSE_BYTES,
        )
    }

    pub fn get_batch(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        batch_id: &str,
    ) -> Result<ApiResponse, AppError> {
        self.send_request(
            Method::GET,
            endpoint.batch_url(batch_id)?,
            api_key,
            RequestPayload::Empty,
            RequestKind::Observation,
            MAX_BATCH_RESPONSE_BYTES,
        )
    }

    pub fn get_file_content(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        file_id: &str,
    ) -> Result<ApiResponse, AppError> {
        self.send_request(
            Method::GET,
            endpoint.file_content_url(file_id)?,
            api_key,
            RequestPayload::Empty,
            RequestKind::FileContent,
            MAX_BATCH_CONTENT_BYTES,
        )
    }

    pub fn get_input_file_content(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        file_id: &str,
    ) -> Result<ApiResponse, AppError> {
        self.send_request(
            Method::GET,
            endpoint.file_content_url(file_id)?,
            api_key,
            RequestPayload::Empty,
            RequestKind::InputFileContent,
            MAX_BATCH_INPUT_BYTES,
        )
    }

    pub fn get_file(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        file_id: &str,
    ) -> Result<ApiResponse, AppError> {
        self.send_request(
            Method::GET,
            endpoint.file_url(file_id)?,
            api_key,
            RequestPayload::Empty,
            RequestKind::Observation,
            MAX_BATCH_RESPONSE_BYTES,
        )
    }

    pub fn cancel_batch(
        &self,
        endpoint: &Endpoint,
        api_key: &str,
        batch_id: &str,
    ) -> Result<ApiResponse, AppError> {
        self.send_request(
            Method::POST,
            endpoint.batch_cancel_url(batch_id)?,
            api_key,
            RequestPayload::Empty,
            RequestKind::ControlPost,
            MAX_BATCH_RESPONSE_BYTES,
        )
    }

    fn send_json<T: Serialize>(
        &self,
        method: Method,
        url: url::Url,
        api_key: &str,
        body: &T,
        kind: RequestKind,
        max_response: usize,
    ) -> Result<ApiResponse, AppError> {
        let body = serde_json::to_vec(body).map_err(|_| {
            AppError::preflight(
                "request_serialization_failed",
                "The batch request could not be serialized safely.",
            )
        })?;
        self.send_request(
            method,
            url,
            api_key,
            RequestPayload::Json(body),
            kind,
            max_response,
        )
    }

    fn send_request(
        &self,
        method: Method,
        url: url::Url,
        api_key: &str,
        payload: RequestPayload,
        kind: RequestKind,
        max_response: usize,
    ) -> Result<ApiResponse, AppError> {
        let mut request = self
            .client
            .request(method.clone(), url)
            .bearer_auth(api_key);
        match payload {
            RequestPayload::Multipart(form) => {
                request = request.multipart(form);
            }
            RequestPayload::Json(body) => {
                request = request
                    .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                    .body(body);
            }
            RequestPayload::Empty => {}
        }
        let response = request.send().map_err(|_| kind.transport_error())?;
        let status = response.status();
        let request_id = request_id(response.headers(), api_key);
        if status.is_redirection() {
            return Err(kind.redirect_error().with_http(status.as_u16(), request_id));
        }
        if status.is_client_error() {
            let code = safe_error_code(response, api_key);
            if matches!(kind, RequestKind::FileContent) && matches!(status.as_u16(), 404 | 410) {
                return Err(AppError::batch_failed(
                    if status.as_u16() == 410 {
                        "batch_output_expired"
                    } else {
                        "batch_output_unavailable"
                    },
                    "The completed Batch output file is unavailable (404/410). Inspect the Batch record and error file before deciding what to do next.",
                )
                .with_http(status.as_u16(), request_id));
            }
            if matches!(
                kind,
                RequestKind::Observation | RequestKind::FileContent | RequestKind::InputFileContent
            ) {
                return Err(AppError::observation(
                    "batch_observation_rejected",
                    format!(
                        "The Batch read was rejected (HTTP {}; {}). Retrying this read-only operation is safe.",
                        status.as_u16(),
                        code.unwrap_or_else(|| "no safe error code returned".to_owned())
                    ),
                )
                .with_http(status.as_u16(), request_id));
            }
            return Err(AppError::api_rejected(
                "api_rejected",
                format!(
                    "The API rejected the batch operation (HTTP {}; {}).",
                    status.as_u16(),
                    code.unwrap_or_else(|| "no safe error code returned".to_owned())
                ),
                status.as_u16(),
                request_id,
            ));
        }
        if status.is_server_error() || !status.is_success() {
            return Err(kind.status_error(status.as_u16(), request_id));
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_response as u64)
        {
            return Err(kind
                .size_error(max_response)
                .with_http(status.as_u16(), request_id));
        }
        let response_body = read_bounded(response, max_response).map_err(|_| {
            kind.read_error()
                .with_http(status.as_u16(), request_id.clone())
        })?;
        if response_body.len() > max_response {
            return Err(kind
                .size_error(max_response)
                .with_http(status.as_u16(), request_id));
        }
        Ok(ApiResponse {
            body: response_body,
            request_id,
            status: status.as_u16(),
        })
    }
}

enum RequestPayload {
    Multipart(Form),
    Json(Vec<u8>),
    Empty,
}

pub const MAX_BATCH_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BATCH_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_BATCH_CONTENT_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
enum RequestKind {
    BillablePost,
    ControlPost,
    Observation,
    FileContent,
    InputFileContent,
}

impl RequestKind {
    fn transport_error(self) -> AppError {
        match self {
            Self::BillablePost => AppError::indeterminate(
                "batch_submission_outcome_unknown",
                "The batch submission POST may have been processed. Do not retry automatically; inspect the persisted job state.",
            ),
            Self::ControlPost => AppError::indeterminate(
                "batch_cancel_outcome_unknown",
                "The batch cancel request outcome is unknown. Query the persisted batch status before trying again; do not resubmit automatically.",
            ),
            Self::Observation => AppError::observation(
                "batch_observation_failed",
                "The batch status or output could not be observed. Retrying this read-only operation is safe.",
            ),
            Self::FileContent => AppError::observation(
                "batch_output_observation_failed",
                "The Batch output could not be observed. Retrying this read-only operation is safe.",
            ),
            Self::InputFileContent => AppError::observation(
                "batch_input_observation_failed",
                "The Batch input file could not be observed. Retrying this read-only operation is safe.",
            ),
        }
    }

    fn redirect_error(self) -> AppError {
        match self {
            Self::BillablePost => AppError::indeterminate(
                "batch_submission_redirected",
                "The batch submission returned a redirect. Credentials were not forwarded and the submission outcome may be unknown; do not retry automatically.",
            ),
            Self::ControlPost => AppError::indeterminate(
                "batch_cancel_redirected",
                "The batch cancel returned a redirect. Credentials were not forwarded, but cancellation outcome may be unknown; query status before trying again.",
            ),
            Self::Observation => AppError::observation(
                "batch_observation_redirected",
                "The batch observation returned a redirect. Credentials were not forwarded; retrying the read-only operation is safe.",
            ),
            Self::FileContent => AppError::observation(
                "batch_output_redirected",
                "The Batch output returned a redirect. Credentials were not forwarded; retrying the read-only operation is safe.",
            ),
            Self::InputFileContent => AppError::observation(
                "batch_input_redirected",
                "The Batch input file returned a redirect. Credentials were not forwarded; retrying the read-only operation is safe.",
            ),
        }
    }

    fn status_error(self, status: u16, request_id: Option<String>) -> AppError {
        match self {
            Self::BillablePost => AppError::indeterminate(
                "batch_submission_server_error",
                "The batch submission returned a server error; the operation outcome may be unknown. Do not retry automatically.",
            )
            .with_http(status, request_id),
            Self::ControlPost => AppError::indeterminate(
                "batch_cancel_outcome_unknown",
                format!("The batch cancel endpoint returned HTTP {status}; cancellation outcome may be unknown. Query status before trying again."),
            )
            .with_http(status, request_id),
            Self::Observation => AppError::observation(
                "batch_observation_server_error",
                format!("The batch endpoint returned HTTP {status}; retrying this read-only operation is safe."),
            )
            .with_http(status, request_id),
            Self::FileContent => AppError::observation(
                "batch_output_server_error",
                format!("The Batch output endpoint returned HTTP {status}; retrying this read-only operation is safe."),
            )
            .with_http(status, request_id),
            Self::InputFileContent => AppError::observation(
                "batch_input_server_error",
                format!("The Batch input endpoint returned HTTP {status}; retrying this read-only operation is safe."),
            )
            .with_http(status, request_id),
        }
    }

    fn size_error(self, max: usize) -> AppError {
        match self {
            Self::BillablePost => AppError::invalid_response(
                "batch_response_too_large",
                format!("The batch submission response exceeded the {max}-byte safety limit; do not retry automatically."),
            ),
            Self::ControlPost => AppError::indeterminate(
                "batch_cancel_outcome_unknown",
                format!("The batch cancel response exceeded the {max}-byte safety limit; cancellation outcome may be unknown. Query status before trying again."),
            ),
            Self::Observation => AppError::observation(
                "batch_response_too_large",
                format!("The batch response exceeded the {max}-byte safety limit; retrying the observation is safe."),
            ),
            Self::FileContent => AppError::observation(
                "batch_output_too_large",
                format!("The Batch output exceeded the {max}-byte safety limit; retrying the observation is safe."),
            ),
            Self::InputFileContent => AppError::observation(
                "batch_input_too_large",
                format!("The Batch input exceeded the {max}-byte safety limit; retrying the observation is safe."),
            ),
        }
    }

    fn read_error(self) -> AppError {
        match self {
            Self::BillablePost => AppError::invalid_response(
                "batch_response_read_failed",
                "The batch submission response could not be read completely; do not retry automatically.",
            ),
            Self::ControlPost => AppError::indeterminate(
                "batch_cancel_outcome_unknown",
                "The batch cancel response could not be read completely; cancellation outcome may be unknown. Query status before trying again.",
            ),
            Self::Observation => AppError::observation(
                "batch_response_read_failed",
                "The batch response could not be read completely; retrying the observation is safe.",
            ),
            Self::FileContent => AppError::observation(
                "batch_output_read_failed",
                "The Batch output could not be read completely; retrying the observation is safe.",
            ),
            Self::InputFileContent => AppError::observation(
                "batch_input_read_failed",
                "The Batch input could not be read completely; retrying the observation is safe.",
            ),
        }
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
            request_file: None,
            provider: crate::cli::Provider::Api,
            prompt: Some("test".to_owned()),
            prompt_file: None,
            n: 1,
            output_dir: ".".into(),
            name: None,
            prefix: None,
            format: crate::cli::OutputFormat::Png,
            size: "auto".to_owned(),
            quality: crate::cli::Quality::Auto,
            confirm_high_quality: false,
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

    #[test]
    fn read_only_batch_rejections_are_retryable_observations() {
        let error = RequestKind::Observation.status_error(429, None);
        assert_eq!(error.status, crate::report::Status::BatchObservationFailed);
        assert!(error.automatic_retry_safe);

        let error = RequestKind::ControlPost.status_error(500, None);
        assert_eq!(error.status, crate::report::Status::OutcomeIndeterminate);
        assert!(!error.automatic_retry_safe);
    }

    #[test]
    fn input_file_not_found_is_a_retryable_observation() {
        use std::{io::Write, net::TcpListener, thread};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let endpoint = Endpoint::authorize(&format!("http://{address}/v1"), None, true).unwrap();
        let client = ApiClient::new(5).unwrap();
        let error = client
            .get_input_file_content(&endpoint, "test-key", "file-input")
            .unwrap_err();
        assert_eq!(error.status, crate::report::Status::BatchObservationFailed);
        assert!(error.automatic_retry_safe);
        assert_eq!(error.code, "batch_observation_rejected");
        server.join().unwrap();
    }
}
