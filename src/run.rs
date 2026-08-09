use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::unix::fs::MetadataExt;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use rustix::{
    fs::{mkdirat, openat, renameat_with, statat, AtFlags, Mode, OFlags, RenameFlags, CWD},
    io::Errno,
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{
    batch,
    cli::{
        Background, BatchJobArgs, BatchRetrieveArgs, GenerateArgs, Moderation, OutputFormat,
        Quality, RunBatchArgs, RunCommonArgs, RunDirectArgs, RunMode, RunPlanArgs,
    },
    endpoint::Endpoint,
    manifest::{self, Manifest, ManifestAsset},
    report::{AppError, Status},
    run_generate_with_preflight, MODEL,
};

const RUN_STATE_SCHEMA_VERSION: u8 = 1;
const DIRECT_RUN_STATE_SCHEMA_VERSION: u8 = 2;
const LEGACY_DIRECT_RUN_STATE_SCHEMA_VERSION: u8 = 1;
const MAX_RUN_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ASSET_STATE_BYTES: usize = 1024 * 1024;
const MAX_DIRECT_CONCURRENCY: usize = 4;
const MAX_BATCH_SHARD_SIZE: usize = crate::cli::MAX_BATCH_IMAGES as usize;
const MAX_BATCH_CONCURRENCY: usize = 4;

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub schema_version: u8,
    pub operation: &'static str,
    pub ok: bool,
    pub status: &'static str,
    pub exit_code: i32,
    pub plan_digest: String,
    pub manifest: String,
    pub total_assets: usize,
    pub started_assets: usize,
    pub succeeded_assets: usize,
    pub failed_assets: usize,
    pub outcome_unknown_assets: usize,
    pub pending_assets: usize,
    pub not_started_assets: usize,
    pub assets: Vec<RunAssetReport>,
    pub shards: Vec<RunShardReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RunAssetReport {
    pub id: String,
    pub output: String,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub outputs: Vec<String>,
    pub retained_artifacts: Vec<String>,
    pub possibly_modified_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RunAssetError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAssetError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedShardState {
    Planned,
    Submitting,
    Submitted,
    Observing,
    Retrieved,
    Failed,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedShard {
    index: usize,
    asset_ids: Vec<String>,
    job_file: String,
    state: PersistedShardState,
    batch_id: Option<String>,
    remote_status: Option<String>,
    error: Option<RunAssetError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchRunState {
    schema_version: u8,
    operation: String,
    plan_digest: String,
    manifest_sha256: String,
    output_dir: String,
    state_path: String,
    shards: Vec<PersistedShard>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug, Clone)]
struct BatchShardPlan {
    index: usize,
    start: usize,
    end: usize,
    job_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchObservationOutcome {
    Retrieved,
    Pending,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchClaim {
    Claimed,
    Skipped,
    CapacityFull,
}

#[derive(Debug)]
struct BatchStatusObservation {
    index: usize,
    batch_id: Option<String>,
    remote_status: Option<String>,
    state: PersistedShardState,
    error: Option<RunAssetError>,
}

#[derive(Debug, Serialize)]
pub struct RunShardReport {
    pub index: usize,
    pub job_file: String,
    pub asset_count: usize,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RunAssetError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedAssetState {
    Planned,
    DispatchInFlight,
    Succeeded,
    Failed,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAsset {
    id: String,
    output: String,
    state: PersistedAssetState,
    request_id: Option<String>,
    http_status: Option<u16>,
    outputs: Vec<String>,
    retained_artifacts: Vec<String>,
    possibly_modified_paths: Vec<String>,
    error: Option<PersistedAssetError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAssetError {
    code: String,
    message: String,
}

#[derive(Debug, Clone, Copy)]
struct AssetClaim {
    identity: Option<crate::output::OutputIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunState {
    schema_version: u8,
    operation: String,
    plan_digest: String,
    manifest_sha256: String,
    output_dir: String,
    state_path: String,
    assets: Vec<PersistedAsset>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug, Serialize)]
struct PlanPayload<'a> {
    schema_version: u8,
    operation: &'static str,
    manifest_sha256: String,
    output_dir: String,
    format: OutputFormat,
    size: &'a str,
    quality: Quality,
    confirm_high_quality: bool,
    background: Background,
    compression: Option<u8>,
    moderation: Moderation,
    overwrite: bool,
    timeout_seconds: u64,
    api_base_url: &'a str,
    dangerous_origin: Option<&'a str>,
    allow_insecure_localhost: bool,
    parallelism: usize,
    continue_on_error: bool,
    max_active_batches: usize,
    assets: Vec<PlanAsset<'a>>,
}

#[derive(Debug, Serialize)]
struct PlanAsset<'a> {
    id: &'a str,
    prompt: &'a str,
    output: &'a str,
}

#[derive(Debug, Clone)]
struct ExecutionPlan {
    manifest: Manifest,
    common: RunCommonArgs,
    output_dir: PathBuf,
    output_names: Vec<String>,
    digest: String,
    operation: &'static str,
}

#[derive(Debug, Clone)]
struct DirectExecution {
    max_concurrency: usize,
    continue_on_error: bool,
}

#[derive(Debug, Clone)]
struct PlanExecution {
    mode: RunMode,
    parallelism: usize,
    continue_on_error: bool,
    max_active_batches: usize,
}

pub fn plan(args: &RunPlanArgs) -> Result<RunReport, AppError> {
    let manifest = manifest::load(&args.common.manifest)?;
    if args.mode == RunMode::Batch
        && (!(1..=MAX_BATCH_CONCURRENCY).contains(&args.max_active_batches)
            || !(1..=86_400).contains(&args.max_wait_seconds)
            || !(1..=3_600).contains(&args.poll_interval_seconds))
    {
        return Err(AppError::usage(
            "invalid_batch_wait",
            "Batch plan settings require 1-4 active jobs, 1-86400 seconds maximum wait, and a 1-3600 second poll interval.",
        ));
    }
    let execution = PlanExecution {
        mode: args.mode,
        parallelism: args.parallelism,
        continue_on_error: args.continue_on_error,
        max_active_batches: if args.mode == RunMode::Batch {
            args.max_active_batches
        } else {
            1
        },
    };
    let plan = build_plan(&manifest, &args.common, execution)?;
    Ok(plan_report(&plan, "run.plan"))
}

pub fn direct(args: &RunDirectArgs) -> Result<RunReport, AppError> {
    let manifest = manifest::load(&args.common.manifest)?;
    let execution = DirectExecution {
        max_concurrency: args.max_concurrency,
        continue_on_error: args.continue_on_error,
    };
    let plan = build_plan(
        &manifest,
        &args.common,
        PlanExecution {
            mode: RunMode::Direct,
            parallelism: execution.max_concurrency,
            continue_on_error: execution.continue_on_error,
            max_active_batches: 1,
        },
    )?;
    if args.dry_run {
        return Ok(plan_report(&plan, "run.direct"));
    }
    validate_approval(args.approve_plan.as_deref(), &plan.digest)?;
    let run_file = args.run_file.as_deref().ok_or_else(|| {
        AppError::usage(
            "run_file_required",
            "A billable run requires --run-file so dispatch state can be recovered safely.",
        )
    })?;
    if !(1..=MAX_DIRECT_CONCURRENCY).contains(&execution.max_concurrency) {
        return Err(AppError::usage(
            "invalid_run_concurrency",
            "--max-concurrency must be between 1 and 4.",
        ));
    }
    let store = RunStore::open(run_file)?;
    let _execution_lock = store.acquire_execution_lock()?;
    store.initialize(&plan)?;
    let state = store.snapshot()?;
    preflight_outputs(&plan, &state, args.common.overwrite)?;
    if state.assets.iter().any(|asset| {
        matches!(
            asset.state,
            PersistedAssetState::DispatchInFlight | PersistedAssetState::OutcomeUnknown
        )
    }) {
        return Ok(report_from_state(&plan, &state, "run.direct"));
    }
    let pending = state
        .assets
        .iter()
        .filter(|asset| matches!(asset.state, PersistedAssetState::Planned))
        .count();
    if pending == 0 {
        return Ok(report_from_state(&plan, &state, "run.direct"));
    }

    let cursor = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let shared_plan = Arc::new(plan);
    let shared_store = Arc::new(store.clone());
    let worker_count = execution.max_concurrency.min(pending);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let cursor = Arc::clone(&cursor);
        let stop = Arc::clone(&stop);
        let plan = Arc::clone(&shared_plan);
        let store = Arc::clone(&shared_store);
        let continue_on_error = execution.continue_on_error;
        workers.push(thread::spawn(move || loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let index = cursor.fetch_add(1, Ordering::AcqRel);
            if index >= plan.manifest.assets.len() {
                break;
            }
            let claim = match store.claim_asset(index) {
                Ok(claim) => claim,
                Err(_) => {
                    stop.store(true, Ordering::Release);
                    break;
                }
            };
            let Some(claim) = claim else { continue };
            if store.verify_asset_claim(index, claim).is_err() {
                stop.store(true, Ordering::Release);
                break;
            }
            let asset = &plan.manifest.assets[index];
            let generation = generation_args(&plan, asset);
            let result =
                run_generate_with_preflight(&generation, || store.verify_asset_claim(index, claim));
            let hard_stop = match &result {
                Ok(report) => store
                    .update_asset(index, |asset| {
                        record_success(asset, report);
                        Ok(())
                    })
                    .is_err(),
                Err(error) => {
                    let unknown = matches!(
                        error.status,
                        Status::OutcomeIndeterminate
                            | Status::InvalidSuccessResponse
                            | Status::OutputCommitFailed
                    );
                    let recorded = store
                        .update_asset(index, |asset| {
                            record_error(asset, error, unknown);
                            Ok(())
                        })
                        .is_ok();
                    unknown || !recorded || !continue_on_error
                }
            };
            if hard_stop {
                stop.store(true, Ordering::Release);
            }
        }));
    }
    for worker in workers {
        if worker.join().is_err() {
            return Err(AppError::preflight(
                "run_worker_failed",
                "A direct run worker terminated unexpectedly; inspect the durable run state before resuming.",
            ));
        }
    }
    let state = store.snapshot()?;
    Ok(report_from_state(&shared_plan, &state, "run.direct"))
}

pub fn batch(args: &RunBatchArgs) -> Result<RunReport, AppError> {
    if !(1..=MAX_BATCH_SHARD_SIZE).contains(&args.shard_size) {
        return Err(AppError::usage(
            "invalid_batch_shard_size",
            "--shard-size must be between 1 and 8.",
        ));
    }
    if !(1..=MAX_BATCH_CONCURRENCY).contains(&args.max_active_batches) {
        return Err(AppError::usage(
            "invalid_batch_concurrency",
            "--max-active-batches must be between 1 and 4.",
        ));
    }
    if !(1..=86_400).contains(&args.max_wait_seconds)
        || !(1..=3_600).contains(&args.poll_interval_seconds)
    {
        return Err(AppError::usage(
            "invalid_batch_wait",
            "Batch wait settings must be between 1 and 86400 seconds, with a poll interval between 1 and 3600 seconds.",
        ));
    }
    let manifest = manifest::load(&args.common.manifest)?;
    let plan = build_plan(
        &manifest,
        &args.common,
        PlanExecution {
            mode: RunMode::Batch,
            parallelism: args.shard_size,
            continue_on_error: false,
            max_active_batches: args.max_active_batches,
        },
    )?;
    if args.dry_run {
        return Ok(plan_report(&plan, "run.batch"));
    }
    batch::ensure_billable_platform()?;
    validate_approval(args.approve_plan.as_deref(), &plan.digest)?;
    let run_file = args.run_file.as_deref().ok_or_else(|| {
        AppError::usage(
            "run_file_required",
            "A Batch run requires --run-file so shard submission state can be recovered safely.",
        )
    })?;
    let store = BatchStore::open(run_file)?;
    let shards = build_batch_shards(&plan, run_file, args.shard_size)?;
    store.initialize(&plan, &shards)?;
    let mut state = store.snapshot()?;
    preflight_batch_outputs(&plan, &state, args.common.overwrite)?;
    if state.shards.iter().any(|shard| {
        matches!(
            shard.state,
            PersistedShardState::Submitting
                | PersistedShardState::Observing
                | PersistedShardState::OutcomeUnknown
        )
    }) {
        reconcile_submitting_batch_shards(&plan, &shards, &store)?;
        state = store.snapshot()?;
        if state.shards.iter().any(|shard| {
            matches!(
                shard.state,
                PersistedShardState::Submitting
                    | PersistedShardState::Observing
                    | PersistedShardState::OutcomeUnknown
            )
        }) {
            return Ok(batch_report_from_state(&plan, &state));
        }
    }
    if args.wait {
        let submitted = state
            .shards
            .iter()
            .filter(|shard| matches!(shard.state, PersistedShardState::Submitted))
            .map(|shard| shard.index)
            .collect::<Vec<_>>();
        let mut wait_incomplete = false;
        for index in submitted {
            if !claim_batch_observation(&store, index)? {
                wait_incomplete = true;
                continue;
            }
            let outcome = observe_batch_shard(
                &plan,
                &shards[index],
                &store,
                args.max_wait_seconds,
                args.poll_interval_seconds,
            )?;
            if outcome != BatchObservationOutcome::Retrieved {
                wait_incomplete = true;
            }
        }
        if wait_incomplete {
            return Ok(batch_report_from_state(&plan, &store.snapshot()?));
        }
    } else {
        refresh_submitted_batch_status(&plan, &shards, &store)?;
    }
    let state = store.snapshot()?;
    let active_remote = state
        .shards
        .iter()
        .filter(|shard| {
            matches!(shard.state, PersistedShardState::Observing)
                || (matches!(shard.state, PersistedShardState::Submitted)
                    && is_remote_active(shard.remote_status.as_deref()))
        })
        .count();
    let available_slots = args.max_active_batches.saturating_sub(active_remote);
    let pending = state
        .shards
        .iter()
        .filter(|shard| matches!(shard.state, PersistedShardState::Planned))
        .count();
    if pending == 0 || available_slots == 0 {
        return Ok(batch_report_from_state(&plan, &state));
    }
    let cursor = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let shared_plan = Arc::new(plan);
    let shared_shards = Arc::new(shards);
    let shared_store = Arc::new(store.clone());
    let max_active_batches = args.max_active_batches;
    let worker_count = if args.wait {
        1
    } else {
        available_slots.min(pending)
    };
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let cursor = Arc::clone(&cursor);
        let stop = Arc::clone(&stop);
        let plan = Arc::clone(&shared_plan);
        let shards = Arc::clone(&shared_shards);
        let store = Arc::clone(&shared_store);
        let wait = args.wait;
        let max_wait_seconds = args.max_wait_seconds;
        let poll_interval_seconds = args.poll_interval_seconds;
        workers.push(thread::spawn(move || loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let index = cursor.fetch_add(1, Ordering::AcqRel);
            if index >= shards.len() {
                break;
            }
            let shard = &shards[index];
            let claim = claim_batch_shard(&store, index, max_active_batches);
            let claim = match claim {
                Ok(claim) => claim,
                Err(_) => {
                    stop.store(true, Ordering::Release);
                    break;
                }
            };
            match claim {
                BatchClaim::Claimed => {}
                BatchClaim::Skipped => continue,
                BatchClaim::CapacityFull => break,
            }
            let assets = &plan.manifest.assets[shard.start..shard.end];
            let mut hard_stop = false;
            let generation = generation_args(&plan, &assets[0]);
            match batch::submit_manifest(&generation, assets, &shard.job_file) {
                Ok(report) => {
                    if store
                        .update(|state| {
                            let persisted = &mut state.shards[index];
                            persisted.state = PersistedShardState::Submitted;
                            persisted.batch_id = report.batch_id.clone();
                            persisted.remote_status = report.remote_status.clone();
                            persisted.error = None;
                            Ok(())
                        })
                        .is_err()
                    {
                        hard_stop = true;
                    } else if wait {
                        hard_stop = match claim_batch_observation(&store, index) {
                            Ok(true) => observe_batch_shard(
                                &plan,
                                shard,
                                &store,
                                max_wait_seconds,
                                poll_interval_seconds,
                            )
                            .map(|outcome| outcome == BatchObservationOutcome::Failed)
                            .unwrap_or(true),
                            Ok(false) | Err(_) => true,
                        };
                    }
                }
                Err(failure) => {
                    let unknown = matches!(
                        failure.error.status,
                        Status::OutcomeIndeterminate
                            | Status::InvalidSuccessResponse
                            | Status::OutputCommitFailed
                    );
                    let _ = store.update(|state| {
                        let persisted = &mut state.shards[index];
                        persisted.state = if unknown {
                            PersistedShardState::OutcomeUnknown
                        } else {
                            PersistedShardState::Failed
                        };
                        persisted.batch_id = failure.context.batch_id.clone();
                        persisted.remote_status = failure.context.remote_status.clone();
                        persisted.error = Some(RunAssetError {
                            code: failure.error.code.to_owned(),
                            message: failure.error.message.clone(),
                        });
                        Ok(())
                    });
                    hard_stop = true;
                }
            }
            if hard_stop {
                stop.store(true, Ordering::Release);
            }
            if !wait {
                break;
            }
        }));
    }
    for worker in workers {
        if worker.join().is_err() {
            return Err(AppError::preflight(
                "run_worker_failed",
                "A Batch run worker terminated unexpectedly; inspect the durable coordinator state before resuming.",
            ));
        }
    }
    let state = store.snapshot()?;
    Ok(batch_report_from_state(&shared_plan, &state))
}
fn build_plan(
    manifest: &Manifest,
    common: &RunCommonArgs,
    execution: PlanExecution,
) -> Result<ExecutionPlan, AppError> {
    if common.max_assets == 0 || manifest.assets.len() > common.max_assets {
        return Err(AppError::usage(
            "run_asset_limit",
            format!(
                "The manifest contains {} assets but --max-assets permits {}.",
                manifest.assets.len(),
                common.max_assets
            ),
        ));
    }
    if common.timeout_seconds == 0 || common.timeout_seconds > 300 {
        return Err(AppError::usage(
            "invalid_timeout",
            "Run HTTP timeout must be between 1 and 300 seconds.",
        ));
    }
    match execution.mode {
        RunMode::Direct if !(1..=MAX_DIRECT_CONCURRENCY).contains(&execution.parallelism) => {
            return Err(AppError::usage(
                "invalid_run_concurrency",
                "Direct run parallelism must be between 1 and 4.",
            ));
        }
        RunMode::Batch if !(1..=MAX_BATCH_SHARD_SIZE).contains(&execution.parallelism) => {
            return Err(AppError::usage(
                "invalid_batch_shard_size",
                "Batch run parallelism/shard size must be between 1 and 8.",
            ));
        }
        _ => {}
    }
    let output_dir = manifest::validate_output_directory(&common.output_dir)?;
    Endpoint::authorize(
        &common.api_base_url,
        common.dangerously_allow_api_key_to.as_deref(),
        common.allow_insecure_localhost,
    )?;
    let output_names = manifest
        .assets
        .iter()
        .map(|asset| asset.output_name(common.format))
        .collect::<Vec<_>>();
    for asset in &manifest.assets {
        let generation = generation_args_for_plan(common, asset);
        generation.validate(&asset.prompt)?;
    }
    let payload = PlanPayload {
        schema_version: RUN_STATE_SCHEMA_VERSION,
        operation: match execution.mode {
            RunMode::Direct => "run.direct",
            RunMode::Batch => "run.batch",
        },
        manifest_sha256: manifest.source_sha256.clone(),
        output_dir: output_dir.to_string_lossy().into_owned(),
        format: common.format,
        size: &common.size,
        quality: common.quality,
        confirm_high_quality: common.confirm_high_quality,
        background: common.background,
        compression: common.compression,
        moderation: common.moderation,
        overwrite: common.overwrite,
        timeout_seconds: common.timeout_seconds,
        api_base_url: &common.api_base_url,
        dangerous_origin: common.dangerously_allow_api_key_to.as_deref(),
        allow_insecure_localhost: common.allow_insecure_localhost,
        parallelism: execution.parallelism,
        continue_on_error: execution.continue_on_error,
        max_active_batches: execution.max_active_batches,
        assets: manifest
            .assets
            .iter()
            .zip(&output_names)
            .map(|(asset, output)| PlanAsset {
                id: &asset.id,
                prompt: &asset.prompt,
                output,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&payload).map_err(|_| {
        AppError::preflight(
            "run_plan_unavailable",
            "The run plan could not be serialized safely; no request was sent.",
        )
    })?;
    let digest = manifest::sha256(&bytes);
    let operation = payload.operation;
    drop(payload);
    Ok(ExecutionPlan {
        manifest: manifest.clone(),
        common: common.clone(),
        output_dir,
        output_names,
        digest,
        operation,
    })
}

fn generation_args_for_plan(common: &RunCommonArgs, asset: &ManifestAsset) -> GenerateArgs {
    GenerateArgs {
        request_file: None,
        provider: crate::cli::Provider::Api,
        prompt: Some(asset.prompt.clone()),
        prompt_file: None,
        n: 1,
        output_dir: common.output_dir.clone(),
        name: Some(asset.stem.clone()),
        prefix: None,
        format: common.format,
        size: common.size.clone(),
        quality: common.quality,
        confirm_high_quality: common.confirm_high_quality,
        background: common.background,
        compression: common.compression,
        moderation: common.moderation,
        overwrite: common.overwrite,
        dry_run: false,
        timeout_seconds: common.timeout_seconds,
        api_base_url: common.api_base_url.clone(),
        dangerously_allow_api_key_to: common.dangerously_allow_api_key_to.clone(),
        allow_insecure_localhost: common.allow_insecure_localhost,
    }
}

fn generation_args(plan: &ExecutionPlan, asset: &ManifestAsset) -> GenerateArgs {
    generation_args_for_plan(&plan.common, asset)
}

fn validate_approval(approval: Option<&str>, digest: &str) -> Result<(), AppError> {
    let approval = approval.ok_or_else(|| {
        AppError::usage(
            "plan_approval_required",
            "A billable run requires --approve-plan with the exact digest from `run plan` or a dry run.",
        )
    })?;
    if approval.len() != 64
        || !approval
            .chars()
            .all(|character| character.is_ascii_hexdigit())
        || approval != digest
    {
        return Err(AppError::usage(
            "plan_digest_mismatch",
            "--approve-plan does not match the current manifest, parameters, output directory, and scheduler settings.",
        ));
    }
    Ok(())
}

fn preflight_outputs(
    plan: &ExecutionPlan,
    state: &RunState,
    overwrite: bool,
) -> Result<(), AppError> {
    for (index, asset) in state.assets.iter().enumerate() {
        if !matches!(asset.state, PersistedAssetState::Planned) {
            continue;
        }
        if !overwrite && crate::output::entry_exists(&plan.output_dir, &plan.output_names[index])? {
            return Err(AppError::preflight(
                "output_collision",
                "A planned run output already exists; no run requests were sent.",
            ));
        }
    }
    Ok(())
}

fn plan_report(plan: &ExecutionPlan, operation: &'static str) -> RunReport {
    let assets = plan
        .manifest
        .assets
        .iter()
        .zip(&plan.output_names)
        .map(|(asset, output)| RunAssetReport {
            id: asset.id.clone(),
            output: plan.output_dir.join(output).to_string_lossy().into_owned(),
            state: "planned",
            request_id: None,
            http_status: None,
            outputs: Vec::new(),
            retained_artifacts: Vec::new(),
            possibly_modified_paths: Vec::new(),
            error: None,
        })
        .collect::<Vec<_>>();
    RunReport {
        schema_version: RUN_STATE_SCHEMA_VERSION,
        operation,
        ok: true,
        status: "dry_run",
        exit_code: 0,
        plan_digest: plan.digest.clone(),
        manifest: plan.manifest.path.to_string_lossy().into_owned(),
        total_assets: assets.len(),
        started_assets: 0,
        succeeded_assets: 0,
        failed_assets: 0,
        outcome_unknown_assets: 0,
        pending_assets: 0,
        not_started_assets: assets.len(),
        assets,
        shards: Vec::new(),
        next_action: Some(format!(
            "approve plan {} and provide --run-file before any billable request",
            plan.digest
        )),
    }
}

fn report_from_state(plan: &ExecutionPlan, state: &RunState, operation: &'static str) -> RunReport {
    let mut started_assets = 0;
    let mut succeeded_assets = 0;
    let mut failed_assets = 0;
    let mut outcome_unknown_assets = 0;
    let mut not_started_assets = 0;
    let assets = state
        .assets
        .iter()
        .map(|asset| {
            let state_name = match asset.state {
                PersistedAssetState::Planned => {
                    not_started_assets += 1;
                    "not_started"
                }
                PersistedAssetState::DispatchInFlight => {
                    started_assets += 1;
                    outcome_unknown_assets += 1;
                    "outcome_unknown"
                }
                PersistedAssetState::Succeeded => {
                    started_assets += 1;
                    succeeded_assets += 1;
                    "succeeded"
                }
                PersistedAssetState::Failed => {
                    started_assets += 1;
                    failed_assets += 1;
                    "failed"
                }
                PersistedAssetState::OutcomeUnknown => {
                    started_assets += 1;
                    outcome_unknown_assets += 1;
                    "outcome_unknown"
                }
            };
            RunAssetReport {
                id: asset.id.clone(),
                output: plan
                    .output_dir
                    .join(&asset.output)
                    .to_string_lossy()
                    .into_owned(),
                state: state_name,
                request_id: asset.request_id.clone(),
                http_status: asset.http_status,
                outputs: asset.outputs.clone(),
                retained_artifacts: asset.retained_artifacts.clone(),
                possibly_modified_paths: asset.possibly_modified_paths.clone(),
                error: asset.error.as_ref().map(|error| RunAssetError {
                    code: error.code.clone(),
                    message: error.message.clone(),
                }),
            }
        })
        .collect::<Vec<_>>();
    let status = if outcome_unknown_assets > 0 {
        "outcome_unknown"
    } else if failed_assets > 0 && succeeded_assets > 0 {
        "partial_success"
    } else if failed_assets > 0 {
        "failed"
    } else if not_started_assets > 0 {
        "stopped"
    } else {
        "success"
    };
    let exit_code = if outcome_unknown_assets > 0 {
        Status::OutcomeIndeterminate.exit_code()
    } else if failed_assets > 0 || not_started_assets > 0 {
        Status::BatchFailed.exit_code()
    } else {
        0
    };
    let next_action = match status {
        "success" => None,
        "outcome_unknown" => Some(
            "inspect dispatch-in-flight assets and reconcile their API activity; do not rerun them automatically"
                .to_owned(),
        ),
        _ => Some("correct definitive failures and resume only planned assets, or start a new run explicitly".to_owned()),
    };
    RunReport {
        schema_version: RUN_STATE_SCHEMA_VERSION,
        operation,
        ok: status == "success",
        status,
        exit_code,
        plan_digest: state.plan_digest.clone(),
        manifest: plan.manifest.path.to_string_lossy().into_owned(),
        total_assets: assets.len(),
        started_assets,
        succeeded_assets,
        failed_assets,
        outcome_unknown_assets,
        pending_assets: 0,
        not_started_assets,
        assets,
        shards: Vec::new(),
        next_action,
    }
}

fn build_batch_shards(
    plan: &ExecutionPlan,
    run_file: &Path,
    shard_size: usize,
) -> Result<Vec<BatchShardPlan>, AppError> {
    let run_file = absolute_path(run_file)?;
    let parent = run_file.parent().ok_or_else(|| {
        AppError::preflight(
            "run_file_unavailable",
            "The Batch run file has no parent directory.",
        )
    })?;
    let stem = run_file
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("run");
    Ok(plan
        .manifest
        .assets
        .chunks(shard_size)
        .enumerate()
        .map(|(index, assets)| BatchShardPlan {
            index,
            start: index * shard_size,
            end: index * shard_size + assets.len(),
            job_file: parent.join(format!("{stem}.shard-{index:05}.json")),
        })
        .collect())
}

fn preflight_batch_outputs(
    plan: &ExecutionPlan,
    state: &BatchRunState,
    overwrite: bool,
) -> Result<(), AppError> {
    if overwrite {
        return Ok(());
    }
    let planned_ids = state
        .shards
        .iter()
        .filter(|shard| matches!(shard.state, PersistedShardState::Planned))
        .flat_map(|shard| shard.asset_ids.iter())
        .collect::<std::collections::HashSet<_>>();
    for asset in &plan.manifest.assets {
        if planned_ids.contains(&asset.id)
            && crate::output::entry_exists(
                &plan.output_dir,
                &asset.output_name(plan.common.format),
            )?
        {
            return Err(AppError::preflight(
                "output_collision",
                "A planned Batch run output already exists; no Batch request was sent.",
            ));
        }
    }
    Ok(())
}

fn batch_report_from_state(plan: &ExecutionPlan, state: &BatchRunState) -> RunReport {
    let mut started_assets = 0;
    let mut succeeded_assets = 0;
    let mut failed_assets = 0;
    let mut outcome_unknown_assets = 0;
    let mut pending_assets = 0;
    let mut not_started_assets = 0;
    let shards = state
        .shards
        .iter()
        .map(|shard| {
            let asset_count = shard.asset_ids.len();
            let state_name = match shard.state {
                PersistedShardState::Planned => {
                    not_started_assets += asset_count;
                    "not_started"
                }
                PersistedShardState::Submitting
                | PersistedShardState::Observing
                | PersistedShardState::OutcomeUnknown => {
                    started_assets += asset_count;
                    outcome_unknown_assets += asset_count;
                    "outcome_unknown"
                }
                PersistedShardState::Submitted => {
                    started_assets += asset_count;
                    pending_assets += asset_count;
                    "pending"
                }
                PersistedShardState::Retrieved => {
                    started_assets += asset_count;
                    succeeded_assets += asset_count;
                    "succeeded"
                }
                PersistedShardState::Failed => {
                    started_assets += asset_count;
                    failed_assets += asset_count;
                    "failed"
                }
            };
            RunShardReport {
                index: shard.index,
                job_file: shard.job_file.clone(),
                asset_count,
                state: state_name,
                batch_id: shard.batch_id.clone(),
                remote_status: shard.remote_status.clone(),
                error: shard.error.clone(),
            }
        })
        .collect::<Vec<_>>();
    let status = if outcome_unknown_assets > 0 {
        "outcome_unknown"
    } else if pending_assets > 0 {
        "pending"
    } else if failed_assets > 0 && succeeded_assets > 0 {
        "partial_success"
    } else if failed_assets > 0 {
        "failed"
    } else if not_started_assets > 0 {
        "stopped"
    } else {
        "success"
    };
    let exit_code = match status {
        "success" => 0,
        "outcome_unknown" => Status::OutcomeIndeterminate.exit_code(),
        "pending" => Status::BatchNotReady.exit_code(),
        _ => Status::BatchFailed.exit_code(),
    };
    let next_action = match status {
        "success" => None,
        "outcome_unknown" => Some(
            "inspect submitting/outcome-unknown child jobs and reconcile them before resuming; no duplicate Batch POST will be sent"
                .to_owned(),
        ),
        "pending" => Some("resume this run with --wait to observe and retrieve submitted child Batches".to_owned()),
        _ => Some("inspect failed child jobs and resume only after correcting the run explicitly".to_owned()),
    };
    RunReport {
        schema_version: RUN_STATE_SCHEMA_VERSION,
        operation: "run.batch",
        ok: status == "success",
        status,
        exit_code,
        plan_digest: state.plan_digest.clone(),
        manifest: plan.manifest.path.to_string_lossy().into_owned(),
        total_assets: plan.manifest.assets.len(),
        started_assets,
        succeeded_assets,
        failed_assets,
        outcome_unknown_assets,
        pending_assets,
        not_started_assets,
        assets: Vec::new(),
        shards,
        next_action,
    }
}

fn batch_job_args(plan: &ExecutionPlan, shard: &BatchShardPlan) -> BatchJobArgs {
    BatchJobArgs {
        job_file: shard.job_file.clone(),
        timeout_seconds: plan.common.timeout_seconds,
        dangerously_allow_api_key_to: plan.common.dangerously_allow_api_key_to.clone(),
        allow_insecure_localhost: plan.common.allow_insecure_localhost,
    }
}

fn reconcile_submitting_batch_shards(
    plan: &ExecutionPlan,
    shards: &[BatchShardPlan],
    store: &BatchStore,
) -> Result<(), AppError> {
    let state = store.snapshot()?;
    for persisted in state.shards.iter().filter(|shard| {
        matches!(
            shard.state,
            PersistedShardState::Submitting
                | PersistedShardState::Observing
                | PersistedShardState::OutcomeUnknown
        )
    }) {
        let shard_index = persisted.index;
        let shard = shards.get(shard_index).ok_or_else(|| {
            AppError::preflight(
                "run_state_invalid",
                "The Batch run shard index is outside the approved shard plan.",
            )
        })?;
        let Ok(job) = batch::inspect_job(&shard.job_file) else {
            continue;
        };
        validate_reconciled_child_job(plan, shard, &job)?;
        let Some((reconciled_state, error)) = reconciled_child_state(&job) else {
            continue;
        };
        store.update(|state| {
            let persisted = state.shards.get_mut(shard_index).ok_or_else(|| {
                AppError::preflight(
                    "run_state_invalid",
                    "The Batch run shard index changed unexpectedly during reconciliation.",
                )
            })?;
            if !matches!(
                persisted.state,
                PersistedShardState::Submitting
                    | PersistedShardState::Observing
                    | PersistedShardState::OutcomeUnknown
            ) {
                return Ok(());
            }
            persisted.state = reconciled_state;
            persisted.batch_id = job.batch_id.clone();
            persisted.remote_status = job.remote_status.clone();
            persisted.error = error.clone();
            Ok(())
        })?;
    }
    Ok(())
}

fn validate_reconciled_child_job(
    plan: &ExecutionPlan,
    shard: &BatchShardPlan,
    job: &batch::BatchJob,
) -> Result<(), AppError> {
    let expected_outputs = plan
        .output_names
        .get(shard.start..shard.end)
        .ok_or_else(|| {
            AppError::preflight(
                "run_state_invalid",
                "The Batch run shard range is outside the approved output plan.",
            )
        })?;
    let assets = plan
        .manifest
        .assets
        .get(shard.start..shard.end)
        .ok_or_else(|| {
            AppError::preflight(
                "run_state_invalid",
                "The Batch run shard range is outside the approved manifest.",
            )
        })?;
    let first_asset = assets.first().ok_or_else(|| {
        AppError::preflight(
            "run_state_invalid",
            "The Batch run shard must contain at least one approved asset.",
        )
    })?;
    let generation = generation_args(plan, first_asset);
    let (expected_input_sha256, expected_input_bytes) =
        batch::input_fingerprint(&generation, assets, &job.custom_ids)?;
    let immutable_matches = job.provider == crate::cli::Provider::Api
        && job.model == MODEL
        && job.api_base_url == plan.common.api_base_url
        && job.output_dir == plan.output_dir.to_string_lossy()
        && job.output_names == expected_outputs
        && usize::from(job.image_count) == shard.end - shard.start
        && job.overwrite == plan.common.overwrite
        && job.format == plan.common.format
        && job.quality == plan.common.quality
        && job.size == plan.common.size
        && job.background == plan.common.background
        && job.compression == plan.common.compression
        && job.moderation == plan.common.moderation
        && job.input_sha256 == expected_input_sha256
        && job.input_bytes == expected_input_bytes;
    if !immutable_matches {
        return Err(AppError::preflight(
            "run_state_invalid",
            "The durable child Batch job does not match the approved shard execution plan.",
        ));
    }
    Ok(())
}

fn reconciled_child_state(
    job: &batch::BatchJob,
) -> Option<(PersistedShardState, Option<RunAssetError>)> {
    match job.state {
        batch::JobState::Submitted | batch::JobState::Completed => {
            Some((PersistedShardState::Submitted, None))
        }
        batch::JobState::Retrieved => Some((PersistedShardState::Retrieved, None)),
        batch::JobState::Failed | batch::JobState::Cancelled => Some((
            PersistedShardState::Failed,
            Some(RunAssetError {
                code: "child_job_failed".to_owned(),
                message: "The durable child Batch job is in a terminal failed or cancelled state."
                    .to_owned(),
            }),
        )),
        // Retrieval can resume the child's durable publication journal.
        batch::JobState::Publishing => Some((PersistedShardState::Submitted, None)),
        batch::JobState::Prepared
        | batch::JobState::UploadInFlight
        | batch::JobState::UploadOutcomeUnknown
        | batch::JobState::InputUploaded
        | batch::JobState::CreateInFlight
        | batch::JobState::CreateOutcomeUnknown
        | batch::JobState::CancelInFlight
        | batch::JobState::CancelOutcomeUnknown => None,
    }
}

fn observe_batch_shard(
    plan: &ExecutionPlan,
    shard: &BatchShardPlan,
    store: &BatchStore,
    max_wait_seconds: u64,
    poll_interval_seconds: u64,
) -> Result<BatchObservationOutcome, AppError> {
    let retrieve = BatchRetrieveArgs {
        job: batch_job_args(plan, shard),
        wait: true,
        max_wait_seconds,
        poll_interval_seconds,
    };
    match batch::retrieve(&retrieve) {
        Ok(report) => {
            let retrieved = report.status == "retrieved";
            store.update(|state| {
                let persisted = state.shards.get_mut(shard.index).ok_or_else(|| {
                    AppError::preflight(
                        "run_state_invalid",
                        "The Batch run shard index changed unexpectedly during retrieval.",
                    )
                })?;
                if !matches!(persisted.state, PersistedShardState::Observing) {
                    return Ok(());
                }
                persisted.state = if retrieved {
                    PersistedShardState::Retrieved
                } else {
                    PersistedShardState::Failed
                };
                persisted.batch_id = report.batch_id.clone().or(persisted.batch_id.clone());
                persisted.remote_status = report.remote_status.clone();
                persisted.error = if retrieved {
                    None
                } else {
                    Some(RunAssetError {
                        code: "batch_not_retrieved".to_owned(),
                        message: "The child Batch reached a terminal state without publishing all requested assets.".to_owned(),
                    })
                };
                Ok(())
            })?;
            Ok(if retrieved {
                BatchObservationOutcome::Retrieved
            } else {
                BatchObservationOutcome::Failed
            })
        }
        Err(failure) => {
            let retryable = matches!(
                failure.error.status,
                Status::BatchNotReady | Status::BatchObservationFailed
            ) || failure.error.code == "job_changed_concurrently";
            store.update(|state| {
                let persisted = state.shards.get_mut(shard.index).ok_or_else(|| {
                    AppError::preflight(
                        "run_state_invalid",
                        "The Batch run shard index changed unexpectedly during retrieval.",
                    )
                })?;
                if !matches!(persisted.state, PersistedShardState::Observing) {
                    return Ok(());
                }
                persisted.state = if retryable {
                    PersistedShardState::Submitted
                } else {
                    PersistedShardState::Failed
                };
                persisted.batch_id = failure
                    .context
                    .batch_id
                    .clone()
                    .or(persisted.batch_id.clone());
                persisted.remote_status = failure.context.remote_status.clone();
                persisted.error = Some(RunAssetError {
                    code: failure.error.code.to_owned(),
                    message: failure.error.message.clone(),
                });
                Ok(())
            })?;
            Ok(if retryable {
                BatchObservationOutcome::Pending
            } else {
                BatchObservationOutcome::Failed
            })
        }
    }
}

fn refresh_submitted_batch_status(
    plan: &ExecutionPlan,
    shards: &[BatchShardPlan],
    store: &BatchStore,
) -> Result<(), AppError> {
    let state = store.snapshot()?;
    let mut observations = Vec::new();
    for persisted in state
        .shards
        .iter()
        .filter(|shard| matches!(shard.state, PersistedShardState::Submitted))
    {
        let shard = shards.get(persisted.index).ok_or_else(|| {
            AppError::preflight(
                "run_state_invalid",
                "The Batch run shard index is outside the approved shard plan.",
            )
        })?;
        let args = batch_job_args(plan, shard);
        match batch::status(&args) {
            Ok(report) => {
                let failed = matches!(
                    report.remote_status.as_deref(),
                    Some("failed" | "expired" | "cancelled")
                );
                let observed_state = if report.status == "retrieved"
                    || report.remote_status.as_deref() == Some("retrieved")
                {
                    PersistedShardState::Retrieved
                } else if failed {
                    PersistedShardState::Failed
                } else {
                    PersistedShardState::Submitted
                };
                observations.push(BatchStatusObservation {
                    index: shard.index,
                    batch_id: report.batch_id,
                    remote_status: report.remote_status,
                    state: observed_state,
                    error: failed.then(|| RunAssetError {
                        code: "batch_failed".to_owned(),
                        message: "The child Batch reached a terminal failed or cancelled state."
                            .to_owned(),
                    }),
                });
            }
            Err(failure) => {
                if failure.context.remote_status.is_some() {
                    let failed = matches!(
                        failure.context.remote_status.as_deref(),
                        Some("failed" | "expired" | "cancelled")
                    );
                    observations.push(BatchStatusObservation {
                        index: shard.index,
                        batch_id: failure.context.batch_id.clone(),
                        remote_status: failure.context.remote_status.clone(),
                        state: if failed {
                            PersistedShardState::Failed
                        } else {
                            PersistedShardState::Submitted
                        },
                        error: failed.then(|| RunAssetError {
                            code: failure.error.code.to_owned(),
                            message: failure.error.message.clone(),
                        }),
                    });
                }
            }
        }
    }
    if observations.is_empty() {
        return Ok(());
    }
    store.update(|state| {
        for observation in observations {
            let persisted = state.shards.get_mut(observation.index).ok_or_else(|| {
                AppError::preflight(
                    "run_state_invalid",
                    "The Batch run shard index changed unexpectedly during status refresh.",
                )
            })?;
            if !matches!(persisted.state, PersistedShardState::Submitted) {
                continue;
            }
            persisted.state = observation.state;
            persisted.batch_id = observation.batch_id.or(persisted.batch_id.take());
            persisted.remote_status = observation.remote_status;
            persisted.error = observation.error;
        }
        Ok(())
    })?;
    Ok(())
}

fn is_remote_active(status: Option<&str>) -> bool {
    !matches!(
        status,
        Some("completed" | "failed" | "expired" | "cancelled" | "retrieved")
    )
}

fn claim_batch_shard(
    store: &BatchStore,
    index: usize,
    max_active_batches: usize,
) -> Result<BatchClaim, AppError> {
    store.update_value(|state| claim_batch_shard_state(state, index, max_active_batches))
}

fn claim_batch_observation(store: &BatchStore, index: usize) -> Result<bool, AppError> {
    store.update_value(|state| {
        let persisted = state.shards.get_mut(index).ok_or_else(|| {
            AppError::preflight(
                "run_state_invalid",
                "The Batch run shard list changed unexpectedly.",
            )
        })?;
        if !matches!(persisted.state, PersistedShardState::Submitted) {
            return Ok(false);
        }
        persisted.state = PersistedShardState::Observing;
        Ok(true)
    })
}

fn claim_batch_shard_state(
    state: &mut BatchRunState,
    index: usize,
    max_active_batches: usize,
) -> Result<BatchClaim, AppError> {
    let planned = state.shards.get(index).ok_or_else(|| {
        AppError::preflight(
            "run_state_invalid",
            "The Batch run shard list changed unexpectedly.",
        )
    })?;
    if !matches!(planned.state, PersistedShardState::Planned) {
        return Ok(BatchClaim::Skipped);
    }
    let active_or_reserved = state
        .shards
        .iter()
        .filter(|shard| {
            matches!(shard.state, PersistedShardState::Submitting)
                || (matches!(shard.state, PersistedShardState::Submitted)
                    && is_remote_active(shard.remote_status.as_deref()))
        })
        .count();
    if active_or_reserved >= max_active_batches {
        return Ok(BatchClaim::CapacityFull);
    }
    state.shards[index].state = PersistedShardState::Submitting;
    Ok(BatchClaim::Claimed)
}

fn record_success(asset: &mut PersistedAsset, report: &crate::report::RunReport) {
    asset.state = PersistedAssetState::Succeeded;
    asset.request_id = report.request.request_id.clone();
    asset.http_status = report.http.status;
    asset.outputs = report.outputs.clone();
    asset.retained_artifacts = report.retained_artifacts.clone();
    asset.possibly_modified_paths = report.possibly_modified_paths.clone();
    asset.error = None;
}

fn record_error(asset: &mut PersistedAsset, error: &AppError, unknown: bool) {
    asset.state = if unknown {
        PersistedAssetState::OutcomeUnknown
    } else {
        PersistedAssetState::Failed
    };
    asset.request_id = error.request_id.clone();
    asset.http_status = error.http_status;
    asset.possibly_modified_paths = error
        .possibly_modified_paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    asset.error = Some(PersistedAssetError {
        code: error.code.to_owned(),
        message: error.message.clone(),
    });
}

#[derive(Debug, Clone)]
struct RunStore {
    path: PathBuf,
    thread_lock: Arc<Mutex<()>>,
    parent_directory: Arc<Mutex<Option<File>>>,
    asset_directory: Arc<Mutex<Option<File>>>,
}

impl RunStore {
    fn open(path: &Path) -> Result<Self, AppError> {
        let path = absolute_path(path)?;
        if path.to_str().is_none() {
            return Err(AppError::preflight(
                "run_file_unavailable",
                "The run file path must be valid UTF-8.",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            AppError::preflight(
                "run_file_unavailable",
                "The run file path has no parent directory.",
            )
        })?;
        let metadata = fs::symlink_metadata(parent).map_err(|_| {
            AppError::preflight(
                "run_file_unavailable",
                "The run file parent directory must already exist.",
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::preflight(
                "run_file_unavailable",
                "The run file parent directory must be a non-symlink directory.",
            ));
        }
        manifest::validate_path_components(
            parent,
            "run_file_unavailable",
            "The run file path must not contain symlinked components or '..'.",
        )?;
        let parent_directory = open_pinned_directory(parent).map_err(|_| {
            AppError::preflight(
                "run_file_unavailable",
                "The run file parent directory could not be pinned safely.",
            )
        })?;
        let path_metadata = fs::symlink_metadata(&path).ok();
        if path_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(AppError::preflight(
                "run_file_unavailable",
                "The run file must not be a symlink.",
            ));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if path_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_file() && metadata.nlink() != 1)
        {
            return Err(AppError::preflight(
                "run_file_hard_linked",
                "The run file must have exactly one filesystem link; use the original path rather than an alias.",
            ));
        }
        Ok(Self {
            path,
            thread_lock: Arc::new(Mutex::new(())),
            parent_directory: Arc::new(Mutex::new(Some(parent_directory))),
            asset_directory: Arc::new(Mutex::new(None)),
        })
    }

    fn initialize(&self, plan: &ExecutionPlan) -> Result<(), AppError> {
        self.with_lock(|store| {
            if store.state_file_exists()? {
                let state = store.read_base_unlocked()?;
                store.validate_state(&state, plan)?;
                if state.schema_version == LEGACY_DIRECT_RUN_STATE_SCHEMA_VERSION {
                    store.migrate_legacy_state(&state)?;
                } else {
                    store.pin_existing_asset_state_dir()?;
                    store.validate_asset_state_files(&state)?;
                }
                return Ok(());
            }
            match fs::symlink_metadata(store.asset_state_dir()) {
                Ok(_) => {
                    return Err(AppError::preflight(
                        "run_state_orphaned",
                        "Per-asset run state exists without its coordinator file; inspect it before cleanup rather than starting a new billable run.",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(AppError::preflight(
                        "run_state_unavailable",
                        "The per-asset run state path could not be inspected safely.",
                    ));
                }
            }
            let now = now_seconds();
            let state = RunState {
                schema_version: DIRECT_RUN_STATE_SCHEMA_VERSION,
                operation: plan.operation.to_owned(),
                plan_digest: plan.digest.clone(),
                manifest_sha256: plan.manifest.source_sha256.clone(),
                output_dir: plan.output_dir.to_string_lossy().into_owned(),
                state_path: self.path.to_string_lossy().into_owned(),
                assets: plan
                    .manifest
                    .assets
                    .iter()
                    .zip(&plan.output_names)
                    .map(|(asset, output)| PersistedAsset {
                        id: asset.id.clone(),
                        output: output.clone(),
                        state: PersistedAssetState::Planned,
                        request_id: None,
                        http_status: None,
                        outputs: Vec::new(),
                        retained_artifacts: Vec::new(),
                        possibly_modified_paths: Vec::new(),
                        error: None,
                    })
                    .collect(),
                created_at: now,
                updated_at: now,
            };
            store.ensure_asset_state_dir()?;
            store.write_all_asset_state(&state)?;
            store.write_unlocked(&state)
        })
    }

    fn state_file_exists(&self) -> Result<bool, AppError> {
        self.with_parent_directory(|parent| {
            if !pinned_path_exists(parent, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_file_unavailable",
                    "The run state path could not be inspected safely.",
                )
            })? {
                return Ok(false);
            }
            let file = open_pinned_asset_file(parent, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_file_unavailable",
                    "The run state path could not be opened safely.",
                )
            })?;
            let metadata = file.metadata().map_err(|_| {
                AppError::preflight(
                    "run_file_unavailable",
                    "The run state file could not be inspected safely.",
                )
            })?;
            if !metadata.is_file() {
                return Err(AppError::preflight(
                    "run_file_unavailable",
                    "The run state path must be a regular file.",
                ));
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if metadata.nlink() != 1 {
                return Err(AppError::preflight(
                    "run_file_hard_linked",
                    "The run file must have exactly one filesystem link; use the original path rather than an alias.",
                ));
            }
            Ok(true)
        })
    }

    fn snapshot(&self) -> Result<RunState, AppError> {
        self.with_lock(|store| store.read_unlocked())
    }

    fn update_asset<F, T>(&self, index: usize, update: F) -> Result<T, AppError>
    where
        F: FnOnce(&mut PersistedAsset) -> Result<T, AppError>,
    {
        self.with_lock(|store| {
            let state = store.read_base_unlocked()?;
            if state.schema_version != DIRECT_RUN_STATE_SCHEMA_VERSION {
                return Err(AppError::preflight(
                    "run_state_invalid",
                    "The direct run coordinator has not completed its durable state migration.",
                ));
            }
            store.pin_existing_asset_state_dir()?;
            let base_asset = state.assets.get(index).ok_or_else(|| {
                AppError::preflight(
                    "run_state_invalid",
                    "The run state asset list changed unexpectedly.",
                )
            })?;
            let mut asset = store.read_asset_unlocked(index, base_asset)?;
            let value = update(&mut asset)?;
            store.write_asset_unlocked(index, &state, &asset)?;
            Ok(value)
        })
    }

    fn claim_asset(&self, index: usize) -> Result<Option<AssetClaim>, AppError> {
        let marked = self.update_asset(index, |asset| {
            if !matches!(asset.state, PersistedAssetState::Planned) {
                return Ok(false);
            }
            asset.state = PersistedAssetState::DispatchInFlight;
            Ok(true)
        })?;
        if !marked {
            return Ok(None);
        }
        self.with_lock(|store| {
            Ok(Some(AssetClaim {
                identity: store.asset_identity_unlocked(index)?,
            }))
        })
    }

    fn verify_asset_claim(&self, index: usize, claim: AssetClaim) -> Result<(), AppError> {
        self.with_lock(|store| {
            let state = store.read_base_unlocked()?;
            if state.schema_version != DIRECT_RUN_STATE_SCHEMA_VERSION {
                return Err(AppError::preflight(
                    "run_state_invalid",
                    "The direct run coordinator has not completed its durable state migration.",
                ));
            }
            store.pin_existing_asset_state_dir()?;
            let base_asset = state.assets.get(index).ok_or_else(|| {
                AppError::preflight(
                    "run_state_invalid",
                    "The run state asset list changed unexpectedly.",
                )
            })?;
            let asset = store.read_asset_unlocked(index, base_asset)?;
            if !matches!(asset.state, PersistedAssetState::DispatchInFlight) {
                return Err(AppError::preflight(
                    "run_state_changed",
                    "The direct asset claim changed before its request was dispatched; inspect durable state before resuming.",
                ));
            }
            if store.asset_identity_unlocked(index)? != claim.identity {
                return Err(AppError::preflight(
                    "run_state_changed",
                    "The direct asset claim file changed before its request was dispatched; inspect durable state before resuming.",
                ));
            }
            Ok(())
        })
    }

    fn asset_identity_unlocked(
        &self,
        index: usize,
    ) -> Result<Option<crate::output::OutputIdentity>, AppError> {
        let path = self.asset_state_path(index);
        self.with_asset_directory(|directory| {
            let file = open_pinned_asset_file(directory, &path).map_err(|_| {
                AppError::preflight(
                    "run_state_incomplete",
                    "The per-asset run state file could not be opened before dispatch; do not retry the request.",
                )
            })?;
            let metadata = file.metadata().map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "The per-asset run state file could not be inspected safely.",
                )
            })?;
            if !metadata.is_file() {
                return Err(AppError::preflight(
                    "run_state_invalid",
                    "The per-asset run state file must be a regular file.",
                ));
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                if metadata.nlink() != 1 {
                    return Err(AppError::preflight(
                        "run_state_hard_linked",
                        "The per-asset run state file must have exactly one filesystem link.",
                    ));
                }
                Ok(Some(crate::output::OutputIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }))
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                Ok(None)
            }
        })
    }

    fn with_lock<F, T>(&self, operation: F) -> Result<T, AppError>
    where
        F: FnOnce(&RunStore) -> Result<T, AppError>,
    {
        let _thread_lock = self.thread_lock.lock().map_err(|_| {
            AppError::preflight(
                "run_lock_unavailable",
                "The run state lock could not be acquired safely after a worker failure.",
            )
        })?;
        operation(self)
    }

    fn acquire_execution_lock(&self) -> Result<File, AppError> {
        let directory = self.with_parent_directory(|directory| {
            open_directory_for_lock(directory).map_err(|_| {
                AppError::preflight(
                    "run_execution_lock_unavailable",
                    "The direct run execution directory could not be opened safely.",
                )
            })
        })?;
        directory.lock_exclusive().map_err(|_| {
            AppError::preflight(
                "run_execution_lock_unavailable",
                "The direct run execution directory lock could not be acquired safely.",
            )
        })?;
        Ok(directory)
    }

    fn with_parent_directory<F, T>(&self, operation: F) -> Result<T, AppError>
    where
        F: FnOnce(&File) -> Result<T, AppError>,
    {
        let pinned = self.parent_directory.lock().map_err(|_| {
            AppError::preflight(
                "run_lock_unavailable",
                "The run state parent directory lock could not be acquired safely.",
            )
        })?;
        let directory = pinned.as_ref().ok_or_else(|| {
            AppError::preflight(
                "run_file_unavailable",
                "The run state parent directory has not been pinned safely.",
            )
        })?;
        let parent = self.path.parent().ok_or_else(|| {
            AppError::preflight(
                "run_file_unavailable",
                "The run state parent directory could not be resolved safely.",
            )
        })?;
        let current = open_pinned_directory(parent).map_err(|_| {
            AppError::preflight(
                "run_state_changed",
                "The run state parent directory could not be revalidated safely.",
            )
        })?;
        if !pinned_directory_matches(directory, &current) {
            return Err(AppError::preflight(
                "run_state_changed",
                "The run state parent directory changed during execution; inspect durable state before resuming.",
            ));
        }
        operation(directory)
    }

    fn validate_state_path(&self, state_path: &str) -> Result<(), AppError> {
        if state_path != self.path.to_string_lossy() {
            return Err(AppError::preflight(
                "run_file_alias",
                "The durable run state belongs to a different state-file path; use the original run file path.",
            ));
        }
        Ok(())
    }

    fn read_unlocked(&self) -> Result<RunState, AppError> {
        let mut state = self.read_base_unlocked()?;
        if state.schema_version != DIRECT_RUN_STATE_SCHEMA_VERSION {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The direct run coordinator has not completed its durable state migration.",
            ));
        }
        self.pin_existing_asset_state_dir()?;
        self.validate_asset_state_files(&state)?;
        for index in 0..state.assets.len() {
            let base_asset = state.assets.get(index).ok_or_else(|| {
                AppError::preflight(
                    "run_state_invalid",
                    "The run state asset list changed unexpectedly.",
                )
            })?;
            state.assets[index] = self.read_asset_unlocked(index, base_asset)?;
        }
        Ok(state)
    }

    fn read_base_unlocked(&self) -> Result<RunState, AppError> {
        let mut file = self.with_parent_directory(|directory| {
            open_pinned_asset_file(directory, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "The durable run state file could not be opened safely.",
                )
            })
        })?;
        let metadata = file.metadata().map_err(|_| {
            AppError::preflight(
                "run_state_invalid",
                "The durable run state file could not be inspected safely.",
            )
        })?;
        if !metadata.is_file() || metadata.len() > MAX_RUN_FILE_BYTES as u64 {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The durable run state file is unsafe, not regular, or too large.",
            ));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if metadata.nlink() != 1 {
            return Err(AppError::preflight(
                "run_state_hard_linked",
                "The durable run state file must have exactly one filesystem link.",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_RUN_FILE_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "The durable run state file could not be read.",
                )
            })?;
        if bytes.len() > MAX_RUN_FILE_BYTES {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The durable run state file is too large.",
            ));
        }
        let state: RunState = serde_json::from_slice(&bytes).map_err(|_| {
            AppError::preflight(
                "run_state_invalid",
                "The durable run state file is not valid JSON matching the run contract.",
            )
        })?;
        self.validate_state_path(&state.state_path)?;
        Ok(state)
    }

    fn asset_state_dir(&self) -> PathBuf {
        PathBuf::from(format!("{}.assets", self.path.display()))
    }

    fn asset_state_path(&self, index: usize) -> PathBuf {
        self.asset_state_dir()
            .join(format!("asset-{index:08}.json"))
    }

    fn validate_asset_state_files(&self, state: &RunState) -> Result<(), AppError> {
        self.with_asset_directory(|directory| {
            for index in 0..state.assets.len() {
                let path = self.asset_state_path(index);
                let file = open_pinned_asset_file(directory, &path).map_err(|_| {
                    if fs::symlink_metadata(&path).is_err() {
                        AppError::preflight(
                            "run_state_incomplete",
                            "A per-asset run state file is missing; do not start a duplicate billable run.",
                        )
                    } else {
                        AppError::preflight(
                            "run_state_invalid",
                            "Every per-asset run state entry must be a regular non-symlink file.",
                        )
                    }
                })?;
                let metadata = file.metadata().map_err(|_| {
                    AppError::preflight(
                        "run_state_invalid",
                        "A per-asset run state file could not be inspected safely.",
                    )
                })?;
                if !metadata.is_file() || metadata.len() > MAX_ASSET_STATE_BYTES as u64 {
                    return Err(AppError::preflight(
                        "run_state_invalid",
                        "Every per-asset run state entry must be a regular file within the local size limit.",
                    ));
                }
            }
            Ok(())
        })
    }

    fn write_all_asset_state(&self, state: &RunState) -> Result<(), AppError> {
        for index in 0..state.assets.len() {
            let asset = state.assets.get(index).ok_or_else(|| {
                AppError::preflight(
                    "run_state_invalid",
                    "The run state asset list changed unexpectedly.",
                )
            })?;
            self.write_asset_unlocked(index, state, asset)?;
        }
        Ok(())
    }

    fn migrate_legacy_state(&self, state: &RunState) -> Result<(), AppError> {
        match fs::symlink_metadata(self.asset_state_dir()) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(AppError::preflight(
                        "run_state_invalid",
                        "The legacy per-asset run state path must be a regular non-symlink directory.",
                    ));
                }
                self.pin_existing_asset_state_dir()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.ensure_asset_state_dir()?;
            }
            Err(_) => {
                return Err(AppError::preflight(
                    "run_state_unavailable",
                    "The legacy per-asset run state path could not be inspected safely.",
                ));
            }
        }
        for index in 0..state.assets.len() {
            let base_asset = state.assets.get(index).ok_or_else(|| {
                AppError::preflight(
                    "run_state_invalid",
                    "The legacy run state asset list changed unexpectedly.",
                )
            })?;
            if self.asset_state_file_exists(index)? {
                let sidecar = self.read_asset_unlocked(index, base_asset)?;
                if sidecar != *base_asset {
                    return Err(AppError::preflight(
                        "run_state_invalid",
                        "The legacy per-asset state does not match its coordinator; inspect it before resuming.",
                    ));
                }
            } else {
                self.write_asset_unlocked(index, state, base_asset)?;
            }
        }
        let mut migrated = state.clone();
        migrated.schema_version = DIRECT_RUN_STATE_SCHEMA_VERSION;
        migrated.updated_at = now_seconds();
        self.write_unlocked(&migrated)
    }

    fn ensure_asset_state_dir(&self) -> Result<(), AppError> {
        let directory = self.asset_state_dir();
        match fs::symlink_metadata(&directory) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(AppError::preflight(
                        "run_state_unavailable",
                        "The per-asset run state path must be a regular directory.",
                    ));
                }
                self.pin_existing_asset_state_dir()?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.with_parent_directory(|parent| {
                    create_pinned_asset_directory(parent, &directory).map_err(|_| {
                        AppError::preflight(
                            "run_state_unavailable",
                            "The per-asset run state directory could not be created safely.",
                        )
                    })
                })?;
            }
            Err(_) => {
                return Err(AppError::preflight(
                    "run_state_unavailable",
                    "The per-asset run state directory could not be inspected safely.",
                ));
            }
        }
        self.with_parent_directory(|parent| {
            parent.sync_all().map_err(|_| {
                AppError::preflight(
                    "run_directory_sync_failed",
                    "The run state parent directory could not be synchronized after creating per-asset state.",
                )
            })
        })?;
        self.pin_existing_asset_state_dir()
    }

    fn pin_existing_asset_state_dir(&self) -> Result<(), AppError> {
        let directory = self.asset_state_dir();
        let metadata = fs::symlink_metadata(&directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::preflight(
                    "run_state_incomplete",
                    "The per-asset run state directory is missing; do not start a duplicate billable run.",
                )
            } else {
                AppError::preflight(
                    "run_state_invalid",
                    "The per-asset run state directory could not be inspected safely.",
                )
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The per-asset run state directory must be a regular non-symlink directory.",
            ));
        }
        let current = self.with_parent_directory(|parent| {
            open_pinned_child_directory(parent, &directory).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The per-asset run state directory could not be pinned safely.",
                )
            })
        })?;
        let mut pinned = self.asset_directory.lock().map_err(|_| {
            AppError::preflight(
                "run_lock_unavailable",
                "The per-asset run state directory lock could not be acquired safely.",
            )
        })?;
        if let Some(existing) = pinned.as_ref() {
            if !pinned_directory_matches(existing, &current) {
                return Err(AppError::preflight(
                    "run_state_changed",
                    "The per-asset run state directory changed during execution; inspect durable state before resuming.",
                ));
            }
        } else {
            *pinned = Some(current);
        }
        Ok(())
    }

    fn with_asset_directory<F, T>(&self, operation: F) -> Result<T, AppError>
    where
        F: FnOnce(&File) -> Result<T, AppError>,
    {
        let directory_path = self.asset_state_dir();
        let current = self.with_parent_directory(|parent| {
            open_pinned_child_directory(parent, &directory_path).map_err(|_| {
                AppError::preflight(
                    "run_state_changed",
                    "The per-asset run state directory could not be revalidated safely.",
                )
            })
        })?;
        let pinned = self.asset_directory.lock().map_err(|_| {
            AppError::preflight(
                "run_lock_unavailable",
                "The per-asset run state directory lock could not be acquired safely.",
            )
        })?;
        let directory = pinned.as_ref().ok_or_else(|| {
            AppError::preflight(
                "run_state_unavailable",
                "The per-asset run state directory has not been pinned safely.",
            )
        })?;
        if !pinned_directory_matches(directory, &current) {
            return Err(AppError::preflight(
                "run_state_changed",
                "The per-asset run state directory changed during execution; inspect durable state before resuming.",
            ));
        }
        operation(directory)
    }

    fn asset_state_file_exists(&self, index: usize) -> Result<bool, AppError> {
        let path = self.asset_state_path(index);
        self.with_asset_directory(|directory| {
            if !pinned_path_exists(directory, &path).map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "A per-asset run state path could not be inspected safely.",
                )
            })? {
                return Ok(false);
            }
            let file = open_pinned_asset_file(directory, &path).map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "A per-asset run state path is unsafe or not a regular non-symlink file.",
                )
            })?;
            let metadata = file.metadata().map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "A per-asset run state file could not be inspected safely.",
                )
            })?;
            if !metadata.is_file() || metadata.len() > MAX_ASSET_STATE_BYTES as u64 {
                return Err(AppError::preflight(
                    "run_state_invalid",
                    "A per-asset run state file is unsafe, not regular, or too large.",
                ));
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if metadata.nlink() != 1 {
                return Err(AppError::preflight(
                    "run_state_hard_linked",
                    "The per-asset run state file must have exactly one filesystem link.",
                ));
            }
            Ok(true)
        })
    }

    fn read_asset_unlocked(
        &self,
        index: usize,
        base_asset: &PersistedAsset,
    ) -> Result<PersistedAsset, AppError> {
        let path = self.asset_state_path(index);
        let mut file = self.with_asset_directory(|directory| {
            open_pinned_asset_file(directory, &path).map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "The per-asset run state file could not be opened safely.",
                )
            })
        })?;
        let metadata = file.metadata().map_err(|_| {
            AppError::preflight(
                "run_state_invalid",
                "The per-asset run state file could not be inspected safely.",
            )
        })?;
        if !metadata.is_file() || metadata.len() > MAX_ASSET_STATE_BYTES as u64 {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The per-asset run state file is unsafe, not regular, or too large.",
            ));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if metadata.nlink() != 1 {
            return Err(AppError::preflight(
                "run_state_hard_linked",
                "The per-asset run state file must have exactly one filesystem link.",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_ASSET_STATE_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "The per-asset run state file could not be read.",
                )
            })?;
        if bytes.len() > MAX_ASSET_STATE_BYTES {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The per-asset run state file is too large.",
            ));
        }
        let asset: PersistedAsset = serde_json::from_slice(&bytes).map_err(|_| {
            AppError::preflight(
                "run_state_invalid",
                "The per-asset run state file is not valid JSON.",
            )
        })?;
        if asset.id != base_asset.id || asset.output != base_asset.output {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The per-asset run state mapping is inconsistent with the approved plan.",
            ));
        }
        Ok(asset)
    }

    fn write_asset_unlocked(
        &self,
        index: usize,
        state: &RunState,
        asset: &PersistedAsset,
    ) -> Result<(), AppError> {
        let base_asset = state.assets.get(index).ok_or_else(|| {
            AppError::preflight(
                "run_state_invalid",
                "The run state asset list changed unexpectedly.",
            )
        })?;
        if asset.id != base_asset.id || asset.output != base_asset.output {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The per-asset run state mapping cannot be changed.",
            ));
        }
        self.ensure_asset_state_dir()?;
        let path = self.asset_state_path(index);
        self.with_asset_directory(|directory| {
            if !pinned_path_exists(directory, &path).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The per-asset run state path could not be inspected safely.",
                )
            })? {
                return Ok(());
            }
            let file = open_pinned_asset_file(directory, &path).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The per-asset run state path must be a regular non-symlink file.",
                )
            })?;
            let metadata = file.metadata().map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The per-asset run state file could not be inspected safely.",
                )
            })?;
            if !metadata.is_file() {
                return Err(AppError::preflight(
                    "run_state_unavailable",
                    "The per-asset run state path must be a regular file.",
                ));
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if metadata.nlink() != 1 {
                return Err(AppError::preflight(
                    "run_state_hard_linked",
                    "The per-asset run state file must have exactly one filesystem link.",
                ));
            }
            Ok(())
        })?;
        let bytes = serde_json::to_vec(asset).map_err(|_| {
            AppError::preflight(
                "run_state_unavailable",
                "The per-asset run state could not be serialized safely.",
            )
        })?;
        if bytes.len() > MAX_ASSET_STATE_BYTES {
            return Err(AppError::preflight(
                "run_state_too_large",
                "The per-asset run state exceeded its local safety limit.",
            ));
        }
        let temporary = self
            .asset_state_dir()
            .join(format!(".asset-{index:08}.tmp-{}", now_nanos()));
        self.with_asset_directory(|directory| {
            let mut file = create_pinned_asset_temp(directory, &temporary).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The temporary per-asset run state file could not be created safely.",
                )
            })?;
            if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
                return Err(AppError::preflight(
                    "run_state_unavailable",
                    format!("The per-asset run state could not be synchronized: {error}"),
                ));
            }
            rename_pinned_asset_file(directory, &temporary, &path).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The per-asset run state could not be atomically published.",
                )
            })?;
            directory.sync_all().map_err(|_| {
                AppError::preflight(
                    "run_directory_sync_failed",
                    "The per-asset run state directory could not be synchronized after the atomic update.",
                )
            })
        })
    }

    fn validate_state(&self, state: &RunState, plan: &ExecutionPlan) -> Result<(), AppError> {
        if !matches!(
            state.schema_version,
            LEGACY_DIRECT_RUN_STATE_SCHEMA_VERSION | DIRECT_RUN_STATE_SCHEMA_VERSION
        ) || state.operation != plan.operation
            || state.plan_digest != plan.digest
            || state.manifest_sha256 != plan.manifest.source_sha256
            || state.output_dir != plan.output_dir.to_string_lossy()
            || state.state_path != self.path.to_string_lossy()
            || state.assets.len() != plan.manifest.assets.len()
        {
            return Err(AppError::preflight(
                "run_plan_mismatch",
                "The durable run state does not match the current manifest, parameters, output directory, or mode.",
            ));
        }
        for ((persisted, asset), output) in state
            .assets
            .iter()
            .zip(&plan.manifest.assets)
            .zip(&plan.output_names)
        {
            if persisted.id != asset.id || persisted.output != *output {
                return Err(AppError::preflight(
                    "run_state_invalid",
                    "The durable run state asset mapping is inconsistent with the approved plan.",
                ));
            }
        }
        Ok(())
    }

    fn write_unlocked(&self, state: &RunState) -> Result<(), AppError> {
        if state.schema_version != DIRECT_RUN_STATE_SCHEMA_VERSION {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The direct run coordinator has an unsupported state schema.",
            ));
        }
        self.validate_state_path(&state.state_path)?;
        self.with_parent_directory(|parent| {
            if pinned_path_exists(parent, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The durable run state path could not be inspected safely.",
                )
            })? {
                let file = open_pinned_asset_file(parent, &self.path).map_err(|_| {
                    AppError::preflight(
                        "run_state_unavailable",
                        "The durable run state path could not be opened safely.",
                    )
                })?;
                let metadata = file.metadata().map_err(|_| {
                    AppError::preflight(
                        "run_state_unavailable",
                        "The durable run state file could not be inspected safely.",
                    )
                })?;
                if !metadata.is_file() {
                    return Err(AppError::preflight(
                        "run_state_unavailable",
                        "The durable run state path must be a regular file.",
                    ));
                }
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if metadata.nlink() != 1 {
                    return Err(AppError::preflight(
                        "run_state_hard_linked",
                        "The durable run state file must have exactly one filesystem link.",
                    ));
                }
            }
            Ok(())
        })?;
        let bytes = serde_json::to_vec_pretty(state).map_err(|_| {
            AppError::preflight(
                "run_state_unavailable",
                "The run state could not be serialized safely.",
            )
        })?;
        if bytes.len() > MAX_RUN_FILE_BYTES {
            return Err(AppError::preflight(
                "run_state_too_large",
                "The durable run state exceeded its local safety limit.",
            ));
        }
        let temporary = PathBuf::from(format!(
            "{}.tmp-{}-{}",
            self.path.display(),
            std::process::id(),
            now_nanos()
        ));
        self.with_parent_directory(|parent| {
            let mut file = create_pinned_asset_temp(parent, &temporary).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The temporary run state file could not be created safely.",
                )
            })?;
            if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
                return Err(AppError::preflight(
                    "run_state_unavailable",
                    format!("The durable run state could not be synchronized: {error}"),
                ));
            }
            rename_pinned_asset_file(parent, &temporary, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The durable run state could not be atomically published.",
                )
            })?;
            parent.sync_all().map_err(|_| {
                AppError::preflight(
                    "run_directory_sync_failed",
                    "The run state parent directory could not be synchronized after the atomic update.",
                )
            })
        })
    }
}

