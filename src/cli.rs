use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::report::AppError;

const MAX_PROMPT_FILE_BYTES: usize = 256 * 1024;
pub const MAX_BATCH_IMAGES: u8 = 8;
const MAX_REQUEST_FILE_BYTES: usize = 256 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "codex-image",
    version,
    about = "Generate OpenAI GPT Image 2 assets without interactive prompts.",
    long_about = "A non-interactive, AI-friendly CLI for GPT Image 2. It uses the Image API by default, or the authenticated Codex CLI with --provider codex."
)]
pub struct Cli {
    /// Emit exactly one machine-readable JSON object on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Generate one or more images from a prompt.
    Generate(Box<GenerateArgs>),
    /// Submit and recover asynchronous OpenAI Batch API image jobs.
    Batch {
        #[command(subcommand)]
        command: BatchCommand,
    },
    /// Check local configuration without sending a request or spending credits.
    Doctor,
    /// Report locally recorded API image usage and estimated costs.
    Cost(CostArgs),
    /// Print the non-interactive contract that AI agents can consume.
    AiHelp,
}

#[derive(Debug, Subcommand)]
pub enum BatchCommand {
    /// Upload and submit a bounded image batch, persisting a recovery job.
    Submit(Box<BatchSubmitArgs>),
    /// Query one persisted Batch job once.
    Status(BatchJobArgs),
    /// Retrieve completed results, optionally polling with a bounded timeout.
    Retrieve(BatchRetrieveArgs),
    /// Request cancellation for one persisted Batch job.
    Cancel(BatchCancelArgs),
    /// Resume a safe local state or attach a manually reconciled remote ID.
    Recover(Box<BatchRecoverArgs>),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CostPeriod {
    #[value(alias = "daily", alias = "day")]
    Today,
    #[value(alias = "weekly")]
    Week,
    #[value(alias = "monthly")]
    Month,
    #[value(alias = "yearly")]
    Year,
    All,
}

#[derive(Debug, Clone, Args)]
pub struct CostArgs {
    /// Period to summarize. Dates are interpreted as UTC calendar dates.
    #[arg(long, value_enum, default_value_t = CostPeriod::Today)]
    pub period: CostPeriod,

    /// Inclusive UTC start date in YYYY-MM-DD form. Overrides the period start.
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub from: Option<String>,

    /// Inclusive UTC end date in YYYY-MM-DD form. Overrides the period end.
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub to: Option<String>,

    /// Include one row for each calendar day in the selected range.
    #[arg(long)]
    pub day_by_day: bool,

    /// Include one row for each recorded image-generation request.
    #[arg(long)]
    pub per_request: bool,

    /// Read a specific ledger instead of the default local state ledger.
    #[arg(long, value_name = "FILE")]
    pub ledger_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct BatchSubmitArgs {
    #[command(flatten)]
    pub generation: GenerateArgs,

    /// Exact local job record path. If omitted, use ~/.config/codex-image/jobs.
    #[arg(long, value_name = "FILE")]
    pub job_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct BatchJobArgs {
    /// Persisted local Batch job record to inspect or update.
    #[arg(long, value_name = "FILE")]
    pub job_file: PathBuf,

    /// Timeout for one HTTP operation, in seconds (1-300).
    #[arg(long, default_value_t = 180, value_name = "SECONDS")]
    pub timeout_seconds: u64,

    /// Explicitly approve sending OPENAI_API_KEY to the persisted custom HTTPS origin.
    #[arg(long, value_name = "ORIGIN")]
    pub dangerously_allow_api_key_to: Option<String>,

    /// Re-approve a persisted loopback HTTP endpoint for this operation.
    #[arg(long)]
    pub allow_insecure_localhost: bool,
}

#[derive(Debug, Args)]
pub struct BatchRetrieveArgs {
    #[command(flatten)]
    pub job: BatchJobArgs,

    /// Poll until a terminal remote state or the bounded maximum wait.
    #[arg(long)]
    pub wait: bool,

    /// Maximum polling duration, in seconds (1-86400).
    #[arg(long, default_value_t = 300, value_name = "SECONDS")]
    pub max_wait_seconds: u64,

    /// Delay between status reads while --wait is active (1-3600 seconds).
    #[arg(long, default_value_t = 10, value_name = "SECONDS")]
    pub poll_interval_seconds: u64,
}

#[derive(Debug, Args)]
pub struct BatchCancelArgs {
    #[command(flatten)]
    pub job: BatchJobArgs,
}

#[derive(Debug, Args)]
pub struct BatchRecoverArgs {
    #[command(flatten)]
    pub job: BatchJobArgs,

    /// Confirmed remote input file ID after inspecting the account/files API.
    #[arg(long, value_name = "FILE_ID", conflicts_with = "batch_id")]
    pub input_file_id: Option<String>,

    /// Confirmed remote Batch ID after inspecting the account/Batches API.
    #[arg(long, value_name = "BATCH_ID", conflicts_with = "input_file_id")]
    pub batch_id: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct GenerateArgs {
    /// Versioned JSON file containing structured generation parameters.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = [
            "provider",
            "prompt",
            "prompt_file",
            "n",
            "format",
            "size",
            "quality",
            "background",
            "compression",
            "moderation",
            "timeout_seconds"
        ]
    )]
    pub request_file: Option<PathBuf>,

