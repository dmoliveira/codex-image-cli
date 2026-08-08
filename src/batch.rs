use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    api::{
        ApiClient, BatchCreateRequest, BatchInfo, BatchRequestCounts, FileInfo,
        ImageGenerationRequest, OutputExpiresAfter, MAX_BATCH_CONTENT_BYTES,
    },
    cli::{
        BatchCancelArgs, BatchJobArgs, BatchRetrieveArgs, BatchSubmitArgs, GenerateArgs,
        OutputFormat, Provider,
    },
    endpoint::{validate_remote_id, Endpoint},
    image::decode_images,
    output::{derive_file_names, derive_output_paths, OutputTransaction},
    report::{AppError, BatchContext, BatchReport},
    MODEL,
};

pub const JOB_SCHEMA_VERSION: u8 = 2;
const MAX_BATCH_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESULT_LINE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_OUTPUT_EXPIRY_SECONDS: u32 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Prepared,
    UploadInFlight,
    UploadOutcomeUnknown,
    InputUploaded,
    CreateInFlight,
    CreateOutcomeUnknown,
    Submitted,
    Completed,
    Publishing,
    Retrieved,
    Failed,
    CancelInFlight,
    CancelOutcomeUnknown,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJob {
    pub schema_version: u8,
    pub revision: u64,
    pub job_id: String,
    pub state: JobState,
    pub provider: Provider,
    pub model: String,
    pub api_base_url: String,
    pub output_dir: String,
    pub output_names: Vec<String>,
    pub overwrite: bool,
    pub format: OutputFormat,
    pub image_count: u8,
    pub quality: crate::cli::Quality,
    pub size: String,
    pub background: crate::cli::Background,
    pub moderation: crate::cli::Moderation,
    pub custom_ids: Vec<String>,
    pub input_file_id: Option<String>,
    pub batch_id: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub remote_status: Option<String>,
    pub request_counts: Option<BatchRequestCounts>,
    pub publishing: Option<PublishingPlan>,
    pub retained_artifacts: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishingPlan {
    pub artifacts: Vec<PublishingArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishingArtifact {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug)]
pub struct BatchFailure {
    pub error: Box<AppError>,
    pub context: Box<BatchContext>,
}

impl BatchFailure {
    fn new(error: AppError, context: BatchContext) -> Self {
        Self {
            error: Box::new(error),
            context: Box::new(context),
        }
    }
}

pub fn submit(args: &BatchSubmitArgs) -> Result<BatchReport, BatchFailure> {
    let generation = args
        .generation
        .resolve_request_file()
        .map_err(|error| failure(error, "batch.submit", None))?;
    let prompt = generation
        .read_prompt()
        .map_err(|error| failure(error, "batch.submit", None))?;
    generation
        .validate_batch(&prompt)
        .map_err(|error| failure(error, "batch.submit", None))?;
    require_api_provider(&generation).map_err(|error| failure(error, "batch.submit", None))?;

    let output_names = derive_file_names(
        generation.n,
        generation.name.as_deref(),
        generation.prefix.as_deref(),
        generation.format,
    )
    .map_err(|error| failure(error, "batch.submit", None))?;
    let output_dir = absolute_path(&generation.output_dir)
        .map_err(|error| failure(error, "batch.submit", None))?;
    let endpoint = Endpoint::authorize(
        &generation.api_base_url,
        generation.dangerously_allow_api_key_to.as_deref(),
        generation.allow_insecure_localhost,
    )
    .map_err(|error| failure(error, "batch.submit", None))?;
    let mut base_context = BatchContext {
        operation: "batch.submit",
        image_count: generation.n,
        ..BatchContext::default()
    };

    let job_id = new_job_id();
    let job_file = JobStore::resolve(args.job_file.as_deref(), &job_id).map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(job_context("batch.submit", Path::new(""))),
        )
    })?;
    let custom_ids = (0..generation.n)
        .map(|index| format!("{job_id}-{index:02}"))
        .collect::<Vec<_>>();
    let input = build_batch_input(&prompt, &generation, &custom_ids).map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job_id,
                None,
                Some(&job_file),
            )),
        )
    })?;
    let mut job = BatchJob {
        schema_version: JOB_SCHEMA_VERSION,
        revision: 0,
        job_id: job_id.clone(),
        state: JobState::Prepared,
        provider: generation.provider,
        model: MODEL.to_owned(),
        api_base_url: generation.api_base_url.clone(),
        output_dir: output_dir.to_string_lossy().into_owned(),
        output_names,
        overwrite: generation.overwrite,
        format: generation.format,
        image_count: generation.n,
        quality: generation.quality,
        size: generation.size.clone(),
        background: generation.background,
        moderation: generation.moderation,
        custom_ids,
        input_file_id: None,
        batch_id: None,
        output_file_id: None,
        error_file_id: None,
        remote_status: None,
        request_counts: None,
        publishing: None,
        retained_artifacts: Vec::new(),
        created_at: now_seconds(),
        updated_at: now_seconds(),
    };

    if generation.dry_run {
        let mut report_context = context_for_job(&base_context, &job.job_id, None, Some(&job_file));
        report_context.outputs = output_names_for_context(&job.output_dir, &job.output_names);
        report_context.next_action = Some("remove --dry-run to submit the batch".to_owned());
        return Ok(dry_run_report(report_context));
    }

    let api_key = api_key().map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job.job_id,
                None,
                Some(&job_file),
            )),
        )
    })?;
    JobStore::create(&job_file, &job).map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job.job_id,
                None,
                Some(&job_file),
            )),
        )
    })?;
    let client = ApiClient::new(generation.timeout_seconds).map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job.job_id,
                None,
                Some(&job_file),
            )),
        )
    })?;

    let upload_in_flight = transition(&job_file, |job| {
        job.state = JobState::UploadInFlight;
        Ok(())
    })
    .map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job_id,
                None,
                Some(&job_file),
            )),
        )
    })?;
    base_context.attempted = true;
    let upload = match client.upload_batch_input(&endpoint, &api_key, input) {
        Ok(response) => response,
        Err(error) => {
            let error = mark_unknown(
                &job_file,
                upload_in_flight.revision,
                JobState::UploadOutcomeUnknown,
                error,
            );
            return Err(failure(
                error,
                "batch.submit",
                Some(context_for_job(
                    &base_context,
                    &job_id,
                    None,
                    Some(&job_file),
                )),
            ));
        }
    };
    let file_info: FileInfo = match serde_json::from_slice::<FileInfo>(&upload.body) {
        Ok(file_info) if validate_remote_id(&file_info.id, "file_id").is_ok() => file_info,
        _ => {
            let error = mark_unknown(
                &job_file,
                upload_in_flight.revision,
                JobState::UploadOutcomeUnknown,
                AppError::invalid_response(
                    "batch_input_upload_invalid",
                    "The file-upload response did not contain a safe file ID; do not retry automatically. Inspect the job and account files.",
                ),
            );
            return Err(failure(
                error,
                "batch.submit",
                Some(context_for_job(
                    &base_context,
                    &job_id,
                    None,
                    Some(&job_file),
                )),
            ));
        }
    };
    let input_uploaded = transition_if_revision(&job_file, upload_in_flight.revision, |job| {
        job.state = JobState::InputUploaded;
        job.input_file_id = Some(file_info.id.clone());
        Ok(())
    })
    .map_err(|error| {
        let mut recovery_error = state_persistence_error("input upload", &file_info.id, error);
        recovery_error.set_http_status(upload.status);
        recovery_error.set_request_id(upload.request_id.clone());
        failure(
            recovery_error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job_id,
                Some(&file_info.id),
                Some(&job_file),
            )),
        )
    })?;
    let create_in_flight = transition_if_revision(&job_file, input_uploaded.revision, |job| {
        job.state = JobState::CreateInFlight;
        Ok(())
    })
    .map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job_id,
                None,
                Some(&job_file),
            )),
        )
    })?;
    let create_request = BatchCreateRequest {
        input_file_id: &file_info.id,
        endpoint: "/v1/images/generations",
        completion_window: "24h",
        output_expires_after: OutputExpiresAfter {
            anchor: "created_at",
            seconds: DEFAULT_OUTPUT_EXPIRY_SECONDS,
        },
    };
    let created = match client.create_batch(&endpoint, &api_key, &create_request) {
        Ok(response) => response,
        Err(error) => {
            let error = mark_unknown(
                &job_file,
                create_in_flight.revision,
                JobState::CreateOutcomeUnknown,
                error,
            );
            return Err(failure(
                error,
                "batch.submit",
                Some(context_for_job(
                    &base_context,
                    &job_id,
                    Some(&file_info.id),
                    Some(&job_file),
                )),
            ));
        }
    };
    let batch_info: BatchInfo = match parse_batch_create_info(&created.body, &file_info.id) {
        Ok(info) => info,
        Err(error) => {
            let error = mark_unknown(
                &job_file,
                create_in_flight.revision,
                JobState::CreateOutcomeUnknown,
                error,
            );
            return Err(failure(
                error,
                "batch.submit",
                Some(context_for_job(
                    &base_context,
                    &job_id,
                    Some(&file_info.id),
                    Some(&job_file),
                )),
            ));
        }
    };
    job = transition_if_revision(&job_file, create_in_flight.revision, |job| {
        job.state = JobState::Submitted;
        job.batch_id = Some(batch_info.id.clone());
        job.remote_status = Some(batch_info.status.clone());
        job.output_file_id = batch_info.output_file_id.clone();
        job.error_file_id = batch_info.error_file_id.clone();
        job.request_counts = batch_info.request_counts.clone();
        Ok(())
    })
    .map_err(|error| {
        let mut recovery_error = state_persistence_error("batch creation", &batch_info.id, error);
        recovery_error.set_http_status(created.status);
        recovery_error.set_request_id(created.request_id.clone());
        failure(
            recovery_error,
            "batch.submit",
            Some(context_for_job(
                &base_context,
                &job_id,
                Some(&file_info.id),
                Some(&job_file),
            )),
        )
    })?;
    let mut report_context = context_for_job(
        &base_context,
        &job.job_id,
        job.batch_id.as_deref(),
        Some(&job_file),
    );
    report_context.input_file_id = job.input_file_id.clone();
    report_context.remote_status = job.remote_status.clone();
    report_context.output_file_id = job.output_file_id.clone();
    report_context.error_file_id = job.error_file_id.clone();
    report_context.http_status = Some(created.status);
    report_context.request_id = created.request_id;
    report_context.next_action =
        Some("run batch status or batch retrieve with this job file".to_owned());
    Ok(report_context.report(None))
}

