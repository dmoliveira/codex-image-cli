use serde::Serialize;
use std::path::PathBuf;

use crate::cli::Provider;
use crate::cost::CostPreview;
use crate::provider;

pub const SCHEMA_VERSION: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    UsageError,
    PreflightError,
    ApiRejected,
    OutcomeIndeterminate,
    InvalidSuccessResponse,
    OutputCommitFailed,
    BatchNotReady,
    BatchObservationFailed,
    BatchFailed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UsageError => "usage_error",
            Self::PreflightError => "preflight_error",
            Self::ApiRejected => "api_rejected",
            Self::OutcomeIndeterminate => "outcome_indeterminate",
            Self::InvalidSuccessResponse => "invalid_success_response",
            Self::OutputCommitFailed => "output_commit_failed",
            Self::BatchNotReady => "batch_not_ready",
            Self::BatchObservationFailed => "batch_observation_failed",
            Self::BatchFailed => "batch_failed",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::UsageError => 2,
            Self::PreflightError => 3,
            Self::ApiRejected => 4,
            Self::OutcomeIndeterminate => 5,
            Self::InvalidSuccessResponse => 6,
            Self::OutputCommitFailed => 7,
            Self::BatchNotReady => 8,
            Self::BatchObservationFailed => 9,
            Self::BatchFailed => 10,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppError {
    pub status: Status,
    pub code: &'static str,
    pub message: String,
    pub request_id: Option<String>,
    pub http_status: Option<u16>,
    pub provider: Option<Provider>,
    pub image_count: Option<u8>,
    pub process_exit_code: Option<i32>,
    pub process_timed_out: bool,
    pub process_diagnostics_bytes: usize,
    pub process_diagnostics_truncated: bool,
    pub automatic_retry_safe: bool,
    pub possibly_modified_paths: Vec<PathBuf>,
}

