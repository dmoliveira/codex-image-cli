use std::{
    collections::{BTreeMap, HashMap},
    env,
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use url::Url;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use std::fs::{self, OpenOptions};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use rustix::{
    fs::{fchmod, mkdirat, openat, Mode, OFlags, CWD},
    io::Errno,
};

use crate::{
    api::TokenUsage,
    cli::{CostArgs, CostPeriod},
    report::AppError,
};

pub const COST_REPORT_SCHEMA_VERSION: u8 = 1;
const LEDGER_SCHEMA_VERSION: u8 = 1;
const MAX_LEDGER_BYTES: usize = 16 * 1024 * 1024;
const MAX_LEDGER_LINE_BYTES: usize = 256 * 1024;
const PRICING_VERSION: &str = "openai-gpt-image-2-2026-08";
const PRICING_SOURCE: &str = "https://developers.openai.com/api/docs/pricing#image-generation";
const MAX_USAGE_TOKENS: u64 = 1_000_000_000_000;
static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostTransport {
    Live,
    Batch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CostEventKind {
    Started,
    Observed,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostOutcome {
    Pending,
    Accepted,
    Observed,
    Succeeded,
    Failed,
    Rejected,
    Unknown,
    Unpriced,
}

#[derive(Debug, Clone)]
pub struct CostOperationSpec {
    pub operation_id: String,
    pub transport: CostTransport,
    pub model: String,
    pub image_count: u32,
    pub quality: String,
    pub size: String,
    pub output_format: String,
    pub pricing_eligible: bool,
    pub batch_id: Option<String>,
    pub custom_id: Option<String>,
}

pub(crate) struct CostResolution {
    pub operation_id: String,
    pub kind: CostEventKind,
    pub outcome: CostOutcome,
    pub recorded_at: u64,
    pub batch_id: Option<String>,
    pub request_id: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CostEvent {
    schema_version: u8,
    event_id: String,
    operation_id: String,
    kind: CostEventKind,
    recorded_at: u64,
    started_at: u64,
    transport: CostTransport,
    model: String,
    image_count: u32,
    quality: String,
    size: String,
    output_format: String,
    pricing_version: String,
    batch_id: Option<String>,
    custom_id: Option<String>,
    request_id: Option<String>,
    outcome: CostOutcome,
    usage: Option<TokenUsage>,
    estimated_nano_usd: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct CostLedger {
    path: PathBuf,
}

impl CostLedger {
    pub fn default_path() -> Result<PathBuf, AppError> {
        let root = env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("XDG_CONFIG_HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".local/state"))
            })
            .ok_or_else(|| {
                AppError::preflight(
                    "cost_ledger_path_unavailable",
                    "A local state directory could not be determined for API cost tracking.",
                )
            })?;
        Ok(root.join("codex-image").join("costs.jsonl"))
    }

    pub fn open(path: Option<&Path>) -> Result<Self, AppError> {
        let path = match path {
            Some(path) => absolute_path(path)?,
            None => absolute_path(&Self::default_path()?)?,
        };
        validate_path(&path)?;
        let directory = LedgerDirectory::open(&path)?;
        let _lock = directory.open_lock(false)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn start(&self, spec: CostOperationSpec, started_at: u64) -> Result<(), AppError> {
        self.start_many(std::slice::from_ref(&spec), started_at)
    }

    pub fn start_many(&self, specs: &[CostOperationSpec], started_at: u64) -> Result<(), AppError> {
        if specs.is_empty() {
            return Ok(());
        }
        let directory = LedgerDirectory::open(&self.path)?;
        let _lock = directory.open_lock(true)?;
        let existing = directory.read_events()?;
        fold_events(&existing)?;
        let mut additions = Vec::new();
        for spec in specs {
            validate_spec(spec)?;
            if let Some(event) = existing.iter().chain(&additions).find(|event| {
                event.operation_id == spec.operation_id && event.kind == CostEventKind::Started
            }) {
                if !same_spec(event, spec) {
                    return Err(ledger_conflict(
                        &spec.operation_id,
                        "A cost operation ID was reused with different request metadata.",
                    ));
                }
                continue;
            }
            additions.push(start_event(spec, started_at));
        }
        directory.append_events(&additions)
    }

    pub(crate) fn resolve(&self, resolution: CostResolution) -> Result<(), AppError> {
        let CostResolution {
            operation_id,
            kind,
            outcome,
            recorded_at,
            batch_id,
            request_id,
            usage,
        } = resolution;
        let directory = LedgerDirectory::open(&self.path)?;
        let _lock = directory.open_lock(true)?;
        let existing = directory.read_events()?;
        fold_events(&existing)?;
        let Some(base) = existing.iter().find(|event| {
            event.operation_id == operation_id && event.kind == CostEventKind::Started
        }) else {
            return Err(ledger_conflict(
                &operation_id,
                "A cost resolution was recorded before its durable start event.",
            ));
        };
        let invalid_usage = usage.as_ref().is_some_and(|value| !valid_usage(value));
        let usage = if invalid_usage { None } else { usage };
        let outcome = if invalid_usage {
            CostOutcome::Unpriced
        } else {
            outcome
        };
        let final_event = existing
            .iter()
            .find(|event| event.operation_id == operation_id && event.kind == CostEventKind::Final);
        if let Some(final_event) = final_event {
            if kind == CostEventKind::Observed {
                return Ok(());
            }
            if outcome == CostOutcome::Unknown && final_event.outcome != CostOutcome::Unknown {
                return Ok(());
            }
            if same_resolution(final_event, outcome, batch_id.as_deref(), usage.as_ref()) {
                return Ok(());
            }
            return Err(ledger_conflict(
                &operation_id,
                "Conflicting final cost resolutions were recorded for one operation.",
            ));
        }
        if kind == CostEventKind::Observed {
            if let Some(previous) = existing.iter().rev().find(|event| {
                event.operation_id == operation_id && event.kind == CostEventKind::Observed
            }) {
                if same_resolution(previous, outcome, batch_id.as_deref(), usage.as_ref()) {
                    return Ok(());
                }
            }
        }
        let pricing_eligible = base.pricing_version == PRICING_VERSION;
        let estimated_nano_usd = if kind == CostEventKind::Final {
            estimate_nano_usd(usage.as_ref(), base.transport, pricing_eligible)
        } else {
            None
        };
        let event = CostEvent {
            schema_version: LEDGER_SCHEMA_VERSION,
            event_id: next_event_id(&operation_id),
            operation_id: operation_id.to_owned(),
            kind,
            recorded_at,
            started_at: base.started_at,
            transport: base.transport,
            model: base.model.clone(),
            image_count: base.image_count,
            quality: base.quality.clone(),
            size: base.size.clone(),
            output_format: base.output_format.clone(),
            pricing_version: base.pricing_version.clone(),
            batch_id: batch_id.or_else(|| base.batch_id.clone()),
            custom_id: base.custom_id.clone(),
            request_id,
            outcome,
            usage,
            estimated_nano_usd,
        };
        directory.append_events(std::slice::from_ref(&event))
    }

    fn read(&self) -> Result<Vec<CostEvent>, AppError> {
        let directory = LedgerDirectory::open(&self.path)?;
        let _lock = directory.open_lock(false)?;
        directory.read_events()
    }
}

fn start_event(spec: &CostOperationSpec, started_at: u64) -> CostEvent {
    CostEvent {
        schema_version: LEDGER_SCHEMA_VERSION,
        event_id: format!("{}:start", spec.operation_id),
        operation_id: spec.operation_id.clone(),
        kind: CostEventKind::Started,
        recorded_at: started_at,
        started_at,
        transport: spec.transport,
        model: spec.model.clone(),
        image_count: spec.image_count,
        quality: spec.quality.clone(),
        size: spec.size.clone(),
        output_format: spec.output_format.clone(),
        pricing_version: if spec.pricing_eligible {
            PRICING_VERSION.to_owned()
        } else {
            "custom_endpoint_unpriced".to_owned()
        },
        batch_id: spec.batch_id.clone(),
        custom_id: spec.custom_id.clone(),
        request_id: None,
        outcome: CostOutcome::Pending,
        usage: None,
        estimated_nano_usd: None,
    }
}

fn same_spec(event: &CostEvent, spec: &CostOperationSpec) -> bool {
    event.transport == spec.transport
        && event.model == spec.model
        && event.image_count == spec.image_count
        && event.quality == spec.quality
        && event.size == spec.size
        && event.output_format == spec.output_format
        && event.custom_id == spec.custom_id
        && event.pricing_version
            == if spec.pricing_eligible {
                PRICING_VERSION
            } else {
                "custom_endpoint_unpriced"
            }
}

fn same_resolution(
    event: &CostEvent,
    outcome: CostOutcome,
    batch_id: Option<&str>,
    usage: Option<&TokenUsage>,
) -> bool {
    event.outcome == outcome
        && (event.batch_id.is_none() || batch_id.is_none() || event.batch_id.as_deref() == batch_id)
        && event.usage.as_ref() == usage
}

fn next_event_id(operation_id: &str) -> String {
    let counter = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{operation_id}:event-{timestamp:x}-{:x}-{counter:x}",
        std::process::id()
    )
}

fn validate_spec(spec: &CostOperationSpec) -> Result<(), AppError> {
    if spec.operation_id.is_empty()
        || spec.operation_id.len() > 256
        || spec.model.is_empty()
        || spec.model.len() > 128
        || spec.image_count == 0
        || spec.image_count > 8
        || spec.quality.len() > 32
        || spec.size.len() > 64
        || spec.output_format.len() > 16
        || spec
            .custom_id
            .as_ref()
            .is_some_and(|value| value.len() > 256)
    {
        return Err(AppError::preflight(
            "cost_operation_invalid",
            "The cost ledger received unsafe or inconsistent operation metadata.",
        ));
    }
    Ok(())
}

fn ledger_conflict(operation_id: &str, message: &'static str) -> AppError {
    AppError::preflight(
        "cost_ledger_conflict",
        format!("{message} Operation: {operation_id}."),
    )
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|_| {
                AppError::preflight(
                    "cost_ledger_path_unavailable",
                    "The current directory could not be resolved for the cost ledger.",
                )
            })
    }
}

fn validate_path(path: &Path) -> Result<(), AppError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(AppError::preflight(
            "cost_ledger_path_invalid",
            "The cost ledger path must not contain '..' components.",
        ));
    }
    path.parent().ok_or_else(|| {
        AppError::preflight(
            "cost_ledger_path_invalid",
            "The cost ledger path has no parent directory.",
        )
    })?;
    if path.file_name().is_none() {
        Err(AppError::preflight(
            "cost_ledger_path_invalid",
            "The cost ledger path must name a regular file.",
        ))
    } else {
        Ok(())
    }
}

