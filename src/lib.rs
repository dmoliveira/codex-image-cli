//! The library behind the `codex-image` binary.
//!
//! It deliberately keeps the generation path small and explicit: validate,
//! reserve outputs, make one billable request, validate every returned image,
//! then publish all files or report a truthful failure.

pub mod api;
pub mod cli;
pub mod endpoint;
pub mod image;
pub mod output;
pub mod report;

use std::{
    env, fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use api::{ApiClient, ImageGenerationRequest};
use cli::{GenerateArgs, Provider};
use image::{decode_images, validate_image_bytes};
use output::{derive_file_names, derive_output_paths, OutputTransaction};
use report::{AppError, RunReport};
use serde::Serialize;

/// Model supported by this focused CLI. Keeping the model fixed lets local
/// validation match the current documented GPT Image 2 feature set.
pub const MODEL: &str = "gpt-image-2";

/// Validate and execute one image-generation command.
pub fn run_generate(args: &GenerateArgs) -> Result<RunReport, AppError> {
    let prompt = args.read_prompt()?;
    let file_names = derive_file_names(
        args.n,
        args.name.as_deref(),
        args.prefix.as_deref(),
        args.format,
    )?;
    args.validate(&prompt)?;

    let planned_outputs = derive_output_paths(&args.output_dir, &file_names);
    validate_provider_args(args)?;
    if args.dry_run {
        return Ok(RunReport::dry_run(args.n, planned_outputs, args.provider));
    }

    let api_key = if args.provider == Provider::Api {
        let key = env::var("OPENAI_API_KEY").map_err(|_| {
            AppError::usage(
                "missing_api_key",
                "OPENAI_API_KEY must be set for --provider api. The default provider uses the authenticated Codex CLI subscription.",
            )
        })?;
        if key.trim().is_empty() {
            return Err(AppError::usage(
                "empty_api_key",
                "OPENAI_API_KEY is empty. Set a non-empty API key in the environment; do not pass it on the command line.",
            ));
        }
        ApiClient::new(args.timeout_seconds)?;
        Some(key)
    } else {
        None
    };
    let mut transaction = OutputTransaction::reserve(&args.output_dir, file_names, args.overwrite)?;

    let (images, request_id, http_status) = match args.provider {
        Provider::Api => {
            let endpoint = endpoint::Endpoint::authorize(
                &args.api_base_url,
                args.dangerously_allow_api_key_to.as_deref(),
                args.allow_insecure_localhost,
            )?;
            let api_key = api_key.expect("API key was preflighted for the API provider");
            let client = ApiClient::new(args.timeout_seconds)?;
            let request = ImageGenerationRequest::from_args(&prompt, args);
            let response = match client.generate(&endpoint, &api_key, &request) {
                Ok(response) => response,
                Err(mut error) => {
                    error.add_possibly_modified_paths(transaction.abort());
                    return Err(error);
                }
            };
            let images = match decode_images(&response.body, args.n, args.format) {
                Ok(images) => images,
                Err(mut error) => {
                    error.set_request_id(response.request_id);
                    error.set_http_status(response.status);
                    error.add_possibly_modified_paths(transaction.abort());
                    return Err(error);
                }
            };
            (images, response.request_id, Some(response.status))
        }
        Provider::Codex => match generate_with_codex(&prompt, args) {
            Ok(image) => (vec![image], None, None),
            Err(mut error) => {
                error.add_possibly_modified_paths(transaction.abort());
                return Err(error);
            }
        },
    };

    if let Err(mut error) = transaction.stage_all(&images) {
        error.set_request_id(request_id.clone());
        if let Some(status) = http_status {
            error.set_http_status(status);
        }
        error.add_possibly_modified_paths(transaction.abort());
        return Err(error);
    }

    match transaction.commit_all() {
        Ok(result) => Ok(RunReport::success(
            args.n,
            result.outputs,
            result.retained_artifacts,
            request_id,
            http_status,
            args.provider,
        )),
        Err(mut error) => {
            error.set_request_id(request_id);
            if let Some(status) = http_status {
                error.set_http_status(status);
            }
            Err(error)
        }
    }
}

fn generate_with_codex(prompt: &str, args: &GenerateArgs) -> Result<Vec<u8>, AppError> {
    let temporary_dir = env::temp_dir().join(format!(
        ".codex-image-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    fs::create_dir(&temporary_dir).map_err(|_| {
        AppError::preflight(
            "codex_workspace_unavailable",
            "The Codex provider could not create a private workspace; no generation was attempted.",
        )
    })?;
    let temporary_path = temporary_dir.join("generated.png");
    let request = CodexGenerationRequest {
        schema_version: 1,
        operation: "generate_image",
        prompt,
        artifact_path: &temporary_path,
        count: args.n,
        format: args.format.as_api_value(),
        size: &args.size,
        quality: args.quality.as_api_value(),
    };
    let request_json = serde_json::to_string(&request).expect("Codex request is serializable");
    let instruction = format!(
        "Use the default built-in image generation capability available through this authenticated Codex subscription. Do not use an API-key fallback. Treat the following JSON as the authoritative request. Save exactly the requested raster artifact and do not merely describe it.\n{request_json}"
    );
    let mut child = Command::new("codex")
        .args([
            "exec",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "workspace-write",
            "--json",
            "-C",
        ])
        .arg(&temporary_dir)
        .arg(instruction)
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            AppError::usage(
                "codex_cli_unavailable",
                "The default Codex provider requires the authenticated `codex` CLI. Use --provider api for direct Image API generation.",
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_seconds);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    if result.is_none() {
        let mut error = AppError::indeterminate(
            "codex_generation_timeout",
            "The Codex image-generation command exceeded the configured timeout. The subscription generation outcome may be unknown; do not retry automatically.",
        );
        error.add_possibly_modified_paths(vec![temporary_path]);
        return Err(error);
    }
    let result = result.expect("checked above");
    let image = match fs::read(&temporary_path) {
        Ok(image) => image,
        Err(_) => {
            let mut error = if result.success() {
                AppError::invalid_response(
                    "codex_image_missing",
                    "Codex completed without producing the requested PNG. No usable image was returned; do not retry automatically.",
                )
            } else {
                AppError::indeterminate(
                    "codex_generation_outcome_unknown",
                    "Codex may have generated an image, but the CLI did not produce a readable PNG. Inspect the output directory before retrying.",
                )
            };
            error.add_possibly_modified_paths(vec![temporary_path]);
            return Err(error);
        }
    };
    if !result.success() {
        let mut error = AppError::indeterminate(
            "codex_generation_failed",
            "The Codex image-generation command failed after it was started. The subscription generation outcome may be unknown; do not retry automatically.",
        );
        error.add_possibly_modified_paths(vec![temporary_path]);
        return Err(error);
    }
    let image = match validate_image_bytes(image, cli::OutputFormat::Png) {
        Ok(image) => image,
        Err(mut error) => {
            error.add_possibly_modified_paths(vec![temporary_path]);
            return Err(error);
        }
    };
    if fs::remove_file(&temporary_path).is_err() || fs::remove_dir(&temporary_dir).is_err() {
        let mut error = AppError::output_commit(
            "codex_workspace_cleanup_failed",
            "The generated image was valid, but the private Codex workspace could not be cleaned safely; inspect the listed path.",
        );
        error.add_possibly_modified_paths(vec![temporary_path]);
        return Err(error);
    }
    Ok(image)
}

#[derive(Debug, Serialize)]
struct CodexGenerationRequest<'a> {
    schema_version: u8,
    operation: &'static str,
    prompt: &'a str,
    artifact_path: &'a Path,
    count: u8,
    format: &'static str,
    size: &'a str,
    quality: &'static str,
}

fn validate_provider_args(args: &GenerateArgs) -> Result<(), AppError> {
    if args.provider == Provider::Codex
        && (args.n != 1
            || args.format != cli::OutputFormat::Png
            || args.compression.is_some()
            || args.background != cli::Background::Auto
            || args.moderation != cli::Moderation::Auto)
    {
        return Err(AppError::usage(
            "codex_provider_constraints",
            "The Codex subscription provider currently supports exactly one PNG per command. Use --provider api for other counts or formats.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_request_serializes_prompt_as_data() {
        let request = CodexGenerationRequest {
            schema_version: 1,
            operation: "generate_image",
            prompt: "quoted \"prompt\"\n日本語",
            artifact_path: Path::new("/private/generated.png"),
            count: 1,
            format: "png",
            size: "1024x1024",
            quality: "high",
        };
        let value: serde_json::Value = serde_json::to_value(request).unwrap();
        assert_eq!(value["prompt"], "quoted \"prompt\"\n日本語");
        assert_eq!(value["artifact_path"], "/private/generated.png");
        assert_eq!(value["operation"], "generate_image");
    }
}