pub fn status(args: &BatchJobArgs) -> Result<BatchReport, BatchFailure> {
    validate_timeout(args.timeout_seconds).map_err(|error| failure(error, "batch.status", None))?;
    let (job_file, job) = load_job(args, "batch.status")?;
    let mut context = context_from_job("batch.status", &job_file, &job);
    if job.state == JobState::Publishing {
        return recover_publishing(&job_file, &job, context);
    }
    if job.state == JobState::Retrieved {
        return recover_retrieved(&job, context);
    }
    let batch_id = require_batch_id(&job, &context)?;
    let endpoint = endpoint_from_job(&job, &context, args)?;
    let key = api_key().map_err(|error| BatchFailure::new(error, context.clone()))?;
    let client = ApiClient::new(args.timeout_seconds)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    context.attempted = true;
    let response = client
        .get_batch(&endpoint, &key, &batch_id)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let info = parse_batch_info(
        &response.body,
        Some(&batch_id),
        job.input_file_id.as_deref(),
    )
    .map_err(|error| BatchFailure::new(error, context.clone()))?;
    validate_remote_transition(&job, &info)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    set_context_from_info(&mut context, &info);
    let state = state_for_remote_status(&info.status);
    let updated = transition_if_revision(&job_file, job.revision, |job| {
        job.state = state;
        job.remote_status = Some(info.status.clone());
        job.output_file_id = info.output_file_id.clone();
        job.error_file_id = info.error_file_id.clone();
        job.request_counts = info.request_counts.clone();
        Ok(())
    })
    .map_err(|error| BatchFailure::new(error, context.clone()))?;
    context = context_from_job("batch.status", &job_file, &updated);
    context.http_status = Some(response.status);
    context.request_id = response.request_id;
    if matches!(updated.remote_status.as_deref(), Some("failed" | "expired")) {
        context.next_action = Some("inspect the Batch error file and run batch status".to_owned());
        return Err(BatchFailure::new(
            terminal_batch_error(updated.remote_status.as_deref().unwrap_or_default()),
            context,
        ));
    }
    if !is_terminal_remote_status(&info.status) {
        context.next_action =
            Some("run batch status again or use batch retrieve --wait".to_owned());
    }
    Ok(context.report(None))
}

