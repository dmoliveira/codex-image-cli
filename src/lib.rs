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

use std::env;

use api::{ApiClient, ImageGenerationRequest};
use cli::GenerateArgs;
use endpoint::Endpoint;
use image::decode_images;
use output::{derive_file_names, derive_output_paths, OutputTransaction};
use report::{AppError, RunReport};

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
    let endpoint = Endpoint::authorize(
        &args.api_base_url,
        args.dangerously_allow_api_key_to.as_deref(),
        args.allow_insecure_localhost,
    )?;
    args.validate(&prompt)?;

    let planned_outputs = derive_output_paths(&args.output_dir, &file_names);
    if args.dry_run {
        return Ok(RunReport::dry_run(args.n, planned_outputs));
    }

    let api_key = env::var("OPENAI_API_KEY").map_err(|_| {
        AppError::usage(
            "missing_api_key",
            "OPENAI_API_KEY must be set for generation. A ChatGPT or Codex subscription login is not an Image API credential.",
        )
    })?;
    if api_key.trim().is_empty() {
        return Err(AppError::usage(
            "empty_api_key",
            "OPENAI_API_KEY is empty. Set a non-empty API key in the environment; do not pass it on the command line.",
        ));
    }

    let client = ApiClient::new(args.timeout_seconds)?;
    let mut transaction = OutputTransaction::reserve(&args.output_dir, file_names, args.overwrite)?;

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

    if let Err(mut error) = transaction.stage_all(&images) {
        error.set_request_id(response.request_id.clone());
        error.set_http_status(response.status);
        error.add_possibly_modified_paths(transaction.abort());
        return Err(error);
    }

    match transaction.commit_all() {
        Ok(result) => Ok(RunReport::success(
            args.n,
            result.outputs,
            result.retained_artifacts,
            response.request_id,
            response.status,
        )),
        Err(mut error) => {
            error.set_request_id(response.request_id);
            error.set_http_status(response.status);
            Err(error)
        }
    }
}