impl AppError {
    pub fn new(status: Status, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            request_id: None,
            http_status: None,
            provider: None,
            image_count: None,
            process_exit_code: None,
            process_timed_out: false,
            process_diagnostics_bytes: 0,
            process_diagnostics_truncated: false,
            automatic_retry_safe: false,
            possibly_modified_paths: Vec::new(),
        }
    }

    pub fn usage(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Status::UsageError, code, message)
    }

    pub fn preflight(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Status::PreflightError, code, message)
    }

    pub fn api_rejected(
        code: &'static str,
        message: impl Into<String>,
        http_status: u16,
        request_id: Option<String>,
    ) -> Self {
        let mut error = Self::new(Status::ApiRejected, code, message);
        error.http_status = Some(http_status);
        error.request_id = request_id;
        error
    }

    pub fn indeterminate(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Status::OutcomeIndeterminate, code, message)
    }

    pub fn invalid_response(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Status::InvalidSuccessResponse, code, message)
    }

    pub fn output_commit(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Status::OutputCommitFailed, code, message)
    }

    pub fn not_ready(code: &'static str, message: impl Into<String>) -> Self {
        let mut error = Self::new(Status::BatchNotReady, code, message);
        error.automatic_retry_safe = true;
        error
    }

    pub fn observation(code: &'static str, message: impl Into<String>) -> Self {
        let mut error = Self::new(Status::BatchObservationFailed, code, message);
        error.automatic_retry_safe = true;
        error
    }

    pub fn batch_failed(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Status::BatchFailed, code, message)
    }

    pub fn retry_safe(mut self) -> Self {
        self.automatic_retry_safe = true;
        self
    }

    pub fn set_request_id(&mut self, request_id: Option<String>) {
        self.request_id = request_id;
    }

    pub fn set_http_status(&mut self, status: u16) {
        self.http_status = Some(status);
    }

    pub fn set_provider(&mut self, provider: Provider) {
        self.provider = Some(provider);
    }

    pub fn set_image_count(&mut self, image_count: u8) {
        self.image_count = Some(image_count);
    }

    pub fn set_process_metadata(
        &mut self,
        exit_code: Option<i32>,
        timed_out: bool,
        diagnostics_bytes: usize,
        diagnostics_truncated: bool,
    ) {
        self.process_exit_code = exit_code;
        self.process_timed_out = timed_out;
        self.process_diagnostics_bytes = diagnostics_bytes;
        self.process_diagnostics_truncated = diagnostics_truncated;
    }

    pub fn add_possibly_modified_paths(&mut self, paths: Vec<PathBuf>) {
        self.possibly_modified_paths.extend(paths);
        self.possibly_modified_paths.sort();
        self.possibly_modified_paths.dedup();
    }

    pub fn report(&self, image_count: u8) -> RunReport {
        RunReport {
            schema_version: SCHEMA_VERSION,
            ok: false,
            status: self.status.as_str(),
            exit_code: self.status.exit_code(),
            request: RequestInfo {
                attempted: matches!(
                    self.status,
                    Status::ApiRejected
                        | Status::OutcomeIndeterminate
                        | Status::InvalidSuccessResponse
                        | Status::OutputCommitFailed
                ),
                image_count: self.image_count.unwrap_or(image_count),
                model: self.provider.and_then(provider::model),
                provider: self.provider.map(Provider::as_str),
                request_id: self.request_id.clone(),
            },
            http: HttpInfo {
                status: self.http_status,
            },
            outputs: Vec::new(),
            retained_artifacts: Vec::new(),
            possibly_modified_paths: path_strings(&self.possibly_modified_paths),
            cost_preview: None,
            error: Some(ErrorInfo {
                code: self.code,
                message: self.message.clone(),
                automatic_retry_safe: self.automatic_retry_safe,
                process_exit_code: self.process_exit_code,
                process_timed_out: self.process_timed_out,
                diagnostics_bytes: self.process_diagnostics_bytes,
                diagnostics_truncated: self.process_diagnostics_truncated,
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub schema_version: u8,
    pub ok: bool,
    pub status: &'static str,
    pub exit_code: i32,
    pub request: RequestInfo,
    pub http: HttpInfo,
    pub outputs: Vec<String>,
    /// Private backup artifacts retained instead of unsafe pathname cleanup.
    pub retained_artifacts: Vec<String>,
    pub possibly_modified_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_preview: Option<CostPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

impl RunReport {
    pub fn dry_run(
        image_count: u8,
        outputs: Vec<PathBuf>,
        provider: Provider,
        cost_preview: CostPreview,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ok: true,
            status: "dry_run",
            exit_code: 0,
            request: RequestInfo {
                attempted: false,
                image_count,
                model: provider::model(provider),
                provider: Some(provider.as_str()),
                request_id: None,
            },
            http: HttpInfo { status: None },
            outputs: path_strings(&outputs),
            retained_artifacts: Vec::new(),
            possibly_modified_paths: Vec::new(),
            cost_preview: Some(cost_preview),
            error: None,
        }
    }

    pub fn success(
        image_count: u8,
        outputs: Vec<PathBuf>,
        retained_artifacts: Vec<PathBuf>,
        request_id: Option<String>,
        http_status: Option<u16>,
        provider: Provider,
        cost_preview: CostPreview,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ok: true,
            status: "success",
            exit_code: 0,
            request: RequestInfo {
                attempted: true,
                image_count,
                model: provider::model(provider),
                provider: Some(provider.as_str()),
                request_id,
            },
            http: HttpInfo {
                status: http_status,
            },
            outputs: path_strings(&outputs),
            retained_artifacts: path_strings(&retained_artifacts),
            possibly_modified_paths: Vec::new(),
            cost_preview: Some(cost_preview),
            error: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RequestInfo {
    pub attempted: bool,
    pub image_count: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HttpInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

#[derive(Debug, Serialize)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub message: String,
    pub automatic_retry_safe: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_exit_code: Option<i32>,
    #[serde(skip_serializing_if = "is_false")]
    pub process_timed_out: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub diagnostics_bytes: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub diagnostics_truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BatchContext {
    pub operation: &'static str,
    pub job_file: Option<String>,
    pub job_id: Option<String>,
    pub batch_id: Option<String>,
    pub input_file_id: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub remote_status: Option<String>,
    pub http_status: Option<u16>,
    pub request_id: Option<String>,
    pub image_count: u8,
    pub attempted: bool,
    pub outputs: Vec<String>,
    pub retained_artifacts: Vec<String>,
    pub possibly_modified_paths: Vec<String>,
    pub next_action: Option<String>,
    pub cost_preview: Option<CostPreview>,
    pub request_counts: Option<BatchRequestCountsInfo>,
}

#[derive(Debug, Serialize)]
pub struct BatchReport {
    pub schema_version: u8,
    pub operation: &'static str,
    pub ok: bool,
    pub status: &'static str,
    pub exit_code: i32,
    pub job_file: Option<String>,
    pub job_id: Option<String>,
    pub batch_id: Option<String>,
    pub input_file_id: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub remote_status: Option<String>,
    pub request: BatchRequestInfo,
    pub http: HttpInfo,
    pub outputs: Vec<String>,
    pub retained_artifacts: Vec<String>,
    pub possibly_modified_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_preview: Option<CostPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_counts: Option<BatchRequestCountsInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Serialize)]
pub struct BatchRequestInfo {
    pub attempted: bool,
    pub image_count: u8,
    pub model: &'static str,
    pub provider: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchRequestCountsInfo {
    pub completed: u32,
    pub failed: u32,
    pub total: u32,
}

impl BatchContext {
    pub fn report(&self, error: Option<&AppError>) -> BatchReport {
        let mut possibly_modified_paths = self.possibly_modified_paths.clone();
        if let Some(error) = error {
            possibly_modified_paths.extend(
                error
                    .possibly_modified_paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
            possibly_modified_paths.sort();
            possibly_modified_paths.dedup();
        }
        let (ok, status, exit_code, request_id, http_status, error_info) = match error {
            Some(error) => (
                false,
                error.status.as_str(),
                error.status.exit_code(),
                error.request_id.clone(),
                error.http_status,
                Some(ErrorInfo {
                    code: error.code,
                    message: error.message.clone(),
                    automatic_retry_safe: error.automatic_retry_safe,
                    process_exit_code: error.process_exit_code,
                    process_timed_out: error.process_timed_out,
                    diagnostics_bytes: error.process_diagnostics_bytes,
                    diagnostics_truncated: error.process_diagnostics_truncated,
                }),
            ),
            None => {
                let status = batch_status(self.remote_status.as_deref());
                (true, status, 0, None, None, None)
            }
        };
        BatchReport {
            schema_version: SCHEMA_VERSION,
            operation: self.operation,
            ok,
            status,
            exit_code,
            job_file: self.job_file.clone(),
            job_id: self.job_id.clone(),
            batch_id: self.batch_id.clone(),
            input_file_id: self.input_file_id.clone(),
            output_file_id: self.output_file_id.clone(),
            error_file_id: self.error_file_id.clone(),
            remote_status: self.remote_status.clone(),
            request: BatchRequestInfo {
                attempted: self.attempted,
                image_count: self.image_count,
                model: "gpt-image-2",
                provider: "api",
                request_id: request_id.or_else(|| self.request_id.clone()),
            },
            http: HttpInfo {
                status: http_status.or(self.http_status),
            },
            outputs: self.outputs.clone(),
            retained_artifacts: self.retained_artifacts.clone(),
            possibly_modified_paths,
            cost_preview: self.cost_preview.clone(),
            request_counts: self.request_counts.clone(),
            next_action: self.next_action.clone(),
            error: error_info,
        }
    }
}

fn batch_status(status: Option<&str>) -> &'static str {
    match status {
        Some("validating") => "validating",
        Some("in_progress") => "in_progress",
        Some("finalizing") => "finalizing",
        Some("completed") => "completed",
        Some("failed") => "failed",
        Some("expired") => "expired",
        Some("cancelling") => "cancelling",
        Some("cancelled") => "cancelled",
        Some("retrieved") => "retrieved",
        _ => "submitted",
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

pub fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}