pub fn retrieve(args: &BatchRetrieveArgs) -> Result<BatchReport, BatchFailure> {
    validate_timeout(args.job.timeout_seconds)
        .and_then(|_| validate_wait(args))
        .map_err(|error| failure(error, "batch.retrieve", None))?;
    let (job_file, job) = load_job(&args.job, "batch.retrieve")?;
    let mut context = context_from_job("batch.retrieve", &job_file, &job);
    if job.state == JobState::Publishing {
        return recover_publishing(&job_file, &job, context);
    }
    if job.state == JobState::Retrieved {
        return recover_retrieved(&job, context);
    }
    let batch_id = require_batch_id(&job, &context)?;
    let endpoint = endpoint_from_job(&job, &context, &args.job)?;
    let key = api_key().map_err(|error| BatchFailure::new(error, context.clone()))?;
    let client = ApiClient::new(args.job.timeout_seconds)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let deadline = Instant::now() + Duration::from_secs(args.max_wait_seconds);
    let mut first_request = true;
    let mut previous_info: Option<BatchInfo> = None;
    let info = loop {
        if args.wait && !first_request && Instant::now() >= deadline {
            let error = AppError::not_ready(
                "batch_wait_timeout",
                "The batch did not complete before --max-wait-seconds. The job remains recoverable; run batch status or batch retrieve again.",
            );
            return Err(BatchFailure::new(error, context));
        }
        first_request = false;
        context.attempted = true;
        let response = client
            .get_batch(&endpoint, &key, &batch_id)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        let info = parse_batch_info(
            &response.body,
            Some(&batch_id),
            job.input_file_id.as_deref(),
        )
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
        validate_remote_transition(&job, &info)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        if let Some(previous) = previous_info.as_ref() {
            validate_observation_progress(
                Some(&previous.status),
                previous.output_file_id.as_deref(),
                previous.error_file_id.as_deref(),
                previous.request_counts.as_ref(),
                &info,
            )
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        }
        previous_info = Some(info.clone());
        context.http_status = Some(response.status);
        context.request_id = response.request_id;
        set_context_from_info(&mut context, &info);
        if is_terminal_remote_status(&info.status) || !args.wait {
            break info;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let error = AppError::not_ready(
                "batch_wait_timeout",
                "The batch did not complete before --max-wait-seconds. The job remains recoverable; run batch status or batch retrieve again.",
            );
            return Err(BatchFailure::new(error, context));
        }
        thread::sleep(remaining.min(Duration::from_secs(args.poll_interval_seconds)));
    };
    let updated = transition_if_revision(&job_file, job.revision, |job| {
        job.state = state_for_remote_status(&info.status);
        job.remote_status = Some(info.status.clone());
        job.output_file_id = info.output_file_id.clone();
        job.error_file_id = info.error_file_id.clone();
        job.request_counts = info.request_counts.clone();
        Ok(())
    })
    .map_err(|error| BatchFailure::new(error, context.clone()))?;
    context = context_from_job("batch.retrieve", &job_file, &updated);
    if !matches!(
        updated.remote_status.as_deref(),
        Some("completed" | "cancelled")
    ) {
        if matches!(updated.remote_status.as_deref(), Some("failed" | "expired")) {
            let error = terminal_batch_error(updated.remote_status.as_deref().unwrap_or_default());
            context.next_action =
                Some("inspect the Batch error file and run batch status".to_owned());
            return Err(BatchFailure::new(error, context));
        }
        let error = AppError::not_ready(
            "batch_not_ready",
            "The batch is still processing. Run batch retrieve again or add --wait with a bounded timeout.",
        );
        context.next_action = Some("run batch retrieve --wait".to_owned());
        return Err(BatchFailure::new(error, context));
    }
    let Some(output_file_id) = updated.output_file_id.as_deref() else {
        let error = AppError::invalid_response(
            "batch_output_missing",
            "The batch reached a terminal state without an output file. Inspect the batch status and error file before retrying.",
        );
        return Err(BatchFailure::new(error, context));
    };
    let content = client
        .get_file_content(&endpoint, &key, output_file_id)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let images = parse_batch_output(&content.body, &updated).map_err(|mut failure| {
        failure.context.job_file = Some(job_file.to_string_lossy().into_owned());
        failure
    })?;
    publish_images(
        &job_file,
        &updated,
        images,
        context,
        content.status,
        content.request_id,
    )
}

pub fn cancel(args: &BatchCancelArgs) -> Result<BatchReport, BatchFailure> {
    validate_timeout(args.job.timeout_seconds)
        .map_err(|error| failure(error, "batch.cancel", None))?;
    let (job_file, job) = load_job(&args.job, "batch.cancel")?;
    let mut context = context_from_job("batch.cancel", &job_file, &job);
    if let Some(status) = job.remote_status.as_deref() {
        if matches!(status, "failed" | "expired") {
            context.next_action =
                Some("inspect the Batch error file and run batch status".to_owned());
            return Err(BatchFailure::new(terminal_batch_error(status), context));
        }
        if matches!(status, "completed" | "cancelled") {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "batch_already_terminal",
                    "The Batch is already terminal; no cancellation request was sent.",
                ),
                context,
            ));
        }
    }
    let batch_id = require_batch_id(&job, &context)?;
    let endpoint = endpoint_from_job(&job, &context, &args.job)?;
    let key = api_key().map_err(|error| BatchFailure::new(error, context.clone()))?;
    let client = ApiClient::new(args.job.timeout_seconds)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let cancel_in_flight = transition_if_revision(&job_file, job.revision, |job| {
        job.state = JobState::CancelInFlight;
        Ok(())
    })
    .map_err(|error| BatchFailure::new(error, context.clone()))?;
    context.attempted = true;
    let response = match client.cancel_batch(&endpoint, &key, &batch_id) {
        Ok(response) => response,
        Err(error) => {
            let error = mark_unknown(
                &job_file,
                cancel_in_flight.revision,
                JobState::CancelOutcomeUnknown,
                error,
            );
            return Err(BatchFailure::new(error, context));
        }
    };
    let info = match parse_batch_info(
        &response.body,
        Some(&batch_id),
        job.input_file_id.as_deref(),
    ) {
        Ok(info) => {
            if let Err(error) = validate_remote_transition(&job, &info) {
                let error = cancel_outcome_unknown(error, &response);
                let error = mark_unknown(
                    &job_file,
                    cancel_in_flight.revision,
                    JobState::CancelOutcomeUnknown,
                    error,
                );
                return Err(BatchFailure::new(error, context));
            }
            info
        }
        Err(error) => {
            let error = cancel_outcome_unknown(error, &response);
            let error = mark_unknown(
                &job_file,
                cancel_in_flight.revision,
                JobState::CancelOutcomeUnknown,
                error,
            );
            return Err(BatchFailure::new(error, context));
        }
    };
    set_context_from_info(&mut context, &info);
    let updated = transition_if_revision(&job_file, cancel_in_flight.revision, |job| {
        job.state = state_for_remote_status(&info.status);
        job.remote_status = Some(info.status.clone());
        job.output_file_id = info.output_file_id.clone();
        job.error_file_id = info.error_file_id.clone();
        job.request_counts = info.request_counts.clone();
        Ok(())
    })
    .map_err(|error| {
        let mut recovery_error = state_persistence_error("batch cancellation", &batch_id, error);
        recovery_error.set_http_status(response.status);
        recovery_error.set_request_id(response.request_id.clone());
        BatchFailure::new(recovery_error, context.clone())
    })?;
    context = context_from_job("batch.cancel", &job_file, &updated);
    context.http_status = Some(response.status);
    context.request_id = response.request_id;
    if matches!(updated.remote_status.as_deref(), Some("failed" | "expired")) {
        context.next_action = Some("inspect the Batch error file and run batch status".to_owned());
        return Err(BatchFailure::new(
            terminal_batch_error(updated.remote_status.as_deref().unwrap_or_default()),
            context,
        ));
    }
    context.next_action =
        Some("run batch status to observe cancellation and any partial output".to_owned());
    Ok(context.report(None))
}

