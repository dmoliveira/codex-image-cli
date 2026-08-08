use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::Value;

const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLxOQAAAABJRU5ErkJggg==";

struct RecordedRequest {
    path_is_correct: bool,
    authorization_was_present: bool,
    request_json: Value,
}

fn spawn_server(status: &str, body: String) -> (String, JoinHandle<RecordedRequest>) {
    spawn_server_with_request_id(status, body, "local-test-request")
}

fn spawn_server_with_request_id(
    status: &str,
    body: String,
    request_id: &str,
) -> (String, JoinHandle<RecordedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let status = status.to_owned();
    let request_id = request_id.to_owned();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nX-Request-ID: {request_id}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
        request
    });
    (format!("http://{address}/v1"), handle)
}

fn spawn_disconnect_server() -> (String, JoinHandle<RecordedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let _ = stream.shutdown(Shutdown::Both);
        request
    });
    (format!("http://{address}/v1"), handle)
}

fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end;
    loop {
        let bytes = stream.read(&mut buffer).unwrap();
        assert!(bytes > 0, "request ended before headers");
        data.extend_from_slice(&buffer[..bytes]);
        if let Some(position) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
        assert!(data.len() < 64 * 1024, "headers exceeded test limit");
    }
    let headers = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length: ")
                .or_else(|| line.strip_prefix("Content-Length: "))
        })
        .unwrap()
        .trim()
        .parse::<usize>()
        .unwrap();
    while data.len() < header_end + content_length {
        let bytes = stream.read(&mut buffer).unwrap();
        assert!(bytes > 0, "request ended before body");
        data.extend_from_slice(&buffer[..bytes]);
    }
    let first_line = headers.lines().next().unwrap_or_default();
    RecordedRequest {
        path_is_correct: first_line.starts_with("POST /v1/images/generations HTTP/"),
        authorization_was_present: headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer test-key")),
        request_json: serde_json::from_slice(&data[header_end..header_end + content_length])
            .unwrap(),
    }
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codex-image"));
    command.stdin(Stdio::null());
    command
}

// macOS commonly maps /var through a system symlink. The CLI deliberately
// rejects symlinked output-path components, so keep test outputs under the
// checked-out repository instead of the platform temporary directory.
fn safe_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(".codex-image-test-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}

#[derive(Debug)]
struct RawHttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_raw_request(stream: &mut TcpStream) -> RawHttpRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end;
    loop {
        let bytes = stream.read(&mut buffer).unwrap();
        assert!(bytes > 0, "request ended before headers");
        data.extend_from_slice(&buffer[..bytes]);
        if let Some(position) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
        assert!(data.len() < 128 * 1024, "headers exceeded test limit");
    }
    let headers = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length: ")
                .or_else(|| line.strip_prefix("Content-Length: "))
        })
        .unwrap_or("0")
        .trim()
        .parse::<usize>()
        .unwrap();
    while data.len() < header_end + content_length {
        let bytes = stream.read(&mut buffer).unwrap();
        assert!(bytes > 0, "request ended before body");
        data.extend_from_slice(&buffer[..bytes]);
    }
    let first_line = headers.lines().next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    RawHttpRequest {
        method: parts.next().unwrap_or_default().to_owned(),
        path: parts.next().unwrap_or_default().to_owned(),
        body: data[header_end..header_end + content_length].to_vec(),
    }
}