    /// Image backend. The direct Image API is the default; Codex is explicit.
    #[arg(long, value_enum, default_value_t = Provider::Api)]
    pub provider: Provider,

    /// Text prompt for the image. Mutually exclusive with --prompt-file.
    #[arg(
        long,
        value_name = "TEXT",
        required_unless_present_any = ["prompt_file", "request_file"],
        conflicts_with_all = ["prompt_file", "request_file"]
    )]
    pub prompt: Option<String>,

    /// UTF-8 file containing the prompt. The special path '-' is intentionally unsupported.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = ["prompt", "request_file"]
    )]
    pub prompt_file: Option<PathBuf>,

    /// Number of images to request in one API call (1-4).
    #[arg(long, default_value_t = 1, value_name = "COUNT")]
    pub n: u8,

    /// Existing, non-symlink directory where images will be written.
    #[arg(long, default_value = ".", value_name = "DIR")]
    pub output_dir: PathBuf,

    /// Exact filename stem for one image, for example 'hero'. Cannot be combined with --prefix.
    #[arg(long, value_name = "STEM", conflicts_with = "prefix")]
    pub name: Option<String>,

    /// Filename stem for generated outputs, for example 'hero' -> hero-01.png.
    #[arg(long, value_name = "STEM")]
    pub prefix: Option<String>,

    /// Output container format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Png)]
    pub format: OutputFormat,

    /// `auto` or WIDTHxHEIGHT. GPT Image 2 dimensions must follow its documented limits.
    #[arg(long, default_value = "auto", value_name = "SIZE")]
    pub size: String,

    /// Render quality. `low` is the cost-conscious default; `high` requires explicit confirmation.
    #[arg(long, value_enum, default_value_t = Quality::Low)]
    pub quality: Quality,

    /// Confirm the approximate cost and increased usage of high-quality generation.
    #[arg(long)]
    pub confirm_high_quality: bool,

    /// Background behavior. GPT Image 2 currently supports `auto` and `opaque` only.
    #[arg(long, value_enum, default_value_t = Background::Auto)]
    pub background: Background,

    /// Compression percentage for JPEG/WebP only (0-100).
    #[arg(long, value_name = "PERCENT")]
    pub compression: Option<u8>,

    /// GPT Image moderation strictness.
    #[arg(long, value_enum, default_value_t = Moderation::Auto)]
    pub moderation: Moderation,

    /// Permit replacing regular output files. Never follows output symlinks.
    #[arg(long)]
    pub overwrite: bool,

    /// Validate the complete request/output plan without reading a key, reserving files, or contacting a network.
    #[arg(long)]
    pub dry_run: bool,

    /// Total timeout for the one API request, in seconds (1-300).
    #[arg(long, default_value_t = 180, value_name = "SECONDS")]
    pub timeout_seconds: u64,

    /// API base URL. Defaults to OpenAI. Custom origins require explicit key-destination approval.
    #[arg(long, default_value = "https://api.openai.com/v1", value_name = "URL")]
    pub api_base_url: String,

    /// Explicitly approve sending OPENAI_API_KEY to this exact custom HTTPS origin, for example https://gateway.example.
    #[arg(long, value_name = "ORIGIN")]
    pub dangerously_allow_api_key_to: Option<String>,

    /// Permit a loopback-only HTTP URL for local tests. Never permits non-loopback HTTP.
    #[arg(long)]
    pub allow_insecure_localhost: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Api,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Api => "api",
        }
    }
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        }
    }

    pub fn as_api_value(self) -> &'static str {
        self.extension()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Auto,
    Low,
    Medium,
    High,
}

impl Quality {
    pub fn as_api_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    Auto,
    Opaque,
    Transparent,
}

impl Background {
    pub fn as_api_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Opaque => "opaque",
            Self::Transparent => "transparent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Moderation {
    Auto,
    Low,
}

impl Moderation {
    pub fn as_api_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
        }
    }
}

