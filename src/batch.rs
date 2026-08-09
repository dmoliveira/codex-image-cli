use std::{
    collections::HashSet,
    env,
    fs::{self, File},
    io::Write,
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use crate::{
    api::{
        ApiClient, BatchCreateRequest, BatchInfo, BatchRequestCounts, FileInfo,
        ImageGenerationRequest, OutputExpiresAfter, MAX_BATCH_CONTENT_BYTES, MAX_BATCH_INPUT_BYTES,
    },
    cli::{
        BatchCancelArgs, BatchJobArgs, BatchRecoverArgs, BatchRetrieveArgs, BatchSubmitArgs,
        GenerateArgs, OutputFormat, Provider,
    },
    endpoint::{validate_remote_id, Endpoint},
    image::decode_base64_image,
    manifest::ManifestAsset,
    output::{
        derive_file_names, derive_output_paths, inspect_recovery_plan, read_regular_file,
        read_regular_file_with_identity, verify_and_sync_plan, verify_regular_file_identity,
        OutputIdentity, OutputTransaction, OutputVerificationArtifact, RecoveryArtifact,
        RecoveryVerificationArtifact, RetainedVerificationArtifact,
    },
    report::{AppError, BatchContext, BatchReport, BatchRequestCountsReport},
    MODEL,
};

pub const JOB_SCHEMA_VERSION: u8 = 7;
const MAX_JOB_FILE_BYTES: usize = 256 * 1024;
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
#[serde(deny_unknown_fields)]
pub struct BatchJob {
    pub schema_version: u8,
    pub revision: u64,
    pub job_id: String,
    pub state_path: String,
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
    #[serde(default)]
    pub compression: Option<u8>,
    pub moderation: crate::cli::Moderation,
    pub custom_ids: Vec<String>,
    pub input_sha256: String,
    pub input_bytes: u64,
    pub input_file_id: Option<String>,
    pub batch_id: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub remote_status: Option<String>,
    pub request_counts: Option<PersistedBatchRequestCounts>,
    pub publishing: Option<PublishingPlan>,
    pub retained_artifacts: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedBatchRequestCounts {
    pub completed: u32,
    pub failed: u32,
    pub total: u32,
}

impl From<BatchRequestCounts> for PersistedBatchRequestCounts {
    fn from(counts: BatchRequestCounts) -> Self {
        Self {
            completed: counts.completed,
            failed: counts.failed,
            total: counts.total,
        }
    }
}

impl From<&PersistedBatchRequestCounts> for BatchRequestCounts {
    fn from(counts: &PersistedBatchRequestCounts) -> Self {
        Self {
            completed: counts.completed,
            failed: counts.failed,
            total: counts.total,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishingPlan {
    pub artifacts: Vec<PublishingArtifact>,
    pub staged_artifacts: Vec<String>,
    pub retained_artifacts: Vec<String>,
    pub retained_artifact_ids: Vec<OutputIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishingArtifact {
    pub path: String,
    pub sha256: String,
    pub staged_path: String,
    pub staged_identity: OutputIdentity,
    pub expected_target: Option<OutputIdentity>,
}

#[derive(Debug)]
pub struct BatchFailure {
    pub error: Box<AppError>,
    pub context: Box<BatchContext>,
}

#[derive(Debug, Clone)]
struct BatchAssetInput {
    prompt: String,
    output_name: String,
}

impl BatchFailure {
    fn new(error: AppError, context: BatchContext) -> Self {
        Self {
            error: Box::new(error),
            context: Box::new(context),
        }
    }
}

fn attach_response_metadata(
    failure: &mut BatchFailure,
    http_status: u16,
    request_id: Option<&str>,
) {
    let request_id = request_id.map(str::to_owned);
    failure.error.set_http_status(http_status);
    failure.error.set_request_id(request_id.clone());
    failure.context.http_status = Some(http_status);
    failure.context.request_id = request_id;
}

fn retain_artifact(
    paths: &mut Vec<String>,
    identities: &mut Vec<OutputIdentity>,
    path: String,
    identity: OutputIdentity,
) {
    if !paths.contains(&path) {
        paths.push(path);
        identities.push(identity);
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
    let assets = output_names
        .into_iter()
        .map(|output_name| BatchAssetInput {
            prompt: prompt.clone(),
            output_name,
        })
        .collect();
    submit_prepared(generation, assets, args.job_file.clone())
}

pub fn submit_manifest(
    generation: &GenerateArgs,
    assets: &[ManifestAsset],
    job_file: &Path,
) -> Result<BatchReport, BatchFailure> {
    if assets.is_empty() || assets.len() > usize::from(crate::cli::MAX_BATCH_IMAGES) {
        return Err(failure(
            AppError::usage(
                "invalid_batch_shard_size",
                "A Batch shard must contain between 1 and 8 assets.",
            ),
            "batch.submit",
            None,
        ));
    }
    let mut generation = generation.clone();
    generation.n = assets.len() as u8;
    generation.prompt = Some(assets[0].prompt.clone());
    generation.prompt_file = None;
    generation.name = None;
    generation.prefix = None;
    generation
        .validate_batch(&assets[0].prompt)
        .map_err(|error| failure(error, "batch.submit", None))?;
    for asset in assets {
        let mut single_generation = generation.clone();
        single_generation.n = 1;
        single_generation
            .validate(&asset.prompt)
            .map_err(|error| failure(error, "batch.submit", None))?;
    }
    require_api_provider(&generation).map_err(|error| failure(error, "batch.submit", None))?;
    let inputs = assets
        .iter()
        .map(|asset| BatchAssetInput {
            prompt: asset.prompt.clone(),
            output_name: asset.output_name(generation.format),
        })
        .collect();
    submit_prepared(generation, inputs, Some(job_file.to_owned()))
}

pub fn inspect_job(path: &Path) -> Result<BatchJob, AppError> {
    let path = JobStore::resolve(Some(path), "inspect")?;
    JobStore::load(&path)
}

pub fn input_fingerprint(
    generation: &GenerateArgs,
    assets: &[ManifestAsset],
    custom_ids: &[String],
) -> Result<(String, u64), AppError> {
    if assets.len() != custom_ids.len() {
        return Err(AppError::preflight(
            "run_state_invalid",
            "The child Batch custom-ID count does not match the approved shard.",
        ));
    }
    let inputs = assets
        .iter()
        .map(|asset| BatchAssetInput {
            prompt: asset.prompt.clone(),
            output_name: asset.output_name(generation.format),
        })
        .collect::<Vec<_>>();
    let bytes = build_batch_input(&inputs, generation, custom_ids)?;
    Ok((sha256(&bytes), bytes.len() as u64))
}

fn submit_prepared(
    generation: GenerateArgs,
    assets: Vec<BatchAssetInput>,
    job_file_arg: Option<PathBuf>,
) -> Result<BatchReport, BatchFailure> {
    let output_names = assets
        .iter()
        .map(|asset| asset.output_name.clone())
        .collect::<Vec<_>>();
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
    let job_file = JobStore::resolve(job_file_arg.as_deref(), &job_id).map_err(|error| {
        failure(
            error,
            "batch.submit",
            Some(job_context("batch.submit", Path::new(""))),
        )
    })?;
    let custom_ids = (0..generation.n)
        .map(|index| format!("{job_id}-{index:02}"))
        .collect::<Vec<_>>();
    let input = build_batch_input(&assets, &generation, &custom_ids).map_err(|error| {
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
        state_path: job_file.to_string_lossy().into_owned(),
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
        compression: generation.compression,
        moderation: generation.moderation,
        custom_ids,
        input_sha256: sha256(&input),
        input_bytes: input.len() as u64,
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

    ensure_billable_platform().map_err(|error| {
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
            let mut response_error = AppError::invalid_response(
                "batch_input_upload_invalid",
                "The file-upload response did not contain a safe file ID; do not retry automatically. Inspect the job and account files.",
            );
            response_error.set_http_status(upload.status);
            response_error.set_request_id(upload.request_id.clone());
            let error = mark_unknown(
                &job_file,
                upload_in_flight.revision,
                JobState::UploadOutcomeUnknown,
                response_error,
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
            Some(context_for_input_job(
                &base_context,
                &job_id,
                &file_info.id,
                None,
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
            Some(context_for_input_job(
                &base_context,
                &job_id,
                &file_info.id,
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
                Some(context_for_input_job(
                    &base_context,
                    &job_id,
                    &file_info.id,
                    None,
                    Some(&job_file),
                )),
            ));
        }
    };
    let batch_info: BatchInfo =
        match parse_batch_create_info(&created.body, &file_info.id, generation.n) {
            Ok(info) => info,
            Err(error) => {
                let mut error = error;
                error.set_http_status(created.status);
                error.set_request_id(created.request_id.clone());
                let error = mark_unknown(
                    &job_file,
                    create_in_flight.revision,
                    JobState::CreateOutcomeUnknown,
                    error,
                );
                return Err(failure(
                    error,
                    "batch.submit",
                    Some(context_for_input_job(
                        &base_context,
                        &job_id,
                        &file_info.id,
                        None,
                        Some(&job_file),
                    )),
                ));
            }
        };
    job = transition_if_revision(&job_file, create_in_flight.revision, |job| {
        job.state = state_for_remote_status(&batch_info.status);
        job.batch_id = Some(batch_info.id.clone());
        job.remote_status = Some(batch_info.status.clone());
        job.output_file_id = batch_info.output_file_id.clone();
        job.error_file_id = batch_info.error_file_id.clone();
        job.request_counts = batch_info
            .request_counts
            .clone()
            .map(PersistedBatchRequestCounts::from);
        Ok(())
    })
    .map_err(|error| {
        let mut recovery_error = state_persistence_error("batch creation", &batch_info.id, error);
        recovery_error.set_http_status(created.status);
        recovery_error.set_request_id(created.request_id.clone());
        failure(
            recovery_error,
            "batch.submit",
            Some(context_for_input_job(
                &base_context,
                &job_id,
                &file_info.id,
                Some(&batch_info.id),
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
    report_context.request_counts = batch_info
        .request_counts
        .as_ref()
        .map(request_counts_report);
    report_context.http_status = Some(created.status);
    report_context.request_id = created.request_id;
    if matches!(job.remote_status.as_deref(), Some("failed" | "expired")) {
        report_context.next_action =
            Some(terminal_batch_next_action(job.output_file_id.as_deref()).to_owned());
        return Err(BatchFailure::new(
            terminal_batch_error(job.remote_status.as_deref().unwrap_or_default()),
            report_context,
        ));
    }
    report_context.next_action =
        Some("run batch status or batch retrieve with this job file".to_owned());
    Ok(report_context.report(None))
}

pub fn recover(args: &BatchRecoverArgs) -> Result<BatchReport, BatchFailure> {
    validate_timeout(args.job.timeout_seconds)
        .map_err(|error| failure(error, "batch.recover", None))?;
    let (job_file, mut job) = load_job(&args.job, "batch.recover")?;
    let mut context = context_from_job("batch.recover", &job_file, &job);

    if let Some(input_file_id) = args.input_file_id.as_deref() {
        validate_remote_id(input_file_id, "input_file_id")
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        if !matches!(
            job.state,
            JobState::UploadInFlight | JobState::UploadOutcomeUnknown
        ) {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "batch_input_recovery_not_applicable",
                    "--input-file-id is only valid for a job whose upload outcome is unknown.",
                ),
                context,
            ));
        }
        let endpoint = endpoint_from_job(&job, &context, &args.job)?;
        let key = api_key().map_err(|error| BatchFailure::new(error, context.clone()))?;
        let client = ApiClient::new(args.job.timeout_seconds)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        context.attempted = true;
        let response = client
            .get_file(&endpoint, &key, input_file_id)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        let file_info: FileInfo = serde_json::from_slice(&response.body).map_err(|_| {
            let mut failure = BatchFailure::new(
                AppError::observation(
                    "batch_input_file_invalid",
                    "The confirmed input file lookup returned an invalid file object; retrying the read-only operation is safe.",
                ),
                context.clone(),
            );
            attach_response_metadata(&mut failure, response.status, response.request_id.as_deref());
            failure
        })?;
        if file_info.id != input_file_id || file_info.purpose.as_deref() != Some("batch") {
            let mut failure = BatchFailure::new(
                AppError::observation(
                    "batch_input_file_mismatch",
                    "The confirmed input file is not a Batch-purpose file owned by the expected remote ID; retrying the read-only operation is safe.",
                ),
                context.clone(),
            );
            attach_response_metadata(
                &mut failure,
                response.status,
                response.request_id.as_deref(),
            );
            return Err(failure);
        }
        let content = client
            .get_input_file_content(&endpoint, &key, input_file_id)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        context.http_status = Some(content.status);
        context.request_id = content.request_id;
        if content.body.len() as u64 != job.input_bytes || sha256(&content.body) != job.input_sha256
        {
            return Err(BatchFailure::new(
                AppError::observation(
                    "batch_input_file_content_mismatch",
                    "The confirmed input file content does not match the persisted request fingerprint; retrying the read-only operation is safe.",
                ),
                context,
            ));
        }
        job = transition_if_revision(&job_file, job.revision, |job| {
            job.state = JobState::InputUploaded;
            job.input_file_id = Some(input_file_id.to_owned());
            Ok(())
        })
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    }

    if let Some(batch_id) = args.batch_id.as_deref() {
        validate_remote_id(batch_id, "batch_id")
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        if !matches!(
            job.state,
            JobState::CreateInFlight | JobState::CreateOutcomeUnknown
        ) {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "batch_creation_recovery_not_applicable",
                    "--batch-id is only valid for a job whose Batch creation outcome is unknown.",
                ),
                context,
            ));
        }
        let endpoint = endpoint_from_job(&job, &context, &args.job)?;
        let key = api_key().map_err(|error| BatchFailure::new(error, context.clone()))?;
        let client = ApiClient::new(args.job.timeout_seconds)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        context.attempted = true;
        let response = client
            .get_batch(&endpoint, &key, batch_id)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        let info = parse_batch_info(
            &response.body,
            Some(batch_id),
            job.input_file_id.as_deref(),
            job.image_count,
        )
        .map_err(|error| {
            let mut failure = BatchFailure::new(error, context.clone());
            attach_response_metadata(
                &mut failure,
                response.status,
                response.request_id.as_deref(),
            );
            failure
        })?;
        validate_remote_transition(&job, &info).map_err(|error| {
            let mut failure = BatchFailure::new(error, context.clone());
            attach_response_metadata(
                &mut failure,
                response.status,
                response.request_id.as_deref(),
            );
            failure
        })?;
        job = persist_batch_observation(
            &job_file,
            &job,
            &info,
            response.status,
            response.request_id.clone(),
            &context,
        )?;
        context = context_from_job("batch.recover", &job_file, &job);
        context.attempted = true;
        context.http_status = Some(response.status);
        context.request_id = response.request_id;
        if matches!(job.remote_status.as_deref(), Some("failed" | "expired")) {
            context.next_action =
                Some(terminal_batch_next_action(job.output_file_id.as_deref()).to_owned());
            return Err(BatchFailure::new(
                terminal_batch_error(job.remote_status.as_deref().unwrap_or_default()),
                context,
            ));
        }
        context.next_action = Some(
            if job.output_file_id.is_some() {
                "run batch retrieve to publish available results"
            } else {
                "run batch status or batch retrieve with this job file"
            }
            .to_owned(),
        );
        return Ok(context.report(None));
    }

    if job.state == JobState::InputUploaded {
        return create_batch_from_job(&args.job, &job_file, job);
    }
    if matches!(
        job.state,
        JobState::UploadInFlight | JobState::UploadOutcomeUnknown
    ) {
        return Err(BatchFailure::new(
            AppError::preflight(
                "batch_input_reconciliation_required",
                "The upload outcome is unknown. Inspect the remote files API, then rerun batch recover with the confirmed --input-file-id; no upload was retried.",
            ),
            context,
        ));
    }
    if matches!(
        job.state,
        JobState::CreateInFlight | JobState::CreateOutcomeUnknown
    ) {
        return Err(BatchFailure::new(
            AppError::preflight(
                "batch_creation_reconciliation_required",
                "The Batch creation outcome is unknown. Inspect the remote Batches API, then rerun batch recover with the confirmed --batch-id; no creation was retried.",
            ),
            context,
        ));
    }
    if job.state == JobState::Prepared {
        return Err(BatchFailure::new(
            AppError::preflight(
                "batch_prompt_not_persisted",
                "This prepared job has no persisted prompt or input file. Rerun batch submit with the original prompt instead of resubmitting this job record.",
            ),
            context,
        ));
    }
    Ok(context.report(None))
}

fn create_batch_from_job(
    args: &BatchJobArgs,
    job_file: &Path,
    mut job: BatchJob,
) -> Result<BatchReport, BatchFailure> {
    let mut context = context_from_job("batch.recover", job_file, &job);
    ensure_billable_platform().map_err(|error| BatchFailure::new(error, context.clone()))?;
    let input_file_id = job.input_file_id.clone().ok_or_else(|| {
        BatchFailure::new(
            AppError::preflight(
                "batch_input_file_missing",
                "The recoverable job has no confirmed input file ID.",
            ),
            context.clone(),
        )
    })?;
    let endpoint = endpoint_from_job(&job, &context, args)?;
    let key = api_key().map_err(|error| BatchFailure::new(error, context.clone()))?;
    let client = ApiClient::new(args.timeout_seconds)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let create_in_flight = transition_if_revision(job_file, job.revision, |job| {
        job.state = JobState::CreateInFlight;
        Ok(())
    })
    .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let request = BatchCreateRequest {
        input_file_id: &input_file_id,
        endpoint: "/v1/images/generations",
        completion_window: "24h",
        output_expires_after: OutputExpiresAfter {
            anchor: "created_at",
            seconds: DEFAULT_OUTPUT_EXPIRY_SECONDS,
        },
    };
    context.attempted = true;
    let response = match client.create_batch(&endpoint, &key, &request) {
        Ok(response) => response,
        Err(error) => {
            let error = mark_unknown(
                job_file,
                create_in_flight.revision,
                JobState::CreateOutcomeUnknown,
                error,
            );
            return Err(BatchFailure::new(error, context));
        }
    };
    let info = match parse_batch_create_info(&response.body, &input_file_id, job.image_count) {
        Ok(info) => info,
        Err(error) => {
            let mut error = error;
            error.set_http_status(response.status);
            error.set_request_id(response.request_id.clone());
            let error = mark_unknown(
                job_file,
                create_in_flight.revision,
                JobState::CreateOutcomeUnknown,
                error,
            );
            return Err(BatchFailure::new(error, context));
        }
    };
    job = persist_batch_observation(
        job_file,
        &create_in_flight,
        &info,
        response.status,
        response.request_id.clone(),
        &context,
    )?;
    context = context_from_job("batch.recover", job_file, &job);
    context.attempted = true;
    context.http_status = Some(response.status);
    context.request_id = response.request_id;
    if matches!(job.remote_status.as_deref(), Some("failed" | "expired")) {
        context.next_action =
            Some(terminal_batch_next_action(job.output_file_id.as_deref()).to_owned());
        return Err(BatchFailure::new(
            terminal_batch_error(job.remote_status.as_deref().unwrap_or_default()),
            context,
        ));
    }
    context.next_action = Some("run batch status or batch retrieve with this job file".to_owned());
    Ok(context.report(None))
}

fn persist_batch_observation(
    job_file: &Path,
    job: &BatchJob,
    info: &BatchInfo,
    http_status: u16,
    request_id: Option<String>,
    context: &BatchContext,
) -> Result<BatchJob, BatchFailure> {
    transition_if_revision(job_file, job.revision, |job| {
        job.state = state_for_remote_status(&info.status);
        job.batch_id = Some(info.id.clone());
        job.remote_status = Some(info.status.clone());
        job.output_file_id = info.output_file_id.clone();
        job.error_file_id = info.error_file_id.clone();
        job.request_counts = info
            .request_counts
            .clone()
            .map(PersistedBatchRequestCounts::from);
        Ok(())
    })
    .map_err(|error| {
        let mut recovery_error = state_persistence_error("Batch reconciliation", &info.id, error);
        recovery_error.set_http_status(http_status);
        recovery_error.set_request_id(request_id);
        BatchFailure::new(recovery_error, context.clone())
    })
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
        job.image_count,
    )
    .map_err(|error| {
        let mut failure = BatchFailure::new(error, context.clone());
        attach_response_metadata(
            &mut failure,
            response.status,
            response.request_id.as_deref(),
        );
        failure
    })?;
    validate_remote_transition(&job, &info).map_err(|error| {
        let mut failure = BatchFailure::new(error, context.clone());
        attach_response_metadata(
            &mut failure,
            response.status,
            response.request_id.as_deref(),
        );
        failure
    })?;
    set_context_from_info(&mut context, &info);
    context.http_status = Some(response.status);
    context.request_id = response.request_id.clone();
    let state = state_for_remote_status(&info.status);
    let updated = transition_if_revision(&job_file, job.revision, |job| {
        job.state = state;
        job.remote_status = Some(info.status.clone());
        job.output_file_id = info.output_file_id.clone();
        job.error_file_id = info.error_file_id.clone();
        job.request_counts = info
            .request_counts
            .clone()
            .map(PersistedBatchRequestCounts::from);
        Ok(())
    })
    .map_err(|error| {
        let mut failure = BatchFailure::new(error, context.clone());
        attach_response_metadata(
            &mut failure,
            response.status,
            response.request_id.as_deref(),
        );
        failure
    })?;
    context = context_from_job("batch.status", &job_file, &updated);
    context.http_status = Some(response.status);
    context.request_id = response.request_id.clone();
    if matches!(updated.remote_status.as_deref(), Some("failed" | "expired")) {
        context.next_action =
            Some(terminal_batch_next_action(updated.output_file_id.as_deref()).to_owned());
        return Err(BatchFailure::new(
            terminal_batch_error(updated.remote_status.as_deref().unwrap_or_default()),
            context,
        ));
    }
    if !is_terminal_remote_status(&info.status) {
        context.next_action =
            Some("run batch status again or use batch retrieve --wait".to_owned());
    } else if updated.output_file_id.is_some() {
        context.next_action = Some("run batch retrieve to publish available results".to_owned());
    } else if info.status == "completed" {
        context.next_action =
            Some("inspect the Batch status, request counts, and error file".to_owned());
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
        return match recover_publishing(&job_file, &job, context.clone()) {
            Ok(report) => Ok(report),
            Err(failure)
                if matches!(
                    failure.error.code,
                    "publishing_output_missing" | "publishing_recovery_required"
                ) && job.output_file_id.is_some() =>
            {
                retrieve_publishing_output(args, &job_file, &job, context)
            }
            Err(failure) => Err(failure),
        };
    }
    if job.state == JobState::Retrieved {
        return recover_retrieved(&job, context);
    }
    if job.state == JobState::Cancelled && job.output_file_id.is_none() {
        return Ok(context.report(None));
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
            job.image_count,
        )
        .map_err(|error| {
            let mut failure = BatchFailure::new(error, context.clone());
            attach_response_metadata(
                &mut failure,
                response.status,
                response.request_id.as_deref(),
            );
            failure
        })?;
        validate_remote_transition(&job, &info).map_err(|error| {
            let mut failure = BatchFailure::new(error, context.clone());
            attach_response_metadata(
                &mut failure,
                response.status,
                response.request_id.as_deref(),
            );
            failure
        })?;
        if let Some(previous) = previous_info.as_ref() {
            validate_observation_progress(
                Some(&previous.status),
                previous.output_file_id.as_deref(),
                previous.error_file_id.as_deref(),
                previous.request_counts.as_ref(),
                &info,
            )
            .map_err(|error| {
                let mut failure = BatchFailure::new(error, context.clone());
                attach_response_metadata(
                    &mut failure,
                    response.status,
                    response.request_id.as_deref(),
                );
                failure
            })?;
        }
        previous_info = Some(info.clone());
        context.http_status = Some(response.status);
        context.request_id = response.request_id.clone();
        set_context_from_info(&mut context, &info);
        if is_terminal_remote_status(&info.status) || !args.wait {
            break (info, response.status, response.request_id.clone());
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
    let (info, last_http_status, last_request_id) = info;
    let updated = transition_if_revision(&job_file, job.revision, |job| {
        job.state = state_for_remote_status(&info.status);
        job.remote_status = Some(info.status.clone());
        job.output_file_id = info.output_file_id.clone();
        job.error_file_id = info.error_file_id.clone();
        job.request_counts = info
            .request_counts
            .clone()
            .map(PersistedBatchRequestCounts::from);
        Ok(())
    })
    .map_err(|error| {
        let mut failure = BatchFailure::new(error, context.clone());
        attach_response_metadata(&mut failure, last_http_status, last_request_id.as_deref());
        failure
    })?;
    context = context_from_job("batch.retrieve", &job_file, &updated);
    context.attempted = true;
    context.http_status = Some(last_http_status);
    context.request_id = last_request_id;
    if !is_terminal_remote_status(updated.remote_status.as_deref().unwrap_or_default()) {
        let error = AppError::not_ready(
            "batch_not_ready",
            "The batch is still processing. Run batch retrieve again or add --wait with a bounded timeout.",
        );
        context.next_action = Some("run batch retrieve --wait".to_owned());
        return Err(BatchFailure::new(error, context));
    }
    if matches!(updated.remote_status.as_deref(), Some("failed" | "expired"))
        && updated.output_file_id.is_none()
    {
        let error = terminal_batch_error(updated.remote_status.as_deref().unwrap_or_default());
        context.next_action = Some(terminal_batch_next_action(None).to_owned());
        return Err(BatchFailure::new(error, context));
    }
    if updated.remote_status.as_deref() == Some("cancelled") && updated.output_file_id.is_none() {
        context.next_action = None;
        return Ok(context.report(None));
    }
    let Some(output_file_id) = updated.output_file_id.as_deref() else {
        let error = AppError::batch_failed(
            "batch_output_missing",
            "The completed Batch has no output file. Inspect the Batch status, request counts, and any error file before deciding what to do next.",
        );
        context.next_action =
            Some("inspect the Batch status, request counts, and error file".to_owned());
        return Err(BatchFailure::new(error, context));
    };
    let content = match client.get_file_content(&endpoint, &key, output_file_id) {
        Ok(content) => content,
        Err(error) => return Err(mark_output_unavailable(&job_file, &updated, context, error)),
    };
    publish_batch_content(
        &job_file,
        &updated,
        &content.body,
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
                Some(terminal_batch_next_action(job.output_file_id.as_deref()).to_owned());
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
        job.image_count,
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
        job.request_counts = info
            .request_counts
            .clone()
            .map(PersistedBatchRequestCounts::from);
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
        context.next_action =
            Some(terminal_batch_next_action(updated.output_file_id.as_deref()).to_owned());
        return Err(BatchFailure::new(
            terminal_batch_error(updated.remote_status.as_deref().unwrap_or_default()),
            context,
        ));
    }
    context.next_action =
        Some("run batch status to observe cancellation and any partial output".to_owned());
    Ok(context.report(None))
}

fn publish_batch_content(
    job_file: &Path,
    job: &BatchJob,
    content: &[u8],
    mut context: BatchContext,
    http_status: u16,
    request_id: Option<String>,
) -> Result<BatchReport, BatchFailure> {
    context.http_status = Some(http_status);
    context.request_id = request_id.clone();
    let output_dir = PathBuf::from(&job.output_dir);
    let paths = derive_output_paths(&output_dir, &job.output_names);
    let images = match parse_batch_output(content, job) {
        Ok(images) => images,
        Err(mut failure) => {
            let next_action = failure.context.next_action.clone();
            let mut parse_context = context.clone();
            parse_context.next_action = next_action;
            failure.context = Box::new(parse_context);
            attach_response_metadata(&mut failure, http_status, request_id.as_deref());
            return Err(failure);
        }
    };
    let digests = images.iter().map(|image| sha256(image)).collect::<Vec<_>>();
    let previous_plan = job.publishing.as_ref();
    let mut selected_indices = Vec::new();
    for (index, digest) in digests.iter().enumerate() {
        let already_published = previous_plan
            .and_then(|plan| plan.artifacts.get(index))
            .is_some_and(|artifact| {
                artifact.sha256 == *digest
                    && read_regular_file_with_identity(
                        Path::new(&artifact.path),
                        crate::image::MAX_IMAGE_BYTES,
                    )
                    .is_ok_and(|(bytes, identity)| {
                        identity == artifact.staged_identity && sha256(&bytes) == *digest
                    })
            });
        if !already_published {
            selected_indices.push(index);
        }
    }

    if selected_indices.is_empty() {
        let plan = previous_plan.cloned().ok_or_else(|| {
            BatchFailure::new(
                AppError::output_commit(
                    "publishing_plan_missing",
                    "The output was already present but no persisted publication plan was available.",
                ),
                context.clone(),
            )
        })?;
        let outputs = verify_publishing_plan(&output_dir, &plan, &mut context)?;
        let updated = transition_if_revision(job_file, job.revision, |job| {
            job.state = JobState::Retrieved;
            job.retained_artifacts = plan.retained_artifacts.clone();
            Ok(())
        })
        .map_err(|error| {
            let recovery_error = state_persistence_error("output publication", &job.job_id, error);
            BatchFailure::new(recovery_error, context.clone())
        })?;
        context.outputs = outputs;
        context.retained_artifacts = updated.retained_artifacts;
        context.http_status = Some(http_status);
        context.request_id = request_id;
        context.remote_status = Some("retrieved".to_owned());
        return Ok(context.report(None));
    }

    let selected_names = selected_indices
        .iter()
        .map(|index| job.output_names[*index].clone())
        .collect::<Vec<_>>();
    let mut selected_expected_ids = Vec::new();
    if let Some(plan) = previous_plan {
        for index in &selected_indices {
            selected_expected_ids.push(plan.artifacts[*index].expected_target);
        }
    }
    let mut transaction = if previous_plan.is_some() {
        OutputTransaction::reserve_with_expected_targets(
            &output_dir,
            selected_names,
            selected_expected_ids,
        )
    } else {
        OutputTransaction::reserve(&output_dir, selected_names, job.overwrite)
    }
    .map_err(|error| {
        let mut failure = BatchFailure::new(error, context.clone());
        attach_response_metadata(&mut failure, http_status, request_id.as_deref());
        failure
    })?;
    let selected_images = selected_indices
        .iter()
        .map(|index| images[*index].clone())
        .collect::<Vec<_>>();
    if let Err(mut error) = transaction.stage_all(&selected_images) {
        error.add_possibly_modified_paths(transaction.abort());
        error.set_http_status(http_status);
        error.set_request_id(request_id.clone());
        return Err(BatchFailure::new(error, context));
    }
    let new_staged_paths = transaction
        .staged_artifact_paths()
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let new_expected_ids = transaction.expected_target_ids();
    let new_staged_ids = transaction.staged_artifact_ids();
    let mut selected_positions = vec![None; job.output_names.len()];
    for (position, index) in selected_indices.iter().enumerate() {
        selected_positions[*index] = Some(position);
    }
    let mut artifacts = Vec::with_capacity(job.output_names.len());
    for (index, path) in paths.iter().enumerate() {
        if let Some(position) = selected_positions[index] {
            artifacts.push(PublishingArtifact {
                path: path.to_string_lossy().into_owned(),
                sha256: digests[index].clone(),
                staged_path: new_staged_paths[position].clone(),
                staged_identity: new_staged_ids[position],
                expected_target: new_expected_ids[position],
            });
        } else if let Some(plan) = previous_plan {
            artifacts.push(plan.artifacts[index].clone());
        }
    }
    let staged_artifacts = artifacts
        .iter()
        .map(|artifact| artifact.staged_path.clone())
        .collect::<Vec<_>>();
    let mut retained_artifacts = Vec::new();
    let mut retained_artifact_ids = Vec::new();
    if let Some(previous_plan) = previous_plan {
        for (path, identity) in previous_plan
            .retained_artifacts
            .iter()
            .zip(&previous_plan.retained_artifact_ids)
        {
            let selected = selected_indices
                .iter()
                .any(|index| previous_plan.artifacts[*index].staged_path == *path);
            if !selected {
                retain_artifact(
                    &mut retained_artifacts,
                    &mut retained_artifact_ids,
                    path.clone(),
                    *identity,
                );
            }
        }
        for index in &selected_indices {
            let previous_artifact = &previous_plan.artifacts[*index];
            if let Some(expected_id) = previous_artifact.expected_target {
                if verify_regular_file_identity(
                    Path::new(&previous_artifact.staged_path),
                    expected_id,
                )
                .is_ok()
                {
                    retain_artifact(
                        &mut retained_artifacts,
                        &mut retained_artifact_ids,
                        previous_artifact.staged_path.clone(),
                        expected_id,
                    );
                }
            }
        }
    }
    for (position, index) in selected_indices.iter().enumerate() {
        if let Some(expected_id) = new_expected_ids[position] {
            retain_artifact(
                &mut retained_artifacts,
                &mut retained_artifact_ids,
                new_staged_paths[position].clone(),
                expected_id,
            );
        }
        debug_assert_eq!(artifacts[*index].staged_path, new_staged_paths[position]);
    }
    let plan = PublishingPlan {
        artifacts,
        staged_artifacts,
        retained_artifacts: retained_artifacts.clone(),
        retained_artifact_ids,
    };
    let publishing_job = match transition_if_revision(job_file, job.revision, |job| {
        job.state = JobState::Publishing;
        job.publishing = Some(plan.clone());
        job.retained_artifacts = retained_artifacts.clone();
        Ok(())
    }) {
        Ok(job) => job,
        Err(error) => {
            let mut recovery_error = state_persistence_error("output staging", &job.job_id, error);
            recovery_error.add_possibly_modified_paths(transaction.abort());
            recovery_error.set_http_status(http_status);
            recovery_error.set_request_id(request_id.clone());
            return Err(BatchFailure::new(recovery_error, context));
        }
    };
    if let Err(mut error) = transaction.commit_all() {
        error.add_possibly_modified_paths(transaction.abort());
        return Err(BatchFailure::new(error, context));
    }
    context.http_status = Some(http_status);
    context.request_id = request_id.clone();
    let outputs = verify_publishing_plan(&output_dir, &plan, &mut context)?;
    let updated = transition_if_revision(job_file, publishing_job.revision, |job| {
        job.state = JobState::Retrieved;
        // Keep the digest plan so repeated retrieval can verify and report the
        // already-published artifacts without contacting the API again.
        job.retained_artifacts = retained_artifacts.clone();
        Ok(())
    });
    if let Err(error) = updated {
        context.outputs = outputs;
        context.retained_artifacts = retained_artifacts.clone();
        context.next_action =
            Some("rerun batch retrieve to reconcile the publishing state".to_owned());
        let mut recovery_error = state_persistence_error("output publication", &job.job_id, error);
        recovery_error.set_http_status(http_status);
        recovery_error.set_request_id(request_id);
        return Err(BatchFailure::new(recovery_error, context));
    }
    context.outputs = outputs;
    context.retained_artifacts = retained_artifacts;
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
    let output_dir = PathBuf::from(&job.output_dir);
    let mut recovery_checks = Vec::with_capacity(plan.artifacts.len());
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        let stage_path = Path::new(&artifact.staged_path);
        let Some(stage_name) = stage_path.file_name().and_then(|name| name.to_str()) else {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "publishing_recovery_invalid",
                    "The persisted publication stage path is invalid.",
                ),
                context,
            ));
        };
        if stage_path.parent() != Some(output_dir.as_path()) {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "publishing_recovery_invalid",
                    "The persisted publication stage path does not belong to the output directory.",
                ),
                context,
            ));
        }
        recovery_checks.push((
            index,
            RecoveryVerificationArtifact {
                output_name: job.output_names[index].clone(),
                expected_output_id: artifact.staged_identity,
                expected_sha256: artifact.sha256.clone(),
                stage_name: stage_name.to_owned(),
                expected_stage_id: artifact.staged_identity,
            },
        ));
    }
    let observations = inspect_recovery_plan(
        &output_dir,
        &recovery_checks
            .iter()
            .map(|(_, check)| check.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(|error| BatchFailure::new(error, context.clone()))?;
    let mut recovery_artifacts = Vec::new();
    for ((index, check), observation) in recovery_checks.iter().zip(observations) {
        if observation.final_matches {
            continue;
        }
        if !observation.stage_matches {
            let artifact = &plan.artifacts[*index];
            context.possibly_modified_paths.push(artifact.path.clone());
            context
                .possibly_modified_paths
                .push(artifact.staged_path.clone());
            let error = AppError::preflight(
                "publishing_output_missing",
                "A publication stage is missing or does not match its persisted digest; the remote Batch output will be used to repair it without replacing changed targets.",
            );
            return Err(BatchFailure::new(error, context));
        }
        let artifact = &plan.artifacts[*index];
        recovery_artifacts.push(RecoveryArtifact {
            final_name: check.output_name.clone(),
            stage_name: check.stage_name.clone(),
            expected_stage_id: check.expected_stage_id,
            expected_id: artifact.expected_target,
        });
    }
    if !recovery_artifacts.is_empty() {
        let mut transaction = OutputTransaction::recover(&output_dir, recovery_artifacts)
            .map_err(|error| BatchFailure::new(error, context.clone()))?;
        if let Err(mut error) = transaction.commit_all() {
            error.add_possibly_modified_paths(transaction.abort());
            return Err(BatchFailure::new(error, context));
        }
    }
    let outputs = verify_publishing_plan(&output_dir, plan, &mut context)?;
    transition_if_revision(job_file, job.revision, |job| {
        job.state = JobState::Retrieved;
        job.retained_artifacts = plan.retained_artifacts.clone();
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
    let outputs = verify_publishing_plan(Path::new(&job.output_dir), plan, &mut context)?;
    context.outputs = outputs;
    context.remote_status = Some("retrieved".to_owned());
    context.next_action = None;
    Ok(context.report(None))
}

fn retrieve_publishing_output(
    args: &BatchRetrieveArgs,
    job_file: &Path,
    job: &BatchJob,
    mut context: BatchContext,
) -> Result<BatchReport, BatchFailure> {
    let output_file_id = job.output_file_id.as_deref().ok_or_else(|| {
        BatchFailure::new(
            AppError::preflight(
                "batch_output_missing",
                "The Publishing job has no confirmed output file ID to recover.",
            ),
            context.clone(),
        )
    })?;
    let endpoint = endpoint_from_job(job, &context, &args.job)?;
    let key = api_key().map_err(|error| BatchFailure::new(error, context.clone()))?;
    let client = ApiClient::new(args.job.timeout_seconds)
        .map_err(|error| BatchFailure::new(error, context.clone()))?;
    context.attempted = true;
    let content = match client.get_file_content(&endpoint, &key, output_file_id) {
        Ok(content) => content,
        Err(error) => return Err(mark_output_unavailable(job_file, job, context, error)),
    };
    publish_batch_content(
        job_file,
        job,
        &content.body,
        context,
        content.status,
        content.request_id,
    )
}

fn mark_output_unavailable(
    job_file: &Path,
    job: &BatchJob,
    mut context: BatchContext,
    mut error: AppError,
) -> BatchFailure {
    if matches!(
        error.code,
        "batch_output_unavailable" | "batch_output_expired"
    ) {
        if job.state == JobState::Publishing {
            context.next_action = Some(
                "inspect the local publication journal and remote Batch output before retrying"
                    .to_owned(),
            );
        } else if let Err(state_error) = transition_if_revision(job_file, job.revision, |job| {
            job.state = JobState::Failed;
            job.publishing = None;
            Ok(())
        }) {
            error.message = format!(
                "{} The output-unavailable state could not be persisted: {}",
                error.message, state_error.message
            );
            error.automatic_retry_safe = false;
        } else {
            context.next_action =
                Some("inspect the remote Batch record and error/output files".to_owned());
        }
    }
    context.http_status = error.http_status.or(context.http_status);
    context.request_id = error.request_id.clone().or(context.request_id);
    BatchFailure::new(error, context)
}

fn verify_publishing_plan(
    output_dir: &Path,
    plan: &PublishingPlan,
    context: &mut BatchContext,
) -> Result<Vec<String>, BatchFailure> {
    let mut verification = Vec::with_capacity(plan.artifacts.len());
    let mut retained_verification: Vec<RetainedVerificationArtifact> =
        Vec::with_capacity(plan.retained_artifacts.len());
    let mut possibly_modified_paths =
        Vec::with_capacity(plan.artifacts.len() + plan.retained_artifacts.len());
    if plan.retained_artifacts.len() != plan.retained_artifact_ids.len() {
        return Err(BatchFailure::new(
            AppError::preflight(
                "publishing_journal_invalid",
                "The publication journal retained-artifact identity count is inconsistent.",
            ),
            context.clone(),
        ));
    }
    for artifact in &plan.artifacts {
        let output_path = Path::new(&artifact.path);
        let Some(output_name) = output_path.file_name().and_then(|name| name.to_str()) else {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "publishing_journal_invalid",
                    "The publication plan contains an invalid output path.",
                ),
                context.clone(),
            ));
        };
        if output_path.parent() != Some(output_dir) {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "publishing_journal_invalid",
                    "The publication plan output is outside the persisted output directory.",
                ),
                context.clone(),
            ));
        }
        if let Some(expected_id) = artifact.expected_target {
            let retained_id = plan
                .retained_artifacts
                .iter()
                .zip(&plan.retained_artifact_ids)
                .find_map(|(path, identity)| (path == &artifact.staged_path).then_some(*identity));
            if retained_id != Some(expected_id) {
                return Err(BatchFailure::new(
                    AppError::preflight(
                        "publishing_journal_invalid",
                        "The publication journal does not retain the identity-checked overwrite backup.",
                    ),
                    context.clone(),
                ));
            }
        }
        possibly_modified_paths.push(PathBuf::from(&artifact.path));
        verification.push(OutputVerificationArtifact {
            output_name: output_name.to_owned(),
            expected_output_id: artifact.staged_identity,
            expected_sha256: artifact.sha256.clone(),
        });
    }
    for (path, expected_id) in plan
        .retained_artifacts
        .iter()
        .zip(&plan.retained_artifact_ids)
    {
        let retained_path = Path::new(path);
        let Some(retained_name) = retained_path.file_name().and_then(|name| name.to_str()) else {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "publishing_journal_invalid",
                    "The publication plan contains an invalid retained path.",
                ),
                context.clone(),
            ));
        };
        if retained_path.parent() != Some(output_dir)
            || !retained_name.starts_with(".codex-image-stage-")
        {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "publishing_journal_invalid",
                    "The publication plan retained artifact path is unsafe.",
                ),
                context.clone(),
            ));
        }
        if retained_verification
            .iter()
            .any(|artifact| artifact.name == retained_name)
        {
            return Err(BatchFailure::new(
                AppError::preflight(
                    "publishing_journal_invalid",
                    "The publication plan contains duplicate retained artifacts.",
                ),
                context.clone(),
            ));
        }
        possibly_modified_paths.push(PathBuf::from(path));
        retained_verification.push(RetainedVerificationArtifact {
            name: retained_name.to_owned(),
            expected_id: *expected_id,
        });
    }
    let outputs = verify_and_sync_plan(output_dir, &verification, &retained_verification).map_err(
        |error| {
            let mut error = error;
            error.add_possibly_modified_paths(possibly_modified_paths);
            BatchFailure::new(error, context.clone())
        },
    )?;
    Ok(outputs
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect())
}