fn custom_ids_from_multipart(body: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')? + 1;
            serde_json::from_str::<Value>(&line[start..end])
                .ok()?
                .get("custom_id")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

fn spawn_batch_server() -> (String, JoinHandle<Vec<RawHttpRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut custom_ids = Vec::new();
        for index in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_raw_request(&mut stream);
            if index == 0 {
                custom_ids = custom_ids_from_multipart(&request.body);
            }
            let body = match (index, request.path.as_str()) {
                (0, "/v1/files") => r#"{"id":"file-input"}"#.to_owned(),
                (1, "/v1/batches") => {
                    r#"{"id":"batch-test","status":"validating","input_file_id":"file-input"}"#.to_owned()
                }
                (2, "/v1/batches/batch-test") | (3, "/v1/batches/batch-test") => {
                    r#"{"id":"batch-test","status":"completed","input_file_id":"file-input","output_file_id":"file-output"}"#.to_owned()
                }
                (4, "/v1/files/file-output/content") => custom_ids
                    .iter()
                    .map(|custom_id| {
                        serde_json::json!({
                            "custom_id": custom_id,
                            "response": {
                                "status_code": 200,
                                "body": {"data": [{"b64_json": PNG_BASE64}]}
                            }
                        })
                        .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => panic!("unexpected batch request {index} {}", request.path),
            };
            let content_type = if index == 4 {
                "application/jsonl"
            } else {
                "application/json"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Request-ID: batch-test\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            requests.push(request);
        }
        requests
    });
    (format!("http://{address}/v1"), handle)
}

#[cfg(unix)]
fn fake_codex_script(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = directory.join("fake-codex");
    fs::write(
        &script,
        r##"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "codex-cli fake"
  exit 0
fi
if [ "$1" = "login" ]; then
  echo "Logged in using ChatGPT"
  exit 0
fi
if [ "$1" = "exec" ]; then
  if [ "${FAKE_CODEX_MODE:-success}" = "fail" ]; then exit 9; fi
  if [ "${FAKE_CODEX_MODE:-success}" = "hang" ]; then
    (sleep 10) &
    wait
  fi
  request=""
  for argument in "$@"; do request="$argument"; done
  request_path=$(printf '%s\n' "$request" | sed -n 's/.*request at \([^ ]*\).*/\1/p' | sed 's/[.]$//')
  request=$(cat "$request_path")
  printf '%s' "$request" > "$FAKE_CODEX_REQUEST"
  path=$(printf '%s\n' "$request" | sed -n 's/.*"artifact_path":"\([^"]*\)".*/\1/p')
  if [ -z "$path" ]; then exit 3; fi
  if [ -n "${OPENAI_API_KEY:-}" ]; then exit 4; fi
  cp "$FAKE_CODEX_IMAGE" "$path"
  exit 0
fi
exit 2
"##,
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&script, permissions).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn codex_provider_uses_fake_structured_request_without_api_key() {
    let fixture_dir = safe_tempdir();
    let fixture = fixture_dir.path().join("fixture.png");
    fs::write(&fixture, b"\x89PNG\r\n\x1a\nfixture").unwrap();
    let request_log = fixture_dir.path().join("request.txt");
    let codex = fake_codex_script(fixture_dir.path());
    let output_dir = safe_tempdir();
    let output = command()
        .env("CODEX_CLI_PATH", &codex)
        .env("FAKE_CODEX_IMAGE", &fixture)
        .env("FAKE_CODEX_REQUEST", &request_log)
        .env("OPENAI_API_KEY", "must-not-reach-codex")
        .args([
            "generate",
            "--provider",
            "codex",
            "--prompt",
            "quoted \"prompt\"\n日本語",
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--name",
            "fox",
            "--size",
            "1024x1024",
            "--quality",
            "high",
            "--confirm-high-quality",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["request"]["provider"], "codex");
    assert!(report["http"].get("status").is_none());
    assert_eq!(
        fs::read(output_dir.path().join("fox.png")).unwrap(),
        b"\x89PNG\r\n\x1a\nfixture"
    );
    let instruction = fs::read_to_string(request_log).unwrap();
    let request: Value = serde_json::from_str(instruction.lines().last().unwrap()).unwrap();
    assert_eq!(request["prompt"], "quoted \"prompt\"\n日本語");
    assert_eq!(request["size"], "1024x1024");
    assert_eq!(request["quality"], "high");
}

#[test]
fn request_file_resolves_structured_generation_parameters() {
    let directory = safe_tempdir();
    let request_file = directory.path().join("request.json");
    fs::write(
        &request_file,
        r#"{"schema_version":1,"prompt":"structured prompt","provider":"api","n":1,"format":"png","size":"1024x1024","quality":"high"}"#,
    )
    .unwrap();
    let output = command()
        .args([
            "generate",
            "--request-file",
            request_file.to_str().unwrap(),
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--name",
            "request",
            "--dry-run",
            "--confirm-high-quality",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["request"]["provider"], "api");
    assert_eq!(report["request"]["model"], "gpt-image-2");
}

#[test]
fn request_file_rejects_unknown_fields_and_stdin() {
    let directory = safe_tempdir();
    let request_file = directory.path().join("unknown.json");
    fs::write(
        &request_file,
        r#"{"schema_version":1,"prompt":"x","unexpected":"nope"}"#,
    )
    .unwrap();
    let unknown = command()
        .args([
            "generate",
            "--request-file",
            request_file.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&unknown.stdout).unwrap();
    assert_eq!(report["error"]["code"], "request_file_invalid_json");

    let stdin_request = command()
        .args(["generate", "--request-file", "-", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert_eq!(stdin_request.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&stdin_request.stdout).unwrap();
    assert_eq!(report["error"]["code"], "request_file_stdin_not_supported");
}

#[cfg(unix)]
#[test]
fn codex_timeout_reports_non_retryable_process_metadata() {
    let fixture_dir = safe_tempdir();
    let codex = fake_codex_script(fixture_dir.path());
    let output_dir = safe_tempdir();
    let output = command()
        .env("CODEX_CLI_PATH", &codex)
        .env("FAKE_CODEX_MODE", "hang")
        .args([
            "generate",
            "--provider",
            "codex",
            "--prompt",
            "timeout test",
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--name",
            "timeout",
            "--timeout-seconds",
            "1",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["error"]["code"], "codex_generation_timeout");
    assert_eq!(report["error"]["process_timed_out"], true);
    assert_eq!(report["error"]["automatic_retry_safe"], false);
}

#[test]
fn generate_writes_a_valid_png_from_a_loopback_mock() {
    let body = format!(r#"{{"data":[{{"b64_json":"{PNG_BASE64}"}}]}}"#);
    let (url, server) = spawn_server("200 OK", body);
    let directory = safe_tempdir();

    let output = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "a small fox",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--name",
            "fox",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "success");
    assert_eq!(report["request"]["request_id"], "local-test-request");
    let image = std::fs::read(directory.path().join("fox.png")).unwrap();
    assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));

    let request = server.join().unwrap();
    assert!(request.path_is_correct);
    assert!(request.authorization_was_present);
    assert_eq!(request.request_json["model"], "gpt-image-2");
    assert_eq!(request.request_json["n"], 1);
}

#[test]
fn dry_run_does_not_connect_or_require_a_key() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let directory = safe_tempdir();

    let output = command()
        .env_remove("OPENAI_API_KEY")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "draft only",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--prefix",
            "draft",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "dry_run");
    assert_eq!(report["request"]["attempted"], false);
    assert!(!directory.path().join("draft.png").exists());
    listener.set_nonblocking(true).unwrap();
    assert!(
        listener.accept().is_err(),
        "dry run made a network connection"
    );
}

#[test]
fn collision_stops_before_the_mock_receives_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let directory = safe_tempdir();
    std::fs::write(directory.path().join("taken.png"), b"keep me").unwrap();

    let output = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "do not call the server",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--name",
            "taken",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "preflight_error");
    assert_eq!(
        std::fs::read(directory.path().join("taken.png")).unwrap(),
        b"keep me"
    );
    listener.set_nonblocking(true).unwrap();
    assert!(
        listener.accept().is_err(),
        "preflight failure made a request"
    );
}

#[test]
fn disconnected_post_is_indeterminate_and_is_not_retried() {
    let (url, server) = spawn_disconnect_server();
    let directory = safe_tempdir();
    let output = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "one request only",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--name",
            "one",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(5));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "outcome_indeterminate");
    assert_eq!(report["error"]["automatic_retry_safe"], false);
    let request = server.join().unwrap();
    assert!(request.path_is_correct);
}