impl GenerateArgs {
    pub fn resolve_request_file(&self) -> Result<Self, AppError> {
        let Some(path) = &self.request_file else {
            return Ok(self.clone());
        };
        if path.as_os_str() == "-" {
            return Err(AppError::usage(
                "request_file_stdin_not_supported",
                "--request-file - is not supported because this CLI never waits for interactive stdin. Use a UTF-8 JSON file path instead.",
            ));
        }
        let bytes = read_bounded_file(
            path,
            MAX_REQUEST_FILE_BYTES,
            request_file_unreadable,
            request_file_unreadable,
        )?;
        let request: RequestFile = serde_json::from_slice(&bytes).map_err(|_| {
            AppError::usage(
                "request_file_invalid_json",
                "The request file is not valid UTF-8 JSON matching the documented schema.",
            )
        })?;
        if request.schema_version != 1 {
            return Err(AppError::usage(
                "request_file_schema_unsupported",
                "The request file schema_version must be 1.",
            ));
        }
        let mut resolved = self.clone();
        resolved.request_file = None;
        resolved.prompt = Some(request.prompt);
        resolved.prompt_file = None;
        if let Some(provider) = request.provider {
            resolved.provider = provider;
        }
        if let Some(n) = request.n {
            resolved.n = n;
        }
        if let Some(format) = request.format {
            resolved.format = format;
        }
        if let Some(size) = request.size {
            resolved.size = size;
        }
        if let Some(quality) = request.quality {
            resolved.quality = quality;
        }
        if let Some(background) = request.background {
            resolved.background = background;
        }
        if let Some(compression) = request.compression {
            resolved.compression = compression;
        }
        if let Some(moderation) = request.moderation {
            resolved.moderation = moderation;
        }
        Ok(resolved)
    }

    pub fn read_prompt(&self) -> Result<String, AppError> {
        let prompt = match (&self.prompt, &self.prompt_file) {
            (Some(prompt), None) => prompt.clone(),
            (None, Some(path)) => {
                if path.as_os_str() == "-" {
                    return Err(AppError::usage(
                        "stdin_prompt_not_supported",
                        "--prompt-file - is not supported because this CLI never waits for interactive stdin. Use a UTF-8 file path instead.",
                    ));
                }
                read_prompt_file(path)?
            }
            _ => {
                return Err(AppError::usage(
                    "missing_prompt",
                    "Provide exactly one of --prompt or --prompt-file.",
                ))
            }
        };
        Ok(prompt)
    }

    pub fn validate(&self, prompt: &str) -> Result<(), AppError> {
        self.validate_with_limit(prompt, 4, true)
    }

    pub fn validate_dry_run(&self, prompt: &str) -> Result<(), AppError> {
        self.validate_with_limit(prompt, 4, false)
    }

    pub fn validate_batch(&self, prompt: &str) -> Result<(), AppError> {
        self.validate_with_limit(prompt, MAX_BATCH_IMAGES, true)
    }

    pub fn validate_batch_dry_run(&self, prompt: &str) -> Result<(), AppError> {
        self.validate_with_limit(prompt, MAX_BATCH_IMAGES, false)
    }

    fn validate_with_limit(
        &self,
        prompt: &str,
        image_limit: u8,
        require_high_quality_confirmation: bool,
    ) -> Result<(), AppError> {
        if prompt.trim().is_empty() {
            return Err(AppError::usage(
                "empty_prompt",
                "The prompt must contain non-whitespace text.",
            ));
        }
        if prompt.chars().count() > 32_000 {
            return Err(AppError::usage(
                "prompt_too_long",
                "The prompt exceeds the local 32,000-character safety limit.",
            ));
        }
        if !(1..=image_limit).contains(&self.n) {
            return Err(AppError::usage(
                "invalid_image_count",
                format!("--n must be between 1 and {image_limit}."),
            ));
        }
        if self.name.is_some() && self.n != 1 {
            return Err(AppError::usage(
                "name_requires_one_image",
                "--name is only valid with --n 1. Use --prefix for multiple deterministic outputs.",
            ));
        }
        if !(1..=300).contains(&self.timeout_seconds) {
            return Err(AppError::usage(
                "invalid_timeout",
                "--timeout-seconds must be between 1 and 300.",
            ));
        }
        validate_size(&self.size)?;
        if self.compression.is_some() && self.format == OutputFormat::Png {
            return Err(AppError::usage(
                "compression_requires_lossy_format",
                "--compression is supported only with --format jpeg or --format webp.",
            ));
        }
        if self
            .compression
            .is_some_and(|compression| compression > 100)
        {
            return Err(AppError::usage(
                "invalid_compression",
                "--compression must be between 0 and 100.",
            ));
        }
        if self.background == Background::Transparent {
            return Err(AppError::usage(
                "transparent_background_unsupported",
                "gpt-image-2 does not currently support transparent backgrounds. Use --background auto or opaque.",
            ));
        }
        if require_high_quality_confirmation
            && self.quality == Quality::High
            && !self.confirm_high_quality
        {
            return Err(AppError::usage(
                "high_quality_confirmation_required",
                high_quality_confirmation_message(self.provider),
            ));
        }
        Ok(())
    }
}