#[derive(Debug, Clone)]
struct BatchStore {
    path: PathBuf,
    thread_lock: Arc<Mutex<()>>,
    parent_directory: Arc<Mutex<Option<File>>>,
}

impl BatchStore {
    fn open(path: &Path) -> Result<Self, AppError> {
        let path = absolute_path(path)?;
        if path.to_str().is_none() {
            return Err(AppError::preflight(
                "run_file_unavailable",
                "The Batch run file path must be valid UTF-8.",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            AppError::preflight(
                "run_file_unavailable",
                "The Batch run file has no parent directory.",
            )
        })?;
        let metadata = fs::symlink_metadata(parent).map_err(|_| {
            AppError::preflight(
                "run_file_unavailable",
                "The Batch run file parent directory must already exist.",
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::preflight(
                "run_file_unavailable",
                "The Batch run file parent directory must be a non-symlink directory.",
            ));
        }
        manifest::validate_path_components(
            parent,
            "run_file_unavailable",
            "The Batch run file path must not contain symlinked components or '..'.",
        )?;
        let parent_directory = open_pinned_directory(parent).map_err(|_| {
            AppError::preflight(
                "run_file_unavailable",
                "The Batch run file parent directory could not be pinned safely.",
            )
        })?;
        let path_metadata = fs::symlink_metadata(&path).ok();
        if path_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(AppError::preflight(
                "run_file_unavailable",
                "The Batch run file must not be a symlink.",
            ));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if path_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_file() && metadata.nlink() != 1)
        {
            return Err(AppError::preflight(
                "run_file_hard_linked",
                "The Batch run file must have exactly one filesystem link; use the original path rather than an alias.",
            ));
        }
        Ok(Self {
            path,
            thread_lock: Arc::new(Mutex::new(())),
            parent_directory: Arc::new(Mutex::new(Some(parent_directory))),
        })
    }

    fn initialize(&self, plan: &ExecutionPlan, shards: &[BatchShardPlan]) -> Result<(), AppError> {
        self.with_lock(|store| {
            if store.state_file_exists()? {
                let state = store.read_unlocked()?;
                store.validate_state(&state, plan, shards)?;
                return Ok(());
            }
            let now = now_seconds();
            let state = BatchRunState {
                schema_version: RUN_STATE_SCHEMA_VERSION,
                operation: "run.batch".to_owned(),
                plan_digest: plan.digest.clone(),
                manifest_sha256: plan.manifest.source_sha256.clone(),
                output_dir: plan.output_dir.to_string_lossy().into_owned(),
                state_path: self.path.to_string_lossy().into_owned(),
                shards: shards
                    .iter()
                    .map(|shard| PersistedShard {
                        index: shard.index,
                        asset_ids: plan.manifest.assets[shard.start..shard.end]
                            .iter()
                            .map(|asset| asset.id.clone())
                            .collect(),
                        job_file: shard.job_file.to_string_lossy().into_owned(),
                        state: PersistedShardState::Planned,
                        batch_id: None,
                        remote_status: None,
                        error: None,
                    })
                    .collect(),
                created_at: now,
                updated_at: now,
            };
            store.write_unlocked(&state)
        })
    }

    fn state_file_exists(&self) -> Result<bool, AppError> {
        self.with_parent_directory(|parent| {
            if !pinned_path_exists(parent, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_file_unavailable",
                    "The Batch run state path could not be inspected safely.",
                )
            })? {
                return Ok(false);
            }
            let file = open_pinned_asset_file(parent, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_file_unavailable",
                    "The Batch run state path could not be opened safely.",
                )
            })?;
            let metadata = file.metadata().map_err(|_| {
                AppError::preflight(
                    "run_file_unavailable",
                    "The Batch run state file could not be inspected safely.",
                )
            })?;
            if !metadata.is_file() {
                return Err(AppError::preflight(
                    "run_file_unavailable",
                    "The Batch run state path must be a regular file.",
                ));
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if metadata.nlink() != 1 {
                return Err(AppError::preflight(
                    "run_file_hard_linked",
                    "The Batch run file must have exactly one filesystem link; use the original path rather than an alias.",
                ));
            }
            Ok(true)
        })
    }

    fn snapshot(&self) -> Result<BatchRunState, AppError> {
        self.with_lock(|store| store.read_unlocked())
    }

    fn update<F>(&self, update: F) -> Result<(), AppError>
    where
        F: FnOnce(&mut BatchRunState) -> Result<(), AppError>,
    {
        self.update_value(|state| {
            update(state)?;
            Ok(())
        })
    }

    fn update_value<F, T>(&self, update: F) -> Result<T, AppError>
    where
        F: FnOnce(&mut BatchRunState) -> Result<T, AppError>,
    {
        self.with_lock(|store| {
            let mut state = store.read_unlocked()?;
            let value = update(&mut state)?;
            state.updated_at = now_seconds();
            store.write_unlocked(&state)?;
            Ok(value)
        })
    }

    fn validate_state_path(&self, state_path: &str) -> Result<(), AppError> {
        if state_path != self.path.to_string_lossy() {
            return Err(AppError::preflight(
                "run_file_alias",
                "The durable Batch run state belongs to a different state-file path; use the original run file path.",
            ));
        }
        Ok(())
    }

    fn with_lock<F, T>(&self, operation: F) -> Result<T, AppError>
    where
        F: FnOnce(&BatchStore) -> Result<T, AppError>,
    {
        let _thread_lock = self.thread_lock.lock().map_err(|_| {
            AppError::preflight(
                "run_lock_unavailable",
                "The Batch run state lock could not be acquired safely after a worker failure.",
            )
        })?;
        let _directory_lock = self.with_parent_directory(|directory| {
            let directory = open_directory_for_lock(directory).map_err(|_| {
                AppError::preflight(
                    "run_lock_unavailable",
                    "The Batch run directory could not be opened safely.",
                )
            })?;
            directory.lock_exclusive().map_err(|_| {
                AppError::preflight(
                    "run_lock_unavailable",
                    "The Batch run directory lock could not be acquired safely.",
                )
            })?;
            Ok(directory)
        })?;
        operation(self)
    }

    fn with_parent_directory<F, T>(&self, operation: F) -> Result<T, AppError>
    where
        F: FnOnce(&File) -> Result<T, AppError>,
    {
        let pinned = self.parent_directory.lock().map_err(|_| {
            AppError::preflight(
                "run_lock_unavailable",
                "The Batch run state parent directory lock could not be acquired safely.",
            )
        })?;
        let directory = pinned.as_ref().ok_or_else(|| {
            AppError::preflight(
                "run_file_unavailable",
                "The Batch run state parent directory has not been pinned safely.",
            )
        })?;
        let parent = self.path.parent().ok_or_else(|| {
            AppError::preflight(
                "run_file_unavailable",
                "The Batch run state parent directory could not be resolved safely.",
            )
        })?;
        let current = open_pinned_directory(parent).map_err(|_| {
            AppError::preflight(
                "run_state_changed",
                "The Batch run state parent directory could not be revalidated safely.",
            )
        })?;
        if !pinned_directory_matches(directory, &current) {
            return Err(AppError::preflight(
                "run_state_changed",
                "The Batch run state parent directory changed during execution; inspect durable state before resuming.",
            ));
        }
        operation(directory)
    }

    fn read_unlocked(&self) -> Result<BatchRunState, AppError> {
        let mut file = self.with_parent_directory(|directory| {
            open_pinned_asset_file(directory, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "The durable Batch run state file could not be opened safely.",
                )
            })
        })?;
        let metadata = file.metadata().map_err(|_| {
            AppError::preflight(
                "run_state_invalid",
                "The durable Batch run state file could not be inspected safely.",
            )
        })?;
        if !metadata.is_file() || metadata.len() > MAX_RUN_FILE_BYTES as u64 {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The durable Batch run state file is unsafe, not regular, or too large.",
            ));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if metadata.nlink() != 1 {
            return Err(AppError::preflight(
                "run_state_hard_linked",
                "The durable Batch run state file must have exactly one filesystem link.",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_RUN_FILE_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| {
                AppError::preflight(
                    "run_state_invalid",
                    "The durable Batch run state file could not be read.",
                )
            })?;
        if bytes.len() > MAX_RUN_FILE_BYTES {
            return Err(AppError::preflight(
                "run_state_invalid",
                "The durable Batch run state file is too large.",
            ));
        }
        let state: BatchRunState = serde_json::from_slice(&bytes).map_err(|_| {
            AppError::preflight(
                "run_state_invalid",
                "The durable Batch run state file is not valid JSON matching the run contract.",
            )
        })?;
        self.validate_state_path(&state.state_path)?;
        Ok(state)
    }

    fn validate_state(
        &self,
        state: &BatchRunState,
        plan: &ExecutionPlan,
        shards: &[BatchShardPlan],
    ) -> Result<(), AppError> {
        if state.schema_version != RUN_STATE_SCHEMA_VERSION
            || state.operation != "run.batch"
            || state.plan_digest != plan.digest
            || state.manifest_sha256 != plan.manifest.source_sha256
            || state.output_dir != plan.output_dir.to_string_lossy()
            || state.state_path != self.path.to_string_lossy()
            || state.shards.len() != shards.len()
        {
            return Err(AppError::preflight(
                "run_plan_mismatch",
                "The durable Batch run state does not match the current manifest, parameters, shard policy, or output directory.",
            ));
        }
        for (persisted, shard) in state.shards.iter().zip(shards) {
            let ids = &plan.manifest.assets[shard.start..shard.end]
                .iter()
                .map(|asset| asset.id.clone())
                .collect::<Vec<_>>();
            if persisted.index != shard.index
                || persisted.job_file != shard.job_file.to_string_lossy()
                || persisted.asset_ids != *ids
            {
                return Err(AppError::preflight(
                    "run_state_invalid",
                    "The durable Batch run shard mapping is inconsistent with the approved plan.",
                ));
            }
        }
        Ok(())
    }

    fn write_unlocked(&self, state: &BatchRunState) -> Result<(), AppError> {
        self.validate_state_path(&state.state_path)?;
        self.with_parent_directory(|parent| {
            if pinned_path_exists(parent, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The durable Batch run state path could not be inspected safely.",
                )
            })? {
                let file = open_pinned_asset_file(parent, &self.path).map_err(|_| {
                    AppError::preflight(
                        "run_state_unavailable",
                        "The durable Batch run state path could not be opened safely.",
                    )
                })?;
                let metadata = file.metadata().map_err(|_| {
                    AppError::preflight(
                        "run_state_unavailable",
                        "The durable Batch run state file could not be inspected safely.",
                    )
                })?;
                if !metadata.is_file() {
                    return Err(AppError::preflight(
                        "run_state_unavailable",
                        "The durable Batch run state path must be a regular file.",
                    ));
                }
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if metadata.nlink() != 1 {
                    return Err(AppError::preflight(
                        "run_state_hard_linked",
                        "The durable Batch run state file must have exactly one filesystem link.",
                    ));
                }
            }
            Ok(())
        })?;
        let bytes = serde_json::to_vec_pretty(state).map_err(|_| {
            AppError::preflight(
                "run_state_unavailable",
                "The Batch run state could not be serialized safely.",
            )
        })?;
        if bytes.len() > MAX_RUN_FILE_BYTES {
            return Err(AppError::preflight(
                "run_state_too_large",
                "The durable Batch run state exceeded its local safety limit.",
            ));
        }
        let temporary = PathBuf::from(format!(
            "{}.tmp-{}-{}",
            self.path.display(),
            std::process::id(),
            now_nanos()
        ));
        self.with_parent_directory(|parent| {
            let mut file = create_pinned_asset_temp(parent, &temporary).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The temporary Batch run state file could not be created safely.",
                )
            })?;
            if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
                return Err(AppError::preflight(
                    "run_state_unavailable",
                    format!("The durable Batch run state could not be synchronized: {error}"),
                ));
            }
            rename_pinned_asset_file(parent, &temporary, &self.path).map_err(|_| {
                AppError::preflight(
                    "run_state_unavailable",
                    "The durable Batch run state could not be atomically published.",
                )
            })?;
            parent.sync_all().map_err(|_| {
                AppError::preflight(
                    "run_directory_sync_failed",
                    "The Batch run state parent directory could not be synchronized after the atomic update.",
                )
            })
        })
    }
}