struct LedgerDirectory {
    path: PathBuf,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    directory: File,
}

impl LedgerDirectory {
    fn open(path: &Path) -> Result<Self, AppError> {
        validate_path(path)?;
        let parent = path.parent().ok_or_else(|| {
            AppError::preflight(
                "cost_ledger_path_invalid",
                "The cost ledger path has no parent directory.",
            )
        })?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let directory = secure_open_directory(parent).map_err(|error| {
            secure_open_error(
                error,
                "cost_ledger_path_unavailable",
                "The cost ledger directory could not be opened without following a symlink.",
            )
        })?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            fs::create_dir_all(parent).map_err(|_| {
                AppError::preflight(
                    "cost_ledger_path_unavailable",
                    "The cost ledger directory could not be created safely.",
                )
            })?;
            validate_no_symlink_components(parent)?;
            reject_symlink(path)?;
            reject_symlink(&lock_path(path))?;
        }
        Ok(Self {
            path: path.to_owned(),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            directory,
        })
    }

    fn ledger_name(&self) -> &std::ffi::OsStr {
        self.path.file_name().expect("validated ledger filename")
    }

    fn lock_name(&self) -> OsString {
        let mut name = self.ledger_name().to_os_string();
        name.push(".lock");
        name
    }

    fn open_lock(&self, exclusive: bool) -> Result<File, AppError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let lock_name = self.lock_name();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let lock = match secure_open_child(
            &self.directory,
            &lock_name,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(lock) => Ok(lock),
            Err(Errno::EXIST) => secure_open_child(
                &self.directory,
                &lock_name,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ),
            Err(error) => Err(error),
        }
        .map_err(|error| {
            secure_open_error(
                error,
                "cost_ledger_unavailable",
                "The cost ledger lock could not be opened safely.",
            )
        })?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path(&self.path))
            .map_err(|_| {
                AppError::preflight(
                    "cost_ledger_unavailable",
                    "The cost ledger lock could not be opened safely.",
                )
            })?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        fchmod(&lock, Mode::RUSR | Mode::WUSR).map_err(|_| {
            AppError::preflight(
                "cost_ledger_unavailable",
                "The cost ledger lock permissions could not be restricted.",
            )
        })?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(lock_path(&self.path), fs::Permissions::from_mode(0o600))
                    .map_err(|_| {
                        AppError::preflight(
                            "cost_ledger_unavailable",
                            "The cost ledger lock permissions could not be restricted.",
                        )
                    })?;
            }
        }
        if !lock
            .metadata()
            .map_err(|_| {
                AppError::preflight(
                    "cost_ledger_unavailable",
                    "The cost ledger lock could not be inspected safely.",
                )
            })?
            .file_type()
            .is_file()
        {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "The cost ledger lock is not a regular file.",
            ));
        }
        if exclusive {
            lock.lock_exclusive().map_err(|_| {
                AppError::preflight(
                    "cost_ledger_unavailable",
                    "The cost ledger could not acquire its exclusive lock.",
                )
            })?;
        } else {
            FileExt::lock_shared(&lock).map_err(|_| {
                AppError::preflight(
                    "cost_ledger_unavailable",
                    "The cost ledger could not acquire its read lock.",
                )
            })?;
        }
        Ok(lock)
    }

    fn read_events(&self) -> Result<Vec<CostEvent>, AppError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let mut file = match secure_open_child(
            &self.directory,
            self.ledger_name(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => file,
            Err(Errno::NOENT) => return Ok(Vec::new()),
            Err(error) => {
                return Err(secure_open_error(
                    error,
                    "cost_ledger_unreadable",
                    "The cost ledger could not be read without following a symlink.",
                ))
            }
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => {
                return Err(AppError::preflight(
                    "cost_ledger_unreadable",
                    "The cost ledger could not be read safely.",
                ))
            }
        };
        let metadata = file.metadata().map_err(|_| {
            AppError::preflight(
                "cost_ledger_unreadable",
                "The cost ledger could not be inspected safely.",
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "The cost ledger is not a regular file.",
            ));
        }
        let length = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if length > MAX_LEDGER_BYTES {
            return Err(AppError::preflight(
                "cost_ledger_too_large",
                "The cost ledger exceeded the local safety limit.",
            ));
        }
        let mut bytes = Vec::with_capacity(length);
        file.read_to_end(&mut bytes).map_err(|_| {
            AppError::preflight(
                "cost_ledger_unreadable",
                "The cost ledger could not be read safely.",
            )
        })?;
        parse_events(&bytes)
    }

    fn append_events(&self, events: &[CostEvent]) -> Result<(), AppError> {
        if events.is_empty() {
            return Ok(());
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let mut file = secure_open_child(
            &self.directory,
            self.ledger_name(),
            OFlags::CREATE | OFlags::APPEND | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| {
            secure_open_error(
                error,
                "cost_ledger_unavailable",
                "The cost ledger could not be opened for append without following a symlink.",
            )
        })?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let mut file = {
            reject_symlink(&self.path)?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.path)
                .map_err(|_| {
                    AppError::preflight(
                        "cost_ledger_unavailable",
                        "The cost ledger could not be opened for append.",
                    )
                })?
        };
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|_| {
            AppError::preflight(
                "cost_ledger_unavailable",
                "The cost ledger permissions could not be restricted.",
            )
        })?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600)).map_err(
                    |_| {
                        AppError::preflight(
                            "cost_ledger_unavailable",
                            "The cost ledger permissions could not be restricted.",
                        )
                    },
                )?;
            }
        }
        let metadata = file.metadata().map_err(|_| {
            AppError::preflight(
                "cost_ledger_unavailable",
                "The cost ledger could not be inspected before append.",
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "The cost ledger is not a regular file.",
            ));
        }
        let existing_length = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let mut appended_length = 0_usize;
        for event in events {
            validate_event(event)?;
            let mut line = serde_json::to_vec(event).map_err(|_| {
                AppError::preflight(
                    "cost_ledger_unavailable",
                    "A cost ledger record could not be serialized.",
                )
            })?;
            line.push(b'\n');
            if line.len() > MAX_LEDGER_LINE_BYTES {
                return Err(AppError::preflight(
                    "cost_ledger_line_too_large",
                    "A cost ledger record exceeded the local safety limit.",
                ));
            }
            appended_length = appended_length.saturating_add(line.len());
            if existing_length.saturating_add(appended_length) > MAX_LEDGER_BYTES {
                return Err(AppError::preflight(
                    "cost_ledger_too_large",
                    "The cost ledger would exceed the local safety limit.",
                ));
            }
            file.write_all(&line).map_err(|_| {
                AppError::preflight(
                    "cost_ledger_write_failed",
                    "The cost ledger record could not be appended safely.",
                )
            })?;
        }
        file.sync_all().map_err(|_| {
            AppError::preflight(
                "cost_ledger_sync_failed",
                "The cost ledger could not be synchronized safely.",
            )
        })?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        self.directory.sync_all().map_err(|_| {
            AppError::preflight(
                "cost_ledger_sync_failed",
                "The cost ledger directory could not be synchronized safely.",
            )
        })?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        File::open(self.path.parent().expect("validated ledger parent"))
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                AppError::preflight(
                    "cost_ledger_sync_failed",
                    "The cost ledger directory could not be synchronized safely.",
                )
            })?;
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn secure_open_directory(path: &Path) -> Result<File, Errno> {
    let root_fd = openat(
        CWD,
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut current: File = root_fd.into();
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir | Component::CurDir) {
                continue;
            }
            return Err(Errno::INVAL);
        };
        let next = match openat(
            &current,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                mkdirat(&current, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
                openat(
                    &current,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?
            }
            Err(error) => return Err(error),
        };
        current = next.into();
    }
    Ok(current)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn secure_open_child(
    directory: &File,
    name: &std::ffi::OsStr,
    flags: OFlags,
    mode: Mode,
) -> Result<File, Errno> {
    Ok(openat(directory, name, flags, mode)?.into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn secure_open_error(error: Errno, default_code: &'static str, message: &'static str) -> AppError {
    let code = if error == Errno::LOOP {
        "cost_ledger_path_symlink"
    } else {
        default_code
    };
    AppError::preflight(code, message)
}

fn parse_events(bytes: &[u8]) -> Result<Vec<CostEvent>, AppError> {
    let mut events = Vec::new();
    let mut event_ids = HashMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_LEDGER_LINE_BYTES {
            return Err(AppError::preflight(
                "cost_ledger_line_too_large",
                "A cost ledger record exceeded the local safety limit.",
            ));
        }
        let event: CostEvent = serde_json::from_slice(line).map_err(|_| {
            AppError::preflight(
                "cost_ledger_invalid",
                "The cost ledger contains malformed or unsupported JSON; no totals were inferred.",
            )
        })?;
        validate_event(&event)?;
        if let Some(previous) = event_ids.insert(event.event_id.clone(), event.clone()) {
            if previous != event {
                return Err(AppError::preflight(
                    "cost_ledger_invalid",
                    "The cost ledger contains conflicting records with one event ID.",
                ));
            }
            continue;
        }
        events.push(event);
    }
    Ok(events)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn validate_no_symlink_components(path: &Path) -> Result<(), AppError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err(AppError::preflight(
                    "cost_ledger_path_symlink",
                    "Cost ledger paths cannot contain symlinked components.",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn reject_symlink(path: &Path) -> Result<(), AppError> {
    if fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(AppError::preflight(
            "cost_ledger_path_symlink",
            "The cost ledger or its lock file is a symlink; refusing to use it.",
        ));
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn lock_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

fn validate_event(event: &CostEvent) -> Result<(), AppError> {
    if event.schema_version != LEDGER_SCHEMA_VERSION
        || event.operation_id.is_empty()
        || event.operation_id.len() > 256
        || event.event_id.is_empty()
        || event.event_id.len() > 512
        || event.model.is_empty()
        || event.image_count == 0
        || event.image_count > 8
        || event.started_at == 0
        || event.recorded_at == 0
    {
        return Err(AppError::preflight(
            "cost_ledger_invalid",
            "The cost ledger contains an invalid record.",
        ));
    }
    if event.pricing_version != PRICING_VERSION
        && event.pricing_version != "custom_endpoint_unpriced"
    {
        return Err(AppError::preflight(
            "cost_ledger_invalid",
            "The cost ledger contains an unsupported pricing snapshot.",
        ));
    }
    match event.kind {
        CostEventKind::Started
            if event.outcome != CostOutcome::Pending
                || event.usage.is_some()
                || event.estimated_nano_usd.is_some() =>
        {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "A cost start event contains resolution-only fields.",
            ));
        }
        CostEventKind::Observed
            if event.estimated_nano_usd.is_some()
                || matches!(
                    event.outcome,
                    CostOutcome::Succeeded
                        | CostOutcome::Failed
                        | CostOutcome::Rejected
                        | CostOutcome::Unpriced
                ) =>
        {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "A cost observation event contains final-only fields.",
            ));
        }
        CostEventKind::Final
            if matches!(
                event.outcome,
                CostOutcome::Pending | CostOutcome::Accepted | CostOutcome::Observed
            ) =>
        {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "A cost final event contains a non-final outcome.",
            ));
        }
        _ => {}
    }
    if let Some(usage) = &event.usage {
        if !valid_usage(usage) {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "The cost ledger contains invalid token usage metadata.",
            ));
        }
        if event.kind == CostEventKind::Final
            && event.pricing_version == PRICING_VERSION
            && event.estimated_nano_usd != estimate_nano_usd(Some(usage), event.transport, true)
        {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "The cost ledger estimate does not match its recorded usage and pricing snapshot.",
            ));
        }
    }
    if event.kind == CostEventKind::Final {
        let expected = event.usage.as_ref().and_then(|usage| {
            estimate_nano_usd(
                Some(usage),
                event.transport,
                event.pricing_version == PRICING_VERSION,
            )
        });
        if event.estimated_nano_usd != expected {
            return Err(AppError::preflight(
                "cost_ledger_invalid",
                "The cost ledger final estimate does not match its pricing snapshot.",
            ));
        }
    }
    if event.pricing_version == "custom_endpoint_unpriced" && event.estimated_nano_usd.is_some() {
        return Err(AppError::preflight(
            "cost_ledger_invalid",
            "A custom endpoint cost record cannot contain an OpenAI price estimate.",
        ));
    }
    Ok(())
}

