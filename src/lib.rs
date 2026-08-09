//! The library behind the `codex-image` binary.
//!
//! It deliberately keeps the generation path small and explicit: validate,
//! reserve outputs, make one billable request, validate every returned image,
//! then publish all files or report a truthful failure.

pub mod api;
pub mod batch;
pub mod cli;
pub mod endpoint;
pub mod image;
pub mod manifest;
pub mod output;
pub mod provider;
pub mod report;
pub mod run;

use std::{
    env, fs,
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use api::{ApiClient, ImageGenerationRequest};
use cli::{GenerateArgs, Provider};
use image::{decode_images, validate_image_bytes};
use output::{derive_file_names, derive_output_paths, OutputTransaction};
use report::{AppError, RunReport};
use serde::Serialize;

const MAX_CODEX_DIAGNOSTIC_BYTES: usize = 32 * 1024;

/// Model supported by this focused CLI. Keeping the model fixed lets local
/// validation match the current documented GPT Image 2 feature set.
pub const MODEL: &str = "gpt-image-2";

/// Validate and execute one image-generation command.
pub fn run_generate(args: &GenerateArgs) -> Result<RunReport, AppError> {
    let raw_provider = args.provider;
    let raw_count = args.n;
    let args = match args.resolve_request_file() {
        Ok(args) => args,
        Err(mut error) => {
            error.set_provider(raw_provider);
            error.set_image_count(raw_count);
            return Err(error);
        }
    };
    let selected_provider = args.provider;
    let selected_count = args.n;
    run_generate_inner(&args).map_err(|mut error| {
        error.set_provider(selected_provider);
        error.set_image_count(selected_count);
        error
    })
}

fn run_generate_inner(args: &GenerateArgs) -> Result<RunReport, AppError> {
    let prompt = args.read_prompt()?;
    let file_names = derive_file_names(
        args.n,
        args.name.as_deref(),
        args.prefix.as_deref(),
        args.format,
    )?;
    args.validate(&prompt)?;

    let planned_outputs = derive_output_paths(&args.output_dir, &file_names);
    provider::validate(args)?;
    if args.dry_run {
        return Ok(RunReport::dry_run(args.n, planned_outputs, args.provider));
    }

    let api_key = if args.provider == Provider::Api {
        let key = env::var("OPENAI_API_KEY").map_err(|_| {
            AppError::usage(
                "missing_api_key",
                "OPENAI_API_KEY must be set for the default --provider api path, or select --provider codex explicitly.",
            )
        })?;
        api::validate_api_key(&key)?;
        let client = ApiClient::new(args.timeout_seconds)?;
        Some((key, client))
    } else {
        None
    };
    let endpoint = if args.provider == Provider::Api {
        Some(endpoint::Endpoint::authorize(
            &args.api_base_url,
            args.dangerously_allow_api_key_to.as_deref(),
            args.allow_insecure_localhost,
        )?)
    } else {
        None
    };
    let mut transaction = OutputTransaction::reserve(&args.output_dir, file_names, args.overwrite)?;

    let (images, request_id, http_status) = match args.provider {
        Provider::Api => {
            let (api_key, client) = api_key.expect("API configuration was preflighted");
            let endpoint = endpoint.expect("endpoint was preflighted for the API provider");
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
    let temporary_dir = tempfile::Builder::new()
        .prefix(".codex-image-cli-")
        .tempdir()
        .map_err(|_| {
            AppError::preflight(
            "codex_workspace_unavailable",
            "The Codex provider could not create a private workspace; no generation was attempted.",
        )
        })?;
    let temporary_path = temporary_dir.path().join("generated.png");
    let request_path = temporary_dir.path().join("request.json");
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
    if fs::write(&request_path, request_json).is_err() {
        return Err(AppError::preflight(
            "codex_request_unavailable",
            "The Codex provider could not write its private structured request; no generation was attempted.",
        ));
    }
    let instruction = format!(
        "Use the default built-in image generation capability available through this authenticated Codex subscription. Do not use an API-key fallback. Read the authoritative structured JSON request at {}. Save exactly the requested raster artifact and do not merely describe it.",
        request_path.display()
    );
    let mut command = Command::new(provider::executable());
    command
        .args([
            "exec",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "workspace-write",
            "--json",
            "-C",
        ])
        .arg(temporary_dir.path())
        .arg(instruction)
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = fs::remove_file(&request_path);
            return Err(AppError::usage(
                "codex_cli_unavailable",
            "The explicit --provider codex path requires the authenticated `codex` CLI. Omit --provider or use --provider api for direct Image API generation.",
            ));
        }
    };
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let stdout = child.stdout.take().map(|stream| {
        thread::spawn(move || {
            let _ = stdout_sender.send(read_bounded_diagnostics(stream));
        })
    });
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    let stderr = child.stderr.take().map(|stream| {
        thread::spawn(move || {
            let _ = stderr_sender.send(read_bounded_diagnostics(stream));
        })
    });
    let deadline = Instant::now() + Duration::from_secs(args.timeout_seconds);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                stop_process_group(&mut child);
                break None;
            }
            Err(_) => {
                stop_process_group(&mut child);
                break None;
            }
        }
    };
    let stdout_diagnostics = stdout_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or((0, true));
    let stderr_diagnostics = stderr_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or((0, true));
    let diagnostics_bytes = stdout_diagnostics.0 + stderr_diagnostics.0;
    let diagnostics_truncated = stdout_diagnostics.1 || stderr_diagnostics.1;
    drop(stdout);
    drop(stderr);
    if result.is_none() {
        let mut error = AppError::indeterminate(
            "codex_generation_timeout",
            "The Codex image-generation command exceeded the configured timeout. The subscription generation outcome may be unknown; do not retry automatically.",
        );
        error.set_process_metadata(None, true, diagnostics_bytes, diagnostics_truncated);
        error.add_possibly_modified_paths(vec![request_path, temporary_path]);
        let _ = temporary_dir.keep();
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
            error.set_process_metadata(
                result.code(),
                false,
                diagnostics_bytes,
                diagnostics_truncated,
            );
            error.add_possibly_modified_paths(vec![request_path, temporary_path]);
            let _ = temporary_dir.keep();
            return Err(error);
        }
    };
    if !result.success() {
        let mut error = AppError::indeterminate(
            "codex_generation_failed",
            "The Codex image-generation command failed after it was started. The subscription generation outcome may be unknown; do not retry automatically.",
        );
        error.set_process_metadata(
            result.code(),
            false,
            diagnostics_bytes,
            diagnostics_truncated,
        );
        error.add_possibly_modified_paths(vec![request_path, temporary_path]);
        let _ = temporary_dir.keep();
        return Err(error);
    }
    let image = match validate_image_bytes(image, cli::OutputFormat::Png) {
        Ok(image) => image,
        Err(mut error) => {
            error.add_possibly_modified_paths(vec![request_path, temporary_path]);
            let _ = temporary_dir.keep();
            return Err(error);
        }
    };
    if fs::remove_file(&temporary_path).is_err()
        || fs::remove_file(&request_path).is_err()
        || fs::remove_dir(temporary_dir.path()).is_err()
    {
        let mut error = AppError::output_commit(
            "codex_workspace_cleanup_failed",
            "The generated image was valid, but the private Codex workspace could not be cleaned safely; inspect the listed path.",
        );
        error.add_possibly_modified_paths(vec![request_path, temporary_path]);
        let _ = temporary_dir.keep();
        return Err(error);
    }
    Ok(image)
}

fn read_bounded_diagnostics(mut reader: impl Read) -> (usize, bool) {
    let mut buffer = [0_u8; 8192];
    let mut bytes = 0_usize;
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                bytes = bytes.saturating_add(read);
                truncated |= bytes > MAX_CODEX_DIAGNOSTIC_BYTES;
            }
            Err(_) => break,
        }
    }
    (bytes.min(MAX_CODEX_DIAGNOSTIC_BYTES), truncated)
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
}

fn stop_process_group(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as libc::pid_t;
        let _ = libc::kill(-pid, libc::SIGTERM);
        thread::sleep(Duration::from_millis(100));
        let _ = libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
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