pub(crate) fn open_pinned_directory(path: &Path) -> Result<File, ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let root = openat(
            CWD,
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| ())?;
        let mut current: File = root.into();
        for component in path.components() {
            let Component::Normal(name) = component else {
                if matches!(component, Component::ParentDir) {
                    return Err(());
                }
                continue;
            };
            let fd = openat(
                &current,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| ())?;
            current = fd.into();
        }
        Ok(current)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        File::open(path).map_err(|_| ())
    }
}

fn open_pinned_child_directory(parent: &File, path: &Path) -> Result<File, ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = path.file_name().ok_or(())?;
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| ())?;
        Ok(fd.into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = parent;
        File::open(path).map_err(|_| ())
    }
}

pub(crate) fn open_directory_for_lock(parent: &File) -> Result<File, ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let fd = openat(
            parent,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| ())?;
        Ok(fd.into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        parent.try_clone().map_err(|_| ())
    }
}

pub(crate) fn open_pinned_lock_file(directory: &File, path: &Path) -> Result<File, ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = path.file_name().ok_or(())?;
        let fd = openat(
            directory,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| ())?;
        Ok(fd.into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = directory;
        fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| ())
    }
}

pub(crate) fn pinned_path_exists(directory: &File, path: &Path) -> Result<bool, ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = path.file_name().ok_or(())?;
        match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(_) => Err(()),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(()),
        }
    }
}