fn high_quality_confirmation_message(provider: Provider) -> String {
    let provider_note = match provider {
        Provider::Api => {
            let live = crate::cost::preflight_preview(
                crate::cost::CostTransport::Live,
                "gpt-image-2",
                1,
                "high",
                "1024x1024",
                true,
            );
            let batch = crate::cost::preflight_preview(
                crate::cost::CostTransport::Batch,
                "gpt-image-2",
                1,
                "high",
                "1024x1024",
                true,
            );
            format!(
                "For a typical 1024x1024 image, the known output-only estimate is about {} on Standard or {} through Batch; total cost is unknown because input charges are excluded.",
                live.estimated_output_usd.as_deref().unwrap_or("unavailable"),
                batch.estimated_output_usd.as_deref().unwrap_or("unavailable")
            )
        }
        Provider::Codex => {
            "Codex subscription usage and limits are account-dependent; the CLI cannot verify a dollar estimate locally."
                .to_owned()
        }
    };
    format!(
        "--quality high requires --confirm-high-quality. {provider_note} Actual cost varies with resolution and image-output tokens."
    )
}

fn read_prompt_file(path: &Path) -> Result<String, AppError> {
    let bytes = read_bounded_file(
        path,
        MAX_PROMPT_FILE_BYTES,
        || {
            AppError::usage(
                "prompt_file_unreadable",
                "The prompt file could not be read as UTF-8.",
            )
        },
        prompt_file_too_large,
    )?;
    String::from_utf8(bytes).map_err(|_| {
        AppError::usage(
            "prompt_file_unreadable",
            "The prompt file could not be read as UTF-8.",
        )
    })
}

fn read_bounded_file(
    path: &Path,
    limit: usize,
    error: fn() -> AppError,
    too_large: fn() -> AppError,
) -> Result<Vec<u8>, AppError> {
    let mut file = File::open(path).map_err(|_| error())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error())?;
    if bytes.len() > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

fn request_file_unreadable() -> AppError {
    AppError::usage(
        "request_file_unreadable",
        "The request file could not be read as bounded UTF-8 JSON.",
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFile {
    schema_version: u8,
    prompt: String,
    provider: Option<Provider>,
    n: Option<u8>,
    format: Option<OutputFormat>,
    size: Option<String>,
    quality: Option<Quality>,
    background: Option<Background>,
    compression: Option<Option<u8>>,
    moderation: Option<Moderation>,
}

fn prompt_file_too_large() -> AppError {
    AppError::usage(
        "prompt_file_too_large",
        "The prompt file exceeds the local 256 KiB safety limit.",
    )
}

fn validate_size(size: &str) -> Result<(), AppError> {
    if size == "auto" {
        return Ok(());
    }
    let Some((width, height)) = size.split_once('x') else {
        return Err(size_error());
    };
    let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>()) else {
        return Err(size_error());
    };
    let (long_edge, short_edge) = (width.max(height), width.min(height));
    let pixels = u64::from(width) * u64::from(height);
    let valid = width > 0
        && height > 0
        && width % 16 == 0
        && height % 16 == 0
        && long_edge <= 3840
        && long_edge <= short_edge.saturating_mul(3)
        && (655_360..=8_294_400).contains(&pixels);
    if valid {
        Ok(())
    } else {
        Err(size_error())
    }
}

fn size_error() -> AppError {
    AppError::usage(
        "invalid_size",
        "--size must be auto or WIDTHxHEIGHT with 16px multiples, <=3840px edges, <=3:1 ratio, and 655,360-8,294,400 pixels.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_documented_and_custom_sizes() {
        for size in ["auto", "1024x1024", "1536x1024", "2048x1152", "3840x2160"] {
            assert!(validate_size(size).is_ok(), "{size}");
        }
    }

    #[test]
    fn rejects_invalid_sizes() {
        for size in ["1025x1024", "3841x1024", "1024x128", "1024x4096", "large"] {
            assert!(validate_size(size).is_err(), "{size}");
        }
    }

    #[test]
    fn rejects_oversized_prompt_file_before_loading_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-prompt.txt");
        std::fs::write(&path, vec![b'x'; MAX_PROMPT_FILE_BYTES + 1]).unwrap();
        let error = read_prompt_file(&path).unwrap_err();
        assert_eq!(error.code, "prompt_file_too_large");
    }

    #[test]
    fn high_quality_requires_explicit_confirmation() {
        let error = high_quality_confirmation_message(Provider::Api);
        assert!(error.contains("$0.211000"));
        assert!(error.contains("--confirm-high-quality"));
    }
}