fn valid_usage(usage: &TokenUsage) -> bool {
    let values = [usage.input_tokens, usage.output_tokens, usage.total_tokens];
    if values
        .iter()
        .flatten()
        .any(|value| *value > MAX_USAGE_TOKENS)
    {
        return false;
    }
    if let Some(total) = usage.total_tokens {
        if let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) {
            if input.saturating_add(output) != total {
                return false;
            }
        }
    }
    for details in [&usage.input_tokens_details, &usage.output_tokens_details]
        .into_iter()
        .flatten()
    {
        if details
            .text_tokens
            .into_iter()
            .chain(details.image_tokens)
            .any(|value| value > MAX_USAGE_TOKENS)
        {
            return false;
        }
    }
    true
}

fn estimate_nano_usd(
    usage: Option<&TokenUsage>,
    transport: CostTransport,
    pricing_eligible: bool,
) -> Option<u64> {
    let usage = usage?;
    if !pricing_eligible || !valid_usage(usage) {
        return None;
    }
    let input = usage.input_tokens?;
    let output = usage.output_tokens?;
    let (text_input, image_input) = split_input_tokens(input, usage.input_tokens_details.as_ref())?;
    let image_output = split_output_tokens(output, usage.output_tokens_details.as_ref())?;
    let (text_rate, image_input_rate, image_output_rate) = match transport {
        CostTransport::Live => (5_000_u64, 8_000_u64, 30_000_u64),
        CostTransport::Batch => (2_500_u64, 4_000_u64, 15_000_u64),
    };
    let total = u128::from(text_input)
        .checked_mul(u128::from(text_rate))?
        .checked_add(u128::from(image_input).checked_mul(u128::from(image_input_rate))?)?
        .checked_add(u128::from(image_output).checked_mul(u128::from(image_output_rate))?)?;
    u64::try_from(total).ok()
}