fn parse_batch_output(content: &[u8], job: &BatchJob) -> Result<Vec<Vec<u8>>, BatchFailure> {
    let mut seen = vec![false; job.custom_ids.len()];
    let mut images: Vec<Option<Vec<u8>>> = (0..job.custom_ids.len()).map(|_| None).collect();
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
        let Some(index) = job
            .custom_ids
            .iter()
            .position(|custom_id| custom_id == &result.custom_id)
        else {
            return Err(batch_result_failure(
                "batch_result_ids_invalid",
                "The Batch output contained an unknown or duplicate custom_id.",
                job,
            ));
        };
        if seen[index] {
            return Err(batch_result_failure(
                "batch_result_ids_invalid",
                "The Batch output contained an unknown or duplicate custom_id.",
                job,
            ));
        }
        seen[index] = true;
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
        if response.body.data.len() != 1 {
            return Err(batch_result_failure(
                "batch_result_body_invalid",
                "A Batch response body did not contain exactly one image record.",
                job,
            ));
        }
        let encoded = response
            .body
            .data
            .into_iter()
            .next()
            .and_then(|image| image.b64_json);
        let decoded = decode_base64_image(encoded, job.format).map_err(|_| {
            batch_result_failure(
                "batch_image_invalid",
                "A Batch image response was malformed or did not match the requested format. No partial output was published.",
                job,
            )
        })?;
        images[index] = Some(decoded);
    }
    if seen.iter().any(|seen| !seen) {
        return Err(batch_result_failure(
            "batch_result_incomplete",
            "The Batch output did not contain exactly one successful result for every requested image.",
            job,
        ));
    }
    images
        .into_iter()
        .map(|image| {
            image.ok_or_else(|| {
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
    error: Option<serde::de::IgnoredAny>,
}

#[derive(Debug, Deserialize)]
struct BatchItemResponse {
    status_code: u16,
    body: BatchImageBody,
}

#[derive(Debug, Deserialize)]
struct BatchImageBody {
    data: Vec<BatchImageData>,
}

#[derive(Debug, Deserialize)]
struct BatchImageData {
    b64_json: Option<String>,
}

fn batch_result_failure(code: &'static str, message: &'static str, job: &BatchJob) -> BatchFailure {
    let mut context = context_from_job("batch.retrieve", Path::new(""), job);
    context.attempted = true;
    context.next_action =
        Some("inspect the Batch output/error files before retrying retrieval".to_owned());
    BatchFailure::new(AppError::invalid_response(code, message), context)
}

fn build_batch_input(
    assets: &[BatchAssetInput],
    args: &GenerateArgs,
    custom_ids: &[String],
) -> Result<Vec<u8>, AppError> {
    let mut input = Vec::new();
    for (asset, custom_id) in assets.iter().zip(custom_ids) {
        let request = serde_json::json!({
            "custom_id": custom_id,
            "method": "POST",
            "url": "/v1/images/generations",
            "body": ImageGenerationRequest::from_args_with_count(&asset.prompt, args, 1),
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
    crate::api::validate_api_key(&value)?;
    Ok(value)
}

pub fn ensure_billable_platform() -> Result<(), AppError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(AppError::preflight(
            "secure_output_transactions_unsupported",
            "Billable Batch submission is supported only on macOS and Linux. Use --dry-run or a read-only reconciliation command on this platform; no request was sent.",
        ))
    }
}

fn parse_batch_info(
    body: &[u8],
    expected_batch_id: Option<&str>,
    expected_input_file_id: Option<&str>,
    expected_image_count: u8,
) -> Result<BatchInfo, AppError> {
    let info = serde_json::from_slice(body).map_err(|_| {
        AppError::observation(
            "batch_status_invalid",
            "The Batch endpoint returned an invalid status object; retrying the read-only operation is safe.",
        )
    })?;
    validate_batch_info(
        &info,
        expected_batch_id,
        expected_input_file_id,
        expected_image_count,
    )?;
    Ok(info)
}

fn parse_batch_create_info(
    body: &[u8],
    expected_input_file_id: &str,
    expected_image_count: u8,
) -> Result<BatchInfo, AppError> {
    let info = serde_json::from_slice(body).map_err(|_| {
        AppError::invalid_response(
            "batch_create_invalid",
            "The batch-create response did not contain a valid Batch object; do not retry automatically. Inspect the persisted job.",
        )
    })?;
    validate_batch_info(
        &info,
        None,
        Some(expected_input_file_id),
        expected_image_count,
    )
    .map_err(|_| {
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
    expected_image_count: u8,
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
    if info
        .endpoint
        .as_deref()
        .is_some_and(|endpoint| endpoint != "/v1/images/generations")
    {
        return Err(AppError::observation(
            "batch_endpoint_mismatch",
            "The Batch endpoint returned a different generation endpoint; retrying the read-only operation is safe.",
        ));
    }
    if info
        .completion_window
        .as_deref()
        .is_some_and(|window| window != "24h")
    {
        return Err(AppError::observation(
            "batch_window_mismatch",
            "The Batch endpoint returned a different completion window; retrying the read-only operation is safe.",
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
        if !request_counts_are_valid(counts, expected_image_count, &info.status) {
            return Err(AppError::observation(
                "batch_counts_invalid",
                "The Batch endpoint returned inconsistent request counts; retrying the read-only operation is safe.",
            ));
        }
    }
    Ok(())
}

fn request_counts_are_valid(
    counts: &BatchRequestCounts,
    expected_image_count: u8,
    status: &str,
) -> bool {
    // OpenAI may return zero counts while it is still validating the input.
    let zero_before_processing =
        counts.total == 0 && counts.completed == 0 && counts.failed == 0 && status == "validating";
    (zero_before_processing || counts.total == u32::from(expected_image_count))
        && counts.completed <= counts.total
        && counts.failed <= counts.total
        && counts.completed.saturating_add(counts.failed) <= counts.total
}

fn validate_remote_transition(job: &BatchJob, info: &BatchInfo) -> Result<(), AppError> {
    let previous_counts = job.request_counts.as_ref().map(BatchRequestCounts::from);
    validate_observation_progress(
        job.remote_status.as_deref(),
        job.output_file_id.as_deref(),
        job.error_file_id.as_deref(),
        previous_counts.as_ref(),
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

fn terminal_batch_next_action(output_file_id: Option<&str>) -> &'static str {
    if output_file_id.is_some() {
        "run batch retrieve to publish any available results"
    } else {
        "inspect the Batch error file and run batch status"
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
    context.request_counts = info.request_counts.as_ref().map(request_counts_report);
}

fn request_counts_report(counts: &BatchRequestCounts) -> BatchRequestCountsReport {
    BatchRequestCountsReport {
        completed: counts.completed,
        failed: counts.failed,
        total: counts.total,
    }
}

fn persisted_request_counts_report(
    counts: &PersistedBatchRequestCounts,
) -> BatchRequestCountsReport {
    BatchRequestCountsReport {
        completed: counts.completed,
        failed: counts.failed,
        total: counts.total,
    }
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

fn context_for_input_job(
    base: &BatchContext,
    job_id: &str,
    input_file_id: &str,
    batch_id: Option<&str>,
    job_file: Option<&Path>,
) -> BatchContext {
    let mut context = context_for_job(base, job_id, batch_id, job_file);
    context.input_file_id = Some(input_file_id.to_owned());
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
        request_counts: job
            .request_counts
            .as_ref()
            .map(persisted_request_counts_report),
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

fn normalize_job_path(path: &Path) -> Result<PathBuf, AppError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => {
                return Err(AppError::preflight(
                    "job_path_invalid",
                    "The Batch job path must not contain '..'.",
                ));
            }
        }
    }
    Ok(normalized)
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

struct JobStore;

fn validate_job_link(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    if fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.is_file() && metadata.nlink() != 1)
    {
        return Err(AppError::preflight(
            "job_file_hard_linked",
            "The Batch job file must have exactly one filesystem link; use the original path rather than an alias.",
        ));
    }
    Ok(())
}

fn validate_job_path(path: &Path, job: &BatchJob) -> Result<(), AppError> {
    let expected = path.to_str().ok_or_else(|| {
        AppError::preflight(
            "job_path_invalid",
            "The Batch job path must be valid UTF-8.",
        )
    })?;
    if job.state_path != expected {
        return Err(AppError::preflight(
            "job_file_alias",
            "The Batch job belongs to a different job-file path; use the original path.",
        ));
    }
    validate_job_link(path)
}

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
    if job
        .compression
        .is_some_and(|compression| compression > 100 || job.format == OutputFormat::Png)
    {
        return Err(invalid_job(
            "The Batch job compression is invalid for its output format.",
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
    if job.input_bytes == 0
        || job.input_bytes > MAX_BATCH_INPUT_BYTES as u64
        || job.input_sha256.len() != 64
        || !job
            .input_sha256
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(invalid_job(
            "The Batch job input integrity metadata is invalid.",
        ));
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
        let counts = BatchRequestCounts::from(counts);
        if !request_counts_are_valid(
            &counts,
            job.image_count,
            job.remote_status.as_deref().unwrap_or_default(),
        ) {
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
        let expected_staged = plan
            .artifacts
            .iter()
            .map(|artifact| artifact.staged_path.clone())
            .collect::<Vec<_>>();
        if expected_staged.iter().collect::<HashSet<_>>().len() != expected_staged.len()
            || plan.retained_artifacts.len() != plan.retained_artifact_ids.len()
            || plan.retained_artifacts.iter().collect::<HashSet<_>>().len()
                != plan.retained_artifacts.len()
            || plan.staged_artifacts != expected_staged
        {
            return Err(invalid_job(
                "The Batch job publication journal does not match its per-output plan.",
            ));
        }
        let mut journal_names = HashSet::new();
        for (artifact, name) in plan.artifacts.iter().zip(&job.output_names) {
            let expected = Path::new(&job.output_dir).join(name);
            let staged = Path::new(&artifact.staged_path);
            let Some(staged_name) = staged.file_name().and_then(|name| name.to_str()) else {
                return Err(invalid_job(
                    "The Batch job publication stage filename is invalid.",
                ));
            };
            if !journal_names.insert(name.clone()) || !journal_names.insert(staged_name.to_owned())
            {
                return Err(invalid_job(
                    "The Batch job publication journal contains colliding artifact names.",
                ));
            }
            if artifact.path != expected.to_string_lossy()
                || artifact.sha256.len() != 64
                || !artifact
                    .sha256
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
                || !staged.is_absolute()
                || staged.parent() != Some(Path::new(&job.output_dir))
                || staged
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_none_or(|name| !name.starts_with(".codex-image-stage-"))
            {
                return Err(invalid_job(
                    "The Batch job publication digest plan is invalid.",
                ));
            }
            if let Some(expected_id) = artifact.expected_target {
                let retained = plan
                    .retained_artifacts
                    .iter()
                    .zip(&plan.retained_artifact_ids)
                    .any(|(path, identity)| {
                        path == &artifact.staged_path && *identity == expected_id
                    });
                if !retained {
                    return Err(invalid_job(
                        "The Batch job overwrite backup identity is not retained.",
                    ));
                }
            }
        }
        if job.retained_artifacts != plan.retained_artifacts {
            return Err(invalid_job(
                "The Batch job retained-artifact journal does not match its publication plan.",
            ));
        }
        let staged_names = expected_staged
            .iter()
            .filter_map(|path| Path::new(path).file_name()?.to_str())
            .collect::<HashSet<_>>();
        let mut retained_names = HashSet::new();
        for path in &plan.retained_artifacts {
            let path = Path::new(path);
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return Err(invalid_job(
                    "The Batch job retained-artifact filename is invalid.",
                ));
            };
            if path.parent() != Some(Path::new(&job.output_dir))
                || !name.starts_with(".codex-image-stage-")
                || !retained_names.insert(name)
                || (journal_names.contains(name) && !staged_names.contains(name))
            {
                return Err(invalid_job(
                    "The Batch job retained-artifact journal contains an unsafe or colliding name.",
                ));
            }
        }
        for path in plan
            .staged_artifacts
            .iter()
            .chain(plan.retained_artifacts.iter())
        {
            let path = Path::new(path);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
                || !path.starts_with(&job.output_dir)
            {
                return Err(invalid_job(
                    "The Batch job publication journal contains an unsafe artifact path.",
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
    validate_state_invariants(job)?;
    Ok(())
}

fn invalid_job(message: &'static str) -> AppError {
    AppError::preflight("job_file_invalid", message)
}

fn validate_state_invariants(job: &BatchJob) -> Result<(), AppError> {
    let has_input = job.input_file_id.is_some();
    let has_batch = job.batch_id.is_some();
    let status = job.remote_status.as_deref();
    let valid = match job.state {
        JobState::Prepared | JobState::UploadInFlight | JobState::UploadOutcomeUnknown => {
            !has_input
                && !has_batch
                && status.is_none()
                && job.output_file_id.is_none()
                && job.error_file_id.is_none()
                && job.request_counts.is_none()
                && job.publishing.is_none()
        }
        JobState::InputUploaded => {
            has_input
                && !has_batch
                && status.is_none()
                && job.output_file_id.is_none()
                && job.error_file_id.is_none()
                && job.request_counts.is_none()
                && job.publishing.is_none()
        }
        JobState::CreateInFlight | JobState::CreateOutcomeUnknown => {
            has_input
                && !has_batch
                && status.is_none()
                && job.output_file_id.is_none()
                && job.error_file_id.is_none()
                && job.request_counts.is_none()
                && job.publishing.is_none()
        }
        JobState::Submitted => {
            has_input && has_batch && status.is_some() && job.publishing.is_none()
        }
        JobState::Completed => {
            has_input && has_batch && status == Some("completed") && job.publishing.is_none()
        }
        JobState::Publishing | JobState::Retrieved => {
            has_input
                && has_batch
                && matches!(
                    status,
                    Some("completed" | "failed" | "expired" | "cancelled")
                )
                && job.publishing.is_some()
        }
        JobState::Failed => {
            has_input
                && has_batch
                && matches!(
                    status,
                    Some("completed" | "failed" | "expired" | "cancelled")
                )
                && job.publishing.is_none()
        }
        JobState::CancelInFlight | JobState::CancelOutcomeUnknown => {
            has_input
                && has_batch
                && matches!(
                    status,
                    Some("validating" | "in_progress" | "finalizing" | "cancelling")
                )
                && job.publishing.is_none()
        }
        JobState::Cancelled => {
            has_input && has_batch && status == Some("cancelled") && job.publishing.is_none()
        }
    };
    if !valid {
        return Err(invalid_job(
            "The Batch job state is inconsistent with its persisted remote IDs and status.",
        ));
    }
    if !matches!(job.state, JobState::Publishing | JobState::Retrieved)
        && !job.retained_artifacts.is_empty()
    {
        return Err(invalid_job(
            "Retained artifacts are only valid after a completed local publication.",
        ));
    }
    Ok(())
}

impl JobStore {
    fn resolve(path: Option<&Path>, job_id: &str) -> Result<PathBuf, AppError> {
        let path = match path {
            Some(path) => absolute_path(path)?,
            None => default_job_directory()?.join(format!("{job_id}.json")),
        };
        let path = absolute_path(&path)?;
        let path = normalize_job_path(&path)?;
        if path.to_str().is_none() {
            return Err(AppError::preflight(
                "job_path_invalid",
                "The Batch job path must be valid UTF-8.",
            ));
        }
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
        validate_job_path(path, job)?;
        write_atomic(path, job, true)
    }

    fn load(path: &Path) -> Result<BatchJob, AppError> {
        let _lock = Self::lock(path)?;
        Self::load_unlocked(path)
    }

    fn load_unlocked(path: &Path) -> Result<BatchJob, AppError> {
        validate_job_link(path)?;
        let bytes = read_regular_file(path, MAX_JOB_FILE_BYTES).map_err(|error| {
            AppError::preflight(
                if error.code == "publishing_output_too_large" {
                    "job_file_too_large"
                } else {
                    "job_file_unreadable"
                },
                "The Batch job record could not be read safely.",
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
        validate_job_path(path, &job)?;
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
        let mut job = Self::load_unlocked(path)?;
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
        write_atomic(path, &job, false)?;
        Ok(job)
    }

    fn lock(path: &Path) -> Result<File, AppError> {
        ensure_parent(path)?;
        let parent = path.parent().ok_or_else(|| {
            AppError::preflight(
                "job_lock_unavailable",
                "The Batch job directory could not be resolved for locking.",
            )
        })?;
        validate_no_symlink_components(parent)?;
        let lock = File::open(parent).map_err(|_| {
            AppError::preflight(
                "job_lock_unavailable",
                "Another Batch operation is updating this job; retry after it exits.",
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

fn write_atomic(path: &Path, value: &BatchJob, no_clobber: bool) -> Result<(), AppError> {
    validate_job_path(path, value)?;
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
    temporary.as_file().sync_all().map_err(|_| {
        AppError::preflight(
            "job_write_failed",
            "The Batch job record could not be synchronized safely.",
        )
    })?;
    let persist = if no_clobber {
        temporary.persist_noclobber(path)
    } else {
        temporary.persist(path)
    };
    persist.map_err(|_| {
        AppError::preflight(
            "job_write_failed",
            "The Batch job record could not be committed atomically.",
        )
    })?;
    let directory = File::open(parent).map_err(|_| {
        AppError::preflight(
            "job_directory_sync_failed",
            "The Batch job directory could not be opened for durability verification.",
        )
    })?;
    directory.sync_all().map_err(|_| {
        AppError::preflight(
            "job_directory_sync_failed",
            "The Batch job directory could not be synchronized after the atomic update.",
        )
    })?;
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
    fn accepts_zero_counts_while_batch_is_validating() {
        let info = BatchInfo {
            id: "batch-test".to_owned(),
            status: "validating".to_owned(),
            input_file_id: "file-input".to_owned(),
            endpoint: Some("/v1/images/generations".to_owned()),
            completion_window: Some("24h".to_owned()),
            output_file_id: None,
            error_file_id: None,
            request_counts: Some(BatchRequestCounts {
                completed: 0,
                failed: 0,
                total: 0,
            }),
        };
        assert!(validate_batch_info(&info, None, Some("file-input"), 1).is_ok());

        let mut processing = info.clone();
        processing.status = "in_progress".to_owned();
        assert!(validate_batch_info(&processing, None, Some("file-input"), 1).is_err());
        for status in ["cancelling", "cancelled", "failed", "expired"] {
            processing.status = status.to_owned();
            assert!(
                validate_batch_info(&processing, None, Some("file-input"), 1).is_err(),
                "zero counts must not bypass validation for {status}"
            );
        }
    }

    #[test]
    fn job_state_round_trips_without_prompt_data() {
        let job = BatchJob {
            schema_version: JOB_SCHEMA_VERSION,
            revision: 0,
            job_id: "job-test".to_owned(),
            state_path: "/tmp/job-test.json".to_owned(),
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
            compression: None,
            moderation: crate::cli::Moderation::Auto,
            custom_ids: vec!["job-test-00".to_owned()],
            input_sha256: "0".repeat(64),
            input_bytes: 1,
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
        let job_file = Path::new("/tmp/job-test.json");
        let failure = publish_batch_content(
            job_file,
            &job,
            b"not-json",
            context_from_job("batch.retrieve", job_file, &job),
            200,
            Some("request-test".to_owned()),
        )
        .unwrap_err();
        assert_eq!(failure.error.code, "batch_result_invalid_json");
        assert_eq!(
            failure.context.job_file.as_deref(),
            Some("/tmp/job-test.json")
        );
        assert_eq!(failure.context.request_id.as_deref(), Some("request-test"));
        let mut invalid = serde_json::to_value(&job).unwrap();
        invalid["state"] = serde_json::json!("input_uploaded");
        assert!(validate_job(&serde_json::from_value(invalid).unwrap()).is_err());

        let mut unknown_job = serde_json::to_value(&job).unwrap();
        unknown_job["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<BatchJob>(unknown_job).is_err());

        let artifact = PublishingArtifact {
            path: "/tmp/images/one.png".to_owned(),
            sha256: "0".repeat(64),
            staged_path: "/tmp/images/.codex-image-stage-test".to_owned(),
            staged_identity: OutputIdentity {
                device: 1,
                inode: 2,
            },
            expected_target: None,
        };
        let mut unknown_artifact = serde_json::to_value(&artifact).unwrap();
        unknown_artifact["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PublishingArtifact>(unknown_artifact).is_err());

        let plan = PublishingPlan {
            artifacts: vec![artifact],
            staged_artifacts: vec!["/tmp/images/.codex-image-stage-test".to_owned()],
            retained_artifacts: Vec::new(),
            retained_artifact_ids: Vec::new(),
        };
        let mut unknown_plan = serde_json::to_value(&plan).unwrap();
        unknown_plan["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PublishingPlan>(unknown_plan).is_err());
    }

    #[test]
    fn loaded_jobs_reject_path_escape_output_names() {
        let mut job = BatchJob {
            schema_version: JOB_SCHEMA_VERSION,
            revision: 0,
            job_id: "job-test".to_owned(),
            state_path: "/tmp/job-test.json".to_owned(),
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
            compression: None,
            moderation: crate::cli::Moderation::Auto,
            custom_ids: vec!["job-test-00".to_owned()],
            input_sha256: "0".repeat(64),
            input_bytes: 1,
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