fn publish_images(
    job_file: &Path,
    job: &BatchJob,
    images: Vec<Vec<u8>>,
    mut context: BatchContext,
    http_status: u16,
    request_id: Option<String>,
) -> Result<BatchReport, BatchFailure> {
    let output_dir = PathBuf::from(&job.output_dir);
    let paths = derive_output_paths(&output_dir, &job.output_names);
    let plan = PublishingPlan {
        artifacts: paths
            .iter()
            .zip(&images)
            .map(|(path, image)| PublishingArtifact {
                path: path.to_string_lossy().into_owned(),
                sha256: sha256(image),
            })
            .collect(),
    };
    let publishing_job = transition_if_revision(job_file, job.revision, |job| {
        job.state = JobState::Publishing;
        job.publishing = Some(plan.clone());
        Ok(())
    })
    .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let mut transaction =
        match OutputTransaction::reserve(&output_dir, job.output_names.clone(), job.overwrite) {
            Ok(transaction) => transaction,
            Err(error) => {
                let _ = transition_if_revision(job_file, publishing_job.revision, |job| {
                    job.state = JobState::Failed;
                    job.publishing = None;
                    Ok(())
                });
                return Err(BatchFailure::new(error, context));
            }
        };
    if let Err(mut error) = transaction.stage_all(&images) {
        error.add_possibly_modified_paths(transaction.abort());
        let _ = transition_if_revision(job_file, publishing_job.revision, |job| {
            job.state = JobState::Failed;
            job.publishing = None;
            Ok(())
        });
        return Err(BatchFailure::new(error, context));
    }
    let result = match transaction.commit_all() {
        Ok(result) => result,
        Err(mut error) => {
            error.add_possibly_modified_paths(transaction.abort());
            return Err(BatchFailure::new(error, context));
        }
    };
    let retained_artifacts = result
        .retained_artifacts
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let updated = transition_if_revision(job_file, publishing_job.revision, |job| {
        job.state = JobState::Retrieved;
        // Keep the digest plan so repeated retrieval can verify and report the
        // already-published artifacts without contacting the API again.
        job.retained_artifacts = retained_artifacts.clone();
        Ok(())
    });
    if let Err(error) = updated {
        context.outputs = result
            .outputs
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        context.retained_artifacts = retained_artifacts.clone();
        context.http_status = Some(http_status);
        context.request_id = request_id.clone();
        context.next_action =
            Some("rerun batch retrieve to reconcile the publishing state".to_owned());
        let mut recovery_error = state_persistence_error("output publication", &job.job_id, error);
        recovery_error.set_http_status(http_status);
        recovery_error.set_request_id(request_id);
        return Err(BatchFailure::new(recovery_error, context));
    }
    context.outputs = result
        .outputs
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    context.retained_artifacts = retained_artifacts;
    context.http_status = Some(http_status);
    context.request_id = request_id;
    context.remote_status = Some("retrieved".to_owned());
    Ok(context.report(None))
}

fn recover_publishing(
    job_file: &Path,
    job: &BatchJob,
    mut context: BatchContext,
) -> Result<BatchReport, BatchFailure> {
    let Some(plan) = job.publishing.as_ref() else {
        return Err(BatchFailure::new(
            AppError::preflight(
                "publishing_state_invalid",
                "The job claims publishing is in progress but has no publication plan.",
            ),
            context,
        ));
    };
    let mut outputs = Vec::new();
    for artifact in &plan.artifacts {
        let bytes = read_output_file(Path::new(&artifact.path)).map_err(|error| {
            let mut error = error;
            error.add_possibly_modified_paths(vec![PathBuf::from(&artifact.path)]);
            BatchFailure::new(error, context.clone())
        })?;
        if sha256(&bytes) != artifact.sha256 {
            let error = AppError::preflight(
                "publishing_recovery_required",
                "A published output does not match the persisted digest. No automatic overwrite was attempted; inspect the listed path and job record.",
            );
            context.possibly_modified_paths.push(artifact.path.clone());
            return Err(BatchFailure::new(error, context));
        }
        outputs.push(artifact.path.clone());
    }
    transition_if_revision(job_file, job.revision, |job| {
        job.state = JobState::Retrieved;
        Ok(())
    })
    .map_err(|error| {
        let recovery_error = state_persistence_error("publishing recovery", &job.job_id, error);
        BatchFailure::new(recovery_error, context.clone())
    })?;
    context.outputs = outputs;
    context.remote_status = Some("retrieved".to_owned());
    context.next_action = None;
    Ok(context.report(None))
}

fn recover_retrieved(
    job: &BatchJob,
    mut context: BatchContext,
) -> Result<BatchReport, BatchFailure> {
    let Some(plan) = job.publishing.as_ref() else {
        return Err(BatchFailure::new(
            AppError::preflight(
                "retrieved_state_invalid",
                "The job is marked retrieved but has no publication digest plan to verify.",
            ),
            context,
        ));
    };
    let outputs = verify_publishing_plan(plan, &mut context)?;
    context.outputs = outputs;
    context.remote_status = Some("retrieved".to_owned());
    context.next_action = None;
    Ok(context.report(None))
}

fn verify_publishing_plan(
    plan: &PublishingPlan,
    context: &mut BatchContext,
) -> Result<Vec<String>, BatchFailure> {
    let mut outputs = Vec::with_capacity(plan.artifacts.len());
    for artifact in &plan.artifacts {
        let bytes = read_output_file(Path::new(&artifact.path)).map_err(|error| {
            let mut error = error;
            error.add_possibly_modified_paths(vec![PathBuf::from(&artifact.path)]);
            BatchFailure::new(error, context.clone())
        })?;
        if sha256(&bytes) != artifact.sha256 {
            let error = AppError::preflight(
                "publishing_recovery_required",
                "A published output does not match the persisted digest. No automatic overwrite was attempted; inspect the listed path and job record.",
            );
            context.possibly_modified_paths.push(artifact.path.clone());
            return Err(BatchFailure::new(error, context.clone()));
        }
        outputs.push(artifact.path.clone());
    }
    Ok(outputs)
}

