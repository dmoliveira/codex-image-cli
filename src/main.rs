use std::env;

use clap::{error::ErrorKind, Parser};
use codex_image_cli::{
    cli::{Cli, Command},
    report::{AppError, RunReport, SCHEMA_VERSION},
    run_generate,
};
use serde::Serialize;

fn main() {
    // Clap normally prints and exits before application code can format an
    // error. Inspect the raw flag first so `--json` remains a reliable
    // machine-output contract even for malformed command lines.
    let wants_json = env::args_os().any(|argument| argument == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            if wants_json {
                if matches!(
                    error.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                ) {
                    print_json(&CliMetaReport {
                        schema_version: SCHEMA_VERSION,
                        ok: true,
                        status: if error.kind() == ErrorKind::DisplayHelp {
                            "help"
                        } else {
                            "version"
                        },
                        exit_code,
                    });
                } else {
                    let parse_error = AppError::usage(
                        "cli_parse_error",
                        "The command line could not be parsed. Run codex-image --help for valid non-interactive usage.",
                    );
                    emit_error(&parse_error, 1, true);
                }
            } else {
                let _ = error.print();
            }
            std::process::exit(exit_code);
        }
    };
    let exit_code = match cli.command {
        Command::Generate(args) => match run_generate(&args) {
            Ok(report) => {
                emit_run_report(&report, cli.json);
                report.exit_code
            }
            Err(error) => {
                emit_error(&error, args.n, cli.json);
                error.status.exit_code()
            }
        },
        Command::Doctor => run_doctor(cli.json),
        Command::AiHelp => run_ai_help(cli.json),
    };
    std::process::exit(exit_code);
}

#[derive(Serialize)]
struct CliMetaReport {
    schema_version: u8,
    ok: bool,
    status: &'static str,
    exit_code: i32,
}

fn emit_run_report(report: &RunReport, json: bool) {
    if json {
        print_json(report);
        return;
    }
    if report.status == "dry_run" {
        println!(
            "DRY RUN: no key was read, no network request was sent, and no files were reserved."
        );
    }
    for output in &report.outputs {
        println!("{output}");
    }
}

fn emit_error(error: &AppError, image_count: u8, json: bool) {
    if json {
        print_json(&error.report(image_count));
        return;
    }
    eprintln!("{}: {}", error.code, error.message);
    if let Some(request_id) = &error.request_id {
        eprintln!("request_id: {request_id}");
    }
    if !error.possibly_modified_paths.is_empty() {
        eprintln!("inspect these paths before retrying:");
        for path in &error.possibly_modified_paths {
            eprintln!("  {}", path.display());
        }
    }
}

#[derive(Serialize)]
struct DoctorReport {
    schema_version: u8,
    ok: bool,
    status: &'static str,
    exit_code: i32,
    checks: Vec<DoctorCheck>,
    note: &'static str,
}

#[derive(Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: &'static str,
    detail: &'static str,
}

fn run_doctor(json: bool) -> i32 {
    let key_present = env::var("OPENAI_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let report = DoctorReport {
        schema_version: SCHEMA_VERSION,
        ok: key_present,
        status: if key_present {
            "local_configuration_ready"
        } else {
            "local_configuration_required"
        },
        exit_code: if key_present { 0 } else { 2 },
        checks: vec![DoctorCheck {
            name: "OPENAI_API_KEY",
            status: if key_present { "present" } else { "missing" },
            detail: if key_present {
                "A non-empty key is present in the environment; it was not sent or validated remotely."
            } else {
                "Set a non-empty API key in the environment. This command never prompts, sends a request, or spends credits."
            },
        }],
        note: "A present key does not prove authentication, billing eligibility, model access, organization verification, or subscription entitlement.",
    };
    if json {
        print_json(&report);
    } else {
        for check in &report.checks {
            println!("{}: {} — {}", check.name, check.status, check.detail);
        }
        println!("Note: {}", report.note);
    }
    report.exit_code
}

fn run_ai_help(json: bool) -> i32 {
    let help = AiHelp {
        schema_version: SCHEMA_VERSION,
        command: "codex-image generate",
        non_interactive: true,
        required: AiRequirements {
            environment: "OPENAI_API_KEY (API billing and any required organization verification are separate from ChatGPT/Codex subscriptions)",
            flags: vec!["--prompt TEXT or --prompt-file FILE"],
        },
        safe_template: "codex-image generate --prompt \"<prompt>\" --output-dir ./artifacts/design --prefix <safe-stem> --n 1 --json",
        planning_template: "codex-image generate --prompt \"<prompt>\" --output-dir ./artifacts/design --prefix <safe-stem> --dry-run --json",
        rules: vec![
            "Run doctor --json before a real request; it only checks local key presence.",
            "Use --dry-run --json to validate names and parameters without reading a key or using a network.",
            "Create --output-dir explicitly; the CLI refuses missing or symlinked output directories.",
            "Use --name only for one image; use --prefix for deterministic multi-image names.",
            "Never retry exit code 5, 6, or 7 automatically because a generation may have been billed.",
            "Do not use ChatGPT/Codex subscription credentials as an Image API key.",
        ],
    };
    if json {
        print_json(&help);
    } else {
        println!("codex-image is fully non-interactive.");
        println!("Required environment: {}", help.required.environment);
        println!("Required input: {}", help.required.flags.join("; "));
        println!("Plan safely: {}", help.planning_template);
        println!("Generate: {}", help.safe_template);
        println!("For machine-readable instructions: codex-image ai-help --json");
    }
    0
}

#[derive(Serialize)]
struct AiHelp {
    schema_version: u8,
    command: &'static str,
    non_interactive: bool,
    required: AiRequirements,
    safe_template: &'static str,
    planning_template: &'static str,
    rules: Vec<&'static str>,
}

#[derive(Serialize)]
struct AiRequirements {
    environment: &'static str,
    flags: Vec<&'static str>,
}

fn print_json(value: &impl Serialize) {
    // Serialization uses only fixed schemas and local paths; no secrets are
    // retained in any report type. A failure here is impossible for these
    // types, but a compact fallback preserves the one-JSON-object contract.
    match serde_json::to_string(value) {
        Ok(json) => println!("{json}"),
        Err(_) => println!(
            "{{\"schema_version\":{SCHEMA_VERSION},\"ok\":false,\"status\":\"serialization_error\",\"exit_code\":1}}"
        ),
    }
}
