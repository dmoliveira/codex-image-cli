use std::{
    env,
    io::Read,
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use clap::{error::ErrorKind, Parser};
use codex_image_cli::{
    batch,
    cli::{BatchCommand, Cli, Command},
    provider,
    report::{AppError, BatchReport, RunReport, SCHEMA_VERSION},
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
        Command::Batch { command } => match command {
            BatchCommand::Submit(args) => emit_batch(batch::submit(&args), cli.json),
            BatchCommand::Status(args) => emit_batch(batch::status(&args), cli.json),
            BatchCommand::Retrieve(args) => emit_batch(batch::retrieve(&args), cli.json),
            BatchCommand::Cancel(args) => emit_batch(batch::cancel(&args), cli.json),
            BatchCommand::Recover(args) => emit_batch(batch::recover(&args), cli.json),
        },
        Command::Doctor => run_doctor(cli.json),
        Command::AiHelp => run_ai_help(cli.json),
    };
    std::process::exit(exit_code);
}

fn emit_batch(
    result: Result<BatchReport, codex_image_cli::batch::BatchFailure>,
    json: bool,
) -> i32 {
    match result {
        Ok(report) => {
            let exit_code = report.exit_code;
            if json {
                print_json(&report);
            } else if report.ok {
                if let Some(job_file) = report.job_file {
                    println!("job: {job_file}");
                }
                if let Some(batch_id) = report.batch_id {
                    println!("batch: {batch_id}");
                }
                for output in report.outputs {
                    println!("{output}");
                }
                if let Some(next_action) = report.next_action {
                    println!("next: {next_action}");
                }
            } else {
                eprintln!("{}", report.status);
            }
            exit_code
        }
        Err(failure) => {
            let report = failure.context.report(Some(&failure.error));
            let exit_code = report.exit_code;
            if json {
                print_json(&report);
            } else {
                eprintln!("{}: {}", failure.error.code, failure.error.message);
                if let Some(job_file) = report.job_file {
                    eprintln!("job: {job_file}");
                }
                if let Some(next_action) = report.next_action {
                    eprintln!("next: {next_action}");
                }
            }
            exit_code
        }
    }
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
    let codex_present = codex_version_available();
    let login_status = if codex_present {
        codex_login_status()
    } else {
        "missing"
    };
    let api_key_present = env::var("OPENAI_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let ready = login_status == "logged_in" || api_key_present;
    let report = DoctorReport {
        schema_version: SCHEMA_VERSION,
        ok: ready,
        status: if ready {
            "local_configuration_ready"
        } else {
            "local_configuration_required"
        },
        exit_code: if ready { 0 } else { 2 },
        checks: vec![
            DoctorCheck {
                name: "CODEX_CLI",
                status: if codex_present { "present" } else { "missing" },
                detail: if codex_present {
                    "The Codex CLI is available locally."
                } else {
                    "Install the Codex CLI only when using the explicit subscription provider."
                },
            },
            DoctorCheck {
                name: "CODEX_LOGIN",
                status: login_status,
                detail: match login_status {
                    "logged_in" => "Codex reports a local login; image entitlement is not verified.",
                    "logged_out" => "Codex is installed but does not report an active login.",
                    "missing" => "Codex login status was not checked because the executable is unavailable.",
                    _ => "Codex login status could not be classified safely.",
                },
            },
            DoctorCheck {
                name: "OPENAI_API_KEY",
                status: if api_key_present { "present" } else { "missing" },
                detail: if api_key_present {
                    "A non-empty API key is present locally; it was not sent or remotely validated."
                } else {
                    "No API key is present; this is required only for --provider api."
                },
            },
        ],
        note: "Login and key presence do not prove API billing, organization verification, model access, entitlement, or successful generation.",
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

fn codex_login_status() -> &'static str {
    let mut command = ProcessCommand::new(provider::executable());
    command
        .args(["login", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return "unknown",
    };
    let Some(mut stdout) = child.stdout.take() else {
        return "unknown";
    };
    let stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.by_ref().take(16 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let error_reader = stderr.map(|mut stderr| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.by_ref().take(16 * 1024).read_to_end(&mut bytes);
            bytes
        })
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) | Err(_) => {
                stop_login_process(&mut child);
                break None;
            }
        }
    };
    let mut output = reader.join().unwrap_or_default();
    if let Some(error_reader) = error_reader {
        output.extend(error_reader.join().unwrap_or_default());
    }
    let text = String::from_utf8_lossy(&output).to_ascii_lowercase();
    if text.contains("logged out") || text.contains("not logged") {
        "logged_out"
    } else if status_success(status)
        && (text.contains("logged in") || text.contains("using chatgpt"))
    {
        "logged_in"
    } else {
        "unknown"
    }
}