fn create_pinned_asset_directory(parent: &File, path: &Path) -> Result<(), ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = path.file_name().ok_or(())?;
        mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(|_| ())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = parent;
        fs::create_dir(path).map_err(|_| ())
    }
}

pub(crate) fn pinned_directory_matches(expected: &File, current: &File) -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let Ok(expected) = expected.metadata() else {
            return false;
        };
        let Ok(current) = current.metadata() else {
            return false;
        };
        expected.dev() == current.dev() && expected.ino() == current.ino()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (expected, current);
        true
    }
}

pub(crate) fn open_pinned_asset_file(directory: &File, path: &Path) -> Result<File, ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = path.file_name().ok_or(())?;
        let fd = openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| ())?;
        Ok(fd.into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = directory;
        File::open(path).map_err(|_| ())
    }
}

pub(crate) fn create_pinned_asset_temp(directory: &File, path: &Path) -> Result<File, ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = path.file_name().ok_or(())?;
        let fd = openat(
            directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| ())?;
        Ok(fd.into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = directory;
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|_| ())
    }
}

pub(crate) fn rename_pinned_asset_file(
    directory: &File,
    source: &Path,
    destination: &Path,
) -> Result<(), ()> {
    rename_pinned_asset_file_with_policy(directory, source, destination, false)
}

