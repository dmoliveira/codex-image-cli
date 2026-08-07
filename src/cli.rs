use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::report::AppError;

const MAX_PROMPT_FILE_BYTES: usize = 256 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "codex-image",
    version,
    about = "Generate OpenAI GPT Image 2 assets without interactive prompts.",
    long_about = "A non-interactive, AI-friendly CLI for GPT Image 2. It uses the authenticated Codex CLI by default, or the Image API with --provider api."
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
    /// Check local configuration without sending a request or spending credits.
    Doctor,
    /// Print the non-interactive contract that AI agents can consume.
    AiHelp,
}

#[derive(Debug, Clone, Args)]
pub struct GenerateArgs {
    /// Image backend. The default uses the authenticated Codex CLI subscription.
    #[arg(long, value_enum, default_value_t = Provider::Codex)]
    pub provider: Provider,

    /// Text prompt for the image. Mutually exclusive with --prompt-file.
    #[arg(
        long,
        value_name = "TEXT",
        required_unless_present = "prompt_file",
        conflicts_with = "prompt_file"
    )]
    pub prompt: Option<String>,

    /// UTF-8 file containing the prompt. The special path '-' is intentionally unsupported.
    #[arg(long, value_name = "FILE", conflicts_with = "prompt")]
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

    /// Render quality. `low` is useful for quick drafts; `high` can cost more.
    #[arg(long, value_enum, default_value_t = Quality::Auto)]
    pub quality: Quality,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
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
        if !(1..=4).contains(&self.n) {
            return Err(AppError::usage(
                "invalid_image_count",
                "--n must be between 1 and 4.",
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
        Ok(())
    }
}

fn read_prompt_file(path: &Path) -> Result<String, AppError> {
    let metadata = fs::metadata(path).map_err(|_| {
        AppError::usage(
            "prompt_file_unreadable",
            "The prompt file could not be read as UTF-8.",
        )
    })?;
    if metadata.len() > MAX_PROMPT_FILE_BYTES as u64 {
        return Err(prompt_file_too_large());
    }
    let mut file = File::open(path).map_err(|_| {
        AppError::usage(
            "prompt_file_unreadable",
            "The prompt file could not be read as UTF-8.",
        )
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_PROMPT_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            AppError::usage(
                "prompt_file_unreadable",
                "The prompt file could not be read as UTF-8.",
            )
        })?;
    if bytes.len() > MAX_PROMPT_FILE_BYTES {
        return Err(prompt_file_too_large());
    }
    String::from_utf8(bytes).map_err(|_| {
        AppError::usage(
            "prompt_file_unreadable",
            "The prompt file could not be read as UTF-8.",
        )
    })
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
}