fn parse_batch_output(content: &[u8], job: &BatchJob) -> Result<Vec<Vec<u8>>, BatchFailure> {
    let expected = job.custom_ids.iter().cloned().collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut images = HashMap::new();
    let mut total: usize = 0;
    for line in content.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        total = total.saturating_add(line.len() + 1);
        if line.len() > MAX_RESULT_LINE_BYTES || total > MAX_BATCH_CONTENT_BYTES {
            return Err(batch_result_failure(
                "batch_result_too_large",
                "The Batch output exceeded the local safety limit.",
                job,
            ));
        }
        let result: BatchResultLine = serde_json::from_slice(line).map_err(|_| {
            batch_result_failure(
                "batch_result_invalid_json",
                "The Batch output contained an invalid JSONL record.",
                job,
            )
        })?;
        if !expected.contains(&result.custom_id) || !seen.insert(result.custom_id.clone()) {
            return Err(batch_result_failure(
                "batch_result_ids_invalid",
                "The Batch output contained an unknown or duplicate custom_id.",
                job,
            ));
        }
        if result.error.is_some() {
            return Err(batch_result_failure(
                "batch_item_failed",
                "At least one image request in the Batch failed. No partial output was published.",
                job,
            ));
        }
        let Some(response) = result.response else {
            return Err(batch_result_failure(
                "batch_result_missing_response",
                "The Batch output record did not contain a response.",
                job,
            ));
        };
        if response.status_code != 200 {
            return Err(batch_result_failure(
                "batch_item_http_error",
                "At least one Batch image response was not successful. No partial output was published.",
                job,
            ));
        }
        let body = serde_json::to_vec(&response.body).map_err(|_| {
            batch_result_failure(
                "batch_result_body_invalid",
                "A Batch response body could not be decoded safely.",
                job,
            )
        })?;
        let decoded = decode_images(&body, 1, job.format).map_err(|_| {
            batch_result_failure(
                "batch_image_invalid",
                "A Batch image response was malformed or did not match the requested format. No partial output was published.",
                job,
            )
        })?;
        images.insert(
            result.custom_id,
            decoded.into_iter().next().expect("one image"),
        );
    }
    if seen != expected {
        return Err(batch_result_failure(
            "batch_result_incomplete",
            "The Batch output did not contain exactly one successful result for every requested image.",
            job,
        ));
    }
    job.custom_ids
        .iter()
        .map(|custom_id| {
            images.remove(custom_id).ok_or_else(|| {
                batch_result_failure(
                    "batch_result_incomplete",
                    "The Batch output could not be mapped deterministically to the requested images.",
                    job,
                )
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct BatchResultLine {
    custom_id: String,
    response: Option<BatchItemResponse>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct BatchItemResponse {
    status_code: u16,
    body: serde_json::Value,
}

fn batch_result_failure(code: &'static str, message: &'static str, job: &BatchJob) -> BatchFailure {
    let mut context = context_from_job("batch.retrieve", Path::new(""), job);
    context.attempted = true;
    context.next_action =
        Some("inspect the Batch output/error files before retrying retrieval".to_owned());
    BatchFailure::new(AppError::invalid_response(code, message), context)
}

fn build_batch_input(
    prompt: &str,
    args: &GenerateArgs,
    custom_ids: &[String],
) -> Result<Vec<u8>, AppError> {
    let mut input = Vec::new();
    for custom_id in custom_ids {
        let request = serde_json::json!({
            "custom_id": custom_id,
            "method": "POST",
            "url": "/v1/images/generations",
            "body": ImageGenerationRequest::from_args_with_count(prompt, args, 1),
        });
        serde_json::to_writer(&mut input, &request).map_err(|_| {
            AppError::preflight(
                "batch_input_unavailable",
                "The Batch JSONL input could not be serialized safely.",
            )
        })?;
        input.push(b'\n');
    }
    if input.len() > MAX_BATCH_INPUT_BYTES {
        return Err(AppError::usage(
            "batch_input_too_large",
            "The generated Batch JSONL input exceeded the local safety limit.",
        ));
    }
    Ok(input)
}

fn require_api_provider(args: &GenerateArgs) -> Result<(), AppError> {
    if args.provider != Provider::Api {
        return Err(AppError::usage(
            "batch_provider_unsupported",
            "Batch commands require --provider api. Codex subscription generation is a separate explicit path and is not silently translated to Batch.",
        ));
    }
    Ok(())
}

fn validate_timeout(seconds: u64) -> Result<(), AppError> {
    if !(1..=300).contains(&seconds) {
        return Err(AppError::usage(
            "invalid_timeout",
            "Batch HTTP timeout must be between 1 and 300 seconds.",
        ));
    }
    Ok(())
}

fn validate_wait(args: &BatchRetrieveArgs) -> Result<(), AppError> {
    if !(1..=86_400).contains(&args.max_wait_seconds) {
        return Err(AppError::usage(
            "invalid_batch_wait",
            "--max-wait-seconds must be between 1 and 86400.",
        ));
    }
    if !(1..=3_600).contains(&args.poll_interval_seconds) {
        return Err(AppError::usage(
            "invalid_batch_poll_interval",
            "--poll-interval-seconds must be between 1 and 3600.",
        ));
    }
    Ok(())
}

fn api_key() -> Result<String, AppError> {
    let value = env::var("OPENAI_API_KEY").map_err(|_| {
        AppError::usage(
            "missing_api_key",
            "OPENAI_API_KEY must be set for Batch API operations; it is never read during --dry-run.",
        )
    })?;
    if value.trim().is_empty() {
        return Err(AppError::usage(
            "empty_api_key",
            "OPENAI_API_KEY is empty; set a non-empty key in the environment.",
        ));
    }
    Ok(value)
}

fn parse_batch_info(
    body: &[u8],
    expected_batch_id: Option<&str>,
    expected_input_file_id: Option<&str>,
) -> Result<BatchInfo, AppError> {
    let info = serde_json::from_slice(body).map_err(|_| {
        AppError::observation(
            "batch_status_invalid",
            "The Batch endpoint returned an invalid status object; retrying the read-only operation is safe.",
        )
    })?;
    validate_batch_info(&info, expected_batch_id, expected_input_file_id)?;
    Ok(info)
}

fn parse_batch_create_info(
    body: &[u8],
    expected_input_file_id: &str,
) -> Result<BatchInfo, AppError> {
    let info = serde_json::from_slice(body).map_err(|_| {
        AppError::invalid_response(
            "batch_create_invalid",
            "The batch-create response did not contain a valid Batch object; do not retry automatically. Inspect the persisted job.",
        )
    })?;
    validate_batch_info(&info, None, Some(expected_input_file_id)).map_err(|_| {
        AppError::invalid_response(
            "batch_create_invalid",
            "The batch-create response failed local identity or status validation; do not retry automatically. Inspect the persisted job.",
        )
    })?;
    Ok(info)
}

fn validate_batch_info(
    info: &BatchInfo,
    expected_batch_id: Option<&str>,
    expected_input_file_id: Option<&str>,
) -> Result<(), AppError> {
    validate_remote_id(&info.id, "batch_id").map_err(|_| {
        AppError::observation(
            "batch_status_invalid",
            "The Batch endpoint returned an unsafe batch ID; retrying the read-only operation is safe.",
        )
    })?;
    validate_remote_id(&info.input_file_id, "input_file_id").map_err(|_| {
        AppError::observation(
            "batch_status_invalid",
            "The Batch endpoint returned an unsafe input file ID; retrying the read-only operation is safe.",
        )
    })?;
    if expected_batch_id.is_some_and(|expected| expected != info.id) {
        return Err(AppError::observation(
            "batch_identity_mismatch",
            "The Batch endpoint returned a different batch ID than the persisted job; retrying the read-only operation is safe.",
        ));
    }
    if expected_input_file_id.is_some_and(|expected| expected != info.input_file_id) {
        return Err(AppError::observation(
            "batch_input_identity_mismatch",
            "The Batch endpoint returned a different input file ID than the persisted job; retrying the read-only operation is safe.",
        ));
    }
    if !is_known_remote_status(&info.status) {
        return Err(AppError::observation(
            "batch_status_invalid",
            "The Batch endpoint returned an unknown status; retrying the read-only operation is safe.",
        ));
    }
    for (file_id, name) in [
        (info.output_file_id.as_deref(), "output_file_id"),
        (info.error_file_id.as_deref(), "error_file_id"),
    ] {
        if let Some(file_id) = file_id {
            validate_remote_id(file_id, name).map_err(|_| {
                AppError::observation(
                    "batch_status_invalid",
                    "The Batch endpoint returned an unsafe output file ID; retrying the read-only operation is safe.",
                )
            })?;
        }
    }
    if let Some(counts) = &info.request_counts {
        if counts.completed > counts.total
            || counts.failed > counts.total
            || counts.completed.saturating_add(counts.failed) > counts.total
        {
            return Err(AppError::observation(
                "batch_counts_invalid",
                "The Batch endpoint returned inconsistent request counts; retrying the read-only operation is safe.",
            ));
        }
    }
    Ok(())
}

fn validate_remote_transition(job: &BatchJob, info: &BatchInfo) -> Result<(), AppError> {
    validate_observation_progress(
        job.remote_status.as_deref(),
        job.output_file_id.as_deref(),
        job.error_file_id.as_deref(),
        job.request_counts.as_ref(),
        info,
    )
}

fn validate_observation_progress(
    previous_status: Option<&str>,
    previous_output_file_id: Option<&str>,
    previous_error_file_id: Option<&str>,
    previous_counts: Option<&BatchRequestCounts>,
    info: &BatchInfo,
) -> Result<(), AppError> {
    if let Some(previous_status) = previous_status {
        if !is_allowed_remote_transition(previous_status, &info.status) {
            return Err(AppError::observation(
                "batch_status_regressed",
                "The Batch endpoint returned a stale or inconsistent remote status; retrying the read-only operation is safe.",
            ));
        }
    }
    if previous_output_file_id.is_some()
        && info.output_file_id.as_deref() != previous_output_file_id
    {
        return Err(AppError::observation(
            "batch_output_id_changed",
            "The Batch endpoint changed a previously confirmed output file ID; retrying the read-only operation is safe.",
        ));
    }
    if previous_error_file_id.is_some() && info.error_file_id.as_deref() != previous_error_file_id {
        return Err(AppError::observation(
            "batch_error_id_changed",
            "The Batch endpoint changed a previously confirmed error file ID; retrying the read-only operation is safe.",
        ));
    }
    if let (Some(previous), Some(current)) = (previous_counts, info.request_counts.as_ref()) {
        if current.total < previous.total
            || current.completed < previous.completed
            || current.failed < previous.failed
        {
            return Err(AppError::observation(
                "batch_counts_regressed",
                "The Batch endpoint returned decreasing request counts; retrying the read-only operation is safe.",
            ));
        }
    } else if previous_counts.is_some() && info.request_counts.is_none() {
        return Err(AppError::observation(
            "batch_counts_disappeared",
            "The Batch endpoint omitted previously confirmed request counts; retrying the read-only operation is safe.",
        ));
    }
    Ok(())
}

fn is_allowed_remote_transition(previous: &str, current: &str) -> bool {
    if previous == current {
        return true;
    }
    match previous {
        "validating" => matches!(
            current,
            "in_progress"
                | "finalizing"
                | "cancelling"
                | "completed"
                | "failed"
                | "expired"
                | "cancelled"
        ),
        "in_progress" => matches!(
            current,
            "finalizing" | "cancelling" | "completed" | "failed" | "expired" | "cancelled"
        ),
        "finalizing" => matches!(current, "completed" | "failed" | "expired"),
        "cancelling" => matches!(current, "completed" | "failed" | "expired" | "cancelled"),
        "completed" | "failed" | "expired" | "cancelled" => false,
        _ => false,
    }
}

fn terminal_batch_error(status: &str) -> AppError {
    if status == "expired" {
        AppError::batch_failed(
            "batch_expired",
            "The Batch expired before completion. Inspect the persisted error file and remote Batch record; do not resubmit automatically.",
        )
    } else {
        AppError::batch_failed(
            "batch_failed",
            "The Batch reached a failed terminal state. Inspect the persisted error file and remote Batch record before deciding what to do next.",
        )
    }
}

fn is_known_remote_status(status: &str) -> bool {
    matches!(
        status,
        "validating"
            | "in_progress"
            | "finalizing"
            | "completed"
            | "failed"
            | "expired"
            | "cancelling"
            | "cancelled"
    )
}

fn state_for_remote_status(status: &str) -> JobState {
    match status {
        "completed" => JobState::Completed,
        "failed" | "expired" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        _ => JobState::Submitted,
    }
}

fn set_context_from_info(context: &mut BatchContext, info: &BatchInfo) {
    context.batch_id = Some(info.id.clone());
    context.input_file_id = Some(info.input_file_id.clone());
    context.remote_status = Some(info.status.clone());
    context.output_file_id = info.output_file_id.clone();
    context.error_file_id = info.error_file_id.clone();
}

fn is_terminal_remote_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "expired" | "cancelled")
}

fn endpoint_from_job(
    job: &BatchJob,
    context: &BatchContext,
    args: &BatchJobArgs,
) -> Result<Endpoint, BatchFailure> {
    Endpoint::authorize(
        &job.api_base_url,
        args.dangerously_allow_api_key_to.as_deref(),
        args.allow_insecure_localhost,
    )
    .map_err(|error| BatchFailure::new(error, context.clone()))
}

fn require_batch_id(job: &BatchJob, context: &BatchContext) -> Result<String, BatchFailure> {
    job.batch_id.clone().ok_or_else(|| {
        BatchFailure::new(
            AppError::preflight(
                "batch_id_unavailable",
                "This job has no confirmed batch ID. Do not resubmit automatically; inspect its state and reconcile the remote account first.",
            ),
            context.clone(),
        )
    })
}

fn load_job(
    args: &BatchJobArgs,
    operation: &'static str,
) -> Result<(PathBuf, BatchJob), BatchFailure> {
    let path = JobStore::resolve(Some(&args.job_file), "unused")
        .map_err(|error| failure(error, operation, None))?;
    let job = JobStore::load(&path)
        .map_err(|error| failure(error, operation, Some(job_context(operation, &path))))?;
    Ok((path, job))
}

fn transition<F>(path: &Path, update: F) -> Result<BatchJob, AppError>
where
    F: FnOnce(&mut BatchJob) -> Result<(), AppError>,
{
    JobStore::update(path, update)
}

fn transition_if_revision<F>(
    path: &Path,
    expected_revision: u64,
    update: F,
) -> Result<BatchJob, AppError>
where
    F: FnOnce(&mut BatchJob) -> Result<(), AppError>,
{
    JobStore::update_if_revision(path, Some(expected_revision), update)
}

fn mark_unknown(
    path: &Path,
    expected_revision: u64,
    state: JobState,
    mut error: AppError,
) -> AppError {
    if let Err(state_error) = transition_if_revision(path, expected_revision, |job| {
        job.state = state;
        Ok(())
    }) {
        error.message = format!(
            "{} The remote operation outcome remains unknown and the local job state could not be persisted: {}",
            error.message, state_error.message
        );
        error.automatic_retry_safe = false;
    }
    error
}

fn state_persistence_error(resource: &str, resource_id: &str, error: AppError) -> AppError {
    AppError::indeterminate(
        "batch_state_persistence_failed",
        format!(
            "The remote {resource} succeeded with confirmed ID {resource_id}, but the local recovery state could not be persisted: {}. Do not resubmit; reconcile the remote resource and job file.",
            error.message
        ),
    )
}

fn cancel_outcome_unknown(error: AppError, response: &crate::api::ApiResponse) -> AppError {
    let mut unknown = AppError::indeterminate(
        "batch_cancel_outcome_unknown",
        format!(
            "The batch cancel POST returned an invalid success response: {}. Query batch status before trying again.",
            error.message
        ),
    );
    unknown.set_http_status(response.status);
    unknown.set_request_id(response.request_id.clone());
    unknown
}

fn context_for_job(
    base: &BatchContext,
    job_id: &str,
    batch_id: Option<&str>,
    job_file: Option<&Path>,
) -> BatchContext {
    let mut context = base.clone();
    context.job_id = Some(job_id.to_owned());
    context.batch_id = batch_id.map(str::to_owned);
    context.job_file = job_file.map(|path| path.to_string_lossy().into_owned());
    context
}

fn context_from_job(operation: &'static str, path: &Path, job: &BatchJob) -> BatchContext {
    BatchContext {
        operation,
        job_file: Some(path.to_string_lossy().into_owned()),
        job_id: Some(job.job_id.clone()),
        batch_id: job.batch_id.clone(),
        input_file_id: job.input_file_id.clone(),
        output_file_id: job.output_file_id.clone(),
        error_file_id: job.error_file_id.clone(),
        remote_status: job.remote_status.clone(),
        image_count: job.image_count,
        attempted: false,
        retained_artifacts: job.retained_artifacts.clone(),
        ..BatchContext::default()
    }
}

fn output_names_for_context(output_dir: &str, output_names: &[String]) -> Vec<String> {
    output_names
        .iter()
        .map(|name| {
            Path::new(output_dir)
                .join(name)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn job_context(operation: &'static str, path: &Path) -> BatchContext {
    BatchContext {
        operation,
        job_file: Some(path.to_string_lossy().into_owned()),
        ..BatchContext::default()
    }
}

fn failure(
    error: AppError,
    operation: &'static str,
    context: Option<BatchContext>,
) -> BatchFailure {
    BatchFailure::new(
        error,
        context.unwrap_or(BatchContext {
            operation,
            ..BatchContext::default()
        }),
    )
}

fn dry_run_report(mut context: BatchContext) -> BatchReport {
    context.attempted = false;
    let mut report = context.report(None);
    report.status = "dry_run";
    report.next_action = context.next_action;
    report
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|_| {
                AppError::preflight(
                    "working_directory_unavailable",
                    "The current directory could not be resolved.",
                )
            })
    }
}

fn new_job_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("job-{:016x}", hasher.finish())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_output_file(path: &Path) -> Result<Vec<u8>, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        AppError::preflight(
            "publishing_output_missing",
            "A persisted publishing output is missing; no automatic overwrite was attempted.",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(AppError::preflight(
            "publishing_output_unsafe",
            "A persisted publishing output is not a regular non-symlink file.",
        ));
    }
    if metadata.len() > crate::image::MAX_IMAGE_BYTES as u64 {
        return Err(AppError::preflight(
            "publishing_output_too_large",
            "A persisted publishing output exceeds the local image safety limit.",
        ));
    }
    fs::read(path).map_err(|_| {
        AppError::preflight(
            "publishing_output_unreadable",
            "A persisted publishing output could not be read safely.",
        )
    })
}

struct JobStore;

fn validate_job(job: &BatchJob) -> Result<(), AppError> {
    if job.revision == u64::MAX {
        return Err(invalid_job("The Batch job revision is exhausted."));
    }
    if job.provider != Provider::Api
        || job.model != MODEL
        || !(1..=crate::cli::MAX_BATCH_IMAGES).contains(&job.image_count)
    {
        return Err(invalid_job(
            "The Batch job provider, model, or image count is invalid.",
        ));
    }
    if !Path::new(&job.output_dir).is_absolute()
        || Path::new(&job.output_dir)
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid_job(
            "The Batch job output directory must be an absolute path without '..'.",
        ));
    }
    validate_no_symlink_components(Path::new(&job.output_dir))?;
    if job.output_names.len() != usize::from(job.image_count)
        || job.custom_ids.len() != usize::from(job.image_count)
    {
        return Err(invalid_job(
            "The Batch job output and custom-ID counts do not match image_count.",
        ));
    }
    let extension = format!(".{}", job.format.extension());
    let mut output_names = HashSet::new();
    for name in &job.output_names {
        let path = Path::new(name);
        if path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
            || name.len() > 100
            || !name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
            || !name.ends_with(&extension)
            || !output_names.insert(name)
        {
            return Err(invalid_job(
                "The Batch job contains an unsafe, duplicate, or format-mismatched output name.",
            ));
        }
    }
    let mut custom_ids = HashSet::new();
    for custom_id in &job.custom_ids {
        if validate_remote_id(custom_id, "custom_id").is_err() || !custom_ids.insert(custom_id) {
            return Err(invalid_job(
                "The Batch job contains an unsafe or duplicate custom ID.",
            ));
        }
    }
    for (id, name) in [
        (job.input_file_id.as_deref(), "input_file_id"),
        (job.batch_id.as_deref(), "batch_id"),
        (job.output_file_id.as_deref(), "output_file_id"),
        (job.error_file_id.as_deref(), "error_file_id"),
    ] {
        if let Some(id) = id {
            validate_remote_id(id, name)
                .map_err(|_| invalid_job("The Batch job contains an unsafe remote ID."))?;
        }
    }
    if let Some(status) = job.remote_status.as_deref() {
        if !is_known_remote_status(status) && status != "retrieved" {
            return Err(invalid_job(
                "The Batch job contains an unknown remote status.",
            ));
        }
    }
    if let Some(counts) = &job.request_counts {
        if counts.completed > counts.total
            || counts.failed > counts.total
            || counts.completed.saturating_add(counts.failed) > counts.total
        {
            return Err(invalid_job(
                "The Batch job contains inconsistent request counts.",
            ));
        }
    }
    if let Some(plan) = &job.publishing {
        if !matches!(job.state, JobState::Publishing | JobState::Retrieved)
            || plan.artifacts.len() != job.output_names.len()
        {
            return Err(invalid_job(
                "The Batch job publication plan is inconsistent.",
            ));
        }
        for (artifact, name) in plan.artifacts.iter().zip(&job.output_names) {
            let expected = Path::new(&job.output_dir).join(name);
            if artifact.path != expected.to_string_lossy()
                || artifact.sha256.len() != 64
                || !artifact
                    .sha256
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return Err(invalid_job(
                    "The Batch job publication digest plan is invalid.",
                ));
            }
        }
    } else if job.state == JobState::Retrieved {
        return Err(invalid_job(
            "A retrieved Batch job must retain its publication digest plan.",
        ));
    }
    for artifact in &job.retained_artifacts {
        let path = Path::new(artifact);
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            || !path.starts_with(&job.output_dir)
        {
            return Err(invalid_job(
                "The Batch job retained-artifact path is invalid.",
            ));
        }
    }
    Ok(())
}