fn status_success(status: Option<std::process::ExitStatus>) -> bool {
    status.is_some_and(|status| status.success())
}

fn codex_version_available() -> bool {
    let mut command = ProcessCommand::new(provider::executable());
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) | Err(_) => {
                stop_login_process(&mut child);
                return false;
            }
        }
    }
}

fn stop_login_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as libc::pid_t;
        let _ = libc::kill(-pid, libc::SIGTERM);
        thread::sleep(Duration::from_millis(100));
        if child.try_wait().ok().flatten().is_none() {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_ai_help(json: bool) -> i32 {
    let help = AiHelp {
        schema_version: SCHEMA_VERSION,
        command: "codex-image generate",
        non_interactive: true,
        required: AiRequirements {
            environment: "OPENAI_API_KEY for the default --provider api; authenticated Codex CLI only with --provider codex",
            flags: vec!["--prompt TEXT or --prompt-file FILE"],
        },
        safe_template: "codex-image generate --provider api --prompt \"<prompt>\" --output-dir ./artifacts/design --name <safe-stem> --n 1 --size 1024x1024 --quality low --json",
        planning_template: "codex-image generate --provider api --prompt \"<prompt>\" --output-dir ./artifacts/design --prefix <safe-stem> --n 1 --size 1024x1024 --quality low --dry-run --json",
        batch_template: "codex-image batch submit --provider api --prompt \"<prompt>\" --output-dir ./artifacts/design --prefix <safe-stem> --n 2 --size 1024x1024 --quality low --job-file ./batch-job.json --json",
        request_file_template: "{\"schema_version\":1,\"prompt\":\"<prompt>\",\"provider\":\"api\",\"size\":\"1024x1024\",\"quality\":\"low\"}",
        capabilities: provider::capabilities(),
        rules: vec![
            "The default provider is the direct Image API and reads OPENAI_API_KEY only from the environment; --provider codex explicitly selects the local subscription path.",
            "Use --dry-run --json to validate names and parameters without reading a key or using a network.",
            "Create --output-dir explicitly; the CLI refuses missing or symlinked output directories.",
            "Use --name only for one image; use --prefix for deterministic multi-image names.",
            "Low quality is the preferred default for cost-controlled runs; choose medium or high explicitly when the request needs it.",
            "The API provider defaults to a 1024x1024 PNG; choose --size auto or a larger size explicitly when needed.",
            "Never retry exit code 5, 6, or 7 automatically because a generation may have been billed.",
            "Use --confirm-high-quality with --quality high after reviewing the approximate cost warning.",
            "Batch commands require --provider api; persist the returned job file and use batch status, retrieve, cancel, or recover.",
            "Repeat custom-origin or loopback approval flags on each Batch operation; editable job files never grant credential-destination approval.",
        ],
    };
    if json {
        print_json(&help);
    } else {
        println!("codex-image is fully non-interactive.");
        println!("Required environment: {}", help.required.environment);
        println!("Required input: {}", help.required.flags.join("; "));
        println!("Plan safely: {}", help.planning_template);
        println!("Batch: {}", help.batch_template);
        println!("Structured request: {}", help.request_file_template);
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
    batch_template: &'static str,
    request_file_template: &'static str,
    capabilities: Vec<provider::Capability>,
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
