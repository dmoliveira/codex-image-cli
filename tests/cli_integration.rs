use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
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

#[test]
fn generate_writes_a_valid_png_from_a_loopback_mock() {
    let body = format!(r#"{{"data":[{{"b64_json":"{PNG_BASE64}"}}]}}"#);
    let (url, server) = spawn_server("200 OK", body);
    let directory = safe_tempdir();

    let output = command()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "generate",
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
fn doctor_and_missing_key_are_machine_safe_and_non_interactive() {
    let doctor = command()
        .env_remove("OPENAI_API_KEY")
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(doctor.status.code(), Some(2));
    assert!(doctor.stderr.is_empty());
    let doctor_report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(doctor_report["status"], "local_configuration_required");
    assert_eq!(doctor_report["checks"][0]["status"], "missing");

    let directory = safe_tempdir();
    let missing_key = command()
        .env_remove("OPENAI_API_KEY")
        .args([
            "generate",
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
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "usage_error");
    assert_eq!(report["error"]["code"], "cli_parse_error");
    assert_eq!(report["request"]["attempted"], false);
}