fn invalid_job(message: &'static str) -> AppError {
    AppError::preflight("job_file_invalid", message)
}

impl JobStore {
    fn resolve(path: Option<&Path>, job_id: &str) -> Result<PathBuf, AppError> {
        let path = match path {
            Some(path) => absolute_path(path)?,
            None => default_job_directory()?.join(format!("{job_id}.json")),
        };
        validate_no_symlink_components(&path)?;
        Ok(path)
    }

    fn create(path: &Path, job: &BatchJob) -> Result<(), AppError> {
        validate_job(job)?;
        ensure_parent(path)?;
        let _lock = Self::lock(path)?;
        if path.exists() {
            return Err(AppError::preflight(
                "job_file_exists",
                "The requested job file already exists; refusing to replace an existing Batch record.",
            ));
        }
        write_atomic(path, job)
    }

    fn load(path: &Path) -> Result<BatchJob, AppError> {
        let bytes = fs::read(path).map_err(|_| {
            AppError::preflight(
                "job_file_unreadable",
                "The Batch job record could not be read.",
            )
        })?;
        let job: BatchJob = serde_json::from_slice(&bytes).map_err(|_| {
            AppError::preflight(
                "job_file_invalid",
                "The Batch job record is invalid or truncated; no remote operation was attempted.",
            )
        })?;
        if job.schema_version != JOB_SCHEMA_VERSION {
            return Err(AppError::preflight(
                "job_schema_unsupported",
                "The Batch job record uses an unsupported schema version.",
            ));
        }
        validate_job(&job)?;
        Ok(job)
    }