#[test]
fn server_error_does_not_reflect_the_api_key() {
    let body = r#"{"error":{"code":"invalid_api_key","message":"test-key"}}"#.to_owned();
    let (url, server) = spawn_server_with_request_id("401 Unauthorized", body, "test-key");
    let directory = safe_tempdir();
    let output = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "safe error",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--name",
            "error",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(4));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("test-key"));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["error"]["code"], "api_rejected");
    assert!(report["request"].get("request_id").is_none());
    let request = server.join().unwrap();
    assert!(request.authorization_was_present);
}

#[test]
fn redirect_is_refused_without_following_it() {
    let (url, server) = spawn_server("302 Found", "{}".to_owned());
    let directory = safe_tempdir();
    let output = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "do not follow redirects",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--name",
            "redirect",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(5));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "outcome_indeterminate");
    assert_eq!(report["error"]["code"], "redirect_outcome_unknown");
    assert_eq!(report["error"]["automatic_retry_safe"], false);
    assert!(!directory.path().join("redirect.png").exists());
    let request = server.join().unwrap();
    assert!(request.path_is_correct);
}

#[test]
fn malformed_multi_image_response_publishes_no_partial_files() {
    let body = format!(r#"{{"data":[{{"b64_json":"{PNG_BASE64}"}},{{"b64_json":"not-base64"}}]}}"#);
    let (url, server) = spawn_server("200 OK", body);
    let directory = safe_tempdir();
    let output = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "all or no files",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--prefix",
            "pair",
            "--n",
            "2",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(6));
    assert!(!directory.path().join("pair-01.png").exists());
    assert!(!directory.path().join("pair-02.png").exists());
    let request = server.join().unwrap();
    assert_eq!(request.request_json["n"], 2);
}

