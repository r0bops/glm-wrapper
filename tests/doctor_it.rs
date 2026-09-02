//! Integration tests for the `glm doctor` command against a local HTTP stub:
//! 200, 401-then-raw-key success, and timeout behavior.

mod common;
use common::*;

/// A config pointing base_url at the stub, key file present.
fn cfg_with_base(xdg: &std::path::Path, base: &str, key: &str) {
    std::fs::create_dir_all(xdg.join("glm")).unwrap();
    std::fs::write(
        xdg.join("glm").join("config.toml"),
        format!(
            "profile = \"zai\"\nmodel = \"glm-5.3\"\nsmall_model = \"glm-5.3-flash\"\nbase_url = \"{base}\"\n"
        ),
    )
    .unwrap();
    std::fs::write(xdg.join("glm").join("key"), format!("{key}\n")).unwrap();
}

#[test]
fn doctor_reports_pass_when_models_ok() {
    let srv = StubServer::start(vec![(
        200,
        r#"{"data":[{"id":"glm-5.3"},{"id":"glm-4.6"}]}"#.to_string(),
    )]);
    let xdg = scratch_xdg("doctor-200");
    cfg_with_base(&xdg, &format!("http://{}", srv.addr), "sk-abc");

    let (stdout, stderr, code) = run_glm(&["doctor"], &xdg, &[]);
    assert_eq!(code, 1, "claude missing on PATH fails doctor: {stderr}");
    assert!(stdout.contains("PASS  API key"), "stdout: {stdout}");
    assert!(
        stdout.contains("resolved from") && stdout.contains("key"),
        "stdout: {stdout}"
    );
    // endpoint check passed: request went to the stub with Bearer auth
    let headers = srv.auth_headers();
    assert!(
        headers.iter().any(|h| h == "Authorization: Bearer sk-abc"),
        "headers: {headers:?}"
    );
    assert!(stdout.contains("PASS  API endpoint"), "stdout: {stdout}");
}

#[test]
fn doctor_retries_raw_key_on_401() {
    // First request -> 401, second -> 200 with the model list.
    let srv = StubServer::start(vec![
        (401, r#"{"error":"unauthorized"}"#.to_string()),
        (200, r#"{"data":[{"id":"glm-5.3"}]}"#.to_string()),
    ]);
    let xdg = scratch_xdg("doctor-401");
    cfg_with_base(&xdg, &format!("http://{}", srv.addr), "sk-raw");

    let (stdout, _stderr, _code) = run_glm(&["doctor"], &xdg, &[]);
    assert!(stdout.contains("PASS  API endpoint"), "stdout: {stdout}");
    let headers = srv.auth_headers();
    assert_eq!(headers.len(), 2, "{headers:?}");
    assert_eq!(headers[0], "Authorization: Bearer sk-raw");
    assert_eq!(headers[1], "Authorization: sk-raw");
}

#[test]
fn doctor_endpoint_fails_on_timeout() {
    let srv = StubServer::start_silent();
    let xdg = scratch_xdg("doctor-timeout");
    cfg_with_base(&xdg, &format!("http://{}", srv.addr), "sk-abc");

    let (stdout, _stderr, _code) = run_glm(&["doctor"], &xdg, &[]);
    assert!(
        stdout.contains("FAIL  API endpoint"),
        "expected endpoint FAIL, got:\n{stdout}"
    );
    assert!(stdout.contains("not reachable"), "stdout: {stdout}");
}

#[test]
fn doctor_reports_unknown_config_keys() {
    let xdg = scratch_xdg("doctor-unknown-key");
    std::fs::create_dir_all(xdg.join("glm")).unwrap();
    std::fs::write(
        xdg.join("glm").join("config.toml"),
        "profile = \"zai\"\nbogus_key = 1\n",
    )
    .unwrap();
    std::fs::write(xdg.join("glm").join("key"), "sk-x\n").unwrap();

    let (stdout, _stderr, _code) = run_glm(&["doctor"], &xdg, &[]);
    assert!(stdout.contains("FAIL  config.toml"), "stdout: {stdout}");
    assert!(stdout.contains("bogus_key"), "stdout: {stdout}");
}