    fn update<F>(path: &Path, update: F) -> Result<BatchJob, AppError>
    where
        F: FnOnce(&mut BatchJob) -> Result<(), AppError>,
    {
        Self::update_if_revision(path, None, update)
    }

    fn update_if_revision<F>(
        path: &Path,
        expected_revision: Option<u64>,
        update: F,
    ) -> Result<BatchJob, AppError>
    where
        F: FnOnce(&mut BatchJob) -> Result<(), AppError>,
    {
        let _lock = Self::lock(path)?;
        let mut job = Self::load(path)?;
        if expected_revision.is_some_and(|expected| expected != job.revision) {
            return Err(AppError::preflight(
                "job_changed_concurrently",
                "The Batch job changed while this operation was in progress; inspect its current state before retrying.",
            ));
        }
        update(&mut job)?;
        job.revision = job.revision.checked_add(1).ok_or_else(|| {
            AppError::preflight(
                "job_revision_exhausted",
                "The Batch job revision is exhausted; no state update was written.",
            )
        })?;
        job.updated_at = now_seconds();
        validate_job(&job)?;
        write_atomic(path, &job)?;
        Ok(job)
    }

    fn lock(path: &Path) -> Result<File, AppError> {
        ensure_parent(path)?;
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        if fs::symlink_metadata(&lock_path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(AppError::preflight(
                "job_lock_unsafe",
                "The Batch job lock path is a symlink; refusing to use it.",
            ));
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|_| {
                AppError::preflight(
                    "job_lock_unavailable",
                    "The Batch job could not be locked safely.",
                )
            })?;
        lock.lock_exclusive().map_err(|_| {
            AppError::preflight(
                "job_lock_unavailable",
                "Another Batch operation is updating this job; retry after it exits.",
            )
        })?;
        Ok(lock)
    }
}