#[test]
fn doctor_and_missing_codex_are_machine_safe_and_non_interactive() {
    let fake_dir = safe_tempdir();
    let fake_codex = fake_codex_script(fake_dir.path());
    let doctor = command()
        .env("CODEX_CLI_PATH", &fake_codex)
        .env_remove("OPENAI_API_KEY")
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(doctor.status.success());
    assert!(doctor.stderr.is_empty());
    let doctor_report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(doctor_report["status"], "local_configuration_ready");
    assert_eq!(doctor_report["checks"][0]["name"], "CODEX_CLI");
    assert_eq!(doctor_report["checks"][1]["status"], "logged_in");

    let directory = safe_tempdir();
    let missing_key = command()
        .env_remove("OPENAI_API_KEY")
        .args([
            "generate",
            "--provider",
            "api",
            "--prompt",
            "no key request",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--name",
            "no-key",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(missing_key.status.code(), Some(2));
    assert!(missing_key.stderr.is_empty());
    let report: Value = serde_json::from_slice(&missing_key.stdout).unwrap();
    assert_eq!(report["error"]["code"], "missing_api_key");
    assert_eq!(report["request"]["attempted"], false);
    assert!(!directory.path().join("no-key.png").exists());
}

#[test]
fn clap_parse_failure_respects_the_json_contract() {
    let output = command()
        .args([
            "generate", "--prompt", "bad enum", "--format", "gif", "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["status"], "usage_error");
    assert_eq!(report["error"]["code"], "cli_parse_error");
    assert_eq!(report["request"]["attempted"], false);
}

#[test]
fn batch_submit_status_and_retrieve_publish_in_order() {
    let directory = safe_tempdir();
    let output_dir = directory.path().join("images");
    fs::create_dir(&output_dir).unwrap();
    fs::write(output_dir.join("fox-01.png"), b"old-one").unwrap();
    fs::write(output_dir.join("fox-02.png"), b"old-two").unwrap();
    let job_file = directory.path().join("batch-job.json");
    let (url, server) = spawn_batch_server();

    let submitted = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "batch",
            "submit",
            "--provider",
            "api",
            "--prompt",
            "batch fox",
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--prefix",
            "fox",
            "--n",
            "2",
            "--overwrite",
            "--job-file",
            job_file.to_str().unwrap(),
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(submitted.status.success(), "{submitted:?}");
    let submitted_report: Value = serde_json::from_slice(&submitted.stdout).unwrap();
    assert_eq!(submitted_report["status"], "validating");
    assert_eq!(submitted_report["batch_id"], "batch-test");
    let job_body = fs::read_to_string(&job_file).unwrap();
    assert!(!job_body.contains("batch fox"));

    let status = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "batch",
            "status",
            "--job-file",
            job_file.to_str().unwrap(),
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    let status_report: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_report["status"], "completed");

    let retrieved = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "batch",
            "retrieve",
            "--job-file",
            job_file.to_str().unwrap(),
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(retrieved.status.success(), "{retrieved:?}");
    let retrieved_report: Value = serde_json::from_slice(&retrieved.stdout).unwrap();
    assert_eq!(retrieved_report["status"], "retrieved");
    assert!(output_dir.join("fox-01.png").exists());
    assert!(output_dir.join("fox-02.png").exists());
    assert_eq!(
        retrieved_report["retained_artifacts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let repeated = command()
        .env_remove("OPENAI_API_KEY")
        .args([
            "batch",
            "retrieve",
            "--job-file",
            job_file.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(repeated.status.success(), "{repeated:?}");
    let repeated_report: Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated_report["status"], "retrieved");
    assert_eq!(
        repeated_report["retained_artifacts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let cancel = command()
        .env_remove("OPENAI_API_KEY")
        .args([
            "batch",
            "cancel",
            "--job-file",
            job_file.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(cancel.status.code(), Some(3));
    let cancel_report: Value = serde_json::from_slice(&cancel.stdout).unwrap();
    assert_eq!(cancel_report["error"]["code"], "batch_already_terminal");

    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/files");
    assert!(String::from_utf8_lossy(&requests[0].body).contains("purpose"));
    assert_eq!(requests[1].path, "/v1/batches");
    assert_eq!(requests[2].path, "/v1/batches/batch-test");
    assert_eq!(requests[3].path, "/v1/batches/batch-test");
    assert_eq!(requests[4].path, "/v1/files/file-output/content");
}

#[test]
fn batch_rejects_more_than_the_local_image_limit_before_network() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let directory = safe_tempdir();
    let output = command()
        .env_remove("OPENAI_API_KEY")
        .args([
            "batch",
            "submit",
            "--prompt",
            "too many",
            "--output-dir",
            directory.path().to_str().unwrap(),
            "--n",
            "9",
            "--api-base-url",
            &url,
            "--allow-insecure-localhost",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["error"]["code"], "invalid_image_count");
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err(), "invalid batch connected to API");
}

#[test]
fn batch_request_file_resolves_during_dry_run_without_a_key() {
    let directory = safe_tempdir();
    let output_dir = directory.path().join("images");
    fs::create_dir(&output_dir).unwrap();
    let request_file = directory.path().join("request.json");
    fs::write(
        &request_file,
        serde_json::json!({
            "schema_version": 1,
            "prompt": "batch request file",
            "n": 2,
            "format": "png",
            "quality": "low"
        })
        .to_string(),
    )
    .unwrap();
    let output = command()
        .env_remove("OPENAI_API_KEY")
        .args([
            "batch",
            "submit",
            "--request-file",
            request_file.to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--prefix",
            "request-file",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "dry_run");
    assert_eq!(report["outputs"].as_array().unwrap().len(), 2);
}