pub(crate) fn rename_pinned_asset_file_with_policy(
    directory: &File,
    source: &Path,
    destination: &Path,
    no_clobber: bool,
) -> Result<(), ()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let source_name = source.file_name().ok_or(())?;
        let destination_name = destination.file_name().ok_or(())?;
        renameat_with(
            directory,
            source_name,
            directory,
            destination_name,
            if no_clobber {
                RenameFlags::NOREPLACE
            } else {
                RenameFlags::empty()
            },
        )
        .map_err(|_| ())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = directory;
        if no_clobber && fs::symlink_metadata(destination).is_ok() {
            return Err(());
        }
        fs::rename(source, destination).map_err(|_| ())
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    if path.is_absolute() {
        return normalize_absolute_path(path);
    }
    std::env::current_dir()
        .map_err(|_| {
            AppError::preflight(
                "working_directory_unavailable",
                "The current directory could not be resolved.",
            )
        })
        .and_then(|directory| normalize_absolute_path(&directory.join(path)))
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => {
                return Err(AppError::preflight(
                    "unsafe_run_path",
                    "The run state path must not contain '..'.",
                ));
            }
        }
    }
    Ok(normalized)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_common() -> RunCommonArgs {
        RunCommonArgs {
            manifest: PathBuf::from("/tmp/assets.jsonl"),
            output_dir: PathBuf::from("/tmp/images"),
            max_assets: 3,
            format: OutputFormat::Png,
            size: "1024x1024".to_owned(),
            quality: Quality::Low,
            confirm_high_quality: false,
            background: Background::Auto,
            compression: None,
            moderation: Moderation::Auto,
            overwrite: false,
            timeout_seconds: 180,
            api_base_url: "https://api.openai.com/v1".to_owned(),
            dangerously_allow_api_key_to: None,
            allow_insecure_localhost: false,
        }
    }

    fn test_child_job(state: batch::JobState) -> batch::BatchJob {
        batch::BatchJob {
            schema_version: batch::JOB_SCHEMA_VERSION,
            revision: 0,
            job_id: "job-test".to_owned(),
            state_path: "/tmp/batch-run.shard-00000.json".to_owned(),
            state,
            provider: crate::cli::Provider::Api,
            model: MODEL.to_owned(),
            api_base_url: "https://api.openai.com/v1".to_owned(),
            output_dir: "/tmp/images".to_owned(),
            output_names: vec!["asset-1.png".to_owned()],
            overwrite: false,
            format: OutputFormat::Png,
            image_count: 1,
            quality: Quality::Low,
            size: "1024x1024".to_owned(),
            background: Background::Auto,
            compression: None,
            moderation: Moderation::Auto,
            custom_ids: vec!["job-test-00".to_owned()],
            input_sha256: "0".repeat(64),
            input_bytes: 1,
            input_file_id: Some("file-input".to_owned()),
            batch_id: Some("batch-test".to_owned()),
            output_file_id: None,
            error_file_id: None,
            remote_status: Some("validating".to_owned()),
            request_counts: None,
            publishing: None,
            retained_artifacts: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn pending_batch_work_takes_precedence_over_partial_failure() {
        let assets = (1..=3)
            .map(|index| ManifestAsset {
                id: format!("asset-{index}"),
                prompt: format!("asset {index}"),
                stem: format!("asset-{index}"),
            })
            .collect::<Vec<_>>();
        let common = test_common();
        let plan = ExecutionPlan {
            manifest: Manifest {
                path: common.manifest.clone(),
                assets,
                source_sha256: "0".repeat(64),
                bytes: 1,
            },
            common,
            output_dir: PathBuf::from("/tmp/images"),
            output_names: vec![
                "asset-1.png".to_owned(),
                "asset-2.png".to_owned(),
                "asset-3.png".to_owned(),
            ],
            digest: "1".repeat(64),
            operation: "run.batch",
        };
        let state = BatchRunState {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            operation: "run.batch".to_owned(),
            plan_digest: plan.digest.clone(),
            manifest_sha256: "0".repeat(64),
            output_dir: "/tmp/images".to_owned(),
            state_path: "/tmp/batch-run.json".to_owned(),
            shards: vec![
                PersistedShard {
                    index: 0,
                    asset_ids: vec!["asset-1".to_owned()],
                    job_file: "/tmp/shard-0.json".to_owned(),
                    state: PersistedShardState::Retrieved,
                    batch_id: Some("batch-0".to_owned()),
                    remote_status: Some("retrieved".to_owned()),
                    error: None,
                },
                PersistedShard {
                    index: 1,
                    asset_ids: vec!["asset-2".to_owned()],
                    job_file: "/tmp/shard-1.json".to_owned(),
                    state: PersistedShardState::Failed,
                    batch_id: Some("batch-1".to_owned()),
                    remote_status: Some("failed".to_owned()),
                    error: Some(RunAssetError {
                        code: "batch_failed".to_owned(),
                        message: "failed".to_owned(),
                    }),
                },
                PersistedShard {
                    index: 2,
                    asset_ids: vec!["asset-3".to_owned()],
                    job_file: "/tmp/shard-2.json".to_owned(),
                    state: PersistedShardState::Submitted,
                    batch_id: Some("batch-2".to_owned()),
                    remote_status: Some("in_progress".to_owned()),
                    error: None,
                },
            ],
            created_at: 0,
            updated_at: 0,
        };
        let report = batch_report_from_state(&plan, &state);
        assert_eq!(report.status, "pending");
        assert_eq!(report.exit_code, 8);
        assert_eq!(report.pending_assets, 1);
        assert_eq!(report.failed_assets, 1);
    }

    #[test]
    fn batch_claim_counts_remote_active_and_reserved_shards() {
        let shard = |index, state, remote_status| PersistedShard {
            index,
            asset_ids: vec![format!("asset-{index}")],
            job_file: format!("/tmp/shard-{index}.json"),
            state,
            batch_id: None,
            remote_status,
            error: None,
        };
        let mut state = BatchRunState {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            operation: "run.batch".to_owned(),
            plan_digest: "0".repeat(64),
            manifest_sha256: "0".repeat(64),
            output_dir: "/tmp/images".to_owned(),
            state_path: "/tmp/batch-run.json".to_owned(),
            shards: vec![
                shard(
                    0,
                    PersistedShardState::Submitted,
                    Some("in_progress".to_owned()),
                ),
                shard(1, PersistedShardState::Planned, None),
                shard(2, PersistedShardState::Planned, None),
            ],
            created_at: 0,
            updated_at: 0,
        };

        assert_eq!(
            claim_batch_shard_state(&mut state, 1, 1).unwrap(),
            BatchClaim::CapacityFull
        );
        state.shards[0].state = PersistedShardState::Retrieved;
        assert_eq!(
            claim_batch_shard_state(&mut state, 1, 1).unwrap(),
            BatchClaim::Claimed
        );
        assert!(matches!(
            state.shards[1].state,
            PersistedShardState::Submitting
        ));
        assert_eq!(
            claim_batch_shard_state(&mut state, 2, 1).unwrap(),
            BatchClaim::CapacityFull
        );
    }

    #[test]
    fn child_reconciliation_recovers_publishing_and_rejects_mismatches() {
        let publishing = test_child_job(batch::JobState::Publishing);
        let publishing_reconciliation =
            reconciled_child_state(&publishing).map(|(state, error)| (state, error.is_none()));
        assert_eq!(
            publishing_reconciliation,
            Some((PersistedShardState::Submitted, true))
        );

        let common = test_common();
        let plan = ExecutionPlan {
            manifest: Manifest {
                path: common.manifest.clone(),
                assets: vec![ManifestAsset {
                    id: "asset-1".to_owned(),
                    prompt: "asset".to_owned(),
                    stem: "asset-1".to_owned(),
                }],
                source_sha256: "0".repeat(64),
                bytes: 1,
            },
            common,
            output_dir: PathBuf::from("/tmp/images"),
            output_names: vec!["asset-1.png".to_owned()],
            digest: "1".repeat(64),
            operation: "run.batch",
        };
        let shard = BatchShardPlan {
            index: 0,
            start: 0,
            end: 1,
            job_file: PathBuf::from("/tmp/batch-run.shard-00000.json"),
        };
        let mut mismatched = test_child_job(batch::JobState::Submitted);
        mismatched.output_names[0] = "other.png".to_owned();
        assert!(validate_reconciled_child_job(&plan, &shard, &mismatched).is_err());

        let mut matching = test_child_job(batch::JobState::Submitted);
        let generation = generation_args(&plan, &plan.manifest.assets[0]);
        let (input_sha256, input_bytes) =
            batch::input_fingerprint(&generation, &plan.manifest.assets, &matching.custom_ids)
                .unwrap();
        matching.input_sha256 = input_sha256;
        matching.input_bytes = input_bytes;
        assert!(validate_reconciled_child_job(&plan, &shard, &matching).is_ok());
        let mut prompt_mismatch = plan.clone();
        prompt_mismatch.manifest.assets[0].prompt = "different asset".to_owned();
        assert!(validate_reconciled_child_job(&prompt_mismatch, &shard, &matching).is_err());
    }
}