fn default_job_directory() -> Result<PathBuf, AppError> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| {
            AppError::preflight(
                "config_directory_unavailable",
                "A config directory could not be determined for Batch job recovery.",
            )
        })?;
    Ok(config.join("codex-image").join("jobs"))
}

fn ensure_parent(path: &Path) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::preflight(
            "job_path_invalid",
            "The Batch job path has no parent directory.",
        )
    })?;
    fs::create_dir_all(parent).map_err(|_| {
        AppError::preflight(
            "job_directory_unavailable",
            "The Batch job directory could not be created.",
        )
    })?;
    validate_no_symlink_components(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|_| {
            AppError::preflight(
                "job_directory_unavailable",
                "The Batch job directory permissions could not be restricted.",
            )
        })?;
    }
    Ok(())
}

fn validate_no_symlink_components(path: &Path) -> Result<(), AppError> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        if matches!(component, Component::RootDir | Component::Prefix(_)) {
            current.push(component.as_os_str());
            continue;
        }
        current.push(component.as_os_str());
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err(AppError::preflight(
                    "job_path_symlink",
                    "Batch job paths cannot contain symlinked components.",
                ));
            }
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, value: &BatchJob) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::preflight(
            "job_path_invalid",
            "The Batch job path has no parent directory.",
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| {
        AppError::preflight(
            "job_write_failed",
            "The Batch job record could not be staged safely.",
        )
    })?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| {
        AppError::preflight(
            "job_write_failed",
            "The Batch job record could not be serialized safely.",
        )
    })?;
    temporary.write_all(&bytes).map_err(|_| {
        AppError::preflight(
            "job_write_failed",
            "The Batch job record could not be written safely.",
        )
    })?;
    temporary.as_file().sync_all().map_err(|_| {
        AppError::preflight(
            "job_write_failed",
            "The Batch job record could not be synchronized safely.",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o600)).map_err(|_| {
            AppError::preflight(
                "job_write_failed",
                "The Batch job record permissions could not be restricted.",
            )
        })?;
    }
    temporary.persist(path).map_err(|_| {
        AppError::preflight(
            "job_write_failed",
            "The Batch job record could not be committed atomically.",
        )
    })?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_ids_are_safe() {
        let id = new_job_id();
        assert!(id.starts_with("job-"));
        assert!(id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'));
    }

    #[test]
    fn job_state_round_trips_without_prompt_data() {
        let job = BatchJob {
            schema_version: JOB_SCHEMA_VERSION,
            revision: 0,
            job_id: "job-test".to_owned(),
            state: JobState::Prepared,
            provider: Provider::Api,
            model: MODEL.to_owned(),
            api_base_url: "https://api.openai.com/v1".to_owned(),
            output_dir: "/tmp/images".to_owned(),
            output_names: vec!["one.png".to_owned()],
            overwrite: false,
            format: OutputFormat::Png,
            image_count: 1,
            quality: crate::cli::Quality::Low,
            size: "auto".to_owned(),
            background: crate::cli::Background::Auto,
            moderation: crate::cli::Moderation::Auto,
            custom_ids: vec!["job-test-00".to_owned()],
            input_file_id: None,
            batch_id: None,
            output_file_id: None,
            error_file_id: None,
            remote_status: None,
            request_counts: None,
            publishing: None,
            retained_artifacts: Vec::new(),
            created_at: 0,
            updated_at: 0,
        };
        let value = serde_json::to_value(&job).unwrap();
        assert!(value.get("prompt").is_none());
        assert_eq!(
            serde_json::from_value::<BatchJob>(value).unwrap().job_id,
            "job-test"
        );
    }

    #[test]
    fn loaded_jobs_reject_path_escape_output_names() {
        let mut job = BatchJob {
            schema_version: JOB_SCHEMA_VERSION,
            revision: 0,
            job_id: "job-test".to_owned(),
            state: JobState::Prepared,
            provider: Provider::Api,
            model: MODEL.to_owned(),
            api_base_url: "https://api.openai.com/v1".to_owned(),
            output_dir: "/tmp/images".to_owned(),
            output_names: vec!["one.png".to_owned()],
            overwrite: false,
            format: OutputFormat::Png,
            image_count: 1,
            quality: crate::cli::Quality::Low,
            size: "auto".to_owned(),
            background: crate::cli::Background::Auto,
            moderation: crate::cli::Moderation::Auto,
            custom_ids: vec!["job-test-00".to_owned()],
            input_file_id: None,
            batch_id: None,
            output_file_id: None,
            error_file_id: None,
            remote_status: None,
            request_counts: None,
            publishing: None,
            retained_artifacts: Vec::new(),
            created_at: 0,
            updated_at: 0,
        };
        job.output_names[0] = "../escape.png".to_owned();
        assert!(validate_job(&job).is_err());
    }

    #[test]
    fn cancellation_can_skip_the_intermediate_remote_status() {
        assert!(is_allowed_remote_transition("in_progress", "cancelled"));
        assert!(!is_allowed_remote_transition("cancelling", "finalizing"));
    }
}