fn split_input_tokens(
    total: u64,
    details: Option<&crate::api::TokenUsageDetails>,
) -> Option<(u64, u64)> {
    let Some(details) = details else {
        return Some((total, 0));
    };
    match (details.text_tokens, details.image_tokens) {
        (None, None) => Some((total, 0)),
        (Some(text), Some(image)) if text.checked_add(image) == Some(total) => Some((text, image)),
        (Some(text), None) if text <= total => Some((text, total - text)),
        (None, Some(image)) if image <= total => Some((total - image, image)),
        _ => None,
    }
}

fn split_output_tokens(total: u64, details: Option<&crate::api::TokenUsageDetails>) -> Option<u64> {
    let Some(details) = details else {
        return Some(total);
    };
    if details.text_tokens.unwrap_or(0) > 0 {
        return None;
    }
    match details.image_tokens {
        Some(image) if image == total => Some(image),
        Some(_) => None,
        None => Some(total),
    }
}

#[derive(Debug, Clone)]
struct CostOperation {
    operation_id: String,
    started_at: u64,
    transport: CostTransport,
    model: String,
    image_count: u32,
    quality: String,
    size: String,
    output_format: String,
    pricing_version: String,
    batch_id: Option<String>,
    custom_id: Option<String>,
    request_id: Option<String>,
    outcome: CostOutcome,
    usage: Option<TokenUsage>,
    estimated_nano_usd: Option<u64>,
    finalized: bool,
    started: bool,
}

fn fold_events(events: &[CostEvent]) -> Result<Vec<CostOperation>, AppError> {
    let mut operations: Vec<CostOperation> = Vec::new();
    let mut indexes = HashMap::new();
    for event in events {
        let index = if let Some(index) = indexes.get(&event.operation_id) {
            *index
        } else {
            if event.kind != CostEventKind::Started {
                return Err(ledger_conflict(
                    &event.operation_id,
                    "A cost operation must begin with exactly one start event.",
                ));
            }
            let index = operations.len();
            indexes.insert(event.operation_id.clone(), index);
            operations.push(CostOperation {
                operation_id: event.operation_id.clone(),
                started_at: event.started_at,
                transport: event.transport,
                model: event.model.clone(),
                image_count: event.image_count,
                quality: event.quality.clone(),
                size: event.size.clone(),
                output_format: event.output_format.clone(),
                pricing_version: event.pricing_version.clone(),
                batch_id: event.batch_id.clone(),
                custom_id: event.custom_id.clone(),
                request_id: event.request_id.clone(),
                outcome: event.outcome,
                usage: event.usage.clone(),
                estimated_nano_usd: event.estimated_nano_usd,
                finalized: event.kind == CostEventKind::Final,
                started: false,
            });
            index
        };
        let operation = &mut operations[index];
        if operation.transport != event.transport
            || operation.model != event.model
            || operation.image_count != event.image_count
            || operation.quality != event.quality
            || operation.size != event.size
            || operation.output_format != event.output_format
            || operation.pricing_version != event.pricing_version
            || operation.custom_id != event.custom_id
        {
            return Err(ledger_conflict(
                &event.operation_id,
                "Cost ledger events disagree about request metadata.",
            ));
        }
        if event.kind == CostEventKind::Started && operation.started_at != event.started_at {
            return Err(ledger_conflict(
                &event.operation_id,
                "Cost ledger start events disagree about the request start time.",
            ));
        }
        if event.kind == CostEventKind::Started && operation.started {
            return Err(ledger_conflict(
                &event.operation_id,
                "Cost ledger contains more than one start event for an operation.",
            ));
        }
        if event.kind == CostEventKind::Started {
            operation.started = true;
        }
        if event.kind == CostEventKind::Final {
            if operation.finalized {
                if operation.outcome != event.outcome
                    || operation.usage != event.usage
                    || operation.estimated_nano_usd != event.estimated_nano_usd
                {
                    return Err(ledger_conflict(
                        &event.operation_id,
                        "Cost ledger contains conflicting final events.",
                    ));
                }
            } else {
                operation.finalized = true;
                operation.outcome = event.outcome;
                operation.usage = event.usage.clone();
                operation.estimated_nano_usd = event.estimated_nano_usd;
                operation.batch_id = event
                    .batch_id
                    .clone()
                    .or_else(|| operation.batch_id.clone());
                operation.request_id = event.request_id.clone();
            }
        } else if operation.finalized && event.kind == CostEventKind::Observed {
            return Err(ledger_conflict(
                &event.operation_id,
                "A cost observation appeared after a final event.",
            ));
        } else if !operation.finalized {
            operation.outcome = event.outcome;
            operation.usage = event.usage.clone();
            operation.batch_id = event
                .batch_id
                .clone()
                .or_else(|| operation.batch_id.clone());
            operation.request_id = event
                .request_id
                .clone()
                .or_else(|| operation.request_id.clone());
        }
    }
    operations.sort_by(|left, right| {
        left.started_at
            .cmp(&right.started_at)
            .then_with(|| left.operation_id.cmp(&right.operation_id))
    });
    Ok(operations)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CivilDate {
    year: i32,
    month: u8,
    day: u8,
}

impl CivilDate {
    fn add_days(self, days: i64) -> Self {
        civil_from_days(days_from_civil(self) + days)
    }

    fn epoch_seconds(self) -> i64 {
        days_from_civil(self) * 86_400
    }

    fn format(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn days_from_civil(date: CivilDate) -> i64 {
    let mut year = i64::from(date.year);
    let month = i64::from(date.month);
    let day = i64::from(date.day);
    year -= i64::from(month <= 2);
    let era = if year >= 0 {
        year / 400
    } else {
        (year - 399) / 400
    };
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> CivilDate {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    CivilDate {
        year: i32::try_from(year + i64::from(month <= 2)).unwrap_or(0),
        month: u8::try_from(month).unwrap_or(1),
        day: u8::try_from(day).unwrap_or(1),
    }
}

fn date_from_timestamp(timestamp: u64) -> CivilDate {
    civil_from_days(i64::try_from(timestamp / 86_400).unwrap_or(i64::MAX / 86_400))
}

fn parse_date(value: &str) -> Result<CivilDate, AppError> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(AppError::usage(
            "cost_date_invalid",
            "Cost dates must use the inclusive UTC YYYY-MM-DD format.",
        ));
    }
    let parse = |slice: &[u8]| -> Option<i64> {
        if slice.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(slice).ok()?.parse().ok()
        } else {
            None
        }
    };
    let date = CivilDate {
        year: i32::try_from(parse(&bytes[..4]).ok_or_else(invalid_date)?)
            .map_err(|_| invalid_date())?,
        month: u8::try_from(parse(&bytes[5..7]).ok_or_else(invalid_date)?)
            .map_err(|_| invalid_date())?,
        day: u8::try_from(parse(&bytes[8..]).ok_or_else(invalid_date)?)
            .map_err(|_| invalid_date())?,
    };
    if date.month == 0
        || date.month > 12
        || date.day == 0
        || civil_from_days(days_from_civil(date)) != date
    {
        return Err(invalid_date());
    }
    Ok(date)
}

fn invalid_date() -> AppError {
    AppError::usage(
        "cost_date_invalid",
        "Cost dates must be valid inclusive UTC YYYY-MM-DD values.",
    )
}

fn period_bounds(period: CostPeriod, now: u64) -> (CivilDate, CivilDate) {
    let today = date_from_timestamp(now);
    match period {
        CostPeriod::Today => (today, today.add_days(1)),
        CostPeriod::Week => {
            let weekday = (days_from_civil(today) + 3).rem_euclid(7);
            let start = today.add_days(-weekday);
            (start, start.add_days(7))
        }
        CostPeriod::Month => {
            let start = CivilDate {
                year: today.year,
                month: today.month,
                day: 1,
            };
            let end = if today.month == 12 {
                CivilDate {
                    year: today.year + 1,
                    month: 1,
                    day: 1,
                }
            } else {
                CivilDate {
                    year: today.year,
                    month: today.month + 1,
                    day: 1,
                }
            };
            (start, end)
        }
        CostPeriod::Year => {
            let start = CivilDate {
                year: today.year,
                month: 1,
                day: 1,
            };
            (
                start,
                CivilDate {
                    year: today.year + 1,
                    month: 1,
                    day: 1,
                },
            )
        }
        CostPeriod::All => (
            CivilDate {
                year: 1970,
                month: 1,
                day: 1,
            },
            today.add_days(1),
        ),
    }
}

fn bounds_for_args(
    args: &CostArgs,
    now: u64,
    operations: &[CostOperation],
) -> Result<(CivilDate, CivilDate, String), AppError> {
    let (period_start, period_end) = period_bounds(args.period, now);
    let start = args
        .from
        .as_deref()
        .map(parse_date)
        .transpose()?
        .unwrap_or_else(|| {
            if matches!(args.period, CostPeriod::All) {
                operations
                    .iter()
                    .map(|operation| date_from_timestamp(operation.started_at))
                    .min()
                    .unwrap_or(period_start)
            } else {
                period_start
            }
        });
    let end_inclusive = args
        .to
        .as_deref()
        .map(parse_date)
        .transpose()?
        .unwrap_or_else(|| period_end.add_days(-1));
    if start > end_inclusive {
        return Err(AppError::usage(
            "cost_date_range_invalid",
            "The cost report start date must not be after its inclusive end date.",
        ));
    }
    let label = if args.from.is_some() || args.to.is_some() {
        "range".to_owned()
    } else {
        match args.period {
            CostPeriod::Today => "today",
            CostPeriod::Week => "week",
            CostPeriod::Month => "month",
            CostPeriod::Year => "year",
            CostPeriod::All => "all",
        }
        .to_owned()
    };
    Ok((start, end_inclusive.add_days(1), label))
}

#[derive(Debug, Serialize)]
pub struct CostReport {
    pub schema_version: u8,
    pub ok: bool,
    pub status: &'static str,
    pub currency: &'static str,
    pub timezone: &'static str,
    pub ledger_file: String,
    pub pricing_version: &'static str,
    pub pricing_source: &'static str,
    pub period: CostPeriodReport,
    pub totals: CostTotals,
    pub by_transport: Vec<CostTransportSummary>,
    pub days: Vec<CostDaySummary>,
    pub requests: Vec<CostRequestSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CostPeriodReport {
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct CostTotals {
    pub requests: u64,
    pub images: u64,
    pub priced_requests: u64,
    pub unpriced_requests: u64,
    pub pending_requests: u64,
    pub unknown_requests: u64,
    pub rejected_requests: u64,
    pub estimated_nano_usd: u64,
    pub estimated_usd: String,
}

#[derive(Debug, Serialize)]
pub struct CostTransportSummary {
    pub transport: CostTransport,
    pub totals: CostTotals,
}

#[derive(Debug, Serialize)]
pub struct CostDaySummary {
    pub day: String,
    pub totals: CostTotals,
}

#[derive(Debug, Serialize)]
pub struct CostRequestSummary {
    pub operation_id: String,
    pub started_at: u64,
    pub transport: CostTransport,
    pub model: String,
    pub image_count: u32,
    pub quality: String,
    pub size: String,
    pub output_format: String,
    pub batch_id: Option<String>,
    pub custom_id: Option<String>,
    pub request_id: Option<String>,
    pub outcome: CostOutcome,
    pub finalized: bool,
    pub usage: Option<TokenUsage>,
    pub estimated_nano_usd: Option<u64>,
    pub estimated_usd: Option<String>,
}

pub fn run_cost(args: &CostArgs) -> Result<CostReport, AppError> {
    let ledger = CostLedger::open(args.ledger_file.as_deref())?;
    let events = ledger.read()?;
    let operations = fold_events(&events)?;
    build_report(args, &operations, now_seconds(), ledger.path())
}

fn build_report(
    args: &CostArgs,
    operations: &[CostOperation],
    now: u64,
    ledger_path: &Path,
) -> Result<CostReport, AppError> {
    let (start, end, name) = bounds_for_args(args, now, operations)?;
    let start_epoch = start.epoch_seconds();
    let end_epoch = end.epoch_seconds();
    let selected = operations
        .iter()
        .filter(|operation| {
            i64::try_from(operation.started_at)
                .ok()
                .is_some_and(|timestamp| timestamp >= start_epoch && timestamp < end_epoch)
        })
        .cloned()
        .collect::<Vec<_>>();
    let totals = totals_for(&selected);
    let mut by_transport = Vec::new();
    for transport in [CostTransport::Live, CostTransport::Batch] {
        let group = selected
            .iter()
            .filter(|operation| operation.transport == transport)
            .cloned()
            .collect::<Vec<_>>();
        if !group.is_empty() {
            by_transport.push(CostTransportSummary {
                transport,
                totals: totals_for(&group),
            });
        }
    }
    let days = if args.day_by_day {
        day_rows(&selected, start, end)?
    } else {
        Vec::new()
    };
    let requests = if args.per_request {
        selected.iter().map(request_summary).collect()
    } else {
        Vec::new()
    };
    let mut warnings = vec![
        "Amounts are local estimates from the recorded token usage and the versioned OpenAI rate snapshot; billing records remain authoritative.".to_owned(),
    ];
    if totals.pending_requests > 0 || totals.unknown_requests > 0 {
        warnings.push(
            "Pending or unknown API outcomes may have been billed; no automatic charge was inferred.".to_owned(),
        );
    }
    if totals.unpriced_requests > 0 {
        warnings.push(
            "Some completed requests have no usable token usage or use a custom endpoint, so their cost is unpriced.".to_owned(),
        );
    }
    Ok(CostReport {
        schema_version: COST_REPORT_SCHEMA_VERSION,
        ok: true,
        status: "cost_report",
        currency: "USD",
        timezone: "UTC",
        ledger_file: ledger_path.to_string_lossy().into_owned(),
        pricing_version: PRICING_VERSION,
        pricing_source: PRICING_SOURCE,
        period: CostPeriodReport {
            name,
            from: start.format(),
            to: end.add_days(-1).format(),
        },
        totals,
        by_transport,
        days,
        requests,
        warnings,
    })
}

fn totals_for(operations: &[CostOperation]) -> CostTotals {
    let mut totals = CostTotals {
        requests: operations.len() as u64,
        images: operations
            .iter()
            .map(|operation| u64::from(operation.image_count))
            .sum(),
        ..CostTotals::default()
    };
    for operation in operations {
        if operation.finalized && operation.outcome == CostOutcome::Rejected {
            totals.rejected_requests += 1;
        }
        if operation.outcome == CostOutcome::Unknown {
            totals.unknown_requests += 1;
        }
        if !operation.finalized || operation.outcome == CostOutcome::Pending {
            totals.pending_requests += 1;
        }
        if operation.finalized {
            if let Some(cost) = operation.estimated_nano_usd {
                totals.priced_requests += 1;
                totals.estimated_nano_usd = totals.estimated_nano_usd.saturating_add(cost);
            } else if operation.outcome != CostOutcome::Rejected {
                totals.unpriced_requests += 1;
            }
        }
    }
    totals.estimated_usd = format_usd(totals.estimated_nano_usd);
    totals
}

fn request_summary(operation: &CostOperation) -> CostRequestSummary {
    CostRequestSummary {
        operation_id: operation.operation_id.clone(),
        started_at: operation.started_at,
        transport: operation.transport,
        model: operation.model.clone(),
        image_count: operation.image_count,
        quality: operation.quality.clone(),
        size: operation.size.clone(),
        output_format: operation.output_format.clone(),
        batch_id: operation.batch_id.clone(),
        custom_id: operation.custom_id.clone(),
        request_id: operation.request_id.clone(),
        outcome: operation.outcome,
        finalized: operation.finalized,
        usage: operation.usage.clone(),
        estimated_nano_usd: operation.estimated_nano_usd,
        estimated_usd: operation.estimated_nano_usd.map(format_usd),
    }
}

fn day_rows(
    operations: &[CostOperation],
    start: CivilDate,
    end: CivilDate,
) -> Result<Vec<CostDaySummary>, AppError> {
    let mut grouped: BTreeMap<CivilDate, Vec<CostOperation>> = BTreeMap::new();
    for operation in operations {
        grouped
            .entry(date_from_timestamp(operation.started_at))
            .or_default()
            .push(operation.clone());
    }
    let span = days_from_civil(end).saturating_sub(days_from_civil(start));
    if span > 3_700 {
        return Err(AppError::usage(
            "cost_day_range_too_large",
            "--day-by-day supports ranges up to 3,700 UTC calendar days; narrow --from/--to or omit the flag.",
        ));
    }
    if span >= 0 {
        let mut day = start;
        let mut rows = Vec::new();
        while day < end {
            let totals = totals_for(grouped.get(&day).map(Vec::as_slice).unwrap_or(&[]));
            rows.push(CostDaySummary {
                day: day.format(),
                totals,
            });
            day = day.add_days(1);
        }
        Ok(rows)
    } else {
        Ok(grouped
            .into_iter()
            .map(|(day, operations)| CostDaySummary {
                day: day.format(),
                totals: totals_for(&operations),
            })
            .collect())
    }
}

fn format_usd(nano_usd: u64) -> String {
    let micros = nano_usd.saturating_add(500) / 1_000;
    format!("${}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

pub fn batch_operation_id(job_id: &str, custom_id: &str) -> String {
    format!("batch/{job_id}/{custom_id}")
}

pub fn live_operation_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("live/{timestamp:x}/{:x}/{counter:x}", std::process::id())
}

pub fn pricing_version() -> &'static str {
    PRICING_VERSION
}

pub fn pricing_source() -> &'static str {
    PRICING_SOURCE
}

pub fn pricing_eligible_for_base_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("api.openai.com")
        && url.port_or_known_default() == Some(443)
        && matches!(url.path(), "/v1" | "/v1/")
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            total_tokens: Some(input + output),
            input_tokens_details: None,
            output_tokens_details: None,
        }
    }

    #[test]
    fn calculates_standard_and_batch_rates_without_float_rounding() {
        assert_eq!(
            estimate_nano_usd(Some(&usage(10, 20)), CostTransport::Live, true),
            Some(650_000)
        );
        assert_eq!(
            estimate_nano_usd(Some(&usage(10, 20)), CostTransport::Batch, true),
            Some(325_000)
        );
    }

    #[test]
    fn rejects_inconsistent_usage_details() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(20),
            total_tokens: Some(30),
            input_tokens_details: Some(crate::api::TokenUsageDetails {
                text_tokens: Some(8),
                image_tokens: Some(1),
            }),
            output_tokens_details: None,
        };
        assert_eq!(
            estimate_nano_usd(Some(&usage), CostTransport::Live, true),
            None
        );
    }

    #[test]
    fn handles_iso_dates_and_monday_weeks() {
        assert_eq!(parse_date("2026-02-28").unwrap().format(), "2026-02-28");
        assert!(parse_date("2026-02-29").is_err());
        let (start, end) = period_bounds(CostPeriod::Week, 1_772_841_600);
        assert_eq!(start.format(), "2026-03-02");
        assert_eq!(end.format(), "2026-03-09");
    }

    #[test]
    fn folds_repeated_identical_final_events_once() {
        let spec = CostOperationSpec {
            operation_id: "live/test".to_owned(),
            transport: CostTransport::Live,
            model: "gpt-image-2".to_owned(),
            image_count: 1,
            quality: "low".to_owned(),
            size: "auto".to_owned(),
            output_format: "png".to_owned(),
            pricing_eligible: true,
            batch_id: None,
            custom_id: None,
        };
        let start = start_event(&spec, 1_700_000_000);
        let final_event = CostEvent {
            schema_version: LEDGER_SCHEMA_VERSION,
            event_id: "final".to_owned(),
            operation_id: spec.operation_id.clone(),
            kind: CostEventKind::Final,
            recorded_at: 1_700_000_001,
            started_at: 1_700_000_000,
            transport: CostTransport::Live,
            model: "gpt-image-2".to_owned(),
            image_count: 1,
            quality: "low".to_owned(),
            size: "auto".to_owned(),
            output_format: "png".to_owned(),
            pricing_version: PRICING_VERSION.to_owned(),
            batch_id: None,
            custom_id: None,
            request_id: Some("req".to_owned()),
            outcome: CostOutcome::Succeeded,
            usage: Some(usage(10, 20)),
            estimated_nano_usd: Some(650_000),
        };
        let operations = fold_events(&[start, final_event.clone(), final_event]).unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].estimated_nano_usd, Some(650_000));
    }

    #[test]
    fn concurrent_starts_are_complete_and_corrupt_ledgers_are_rejected() {
        let directory = tempfile::Builder::new()
            .prefix("cost-ledger-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let path = directory.path().join("costs.jsonl");
        let ledger = std::sync::Arc::new(CostLedger::open(Some(&path)).unwrap());
        let handles = (0..8)
            .map(|index| {
                let ledger = ledger.clone();
                std::thread::spawn(move || {
                    ledger
                        .start(
                            CostOperationSpec {
                                operation_id: format!("live/concurrent-{index}"),
                                transport: CostTransport::Live,
                                model: "gpt-image-2".to_owned(),
                                image_count: 1,
                                quality: "low".to_owned(),
                                size: "auto".to_owned(),
                                output_format: "png".to_owned(),
                                pricing_eligible: true,
                                batch_id: None,
                                custom_id: None,
                            },
                            1_700_000_000 + index,
                        )
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(ledger.read().unwrap().len(), 8);

        std::fs::write(&path, b"{malformed}\n").unwrap();
        assert!(ledger.read().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ledger_paths_are_rejected_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::Builder::new()
            .prefix("cost-ledger-symlink-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let target = directory.path().join("outside");
        std::fs::write(&target, b"protected\n").unwrap();
        let ledger_path = directory.path().join("costs.jsonl");
        let spec = CostOperationSpec {
            operation_id: "live/symlink-test".to_owned(),
            transport: CostTransport::Live,
            model: "gpt-image-2".to_owned(),
            image_count: 1,
            quality: "low".to_owned(),
            size: "auto".to_owned(),
            output_format: "png".to_owned(),
            pricing_eligible: true,
            batch_id: None,
            custom_id: None,
        };

        symlink(&target, &ledger_path).unwrap();
        let ledger = CostLedger::open(Some(&ledger_path)).unwrap();
        assert!(ledger.start(spec.clone(), 1_700_000_000).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"protected\n");

        std::fs::remove_file(&ledger_path).unwrap();
        std::fs::remove_file(format!("{}.lock", ledger_path.display())).unwrap();
        symlink(&target, format!("{}.lock", ledger_path.display())).unwrap();
        assert!(ledger.start(spec, 1_700_000_001).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"protected\n");

        let real_parent = directory.path().join("real");
        std::fs::create_dir(&real_parent).unwrap();
        let linked_parent = directory.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();
        assert!(CostLedger::open(Some(&linked_parent.join("costs.jsonl"))).is_err());
    }

    #[test]
    fn unknown_batch_observation_can_be_reconciled_and_finalized() {
        let directory = tempfile::Builder::new()
            .prefix("cost-recovery-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let ledger = CostLedger::open(Some(&directory.path().join("costs.jsonl"))).unwrap();
        let operation_id = "batch/job-test/job-test-00".to_owned();
        ledger
            .start(
                CostOperationSpec {
                    operation_id: operation_id.clone(),
                    transport: CostTransport::Batch,
                    model: "gpt-image-2".to_owned(),
                    image_count: 1,
                    quality: "low".to_owned(),
                    size: "auto".to_owned(),
                    output_format: "png".to_owned(),
                    pricing_eligible: true,
                    batch_id: None,
                    custom_id: Some("job-test-00".to_owned()),
                },
                1_700_000_000,
            )
            .unwrap();
        ledger
            .resolve(CostResolution {
                operation_id: operation_id.clone(),
                kind: CostEventKind::Observed,
                outcome: CostOutcome::Unknown,
                recorded_at: 1_700_000_001,
                batch_id: None,
                request_id: None,
                usage: None,
            })
            .unwrap();
        ledger
            .resolve(CostResolution {
                operation_id: operation_id.clone(),
                kind: CostEventKind::Observed,
                outcome: CostOutcome::Accepted,
                recorded_at: 1_700_000_002,
                batch_id: Some("batch-test".to_owned()),
                request_id: Some("create".to_owned()),
                usage: None,
            })
            .unwrap();
        ledger
            .resolve(CostResolution {
                operation_id,
                kind: CostEventKind::Final,
                outcome: CostOutcome::Succeeded,
                recorded_at: 1_700_000_003,
                batch_id: Some("batch-test".to_owned()),
                request_id: Some("content".to_owned()),
                usage: Some(usage(10, 20)),
            })
            .unwrap();
        let operations = fold_events(&ledger.read().unwrap()).unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].outcome, CostOutcome::Succeeded);
        assert_eq!(operations[0].estimated_nano_usd, Some(325_000));
    }
}
